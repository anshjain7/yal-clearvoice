//! Speaker output through WASAPI: shared mode, event-driven, 48 kHz stereo f32.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioClient, IAudioRenderClient};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Owned;

use crate::devices::{Device, Direction, SelectError};
use crate::format::AudioFormat;
use crate::ipc;
use crate::ring::Consumer;
use crate::stream::{self, Stats, WAIT_MS};

/// Endpoint buffer capacity, in 100 ns units: 40 ms. Only about half of it is
/// kept filled (see `pump`), so frames played a sample longer or shorter
/// always fit and the device never runs low while the clocks are steered.
const BUFFER_HNS: i64 = 400_000;
/// Frames in each ring frame (10 ms).
const FRAMES: usize = AudioFormat::SPEAKER.frames;
/// Spare audio (1 ms) left in the device at a wakeup when the ring ran dry, for
/// the next wakeup coming a little late.
const GUARD: usize = FRAMES / 10;
/// Ring frames the cushion keeps waiting, to begin with and at least (30 ms):
/// enough to ride out audio from noise removal arriving 20-30 ms late. (One
/// frame of it is always on its way out, so it covers the rest, plus about a
/// period queued in the device.)
const MIN_TARGET: usize = 3;
/// The deepest the cushion grows on a PC whose audio keeps arriving late (70 ms).
const MAX_TARGET: usize = 7;
/// The longest gap, in silence top-ups (~10 ms each), that a deeper cushion
/// would have covered.
const SHORT_GAP: u32 = 5;
/// Wakeups (~10 ms each) without a gap before the cushion shrinks a frame: ~1 min.
const CALM_WAKEUPS: u32 = 6_000;
/// A backlog this many frames over the target (a burst after a stall) plays
/// slightly fast until it's gone, instead of being cut.
const FAST_OVER: usize = 3;
/// Samples a 480-sample frame plays as while a burst drains: 2% fast.
const FAST_LEN: usize = FRAMES - 10;
/// Beyond this many ring frames waiting (120 ms), the stalest are skipped:
/// draining that much would take too long.
const MAX_BACKLOG: usize = 12;

/// An initialized, not yet started, speaker stream. Windows converts `format`
/// to the device's own rate and channel count (AUTOCONVERTPCM).
pub struct Speaker {
    pub device: Device,
    /// Stream format: 48 kHz stereo f32. `frames` is one device period.
    pub format: AudioFormat,
    /// Endpoint buffer capacity in frames: the most audio queued ahead.
    pub buffer_frames: u32,
    pub client: IAudioClient,
    pub render: IAudioRenderClient,
    /// Signalled once per device period after `client.Start()`.
    pub event: Owned<HANDLE>,
    pub stats: Stats,
}

impl Speaker {
    /// Opens the speaker with `id`, or for `None` the Windows default
    /// communications speaker. Any failure comes back as an error.
    pub fn open(id: Option<&str>) -> Result<Speaker, SelectError> {
        let s = stream::open(Direction::Output, id, AudioFormat::SPEAKER, BUFFER_HNS)?;
        Ok(Speaker {
            // SAFETY: a COM call on the client `open` just initialized.
            render: unsafe { s.client.GetService()? },
            device: s.device,
            format: s.format,
            buffer_frames: s.buffer_frames,
            client: s.client,
            event: s.event,
            stats: Stats::default(),
        })
    }

    /// Plays frames from `input` until `stop` is set or the device fails; mono
    /// frames play on every channel. A cushion of frames is kept waiting so a
    /// late frame still plays on time (see `Cushion`): clock drift between the
    /// ring's source and the speaker is absorbed by playing frames a sample
    /// longer or shorter, a burst after a stall plays slightly fast, and only a
    /// backlog over 120 ms is skipped. If the ring is empty as the device is
    /// about to run dry, it plays only as much silence as it takes to reach the
    /// next wakeup (an underrun, once audio has started) and the cushion grows.
    /// Same rules as `Mic::run`: call it on the opening thread; it only waits
    /// on the device.
    pub fn run(&mut self, input: &mut Consumer, stop: &AtomicBool) -> Result<(), SelectError> {
        stream::realtime(|| self.pump(input, stop))
    }

    fn pump(&mut self, input: &mut Consumer, stop: &AtomicBool) -> Result<(), SelectError> {
        let channels = self.format.channels as usize;
        let mut cushion = Cushion::new();
        // Keep the device topped up to two periods: whatever length frames
        // play at, it then has at least a period queued at every wakeup.
        let fill_to = fill_level(self.format.frames);
        // SAFETY: COM calls on this stream's own interfaces. Each buffer from
        // GetBuffer is written, then released, before the next one is fetched.
        unsafe {
            // Start on a period of silence so the first periods can't glitch.
            self.silence((fill_to as u32).min(self.buffer_frames))?;
            self.client.Start()?;
            while !stop.load(Relaxed) {
                WaitForSingleObject(*self.event, WAIT_MS);
                let woke = Instant::now();
                let (skip, len) = cushion.steer(input.queued());
                for _ in 0..skip {
                    input.pop();
                    self.stats.skipped += 1;
                }
                // Before audio starts, let the cushion fill (the device plays
                // silence meanwhile), so it's there from the first frame.
                let filling = self.stats.frames == 0 && input.queued() < cushion.target;
                let mut queued = self.client.GetCurrentPadding()? as usize;
                while queued < fill_to && self.buffer_frames as usize - queued >= len {
                    let next = if filling { None } else { input.pop() };
                    let Some(frame) = next else {
                        // A late frame is fine while the device still has a
                        // period queued. If it would run dry before the next
                        // wakeup, queue just enough silence to get there (so a
                        // frame a few ms late leaves a gap of a few ms, not a
                        // whole frame), and keep a deeper cushion from now on.
                        if queued < FRAMES {
                            if self.stats.frames > 0 {
                                self.stats.underruns += 1;
                                cushion.underrun();
                            }
                            self.silence((FRAMES + GUARD - queued) as u32)?;
                        }
                        break;
                    };
                    cushion.played();
                    let out = self.render.GetBuffer(len as u32)?.cast::<f32>();
                    let out = std::slice::from_raw_parts_mut(out, len * channels);
                    fit(frame.samples(), frame.channels() as usize, out, channels);
                    self.render.ReleaseBuffer(len as u32, 0)?;
                    if len != FRAMES {
                        self.stats.resampled += 1;
                    }
                    let ahead = queued as u64 * 1_000_000 / self.format.sample_rate as u64;
                    let ahead = Duration::from_micros(ahead);
                    self.stats.queue_total += ahead;
                    self.stats.queue_max = self.stats.queue_max.max(ahead);
                    if let Some(captured) = frame.captured {
                        let latency = captured.elapsed() + ahead;
                        self.stats.latency_total += latency;
                        self.stats.latency_max = self.stats.latency_max.max(latency);
                        ipc::TELEMETRY
                            .latency_us
                            .store(latency.as_micros() as u64, Relaxed);
                    }
                    self.stats.frames += 1;
                    queued += len;
                }
                self.stats.wakeup(woke.elapsed());
                // In VB-CABLE mode this speaker stream is the send path.
                ipc::TELEMETRY.set_vmic_errors(self.stats.underruns + self.stats.skipped);
                ipc::TELEMETRY
                    .out_underruns
                    .store(self.stats.underruns, Relaxed);
                ipc::TELEMETRY
                    .out_skipped
                    .store(self.stats.skipped, Relaxed);
            }
            self.client.Stop()?;
        }
        Ok(())
    }

    /// Queues `frames` frames of silence.
    unsafe fn silence(&self, frames: u32) -> windows::core::Result<()> {
        // SAFETY: the caller checked the buffer has room for `frames`.
        unsafe {
            self.render.GetBuffer(frames)?;
            self.render
                .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        }
    }
}

/// How full `pump` keeps the device for a device period of `period` frames:
/// two periods (at least two ring frames), so after a period plays there's
/// still one queued.
fn fill_level(period: usize) -> usize {
    2 * period.max(FRAMES)
}

/// The send path's cushion: ring frames kept waiting, so a frame that arrives
/// late (noise removal briefly slow, the PC busy) still plays on time instead
/// of leaving a gap. It starts at `MIN_TARGET` and grows a frame after each
/// short gap (up to `MAX_TARGET`); after a minute in which part of it was never
/// needed, it shrinks a frame. A busy PC gets a deeper cushion, a quiet one
/// keeps latency low.
pub(crate) struct Cushion {
    /// Ring frames to keep waiting.
    pub(crate) target: usize,
    /// Frames waiting in the ring, averaged over about 1 s of wakeups.
    backlog: f32,
    /// Wakeups in the current minute, and the fewest frames waiting at any of
    /// them: 2 or more means a frame of the cushion was never touched.
    calm: u32,
    low: usize,
    /// Silence top-ups in the current gap (0: not in a gap).
    dry: u32,
}

impl Cushion {
    pub(crate) fn new() -> Cushion {
        Cushion {
            target: MIN_TARGET,
            backlog: MIN_TARGET as f32,
            calm: 0,
            low: usize::MAX,
            dry: 0,
        }
    }

    /// Once per wakeup, with the frames waiting in the ring: how many of the
    /// stalest to skip (only after a long stall), and how many samples to play
    /// each frame as. Clock drift (the source and the speaker never run at
    /// exactly the same rate) is steered out by playing frames a sample longer
    /// while the backlog is under the target and a sample shorter while it's
    /// over; a burst plays 2% fast until it's down near the target.
    pub(crate) fn steer(&mut self, queued: usize) -> (usize, usize) {
        self.calm += 1;
        self.low = self.low.min(queued);
        if self.calm >= CALM_WAKEUPS {
            if self.low >= 2 && self.target > MIN_TARGET {
                self.target -= 1;
            }
            (self.calm, self.low) = (0, usize::MAX);
        }
        let skip = if queued > MAX_BACKLOG {
            queued - self.target
        } else {
            0
        };
        let queued = queued - skip;
        self.backlog += (queued as f32 - self.backlog) / 100.0;
        let target = self.target as f32;
        let len = if queued >= self.target + FAST_OVER {
            FAST_LEN
        } else if self.backlog < target - 0.5 {
            FRAMES + 1
        } else if self.backlog > target + 0.5 {
            FRAMES - 1
        } else {
            FRAMES
        };
        (skip, len)
    }

    /// The device was about to run dry with nothing to play, so it got silence.
    pub(crate) fn underrun(&mut self) {
        self.dry += 1;
        (self.calm, self.low) = (0, usize::MAX);
    }

    /// A frame played. If it ends a short gap, audio that was only a little
    /// late, the cushion was too shallow for this PC and grows. A long gap means
    /// the audio stopped altogether (the microphone glitched, or was changed):
    /// no cushion would have covered it, so it's no reason to add latency.
    pub(crate) fn played(&mut self) {
        if (1..=SHORT_GAP).contains(&self.dry) && self.target < MAX_TARGET {
            self.target += 1;
        }
        self.dry = 0;
    }
}

/// Fits interleaved `src` (`src_ch` channels) into `out` (`out_ch` channels),
/// which may hold a frame or so more or fewer: samples are linearly
/// interpolated across the difference, and a same-length copy is exact.
/// Channels map one to one; extra output channels repeat the last input
/// channel, so mono plays on both sides of a stereo speaker.
pub(crate) fn fit(src: &[f32], src_ch: usize, out: &mut [f32], out_ch: usize) {
    let (n_in, n_out) = (src.len() / src_ch, out.len() / out_ch);
    let step = (n_in - 1) as f32 / (n_out - 1) as f32;
    for (j, frame) in out.chunks_exact_mut(out_ch).enumerate() {
        let pos = j as f32 * step;
        let (i, t) = (pos as usize, pos.fract());
        let next = (i + 1).min(n_in - 1);
        for (c, o) in frame.iter_mut().enumerate() {
            let c = c.min(src_ch - 1);
            let (a, b) = (src[i * src_ch + c], src[next * src_ch + c]);
            *o = a + (b - a) * t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::SampleFormat;
    use crate::frame::AudioFrame;
    use crate::ring;
    use std::f32::consts::PI;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn fits_channels_exactly() {
        let mut out = [0.0; 4];
        fit(&[1.0, 2.0], 1, &mut out, 2);
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0]);
        fit(&[1.0, 2.0, 3.0, 4.0], 2, &mut out, 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn fits_one_sample_longer_or_shorter() {
        let mut longer = [0.0; 4];
        fit(&[0.0, 1.0, 2.0], 1, &mut longer, 1);
        let expected = [0.0, 2.0 / 3.0, 4.0 / 3.0, 2.0];
        assert!(
            longer
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 1e-6)
        );
        let mut shorter = [0.0; 3];
        fit(&[0.0, 1.0, 2.0, 3.0], 1, &mut shorter, 1);
        assert_eq!(shorter, [0.0, 1.5, 3.0]);
    }

    #[test]
    fn cushion_grows_after_short_gaps_only_and_shrinks_when_unused() {
        let mut c = Cushion::new();
        assert_eq!(c.target, MIN_TARGET);
        // Audio a little late (two periods of silence): it grows a frame once
        // the audio is back, and only once.
        c.underrun();
        c.underrun();
        c.played();
        assert_eq!(c.target, MIN_TARGET + 1);
        c.played();
        assert_eq!(c.target, MIN_TARGET + 1);
        // Audio gone for 200 ms (a microphone glitch): no cushion would have
        // covered that, so no latency is added for it.
        for _ in 0..20 {
            c.underrun();
        }
        c.played();
        assert_eq!(c.target, MIN_TARGET + 1);
        c.underrun();
        c.played();
        assert_eq!(c.target, MIN_TARGET + 2);
        // A minute in which it dipped to one frame: still needed, kept.
        for i in 0..CALM_WAKEUPS {
            c.steer(if i == 100 { 1 } else { 4 });
        }
        assert_eq!(c.target, MIN_TARGET + 2);
        // A minute with two or more frames always waiting: one goes.
        for _ in 0..CALM_WAKEUPS {
            c.steer(4);
        }
        assert_eq!(c.target, MIN_TARGET + 1);
        for _ in 0..10 * CALM_WAKEUPS {
            c.steer(4);
        }
        assert_eq!(c.target, MIN_TARGET, "never below the minimum");
        for _ in 0..20 {
            c.underrun();
            c.played();
        }
        assert_eq!(c.target, MAX_TARGET, "never above the maximum");
    }

    #[test]
    fn cushion_plays_bursts_fast_and_skips_only_huge_ones() {
        let mut c = Cushion::new();
        assert_eq!(c.steer(MIN_TARGET), (0, FRAMES));
        assert_eq!(c.steer(MIN_TARGET + FAST_OVER), (0, FAST_LEN));
        assert_eq!(c.steer(MAX_BACKLOG), (0, FAST_LEN));
        assert_eq!(
            c.steer(MAX_BACKLOG + 3),
            (MAX_BACKLOG + 3 - MIN_TARGET, FRAMES)
        );
    }

    /// What `simulate` saw.
    #[derive(Debug)]
    struct Sim {
        /// Times the device was about to run dry and played silence.
        gaps: u32,
        /// Frames skipped as stale, and dropped because the ring was full.
        skipped: usize,
        dropped: usize,
        /// The least audio the device had left at a wakeup (after the first second).
        least_left: usize,
        /// Frames waiting in the ring, on average over the last 10 s.
        waiting: f32,
    }

    /// Simulates the send path for `seconds` in 1 ms steps, by `pump`'s rules
    /// and its real `Cushion`: the source makes a frame every `period_ms` (its
    /// clock off from the speaker's by however much that differs from 10), but
    /// while `stalled(ms)` its frames wait, then arrive in one burst, as when
    /// noise removal or the PC hiccups. The device plays 480 samples every 10 ms
    /// from a 40 ms buffer; the ring holds 16 frames, as in the engine.
    fn simulate(seconds: u32, period_ms: f64, stalled: impl Fn(u32) -> bool) -> Sim {
        simulate_losing(seconds, period_ms, stalled, false)
    }

    /// `simulate`, but with `lost` the frames due during a stall never come,
    /// as when the microphone itself drops out.
    fn simulate_losing(
        seconds: u32,
        period_ms: f64,
        stalled: impl Fn(u32) -> bool,
        lost: bool,
    ) -> Sim {
        const BUFFER: usize = 4 * FRAMES;
        const RING: usize = 16;
        let mut c = Cushion::new();
        let fill_to = fill_level(FRAMES);
        let (mut made, mut ring, mut device) = (0u64, 0usize, fill_to);
        let (mut started, mut dry) = (false, false);
        let mut sim = Sim {
            gaps: 0,
            skipped: 0,
            dropped: 0,
            least_left: usize::MAX,
            waiting: 0.0,
        };
        let mut waiting = Vec::new();
        for ms in 0..seconds * 1000 {
            let stall = stalled(ms);
            while (!stall || lost) && (made + 1) as f64 * period_ms <= ms as f64 {
                made += 1;
                if stall {
                    // lost
                } else if ring < RING {
                    ring += 1;
                } else {
                    sim.dropped += 1;
                }
            }
            if ms % 10 != 0 {
                continue;
            }
            // A period played since the last wakeup.
            if ms > 0 {
                assert!(device >= FRAMES, "the device ran dry at {ms} ms");
                device -= FRAMES;
                if ms >= 1000 {
                    sim.least_left = sim.least_left.min(device);
                }
            }
            let (skip, len) = c.steer(ring);
            ring -= skip;
            sim.skipped += skip;
            let filling = !started && ring < c.target;
            while device < fill_to && BUFFER - device >= len {
                if filling || ring == 0 {
                    if device < FRAMES {
                        if started {
                            sim.gaps += u32::from(!dry);
                            dry = true;
                            c.underrun();
                        }
                        device = FRAMES + GUARD;
                    }
                    break;
                }
                ring -= 1;
                c.played();
                (started, dry) = (true, false);
                device += len;
            }
            if ms >= (seconds - 10) * 1000 {
                waiting.push(ring);
            }
        }
        sim.waiting = waiting.iter().sum::<usize>() as f32 / waiting.len() as f32;
        sim
    }

    #[test]
    fn steady_audio_never_gaps_whatever_the_clock_drift() {
        // The source's clock 0.1% fast, exact, and 0.1% slow.
        for period in [9.99, 10.0, 10.01] {
            let sim = simulate(300, period, |_| false);
            assert_eq!(
                (sim.gaps, sim.skipped, sim.dropped),
                (0, 0, 0),
                "{period}: {sim:?}"
            );
            assert!(sim.least_left >= FRAMES, "{period}: {sim:?}");
            assert!(sim.waiting <= (MIN_TARGET + 1) as f32, "{period}: {sim:?}");
        }
    }

    #[test]
    fn repeated_late_audio_gaps_at_most_twice_then_never() {
        // Every 3 s, 40 ms with nothing arriving: a PC that keeps stalling.
        let sim = simulate(300, 10.0, |ms| ms > 1000 && ms % 3000 < 40);
        assert!(sim.gaps <= 2, "{sim:?}");
        assert_eq!((sim.skipped, sim.dropped), (0, 0), "{sim:?}");
    }

    #[test]
    fn a_microphone_dropout_adds_no_latency() {
        // Half a second of audio never arriving, twice: a gap each time (there
        // is nothing to play), but the cushion stays as it was.
        let sim = simulate_losing(
            40,
            10.0,
            |ms| (5000..5500).contains(&ms) || (20_000..20_500).contains(&ms),
            true,
        );
        assert_eq!((sim.gaps, sim.skipped, sim.dropped), (2, 0, 0), "{sim:?}");
        assert!(sim.waiting <= (MIN_TARGET + 1) as f32, "{sim:?}");
    }

    #[test]
    fn a_long_stall_drains_without_cuts() {
        // 100 ms with nothing, once: one gap, then its burst plays slightly
        // fast instead of being cut, and the backlog is back down within 30 s.
        let sim = simulate(40, 10.0, |ms| (5000..5100).contains(&ms));
        assert_eq!(sim.gaps, 1, "{sim:?}");
        assert_eq!((sim.skipped, sim.dropped), (0, 0), "{sim:?}");
        assert!(sim.waiting <= (MIN_TARGET + 2) as f32, "{sim:?}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn opens_default_speaker() {
        let speaker = Speaker::open(None).unwrap();
        let f = speaker.format;
        assert_eq!(
            (f.sample_rate, f.channels, f.sample_format),
            (48_000, 2, SampleFormat::F32)
        );
        assert!(speaker.buffer_frames as usize >= FRAMES);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fails_cleanly_on_unknown_device() {
        let result = Speaker::open(Some("not-a-device"));
        assert!(matches!(result, Err(SelectError::UnknownId)));
    }

    /// The real send path on VB-CABLE, a virtual device (nothing is heard and
    /// no microphone is used): a quiet 440 Hz tone goes through the ring into
    /// "CABLE Input" for 30 s, its source going 50 ms without a frame every 2 s
    /// (far worse than a busy PC), while "CABLE Output" is recorded and checked
    /// for gaps in the tone.
    /// Close YAL ClearVoice first; it uses VB-CABLE. Skipped if VB-CABLE is
    /// Windows' speaker (other sounds would be in the recording).
    /// cargo test --release cable_send_path -- --ignored --nocapture
    #[test]
    #[ignore = "plays a test tone through VB-CABLE"]
    fn cable_send_path_rides_out_stalls() {
        use crate::devices;
        use crate::transcribe::Tap;
        let list = devices::list().unwrap();
        let find = |direction, name: &str| {
            list.iter()
                .find(|d| d.direction == direction && d.name.contains(name))
                .expect("VB-CABLE not installed")
        };
        let (input, output) = (
            find(Direction::Output, "CABLE Input"),
            find(Direction::Input, "CABLE Output"),
        );
        if list.iter().any(|d| {
            d.direction == Direction::Output
                && (d.default || d.default_comms)
                && d.name.contains("VB-Audio")
        }) {
            println!("skipped: VB-CABLE is Windows' speaker");
            return;
        }
        let (stop, opened) = (AtomicBool::new(false), AtomicBool::new(false));
        let (mut tx, mut rx) = ring::new(16);
        let (heard, stats) = thread::scope(|s| {
            let recorder = s.spawn(|| {
                let mut tap = Tap::open(Direction::Input, &output.id).unwrap();
                let mut heard = Vec::new();
                tap.run(&|| None::<String>, &stop, |chunk| heard.extend(chunk))
                    .unwrap();
                heard
            });
            let player = s.spawn(|| {
                let mut speaker = Speaker::open(Some(&input.id)).unwrap();
                println!(
                    "CABLE Input: period {} frames, buffer {} frames",
                    speaker.format.frames, speaker.buffer_frames
                );
                opened.store(true, Relaxed);
                speaker.run(&mut rx, &stop).unwrap();
                speaker.stats
            });
            // As in the engine, audio starts once the speaker is open.
            while !opened.load(Relaxed) {
                thread::sleep(Duration::from_millis(1));
            }
            let start = Instant::now();
            let mut frame = AudioFrame::new(1);
            for seq in 0..3000u64 {
                let due = start + Duration::from_millis(10 * seq);
                // Every 2 s the source stalls for 40 ms (50 ms between two
                // frames), then catches up.
                let ms = 10 * seq;
                let due = if ms % 2000 < 40 && ms > 1000 {
                    due + Duration::from_millis(40 - ms % 2000)
                } else {
                    due
                };
                thread::sleep(due.saturating_duration_since(Instant::now()));
                frame.seq = seq;
                for (i, x) in frame.samples_mut().iter_mut().enumerate() {
                    let t = (seq as usize * FRAMES + i) as f32 / 48_000.0;
                    *x = 0.05 * (2.0 * PI * 440.0 * t).sin();
                }
                assert!(tx.push(&frame), "ring full");
            }
            thread::sleep(Duration::from_millis(300));
            stop.store(true, Relaxed);
            (recorder.join().unwrap(), player.join().unwrap())
        });
        // The tone, 2 ms windows at 16 kHz: any quiet window inside it is a gap.
        let loud: Vec<bool> = heard
            .chunks(32)
            .map(|w| w.iter().map(|&x| (x as f32).powi(2)).sum::<f32>() / 32.0 > 200_000.0)
            .collect();
        let first = loud.iter().position(|&l| l).expect("no tone recorded");
        let last = loud.iter().rposition(|&l| l).unwrap();
        let at: Vec<f32> = (first..last)
            .filter(|&i| loud[i] && !loud[i + 1])
            .map(|i| (i + 1 - first) as f32 * 0.002)
            .collect();
        let quiet_ms = 2 * loud[first..=last].iter().filter(|&&l| !l).count();
        println!("gaps at {at:?} s, {quiet_ms} ms quiet in all");
        let gaps = at.len();
        println!(
            "tone {:.1} s, gaps heard {gaps}; send path: {} underruns (with the \
             silence after the tone), {} skipped, {} resampled, device queue avg {:.1} ms",
            (last - first) as f32 * 0.002,
            stats.underruns,
            stats.skipped,
            stats.resampled,
            stats.queue_total.as_secs_f64() * 1000.0 / stats.frames.max(1) as f64,
        );
        // A few short gaps while the cushion finds its depth, then hardly any,
        // though the stalls go on (14 of them). The rules before this had a
        // gap at nearly every stall and more besides: 19-26 gaps, over 400 ms.
        assert_eq!(stats.skipped, 0);
        assert!(
            gaps <= 5 && quiet_ms <= 80,
            "gaps at {at:?} s, {quiet_ms} ms"
        );
    }

    /// Diagnostic: plays a quiet (-30 dBFS) 440 Hz tone for 1 s on the default
    /// speaker, from mono frames through the ring.
    /// cargo test speaker_diagnostic -- --ignored --nocapture
    #[test]
    #[ignore = "plays a 1 s tone on the default speaker"]
    fn speaker_diagnostic() {
        let (mut tx, mut rx) = ring::new(8);
        let stop = Arc::new(AtomicBool::new(false));
        let player = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut speaker = Speaker::open(None).unwrap();
                let result = speaker.run(&mut rx, &stop);
                let (f, buffer, stats) = (speaker.format, speaker.buffer_frames, speaker.stats);
                (speaker.device.name, f, buffer, stats, result)
            }
        });
        // Push as fast as the speaker takes frames; the full ring paces us.
        // Halfway, stall 100 ms so the ring runs dry and playback must recover.
        let start = Instant::now();
        let mut frame = AudioFrame::new(1);
        for seq in 0..100 {
            if seq == 50 {
                thread::sleep(Duration::from_millis(100));
            }
            frame.seq = seq;
            for (i, s) in frame.samples_mut().iter_mut().enumerate() {
                let t = (seq as usize * FRAMES + i) as f32 / 48_000.0;
                *s = 0.03 * (2.0 * PI * 440.0 * t).sin();
            }
            while !tx.push(&frame) {
                assert!(start.elapsed() < Duration::from_secs(5), "speaker stalled");
                thread::sleep(Duration::from_millis(1));
            }
        }
        stop.store(true, Relaxed);
        let (name, f, buffer, stats, result) = player.join().unwrap();
        let ms = |frames: f64| frames * 1000.0 / f.sample_rate as f64;
        let busy_ms = |d: Duration| d.as_secs_f64() * 1000.0;
        println!(
            "{name}\n  \
             format      {} Hz, {} ch, {:?}, device period {} frames ({} ms)\n  \
             latency     {buffer} frame buffer ({} ms queued at most)\n  \
             played      {} frames ({:.2} s)\n  \
             underruns   {} (device ran dry; includes the 100 ms stall)\n  \
             errors      run {result:?}\n  \
             processing  avg {:.3} ms, max {:.3} ms per wakeup",
            f.sample_rate,
            f.channels,
            f.sample_format,
            f.frames,
            ms(f.frames as f64),
            ms(buffer as f64),
            stats.frames,
            ms((stats.frames as usize * FRAMES) as f64) / 1000.0,
            stats.underruns,
            busy_ms(stats.busy_total) / stats.wakeups.max(1) as f64,
            busy_ms(stats.busy_max),
        );
        result.unwrap();
        assert!(stats.underruns > 0, "the stall never emptied the ring");
        assert!(stats.frames > 60, "playback did not resume after the stall");
    }
}
