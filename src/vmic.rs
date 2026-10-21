//! Feeds processed audio to the YAL ClearVoice virtual microphone endpoint.
//!
//! The driver (`driver/`) publishes "YAL ClearVoice Microphone", which Chrome,
//! Edge and Genesys capture from. This hands the driver one 10 ms frame at a time
//! over IOCTL_YALCV_WRITE and reads back how full its ring is, so clock drift
//! between the headset and the driver's clock can be steered out. No audio is
//! written to disk and nothing leaves the machine.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::core::Owned;

use crate::devices::SelectError;
use crate::driver;
use crate::format::AudioFormat;
use crate::ipc;
use crate::playback::fit;
use crate::ring::Consumer;
use crate::stream::Stats;

/// A 480-sample frame, room for one stretched to 481 by drift steering.
const MAX_SAMPLES: usize = 481;
/// Samples in one unstretched frame (10 ms mono).
const FRAMES: usize = AudioFormat::MIC.frames;
/// Driver-ring fill we steer toward, in frames: one frame plus a 10 ms cushion.
const TARGET_FRAMES: f32 = 2.0;
/// Most ring frames worth keeping (30 ms); older ones are skipped as stale.
const MAX_BACKLOG: usize = 3;

/// One IOCTL_YALCV_WRITE request: a frame of 16-bit mono PCM. `repr(C)` to match
/// the driver's `YALCV_WRITE_IN`.
#[repr(C)]
struct WriteIn {
    abi: u32,
    samples: u32,
    pcm: [i16; MAX_SAMPLES],
}

/// Its reply: ring fill and whether an application is recording, plus the
/// driver's cumulative glitch counts. Matches the driver's `YALCV_WRITE_OUT`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct WriteOut {
    /// Samples queued in the driver ring after this write.
    queued: u32,
    /// 1 while an application holds the endpoint open for capture.
    running: u32,
    /// Times the ring ran empty and the driver played silence.
    underruns: u32,
    /// Frames dropped because the ring was full.
    overruns: u32,
}

/// An open handle to the virtual microphone endpoint.
pub struct Vmic {
    handle: Owned<HANDLE>,
    pub stats: Stats,
}

impl Vmic {
    /// Opens the driver's control device. A missing driver comes back as
    /// `Disconnected`, so `supervise` retries.
    pub fn open() -> Result<Vmic, SelectError> {
        Ok(Vmic {
            handle: driver::open()?,
            stats: Stats::default(),
        })
    }

    /// Writes frames from `input` to the driver until `stop` is set or the driver
    /// goes away. Frames are 48 kHz mono f32; each is converted to 16-bit PCM and
    /// handed to the driver, which serves it to Chrome. Clock drift between the
    /// headset and the driver is absorbed by sending a frame one sample longer or
    /// shorter, and a backlog after a stall by skipping the stalest frames. Call
    /// it on the opening thread; it only ever waits on an empty ring.
    pub fn run(&mut self, input: &mut Consumer, stop: &AtomicBool) -> Result<(), SelectError> {
        let mut scratch = [0.0f32; MAX_SAMPLES];
        // Driver-ring fill, in frames, averaged over about 1 s of writes.
        let mut backlog = TARGET_FRAMES;
        let mut was_running = false;
        let (mut last_under, mut last_over) = (0u32, 0u32);
        while !stop.load(Relaxed) {
            // A burst after a stall would otherwise sit as latency for minutes;
            // drop the stalest frames (same rule as the speaker).
            while input.queued() > MAX_BACKLOG {
                input.pop();
                self.stats.skipped += 1;
            }
            let Some(frame) = input.pop() else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            let woke = Instant::now();
            // Steer the ring back toward its target: a bit longer while it's low,
            // a bit shorter while it's high, one sample at a time.
            let len = match backlog {
                b if b < TARGET_FRAMES - 0.5 => FRAMES + 1,
                b if b > TARGET_FRAMES + 0.5 => FRAMES - 1,
                _ => FRAMES,
            };
            fit(frame.samples(), 1, &mut scratch[..len], 1);
            let mut req = WriteIn {
                abi: driver::YALCV_ABI,
                samples: len as u32,
                pcm: [0; MAX_SAMPLES],
            };
            for (dst, &s) in req.pcm[..len].iter_mut().zip(scratch[..len].iter()) {
                *dst = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            }
            if len != FRAMES {
                self.stats.resampled += 1;
            }

            let mut out = WriteOut::default();
            driver::ioctl(*self.handle, driver::IOCTL_YALCV_WRITE, &req, &mut out)?;

            // A fresh recording empties the ring, so start the estimate over.
            let running = out.running != 0;
            if running && !was_running {
                backlog = TARGET_FRAMES;
            }
            was_running = running;
            backlog += (out.queued as f32 / FRAMES as f32 - backlog) / 100.0;

            self.stats.underruns += out.underruns.saturating_sub(last_under) as u64;
            self.stats.dropped += out.overruns.saturating_sub(last_over) as u64;
            (last_under, last_over) = (out.underruns, out.overruns);

            let ahead = Duration::from_micros(
                out.queued as u64 * 1_000_000 / AudioFormat::MIC.sample_rate as u64,
            );
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
            self.stats.wakeup(woke.elapsed());
            ipc::TELEMETRY.set_vmic_errors(self.stats.dropped + self.stats.underruns);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing driver is reported as Disconnected, so the supervisor keeps
    /// retrying until the driver is installed.
    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn missing_driver_is_disconnected() {
        assert!(matches!(Vmic::open(), Err(SelectError::Disconnected)));
    }
}
