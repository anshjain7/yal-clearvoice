//! Plays the YAL ClearVoice virtual speaker's audio to the physical headset.
//!
//! The driver publishes "YAL ClearVoice Speaker"; Chrome/Edge (Genesys) render
//! the call's far end to it, Windows mixes it, and the driver stores it in a ring.
//! This reads that ring one period at a time over IOCTL_YALCV_READ and plays it to
//! the headset through WASAPI. It is the return path's only clock crossing
//! (driver clock -> headset clock), so the single resampler lives here: it pulls a
//! frame one sample longer or shorter to hold the driver ring at its target, and
//! always writes one full device period to the headset. No processing happens on
//! this path yet. Nothing is written to disk and nothing leaves the machine.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioClient, IAudioRenderClient};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Owned;

use crate::devices::{Device, Direction, SelectError};
use crate::driver;
use crate::format::AudioFormat;
use crate::ipc;
use crate::playback::fit;
use crate::stream::{self, Stats, WAIT_MS};

/// Endpoint buffer, 100 ns units: 20 ms (two periods), the most audio queued ahead.
const BUFFER_HNS: i64 = 200_000;
/// Frames in one device period written per step (10 ms).
const FRAMES: usize = AudioFormat::SPEAKER.frames;
/// Interleaved stereo samples in a frame stretched to 481 for drift steering.
const SPK_MAX_SAMPLES: usize = 962;
/// Driver-ring fill we steer toward, in frames: one frame plus a 10 ms cushion.
const TARGET_FRAMES: f32 = 2.0;

/// One IOCTL_YALCV_READ request: how many interleaved samples to pull. `repr(C)`
/// to match the driver's `YALCV_READ_IN`.
#[repr(C)]
struct ReadIn {
    abi: u32,
    samples: u32,
}

/// Its reply: ring fill, whether anything is playing, the driver's cumulative
/// glitch counts, and the frame itself. Matches the driver's `YALCV_READ_OUT`.
#[repr(C)]
struct ReadOut {
    /// Interleaved samples queued in the driver ring (frames = queued / 2).
    queued: u32,
    /// 1 while an application is rendering to the endpoint.
    running: u32,
    /// Times the ring ran empty when read.
    underruns: u32,
    /// Frames dropped because the ring was full (the engine fell behind).
    overruns: u32,
    /// Interleaved samples actually returned in `pcm`.
    samples: u32,
    /// 48 kHz 16-bit stereo, interleaved.
    pcm: [i16; SPK_MAX_SAMPLES],
}

impl ReadOut {
    fn new() -> Self {
        ReadOut {
            queued: 0,
            running: 0,
            underruns: 0,
            overruns: 0,
            samples: 0,
            pcm: [0; SPK_MAX_SAMPLES],
        }
    }
}

/// An open virtual speaker: a handle to the driver plus a headset render stream.
pub struct Vspk {
    pub device: Device,
    pub format: AudioFormat,
    pub buffer_frames: u32,
    handle: Owned<HANDLE>,
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Owned<HANDLE>,
    pub stats: Stats,
}

impl Vspk {
    /// Opens the driver's control device and the physical speaker with `id`, or
    /// for `None` the default communications speaker (the headset, never our own
    /// virtual endpoint). A missing driver comes back as `Disconnected`, so
    /// `supervise` retries.
    pub fn open(id: Option<&str>) -> Result<Vspk, SelectError> {
        let handle = driver::open()?;
        let s = stream::open(Direction::Output, id, AudioFormat::SPEAKER, BUFFER_HNS)?;
        Ok(Vspk {
            // SAFETY: a COM call on the client `open` just initialized.
            render: unsafe { s.client.GetService()? },
            device: s.device,
            format: s.format,
            buffer_frames: s.buffer_frames,
            handle,
            client: s.client,
            event: s.event,
            stats: Stats::default(),
        })
    }

    /// Plays the virtual speaker to the headset until `stop` is set or either the
    /// driver or the headset goes away. Call it on the opening thread; apart from
    /// waiting on the headset's event it never blocks.
    /// `watch`, when it is set, means the Windows default output changed: the
    /// stream stops cleanly so the supervisor reopens on the new default headset.
    pub fn run(
        &mut self,
        stop: &AtomicBool,
        watch: Option<&AtomicBool>,
    ) -> Result<(), SelectError> {
        stream::realtime(|| self.pump(stop, watch))
    }

    fn pump(&mut self, stop: &AtomicBool, watch: Option<&AtomicBool>) -> Result<(), SelectError> {
        let dev_ch = self.format.channels as usize;
        // Driver-ring fill, in frames, averaged over about 1 s of reads.
        let mut backlog = TARGET_FRAMES;
        let mut was_running = false;
        let (mut last_under, mut last_over) = (0u32, 0u32);
        let mut scratch = [0.0f32; SPK_MAX_SAMPLES];
        let mut out = ReadOut::new();
        // SAFETY: COM calls on this stream's own interfaces; each buffer from
        // GetBuffer is written and released before the next is fetched.
        unsafe {
            // Lead with silence so the first periods can't glitch.
            self.silence(self.buffer_frames)?;
            self.client.Start()?;
            while !stop.load(Relaxed) {
                WaitForSingleObject(*self.event, WAIT_MS);
                // Default output changed: stop so the supervisor reopens on it.
                if watch.is_some_and(|w| w.swap(false, Relaxed)) {
                    break;
                }
                let woke = Instant::now();
                let mut queued = self.client.GetCurrentPadding()? as usize;
                while self.buffer_frames as usize - queued >= FRAMES {
                    // Driver ring filling (far end faster than the headset) -> pull
                    // a sample more to drain it; draining -> pull a sample less.
                    let want = match backlog {
                        b if b > TARGET_FRAMES + 0.5 => FRAMES + 1,
                        b if b < TARGET_FRAMES - 0.5 => FRAMES - 1,
                        _ => FRAMES,
                    };
                    let req = ReadIn {
                        abi: driver::YALCV_ABI,
                        samples: (want * 2) as u32,
                    };
                    driver::ioctl(*self.handle, driver::IOCTL_YALCV_READ, &req, &mut out)?;
                    let got = out.samples as usize / 2; // stereo frames returned

                    let running = out.running != 0;
                    // A fresh stream empties the ring, so start the estimate over.
                    if running && !was_running {
                        backlog = TARGET_FRAMES;
                    }
                    was_running = running;
                    backlog += (out.queued as f32 / 2.0 / FRAMES as f32 - backlog) / 100.0;
                    self.stats.underruns += out.underruns.saturating_sub(last_under) as u64;
                    self.stats.dropped += out.overruns.saturating_sub(last_over) as u64;
                    (last_under, last_over) = (out.underruns, out.overruns);

                    let dst = self.render.GetBuffer(FRAMES as u32)?.cast::<f32>();
                    let dst = std::slice::from_raw_parts_mut(dst, FRAMES * dev_ch);
                    if running && got >= 2 {
                        let n = got * 2;
                        for (d, &s) in scratch[..n].iter_mut().zip(out.pcm[..n].iter()) {
                            *d = s as f32 / 32768.0;
                        }
                        // Resample the pulled frame to exactly one device period.
                        fit(&scratch[..n], 2, dst, dev_ch);
                        if want != FRAMES {
                            self.stats.resampled += 1;
                        }
                    } else {
                        // Nothing playing, or the ring is empty: a period of silence.
                        dst.fill(0.0);
                    }
                    self.render.ReleaseBuffer(FRAMES as u32, 0)?;

                    let ahead = Duration::from_micros(
                        queued as u64 * 1_000_000 / self.format.sample_rate as u64,
                    );
                    self.stats.queue_total += ahead;
                    self.stats.queue_max = self.stats.queue_max.max(ahead);
                    self.stats.frames += 1;
                    queued += FRAMES;
                }
                self.stats.wakeup(woke.elapsed());
                ipc::TELEMETRY.set_vspk_errors(self.stats.dropped + self.stats.underruns);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing driver is reported as Disconnected (checked before the headset
    /// is touched), so the supervisor keeps retrying until the driver is installed.
    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn missing_driver_is_disconnected() {
        assert!(matches!(Vspk::open(None), Err(SelectError::Disconnected)));
    }
}
