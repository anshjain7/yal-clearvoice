//! The smallest wrapper around DeepFilterNet3's native runtime (libDF's
//! `df::tract`): one mono 48 kHz model, one 10 ms frame per call.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};

use crate::format::AudioFormat;
use crate::frame::AudioFrame;
use crate::ipc;
use crate::ring::{Consumer, Producer};
use crate::stream;

/// A loaded DeepFilterNet3 model. Dropping it frees everything.
pub struct DeepFilter {
    model: DfTract,
    /// Copy taken right after loading; `reset` returns to it.
    fresh: DfTract,
    /// The model's algorithmic delay in 10 ms frames (3 standard, 1 low-latency).
    pub delay_frames: u64,
}

// SAFETY: DfTract isn't Send only because tract keeps runtime values in `Rc`
// and op states in `Box<dyn OpState>`. A DeepFilter owns that whole graph (the
// pristine copy shares `Rc`s only with the live model, both in here) and one
// thread uses it at a time, so moving it between threads as a unit can't race
// on a reference count. This is what lets a model load on a background thread
// while the audio thread keeps running. libDF's LADSPA plugin likewise hands
// models to worker threads.
unsafe impl Send for DeepFilter {}

/// Delay in frames before any model has loaded: DeepFilterNet3's standard model
/// (10 ms STFT overlap + 20 ms lookahead). The actual delay is taken from each
/// model once loaded, so the low-latency model (10 ms) works too. The stage keeps
/// this delay in bypass as well, so DeepFilterNet can fail, restart, be toggled or
/// overload without moving the audio.
pub const DEFAULT_DELAY_FRAMES: u64 = 3;
/// Largest model delay the recent-frame history can line up a dry signal against.
const MAX_DELAY_FRAMES: u64 = 7;
/// The most noise DeepFilterNet takes out, in dB, unless settings.ini's
/// `[noise] attenuation_db` says otherwise. Taking out everything (100, no
/// limit) also takes the life out of the voice: it sounds muffled, as if in a
/// closed room, and quiet syllables go with the noise. Headsets like the
/// Detecta 201 already cut background noise at their noise-cancelling
/// microphone, so a limit leaves the voice natural and the remaining noise
/// far down. Below the limit, libDF mixes that much of the original back in.
pub const DEFAULT_ATTENUATION_DB: f32 = 15.0;
/// The range settings.ini may set: 6 dB (barely any) to 100 (no limit).
const ATTENUATION_RANGE: std::ops::RangeInclusive<f32> = 6.0..=100.0;
/// Model failures in a row before the stage drops the model and bypasses.
const MAX_FAILURES: u32 = 5;
/// Frames in a row over the 10 ms budget (or falling behind) before the stage
/// bypasses to keep up.
const MAX_OVERLOADS: u32 = 10;
/// Frames still waiting once a frame is done that mean the stage is falling
/// behind (40 ms): frames slow on average pile up even if some are on time.
const BEHIND_FRAMES: usize = 4;
/// One frame's real-time budget: a 10 ms frame must be handled within its own 10 ms.
const BUDGET: Duration = Duration::from_micros(10_000);
/// Frames to stay in bypass after an overload before retrying DeepFilterNet (~5 s).
const COOLDOWN_FRAMES: u64 = 500;
/// Denoised/original mix change per sample: a full switch takes one 10 ms frame.
const FADE_STEP: f32 = 1.0 / 480.0;

/// Shared between the DeepFilterNet stage and whoever drives it.
pub struct Control {
    /// DeepFilterNet wanted (true) or bypassed on purpose (false).
    pub enabled: AtomicBool,
    /// Set by the stage while it has a working model; cleared when it drops a
    /// failing one. Watch it to report failures.
    pub active: AtomicBool,
    pub stop: AtomicBool,
    /// Frames handled so far, and the processing time they took (µs): their
    /// real-time factor, for whoever reads them (stages.rs's rtf.csv).
    pub handled: AtomicU64,
    pub busy_us: AtomicU64,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            active: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            handled: AtomicU64::new(0),
            busy_us: AtomicU64::new(0),
        }
    }
}

/// What `run` measured.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Frames handed on, denoised or not.
    pub frames: u64,
    /// Frames that went out entirely unprocessed (bypass, warm-up, failures).
    pub bypassed: u64,
    /// Frames dropped because the output ring was full.
    pub dropped: u64,
    /// Frames missing from the input (dropped before they reached this stage).
    pub gaps: u64,
    /// Frames the model failed or panicked on, plus panics anywhere in the stage.
    pub errors: u64,
    /// Models installed: the first load plus restarts.
    pub loads: u64,
    /// The stage dropped a failing model at least once and fell back to bypass.
    pub failed: bool,
    /// The stage bypassed at least once because DeepFilterNet couldn't keep up.
    pub overloaded: bool,
    /// The loaded model's delay, in 10 ms frames (the default until one loads).
    pub delay_frames: u64,
    pub busy_total: Duration,
    pub busy_max: Duration,
}

/// The stage between capture and playback. It starts in bypass (audio passes
/// through unprocessed) and switches to DeepFilterNet3 when a model arrives on
/// `models` and `enabled` is set; sending another model restarts it. Bypass
/// keeps the model's delay, so these changes crossfade over 10 ms instead of
/// moving the audio, and a new model warms up for its delay before fading in.
/// A frame the model fails or panics on plays unprocessed and the model
/// resets; after `MAX_FAILURES` in a row, or a panic elsewhere in the stage,
/// the model is dropped, `control.active` clears and the audio keeps flowing
/// in bypass until a model is sent again. If the model can't hold the 10 ms
/// budget for `MAX_OVERLOADS` frames running, the stage crossfades to bypass for
/// a few seconds and retries, so a slow PC degrades to clean passthrough instead
/// of dropping frames into the call. Frames keep their `seq` and are stamped with
/// the capture time of the audio they hold.
///
/// Give it its own thread: libDF allocates per frame, so it must not run on a
/// WASAPI thread. It gets Pro Audio priority like them, since a late frame
/// here is a gap at the speaker.
pub fn run(
    models: Receiver<DeepFilter>,
    input: &mut Consumer,
    output: &mut Producer,
    control: &Control,
) -> Stats {
    run_tapped(models, input, output, control, None)
}

/// `run`, also handing each frame to `tap` as it goes out: a stereo frame of
/// the microphone (left) and what went out (right), lined up for the model's
/// delay. The hand-off never waits: when `tap` is full the copy is dropped,
/// never the frame going out.
pub fn run_tapped(
    models: Receiver<DeepFilter>,
    input: &mut Consumer,
    output: &mut Producer,
    control: &Control,
    mut tap: Option<&mut Producer>,
) -> Stats {
    let mut stats = Stats::default();
    let _ = stream::realtime(|| {
        // Restart in bypass rather than let a panic silence the pipeline.
        while catch_unwind(AssertUnwindSafe(|| {
            pump(
                &models,
                input,
                output,
                control,
                &mut stats,
                tap.as_deref_mut(),
            )
        }))
        .is_err()
        {
            control.active.store(false, Relaxed);
            stats.errors += 1;
            stats.failed = true;
        }
        Ok(())
    });
    stats
}

fn pump(
    models: &Receiver<DeepFilter>,
    input: &mut Consumer,
    output: &mut Producer,
    control: &Control,
    stats: &mut Stats,
    mut tap: Option<&mut Producer>,
) {
    let mut model: Option<DeepFilter> = None;
    let mut copy = AudioFrame::new(2);
    // Recent input frames by seq: the unprocessed audio, `delay` frames back,
    // lines up with what the model outputs now.
    let mut recent = [AudioFrame::new(1); MAX_DELAY_FRAMES as usize + 1];
    let mut next_seq = None;
    let (mut wet, mut out) = (AudioFrame::new(1), AudioFrame::new(1));
    let (mut running, mut warmup, mut failures, mut mix) = (false, 0u64, 0u32, 0.0f32);
    // The loaded model's delay (the default until one loads), and the overload
    // watchdog and the seq it has the stage bypass until.
    let mut delay = DEFAULT_DELAY_FRAMES;
    let (mut watchdog, mut paused_until) = (Watchdog::default(), 0u64);
    stats.delay_frames = delay;
    while !control.stop.load(Relaxed) {
        let Some(frame) = input.pop() else {
            thread::sleep(Duration::from_millis(1));
            continue;
        };
        // A freshly loaded model (first load or restart) takes over, with its
        // own delay.
        if let Ok(new) = models.try_recv() {
            delay = new.delay_frames;
            stats.delay_frames = delay;
            model = Some(new);
            (running, failures, watchdog, paused_until) = (false, 0, Watchdog::default(), 0);
            control.active.store(true, Relaxed);
            stats.loads += 1;
        }
        match next_seq {
            // The microphone came back as a new capture session: the model's
            // history is from before the break, so restart it.
            Some(next) if frame.seq < next => running = false,
            Some(next) => stats.gaps += frame.seq - next,
            None => {}
        }
        next_seq = Some(frame.seq + 1);
        recent[frame.seq as usize % recent.len()] = frame;
        let dry = frame
            .seq
            .checked_sub(delay)
            .map(|seq| recent[seq as usize % recent.len()])
            .filter(|f| f.seq + delay == frame.seq)
            .unwrap_or(AudioFrame::new(1));

        let started = Instant::now();
        // While recovering from an overload, behave as a deliberate bypass: the
        // crossfade and model-reset paths below then handle it with no clicks.
        let overloaded = frame.seq < paused_until;
        let want = control.enabled.load(Relaxed) && !overloaded;
        let mut ready = false;
        let mut give_up = false;
        // Keep running through a fade-out, so it fades from real output.
        if let Some(m) = model.as_mut()
            && (want || mix > 0.0)
        {
            if !running {
                m.reset();
                (running, warmup) = (true, delay);
            }
            let ok = catch_unwind(AssertUnwindSafe(|| {
                m.process(frame.samples(), wet.samples_mut())
            }))
            .is_ok_and(|result| result.is_ok())
                && wet.samples().iter().all(|s| s.is_finite());
            if !ok {
                stats.errors += 1;
                failures += 1;
                give_up = failures >= MAX_FAILURES;
                m.reset();
                warmup = delay;
            } else if warmup > 0 {
                failures = 0;
                warmup -= 1;
            } else {
                failures = 0;
                ready = true;
            }
        } else {
            running = false;
        }
        if give_up {
            model = None;
            control.active.store(false, Relaxed);
            stats.failed = true;
        }
        if !ready {
            wet = dry;
        }
        let target = if want && ready { 1.0 } else { 0.0 };
        for ((o, d), w) in out
            .samples_mut()
            .iter_mut()
            .zip(dry.samples())
            .zip(wet.samples())
        {
            mix += (target - mix).clamp(-FADE_STEP, FADE_STEP);
            *o = d + (w - d) * mix;
        }
        let busy = started.elapsed();
        stats.busy_total += busy;
        stats.busy_max = stats.busy_max.max(busy);
        // Overload watchdog, once the model is steady (past warm-up): if the
        // CPU can't keep up, bypass for a cooldown and retry.
        if !ready {
            watchdog = Watchdog::default();
        } else if watchdog.frame(busy, input.queued()) {
            paused_until = frame.seq + COOLDOWN_FRAMES;
            stats.overloaded = true;
        }

        // Publish processing time and DeepFilterNet state for the UI.
        ipc::TELEMETRY
            .busy_us
            .store(busy.as_micros() as u64, Relaxed);
        ipc::TELEMETRY
            .busy_peak_us
            .fetch_max(busy.as_micros() as u64, Relaxed);
        ipc::TELEMETRY
            .mic_peak
            .fetch_max(level(frame.samples()), Relaxed);
        ipc::TELEMETRY
            .sent_peak
            .fetch_max(level(out.samples()), Relaxed);
        let state = if give_up || (stats.failed && model.is_none()) {
            ipc::DFN_FAILED
        } else if frame.seq < paused_until {
            ipc::DFN_OVERLOADED
        } else if ready {
            ipc::DFN_RUNNING
        } else {
            ipc::DFN_BYPASS
        };
        ipc::TELEMETRY.dfn_state.store(state, Relaxed);

        if mix == 0.0 {
            stats.bypassed += 1;
        }
        out.seq = frame.seq;
        out.captured = dry.captured;
        if output.push(&out) {
            stats.frames += 1;
        } else {
            stats.dropped += 1;
        }
        control.handled.fetch_add(1, Relaxed);
        control.busy_us.fetch_add(busy.as_micros() as u64, Relaxed);
        if let Some(t) = tap.as_deref_mut() {
            for (pair, (d, o)) in copy
                .samples_mut()
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(dry.samples().iter().zip(out.samples()))
            {
                pair[0] = *d;
                pair[1] = *o;
            }
            // Numbered as the frame it came from (`dry`), so a stage before
            // this one can line its own copies up with it.
            copy.seq = frame.seq.saturating_sub(delay);
            // Full: the copy is dropped, never the frame going out.
            let _ = t.push(&copy);
        }
    }
}

/// A frame's RMS in millionths of full scale, for `ipc::TELEMETRY`'s levels.
fn level(samples: &[f32]) -> u64 {
    let power = samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32;
    (power.sqrt() * 1e6) as u64
}

/// Decides when DeepFilterNet can't keep up on this PC: `MAX_OVERLOADS` frames
/// in a row that each took over their 10 ms, or that left the stage behind.
/// A brief spike, or a burst from the microphone it then catches up on, resets it.
#[derive(Default)]
struct Watchdog {
    over: u32,
}

impl Watchdog {
    /// After a frame from a steady model, with how long it took and how many
    /// frames are still waiting: true when it's time to bypass.
    fn frame(&mut self, busy: Duration, waiting: usize) -> bool {
        if busy > BUDGET || waiting >= BEHIND_FRAMES {
            self.over += 1;
        } else {
            self.over = 0;
        }
        let tripped = self.over >= MAX_OVERLOADS;
        if tripped {
            self.over = 0;
        }
        tripped
    }
}

/// `[noise] attenuation_db` from settings.ini's text, within 6..=100 dB;
/// `DEFAULT_ATTENUATION_DB` when it isn't set or isn't a number.
pub fn attenuation_from_ini(ini: &str) -> f32 {
    let mut section = "";
    for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
        } else if section == "noise"
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == "attenuation_db"
            && let Ok(db) = v.split(';').next().unwrap_or("").trim().parse::<f32>()
        {
            return db.clamp(*ATTENUATION_RANGE.start(), *ATTENUATION_RANGE.end());
        }
    }
    DEFAULT_ATTENUATION_DB
}

impl DeepFilter {
    /// Loads a model archive (`.tar.gz` of `enc.onnx`, `erb_dec.onnx`,
    /// `df_dec.onnx` and `config.ini`) and initializes the runtime for one
    /// channel: `DfParams::new` then `DfTract::new`. Slow (tract optimizes the
    /// graphs), so call it off the audio threads.
    /// `attenuation_db` caps how much noise it removes (see
    /// `DEFAULT_ATTENUATION_DB`); 100 or more removes all it can.
    pub fn new(model: &Path, attenuation_db: f32) -> Result<Self> {
        // Below `min_db_thresh` of local SNR, libDF treats a frame as noise
        // only and silences it outright. Its default of -10 dB also silences
        // quiet speech in a loud room, heard as the voice cutting out; -15 dB
        // is what libDF's own command line and C API use. The other two
        // thresholds stay at their defaults (raising them costs CPU).
        let model = DfTract::new(
            DfParams::new(model.to_path_buf())?,
            &RuntimeParams::default_with_ch(1)
                .with_thresholds(-15.0, 30.0, 20.0)
                .with_atten_lim(attenuation_db),
        )?;
        let want = AudioFormat::MIC;
        ensure!(
            model.sr == want.sample_rate as usize && model.hop_size == want.frames,
            "model runs at {} Hz in {}-sample frames; ClearVoice needs {} Hz in {}",
            model.sr,
            model.hop_size,
            want.sample_rate,
            want.frames
        );
        // STFT overlap plus lookahead, as libDF's own CLI computes it. Whole
        // frames only, within what the stage's history can line up a dry signal
        // against: 3 for the standard model, 1 for the low-latency model.
        let delay = model.fft_size - model.hop_size + model.lookahead * model.hop_size;
        let frames = delay / model.hop_size;
        ensure!(
            delay % model.hop_size == 0 && (1..=MAX_DELAY_FRAMES as usize).contains(&frames),
            "model delay is {delay} samples ({} frames); ClearVoice supports 1..={MAX_DELAY_FRAMES}",
            delay as f32 / model.hop_size as f32
        );
        Ok(Self {
            delay_frames: frames as u64,
            fresh: model.clone(),
            model,
        })
    }

    /// Denoises one 480-sample mono frame (`DfTract::process`). The output lags
    /// the input by the model's delay: 30 ms for DeepFilterNet3. Returns the
    /// local SNR estimate in dB. This allocates, so keep it off the WASAPI threads.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<f32> {
        let n = self.model.hop_size;
        let input = ArrayView2::from_shape((1, n), input)?;
        let output = ArrayViewMut2::from_shape((1, n), output)?;
        self.model.process(input, output)
    }

    /// Forgets all audio history, as if just loaded (e.g. after a device change).
    pub fn reset(&mut self) {
        self.model = self.fresh.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const MODEL: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/models/DeepFilterNet3_onnx.tar.gz"
    );

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn loads_processes_resets_and_drops() {
        assert!(DeepFilter::new(Path::new("missing.tar.gz"), 100.0).is_err());
        let mut df = DeepFilter::new(Path::new(MODEL), 100.0).unwrap();
        assert_eq!(
            df.delay_frames, 3,
            "standard DeepFilterNet3 delay is 3 frames"
        );
        assert_eq!(
            df.model.min_db_thresh, -15.0,
            "quiet speech mustn't be silenced"
        );

        // 100 ms of a tone in noise, as ten 480-sample frames.
        let input: Vec<f32> = (0..4800)
            .map(|i| {
                0.2 * (i as f32 * 0.05).sin() + 0.05 * ((i * 7919 % 1000) as f32 / 500.0 - 1.0)
            })
            .collect();
        let run = |df: &mut DeepFilter| {
            let mut out = vec![0.0; input.len()];
            for (i, o) in input.chunks(480).zip(out.chunks_mut(480)) {
                df.process(i, o).unwrap();
            }
            out
        };
        let first = run(&mut df);
        assert!(first.iter().all(|s| s.is_finite()));
        assert!(
            first.iter().any(|&s| s != 0.0),
            "model produced only silence"
        );

        df.reset();
        assert_eq!(
            run(&mut df),
            first,
            "reset should restore the just-loaded state"
        );
        assert!(
            df.process(&input[..479], &mut [0.0; 479]).is_err(),
            "wrong frame size"
        );
        drop(df);
    }

    #[test]
    fn watchdog_trips_on_slow_or_falling_behind_frames_only() {
        let ms = Duration::from_millis;
        let mut w = Watchdog::default();
        // Nine slow frames, then one on time: a spike, not an overload.
        assert!(!(0..9).any(|_| w.frame(ms(12), 0)));
        assert!(!w.frame(ms(5), 0));
        // Ten slow frames in a row: overloaded.
        assert!(!(0..9).any(|_| w.frame(ms(12), 0)));
        assert!(w.frame(ms(12), 0));
        // Each frame on time, yet 40 ms or more of audio piling up: overloaded.
        assert!(!(0..9).any(|_| w.frame(ms(9), 5)));
        assert!(w.frame(ms(9), 5));
        // A burst from the microphone, caught up on quickly: fine.
        assert!(!(0..=10).rev().any(|waiting| w.frame(ms(3), waiting)));
    }

    #[test]
    fn attenuation_comes_from_settings_within_range() {
        let read = attenuation_from_ini;
        assert_eq!(read(""), DEFAULT_ATTENUATION_DB);
        assert_eq!(read("[noise]\nattenuation_db = 18 ; Detecta\n"), 18.0);
        assert_eq!(read("[noise]\nattenuation_db = 2\n"), 6.0);
        assert_eq!(read("[noise]\nattenuation_db = 300\n"), 100.0);
        assert_eq!(
            read("[noise]\nattenuation_db = lots\n"),
            DEFAULT_ATTENUATION_DB
        );
        assert_eq!(
            read("[summary]\nattenuation_db = 10\n"),
            DEFAULT_ATTENUATION_DB
        );
    }

    /// With a limit, pure noise comes out that far down, not all but gone.
    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn attenuation_limit_keeps_noise_that_far_down() {
        let mut seed = 7u32;
        let noise: Vec<f32> = (0..300 * 480)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.05 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect();
        let reduction = |db: f32| {
            let mut df = DeepFilter::new(Path::new(MODEL), db).unwrap();
            let mut out = vec![0.0; noise.len()];
            for (i, o) in noise.chunks(480).zip(out.chunks_mut(480)) {
                df.process(i, o).unwrap();
            }
            // The last second, once the model has settled.
            let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
            let tail = noise.len() - 48_000..noise.len();
            20.0 * (rms(&out[tail.clone()]) / rms(&noise[tail])).log10()
        };
        let limit = DEFAULT_ATTENUATION_DB;
        let (full, limited) = (reduction(100.0), reduction(limit));
        println!("noise reduction: {full:.1} dB without a limit, {limited:.1} dB with {limit} dB");
        assert!(
            full < -30.0,
            "without a limit it removes far more: {full:.1} dB"
        );
        assert!(
            (-limit - 2.0..=-limit + 4.0).contains(&limited),
            "{limit} dB limit gave {limited:.1} dB"
        );
    }

    /// `frames` frames of a continuous 200 Hz tone, starting at phase zero.
    fn tone(frames: usize) -> Vec<AudioFrame> {
        (0..frames)
            .map(|k| {
                let mut f = AudioFrame::new(1);
                f.seq = k as u64;
                for (i, s) in f.samples_mut().iter_mut().enumerate() {
                    let t = (k * 480 + i) as f32 / 48_000.0;
                    *s = 0.3 * (std::f32::consts::TAU * 200.0 * t).sin();
                }
                f
            })
            .collect()
    }

    fn model() -> DeepFilter {
        DeepFilter::new(Path::new(MODEL), 100.0).unwrap()
    }

    /// Runs the stage on its own thread and feeds it `frames` one at a time,
    /// calling `before(i, models)` ahead of frame `i`; a model sent there is
    /// in place for that frame.
    fn drive(
        frames: &[AudioFrame],
        control: &Control,
        mut before: impl FnMut(usize, &mpsc::Sender<DeepFilter>),
    ) -> (Vec<AudioFrame>, Stats) {
        use crate::ring;
        let (mut tx, mut rx) = ring::new(8);
        let (mut out_tx, mut out_rx) = ring::new(8);
        let (models, models_rx) = mpsc::channel();
        thread::scope(|s| {
            let stage = s.spawn(move || run(models_rx, &mut rx, &mut out_tx, control));
            let mut out = Vec::new();
            for (i, frame) in frames.iter().enumerate() {
                before(i, &models);
                assert!(tx.push(frame));
                let waiting = Instant::now();
                out.push(loop {
                    if let Some(o) = out_rx.pop() {
                        break o;
                    }
                    assert!(waiting.elapsed() < Duration::from_secs(10), "stage stalled");
                    thread::sleep(Duration::from_millis(1));
                });
            }
            control.stop.store(true, Relaxed);
            (out, stage.join().unwrap())
        })
    }

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn tap_gets_each_frame_out_beside_its_microphone_audio() {
        use crate::ring;
        let input = tone(30);
        let control = Control::default();
        let (mut tx, mut rx) = ring::new(64);
        let (mut out_tx, mut out_rx) = ring::new(64);
        let (mut tap_tx, mut tap_rx) = ring::new(64);
        let (models, models_rx) = mpsc::channel();
        models.send(model()).unwrap();
        let outs = thread::scope(|s| {
            let stage = s.spawn(|| {
                let tap = Some(&mut tap_tx);
                run_tapped(models_rx, &mut rx, &mut out_tx, &control, tap)
            });
            for f in &input {
                assert!(tx.push(f));
            }
            let (mut outs, started) = (Vec::new(), Instant::now());
            while outs.len() < input.len() {
                match out_rx.pop() {
                    Some(o) => outs.push(o),
                    None => {
                        assert!(started.elapsed() < Duration::from_secs(10), "stage stalled");
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            control.stop.store(true, Relaxed);
            stage.join().unwrap();
            outs
        });
        for (i, o) in outs.iter().enumerate() {
            let copy = tap_rx.pop().expect("a copy of every frame sent");
            assert_eq!((copy.channels(), copy.seq), (2, o.seq.saturating_sub(3)));
            let (mic, sent): (Vec<f32>, Vec<f32>) = copy
                .samples()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| (p[0], p[1]))
                .unzip();
            // Right: exactly what went out. Left: the microphone frame it came
            // from, the model's 3 frames earlier.
            assert_eq!(sent, o.samples());
            let expected = i.checked_sub(3).map_or(AudioFrame::new(1), |j| input[j]);
            assert_eq!(mic, expected.samples());
        }
        assert!(tap_rx.pop().is_none());
        assert_eq!(control.handled.load(Relaxed), input.len() as u64);
        assert!(control.busy_us.load(Relaxed) > 0);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // the stage sets Windows thread priority
    fn bypass_without_model_passes_audio_through_3_frames_late() {
        let input = tone(20);
        let control = Control::default();
        let (out, stats) = drive(&input, &control, |_, _| {});
        for (i, f) in out.iter().enumerate() {
            assert_eq!(f.seq, i as u64);
            let expected = i.checked_sub(3).map_or(AudioFrame::new(1), |j| input[j]);
            assert_eq!(f.samples(), expected.samples());
        }
        assert_eq!((stats.frames, stats.bypassed, stats.loads), (20, 20, 0));
        assert!(!control.active.load(Relaxed));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn toggling_crossfades_without_jumps() {
        let input = tone(150);
        let control = Control::default();
        let (out, stats) = drive(&input, &control, |i, models| match i {
            0 => models.send(model()).unwrap(),
            50 => control.enabled.store(false, Relaxed),
            100 => control.enabled.store(true, Relaxed),
            _ => {}
        });
        assert!(out.iter().enumerate().all(|(i, f)| f.seq == i as u64));
        let samples: Vec<f32> = out.iter().flat_map(|f| f.samples().to_vec()).collect();
        let jump = samples
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        // The tone itself moves at most 0.008 per sample; a click is a step of ~0.3.
        assert!(jump < 0.05, "click: a jump of {jump}");
        // 3 warm-up frames at start, frames 50..100 bypassed, 3 warm-up frames again.
        assert_eq!((stats.errors, stats.bypassed), (0, 56));
        // A normal-speed run must not trip the overload watchdog.
        assert!(!stats.overloaded, "overload watchdog false-tripped");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn fails_over_to_bypass_and_restarts() {
        let mut input = tone(60);
        for f in &mut input[10..20] {
            f.samples_mut()[0] = f32::NAN;
        }
        let control = Control::default();
        let mut active_before_restart = true;
        let (out, stats) = drive(&input, &control, |i, models| match i {
            0 => models.send(model()).unwrap(),
            30 => {
                active_before_restart = control.active.load(Relaxed);
                models.send(model()).unwrap();
            }
            _ => {}
        });
        assert_eq!(out.len(), 60);
        assert_eq!((stats.errors, stats.loads), (MAX_FAILURES as u64, 2));
        assert!(stats.failed && !active_before_restart && control.active.load(Relaxed));
        // Failed over, it passed the original through, 3 frames late...
        assert_eq!(out[29].samples(), input[26].samples());
        // ...and once restarted, it denoises again.
        assert_ne!(out[59].samples(), input[56].samples());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn restarts_the_model_when_capture_restarts() {
        // Seq starting over is what a reconnected microphone looks like.
        let input: Vec<AudioFrame> = tone(20).into_iter().chain(tone(20)).collect();
        let control = Control::default();
        let (out, stats) = drive(&input, &control, |i, models| {
            if i == 0 {
                models.send(model()).unwrap();
            }
        });
        assert_eq!((out.len(), stats.gaps, stats.errors), (40, 0, 0));
        // Warm-up after the first load, and again after the restart.
        assert_eq!(stats.bypassed, 2 * DEFAULT_DELAY_FRAMES);
    }

    /// Diagnostic: denoises 10 s of synthetic audio (2 s of noise, then a
    /// voice-like tone in noise) and reports timing and CPU use. Nothing is
    /// recorded or saved.
    /// cargo test --release processing_time -- --ignored --nocapture
    #[test]
    #[ignore = "timing: run alone with --release"]
    fn processing_time() {
        use std::f32::consts::TAU;
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let mut df = DeepFilter::new(Path::new(MODEL), 100.0).unwrap();
        let load = started.elapsed();

        let mut seed = 1u32;
        let input: Vec<f32> = (0..1000 * 480)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = 0.05 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0);
                let t = i as f32 / 48_000.0;
                let envelope = 0.5 + 0.5 * (TAU * 4.0 * t).sin();
                let voice = (TAU * 200.0 * t).sin() + 0.5 * (TAU * 400.0 * t).sin();
                noise + if t < 2.0 { 0.0 } else { 0.2 * envelope * voice }
            })
            .collect();
        let mut output = vec![0.0; input.len()];
        let mut times = Vec::with_capacity(1000);
        let cpu_before = process_cpu();
        let start = Instant::now();
        for (i, o) in input.chunks(480).zip(output.chunks_mut(480)) {
            let t = Instant::now();
            df.process(i, o).unwrap();
            times.push(t.elapsed());
        }
        let wall = start.elapsed();
        let cpu = process_cpu() - cpu_before;
        drop(df);

        assert!(output.iter().all(|s| s.is_finite() && s.abs() < 1.5));
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        // Noise-only stretch, skipping the first 0.5 s while the model settles.
        let noise = 24_000..96_000;
        println!(
            "input       f32 mono 48 kHz, 480-sample frames, 10 s of audio\n\
             load        {:.0} ms (model file + runtime init)\n\
             per frame   avg {:.3} ms, median {:.3} ms, p99 {:.3} ms, max {:.3} ms (budget 10 ms)\n\
             real time   {:.3}x ({:.2} s of compute for 10 s of audio)\n\
             CPU         {:.2} s process CPU over {:.2} s wall = {:.2} cores busy\n\
             noise       {:.1} dB on the noise-only stretch\n\
             output      f32 mono 48 kHz, 480 samples per frame, 30 ms behind the input",
            ms(load),
            ms(wall) / 1000.0,
            ms(times[500]),
            ms(times[990]),
            ms(times[999]),
            wall.as_secs_f64() / 10.0,
            wall.as_secs_f64(),
            cpu.as_secs_f64(),
            wall.as_secs_f64(),
            cpu.as_secs_f64() / wall.as_secs_f64(),
            20.0 * (rms(&output[noise.clone()]) / rms(&input[noise])).log10(),
        );
    }

    /// Diagnostic: 5 s from the default microphone through WASAPI capture,
    /// the capture ring and DeepFilterNet3 into the processed ring, read back
    /// one frame per 10 ms as the speaker will. Audio is measured, never stored.
    /// cargo test --release mic_denoise_diagnostic -- --ignored --nocapture
    #[test]
    #[ignore = "records 5 s from the default microphone"]
    fn mic_denoise_diagnostic() {
        use crate::capture::Mic;
        use crate::ring;

        const READS: u32 = 500;
        let (mut raw_tx, mut raw_rx) = ring::new(8);
        let (mut out_tx, mut out_rx) = ring::new(8);
        let (mic_stop, control) = (AtomicBool::new(false), Control::default());
        // Load before capture starts, so the model is there from the first frame.
        let (models, models_rx) = mpsc::channel();
        models.send(model()).unwrap();
        thread::scope(|s| {
            let df = s.spawn(|| run(models_rx, &mut raw_rx, &mut out_tx, &control));
            let mic = s.spawn(|| {
                let mut mic = Mic::open(None).unwrap();
                let result = mic.run(&mut raw_tx, &mic_stop, None);
                (mic.stats, result)
            });

            // After the first frame, read one every 10 ms, half a period late.
            let waiting = Instant::now();
            while out_rx.queued() == 0 {
                assert!(
                    waiting.elapsed() < Duration::from_secs(5),
                    "no processed audio"
                );
                thread::sleep(Duration::from_millis(1));
            }
            let start = Instant::now() + Duration::from_millis(5);
            let (mut frames, mut underruns, mut peak) = (0u32, 0u32, 0f32);
            let (mut latency_total, mut latency_max) = (Duration::ZERO, Duration::ZERO);
            for read in 0..READS {
                thread::sleep(
                    (start + Duration::from_millis(10) * read)
                        .saturating_duration_since(Instant::now()),
                );
                let Some(f) = out_rx.pop() else {
                    underruns += 1;
                    continue;
                };
                assert!(f.samples().iter().all(|s| s.is_finite()));
                peak = f.samples().iter().fold(peak, |p, s| p.max(s.abs()));
                if let Some(captured) = f.captured {
                    let latency = captured.elapsed();
                    latency_total += latency;
                    latency_max = latency_max.max(latency);
                }
                frames += 1;
            }
            mic_stop.store(true, Relaxed);
            let (mic, result) = mic.join().unwrap();
            control.stop.store(true, Relaxed);
            let df = df.join().unwrap();
            result.unwrap();

            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            println!(
                "read        {frames} frames (f32 mono 48 kHz, 480 samples), peak {:.1} dBFS\n\
                 processing  avg {:.3} ms, max {:.3} ms per frame (budget 10 ms), {} model errors\n\
                 underruns   {underruns} (processed ring empty when read)\n\
                 overruns    {} at capture (ring full), {} after DeepFilterNet (ring full)\n\
                 dropped     {} frames never reached the model, {} capture glitches\n\
                 latency     capture to read avg {:.1} ms, max {:.1} ms (includes the model's 30 ms)",
                20.0 * peak.max(1e-6).log10(),
                ms(df.busy_total) / df.frames.max(1) as f64,
                ms(df.busy_max),
                df.errors,
                mic.dropped,
                df.dropped,
                df.gaps,
                mic.glitches,
                ms(latency_total) / frames.max(1) as f64,
                ms(latency_max),
            );
        });
    }

    /// CPU time used by this process so far, all threads.
    fn process_cpu() -> std::time::Duration {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        // SAFETY: a plain Win32 call writing four FILETIMEs we own.
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        }
        .unwrap();
        let hns = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        std::time::Duration::from_nanos((hns(kernel) + hns(user)) * 100)
    }
}
