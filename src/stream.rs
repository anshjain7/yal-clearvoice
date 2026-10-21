//! WASAPI setup shared by the microphone and the speaker: shared mode,
//! event-driven, f32 at the requested rate and channel count, with Windows
//! converting from the device's own format (AUTOCONVERTPCM).

use std::ops::AddAssign;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetProcessInformation,
};
use windows::core::{GUID, HSTRING, Owned, PCWSTR, w};

use crate::devices::{self, Device, Direction, SelectError};
use crate::format::AudioFormat;

// From mmreg.h and ksmedia.h.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Longest wait for the device's event before `run` rechecks `stop` and the device.
pub(crate) const WAIT_MS: u32 = 100;

/// What a stream's `run` measured. A wakeup is one event (or timeout) from the
/// device; its busy time is the work done before waiting again, against a
/// real-time budget of one device period.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Frames moved between the device and the ring.
    pub frames: u64,
    /// Capture: frames dropped because the ring was full.
    pub dropped: u64,
    /// Playback: 10 ms gaps of silence played because the ring was empty as the
    /// device was about to run dry, counted once audio has started.
    pub underruns: u64,
    /// Capture: gaps Windows reported in the audio (data discontinuities).
    pub glitches: u64,
    /// Playback: frames played a sample longer or shorter to absorb clock drift.
    pub resampled: u64,
    /// Playback: stale frames skipped to cut a backlog over 30 ms after a stall.
    pub skipped: u64,
    pub wakeups: u64,
    pub busy_total: Duration,
    pub busy_max: Duration,
    /// Over frames handed on, measured from when WASAPI recorded a frame's
    /// first sample. Capture: to the frame leaving the mic (includes the 10 ms
    /// it takes to fill). Playback: to that sample reaching the speaker
    /// (includes the audio queued ahead of it).
    pub latency_total: Duration,
    pub latency_max: Duration,
    /// Playback: audio already queued for the device as each frame went in.
    pub queue_total: Duration,
    pub queue_max: Duration,
}

impl Stats {
    pub(crate) fn wakeup(&mut self, busy: Duration) {
        self.wakeups += 1;
        self.busy_total += busy;
        self.busy_max = self.busy_max.max(busy);
    }
}

impl AddAssign for Stats {
    fn add_assign(&mut self, o: Self) {
        self.frames += o.frames;
        self.dropped += o.dropped;
        self.underruns += o.underruns;
        self.glitches += o.glitches;
        self.resampled += o.resampled;
        self.skipped += o.skipped;
        self.wakeups += o.wakeups;
        self.busy_total += o.busy_total;
        self.busy_max = self.busy_max.max(o.busy_max);
        self.latency_total += o.latency_total;
        self.latency_max = self.latency_max.max(o.latency_max);
        self.queue_total += o.queue_total;
        self.queue_max = self.queue_max.max(o.queue_max);
    }
}

/// The performance counter in 100 ns units: the clock WASAPI timestamps with.
pub(crate) fn qpc_hns() -> u64 {
    let (mut count, mut frequency) = (0i64, 1i64);
    // SAFETY: plain Win32 calls writing to locals; they can't fail since Windows XP.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (count as u128 * 10_000_000 / frequency.max(1) as u128) as u64
}

/// Keeps a stream going until `stop`: opens it, runs it, and whenever opening
/// or running fails (device unplugged or missing, audio service restarted)
/// waits `retry` and opens it again. `run` returns a session's stats and how it
/// ended. Returns the stats summed over sessions, the failure count and the
/// last failure.
pub fn supervise<S>(
    stop: &AtomicBool,
    retry: Duration,
    mut open: impl FnMut() -> Result<S, SelectError>,
    mut run: impl FnMut(&mut S) -> (Stats, Result<(), SelectError>),
) -> (Stats, u32, Option<SelectError>) {
    let (mut total, mut failures, mut last) = (Stats::default(), 0, None);
    while !stop.load(Relaxed) {
        let result = open().and_then(|mut stream| {
            let (stats, result) = run(&mut stream);
            total += stats;
            result
        });
        if let Err(e) = result {
            failures += 1;
            last = Some(e);
            thread::sleep(retry);
        }
    }
    (total, failures, last)
}

/// An initialized, not yet started, stream.
pub(crate) struct Stream {
    pub device: Device,
    /// `want`, with `frames` set to one device period.
    pub format: AudioFormat,
    /// Endpoint buffer capacity in frames.
    pub buffer_frames: u32,
    pub client: IAudioClient,
    /// Signalled once per device period after `client.Start()`.
    pub event: Owned<HANDLE>,
}

/// Opens the `direction` device with `id`, or for `None` the Windows default
/// communications device, as `want` (mono or stereo f32) with an endpoint
/// buffer of `buffer_hns` (100 ns units).
pub(crate) fn open(
    direction: Direction,
    id: Option<&str>,
    want: AudioFormat,
    buffer_hns: i64,
) -> Result<Stream, SelectError> {
    let device = devices::select(direction, id)?;
    let wave = wave_format(want);
    // SAFETY: COM calls on interfaces returned by COM, `wave` outlives
    // Initialize, and the event is owned (closed on drop) as soon as it exists.
    unsafe {
        let client: IAudioClient = devices::enumerator()?
            .GetDevice(&HSTRING::from(device.id.as_str()))?
            .Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_hns,
            0,
            (&raw const wave).cast(),
            None,
        )?;
        let mut period = 0;
        client.GetDevicePeriod(Some(&mut period), None)?;
        let event = Owned::new(CreateEventW(None, false, false, PCWSTR::null())?);
        client.SetEventHandle(*event)?;
        Ok(Stream {
            format: AudioFormat {
                frames: (period * want.sample_rate as i64 / 10_000_000) as usize,
                ..want
            },
            buffer_frames: client.GetBufferSize()?,
            client,
            event,
            device,
        })
    }
}

/// Opens device `id` as a polled capture stream, as `want` (mono or stereo
/// f32): an input device as itself, an output device in loopback (everything
/// Windows plays to it). Polled, not event-driven: a loopback stream gets no
/// packets while nothing plays.
pub(crate) fn open_polled(
    direction: Direction,
    id: &str,
    want: AudioFormat,
    buffer_hns: i64,
) -> Result<(Device, IAudioClient), SelectError> {
    let device = devices::select(direction, Some(id))?;
    let wave = wave_format(want);
    let loopback = if direction == Direction::Output {
        AUDCLNT_STREAMFLAGS_LOOPBACK
    } else {
        0
    };
    // SAFETY: COM calls on interfaces returned by COM; `wave` outlives Initialize.
    unsafe {
        let client: IAudioClient = devices::enumerator()?
            .GetDevice(&HSTRING::from(device.id.as_str()))?
            .Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            loopback | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_hns,
            0,
            (&raw const wave).cast(),
            None,
        )?;
        Ok((device, client))
    }
}

/// `want` (mono or stereo f32) as the format WASAPI's Initialize takes.
fn wave_format(want: AudioFormat) -> WAVEFORMATEXTENSIBLE {
    let block = want.channels * want.sample_format.bytes() as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: want.channels,
            nSamplesPerSec: want.sample_rate,
            nAvgBytesPerSec: want.sample_rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: if want.channels == 1 {
            SPEAKER_FRONT_CENTER
        } else {
            SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        },
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Asks Windows never to slow this process down to save power. Windows 11 runs
/// background processes that have no window ("efficiency mode") on slower
/// cores at lower clock speeds, and the engine is one: noise removal falling
/// behind there is a gap in the call. Does nothing where Windows lacks it.
pub fn keep_full_speed() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0, // throttling off for both
    };
    // SAFETY: a PROCESS_POWER_THROTTLING_STATE of the size given, for this process.
    let _ = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
}

/// Runs a stream's `pump` at MMCSS "Pro Audio" priority (normal priority if
/// that service is off) and reports a removed device as `Disconnected`.
pub(crate) fn realtime(pump: impl FnOnce() -> Result<(), SelectError>) -> Result<(), SelectError> {
    let mut task = 0;
    // SAFETY: plain Win32 calls; the registration is reverted below.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) };
    let result = pump();
    if let Ok(handle) = mmcss {
        let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
    }
    result.map_err(|e| match e {
        SelectError::Windows(w) if w.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
            SelectError::Disconnected
        }
        e => e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervise_reopens_until_stopped() {
        let stop = AtomicBool::new(false);
        let mut opens = 0;
        let session = Stats {
            frames: 10,
            ..Stats::default()
        };
        // Missing twice, then a session that loses its device, then one that
        // runs until stopped.
        let (total, failures, last) = supervise(
            &stop,
            Duration::ZERO,
            || {
                opens += 1;
                if opens <= 2 {
                    Err(SelectError::NoDevice)
                } else {
                    Ok(opens)
                }
            },
            |&mut n| {
                if n == 3 {
                    (session, Err(SelectError::Disconnected))
                } else {
                    stop.store(true, Relaxed);
                    (session, Ok(()))
                }
            },
        );
        assert_eq!((opens, failures, total.frames), (4, 3, 20));
        assert!(matches!(last, Some(SelectError::Disconnected)));
    }
}
