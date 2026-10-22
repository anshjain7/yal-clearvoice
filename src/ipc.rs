//! Control/telemetry link to the UI over a local named pipe
//! (`\\.\pipe\YalClearVoice`). Request/response, newline-delimited JSON, no audio
//! ever crosses it. The UI polls for telemetry and sends commands; the engine
//! applies commands by forwarding them to the main thread over a channel, so this
//! stays off the audio path and shares no locks with it.
//!
//! The thread is detached (never joined): it may block in ConnectNamedPipe waiting
//! for the UI, and process exit tears it down. Telemetry is read from a global of
//! atomics the engine's threads update cheaply (one Relaxed store each).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE,
};
use windows::core::PCWSTR;

/// PIPE_ACCESS_DUPLEX for CreateNamedPipeW's open mode (read + write).
const PIPE_ACCESS_DUPLEX: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAGS_AND_ATTRIBUTES(0x0000_0003);

use crate::devices::{self, Direction};
use crate::{record, transcribe};

/// DeepFilterNet processing state for the UI (`dfn_state`).
pub const DFN_BYPASS: u8 = 0;
pub const DFN_RUNNING: u8 = 1;
pub const DFN_FAILED: u8 = 2;
pub const DFN_OVERLOADED: u8 = 3;

/// Something the user must fix, shown by the UI (`notice`).
pub const NOTICE_NONE: u8 = 0;
/// VB-CABLE mode: the VB-CABLE driver isn't installed.
pub const NOTICE_NO_CABLE: u8 = 1;
/// The DeepFilterNet3 model file couldn't be loaded.
pub const NOTICE_NO_MODEL: u8 = 2;
/// VB-CABLE mode: Windows' default speaker is VB-CABLE, so the user hears
/// nothing and PC sounds leak into calls.
pub const NOTICE_CABLE_IS_SPEAKER: u8 = 3;

/// The agent's voice sample and the voice server, for the UI (`voice`):
/// no `[voice]` server set up; no sample yet; recording one; ready (the
/// server answered, or hasn't been needed yet); the server didn't answer
/// (the transcript uses the original audio meanwhile); the last recording
/// held too little speech to keep; recording failed.
pub const VOICE_OFF: u64 = 0;
pub const VOICE_NO_SAMPLE: u64 = 1;
pub const VOICE_RECORDING: u64 = 2;
pub const VOICE_READY: u64 = 3;
pub const VOICE_SERVER_DOWN: u64 = 4;
pub const VOICE_TOO_QUIET: u64 = 5;
pub const VOICE_FAILED: u64 = 6;

/// Live engine telemetry, updated by the audio threads (cheap Relaxed stores) and
/// read by the IPC thread. A process-wide singleton: there is one engine.
pub struct Telemetry {
    pub mic_connected: AtomicBool,
    pub spk_connected: AtomicBool,
    /// False when the output is fixed (VB-CABLE mode), so the UI greys out the
    /// speaker picker.
    pub spk_selectable: AtomicBool,
    /// Noise cancellation wanted (mirrors Control::enabled, set by main).
    pub nc_on: AtomicBool,
    pub dfn_state: AtomicU8,
    /// One of the NOTICE_* codes.
    pub notice: AtomicU8,
    /// One of the VOICE_* codes.
    pub voice: AtomicU64,
    /// End-to-end send-path latency, microseconds (set by the virtual mic).
    pub latency_us: AtomicU64,
    /// DeepFilterNet per-frame processing time, microseconds.
    pub busy_us: AtomicU64,
    /// The slowest DeepFilterNet frame since a call recording's log last read
    /// it (record.rs swaps it to 0), microseconds.
    pub busy_peak_us: AtomicU64,
    /// The loudest 10 ms frame into and out of noise removal since a call
    /// recording's log last read them (record.rs swaps them to 0), as RMS in
    /// millionths of full scale: whether the agent spoke, and what was sent.
    pub mic_peak: AtomicU64,
    pub sent_peak: AtomicU64,
    /// Breakup-related counts, each since its stream (re)opened: the mic's
    /// dropped frames and Windows-reported gaps, and on the send path, silence
    /// played for want of audio (underruns) and audio skipped to catch up.
    pub mic_dropped: AtomicU64,
    pub mic_glitches: AtomicU64,
    pub out_underruns: AtomicU64,
    pub out_skipped: AtomicU64,
    /// CPU, percent of one core (set by the IPC thread).
    pub cpu_pct: AtomicU64,
    mic_errors: AtomicU64,
    vmic_errors: AtomicU64,
    vspk_errors: AtomicU64,
    /// The microphone in use, and the headset (or speaker) the agent hears the
    /// customer on, by name. Set by main, rarely, so a lock is fine.
    mic_name: Mutex<String>,
    headset_name: Mutex<String>,
}

impl Telemetry {
    const fn new() -> Self {
        Telemetry {
            mic_connected: AtomicBool::new(false),
            spk_connected: AtomicBool::new(false),
            spk_selectable: AtomicBool::new(true),
            nc_on: AtomicBool::new(true),
            dfn_state: AtomicU8::new(DFN_BYPASS),
            voice: AtomicU64::new(VOICE_OFF),
            notice: AtomicU8::new(NOTICE_NONE),
            latency_us: AtomicU64::new(0),
            busy_us: AtomicU64::new(0),
            busy_peak_us: AtomicU64::new(0),
            mic_peak: AtomicU64::new(0),
            sent_peak: AtomicU64::new(0),
            mic_dropped: AtomicU64::new(0),
            mic_glitches: AtomicU64::new(0),
            out_underruns: AtomicU64::new(0),
            out_skipped: AtomicU64::new(0),
            cpu_pct: AtomicU64::new(0),
            mic_errors: AtomicU64::new(0),
            vmic_errors: AtomicU64::new(0),
            vspk_errors: AtomicU64::new(0),
            mic_name: Mutex::new(String::new()),
            headset_name: Mutex::new(String::new()),
        }
    }
    pub fn set_mic_name(&self, name: &str) {
        name.clone_into(&mut self.mic_name.lock().unwrap_or_else(|e| e.into_inner()));
    }
    pub fn set_headset_name(&self, name: &str) {
        name.clone_into(&mut self.headset_name.lock().unwrap_or_else(|e| e.into_inner()));
    }
    fn names(&self) -> (String, String) {
        let get = |m: &Mutex<String>| m.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (get(&self.mic_name), get(&self.headset_name))
    }
    /// The audio threads call these once per wake (one Relaxed store).
    pub fn set_mic_errors(&self, n: u64) {
        self.mic_errors.store(n, Relaxed);
    }
    pub fn set_vmic_errors(&self, n: u64) {
        self.vmic_errors.store(n, Relaxed);
    }
    pub fn set_vspk_errors(&self, n: u64) {
        self.vspk_errors.store(n, Relaxed);
    }
    fn errors(&self) -> u64 {
        self.mic_errors.load(Relaxed)
            + self.vmic_errors.load(Relaxed)
            + self.vspk_errors.load(Relaxed)
    }
}

/// The one telemetry instance.
pub static TELEMETRY: Telemetry = Telemetry::new();

/// A command from the UI for the main thread to apply.
pub enum UiCommand {
    /// Noise cancellation on/off.
    Nc(bool),
    /// Microphone: an explicit device ID, or None for the Windows default.
    SetMic(Option<String>),
    /// Speaker (physical headset): an explicit ID, or None for the default.
    SetSpk(Option<String>),
    /// A call recording: start (into a file) or stop.
    Record(record::Cmd),
    /// Record the agent's voice sample (see voice.rs).
    RecordVoice,
}

/// Total CPU time this process has used across all threads.
pub fn process_cpu() -> std::time::Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: a plain Win32 call writing four FILETIMEs we own.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .is_err()
    {
        return std::time::Duration::ZERO;
    }
    let hns = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    std::time::Duration::from_nanos((hns(kernel) + hns(user)) * 100)
}

/// Serves the control pipe forever: accept a UI client, answer its polls with
/// telemetry and forward its commands, then accept the next. Detached; runs until
/// the process exits.
pub fn serve(commands: Sender<UiCommand>) {
    let name: Vec<u16> = r"\\.\pipe\YalClearVoice"
        .encode_utf16()
        .chain([0])
        .collect();
    let (mut last_cpu, mut last_t) = (process_cpu(), Instant::now());
    loop {
        // SAFETY: a named-pipe server handle; closed before looping.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                NAMED_PIPE_MODE(0), // byte stream, blocking
                1,
                4096,
                4096,
                0,
                None, // default security: the creating user
            )
        };
        if pipe.is_invalid() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        // SAFETY: ConnectNamedPipe blocks until the UI connects; ERROR_PIPE_CONNECTED
        // means it already did. Either way we then serve it.
        let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if connected {
            serve_client(pipe, &commands, &mut last_cpu, &mut last_t);
        }
        // SAFETY: done with this client; drop the connection and handle.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
    }
}

/// Reads newline-delimited commands from one client and writes a JSON reply to
/// each, until the client disconnects.
fn serve_client(
    pipe: HANDLE,
    commands: &Sender<UiCommand>,
    last_cpu: &mut std::time::Duration,
    last_t: &mut Instant,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let mut read = 0u32;
        // SAFETY: reads one chunk into our buffer; 0 bytes or an error = client gone.
        if unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut read), None) }.is_err() || read == 0
        {
            return;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let cmd = String::from_utf8_lossy(&line);
            refresh_cpu(last_cpu, last_t);
            let reply = handle(cmd.trim(), commands);
            let out = format!("{reply}\n");
            let mut written = 0u32;
            // SAFETY: writes the reply back to the client; an error = client gone.
            if unsafe { WriteFile(pipe, Some(out.as_bytes()), Some(&mut written), None) }.is_err() {
                return;
            }
        }
    }
}

fn refresh_cpu(last_cpu: &mut std::time::Duration, last_t: &mut Instant) {
    let (now, cpu) = (Instant::now(), process_cpu());
    let dt = now.duration_since(*last_t).as_secs_f64();
    if dt >= 0.05 {
        let pct = 100.0 * cpu.saturating_sub(*last_cpu).as_secs_f64() / dt;
        TELEMETRY.cpu_pct.store(pct.round() as u64, Relaxed);
        (*last_cpu, *last_t) = (cpu, now);
    }
}

/// Maps one command line to a JSON reply, forwarding any state change to main.
fn handle(cmd: &str, commands: &Sender<UiCommand>) -> String {
    let send = |c| {
        let _ = commands.send(c);
    };
    match cmd.split_once(' ').unwrap_or((cmd, "")) {
        ("poll", _) => telemetry_json(),
        ("list", _) => devices_json(),
        ("nc", "on") => {
            send(UiCommand::Nc(true));
            telemetry_json()
        }
        ("nc", "off") => {
            send(UiCommand::Nc(false));
            telemetry_json()
        }
        ("mic", "default") => {
            send(UiCommand::SetMic(None));
            telemetry_json()
        }
        ("mic", id) if !id.is_empty() => {
            send(UiCommand::SetMic(Some(id.to_string())));
            telemetry_json()
        }
        ("spk", "default") => {
            send(UiCommand::SetSpk(None));
            telemetry_json()
        }
        ("spk", id) if !id.is_empty() => {
            send(UiCommand::SetSpk(Some(id.to_string())));
            telemetry_json()
        }
        // Strength maps to DeepFilterNet's attenuation limit, which the engine
        // doesn't expose yet: accept it, report it unsupported.
        ("strength", _) => telemetry_json(),
        // "transcript <call> <lines>": what the window shows (call id, line count).
        ("transcript", args) => {
            let mut n = args
                .split_whitespace()
                .map(|a| a.parse::<usize>().unwrap_or(0));
            let (call, from) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
            transcribe::json(call as u32, from)
        }
        ("startcall", _) => {
            transcribe::start_call();
            telemetry_json()
        }
        ("endcall", _) => {
            transcribe::end_call();
            telemetry_json()
        }
        ("voice", "record") => {
            send(UiCommand::RecordVoice);
            telemetry_json()
        }
        // "rec start <file.wav>", "rec stop": call recordings.
        ("rec", args) => {
            match args.split_once(' ').unwrap_or((args, "")) {
                ("start", path) if !path.trim().is_empty() => {
                    send(UiCommand::Record(record::Cmd::Start(path.trim().into())));
                }
                ("stop", _) => send(UiCommand::Record(record::Cmd::Stop)),
                _ => {}
            }
            telemetry_json()
        }
        _ => telemetry_json(),
    }
}

fn telemetry_json() -> String {
    let dfn = match TELEMETRY.dfn_state.load(Relaxed) {
        DFN_RUNNING => "running",
        DFN_FAILED => "failed",
        DFN_OVERLOADED => "overloaded",
        _ => "bypassed",
    };
    let (mic_name, headset_name) = TELEMETRY.names();
    format!(
        "{{\"type\":\"telemetry\",\"mic_connected\":{},\"spk_connected\":{},\
         \"spk_selectable\":{},\"nc\":{},\"dfn\":\"{}\",\"notice\":{},\"latency_ms\":{},\
         \"processing_ms\":{:.2},\"cpu_pct\":{},\"errors\":{},\"strength_supported\":false,\
         \"mic_name\":\"{}\",\"headset_name\":\"{}\",\"voice\":\"{}\"}}",
        TELEMETRY.mic_connected.load(Relaxed),
        TELEMETRY.spk_connected.load(Relaxed),
        TELEMETRY.spk_selectable.load(Relaxed),
        TELEMETRY.nc_on.load(Relaxed),
        dfn,
        TELEMETRY.notice.load(Relaxed),
        TELEMETRY.latency_us.load(Relaxed) / 1000,
        TELEMETRY.busy_us.load(Relaxed) as f64 / 1000.0,
        TELEMETRY.cpu_pct.load(Relaxed),
        TELEMETRY.errors(),
        escape(&mic_name),
        escape(&headset_name),
        voice_name(TELEMETRY.voice.load(Relaxed)),
    )
}

fn voice_name(state: u64) -> &'static str {
    match state {
        VOICE_NO_SAMPLE => "no_sample",
        VOICE_RECORDING => "recording",
        VOICE_READY => "ready",
        VOICE_SERVER_DOWN => "server_down",
        VOICE_TOO_QUIET => "too_quiet",
        VOICE_FAILED => "failed",
        _ => "off",
    }
}

fn devices_json() -> String {
    let one = |d: &devices::Device| {
        format!(
            "{{\"id\":\"{}\",\"name\":\"{}\",\"default\":{}}}",
            escape(&d.id),
            escape(&d.name),
            d.default_comms
        )
    };
    let list = devices::list().unwrap_or_default();
    let dir = |direction| {
        list.iter()
            .filter(|d| d.direction == direction)
            .map(one)
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"type\":\"devices\",\"inputs\":[{}],\"outputs\":[{}]}}",
        dir(Direction::Input),
        dir(Direction::Output)
    )
}

/// Minimal JSON string escaping for device names and IDs.
fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn handle_parses_commands_and_forwards_them() {
        let (tx, rx) = mpsc::channel();
        TELEMETRY.set_headset_name("Speakers (Detecta 201 USB)");
        let reply = handle("nc on", &tx);
        assert!(reply.contains("\"type\":\"telemetry\""));
        assert!(
            reply.contains("\"headset_name\":\"Speakers (Detecta 201 USB)\",")
                && reply.ends_with("\"voice\":\"off\"}"),
            "{reply}"
        );
        assert!(matches!(rx.try_recv(), Ok(UiCommand::Nc(true))));
        handle("nc off", &tx);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::Nc(false))));
        handle("voice record", &tx);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::RecordVoice)));
        handle("mic abc-123", &tx);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SetMic(Some(id))) if id == "abc-123"));
        handle("mic default", &tx);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SetMic(None))));
        handle("spk default", &tx);
        assert!(matches!(rx.try_recv(), Ok(UiCommand::SetSpk(None))));
        // poll, strength and the transcript don't go through main.
        handle("poll", &tx);
        handle("strength 50", &tx);
        assert!(handle("transcript 0 0", &tx).contains("\"type\":\"transcript\""));
        assert!(handle("transcript", &tx).contains("\"lines\":[]"));
        handle("startcall", &tx);
        handle("endcall", &tx);
        assert!(rx.try_recv().is_err());
        // Recording commands go to main (and on to the recorder).
        handle(
            r"rec start C:\Users\a b\Recordings\2026-10-14 10-32-05.wav",
            &tx,
        );
        assert!(
            matches!(rx.try_recv(), Ok(UiCommand::Record(record::Cmd::Start(path)))
            if path == std::path::Path::new(r"C:\Users\a b\Recordings\2026-10-14 10-32-05.wav"))
        );
        handle("rec stop", &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(UiCommand::Record(record::Cmd::Stop))
        ));
        handle("rec start", &tx); // no file: ignored
        handle("rec mark", &tx); // no longer a command: ignored
        assert!(rx.try_recv().is_err());
    }

    /// The real pipe end to end: a client connects, polls, and gets telemetry.
    /// (Fails if an engine is already running: the pipe name is per machine.)
    #[test]
    fn serve_answers_a_client_over_the_pipe() {
        use std::io::{BufRead, BufReader, Write};
        let (tx, _rx) = mpsc::channel();
        std::thread::spawn(move || serve(tx));
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(r"\\.\pipe\YalClearVoice")
        };
        let mut pipe = (0..250)
            .find_map(|_| {
                open().ok().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                })
            })
            .expect("pipe never came up");
        pipe.write_all(b"poll\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&pipe).read_line(&mut reply).unwrap();
        assert!(reply.starts_with("{\"type\":\"telemetry\""), "{reply}");
        assert!(reply.ends_with("}\n"));
    }

    #[test]
    fn escape_quotes_and_backslashes() {
        assert_eq!(escape("a\"b\\c"), "a\\\"b\\\\c");
    }
}
