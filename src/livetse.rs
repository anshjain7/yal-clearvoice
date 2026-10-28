//! Live TSE: WeSep in the live chain, between the microphone and
//! DeepFilterNet3, for the customer and the transcript alike, on a sliding
//! window (sliding.rs): every hop (0.32 s), WeSep on the 2.0 s before, the
//! hop and the 0.2 s after, only the hop kept. On this PC's CPU, or on a GPU
//! voice server when one is set up for it (`[voice]` url, remote_tse.rs). On
//! by default; the window can switch it off.
//!
//! Latency is about hop + lookahead + compute: about 0.67 s with a GPU
//! server. On a laptop CPU a window takes longer than a hop, so WeSep cleans
//! only some hops (the newest ready each time) and the others play as
//! recorded, at a fixed delay of hop + lookahead + `budget_ms` (1.8 s).
//! Speech enhancement (run_se.sh) follows in the chain: DeepFilterNet3.
//!
//! Off, frames pass straight through.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread;
use std::time::Duration;

use crate::frame::AudioFrame;
use crate::ipc;
use crate::remote_tse;
use crate::ring::{Consumer, Producer};
use crate::sliding::{self, Status, Streaming, Window};
use crate::stream;
use crate::tse;

/// The time allowed for WeSep on this PC's CPU, unless `budget_ms` says: a
/// 2.5 s window took about 1.5 s on the development PC, and a hop can wait
/// up to a hop for the one before.
pub const CPU_BUDGET_MS: u32 = 1800;

/// The stage's state for the window (`ipc::TELEMETRY.live_tse`).
pub const OFF: u8 = 0;
pub const ON: u8 = 1;
/// Switched on, but WeSep couldn't run (no voice sample, or its files
/// failed): the voice passes through as recorded, still delayed.
pub const FAILED: u8 = 2;
/// A hop wasn't ready in time (the PC too slow): it played as recorded.
pub const LATE: u8 = 3;
/// WeSep's files aren't there, nor a server: no live TSE (set at startup).
pub const UNAVAILABLE: u8 = 4;
/// The voice server can't be reached: the voice passes through as recorded.
pub const SERVER_DOWN: u8 = 5;
/// "PC too slow" stays up this many frames after a late hop (1 s), so the
/// window sees it.
const LATE_SHOWN: u64 = 100;

/// Shared between the stage and whoever drives it.
#[derive(Default)]
pub struct Control {
    /// Live TSE wanted.
    pub enabled: AtomicBool,
    pub stop: AtomicBool,
    /// Frames WeSep has cleaned while on, and the time it took (µs): its
    /// real-time factor, for stages.rs's rtf.csv (on this PC only).
    pub handled: AtomicU64,
    pub busy_us: AtomicU64,
}

/// WeSep's threads on the live call: half the PC's logical processors, 2 to
/// 4 (2 threads fell behind on the development PC on battery), or `wanted`
/// (settings.ini) if more.
pub fn threads(wanted: usize) -> usize {
    let half = thread::available_parallelism().map_or(2, |n| n.get() / 2);
    half.clamp(2, 4).max(wanted)
}

/// What WeSep works with: on a voice server (used when set), the stream's
/// settings; on this PC, its files, threads, the window and how much of the
/// microphone to mix back in; and the voice sample.
pub struct Setup {
    pub files: Option<tse::Files>,
    pub threads: usize,
    pub sample: PathBuf,
    pub remote: Option<remote_tse::Settings>,
    pub window: Window,
    pub remix: f32,
}

impl Setup {
    /// How far behind the microphone the step plays, in 10 ms frames.
    pub fn delay_frames(&self) -> usize {
        self.remote
            .as_ref()
            .map_or(self.window, |r| r.window)
            .delay_frames()
    }

    /// The stream, with WeSep on the server or on this PC.
    fn start(&self) -> Option<Streaming> {
        let delay = self.delay_frames();
        if let Some(r) = &self.remote {
            return Some(Streaming::start(
                delay,
                remote_tse::feed(r.clone(), self.sample.clone()),
            ));
        }
        let files = self.files.clone()?;
        Some(Streaming::start(
            delay,
            sliding::local(
                files,
                self.threads,
                self.sample.clone(),
                self.window,
                self.remix,
            ),
        ))
    }
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
    let down = if setup.remote.is_some() {
        SERVER_DOWN
    } else {
        FAILED
    };
    let _ = stream::realtime(|| {
        let mut on: Option<Streaming> = None;
        let mut copy = AudioFrame::new(2);
        let (mut frames, mut late_until) = (0u64, 0u64);
        while !control.stop.load(Relaxed) {
            let Some(frame) = input.pop() else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            frames += 1;
            let wanted = control.enabled.load(Relaxed);
            if wanted != on.is_some() {
                on = if wanted { setup.start() } else { None };
                state(if wanted { ON } else { OFF });
            }
            let (out, mic) = match on.as_mut() {
                None => (frame, frame),
                Some(s) => {
                    let (out, mic, status) = s.next(&frame);
                    let (cleaned, took) = s.take_work();
                    control.handled.fetch_add(cleaned, Relaxed);
                    control.busy_us.fetch_add(took.as_micros() as u64, Relaxed);
                    if status == Status::Late {
                        late_until = frames + LATE_SHOWN;
                    }
                    state(match status {
                        Status::Down => down,
                        _ if frames < late_until => LATE,
                        _ => ON,
                    });
                    (out, mic)
                }
            };
            send_tap(tap.as_deref_mut(), &mut copy, &mic, &out);
            let _ = output.push(&out);
        }
        Ok(())
    });
}

/// A copy for the stage files (stages.rs): stereo, the microphone `mic`
/// (lined up) and what was sent on, `out`.
fn send_tap(tap: Option<&mut Producer>, copy: &mut AudioFrame, mic: &AudioFrame, out: &AudioFrame) {
    if let Some(t) = tap {
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
        let _ = t.push(copy);
    }
}

fn state(s: u8) {
    ipc::TELEMETRY.live_tse.store(s, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhance::Fir;
    use std::sync::mpsc;
    use std::time::Instant;

    const DOWN: usize = 3;
    const FRAME: usize = 480;

    /// The live chain on a real recording, in real time, as in the app: this
    /// stage (WeSep on the sliding window, on this PC's CPU, or with
    /// YAL_LIVETSE_REMOTE on a voice server), then
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
            files: Some(tse::Files::find(Path::new(dir)).expect("WeSep files")),
            threads: std::env::var("YAL_LIVETSE_THREADS")
                .map_or(threads(2), |t| t.parse().unwrap()),
            sample: voice.clone(),
            // YAL_LIVETSE_REMOTE: a settings.ini with a `[voice]` url, for
            // WeSep on that server instead (remote_tse.rs).
            remote: std::env::var("YAL_LIVETSE_REMOTE").ok().map(|ini| {
                remote_tse::Settings::parse(&std::fs::read_to_string(ini).unwrap())
                    .expect("a [voice] url")
            }),
            // YAL_LIVETSE_INI: a settings.ini with the window's `[voice]`
            // settings for this PC.
            window: Window::parse(
                &std::env::var("YAL_LIVETSE_INI")
                    .map(|ini| std::fs::read_to_string(ini).unwrap())
                    .unwrap_or_default(),
                CPU_BUDGET_MS,
            ),
            remix: 0.0,
        };
        // How far behind the microphone the step plays.
        let skip = setup.delay_frames();
        let shape = setup.remote.as_ref().map_or(setup.window, |r| r.window);
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
        let total = frames + skip;
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
            // YAL_LIVETSE_SLOWER: feed the recording this many times slower
            // than real time (what WeSep would give on a faster computer).
            let pace =
                10 * std::env::var("YAL_LIVETSE_SLOWER").map_or(1, |x| x.parse::<u64>().unwrap());
            let mut started = Instant::now();
            for k in 0..total {
                let mut f = AudioFrame::new(1);
                f.seq = k as u64;
                if k < frames {
                    f.samples_mut()
                        .copy_from_slice(&mic[k * FRAME..(k + 1) * FRAME]);
                }
                assert!(tx.push(&f));
                if k == 0 {
                    // As in the app, where the filter is on from the start:
                    // WeSep and the voice load before the talking does.
                    thread::sleep(Duration::from_secs(3));
                    started = Instant::now() - Duration::from_millis(10);
                }
                let due = started + Duration::from_millis(pace * (k as u64 + 1));
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
            let x: Vec<f32> = stages[skip..].iter().flat_map(|st| st[i]).collect();
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
                 tse,{:.2},{:.3},{:.3},WeSep sliding window {} ms + {} ms hop + {} ms lookahead; {} of {frames} frames late (played as recorded)\n\
                 se,{:.2},{:.3},{:.3},DeepFilterNet3 15 dB\n",
                tse_rtf.0,
                tse_rtf.1,
                tse_rtf.2,
                shape.context_ms,
                shape.hop_ms,
                shape.lookahead_ms,
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
