//! Microphone input through WASAPI: shared mode, event-driven, 48 kHz mono f32.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, IAudioCaptureClient, IAudioClient,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Owned;

use crate::devices::{Device, Direction, SelectError};
use crate::format::AudioFormat;
use crate::frame::AudioFrame;
use crate::ipc;
use crate::ring::Producer;
use crate::stream::{self, Stats, WAIT_MS};

/// Endpoint buffer, in 100 ns units: 100 ms of slack for a late capture
/// thread. Latency comes from the device period, not from this.
const BUFFER_HNS: i64 = 1_000_000;

/// An initialized, not yet started, microphone stream. Windows converts the
/// device's own rate and channel count to `format` (AUTOCONVERTPCM).
pub struct Mic {
    pub device: Device,
    /// Stream format: 48 kHz mono f32. `frames` is one device period, the size
    /// of the packet each event announces.
    pub format: AudioFormat,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    pub client: IAudioClient,
    pub capture: IAudioCaptureClient,
    /// Signalled once per device period after `client.Start()`.
    pub event: Owned<HANDLE>,
    pub stats: Stats,
}

impl Mic {
    /// Opens the microphone with `id`, or for `None` the Windows default
    /// communications microphone. Any failure comes back as an error.
    pub fn open(id: Option<&str>) -> Result<Mic, SelectError> {
        let s = stream::open(Direction::Input, id, AudioFormat::MIC, BUFFER_HNS)?;
        Ok(Mic {
            // SAFETY: a COM call on the client `open` just initialized.
            capture: unsafe { s.client.GetService()? },
            device: s.device,
            format: s.format,
            buffer_frames: s.buffer_frames,
            client: s.client,
            event: s.event,
            stats: Stats::default(),
        })
    }

    /// Captures until `stop` is set or the device fails, pushing 10 ms frames
    /// into `out` (dropped and counted in `stats` when it is full). Call it on
    /// the thread that opened the mic: apart from waiting for the device's event
    /// it never blocks, allocates or does I/O. A device that goes away ends it
    /// with `SelectError::Disconnected`.
    /// `watch`, when it is set, means the Windows default device changed: the
    /// stream stops cleanly so the supervisor reopens on the new default.
    pub fn run(
        &mut self,
        out: &mut Producer,
        stop: &AtomicBool,
        watch: Option<&AtomicBool>,
    ) -> Result<(), SelectError> {
        stream::realtime(|| self.pump(out, stop, watch))
    }

    fn pump(
        &mut self,
        out: &mut Producer,
        stop: &AtomicBool,
        watch: Option<&AtomicBool>,
    ) -> Result<(), SelectError> {
        let mut frame = AudioFrame::new(self.format.channels);
        let mut fill = 0;
        // When the frame being filled had its first sample recorded (100 ns units).
        let mut frame_start = 0;
        let channels = self.format.channels as usize;
        let hns_per_frame = 10_000_000 / self.format.sample_rate as u64;
        // Windows flags the first packet after Start as a discontinuity; that's not a glitch.
        let mut first_packet = true;
        // SAFETY: COM calls on this stream's own interfaces. Each packet from
        // GetBuffer is read, then released, before the next one is fetched.
        unsafe {
            self.client.Start()?;
            while !stop.load(Relaxed) {
                WaitForSingleObject(*self.event, WAIT_MS);
                // Default device changed: stop so the supervisor reopens on it.
                if watch.is_some_and(|w| w.swap(false, Relaxed)) {
                    break;
                }
                let woke = Instant::now();
                while self.capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags, mut recorded) =
                        (std::ptr::null_mut(), 0, 0, 0);
                    self.capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        None,
                        Some(&mut recorded),
                    )?;
                    // WASAPI's timestamp of the packet's first sample; now if it has none.
                    if recorded == 0 || flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                        recorded = stream::qpc_hns();
                    }
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && !first_packet
                    {
                        self.stats.glitches += 1;
                    }
                    first_packet = false;
                    let len = frames as usize * self.format.channels as usize;
                    // A silent packet's data is meaningless: read it as zeros.
                    let packet = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0)
                        .then(|| std::slice::from_raw_parts(data.cast::<f32>(), len));
                    let mut i = 0;
                    while i < len {
                        if fill == 0 {
                            frame_start = recorded + (i / channels) as u64 * hns_per_frame;
                        }
                        let n = (frame.sample_count() - fill).min(len - i);
                        let dst = &mut frame.samples_mut()[fill..fill + n];
                        match packet {
                            Some(p) => dst.copy_from_slice(&p[i..i + n]),
                            None => dst.fill(0.0),
                        }
                        fill += n;
                        i += n;
                        if fill == frame.sample_count() {
                            // Capture latency: first sample recorded -> frame handed on.
                            let age = stream::qpc_hns().saturating_sub(frame_start);
                            let age = Duration::from_nanos(age * 100);
                            let now = Instant::now();
                            frame.captured = Some(now.checked_sub(age).unwrap_or(now));
                            self.stats.latency_total += age;
                            self.stats.latency_max = self.stats.latency_max.max(age);
                            if out.push(&frame) {
                                self.stats.frames += 1;
                            } else {
                                self.stats.dropped += 1;
                            }
                            frame.seq += 1;
                            fill = 0;
                        }
                    }
                    self.capture.ReleaseBuffer(frames)?;
                }
                self.stats.wakeup(woke.elapsed());
                ipc::TELEMETRY.set_mic_errors(self.stats.dropped + self.stats.glitches);
                ipc::TELEMETRY
                    .mic_dropped
                    .store(self.stats.dropped, Relaxed);
                ipc::TELEMETRY
                    .mic_glitches
                    .store(self.stats.glitches, Relaxed);
            }
            self.client.Stop()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices;
    use crate::format::SampleFormat;
    use crate::ring;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{E_ACCESSDENIED, ERROR_INSUFFICIENT_BUFFER};

    /// Diagnostic: cargo test opens_microphones -- --nocapture
    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn opens_microphones() {
        let ids: Vec<String> = devices::list()
            .unwrap()
            .into_iter()
            .filter(|d| d.direction == Direction::Input)
            .map(|d| d.id)
            .collect();
        // The default first, then every active microphone by ID.
        for id in [None]
            .into_iter()
            .chain(ids.iter().map(|id| Some(id.as_str())))
        {
            let mic = Mic::open(id).unwrap_or_else(|e| panic!("{e:?}{}", hint(&e)));
            let (d, f) = (&mic.device, &mic.format);
            println!(
                "{}{}\n  device {} Hz {} ch -> stream {} Hz {} ch {:?}, \
                 packet {} frames ({} ms), buffer {} frames ({} ms)",
                if id.is_none() { "[default] " } else { "" },
                d.name,
                d.sample_rate,
                d.channels,
                f.sample_rate,
                f.channels,
                f.sample_format,
                f.frames,
                f.frames * 1000 / f.sample_rate as usize,
                mic.buffer_frames,
                mic.buffer_frames * 1000 / f.sample_rate,
            );
            assert_eq!(
                (f.sample_rate, f.channels, f.sample_format),
                (48_000, 1, SampleFormat::F32)
            );
            assert!(f.frames > 0 && mic.buffer_frames as usize >= f.frames);
        }
    }

    /// Plain-English causes for the failures seen in practice.
    fn hint(e: &SelectError) -> &'static str {
        match e {
            SelectError::Windows(w) if w.code() == E_ACCESSDENIED => {
                "\nMicrophone access is off: Settings > Privacy & security > Microphone."
            }
            SelectError::Windows(w) if w.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {
                "\nWindows rejects microphone use from an .exe path this long; \
                 build somewhere shorter, e.g. set CARGO_TARGET_DIR."
            }
            _ => "",
        }
    }

    /// Diagnostic: captures 2 s from the default microphone and reads it back
    /// one frame per 10 ms, as the next stage will. Audio is measured, never stored.
    /// cargo test microphone_diagnostic -- --ignored --nocapture
    #[test]
    #[ignore = "records 2 s from the default microphone"]
    fn microphone_diagnostic() {
        const READS: u32 = 200;
        let (mut tx, mut rx) = ring::new(8);
        let stop = Arc::new(AtomicBool::new(false));
        let capture = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut mic = Mic::open(None).unwrap_or_else(|e| panic!("{e:?}{}", hint(&e)));
                let result = mic.run(&mut tx, &stop, None);
                (mic.device.name, mic.format, mic.stats, result)
            }
        });
        let (mut frames, mut underruns, mut peak) = (0u32, 0u32, 0f32);
        let mut take = |f: AudioFrame| {
            assert!(f.samples().iter().all(|s| s.is_finite()), "corrupt sample");
            peak = f.samples().iter().fold(peak, |p, s| p.max(s.abs()));
            frames += 1;
        };
        // Wait for the first frame, then read every 10 ms, half a period behind
        // the microphone. An empty ring at read time is an underrun.
        let waiting = Instant::now();
        let first = loop {
            if let Some(f) = rx.pop() {
                break f;
            }
            assert!(waiting.elapsed() < Duration::from_secs(5), "no audio");
            thread::sleep(Duration::from_millis(1));
        };
        take(first);
        let start = Instant::now() + Duration::from_millis(5);
        for read in 1..READS {
            let due = start + Duration::from_millis(10) * read;
            thread::sleep(due.saturating_duration_since(Instant::now()));
            match rx.pop() {
                Some(f) => take(f),
                None => underruns += 1,
            }
        }
        stop.store(true, Relaxed);
        let (name, f, stats, result) = capture.join().unwrap();
        let period_ms = f.frames as f64 * 1000.0 / f.sample_rate as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        println!(
            "{name}\n  \
             format      {} Hz, {} ch, {:?}, {} frames per packet ({period_ms} ms)\n  \
             captured    {frames} frames ({:.2} s of audio), peak {:.1} dBFS\n  \
             underruns   {underruns} (ring empty when read)\n  \
             overruns    {} (ring full, frame dropped)\n  \
             errors      {} discontinuities, run {result:?}\n  \
             processing  avg {:.3} ms, max {:.3} ms per wakeup, budget {period_ms} ms",
            f.sample_rate,
            f.channels,
            f.sample_format,
            f.frames,
            frames as f64 * period_ms / 1000.0,
            20.0 * peak.max(1e-6).log10(),
            stats.dropped,
            stats.glitches,
            ms(stats.busy_total) / stats.wakeups.max(1) as f64,
            ms(stats.busy_max),
        );
        result.unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fails_cleanly_on_unknown_device() {
        let result = Mic::open(Some("not-a-device"));
        assert!(matches!(result, Err(SelectError::UnknownId)));
    }
}
