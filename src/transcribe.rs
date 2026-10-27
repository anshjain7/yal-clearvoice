//! Live call transcription for call-center use. Both sides of a call are
//! recorded by WASAPI loopback, so the audio path itself is untouched:
//! - the customer: what Windows plays to the agent's headset;
//! - the agent: what the engine plays into VB-CABLE, i.e. their cleaned voice.
//!
//! Each side becomes 100 ms chunks of 16 kHz 16-bit mono which, during a call,
//! stream to Sarvam AI's real-time speech-to-text. Outside calls no audio
//! leaves the PC. The transcript lives only in memory and the next call
//! replaces it.
//!
//! The agent starts and stops transcription from the window (`start_call`,
//! `end_call`). As a safety net for a forgotten Stop, it also stops after a
//! long stretch with nobody speaking on either side.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::CloseFrame;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, IAudioCaptureClient, IAudioClient};

use crate::devices::{Direction, SelectError};
use crate::format::{AudioFormat, SampleFormat};
use crate::stream::{self, Stats, supervise};

/// Sarvam's format: 16 kHz mono, sent as 16-bit PCM.
pub(crate) const RATE: u32 = 16_000;
/// Samples per chunk: 100 ms.
pub(crate) const CHUNK: usize = RATE as usize / 10;
/// A chunk louder than this (RMS, dBFS) counts as speech.
const SPEECH_DBFS: f64 = -45.0;
/// Transcription left on with nobody speaking on either side stops after this
/// long, so a forgotten Stop doesn't stream silence to Sarvam all day.
const FORGOTTEN_AFTER: Duration = Duration::from_secs(10 * 60);
/// Chunks kept from just before Start, so a word already begun isn't cut: 1 s.
const PREROLL: usize = 10;
/// Most chunks held while Sarvam can't be reached: 10 s.
const BACKLOG: usize = 100;
const RETRY: Duration = Duration::from_secs(1);

/// `[transcription]` from settings.ini.
#[derive(Clone)]
pub struct Settings {
    pub api_key: String,
    /// Sarvam language code, or "auto" to detect it.
    pub language: String,
}

impl Settings {
    /// Reads settings.ini's text. None when it has no Sarvam API key.
    pub fn parse(ini: &str) -> Option<Settings> {
        let (mut section, mut api_key, mut language) = ("", "", "auto");
        for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim();
            } else if section == "transcription"
                && !line.starts_with(';')
                && let Some((k, v)) = line.split_once('=')
            {
                match k.trim() {
                    "api_key" => api_key = v.trim(),
                    "language" => language = v.trim(),
                    _ => {}
                }
            }
        }
        // It goes into the URL: anything but a plain code means "detect it".
        if language.is_empty()
            || !language
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            language = "auto";
        }
        (!api_key.is_empty()).then(|| Settings {
            api_key: api_key.to_string(),
            language: language.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Customer = 0,
    Agent = 1,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Customer => "customer",
            Side::Agent => "agent",
        }
    }
}

/// The current (or last) call.
struct Call {
    /// settings.ini has a Sarvam key, so transcription runs.
    enabled: bool,
    /// Counts calls; 0 = none yet.
    id: u32,
    active: bool,
    /// Finished sentences, in the order Sarvam finalized them, with when
    /// (seconds into the call).
    lines: Vec<(Side, String, f64)>,
    /// When the call started.
    started: Option<Instant>,
    /// A call's improved transcript, from the after-call pass (aftercall.rs).
    improved: Option<(u32, Vec<(Side, String)>)>,
    /// What each side is saying right now, not final yet.
    live: [String; 2],
    last_speech: Option<Instant>,
    /// The last problem reaching Sarvam, for the window; cleared on reconnect.
    error: String,
}

impl Call {
    const fn new() -> Self {
        Call {
            enabled: false,
            id: 0,
            active: false,
            lines: Vec::new(),
            started: None,
            improved: None,
            live: [String::new(), String::new()],
            last_speech: None,
            error: String::new(),
        }
    }

    /// Seconds into the call.
    fn secs(&self) -> f64 {
        self.started.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }

    /// Starts a new call (if transcription is set up and none is on),
    /// replacing the last one's transcript.
    fn start(&mut self, now: Instant) {
        if self.enabled && !self.active {
            self.id += 1;
            self.active = true;
            self.lines.clear();
            self.live = Default::default();
            self.last_speech = Some(now);
            self.started = Some(now);
        }
    }

    /// One chunk from either side heard at `now`: speech keeps the call going;
    /// a quiet of FORGOTTEN_AFTER ends it.
    fn heard(&mut self, speech: bool, now: Instant) {
        if !self.active {
            return;
        }
        if speech {
            self.last_speech = Some(now);
        } else if self
            .last_speech
            .is_some_and(|t| now.saturating_duration_since(t) >= FORGOTTEN_AFTER)
        {
            self.finish();
        }
    }

    /// Ends the call, keeping what each side was midway through saying.
    fn finish(&mut self) {
        self.active = false;
        for side in [Side::Customer, Side::Agent] {
            let text = std::mem::take(&mut self.live[side as usize]);
            if !text.is_empty() {
                let at = self.secs();
                self.lines.push((side, text, at));
            }
        }
    }

    /// Sarvam's text for `side` in call `id`, ignored once that call is over.
    fn result(&mut self, id: u32, side: Side, text: &str, is_final: bool) {
        if id != self.id || !self.active {
            return;
        }
        let live = &mut self.live[side as usize];
        live.clear();
        if !is_final {
            live.push_str(text);
        } else if !text.is_empty() {
            let at = self.secs();
            self.lines.push((side, text.to_string(), at));
        }
    }

    /// The window's view: lines from `from` on (all of them if the window
    /// still shows another call), what's being said now, and the state.
    fn json(&self, window_call: u32, from: usize) -> String {
        let from = if window_call == self.id {
            from.min(self.lines.len())
        } else {
            0
        };
        let lines: Vec<_> = self.lines[from..]
            .iter()
            .map(|(side, text, _)| serde_json::json!({ "who": side.name(), "text": text }))
            .collect();
        let improved = self.improved.as_ref().map(|(id, lines)| {
            let lines: Vec<_> = lines
                .iter()
                .map(|(side, text)| serde_json::json!({ "who": side.name(), "text": text }))
                .collect();
            serde_json::json!({ "call": id, "lines": lines })
        });
        serde_json::json!({
            "type": "transcript",
            "enabled": self.enabled,
            "call": self.id,
            "active": self.active,
            "total": self.lines.len(),
            "lines": lines,
            "live": { "customer": self.live[0], "agent": self.live[1] },
            "improved": improved,
            "error": self.error,
        })
        .to_string()
    }
}

static CALL: Mutex<Call> = Mutex::new(Call::new());

fn call() -> MutexGuard<'static, Call> {
    CALL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Marks transcription as configured (the window says so).
pub fn enable() {
    call().enabled = true;
}

/// The window's "Start transcription": starts a new call.
pub fn start_call() {
    call().start(Instant::now());
}

/// The window's "Stop transcription": ends the current call now.
pub fn end_call() {
    let mut c = call();
    if c.active {
        c.finish();
    }
}

/// True while a call is being transcribed.
pub(crate) fn active() -> bool {
    call().active
}

/// The last call once it's over: its number and the customer's lines (with
/// seconds into the call), for the after-call pass.
pub struct Finished {
    pub id: u32,
    pub customer: Vec<(f64, String)>,
}

/// The last call, if it's over.
pub(crate) fn finished() -> Option<Finished> {
    let c = call();
    (!c.active && c.id > 0).then(|| Finished {
        id: c.id,
        customer: c
            .lines
            .iter()
            .filter(|(side, ..)| *side == Side::Customer)
            .map(|(_, text, at)| (*at, text.clone()))
            .collect(),
    })
}

/// Call `id`'s improved transcript, for the window to show and summarize.
pub(crate) fn improve(id: u32, lines: Vec<(Side, String)>) {
    call().improved = Some((id, lines));
}

/// The transcript as JSON for the window; see `Call::json`.
pub fn json(window_call: u32, from: usize) -> String {
    call().json(window_call, from)
}

fn set_error(message: &str) {
    let mut c = call();
    if c.error != message {
        c.error = message.to_string();
    }
}

/// Records one side from the device `endpoint` names (followed as it
/// changes): an output device by loopback, or an input device. Runs until
/// `stop`, updating the call and handing chunks to `out`.
pub fn listen(
    direction: Direction,
    endpoint: fn() -> Option<String>,
    out: Sender<Vec<i16>>,
    stop: &AtomicBool,
) {
    supervise(
        stop,
        RETRY,
        || Tap::open(direction, &endpoint().ok_or(SelectError::NoDevice)?),
        |tap| {
            let result = tap.run(&endpoint, stop, |chunk| {
                call().heard(is_speech(&chunk), Instant::now());
                let _ = out.send(chunk);
            });
            (Stats::default(), result)
        },
    );
}

/// A 16 kHz mono recording of one device, polled, off the audio path: an
/// output device by loopback (what Windows plays to it), or an input device.
/// Used by the transcript and by call recordings (record.rs).
pub(crate) struct Tap {
    id: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl Tap {
    pub(crate) fn open(direction: Direction, id: &str) -> Result<Tap, SelectError> {
        let want = AudioFormat {
            sample_rate: RATE,
            channels: 1,
            sample_format: SampleFormat::F32,
            frames: CHUNK,
        };
        // 1 s of slack; it's polled every 20 ms.
        let (device, client) = stream::open_polled(direction, id, want, 10_000_000)?;
        Ok(Tap {
            id: device.id,
            // SAFETY: a COM call on the client open_polled just initialized.
            capture: unsafe { client.GetService()? },
            client,
        })
    }

    /// Records until `stop`, the device fails, or `endpoint` names another
    /// device (checked every 3 s), handing each 100 ms chunk to `chunk_done`.
    pub(crate) fn run(
        &mut self,
        endpoint: &dyn Fn() -> Option<String>,
        stop: &AtomicBool,
        mut chunk_done: impl FnMut(Vec<i16>),
    ) -> Result<(), SelectError> {
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut take = |chunk: &mut Vec<i16>| {
            chunk_done(std::mem::replace(chunk, Vec::with_capacity(CHUNK)));
        };
        let mut dry_since = Instant::now();
        let mut next_check = Instant::now() + Duration::from_secs(3);
        // SAFETY: COM calls on this stream's own interfaces. Each packet from
        // GetBuffer is read, then released, before the next one is fetched.
        unsafe {
            self.client.Start()?;
            while !stop.load(Relaxed) {
                thread::sleep(Duration::from_millis(20));
                while self.capture.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    // A silent packet's data is meaningless: read it as zeros.
                    let packet = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0)
                        .then(|| std::slice::from_raw_parts(data.cast::<f32>(), frames as usize));
                    for i in 0..frames as usize {
                        let s = packet.map_or(0.0, |p| p[i]);
                        chunk.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                        if chunk.len() == CHUNK {
                            take(&mut chunk);
                        }
                    }
                    self.capture.ReleaseBuffer(frames)?;
                    dry_since = Instant::now();
                }
                // While nothing plays Windows sends no packets. Count that as
                // silence, so the call timer and Sarvam's pause detection go on.
                if dry_since.elapsed() >= Duration::from_millis(100) {
                    chunk.resize(CHUNK, 0);
                    take(&mut chunk);
                    dry_since = Instant::now();
                }
                if Instant::now() >= next_check {
                    next_check = Instant::now() + Duration::from_secs(3);
                    if endpoint().is_some_and(|id| id != self.id) {
                        break; // the supervisor reopens on the new device
                    }
                }
            }
            self.client.Stop()?;
        }
        Ok(())
    }
}

pub(crate) fn is_speech(chunk: &[i16]) -> bool {
    let energy = chunk.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
    let rms = (energy / chunk.len().max(1) as f64).sqrt() / 32768.0;
    20.0 * rms.max(1e-9).log10() > SPEECH_DBFS
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Streams `side`'s chunks to Sarvam while a call is on, until `chunks` closes,
/// and files the text it sends back. Keeps the last second before a call and,
/// while Sarvam can't be reached, the last 10 s.
pub fn relay(side: Side, settings: &Settings, chunks: Receiver<Vec<i16>>) {
    let mut pending: VecDeque<Vec<i16>> = VecDeque::new();
    let mut socket: Option<(u32, Socket)> = None;
    let mut retry_at = Instant::now();
    loop {
        match chunks.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => pending.push_back(chunk),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        pending.extend(chunks.try_iter());
        let current = {
            let c = call();
            c.active.then_some(c.id)
        };
        // Between calls, or a new call: close the old connection.
        if socket.as_ref().is_some_and(|(id, _)| Some(*id) != current) {
            close(socket.take());
        }
        let Some(id) = current else {
            trim(&mut pending, PREROLL);
            continue;
        };
        if socket.is_none() {
            trim(&mut pending, BACKLOG);
            if Instant::now() < retry_at {
                continue;
            }
            match connect(settings) {
                Ok(s) => {
                    socket = Some((id, s));
                    set_error("");
                }
                Err(e) => {
                    set_error(&e);
                    retry_at = Instant::now() + RETRY * 5;
                    continue;
                }
            }
        }
        if let Some((_, s)) = socket.as_mut()
            && let Err(e) = exchange(s, &mut pending, id, side)
        {
            set_error(&e);
            socket = None;
            retry_at = Instant::now() + RETRY;
        }
    }
    close(socket);
}

fn trim(pending: &mut VecDeque<Vec<i16>>, keep: usize) {
    while pending.len() > keep {
        pending.pop_front();
    }
}

fn connect(settings: &Settings) -> Result<Socket, String> {
    let url = format!(
        "wss://api.sarvam.ai/speech-to-text-realtime/ws?language_code={}\
         &model=saaras:v3-realtime&sample_rate={RATE}&encoding=linear16",
        settings.language
    );
    let mut request = url.into_client_request().map_err(describe)?;
    let key = HeaderValue::from_str(&settings.api_key)
        .map_err(|_| "The Sarvam key in settings.ini isn't valid.".to_string())?;
    request.headers_mut().insert("api-subscription-key", key);
    let (mut s, _) = tungstenite::connect(request).map_err(describe)?;
    // From here on a read with nothing to read returns at once.
    let tcp = match s.get_mut() {
        MaybeTlsStream::NativeTls(t) => t.get_mut(),
        MaybeTlsStream::Plain(t) => t,
        _ => return Err("Unexpected connection type.".into()),
    };
    tcp.set_nonblocking(true).map_err(|e| describe(e.into()))?;
    Ok(s)
}

fn close(socket: Option<(u32, Socket)>) {
    if let Some((_, mut s)) = socket {
        let _ = s.close(None);
        let _ = s.flush();
    }
}

/// Sends the pending audio, then files every result Sarvam has ready.
fn exchange(
    s: &mut Socket,
    pending: &mut VecDeque<Vec<i16>>,
    id: u32,
    side: Side,
) -> Result<(), String> {
    while let Some(chunk) = pending.pop_front() {
        let bytes: Vec<u8> = chunk.iter().flat_map(|v| v.to_le_bytes()).collect();
        let msg = format!(
            "{{\"event\":\"audio_input\",\"audio\":\"{}\"}}",
            base64(&bytes)
        );
        // A send that would block stays queued and goes out with the next one.
        match s.send(Message::text(msg)) {
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
            r => r.map_err(describe)?,
        }
    }
    loop {
        match s.read() {
            Ok(Message::Text(t)) => match parse(&t) {
                Event::Partial(text) => call().result(id, side, &text, false),
                Event::Final(text) => call().result(id, side, &text, true),
                Event::Error(message) => set_error(&format!("Sarvam: {message}")),
                Event::Other => {}
            },
            Ok(Message::Close(frame)) => return Err(closed(frame)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(describe(e)),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Event {
    Partial(String),
    Final(String),
    Error(String),
    Other,
}

fn parse(message: &str) -> Event {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(message) else {
        return Event::Other;
    };
    let text = || v["text"].as_str().unwrap_or("").trim().to_string();
    match v["event"].as_str() {
        Some("transcript.partial") => Event::Partial(text()),
        Some("transcript.final") => Event::Final(text()),
        Some("error") => Event::Error(v["message"].as_str().unwrap_or("unknown error").into()),
        _ => Event::Other,
    }
}

/// A connection problem in words the agent can act on.
fn describe(e: tungstenite::Error) -> String {
    use tungstenite::Error as E;
    match e {
        E::Http(r) if matches!(r.status().as_u16(), 401 | 403) => {
            "Sarvam rejected the API key in settings.ini.".into()
        }
        E::Http(r) => format!("Sarvam refused the connection (HTTP {}).", r.status()),
        E::Io(_) | E::Tls(_) => "Can't reach Sarvam. Check the internet connection.".into(),
        E::ConnectionClosed | E::AlreadyClosed => "Sarvam closed the connection.".into(),
        e => format!("Sarvam connection failed ({e})."),
    }
}

fn closed(frame: Option<CloseFrame>) -> String {
    match frame {
        // 1003: Sarvam's code for a bad key, used-up credits or rate limits.
        Some(f) if u16::from(f.code) == 1003 => format!(
            "Sarvam stopped transcribing: {}. Check the API key and your Sarvam credits.",
            f.reason
        ),
        Some(f) if !f.reason.is_empty() => format!("Sarvam closed the connection: {}.", f.reason),
        _ => "Sarvam closed the connection.".into(),
    }
}

/// Standard base64 with padding (what Sarvam's `audio` field takes).
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{self, Direction};
    use std::sync::mpsc;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x01]), "//4AAQ==");
    }

    #[test]
    fn settings_read_only_the_transcription_section() {
        let ini = "\u{feff}; comment\n[summary]\napi_key = other\n\
                   [transcription]\n; api_key = commented\napi_key = sk_abc \nlanguage = hi-IN\n";
        let s = Settings::parse(ini).unwrap();
        assert_eq!(
            (s.api_key.as_str(), s.language.as_str()),
            ("sk_abc", "hi-IN")
        );
        // Anything that isn't a plain code can't reach the URL.
        let s = Settings::parse("[transcription]\napi_key=k\nlanguage=en&x=1\n").unwrap();
        assert_eq!(s.language, "auto");
        assert!(Settings::parse("[summary]\napi_key = other\n").is_none());
        assert!(Settings::parse("[transcription]\napi_key =\n").is_none());
    }

    /// A call with transcription set up, started at `t0`.
    fn started(t0: Instant) -> Call {
        let mut c = Call::new();
        c.enabled = true;
        c.start(t0);
        c
    }

    #[test]
    fn only_start_starts_a_call_and_a_forgotten_one_stops() {
        let t0 = Instant::now();
        let mut c = Call::new();
        // Not set up: Start does nothing.
        c.start(t0);
        assert!(!c.active);
        c.enabled = true;
        // Speech alone never starts a call.
        c.heard(true, t0);
        assert!(!c.active);
        c.start(t0);
        assert!(c.active && c.id == 1);
        c.result(1, Side::Customer, "Hello", true);
        // A second Start while on changes nothing.
        c.start(t0);
        assert_eq!((c.id, c.lines.len()), (1, 1));
        // Speech keeps it on; only a long quiet after it stops it.
        let spoke = t0 + Duration::from_secs(60);
        c.heard(true, spoke);
        c.heard(false, spoke + FORGOTTEN_AFTER - Duration::from_secs(1));
        assert!(c.active);
        c.heard(false, spoke + FORGOTTEN_AFTER);
        assert!(!c.active);
        // The next Start begins an empty new call.
        c.start(t0);
        assert!(c.active && c.id == 2 && c.lines.is_empty());
    }

    #[test]
    fn a_finished_call_gives_its_customer_lines_and_takes_an_improved_transcript() {
        let mut c = started(Instant::now());
        c.result(1, Side::Agent, "Hello, YAL support.", true);
        c.result(1, Side::Customer, "My order is late.", true);
        let json = |c: &Call| serde_json::from_str::<serde_json::Value>(&c.json(1, 0)).unwrap();
        assert!(json(&c)["improved"].is_null());
        c.finish();
        c.improved = Some((1, vec![(Side::Agent, "Hello, YAL support here.".into())]));
        let j = json(&c);
        assert_eq!(j["improved"]["call"], 1);
        assert_eq!(j["improved"]["lines"][0]["who"], "agent");
        assert_eq!(
            j["improved"]["lines"][0]["text"],
            "Hello, YAL support here."
        );
    }

    #[test]
    fn results_fill_lines_and_ending_keeps_unfinished_speech() {
        let mut c = started(Instant::now());
        c.result(1, Side::Customer, "I was charged", false);
        c.result(1, Side::Customer, "I was charged twice.", true);
        c.result(1, Side::Agent, "Let me check", false);
        assert_eq!(c.live, [String::new(), "Let me check".to_string()]);
        // A late result from another call is dropped.
        c.result(7, Side::Customer, "stale", true);
        c.finish();
        let lines: Vec<_> = c.lines.iter().map(|(s, t, _)| (*s, t.as_str())).collect();
        assert_eq!(
            lines,
            [
                (Side::Customer, "I was charged twice."),
                (Side::Agent, "Let me check")
            ]
        );
        // Each line has when it was finalized, in order.
        assert!(c.lines[0].2 <= c.lines[1].2 && c.lines[1].2 < 5.0);
        // And once it's over, nothing more is added.
        c.result(1, Side::Agent, "after", true);
        assert_eq!(c.lines.len(), 2);
    }

    #[test]
    fn json_sends_only_new_lines_unless_the_call_changed() {
        let mut c = started(Instant::now());
        c.result(1, Side::Customer, "One \"quoted\"", true);
        c.result(1, Side::Agent, "दो", true);
        let v: serde_json::Value = serde_json::from_str(&c.json(1, 1)).unwrap();
        assert_eq!(v["type"], "transcript");
        assert_eq!(
            (v["call"].as_u64(), v["total"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(v["lines"].as_array().unwrap().len(), 1);
        assert_eq!(v["lines"][0]["text"], "दो");
        // The window still shows call 0: it gets everything.
        let v: serde_json::Value = serde_json::from_str(&c.json(0, 1)).unwrap();
        assert_eq!(v["lines"][0]["text"], "One \"quoted\"");
        assert!(!c.json(1, 99).contains('\n'));
    }

    #[test]
    fn parses_sarvam_events() {
        assert_eq!(
            parse(
                r#"{"event":"transcript.final","utterance_idx":0,"text":" Hi. ","language":"en-IN"}"#
            ),
            Event::Final("Hi.".into())
        );
        assert_eq!(
            parse(r#"{"event":"transcript.partial","text":"Hi"}"#),
            Event::Partial("Hi".into())
        );
        assert_eq!(
            parse(r#"{"event":"error","code":"x","is_fatal":true,"message":"Bad key"}"#),
            Event::Error("Bad key".into())
        );
        assert_eq!(parse(r#"{"event":"vad.speech_start"}"#), Event::Other);
        assert_eq!(parse("not json"), Event::Other);
    }

    #[test]
    fn speech_threshold() {
        assert!(!is_speech(&[0; CHUNK]));
        assert!(!is_speech(&[100; CHUNK])); // about -50 dBFS: line noise
        assert!(is_speech(&[3000; CHUNK])); // about -21 dBFS: talking
    }

    /// Opens (never starts) a loopback stream on every output device: checks
    /// Windows accepts the 16 kHz mono format. Nothing is recorded.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn opens_loopback_on_every_output() {
        for d in devices::list().unwrap() {
            if d.direction == Direction::Output {
                let lb = Tap::open(Direction::Output, &d.id)
                    .unwrap_or_else(|e| panic!("{}: {e:?}", d.name));
                println!("loopback ok: {}", lb.id == d.id);
            }
        }
    }

    /// Sends a recording (16 kHz 16-bit mono WAV) to Sarvam as the customer,
    /// paced like the live audio, and checks the words come back.
    /// SARVAM_TEST_WAV=... CLEARVOICE_SETTINGS=...\settings.ini
    /// cargo test sarvam_transcribes_a_recording -- --ignored --nocapture
    #[test]
    #[ignore = "sends a recording to Sarvam's cloud"]
    fn sarvam_transcribes_a_recording() {
        let ini = std::fs::read_to_string(std::env::var("CLEARVOICE_SETTINGS").unwrap()).unwrap();
        let settings = Settings::parse(&ini).expect("settings.ini has no Sarvam key");
        let wav = std::fs::read(std::env::var("SARVAM_TEST_WAV").unwrap()).unwrap();
        let pcm: Vec<i16> = wav[44..]
            .as_chunks()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        enable();
        start_call(); // the agent clicks Start
        let (tx, rx) = mpsc::channel();
        let net = thread::spawn(move || relay(Side::Customer, &settings, rx));
        let silence = std::iter::repeat_n(vec![0i16; CHUNK], 30);
        let started = Instant::now();
        for (n, chunk) in pcm
            .chunks(CHUNK)
            .map(<[i16]>::to_vec)
            .chain(silence)
            .enumerate()
        {
            let mut chunk = chunk;
            chunk.resize(CHUNK, 0);
            call().heard(is_speech(&chunk), Instant::now());
            tx.send(chunk).unwrap();
            let due = started + Duration::from_millis(100) * (n as u32 + 1);
            thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while call().lines.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        drop(tx);
        net.join().unwrap();
        let c = call();
        println!(
            "lines: {:?}\nerror: {:?}\n{}",
            c.lines,
            c.error,
            c.json(0, 0)
        );
        assert!(c.lines.iter().any(
            |(side, text, _)| *side == Side::Customer && text.to_lowercase().contains("refund")
        ));
    }
}
