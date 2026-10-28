//! Live TSE, for testing: WeSep in the live chain, between the microphone
//! and DeepFilterNet3, switched on from the window (off by default).
//!
//! WeSep's public model looks at audio after each moment as well as before,
//! so it can't work frame by frame. As the team's
//!
//!   run_tse.sh --enroll <voice sample> --input <mic> --chunk_sec 3.0 --overlap_sec 1.0
//!
//! this stage cuts the microphone into 3 s chunks overlapping by 1 s (the
//! second before, then 2 s new), has a worker thread run WeSep on each as
//! soon as it's gathered, and joins the results chunk by chunk: the
//! overlapping second crossfaded from the chunk before to this one. It plays
//! them out at a steady pace a fixed DELAY behind, long enough for WeSep to
//! finish each chunk in time. So while it's on, the customer hears the agent
//! about 5 s late: a test of what TSE does to the call, not something to use
//! on real calls. Speech enhancement (run_se.sh) follows in the chain:
//! DeepFilterNet3. A causal model, trained for this, would take WeSep's
//! place with about 30 ms of delay.
//!
//! Off, frames pass straight through. A chunk WeSep can't do at all, or not
//! in time, plays as it was recorded (still delayed), so the customer always
//! hears the agent. When WeSep falls behind, it skips chunks already past
//! their time to catch up.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::enhance::Fir;
use crate::frame::AudioFrame;
use crate::ipc;
use crate::ring::{Consumer, Producer};
use crate::stream;
use crate::tse;

/// 10 ms frames of new audio in each chunk: 2 s.
const BLOCK_FRAMES: usize = 200;
/// The earlier audio each chunk starts with, 16 kHz samples: 1 s, so chunks
/// are 3 s overlapping by 1 s (--chunk_sec 3.0 --overlap_sec 1.0).
const OVERLAP: usize = 16_000;
/// Frames the output runs behind the microphone while on: 5 s (a chunk's
/// 2 s of new audio, then up to 2 s for WeSep, and the 1 s at its end held
/// back to crossfade with the next chunk).
pub const DELAY_FRAMES: usize = 500;
/// 48 kHz to 16 kHz.
const DOWN: usize = 3;
const FRAME: usize = 480;

/// The worker's answer for a chunk it skipped because it was already late.
const SKIPPED: &str = "skipped: late";

/// The stage's state for the window (`ipc::TELEMETRY.live_tse`).
pub const OFF: u8 = 0;
pub const ON: u8 = 1;
/// Switched on, but WeSep couldn't run (no voice sample, or its files
/// failed): the voice passes through as recorded, still delayed.
pub const FAILED: u8 = 2;
/// A block was late: the customer heard a gap.
pub const LATE: u8 = 3;
/// WeSep's files aren't there: no live TSE (set at startup).
pub const UNAVAILABLE: u8 = 4;

/// Shared between the stage and whoever drives it.
#[derive(Default)]
pub struct Control {
    /// Live TSE wanted.
    pub enabled: AtomicBool,
    pub stop: AtomicBool,
    /// Frames WeSep has handled while on, and the time it took (µs): its
    /// real-time factor, for stages.rs's rtf.csv.
    pub handled: AtomicU64,
    pub busy_us: AtomicU64,
}

/// A job back from the worker: its generation, whether it was only a warm-up,
/// WeSep's output (16 kHz) and how long it took.
type Done = (u64, bool, Result<Vec<f32>, String>, Duration);

/// WeSep's threads on the live call: half the PC's logical processors, 2 to
/// 4 (2 threads fell behind on the development PC on battery), or `wanted`
/// (settings.ini) if more.
pub fn threads(wanted: usize) -> usize {
    let half = thread::available_parallelism().map_or(2, |n| n.get() / 2);
    half.clamp(2, 4).max(wanted)
}

/// What WeSep works with: its files, its threads, and the voice sample.
pub struct Setup {
    pub files: tse::Files,
    pub threads: usize,
    pub sample: PathBuf,
}

/// The stage: frames from `input` (the microphone, mono 48 kHz) go to
/// `output` (DeepFilterNet3), through WeSep while `control.enabled`. `tap`
/// gets a copy of each frame sent: stereo, the microphone it came from (left,
/// lined up) and what was sent (right). Runs until `control.stop`.
pub fn run(
    input: &mut Consumer,
    output: &mut Producer,
    mut tap: Option<&mut Producer>,
    control: &Control,
    setup: Setup,
) {
    let (jobs, jobs_rx) = mpsc::channel::<(u64, Vec<f32>)>();
    let (done_tx, done) = mpsc::channel::<Done>();
    thread::spawn(move || worker(&setup, jobs_rx, done_tx));
    let _ = stream::realtime(|| {
        let mut on: Option<On> = None;
        let mut generation = 0u64;
        let mut copy = AudioFrame::new(2);
        while !control.stop.load(Relaxed) {
            let Some(frame) = input.pop() else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            let wanted = control.enabled.load(Relaxed);
            if wanted != on.is_some() {
                // Switched: start afresh (stale WeSep results are ignored).
                generation += 1;
                on = wanted.then(On::new);
                state(if wanted { ON } else { OFF });
                if wanted {
                    // Load WeSep and the voice now, not on the first block.
                    let _ = jobs.send((generation, Vec::new()));
                }
            }
            let (out, mic) = match on.as_mut() {
                None => (frame, frame),
                Some(s) => {
                    for (_, warm, result, took) in
                        done.try_iter().filter(|(g, ..)| *g == generation)
                    {
                        match result {
                            Ok(_) if warm => {}
                            Err(_) if warm => state(FAILED),
                            Ok(cleaned) => {
                                control.handled.fetch_add(BLOCK_FRAMES as u64, Relaxed);
                                control.busy_us.fetch_add(took.as_micros() as u64, Relaxed);
                                s.finished(&cleaned);
                                state(ON);
                            }
                            Err(e) if e == SKIPPED => {
                                state(LATE);
                                s.finished_raw();
                            }
                            Err(_) => {
                                state(FAILED);
                                s.finished_raw();
                            }
                        }
                    }
                    if let Some(block) = s.push(&frame) {
                        let _ = jobs.send((generation, block));
                    }
                    let (out, mic, late) = s.next(&frame);
                    if late {
                        state(LATE);
                    }
                    (out, mic)
                }
            };
            if let Some(t) = tap.as_deref_mut() {
                for (pair, (m, o)) in copy
                    .samples_mut()
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(mic.samples().iter().zip(out.samples()))
                {
                    (pair[0], pair[1]) = (*m, *o);
                }
                copy.seq = out.seq;
                let _ = t.push(&copy);
            }
            let _ = output.push(&out);
        }
        Ok(())
    });
}

fn state(s: u8) {
    ipc::TELEMETRY.live_tse.store(s, Relaxed);
}

/// The stage while on.
struct On {
    down: Fir,
    /// The new audio being gathered (48 kHz), and the last second of the
    /// block before it: the overlap the next chunk starts with, 48 kHz and,
    /// for WeSep, 16 kHz.
    block: Vec<f32>,
    overlap: Vec<f32>,
    overlap_low: Vec<f32>,
    /// Microphone frames waiting to be played out (the delay line), and the
    /// joined audio ready to play, 48 kHz.
    mics: VecDeque<AudioFrame>,
    ready: VecDeque<f32>,
    /// Chunks sent to WeSep and not back yet, as recorded (48 kHz), in case
    /// WeSep fails on them.
    pending: VecDeque<Vec<f32>>,
    /// The last chunk's final second, held back to crossfade with the next
    /// chunk's first (48 kHz).
    held: Vec<f32>,
    /// Microphone audio sent in place of audio that wasn't ready (48 kHz):
    /// that much of what comes back is past its time and is dropped, so the
    /// delay stays DELAY_FRAMES.
    owed: usize,
}

impl On {
    fn new() -> On {
        let mut silent = AudioFrame::new(1);
        silent.captured = None;
        On {
            down: Fir::new(),
            block: Vec::with_capacity(BLOCK_FRAMES * FRAME),
            overlap: Vec::new(),
            overlap_low: Vec::new(),
            mics: (0..DELAY_FRAMES).map(|_| silent).collect(),
            ready: VecDeque::from(vec![0.0; DELAY_FRAMES * FRAME]),
            pending: VecDeque::new(),
            held: Vec::new(),
            owed: 0,
        }
    }

    /// Queues audio to play, minus what the microphone already stood in for.
    fn ready(&mut self, audio: impl IntoIterator<Item = f32>) {
        let mut audio = audio.into_iter();
        while self.owed > 0 && audio.next().is_some() {
            self.owed -= 1;
        }
        self.ready.extend(audio);
    }

    /// Adds a microphone frame; once 2 s of new audio are in, returns the
    /// chunk for WeSep: 16 kHz, the overlap first (none for the first).
    fn push(&mut self, frame: &AudioFrame) -> Option<Vec<f32>> {
        self.mics.push_back(*frame);
        self.block.extend_from_slice(frame.samples());
        if self.block.len() < BLOCK_FRAMES * FRAME {
            return None;
        }
        let block = std::mem::take(&mut self.block);
        let low = self.down.filter(&block, DOWN);
        let job: Vec<f32> = self.overlap_low.iter().chain(&low).copied().collect();
        let chunk: Vec<f32> = self.overlap.iter().chain(&block).copied().collect();
        self.overlap_low = low[low.len() - OVERLAP..].to_vec();
        self.overlap = block[block.len() - OVERLAP * DOWN..].to_vec();
        self.pending.push_back(chunk);
        Some(job)
    }

    /// WeSep's result for the oldest chunk sent (16 kHz, as sent).
    fn finished(&mut self, cleaned: &[f32]) {
        let Some(chunk) = self.pending.pop_front() else {
            return;
        };
        // Back to 48 kHz: zeros between the samples, then the same filter as
        // on the way down.
        let mut stuffed = vec![0f32; chunk.len()];
        for (i, v) in cleaned.iter().take(chunk.len() / DOWN).enumerate() {
            stuffed[i * DOWN] = v * DOWN as f32;
        }
        let up = Fir::new().filter(&stuffed, 1);
        self.join(&up, chunk.len() - BLOCK_FRAMES * FRAME);
    }

    /// WeSep failed on the oldest chunk: it plays as recorded.
    fn finished_raw(&mut self) {
        if let Some(chunk) = self.pending.pop_front() {
            self.join(&chunk, chunk.len() - BLOCK_FRAMES * FRAME);
        }
    }

    /// Joins a chunk (48 kHz) whose first `overlap` samples are the same
    /// stretch as the last chunk's held end: that stretch crossfaded from the
    /// last chunk to this one, then this chunk up to its own last second,
    /// which is held for the next.
    fn join(&mut self, chunk: &[f32], overlap: usize) {
        let (head, rest) = chunk.split_at(overlap);
        let held = std::mem::take(&mut self.held);
        let n = head.len().min(held.len());
        let faded: Vec<f32> = (0..n)
            .map(|i| {
                let w = (i as f32 + 0.5) / n as f32;
                held[i] * (1.0 - w) + head[i] * w
            })
            .collect();
        self.ready(faded);
        let keep = rest.len().saturating_sub(OVERLAP * DOWN);
        self.ready(rest[..keep].iter().copied());
        self.held = rest[keep..].to_vec();
    }

    /// The next frame out (cleaned, DELAY_FRAMES behind `now`), the
    /// microphone frame it came from, and whether it was late (then it's
    /// that microphone frame, as recorded).
    fn next(&mut self, now: &AudioFrame) -> (AudioFrame, AudioFrame, bool) {
        let mic = self.mics.pop_front().unwrap_or(AudioFrame::new(1));
        let mut out = AudioFrame::new(1);
        let late = self.ready.len() < FRAME;
        if late {
            // The microphone as recorded goes out instead; what's there is
            // played next time with the rest, and the frame's worth after it
            // is owed.
            out.samples_mut().copy_from_slice(mic.samples());
            self.owed += FRAME;
        } else {
            for s in out.samples_mut() {
                *s = self.ready.pop_front().unwrap_or(0.0);
            }
        }
        out.seq = now.seq;
        // The time the audio it holds was captured, so the reported latency
        // shows the delay.
        out.captured = mic.captured;
        (out, mic, late)
    }
}

/// Runs WeSep on the blocks from `jobs` (16 kHz, context first), returning
/// each with how long it took. Loads the model on the first job, and the
/// voice sample whenever it changes; an empty job only does that (warm: true).
fn worker(setup: &Setup, jobs: mpsc::Receiver<(u64, Vec<f32>)>, done: mpsc::Sender<Done>) {
    let mut model: Option<tse::Extractor> = None;
    let mut voice: Option<(Vec<f32>, std::time::SystemTime)> = None;
    let mut waiting: VecDeque<(u64, Vec<f32>)> = VecDeque::new();
    loop {
        if waiting.is_empty() {
            match jobs.recv() {
                Ok(job) => waiting.push_back(job),
                Err(_) => return,
            }
        }
        waiting.extend(jobs.try_iter());
        // A chunk with a newer one already waiting behind it is past its
        // time (they come 2 s apart, and each has 2 s): skip it, so WeSep
        // catches up instead of making every chunk after it late too.
        while waiting.len() > 1 && waiting.front().is_some_and(|(_, m)| !m.is_empty()) {
            let (generation, _) = waiting.pop_front().expect("checked above");
            let skipped = (generation, false, Err(SKIPPED.to_string()), Duration::ZERO);
            if done.send(skipped).is_err() {
                return;
            }
        }
        let (generation, mix) = waiting.pop_front().expect("one at least");
        let (started, warm) = (Instant::now(), mix.is_empty());
        let result = (|| -> Result<Vec<f32>, String> {
            if model.is_none() {
                model = Some(tse::Extractor::load(&setup.files, setup.threads)?);
            }
            let m = model.as_mut().expect("loaded above");
            let modified = std::fs::metadata(&setup.sample)
                .and_then(|x| x.modified())
                .map_err(|_| "no voice sample".to_string())?;
            if voice.as_ref().is_none_or(|(_, at)| *at != modified) {
                let b = std::fs::read(&setup.sample).map_err(|e| e.to_string())?;
                let samples: Vec<f32> = b
                    .get(44..)
                    .unwrap_or_default()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| f32::from(i16::from_le_bytes(c)) / 32768.0)
                    .collect();
                voice = Some((m.enroll(&samples)?, modified));
            }
            if warm {
                return Ok(Vec::new());
            }
            let embedding = &voice.as_ref().expect("enrolled above").0;
            let est = m.extract(embedding, &mix)?;
            Ok(tse::remix(&est, &mix, 0.0))
        })();
        if done
            .send((generation, warm, result, started.elapsed()))
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_frame(k: usize) -> AudioFrame {
        let mut f = AudioFrame::new(1);
        f.seq = k as u64;
        for (i, s) in f.samples_mut().iter_mut().enumerate() {
            let t = (k * FRAME + i) as f32 / 48_000.0;
            *s = 0.3 * (std::f32::consts::TAU * 440.0 * t).sin();
        }
        f
    }

    #[test]
    fn on_plays_blocks_a_fixed_delay_behind() {
        let mut s = On::new();
        let mut sent = Vec::new();
        let mut outs = Vec::new();
        for k in 0..DELAY_FRAMES + 2 * BLOCK_FRAMES {
            if let Some(job) = s.push(&tone_frame(k)) {
                // A chunk: the overlap (none for the first), then 2 s new, 16 kHz.
                let ctx = if sent.is_empty() { 0 } else { OVERLAP };
                assert_eq!(job.len(), ctx + BLOCK_FRAMES * FRAME / DOWN);
                sent.push(job);
                // "WeSep" hands it straight back, unchanged.
                let job = sent.last().unwrap().clone();
                s.finished(&job);
            }
            outs.push(s.next(&tone_frame(k)));
        }
        // The first DELAY_FRAMES are silence; then the microphone, delayed.
        assert!(
            outs[..DELAY_FRAMES]
                .iter()
                .all(|(o, _, late)| !late && o.samples().iter().all(|&v| v == 0.0))
        );
        for (k, (out, mic, late)) in outs.iter().enumerate().skip(DELAY_FRAMES + 10) {
            assert!(!late, "frame {k} late");
            let expected = tone_frame(k - DELAY_FRAMES);
            assert_eq!(
                mic.samples(),
                expected.samples(),
                "the delay line, frame {k}"
            );
            // Down and back up: the same tone, a little filter delay apart.
            let level = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
            let ratio = level(out.samples()) / level(expected.samples());
            assert!((ratio - 1.0).abs() < 0.05, "frame {k}: level ratio {ratio}");
        }
    }

    #[test]
    fn a_block_wesep_misses_or_late_plays_as_recorded() {
        let mut s = On::new();
        let mut outs = Vec::new();
        for k in 0..DELAY_FRAMES + BLOCK_FRAMES + 5 {
            if s.push(&tone_frame(k)).is_some() {
                s.finished_raw();
            }
            outs.push(s.next(&tone_frame(k)));
        }
        let (out, _, late) = &outs[DELAY_FRAMES + 3];
        assert!(!late);
        assert_eq!(out.samples(), tone_frame(3).samples());
        // Nothing more came back: the frames after the block are late.
        let mut s = On::new();
        for k in 0..DELAY_FRAMES {
            s.push(&tone_frame(k));
            s.next(&tone_frame(k));
        }
        // Late: the microphone it came from, as recorded, instead.
        let (out, _, late) = s.next(&tone_frame(DELAY_FRAMES));
        assert!(late);
        assert_eq!(out.samples(), tone_frame(0).samples());
    }

    #[test]
    fn chunks_are_3_s_overlapping_by_1_s_and_crossfaded() {
        // A steady level, and a "WeSep" that scales chunk k by 1 + k / 10:
        // which chunk each moment came from shows in its level.
        let level = |k: usize| {
            let mut f = AudioFrame::new(1);
            f.seq = k as u64;
            f.samples_mut().fill(0.1);
            f
        };
        let gain = |k: usize| 1.0 + k as f32 / 10.0;
        let mean = |x: &[f32]| x.iter().sum::<f32>() / x.len() as f32;
        let mut s = On::new();
        let (mut chunks, mut outs) = (0, Vec::new());
        for k in 0..DELAY_FRAMES + 5 * BLOCK_FRAMES {
            if let Some(job) = s.push(&level(k)) {
                // 3 s each, the first 2 s (it has no second before it).
                let secs = job.len() as f32 / 16_000.0;
                assert_eq!(secs, if chunks == 0 { 2.0 } else { 3.0 });
                let g = gain(chunks);
                s.finished(&job.iter().map(|v| v * g).collect::<Vec<_>>());
                chunks += 1;
            }
            outs.push(s.next(&level(k)).0);
        }
        // The microphone's frame m plays at m + DELAY_FRAMES. Chunk k (k > 0)
        // covers frames 200k - 100 to 200k + 200: its own second in the
        // middle at its own level, the second before it halfway through the
        // crossfade from chunk k - 1.
        let at = |m: usize| mean(outs[m + DELAY_FRAMES].samples()) / 0.1;
        assert!((at(50) - gain(0)).abs() < 0.01, "{}", at(50));
        for k in 1..4 {
            let own = at(200 * k + 50);
            assert!((own - gain(k)).abs() < 0.01, "chunk {k}: {own}");
            let fade = at(200 * k - 50);
            let half = (gain(k - 1) + gain(k)) / 2.0;
            assert!((fade - half).abs() < 0.01, "chunk {k} crossfade: {fade}");
        }
    }

    #[test]
    fn a_late_block_does_not_add_to_the_delay() {
        // A slow ramp: each moment has its own level.
        let ramp = |k: usize| {
            let mut f = AudioFrame::new(1);
            f.seq = k as u64;
            for (i, s) in f.samples_mut().iter_mut().enumerate() {
                *s = (k * FRAME + i) as f32 / 48_000.0 * 0.1;
            }
            f
        };
        let mean = |x: &[f32]| x.iter().sum::<f32>() / x.len() as f32;
        let mut s = On::new();
        let mut held = Vec::new();
        let mut outs = Vec::new();
        for k in 0..DELAY_FRAMES + 3 * BLOCK_FRAMES {
            if let Some(job) = s.push(&ramp(k)) {
                held.push(job);
            }
            // The first block comes back 50 frames late, the rest on time.
            if k >= DELAY_FRAMES + 50 {
                for job in held.drain(..) {
                    s.finished(&job);
                }
            }
            outs.push(s.next(&ramp(k)));
        }
        let late = outs.iter().filter(|(.., late)| *late).count();
        assert_eq!(late, 50);
        // After the gap: the microphone, still DELAY_FRAMES behind, and the
        // cleaned audio of that same moment (0.005 is 50 frames of ramp).
        for (k, (out, mic, late)) in outs.iter().enumerate().skip(DELAY_FRAMES + 60) {
            assert!(!late, "frame {k} late");
            assert_eq!(mic.samples(), ramp(k - DELAY_FRAMES).samples());
            let off = mean(out.samples()) - mean(mic.samples());
            assert!(off.abs() < 0.0005, "frame {k}: {off}");
        }
    }

    /// The live chain on a real recording, in real time, as in the app: this
    /// stage (today's WeSep, 3 s chunks overlapping by 1 s), then
    /// DeepFilterNet3. YAL_LIVETSE_TEST="<dir with onnxruntime.dll and
    /// wesep\\>|<voice sample>|<recording>|<out folder>", YAL_LIVETSE_DFN =
    /// DeepFilterNet3_onnx.tar.gz. Saves input.m4a, audio_tse.m4a and
    /// audio_tse_se.m4a (lined up, from the recording's start) and rtf.csv in
    /// the folder, and prints how far behind WeSep fell.
    /// cargo test --release live_tse_on_a_recording -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep files and a recording; runs in real time"]
    fn live_tse_on_a_recording() {
        use crate::ring;
        use std::path::Path;
        let spec = std::env::var("YAL_LIVETSE_TEST").expect("set YAL_LIVETSE_TEST");
        let [dir, sample, recording, out] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_LIVETSE_TEST needs 4 parts");
        };
        let out = Path::new(out);
        std::fs::create_dir_all(out).unwrap();
        // The voice sample as the app keeps it: a 16 kHz WAV.
        let voice = std::env::temp_dir().join(format!("livetse-voice-{}.wav", std::process::id()));
        let enrolled = crate::audiofile::read(Path::new(sample)).unwrap();
        std::fs::write(&voice, crate::record::wav_bytes(&enrolled)).unwrap();
        let setup = Setup {
            files: tse::Files::find(Path::new(dir)).expect("WeSep files"),
            threads: std::env::var("YAL_LIVETSE_THREADS")
                .map_or(threads(2), |t| t.parse().unwrap()),
            sample: voice.clone(),
        };
        let model = std::env::var("YAL_LIVETSE_DFN").expect("set YAL_LIVETSE_DFN");
        // 48 kHz: each 16 kHz sample three times, then smoothed.
        let mix = crate::audiofile::read(Path::new(recording)).unwrap();
        let tripled: Vec<f32> = mix
            .iter()
            .flat_map(|&v| [f32::from(v) / 32768.0; 3])
            .collect();
        let mic = Fir::new().filter(&tripled, 1);
        let frames = mic.len() / FRAME;
        let control = Control::default();
        control.enabled.store(true, Relaxed);
        let df_control = crate::denoise::Control::default();
        df_control.enabled.store(true, Relaxed);
        let (mut tx, mut rx) = ring::new(64);
        let (mut mid_tx, mut mid_rx) = ring::new(16);
        let (mut df_tx, mut df_rx) = ring::new(64);
        // The stages' copies, each numbered by the frame sent on from here:
        // (microphone lined up, TSE) from this stage, (TSE, SE) from
        // DeepFilterNet3.
        let (mut tse_tap_tx, mut tse_tap_rx) = ring::new(1024);
        let (mut se_tap_tx, mut se_tap_rx) = ring::new(1024);
        let total = frames + DELAY_FRAMES;
        let mut stages = vec![[[0f32; FRAME]; 3]; total];
        let mut late = Vec::new();
        let (models, models_rx) = mpsc::channel();
        models
            .send(crate::denoise::DeepFilter::new(Path::new(&model), 15.0).unwrap())
            .unwrap();
        thread::scope(|s| {
            let (rx, mid_tx, control) = (&mut rx, &mut mid_tx, &control);
            let tap = &mut tse_tap_tx;
            s.spawn(move || run(rx, mid_tx, Some(tap), control, setup));
            let (input, output, dc, tap) = (&mut mid_rx, &mut df_tx, &df_control, &mut se_tap_tx);
            s.spawn(move || crate::denoise::run_tapped(models_rx, input, output, dc, Some(tap)));
            let mut collect = |stages: &mut Vec<[[f32; FRAME]; 3]>| {
                while df_rx.pop().is_some() {}
                while let Some(c) = tse_tap_rx.pop() {
                    if let Some(st) = stages.get_mut(c.seq as usize) {
                        for (i, p) in c.samples().as_chunks::<2>().0.iter().enumerate() {
                            (st[0][i], st[1][i]) = (p[0], p[1]);
                        }
                    }
                }
                while let Some(c) = se_tap_rx.pop() {
                    if let Some(st) = stages.get_mut(c.seq as usize) {
                        for (i, p) in c.samples().as_chunks::<2>().0.iter().enumerate() {
                            st[2][i] = p[1];
                        }
                    }
                }
            };
            let started = Instant::now();
            for k in 0..total {
                let mut f = AudioFrame::new(1);
                f.seq = k as u64;
                if k < frames {
                    f.samples_mut()
                        .copy_from_slice(&mic[k * FRAME..(k + 1) * FRAME]);
                }
                assert!(tx.push(&f));
                let due = started + Duration::from_millis(10 * (k as u64 + 1));
                thread::sleep(due.saturating_duration_since(Instant::now()));
                collect(&mut stages);
                if ipc::TELEMETRY.live_tse.load(Relaxed) == LATE {
                    late.push(k);
                    ipc::TELEMETRY.live_tse.store(ON, Relaxed);
                }
            }
            thread::sleep(Duration::from_millis(100));
            collect(&mut stages);
            control.stop.store(true, Relaxed);
            df_control.stop.store(true, Relaxed);
        });
        let rtf = |handled: u64, busy: u64| {
            let audio = handled as f64 * 0.01;
            (
                audio,
                busy as f64 / 1e6,
                busy as f64 / 1e6 / audio.max(1e-9),
            )
        };
        let tse_rtf = rtf(control.handled.load(Relaxed), control.busy_us.load(Relaxed));
        let se_rtf = rtf(
            df_control.handled.load(Relaxed),
            df_control.busy_us.load(Relaxed),
        );
        println!(
            "{frames} frames, WeSep RTF {:.2}, SE RTF {:.2}, {} frames late{}",
            tse_rtf.2,
            se_rtf.2,
            late.len(),
            late.first().map_or(String::new(), |f| format!(
                ", from frame {f} to {}",
                late.last().unwrap()
            )),
        );
        // Each stage at 16 kHz, from the recording's start (the frames after
        // the delay).
        for (i, name) in ["input", "audio_tse", "audio_tse_se"].iter().enumerate() {
            let x: Vec<f32> = stages[DELAY_FRAMES..].iter().flat_map(|st| st[i]).collect();
            let x: Vec<i16> = Fir::new()
                .filter(&x[..x.len() / DOWN * DOWN], DOWN)
                .iter()
                .map(|&v| (v * 32768.0).clamp(-32768.0, 32767.0) as i16)
                .collect();
            crate::audiofile::write_m4a(&x, &out.join(format!("{name}.m4a"))).unwrap();
        }
        std::fs::write(
            out.join("rtf.csv"),
            format!(
                "step,audio_s,time_s,rtf,note\n\
                 tse,{:.2},{:.3},{:.3},WeSep 3 s chunks overlapping 1 s; {} of {frames} frames late (played as recorded)\n\
                 se,{:.2},{:.3},{:.3},DeepFilterNet3 15 dB\n",
                tse_rtf.0,
                tse_rtf.1,
                tse_rtf.2,
                late.len(),
                se_rtf.0,
                se_rtf.1,
                se_rtf.2
            ),
        )
        .unwrap();
        let _ = std::fs::remove_file(voice);
    }
}
