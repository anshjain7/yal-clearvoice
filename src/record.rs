//! Call recordings. While the agent has agreed (each time "Record call audio"
//! is ticked), each transcribed call is recorded, start to stop, into one file
//! the window names, e.g. `Recordings\2026-10-14 10-32-05.wav`: the customer
//! (loopback of the agent's headset: what the agent hears) mixed with the
//! agent's voice as sent to the call ("CABLE Output", after noise removal).
//! 16 kHz mono, so it plays anywhere.
//!
//! Next to it, `<name> log.csv` notes once a second what the engine was doing
//! (noise removal, its slowest frame, CPU, how loud the microphone and the
//! sent voice were, microphone and send-path glitches), timed from the start
//! of the recording, so a breakup heard in the recording can be matched to
//! its cause: the agent's side apart from the customer's, and a microphone
//! that went quiet apart from noise removal that removed speech. Each side is recorded by its own polled
//! stream, off the audio path. Nothing leaves the PC.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::devices::Direction;
use crate::ipc::{self, TELEMETRY};
use crate::stream::{Stats, supervise};
use crate::transcribe::{CHUNK, RATE, Tap};

/// What the window asks of the recorder.
pub enum Cmd {
    /// Start recording into this .wav file (ending any recording in progress).
    Start(PathBuf),
    Stop,
}

/// Where each side of the call comes from.
pub struct Sources {
    /// The agent's headset or speaker, where the customer is heard.
    pub customer: fn() -> Option<String>,
    /// VB-CABLE's "CABLE Output": the agent's voice as the calling app gets it.
    pub agent: fn() -> Option<String>,
}

/// Runs the recorder until `cmds` closes.
pub fn run(cmds: Receiver<Cmd>, sources: &Sources) {
    let mut session: Option<Session> = None;
    loop {
        match cmds.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Start(path)) => {
                if let Some(s) = session.take() {
                    s.finish();
                }
                let sides = [
                    ("customer", Direction::Output, (sources.customer)()),
                    ("agent", Direction::Input, (sources.agent)()),
                ];
                session = Session::start(&path, sides).ok();
            }
            Ok(Cmd::Stop) => {
                if let Some(s) = session.take() {
                    s.finish();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(s) = &mut session {
            s.pump();
        }
    }
    if let Some(s) = session {
        s.finish();
    }
}

/// The log next to a recording: `2026-10-14 10-32-05.wav` → `2026-10-14 10-32-05 log.csv`.
fn log_path(wav: &Path) -> PathBuf {
    let stem = wav.file_stem().unwrap_or_default().to_string_lossy();
    wav.with_file_name(format!("{stem} log.csv"))
}

/// The call recording in progress, for voice.rs to save the agent's cleaned
/// voice next to it.
static RECORDING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The call recording in progress (its .wav), if any.
pub(crate) fn recording() -> Option<PathBuf> {
    RECORDING.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Where the agent's cleaned voice goes for the recording `wav`:
/// `2026-10-14 10-32-05.wav` -> `2026-10-14 10-32-05 cleaned voice.wav`.
pub(crate) fn cleaned_path(wav: &Path) -> PathBuf {
    let stem = wav.file_stem().unwrap_or_default().to_string_lossy();
    wav.with_file_name(format!("{stem} cleaned voice.wav"))
}

/// A side to record: its name, and the device it comes from, if found.
type SideSource = (&'static str, Direction, Option<String>);

/// One recording: its sides, the file they're mixed into, and its log.
struct Session {
    started: Instant,
    sides: Vec<Side>,
    wav: Option<Wav>,
    log: File,
    next_line: Duration,
    last: Counts,
}

impl Session {
    fn start<const N: usize>(path: &Path, sides: [SideSource; N]) -> io::Result<Session> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let wav = Wav::create(path)?;
        let mut log = File::create(log_path(path))?;
        *RECORDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
        writeln!(
            log,
            "seconds,noise_removal,slowest_frame_ms,cpu_percent,latency_ms,\
             mic_level_db,sent_level_db,mic_dropped,mic_glitches,sent_underruns,\
             sent_skipped,note"
        )?;
        for peak in [
            &TELEMETRY.busy_peak_us,
            &TELEMETRY.mic_peak,
            &TELEMETRY.sent_peak,
        ] {
            peak.store(0, Relaxed);
        }
        let mut s = Session {
            started: Instant::now(),
            sides: Vec::new(),
            wav: Some(wav),
            log,
            next_line: Duration::from_secs(1),
            last: Counts::now(),
        };
        for (name, direction, id) in sides {
            match id {
                Some(id) => s.sides.push(Side::start(direction, id)),
                None => s.note(&format!("{name}: device not found, not recorded")),
            }
        }
        s.note("recording started");
        Ok(s)
    }

    /// Mixes in the audio both sides have delivered, and writes the log line
    /// once a second.
    fn pump(&mut self) {
        let elapsed = self.started.elapsed();
        for side in &mut self.sides {
            side.take(elapsed);
        }
        let ready = self
            .sides
            .iter()
            .map(|s| s.pending.len())
            .min()
            .unwrap_or(0);
        self.write(ready);
        if elapsed >= self.next_line {
            self.next_line += Duration::from_secs(1);
            self.line();
        }
    }

    /// Mixes the next `n` samples of every side into the recording.
    fn write(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let mixed = mix(self.sides.iter_mut().map(|s| &mut s.pending), n);
        if let Some(wav) = &mut self.wav
            && let Err(e) = wav.write(&mixed)
        {
            self.wav = None;
            self.note(&format!("recording couldn't be written ({e})"));
        }
    }

    /// The once-a-second line: the engine's state and what went wrong since
    /// the last line.
    fn line(&mut self) {
        let now = Counts::now();
        let new = now.since(self.last);
        self.last = now;
        let peak_ms = TELEMETRY.busy_peak_us.swap(0, Relaxed) as f64 / 1000.0;
        let _ = writeln!(
            self.log,
            "{:.1},{},{peak_ms:.2},{},{},{:.0},{:.0},{},{},{},{},",
            self.started.elapsed().as_secs_f64(),
            noise_removal(),
            TELEMETRY.cpu_pct.load(Relaxed),
            TELEMETRY.latency_us.load(Relaxed) / 1000,
            db(TELEMETRY.mic_peak.swap(0, Relaxed)),
            db(TELEMETRY.sent_peak.swap(0, Relaxed)),
            new.mic_dropped,
            new.mic_glitches,
            new.out_underruns,
            new.out_skipped,
        );
    }

    fn note(&mut self, note: &str) {
        let _ = writeln!(
            self.log,
            "{:.1},,,,,,,,,,,{}",
            self.started.elapsed().as_secs_f64(),
            note.replace(',', ";")
        );
    }

    /// Stops both sides and mixes in everything they delivered, the shorter
    /// side padded with silence.
    fn finish(mut self) {
        *RECORDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        for side in &mut self.sides {
            side.stop();
        }
        let rest = self
            .sides
            .iter()
            .map(|s| s.pending.len())
            .max()
            .unwrap_or(0);
        self.write(rest);
        self.note("recording stopped");
        if let Some(mut wav) = self.wav.take() {
            let _ = wav.patch();
        }
    }
}

/// Takes the next `n` samples from each queue (silence where one has fewer)
/// and adds them together.
fn mix<'a>(queues: impl IntoIterator<Item = &'a mut VecDeque<i16>>, n: usize) -> Vec<i16> {
    let mut out = vec![0i16; n];
    for queue in queues {
        let take = n.min(queue.len());
        for (o, s) in out.iter_mut().zip(queue.drain(..take)) {
            *o = o.saturating_add(s);
        }
    }
    out
}

/// A level from `ipc::TELEMETRY` (RMS in millionths of full scale) in dBFS:
/// about -20 for speech close to the microphone, -120 for none at all (no
/// audio came through that second).
fn db(level: u64) -> f64 {
    (20.0 * (level as f64 / 1e6).log10()).max(-120.0)
}

/// What the window's "Noise removal (AI)" box would say.
fn noise_removal() -> &'static str {
    if !TELEMETRY.nc_on.load(Relaxed) {
        return "off";
    }
    match TELEMETRY.dfn_state.load(Relaxed) {
        ipc::DFN_RUNNING => "active",
        ipc::DFN_OVERLOADED => "paused (PC busy)",
        ipc::DFN_FAILED => "stopped (error)",
        _ => "starting",
    }
}

/// The engine's breakup-related counts at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counts {
    mic_dropped: u64,
    mic_glitches: u64,
    out_underruns: u64,
    out_skipped: u64,
}

impl Counts {
    fn now() -> Counts {
        Counts {
            mic_dropped: TELEMETRY.mic_dropped.load(Relaxed),
            mic_glitches: TELEMETRY.mic_glitches.load(Relaxed),
            out_underruns: TELEMETRY.out_underruns.load(Relaxed),
            out_skipped: TELEMETRY.out_skipped.load(Relaxed),
        }
    }

    /// How much each count grew since `before`. A stream that reopened
    /// restarted its count from 0, so then the whole count is new.
    fn since(self, before: Counts) -> Counts {
        let grew = |now: u64, then: u64| if now >= then { now - then } else { now };
        Counts {
            mic_dropped: grew(self.mic_dropped, before.mic_dropped),
            mic_glitches: grew(self.mic_glitches, before.mic_glitches),
            out_underruns: grew(self.out_underruns, before.out_underruns),
            out_skipped: grew(self.out_skipped, before.out_skipped),
        }
    }
}

/// One side of the call, recorded from its device by its own thread: an
/// input device, or an output device by loopback.
struct Side {
    chunks: Receiver<Vec<i16>>,
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Audio received (and silence added) but not yet mixed.
    pending: VecDeque<i16>,
    /// Everything received so far, silence included.
    received: u64,
}

impl Side {
    fn start(direction: Direction, id: String) -> Side {
        let (tx, chunks) = mpsc::channel();
        let quit = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&quit);
        let thread = thread::spawn(move || {
            supervise(
                &flag,
                Duration::from_secs(1),
                || Tap::open(direction, &id),
                |tap| {
                    let result = tap.run(&|| None::<String>, &flag, |chunk| {
                        let _ = tx.send(chunk);
                    });
                    (Stats::default(), result)
                },
            );
        });
        Side {
            chunks,
            quit,
            thread: Some(thread),
            pending: VecDeque::new(),
            received: 0,
        }
    }

    /// Takes what has arrived; if the device stalled or is reopening, adds
    /// silence so this side keeps time with the other and with the log.
    fn take(&mut self, elapsed: Duration) {
        self.drain();
        let missing = gap(elapsed, self.received);
        self.received += missing as u64;
        self.pending.extend(std::iter::repeat_n(0, missing));
    }

    fn drain(&mut self) {
        for chunk in self.chunks.try_iter() {
            self.received += chunk.len() as u64;
            self.pending.extend(chunk);
        }
    }

    fn stop(&mut self) {
        self.quit.store(true, Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.drain();
    }
}

/// Silence to add to a side `received` samples long, `elapsed` into the
/// recording: none while it's under 300 ms behind (audio arrives in 100 ms
/// chunks), else enough to leave it one chunk behind.
fn gap(elapsed: Duration, received: u64) -> usize {
    let due = (elapsed.as_secs_f64() * f64::from(RATE)) as u64;
    let behind = due.saturating_sub(received);
    if behind > u64::from(RATE) * 3 / 10 {
        (behind - CHUNK as u64) as usize
    } else {
        0
    }
}

/// Writes `samples` (16 kHz mono) to `path` as a WAV file.
pub(crate) fn write_wav(path: &Path, samples: &[i16]) -> io::Result<()> {
    let mut wav = Wav::create(path)?;
    wav.write(samples)?;
    wav.patch()
}

/// A 16 kHz mono 16-bit WAV file being written. Its header is rewritten every
/// 5 s, so a recording cut off by a crash or power cut still plays.
pub(crate) struct Wav {
    file: File,
    samples: u64,
    header_at: u64,
}

impl Wav {
    pub(crate) fn create(path: &Path) -> io::Result<Wav> {
        let mut file = File::create(path)?;
        file.write_all(&header(0))?;
        Ok(Wav {
            file,
            samples: 0,
            header_at: 0,
        })
    }

    pub(crate) fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.file.write_all(&bytes)?;
        self.samples += samples.len() as u64;
        if self.samples - self.header_at >= u64::from(RATE) * 5 {
            self.patch()?;
        }
        Ok(())
    }

    /// Writes the current length into the header.
    pub(crate) fn patch(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.samples))?;
        self.file.seek(SeekFrom::End(0))?;
        self.header_at = self.samples;
        Ok(())
    }
}

/// The 44-byte header of a 16 kHz mono 16-bit PCM WAV holding `samples`.
fn header(samples: u64) -> [u8; 44] {
    let data = (samples * 2).min(u64::from(u32::MAX - 36)) as u32;
    let mut h = [0u8; 44];
    let mut put = |at: usize, bytes: &[u8]| h[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, b"RIFF");
    put(4, &(36 + data).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes()); // fmt chunk size
    put(20, &1u16.to_le_bytes()); // PCM
    put(22, &1u16.to_le_bytes()); // mono
    put(24, &RATE.to_le_bytes());
    put(28, &(RATE * 2).to_le_bytes()); // bytes per second
    put(32, &2u16.to_le_bytes()); // bytes per sample
    put(34, &16u16.to_le_bytes()); // bits per sample
    put(36, b"data");
    put(40, &data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clearvoice-record-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn header_describes_16k_mono_pcm() {
        let h = header(16_000);
        let u32_at = |at: usize| u32::from_le_bytes(h[at..at + 4].try_into().unwrap());
        let u16_at = |at: usize| u16::from_le_bytes(h[at..at + 2].try_into().unwrap());
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(
            (u16_at(20), u16_at(22), u32_at(24), u16_at(34)),
            (1, 1, 16_000, 16)
        );
        assert_eq!((u32_at(40), u32_at(4)), (32_000, 32_036));
    }

    #[test]
    fn wav_file_holds_what_was_written() {
        let dir = temp_dir("wav");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let mut wav = Wav::create(&path).unwrap();
        wav.write(&vec![1000i16; 16_000 * 6]).unwrap(); // past one 5 s header update
        wav.write(&[-1, 2]).unwrap();
        wav.patch().unwrap();
        drop(wav);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + (16_000 * 6 + 2) * 2);
        assert_eq!(&bytes[..44], &header(16_000 * 6 + 2));
        assert_eq!(&bytes[bytes.len() - 4..], &[0xff, 0xff, 2, 0]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn both_sides_are_mixed_into_one() {
        let mut customer = VecDeque::from(vec![1000, 2000, 3000, 30_000]);
        let mut agent = VecDeque::from(vec![10, -20]);
        // The shorter side counts as silence; loud overlaps don't wrap around.
        assert_eq!(mix([&mut customer, &mut agent], 3), [1010, 1980, 3000]);
        agent.push_back(10_000);
        assert_eq!(mix([&mut customer, &mut agent], 1), [i16::MAX]);
        assert!(customer.is_empty() && agent.is_empty());
    }

    #[test]
    fn levels_read_in_dbfs() {
        assert_eq!(db(1_000_000), 0.0);
        assert!((db(100_000) + 20.0).abs() < 1e-9);
        assert_eq!(db(0), -120.0);
    }

    #[test]
    fn cleaned_voice_sits_next_to_its_recording() {
        assert_eq!(
            cleaned_path(Path::new(r"C:\Rec\2026-10-14 10-32-05.wav")),
            Path::new(r"C:\Rec\2026-10-14 10-32-05 cleaned voice.wav")
        );
    }

    #[test]
    fn log_sits_next_to_its_recording() {
        assert_eq!(
            log_path(Path::new(r"C:\Rec\2026-10-14 10-32-05.wav")),
            Path::new(r"C:\Rec\2026-10-14 10-32-05 log.csv")
        );
    }

    #[test]
    fn gaps_fill_only_when_a_side_falls_behind() {
        let s = |ms| Duration::from_millis(ms);
        assert_eq!(gap(s(1000), 16_000), 0);
        assert_eq!(gap(s(1250), 16_000), 0); // 250 ms behind: a chunk is on its way
        assert_eq!(gap(s(2000), 16_000), 16_000 - CHUNK); // stalled: catch up to one chunk behind
        assert_eq!(gap(s(500), 16_000), 0); // ahead is fine
    }

    #[test]
    fn counts_since_survive_a_stream_reopening() {
        let c = |n| Counts {
            mic_dropped: n,
            mic_glitches: n,
            out_underruns: n,
            out_skipped: n,
        };
        assert_eq!(c(7).since(c(5)), c(2));
        assert_eq!(c(3).since(c(5)), c(3)); // restarted from 0, then 3 more
    }

    /// A session with no devices still writes its log, with the reason.
    #[test]
    fn session_without_devices_logs_why() {
        let dir = temp_dir("session");
        let path = dir.join("2026-10-14 10-32-05.wav");
        let mut s = Session::start(
            &path,
            [
                ("customer", Direction::Output, None),
                ("agent", Direction::Input, None),
            ],
        )
        .unwrap();
        assert_eq!(recording(), Some(path.clone()));
        s.pump();
        s.finish();
        assert_eq!(recording(), None);
        let log = fs::read_to_string(dir.join("2026-10-14 10-32-05 log.csv")).unwrap();
        assert!(log.starts_with("seconds,noise_removal,"));
        assert!(
            log.lines().all(|l| l.matches(',').count() == 11),
            "every line has all 12 columns:\n{log}"
        );
        assert!(log.contains("customer: device not found"));
        assert!(log.contains("agent: device not found"));
        assert!(log.contains("recording stopped"));
        assert_eq!(fs::read(&path).unwrap(), header(0));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Records 3 s from VB-CABLE, a virtual device (silence unless something
    /// plays into it), as both sides: "CABLE Input" by loopback, as the
    /// customer is recorded from the headset, and "CABLE Output" as an input,
    /// as the agent is. Checks the mixed file's length.
    /// cargo test records_cable -- --ignored --nocapture
    #[test]
    #[ignore = "records from VB-CABLE"]
    fn records_cable() {
        use crate::devices;
        let cable = |direction, name| {
            devices::list()
                .unwrap()
                .into_iter()
                .find(|d| d.direction == direction && d.name.contains(name))
                .map(|d| d.id)
                .expect("VB-CABLE not installed")
        };
        let dir = temp_dir("cable");
        let path = dir.join("call.wav");
        let mut s = Session::start(
            &path,
            [
                (
                    "customer",
                    Direction::Output,
                    Some(cable(Direction::Output, "CABLE Input")),
                ),
                (
                    "agent",
                    Direction::Input,
                    Some(cable(Direction::Input, "CABLE Output")),
                ),
            ],
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            thread::sleep(Duration::from_millis(100));
            s.pump();
        }
        s.finish();
        println!("{}", fs::read_to_string(dir.join("call log.csv")).unwrap());
        let seconds = (fs::metadata(&path).unwrap().len() - 44) as f64 / 32_000.0;
        println!("call.wav: {seconds:.2} s");
        assert!((2.7..=3.3).contains(&seconds), "{seconds} s");
        fs::remove_dir_all(&dir).unwrap();
    }
}
