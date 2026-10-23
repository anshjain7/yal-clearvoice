//! The agent's own voice, kept apart from voices nearby, for the transcript.
//!
//! In an office the background is mostly other agents talking, which noise
//! removal keeps (it's speech) and Sarvam then writes down as the agent. So
//! the agent's side of a transcribed call goes through target speaker
//! extraction first (WeSep, as the team's run_tse.sh: the agent's voice sample
//! as the enrollment, part of the original mixed back in): on this PC
//! (tse.rs) when the WeSep files ship with the engine, or on a `[voice]`
//! server from settings.ini. It works on 2 s at a time with 0.5 s before it
//! for context, so the agent's lines reach the transcript a few seconds
//! later. Only the transcript is affected; the call itself still gets the
//! voice straight from noise removal.
//!
//! On this PC it stays out of the call's way: it runs on its own thread below
//! normal priority (noise removal and the audio threads run at Pro Audio
//! priority above it), cleans only blocks with speech in them, and when the
//! PC can't keep up it lets blocks through uncleaned until it has caught up.
//! Whenever extraction isn't possible, the original audio is transcribed.
//!
//! The voice sample is 20 s of the agent speaking, recorded once from the
//! cleaned voice (the same sound extraction later works on) and kept in
//! `%LOCALAPPDATA%\YAL ClearVoice\Voice sample.wav`.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use crate::devices::Direction;
use crate::ipc::{self, TELEMETRY};
use crate::record;
use crate::transcribe::{self, CHUNK, RATE, Tap};
use crate::tse;

/// Audio cleaned per request: 2 s.
const BLOCK: usize = 2 * RATE as usize;
/// Audio sent before each block for context, and not returned: 0.5 s
/// (half the work of a full second, for the same result inside the block).
const CONTEXT: usize = RATE as usize / 2;
/// One block, in time.
const BLOCK_TIME: Duration = Duration::from_secs(2);
/// Blocks with less speech than this (100 ms chunks) go on uncleaned: after
/// noise removal the agent's side is nearly silent between words, so they
/// hold no voices worth removing, and skipping them about halves the work.
const MIN_SPEECH: usize = 3;
/// How far behind the call extraction may fall (a slow PC) before blocks go
/// on uncleaned until it has caught up, so the transcript never lags more.
const MAX_BEHIND: Duration = Duration::from_secs(4);
/// How long the voice sample is.
pub const SAMPLE_SECONDS: usize = 20;
/// Speech the sample must hold to be any use: 6 s.
const SAMPLE_MIN_SPEECH: usize = 60;
/// After the server fails, the original audio is transcribed for this long
/// before trying it again.
const OFF_AFTER_FAILURE: Duration = Duration::from_secs(30);
/// Longest a request may take before the server counts as down.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `[voice]` from settings.ini.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The extraction server, e.g. `http://10.0.0.5:8090`.
    pub url: String,
    /// Sent as `Authorization: Bearer` when set.
    pub api_key: String,
    /// How much of the original to mix back in (run_tse.sh's --remix).
    pub remix: f32,
}

impl Settings {
    /// Reads settings.ini's text. None when it has no `[voice]` url.
    pub fn parse(ini: &str) -> Option<Settings> {
        let (mut section, mut url, mut api_key, mut remix) = ("", "", "", 0.2f32);
        for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim();
            } else if section == "voice"
                && !line.starts_with(';')
                && let Some((k, v)) = line.split_once('=')
            {
                let v = v.split(';').next().unwrap_or("").trim();
                match k.trim() {
                    "url" => url = v,
                    "api_key" => api_key = v,
                    "remix" => remix = v.parse().unwrap_or(remix).clamp(0.0, 1.0),
                    _ => {}
                }
            }
        }
        Url::parse(url).ok().map(|_| Settings {
            url: url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            remix,
        })
    }
}

/// Where the agent's voice is extracted.
pub enum Backend {
    /// A `[voice]` server.
    Server(Settings),
    /// On this PC, with the WeSep files that ship with the engine.
    Local {
        files: tse::Files,
        remix: f32,
        /// Threads for the separator.
        threads: usize,
    },
}

/// `[voice]` remix (default 0.2) and threads (default 2, at most 8) for
/// extraction on this PC.
pub fn local_options(ini: &str) -> (f32, usize) {
    let (mut section, mut remix, mut threads) = ("", 0.2f32, 2usize);
    for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
        } else if section == "voice"
            && !line.starts_with(';')
            && let Some((k, v)) = line.split_once('=')
        {
            let v = v.split(';').next().unwrap_or("").trim();
            match k.trim() {
                "remix" => remix = v.parse().unwrap_or(remix).clamp(0.0, 1.0),
                "threads" => threads = v.parse().unwrap_or(threads).clamp(1, 8),
                _ => {}
            }
        }
    }
    (remix, threads)
}

/// Where the agent's voice sample is kept.
pub fn sample_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from);
    base.join("YAL ClearVoice").join("Voice sample.wav")
}

/// What the window shows about the voice sample and the server.
pub fn set_state(state: u64) {
    TELEMETRY.voice.store(state, Relaxed);
}

/// The state at startup: off without a way to extract, else from whether a
/// sample exists.
pub fn initial_state(available: bool) -> u64 {
    match available {
        false => ipc::VOICE_OFF,
        true if sample_path().is_file() => ipc::VOICE_READY,
        true => ipc::VOICE_NO_SAMPLE,
    }
}

/// Records the voice sample: `SAMPLE_SECONDS` of the cleaned voice played
/// into `device` (VB-CABLE's input, by loopback). Kept only if it holds at
/// least 3 s of speech; otherwise any earlier sample stays.
pub fn record_sample(device: Option<String>, path: &Path) {
    set_state(ipc::VOICE_RECORDING);
    let result = (|| {
        let id = device.ok_or("VB-CABLE wasn't found")?;
        let mut tap = Tap::open(Direction::Output, &id).map_err(|e| format!("{e:?}"))?;
        let (stop, mut samples, mut speech) = (AtomicBool::new(false), Vec::new(), 0);
        tap.run(&|| None::<String>, &stop, |chunk| {
            speech += usize::from(transcribe::is_speech(&chunk));
            samples.extend(chunk);
            if samples.len() >= SAMPLE_SECONDS * RATE as usize {
                stop.store(true, Relaxed);
            }
        })
        .map_err(|e| format!("{e:?}"))?;
        if speech < SAMPLE_MIN_SPEECH {
            return Err(String::from("too little speech"));
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        record::write_wav(path, &samples).map_err(|e| e.to_string())
    })();
    set_state(match result {
        Ok(()) => ipc::VOICE_READY,
        Err(e) if e == "too little speech" => ipc::VOICE_TOO_QUIET,
        Err(_) => ipc::VOICE_FAILED,
    });
}

/// Sits between the agent side's listener and its Sarvam relay: while a call
/// is being transcribed and there's a voice sample, cleans the audio in 2 s
/// blocks and passes on the result; otherwise, or when extraction fails,
/// passes the audio on as it is. Runs until `input` closes.
pub fn clean(backend: &Backend, input: Receiver<Vec<i16>>, out: Sender<Vec<i16>>) {
    below_normal_priority();
    let mut engine = match backend {
        Backend::Server(settings) => Engine::Server(Client::new(settings.clone(), sample_path())),
        Backend::Local {
            files,
            remix,
            threads,
        } => Engine::Local(Box::new(Local::new(files, *remix, *threads, sample_path()))),
    };
    let mut blocks = Blocks::default();
    let mut off_until: Option<Instant> = None;
    // How far behind the call extraction has fallen.
    let mut behind = Duration::ZERO;
    for chunk in input {
        let on = transcribe::active()
            && engine.sample().is_file()
            && off_until.is_none_or(|t| Instant::now() >= t);
        if !on {
            // Audio held for a block that won't be cleaned goes on as it is.
            for c in blocks.flush().chunks(CHUNK).chain([chunk.as_slice()]) {
                let _ = out.send(c.to_vec());
            }
            blocks.remember(&chunk);
            continue;
        }
        let Some((context, block)) = blocks.push(&chunk) else {
            continue;
        };
        let speech = block
            .chunks(CHUNK)
            .filter(|c| transcribe::is_speech(c))
            .count();
        let cleaned = if speech < MIN_SPEECH || behind > MAX_BEHIND {
            behind = behind.saturating_sub(BLOCK_TIME);
            block
        } else {
            let started = Instant::now();
            let result = engine.extract(&context, &block);
            behind = (behind + started.elapsed()).saturating_sub(BLOCK_TIME);
            match result {
                Ok(cleaned) => {
                    set_state(ipc::VOICE_READY);
                    off_until = None;
                    cleaned
                }
                Err(_) => {
                    set_state(engine.failed_state());
                    off_until = Some(Instant::now() + OFF_AFTER_FAILURE);
                    block
                }
            }
        };
        for c in cleaned.chunks(CHUNK) {
            let _ = out.send(c.to_vec());
        }
    }
}

/// Gathers audio into blocks, each with the second before it as context.
#[derive(Default)]
struct Blocks {
    /// The latest audio, up to `CONTEXT`, whether or not it was cleaned.
    recent: VecDeque<i16>,
    /// Audio for the block being gathered.
    block: Vec<i16>,
}

impl Blocks {
    /// Adds a chunk; once a block is full, returns its context and the block.
    fn push(&mut self, chunk: &[i16]) -> Option<(Vec<i16>, Vec<i16>)> {
        self.block.extend_from_slice(chunk);
        if self.block.len() < BLOCK {
            return None;
        }
        let context: Vec<i16> = self.recent.iter().copied().collect();
        let block = std::mem::take(&mut self.block);
        self.remember(&block);
        Some((context, block))
    }

    /// Notes audio that went on without being cleaned, as context.
    fn remember(&mut self, audio: &[i16]) {
        self.recent.extend(audio);
        while self.recent.len() > CONTEXT {
            self.recent.pop_front();
        }
    }

    /// Takes the audio gathered so far for a block.
    fn flush(&mut self) -> Vec<i16> {
        let block = std::mem::take(&mut self.block);
        self.remember(&block);
        block
    }
}

/// Whichever extracts: the server or this PC.
enum Engine {
    Server(Client),
    Local(Box<Local>),
}

impl Engine {
    fn sample(&self) -> &Path {
        match self {
            Engine::Server(c) => &c.sample,
            Engine::Local(l) => &l.sample,
        }
    }

    fn extract(&mut self, context: &[i16], block: &[i16]) -> Result<Vec<i16>, String> {
        match self {
            Engine::Server(c) => c.extract(context, block),
            Engine::Local(l) => l.extract(context, block),
        }
    }

    /// What the window shows when extraction fails.
    fn failed_state(&self) -> u64 {
        match self {
            Engine::Server(_) => ipc::VOICE_SERVER_DOWN,
            Engine::Local(_) => ipc::VOICE_UNAVAILABLE,
        }
    }
}

/// WeSep on this PC (tse.rs). Loaded when cleaning starts, not on the first
/// call, since loading takes a few seconds.
struct Local {
    files: tse::Files,
    remix: f32,
    threads: usize,
    sample: PathBuf,
    model: Option<tse::Extractor>,
    /// The voice sample's embedding, and the sample's time when computed.
    embedding: Option<(Vec<f32>, SystemTime)>,
}

impl Local {
    fn new(files: &tse::Files, remix: f32, threads: usize, sample: PathBuf) -> Local {
        let files = tse::Files {
            runtime: files.runtime.clone(),
            speaker: files.speaker.clone(),
            separator: files.separator.clone(),
        };
        let mut local = Local {
            files,
            remix,
            threads,
            sample,
            model: None,
            embedding: None,
        };
        if local.load().is_err() {
            set_state(ipc::VOICE_UNAVAILABLE);
        }
        local
    }

    fn load(&mut self) -> Result<&mut tse::Extractor, String> {
        if self.model.is_none() {
            self.model = Some(tse::Extractor::load(&self.files, self.threads)?);
        }
        Ok(self.model.as_mut().expect("just loaded"))
    }

    fn extract(&mut self, context: &[i16], block: &[i16]) -> Result<Vec<i16>, String> {
        let modified = fs::metadata(&self.sample)
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string())?;
        let stale = !matches!(&self.embedding, Some((_, at)) if *at == modified);
        let voice = if stale {
            Some(read_sample(&self.sample)?)
        } else {
            None
        };
        let model = self.load()?;
        if let Some(voice) = voice {
            let embedding = model.enroll(&voice)?;
            self.embedding = Some((embedding, modified));
        }
        let model = self.model.as_mut().expect("loaded above");
        let embedding = &self.embedding.as_ref().expect("enrolled above").0;
        let mix: Vec<f32> = context
            .iter()
            .chain(block)
            .map(|&s| f32::from(s) / 32768.0)
            .collect();
        let est = model.extract(embedding, &mix)?;
        let out = tse::remix(&est, &mix, self.remix);
        Ok(out[context.len()..]
            .iter()
            .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect())
    }
}

/// The voice sample as samples in -1..1 (record_sample's own 16 kHz WAV).
fn read_sample(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let data = bytes.get(44..).ok_or("voice sample is empty")?;
    Ok(data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
        .collect())
}

/// Lowers this thread below normal priority: extraction is never more urgent
/// than anything else on the PC, least of all the call's audio.
fn below_normal_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // SAFETY: a plain Win32 call on this thread's own pseudo-handle.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

/// Talks to the extraction server.
struct Client {
    settings: Settings,
    url: Url,
    sample: PathBuf,
    /// The server's id for the voice sample, and the sample's time when sent.
    enrolled: Option<(String, SystemTime)>,
}

impl Client {
    fn new(settings: Settings, sample: PathBuf) -> Client {
        let url = Url::parse(&settings.url).unwrap_or_default();
        Client {
            settings,
            url,
            sample,
            enrolled: None,
        }
    }

    /// The cleaned `block` (same length), or why the server didn't give it.
    fn extract(&mut self, context: &[i16], block: &[i16]) -> Result<Vec<i16>, String> {
        let body: Vec<u8> = context
            .iter()
            .chain(block)
            .flat_map(|s| s.to_le_bytes())
            .collect();
        // A server restarted since forgets the sample: send it again, once.
        for _ in 0..2 {
            let id = self.enroll()?;
            let path = format!(
                "/extract?id={id}&context={}&remix={}",
                context.len(),
                self.settings.remix
            );
            let (status, reply) = self.post(&path, "application/octet-stream", &body)?;
            match status {
                200 if reply.len() == block.len() * 2 => {
                    return Ok(reply
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&b| i16::from_le_bytes(b))
                        .collect());
                }
                200 => return Err(format!("server sent {} bytes", reply.len())),
                404 => self.enrolled = None,
                s => return Err(format!("server said {s}")),
            }
        }
        Err("server doesn't keep the voice sample".into())
    }

    /// The server's id for the current voice sample, sending it if needed.
    fn enroll(&mut self) -> Result<String, String> {
        let modified = fs::metadata(&self.sample)
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string())?;
        if let Some((id, at)) = &self.enrolled
            && *at == modified
        {
            return Ok(id.clone());
        }
        let wav = fs::read(&self.sample).map_err(|e| e.to_string())?;
        let (status, reply) = self.post("/enroll", "audio/wav", &wav)?;
        if status != 200 {
            return Err(format!("server said {status} to the voice sample"));
        }
        let json: serde_json::Value = serde_json::from_slice(&reply).map_err(|e| e.to_string())?;
        let id = json["id"]
            .as_str()
            .filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric()))
            .ok_or("no id from the server")?
            .to_string();
        self.enrolled = Some((id.clone(), modified));
        Ok(id)
    }

    fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
        let auth = if self.settings.api_key.is_empty() {
            String::new()
        } else {
            format!("Authorization: Bearer {}\r\n", self.settings.api_key)
        };
        let head = format!(
            "POST {}{path} HTTP/1.1\r\nHost: {}\r\nContent-Type: {content_type}\r\n\
             Content-Length: {}\r\n{auth}Connection: close\r\n\r\n",
            self.url.base,
            self.url.host,
            body.len()
        );
        post(&self.url, head.as_bytes(), body)
    }
}

/// An http:// or https:// address, split up.
#[derive(Debug, Default, Clone, PartialEq)]
struct Url {
    tls: bool,
    host: String,
    port: u16,
    /// Path before the server's own, without a trailing slash ("" for none).
    base: String,
}

impl Url {
    fn parse(url: &str) -> Result<Url, String> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("not an http(s) address".into());
        };
        let (authority, base) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| "bad port")?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err("no host".into());
        }
        let base = base.trim_end_matches('/');
        Ok(Url {
            tls,
            host: host.to_string(),
            port,
            base: if base.is_empty() {
                String::new()
            } else {
                format!("/{base}")
            },
        })
    }
}

/// Sends one HTTP request (`head` then `body`) and returns the status and body.
fn post(url: &Url, head: &[u8], body: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("host not found")?;
    let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    if url.tls {
        let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
        let mut s = tls.connect(&url.host, tcp).map_err(|e| e.to_string())?;
        exchange(&mut s, head, body, &mut reply)?;
    } else {
        let mut s = tcp;
        exchange(&mut s, head, body, &mut reply)?;
    }
    parse_response(&reply)
}

fn exchange(
    s: &mut impl ReadWrite,
    head: &[u8],
    body: &[u8],
    reply: &mut Vec<u8>,
) -> Result<(), String> {
    s.write_all(head).map_err(|e| e.to_string())?;
    s.write_all(body).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;
    s.read_to_end(reply).map_err(|e| e.to_string())?;
    Ok(())
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// An HTTP/1.1 response's status and body (Content-Length, else to the end).
fn parse_response(reply: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let end = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no reply from the server")?;
    let head = String::from_utf8_lossy(&reply[..end]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad reply from the server")?;
    let mut body = reply[end + 4..].to_vec();
    if let Some(len) = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse::<usize>().ok())?
    }) {
        body.truncate(len);
    }
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn local_options_have_defaults_and_limits() {
        assert_eq!(local_options(""), (0.2, 2));
        assert_eq!(
            local_options("[voice]\nremix = 0.3\nthreads = 4 ; faster PC\n"),
            (0.3, 4)
        );
        assert_eq!(
            local_options("[voice]\nthreads = 64\nremix = 7\n"),
            (1.0, 8)
        );
        assert_eq!(local_options("[noise]\nthreads = 4\n"), (0.2, 2));
    }

    #[test]
    fn settings_read_only_the_voice_section() {
        let ini = "[summary]\nurl = http://a/v1\n[voice]\nurl = http://10.0.0.5:8090/ ; tse\n\
                   api_key = k1\nremix = 0.3\n";
        assert_eq!(
            Settings::parse(ini),
            Some(Settings {
                url: "http://10.0.0.5:8090".into(),
                api_key: "k1".into(),
                remix: 0.3
            })
        );
        let s = Settings::parse("[voice]\nurl = https://tse.example.com/yal\n").unwrap();
        assert_eq!((s.remix, s.api_key.as_str()), (0.2, ""));
        assert_eq!(Settings::parse("[voice]\n; url = http://x\n"), None);
        assert_eq!(Settings::parse("[voice]\nurl = ftp://x\n"), None);
        assert_eq!(Settings::parse("[summary]\nurl = http://x\n"), None);
    }

    #[test]
    fn addresses_split_into_parts() {
        let u = Url::parse("http://10.0.0.5:8090").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.base.as_str()),
            (false, "10.0.0.5", 8090, "")
        );
        let u = Url::parse("https://tse.example.com/yal/").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.base.as_str()),
            (true, "tse.example.com", 443, "/yal")
        );
        assert!(Url::parse("http://:80").is_err());
        assert!(Url::parse("tse.example.com").is_err());
    }

    #[test]
    fn responses_are_read_by_content_length() {
        let r = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nX: y\r\n\r\nabcdef";
        assert_eq!(parse_response(r).unwrap(), (200, b"abc".to_vec()));
        let r = b"HTTP/1.0 404 Not Found\r\n\r\nnope";
        assert_eq!(parse_response(r).unwrap(), (404, b"nope".to_vec()));
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn blocks_carry_a_second_of_context() {
        let mut b = Blocks::default();
        let chunk = |v: i16| vec![v; CHUNK];
        // 2 s of chunks make the first block, with no context yet.
        for i in 0..19 {
            assert!(b.push(&chunk(i)).is_none());
        }
        let (context, block) = b.push(&chunk(19)).unwrap();
        assert_eq!((context.len(), block.len()), (0, BLOCK));
        // The next block's context is the last half second of the first.
        for i in 20..39 {
            assert!(b.push(&chunk(i)).is_none());
        }
        let (context, block) = b.push(&chunk(39)).unwrap();
        assert_eq!(
            (context.len(), context[0], *context.last().unwrap()),
            (CONTEXT, 15, 19)
        );
        assert_eq!((block[0], *block.last().unwrap()), (20, 39));
        // Audio that went on uncleaned still counts as context.
        b.push(&chunk(40));
        assert_eq!(b.flush(), chunk(40));
        b.remember(&chunk(41));
        assert_eq!(*b.recent.back().unwrap(), 41);
        assert_eq!(b.recent.len(), CONTEXT);
    }

    /// A stand-in server: answers each request with the next canned reply,
    /// handing back what it was sent.
    fn serve(replies: Vec<Vec<u8>>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut got = Vec::new();
            for reply in replies {
                let (mut s, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 65536];
                // Read the head, then as much body as it announces.
                loop {
                    let n = s.read(&mut buf).unwrap();
                    req.extend_from_slice(&buf[..n]);
                    if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..end]).to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                got.push(req);
                s.write_all(&reply).unwrap();
            }
            got
        });
        (url, handle)
    }

    #[test]
    fn extract_enrolls_once_and_again_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("clearvoice-voice-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let sample = dir.join("Voice sample.wav");
        record::write_wav(&sample, &[1, 2, 3]).unwrap();
        let block: Vec<i16> = (0..8).collect();
        let cleaned: Vec<u8> = block.iter().flat_map(|s| (s * 10).to_le_bytes()).collect();
        let ok = |body: &[u8]| {
            let mut r =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            r.extend_from_slice(body);
            r
        };
        let (url, server) = serve(vec![
            ok(br#"{"id":"abc123"}"#),
            ok(&cleaned),
            ok(&cleaned),
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
            ok(br#"{"id":"def456"}"#),
            ok(&cleaned),
        ]);
        let settings = Settings {
            url,
            api_key: "k".into(),
            remix: 0.2,
        };
        let mut client = Client::new(settings, sample);
        let want: Vec<i16> = block.iter().map(|s| s * 10).collect();
        assert_eq!(client.extract(&[9, 9], &block).unwrap(), want);
        assert_eq!(client.extract(&[9, 9], &block).unwrap(), want); // no second enroll
        assert_eq!(client.extract(&[9, 9], &block).unwrap(), want); // 404: enrolls again
        let got = server.join().unwrap();
        let text = |r: &Vec<u8>| String::from_utf8_lossy(r).to_string();
        assert!(text(&got[0]).starts_with("POST /enroll HTTP/1.1"));
        assert!(text(&got[0]).contains("Authorization: Bearer k\r\n"));
        assert!(text(&got[1]).starts_with("POST /extract?id=abc123&context=2&remix=0.2 HTTP/1.1"));
        assert!(got[1].ends_with(&[9, 0, 9, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0]));
        assert!(text(&got[4]).starts_with("POST /enroll"));
        assert!(text(&got[5]).contains("id=def456"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// WeSep on this PC through `clean()`, as during a transcribed call: a
    /// recording (repeated to 30 s, real-time paced, so the lag limit and the
    /// speech-only rule are at work) goes in as 100 ms chunks, and what comes
    /// out is checked and written. The voice sample must already be at
    /// `sample_path()`. Set YAL_TSE_TEST to "<dir with onnxruntime.dll and
    /// wesep\\>|<noisy recording.wav>|<output.wav>".
    /// cargo test --release cleans_on_this_pc -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep files and a voice sample"]
    fn cleans_on_this_pc() {
        let spec = std::env::var("YAL_TSE_TEST").expect("set YAL_TSE_TEST");
        let [dir, input, output] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_TSE_TEST needs 3 parts");
        };
        assert!(
            sample_path().is_file(),
            "no voice sample at {}",
            sample_path().display()
        );
        let b = fs::read(input).unwrap();
        let one: Vec<i16> = b[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c))
            .collect();
        let audio: Vec<i16> = one
            .iter()
            .copied()
            .cycle()
            .take(30 * RATE as usize)
            .collect();
        let files = tse::Files::find(Path::new(dir)).expect("WeSep files");
        let backend = Backend::Local {
            files,
            remix: 0.2,
            threads: 2,
        };
        transcribe::enable();
        transcribe::start_call();
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let t0 = Instant::now();
        let cleaner = std::thread::spawn(move || clean(&backend, rx, out_tx));
        std::thread::sleep(Duration::from_secs(8)); // the model loads first
        let started = Instant::now();
        for (i, c) in audio.chunks(CHUNK).enumerate() {
            let due = started + Duration::from_millis(100 * i as u64);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            tx.send(c.to_vec()).unwrap();
        }
        // Whatever is still waiting when the call ends goes on as it is.
        std::thread::sleep(Duration::from_secs(3));
        transcribe::end_call();
        tx.send(vec![0; CHUNK]).unwrap();
        drop(tx);
        cleaner.join().unwrap();
        let got: Vec<i16> = out_rx.iter().flatten().collect();
        println!(
            "{:.1} s in, {:.1} s out, voice state {}, finished {:.1} s after the last chunk",
            audio.len() as f32 / RATE as f32,
            got.len() as f32 / RATE as f32,
            TELEMETRY.voice.load(Relaxed),
            started.elapsed().as_secs_f32() - 30.0 - 3.0
        );
        let _ = t0;
        assert!(got.len() >= audio.len(), "audio went missing");
        let changed = got
            .chunks(BLOCK)
            .zip(audio.chunks(BLOCK))
            .filter(|(a, b)| a != b)
            .count();
        println!("{changed} of {} blocks cleaned", audio.len() / BLOCK);
        assert!(changed > 0, "nothing was cleaned");
        record::write_wav(Path::new(output), &got[..audio.len()]).unwrap();
    }

    /// Against a real voice server (voice-server/voice_server.py): sends a
    /// recording through it in 2 s blocks, as during a call, and reports how
    /// long each took. Set YAL_VOICE_TEST to "<url>|<key>|<voice sample.wav>|
    /// <noisy recording.wav>|<output.wav>" (16 kHz mono).
    /// cargo test --release extracts_through_a_server -- --ignored --nocapture
    #[test]
    #[ignore = "needs a voice server"]
    fn extracts_through_a_server() {
        let spec = std::env::var("YAL_VOICE_TEST").expect("set YAL_VOICE_TEST");
        let [url, key, sample, input, output] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_VOICE_TEST needs 5 parts");
        };
        let read = |p: &str| -> Vec<i16> {
            let b = fs::read(p).unwrap();
            b[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| i16::from_le_bytes(c))
                .collect()
        };
        let audio = read(input);
        let settings = Settings {
            url: url.into(),
            api_key: key.into(),
            remix: 0.2,
        };
        let mut client = Client::new(settings, PathBuf::from(sample));
        let (mut blocks, mut cleaned) = (Blocks::default(), Vec::new());
        for chunk in audio.chunks(CHUNK) {
            if let Some((context, block)) = blocks.push(chunk) {
                let t = Instant::now();
                let out = client.extract(&context, &block).unwrap();
                println!(
                    "block of {:.1} s (+{:.1} s context): {} ms",
                    block.len() as f32 / RATE as f32,
                    context.len() as f32 / RATE as f32,
                    t.elapsed().as_millis()
                );
                assert_eq!(out.len(), block.len());
                cleaned.extend(out);
            }
        }
        record::write_wav(Path::new(output), &cleaned).unwrap();
        println!(
            "wrote {output}: {:.1} s",
            cleaned.len() as f32 / RATE as f32
        );
    }

    #[test]
    fn extract_fails_cleanly_with_no_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener); // nothing listens there now
        let dir =
            std::env::temp_dir().join(format!("clearvoice-voice-down-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let sample = dir.join("Voice sample.wav");
        record::write_wav(&sample, &[1]).unwrap();
        let mut client = Client::new(
            Settings {
                url,
                api_key: String::new(),
                remix: 0.2,
            },
            sample,
        );
        assert!(client.extract(&[], &[1, 2, 3]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
