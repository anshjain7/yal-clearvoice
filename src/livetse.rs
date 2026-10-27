//! Live TSE, for testing: WeSep in the live chain, between the microphone
//! and DeepFilterNet3, switched on from the window (off by default).
//!
//! WeSep's public model looks at audio after each moment as well as before,
//! so it can't work frame by frame: this stage gathers 2 s of the
//! microphone, has a worker thread run WeSep on it (with the 0.5 s before it
//! as context), and plays the results out at a steady pace a fixed DELAY
//! behind, long enough for WeSep to finish each block in time. So while it's
//! on, the customer hears the agent about 4 s late: a test of what TSE does
//! to the call, not something to use on real calls. A causal model, trained
//! for this, would take its place with about 30 ms of delay.
//!
//! Off, frames pass straight through. A block WeSep can't do in time, or at
//! all, plays as it was recorded, or as silence when it's late.

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

/// 10 ms frames in a WeSep block: 2 s.
const BLOCK_FRAMES: usize = 200;
/// Frames the output runs behind the microphone while on: 4 s (a block's
/// 2 s, then up to 2 s for WeSep).
pub const DELAY_FRAMES: usize = 400;
/// Earlier audio given to WeSep with each block, 16 kHz samples: 0.5 s.
const CONTEXT: usize = 8000;
/// 48 kHz to 16 kHz.
const DOWN: usize = 3;
const FRAME: usize = 480;

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
    up: Fir,
    /// The block being gathered (48 kHz), and the 16 kHz audio before it.
    block: Vec<f32>,
    context: Vec<f32>,
    /// Microphone frames waiting to be played out (the delay line), and the
    /// cleaned audio ready to play, 48 kHz.
    mics: VecDeque<AudioFrame>,
    ready: VecDeque<f32>,
    /// Blocks sent to WeSep and not back yet, with their microphone audio,
    /// in case WeSep fails on them.
    pending: VecDeque<Vec<f32>>,
    /// Silence sent in place of audio that wasn't ready (48 kHz samples):
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
            up: Fir::new(),
            block: Vec::with_capacity(BLOCK_FRAMES * FRAME),
            context: Vec::new(),
            mics: (0..DELAY_FRAMES).map(|_| silent).collect(),
            ready: VecDeque::from(vec![0.0; DELAY_FRAMES * FRAME]),
            pending: VecDeque::new(),
            owed: 0,
        }
    }

    /// Queues audio to play, minus what silence already stood in for.
    fn ready(&mut self, audio: impl IntoIterator<Item = f32>) {
        let mut audio = audio.into_iter();
        while self.owed > 0 && audio.next().is_some() {
            self.owed -= 1;
        }
        self.ready.extend(audio);
    }

    /// Adds a microphone frame; once a block is full, returns it for WeSep:
    /// 16 kHz, its context first.
    fn push(&mut self, frame: &AudioFrame) -> Option<Vec<f32>> {
        self.mics.push_back(*frame);
        self.block.extend_from_slice(frame.samples());
        if self.block.len() < BLOCK_FRAMES * FRAME {
            return None;
        }
        let block = std::mem::take(&mut self.block);
        let low = self.down.filter(&block, DOWN);
        let job: Vec<f32> = self.context.iter().chain(&low).copied().collect();
        self.context = low[low.len() - CONTEXT..].to_vec();
        self.pending.push_back(block);
        Some(job)
    }

    /// WeSep's result for the oldest block sent (16 kHz, context first).
    fn finished(&mut self, cleaned: &[f32]) {
        let Some(block) = self.pending.pop_front() else {
            return;
        };
        let new = &cleaned[cleaned.len().saturating_sub(block.len() / DOWN)..];
        let mut stuffed = vec![0f32; new.len() * DOWN];
        for (i, v) in new.iter().enumerate() {
            stuffed[i * DOWN] = v * DOWN as f32;
        }
        let up = self.up.filter(&stuffed, 1);
        self.ready(up);
    }

    /// WeSep failed on the oldest block: it plays as recorded.
    fn finished_raw(&mut self) {
        if let Some(block) = self.pending.pop_front() {
            self.ready(block);
        }
    }

    /// The next frame out (cleaned, DELAY_FRAMES behind `now`), the
    /// microphone frame it came from, and whether it was late (silence).
    fn next(&mut self, now: &AudioFrame) -> (AudioFrame, AudioFrame, bool) {
        let mic = self.mics.pop_front().unwrap_or(AudioFrame::new(1));
        let mut out = AudioFrame::new(1);
        let late = self.ready.len() < FRAME;
        if late {
            // What's there is played next time with the rest; the frame's
            // worth after it is owed.
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
    for (generation, mix) in jobs {
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
                // A block plus its context, 16 kHz.
                let ctx = if sent.is_empty() { 0 } else { CONTEXT };
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
    fn a_block_wesep_misses_plays_as_recorded_and_late_ones_are_silence() {
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
        assert!(s.next(&tone_frame(DELAY_FRAMES)).2);
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

    /// The stage on a real recording, in real time, with today's WeSep:
    /// YAL_LIVETSE_TEST="<dir with onnxruntime.dll and wesep\>|<voice
    /// sample.wav>|<recording>|<out.wav>". Prints how far behind WeSep fell.
    /// With YAL_LIVETSE_DFN (DeepFilterNet3_onnx.tar.gz), the output goes
    /// through noise removal too, as in the app.
    /// cargo test --release live_tse_on_a_recording -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep files and a recording; runs in real time"]
    fn live_tse_on_a_recording() {
        use crate::ring;
        let spec = std::env::var("YAL_LIVETSE_TEST").expect("set YAL_LIVETSE_TEST");
        let [dir, sample, recording, out] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_LIVETSE_TEST needs 4 parts");
        };
        // The voice sample as the app keeps it: a 16 kHz WAV.
        let voice = std::path::Path::new(out).with_extension("voice.wav");
        let enrolled = crate::audiofile::read(std::path::Path::new(sample)).unwrap();
        std::fs::write(&voice, crate::record::wav_bytes(&enrolled)).unwrap();
        let setup = Setup {
            files: tse::Files::find(std::path::Path::new(dir)).expect("WeSep files"),
            threads: std::env::var("YAL_LIVETSE_THREADS")
                .map_or(threads(2), |t| t.parse().unwrap()),
            sample: voice.clone(),
        };
        // 48 kHz: each 16 kHz sample three times, then smoothed.
        let mix = crate::audiofile::read(std::path::Path::new(recording)).unwrap();
        let tripled: Vec<f32> = mix
            .iter()
            .flat_map(|&v| [f32::from(v) / 32768.0; 3])
            .collect();
        let mic = Fir::new().filter(&tripled, 1);
        let frames = mic.len() / FRAME;
        let control = Control::default();
        control.enabled.store(true, Relaxed);
        let (mut tx, mut rx) = ring::new(64);
        let (mut out_tx, mut out_rx) = ring::new(16);
        let (mut df_tx, mut df_rx) = ring::new(64);
        let (models, models_rx) = mpsc::channel();
        let df_control = crate::denoise::Control::default();
        df_control.enabled.store(true, Relaxed);
        let (mut late, mut sent) = (Vec::new(), Vec::new());
        thread::scope(|s| {
            let (rx, out_tx, control) = (&mut rx, &mut out_tx, &control);
            s.spawn(move || run(rx, out_tx, None, control, setup));
            let heard_rx = match std::env::var("YAL_LIVETSE_DFN") {
                Ok(model) => {
                    let df = crate::denoise::DeepFilter::new(std::path::Path::new(&model), 15.0);
                    models.send(df.unwrap()).unwrap();
                    let (input, output, dc) = (&mut out_rx, &mut df_tx, &df_control);
                    s.spawn(move || crate::denoise::run(models_rx, input, output, dc));
                    &mut df_rx
                }
                Err(_) => &mut out_rx,
            };
            let started = Instant::now();
            for k in 0..frames + DELAY_FRAMES {
                let mut f = AudioFrame::new(1);
                f.seq = k as u64;
                if k < frames {
                    f.samples_mut()
                        .copy_from_slice(&mic[k * FRAME..(k + 1) * FRAME]);
                }
                assert!(tx.push(&f));
                let due = started + Duration::from_millis(10 * (k as u64 + 1));
                thread::sleep(due.saturating_duration_since(Instant::now()));
                while let Some(o) = heard_rx.pop() {
                    sent.extend_from_slice(o.samples());
                }
                if ipc::TELEMETRY.live_tse.load(Relaxed) == LATE {
                    late.push(k);
                    ipc::TELEMETRY.live_tse.store(ON, Relaxed);
                }
            }
            control.stop.store(true, Relaxed);
            df_control.stop.store(true, Relaxed);
        });
        let (handled, busy) = (control.handled.load(Relaxed), control.busy_us.load(Relaxed));
        println!(
            "{frames} frames, WeSep handled {handled} ({:.2} s), RTF {:.2}, {} frames late{}",
            handled as f64 * 0.01,
            busy as f64 / 1e6 / (handled as f64 * 0.01).max(1e-9),
            late.len(),
            late.first().map_or(String::new(), |f| format!(
                ", from frame {f} to {}",
                late.last().unwrap()
            )),
        );
        // What the customer would have heard, from the delay on, at 16 kHz.
        let heard: Vec<i16> = Fir::new()
            .filter(&sent[..sent.len() / DOWN * DOWN], DOWN)
            .iter()
            .skip(DELAY_FRAMES * FRAME / DOWN)
            .map(|&v| (v * 32768.0).clamp(-32768.0, 32767.0) as i16)
            .collect();
        std::fs::write(out, crate::record::wav_bytes(&heard)).unwrap();
        let _ = std::fs::remove_file(voice);
    }
}
