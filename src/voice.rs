//! The agent's own voice, kept apart from voices nearby.
//!
//! In an office the background is mostly other agents talking, which noise
//! removal keeps (it's speech). Target speaker extraction (TSE: WeSep, with
//! the agent's voice sample as the enrollment) keeps only the agent.
//!
//! On this PC the agent's voice goes through one live chain (denoise.rs,
//! stages.rs): microphone, TSE, then speech enhancement, out to VB-CABLE, the
//! customer and the transcript. WeSep's public model can't be that live TSE:
//! it looks at audio after each moment too, so it would hold the voice back
//! by seconds. Its files (tse.rs) stay for work that can wait, such as a
//! whole recording after the call; `clean` here runs only for a `[voice]`
//! server, which cleans the agent's side for the transcript during the call.
//!
//! The voice sample is 20 s of the agent speaking, recorded once from the
//! cleaned voice (the same sound extraction later works on) and kept in
//! `%LOCALAPPDATA%\YAL ClearVoice\Voice sample.wav`.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use crate::audiofile;
use crate::devices::Direction;
use crate::enhance::Enhancer;
use crate::ipc::{self, TELEMETRY};
use crate::record;
use crate::transcribe::{self, CHUNK, RATE, Tap};
use crate::tse;

/// Audio cleaned at a time: 2 s, one block after another.
const BLOCK: usize = 2 * RATE as usize;
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
    /// On this PC, with the WeSep files that ship with the engine, on the raw
    /// microphone, then speech enhancement.
    Local {
        files: tse::Files,
        remix: f32,
        /// Threads for the separator.
        threads: usize,
        /// DeepFilterNet3 for speech enhancement, and its attenuation limit.
        enhance: (PathBuf, f32),
    },
}

/// `[voice]` remix (default 0: run_tse.sh without --remix) and threads
/// (default 2, at most 8) for extraction on this PC.
pub fn local_options(ini: &str) -> (f32, usize) {
    let (mut section, mut remix, mut threads) = ("", 0.0f32, 2usize);
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

/// Makes the voice sample from an audio file the agent picked instead of
/// recording one: `SAMPLE_SECONDS` of it from the first speech. Kept only if
/// that holds enough speech; otherwise any earlier sample stays.
pub fn import_sample(file: &Path, path: &Path) {
    set_state(ipc::VOICE_IMPORTING);
    let result = (|| {
        let audio = audiofile::read(file).map_err(|_| ipc::VOICE_BAD_FILE)?;
        let start = audio.chunks(CHUNK).position(transcribe::is_speech);
        let start = start.ok_or(ipc::VOICE_SHORT_FILE)? * CHUNK;
        let sample = &audio[start..audio.len().min(start + SAMPLE_SECONDS * RATE as usize)];
        if sample
            .chunks(CHUNK)
            .filter(|c| transcribe::is_speech(c))
            .count()
            < SAMPLE_MIN_SPEECH
        {
            return Err(ipc::VOICE_SHORT_FILE);
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|_| ipc::VOICE_FAILED)?;
        }
        record::write_wav(path, sample).map_err(|_| ipc::VOICE_FAILED)
    })();
    set_state(result.map_or_else(|e| e, |()| ipc::VOICE_READY));
}

/// Sits between the agent side's listener and its Sarvam relay: while a call
/// is being transcribed and there's a voice sample, extracts the agent's
/// voice from every 2 s block and passes on the result; otherwise, or when
/// extraction fails, passes the audio on without it. On this PC everything
/// passed on during a call then goes through speech enhancement. Runs until
/// `input` closes.
pub fn clean(backend: &Backend, input: Receiver<Vec<i16>>, out: Sender<Vec<i16>>) {
    below_normal_priority();
    let (mut engine, mut enhancer) = match backend {
        Backend::Server(settings) => (
            Engine::Server(Client::new(settings.clone(), sample_path())),
            None,
        ),
        Backend::Local {
            files,
            remix,
            threads,
            enhance: (model, attenuation),
        } => (
            Engine::Local(Box::new(Local::new(files, *remix, *threads, sample_path()))),
            // Without it the agent's voice is transcribed unenhanced: noisier,
            // but nothing is lost.
            Enhancer::new(model, *attenuation).ok(),
        ),
    };
    let mut blocks = Blocks::default();
    let mut off_until: Option<Instant> = None;
    for chunk in input {
        let calling = transcribe::active();
        let on =
            calling && engine.sample().is_file() && off_until.is_none_or(|t| Instant::now() >= t);
        if !on {
            // Audio held for a block that won't be cleaned goes on as it is.
            let mut audio = blocks.flush();
            audio.extend(&chunk);
            // Speech enhancement only during a call: between calls nothing is
            // transcribed, and it would keep a core busy for nothing.
            let enhance = enhancer.as_mut().filter(|_| calling);
            send(&audio, enhance, &out);
            continue;
        }
        let Some(block) = blocks.push(&chunk) else {
            continue;
        };
        let tse = match engine.extract(&block) {
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
        };
        send(&tse, enhancer.as_mut(), &out);
    }
    send(&blocks.flush(), enhancer.as_mut(), &out);
}

/// Hands the agent's voice on: `tse` (after TSE, or as it was when TSE
/// didn't run) through speech enhancement (`enhancer`, when there is one) to
/// the relay.
fn send(tse: &[i16], enhancer: Option<&mut Enhancer>, out: &Sender<Vec<i16>>) {
    let audio = match enhancer {
        Some(e) => e.process(tse),
        None => tse.to_vec(),
    };
    for c in audio.chunks(CHUNK) {
        let _ = out.send(c.to_vec());
    }
}

/// Gathers audio into 2 s blocks, one after another.
#[derive(Default)]
struct Blocks {
    /// Audio for the block being gathered.
    block: Vec<i16>,
}

impl Blocks {
    /// Adds a chunk; once a block is full, returns it.
    fn push(&mut self, chunk: &[i16]) -> Option<Vec<i16>> {
        self.block.extend_from_slice(chunk);
        (self.block.len() >= BLOCK).then(|| std::mem::take(&mut self.block))
    }

    /// Takes the audio gathered so far for a block.
    fn flush(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.block)
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

    /// The agent's voice extracted from `block` (same length).
    fn extract(&mut self, block: &[i16]) -> Result<Vec<i16>, String> {
        match self {
            Engine::Server(c) => c.extract(&[], block),
            Engine::Local(l) => l.extract(block),
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

    /// WeSep on `block`, with the voice sample as the enrollment.
    fn extract(&mut self, block: &[i16]) -> Result<Vec<i16>, String> {
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
        let mix: Vec<f32> = block.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let est = model.extract(embedding, &mix)?;
        Ok(tse::remix(&est, &mix, self.remix)
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
pub(crate) fn below_normal_priority() {
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
    fn upload_keeps_20_s_from_the_first_speech() {
        let dir = std::env::temp_dir().join(format!("clearvoice-upload-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (file, sample) = (dir.join("mine.wav"), dir.join("Voice sample.wav"));
        // 3 s of silence, then 30 s of "speech" (a loud tone).
        let mut audio = vec![0i16; 3 * RATE as usize];
        audio.extend((0..30 * RATE as usize).map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16));
        record::write_wav(&file, &audio).unwrap();
        import_sample(&file, &sample);
        assert_eq!(TELEMETRY.voice.load(Relaxed), ipc::VOICE_READY);
        let kept = (fs::read(&sample).unwrap().len() - 44) / 2;
        assert_eq!(kept, SAMPLE_SECONDS * RATE as usize);
        // Too little speech: the sample made above stays.
        record::write_wav(&file, &audio[..6 * RATE as usize]).unwrap();
        import_sample(&file, &sample);
        assert_eq!(TELEMETRY.voice.load(Relaxed), ipc::VOICE_SHORT_FILE);
        import_sample(&dir.join("not there.mp3"), &sample);
        assert_eq!(TELEMETRY.voice.load(Relaxed), ipc::VOICE_BAD_FILE);
        assert_eq!((fs::read(&sample).unwrap().len() - 44) / 2, kept);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_options_have_defaults_and_limits() {
        // No remix by default: run_tse.sh without --remix.
        assert_eq!(local_options(""), (0.0, 2));
        assert_eq!(
            local_options("[voice]\nremix = 0.3\nthreads = 4 ; faster PC\n"),
            (0.3, 4)
        );
        assert_eq!(
            local_options("[voice]\nthreads = 64\nremix = 7\n"),
            (1.0, 8)
        );
        assert_eq!(local_options("[noise]\nthreads = 4\n"), (0.0, 2));
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
    fn blocks_follow_one_another() {
        let mut b = Blocks::default();
        let chunk = |v: i16| vec![v; CHUNK];
        // 2 s of chunks make a block, then the next starts afresh.
        for i in 0..19 {
            assert!(b.push(&chunk(i)).is_none());
        }
        let block = b.push(&chunk(19)).unwrap();
        assert_eq!(
            (block.len(), block[0], *block.last().unwrap()),
            (BLOCK, 0, 19)
        );
        assert!(b.push(&chunk(20)).is_none());
        assert_eq!(b.flush(), chunk(20));
        assert!(b.flush().is_empty());
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

    /// WeSep then speech enhancement on this PC through `clean()`, as during
    /// a transcribed call: a recording (repeated to 30 s, real-time paced)
    /// goes in as 100 ms chunks, and what comes out is checked and written. The voice sample must already be at
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
            remix: 0.0,
            threads: 2,
            enhance: (dfn3(), crate::denoise::DEFAULT_ATTENUATION_DB),
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

    /// DeepFilterNet3 in the repository, for the speech enhancement step.
    fn dfn3() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("models/DeepFilterNet3_onnx.tar.gz")
    }

    /// The team's configuration on this PC on a recording, block by block
    /// through the same steps as `clean()` during a recorded call (no
    /// pacing): WeSep on every 2 s block, then speech enhancement, saved by
    /// the app's own `Dump`. The files land where a call recorded as
    /// `<recording.wav>` would put them: `Agent voice\<recording>\` beside it
    /// (input, audio_tse and audio_tse_se as M4A, and rtf.csv). Set
    /// YAL_CLEAN_TEST to "<dir with onnxruntime.dll and wesep\>|<voice
    /// sample.wav>|<noisy recording.wav>|<recording.wav>" (16 kHz mono WAV).
    /// cargo test --release extracts_then_enhances -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep files and test recordings"]
    fn extracts_then_enhances() {
        let spec = std::env::var("YAL_CLEAN_TEST").expect("set YAL_CLEAN_TEST");
        let [dir, sample, input, recording] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_CLEAN_TEST needs 4 parts");
        };
        let files = tse::Files::find(Path::new(dir)).expect("WeSep files");
        let mut local = Local::new(&files, 0.0, 2, PathBuf::from(sample));
        let attenuation = crate::denoise::DEFAULT_ATTENUATION_DB;
        let mut enhancer = Enhancer::new(&dfn3(), attenuation).unwrap();
        let b = fs::read(input).unwrap();
        let audio: Vec<i16> = b[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c))
            .collect();
        let recording = Some(PathBuf::from(recording));
        let (mut blocks, mut dump) = (Blocks::default(), crate::stages::Dump::new(true));
        let (out, relay) = std::sync::mpsc::channel();
        // As the transcript side does, with each stage saved as a recorded
        // call's would be.
        let mut step = |raw: &[i16], tse: &[i16], tse_took| {
            let started = Instant::now();
            let se = enhancer.process(tse);
            let se_took = Some(started.elapsed());
            dump.add(recording.clone(), raw, tse, tse_took, &se, se_took);
            send(&se, None, &out);
        };
        for chunk in audio.chunks(CHUNK) {
            if let Some(block) = blocks.push(chunk) {
                let started = Instant::now();
                let tse = local.extract(&block).unwrap();
                step(&block, &tse, Some(started.elapsed()));
            }
        }
        let rest = blocks.flush();
        step(&rest, &rest, None);
        drop(out);
        let sent: usize = relay.iter().map(|c| c.len()).sum();
        println!("{} samples in, {sent} on to the transcript", audio.len());
        dump.close().expect("a call's files").join().unwrap();
        let dir = record::agent_voice_dir(recording.as_ref().unwrap());
        println!("{}", fs::read_to_string(dir.join("rtf.csv")).unwrap());
        for name in crate::stages::STAGES {
            assert!(dir.join(format!("{name}.m4a")).is_file(), "{name}.m4a");
        }
        println!("written to {}", dir.display());
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
            if let Some(block) = blocks.push(chunk) {
                let t = Instant::now();
                let out = client.extract(&[], &block).unwrap();
                println!(
                    "block of {:.1} s: {} ms",
                    block.len() as f32 / RATE as f32,
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
