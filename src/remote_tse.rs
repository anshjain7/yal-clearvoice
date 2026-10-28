//! Live TSE on a GPU voice server (voice-server/voice_server.py, its stream
//! port): the feed for the live chain's sliding window (sliding.rs) with
//! WeSep there instead of on this PC.
//!
//! The microphone goes to the server as it's recorded (16 kHz, 20 ms at a
//! time). Every hop (0.32 s), once the lookahead after it (0.2 s) has
//! arrived, the server runs WeSep on [history (2.0 s) + hop + lookahead] and
//! sends back only the hop, crossfaded with the one before. They play a
//! fixed delay behind the microphone: the hop, the lookahead and a budget for
//! the network and the GPU (0.15 s), 0.67 s in all. The server being
//! unreachable plays the microphone as recorded, while this keeps trying it.
//!
//! `[voice]` in settings.ini: the server's `url` and `api_key` (as for
//! transcripts), and `stream = off` to keep WeSep on this PC instead;
//! `stream_port` (8091) and `stream_remix` (0), with sliding.rs's window
//! settings (`budget_ms` 150 here). The stream is plain TCP: keep the server
//! on the company network.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::sliding::{Event, Packets, Window};
use crate::voice;

/// After the server can't be reached, it's tried again this much later.
const RETRY: Duration = Duration::from_secs(3);
/// Longest a connection may take to open, and a write to go out.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Longest the server may take to accept the stream: the first stream for a
/// voice sample measures WeSep's gain on it first.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// The time allowed for the network and the GPU, unless `budget_ms` says.
const BUDGET_MS: u32 = 150;

/// The stream's settings, from `[voice]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub voice: voice::Settings,
    pub port: u16,
    pub window: Window,
    /// How much of the microphone to mix back in (0: WeSep's output alone).
    pub remix: f32,
}

impl Settings {
    /// Reads settings.ini's text. None without a `[voice]` url, or with
    /// `stream = off`.
    pub fn parse(ini: &str) -> Option<Settings> {
        let mut s = Settings {
            voice: voice::Settings::parse(ini)?,
            port: 8091,
            window: Window::parse(ini, BUDGET_MS),
            remix: 0.0,
        };
        let (mut section, mut on) = ("", true);
        for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim();
            } else if section == "voice"
                && !line.starts_with(';')
                && let Some((k, v)) = line.split_once('=')
            {
                let v = v.split(';').next().unwrap_or("").trim();
                match k.trim() {
                    "stream" => on = !matches!(v, "off" | "no" | "false" | "0"),
                    "stream_port" => s.port = v.parse().unwrap_or(s.port),
                    "stream_remix" => s.remix = v.parse().unwrap_or(0.0f32).clamp(0.0, 1.0),
                    _ => {}
                }
            }
        }
        on.then_some(s)
    }
}

/// The feed for the sliding window (sliding::Streaming) with WeSep on the
/// server: sends the microphone there and passes back the hops, connecting
/// (and reconnecting) as needed, until the stream is dropped.
pub fn feed(
    settings: Settings,
    sample: PathBuf,
) -> impl FnOnce(Packets, Sender<Event>) + Send + 'static {
    move |packets, events| network(&settings, &sample, &packets, &events)
}

/// One connection to the server's stream port.
struct Connection {
    tcp: TcpStream,
    /// False once the reader has hit an error, or the connection is closed.
    alive: Arc<AtomicBool>,
    /// The next microphone sample it expects (16 kHz since the stream started).
    expected: u64,
}

impl Connection {
    /// Sends the voice sample (if the server doesn't have it), opens the
    /// stream for microphone audio from `start` on, and has a thread pass
    /// the cleaned hops to `events`.
    fn open(
        settings: &Settings,
        sample: &Path,
        start: u64,
        events: &Sender<Event>,
    ) -> Result<Connection, String> {
        let id = voice::enroll(&settings.voice, sample)?;
        let host = voice::host(&settings.voice).ok_or("no server address")?;
        let addr = (host.as_str(), settings.port)
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("host not found")?;
        let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| e.to_string())?;
        tcp.set_nodelay(true).map_err(|e| e.to_string())?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|e| e.to_string())?;
        tcp.set_write_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        let hello = serde_json::json!({
            "key": settings.voice.api_key,
            "id": id,
            "context": settings.window.context(),
            "hop": settings.window.hop(),
            "lookahead": settings.window.lookahead(),
            "remix": settings.remix,
        });
        (&tcp)
            .write_all(format!("{hello}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(tcp.try_clone().map_err(|e| e.to_string())?);
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let reply: serde_json::Value =
            serde_json::from_str(&line).map_err(|_| "no answer from the stream port")?;
        if reply["ok"] != true {
            return Err(reply["error"].as_str().unwrap_or("refused").to_string());
        }
        // Hops come back as the server makes them: no read timeout.
        tcp.set_read_timeout(None).map_err(|e| e.to_string())?;
        let alive = Arc::new(AtomicBool::new(true));
        let (hop, events, flag) = (settings.window.hop(), events.clone(), alive.clone());
        thread::spawn(move || {
            let (mut buf, mut at) = (vec![0u8; hop * 2], start);
            while reader.read_exact(&mut buf).is_ok() {
                let pcm = buf
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&b| i16::from_le_bytes(b))
                    .collect();
                if events.send(Event::Piece(at, pcm, Duration::ZERO)).is_err() {
                    break;
                }
                at += hop as u64;
            }
            // Lost (not closed on purpose): say so.
            if flag.swap(false, Relaxed) {
                let _ = events.send(Event::Down);
            }
        });
        Ok(Connection {
            tcp,
            alive,
            expected: start,
        })
    }

    fn close(self) {
        self.alive.store(false, Relaxed);
        let _ = self.tcp.shutdown(Shutdown::Both);
    }
}

/// The network thread: sends the microphone from `packets` to the server,
/// connecting (and reconnecting) as needed, until the stream is dropped.
fn network(settings: &Settings, sample: &Path, packets: &Packets, events: &Sender<Event>) {
    let mut conn: Option<Connection> = None;
    let mut retry_at = Instant::now();
    while let Ok(mut packet) = packets.recv() {
        // Lost, or the stream restarted: start a new connection.
        if conn
            .as_ref()
            .is_some_and(|c| !c.alive.load(Relaxed) || c.expected != packet.0)
            && let Some(c) = conn.take()
        {
            c.close();
        }
        if conn.is_none() {
            if Instant::now() < retry_at {
                continue;
            }
            // Connecting takes a while: start from the newest audio.
            if let Some(newest) = packets.try_iter().last() {
                packet = newest;
            }
            match Connection::open(settings, sample, packet.0, events) {
                Ok(c) => {
                    let _ = events.send(Event::Up);
                    conn = Some(c);
                }
                Err(_) => {
                    let _ = events.send(Event::Down);
                    retry_at = Instant::now() + RETRY;
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected above");
        let bytes: Vec<u8> = packet.1.iter().flat_map(|s| s.to_le_bytes()).collect();
        if (&c.tcp).write_all(&bytes).is_ok() {
            c.expected = packet.0 + packet.1.len() as u64;
        } else {
            if let Some(c) = conn.take() {
                c.close();
            }
            let _ = events.send(Event::Down);
            retry_at = Instant::now() + RETRY;
        }
    }
    if let Some(c) = conn {
        c.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::AudioFrame;
    use crate::sliding::{Status, Streaming};
    use std::net::TcpListener;

    #[test]
    fn parses_the_stream_settings() {
        let ini = "[voice]\nurl = http://10.0.0.5:8090\napi_key = k\nhop_ms = 250\n";
        let s = Settings::parse(ini).unwrap();
        let w = s.window;
        assert_eq!(
            (s.port, w.context_ms, w.hop_ms, w.lookahead_ms, w.budget_ms),
            (8091, 2000, 250, 200, 150)
        );
        assert_eq!(w.delay_frames(), 60);
        let s = Settings::parse("[voice]\nurl = http://a:1\n").unwrap();
        assert_eq!(s.window.delay_frames(), 67);
        assert!(Settings::parse("[voice]\nurl = http://a:1\nstream = off\n").is_none());
        assert!(Settings::parse("[voice]\n").is_none());
    }

    /// A stand-in server: /enroll over HTTP, and a stream port that sends
    /// each hop back unchanged once the lookahead after it has come in.
    fn fake_server() -> (u16, u16) {
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpListener::bind("127.0.0.1:0").unwrap();
        let ports = (
            http.local_addr().unwrap().port(),
            stream.local_addr().unwrap().port(),
        );
        thread::spawn(move || {
            for c in http.incoming().flatten() {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                r.read_exact(&mut body).unwrap();
                let reply = "{\"id\": \"abc\"}";
                let _ = (&c).write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            }
        });
        thread::spawn(move || {
            for c in stream.incoming().flatten() {
                thread::spawn(move || {
                    let mut r = BufReader::new(c.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let h: serde_json::Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(h["id"], "abc");
                    let (hop, lookahead) = (
                        h["hop"].as_u64().unwrap() as usize,
                        h["lookahead"].as_u64().unwrap() as usize,
                    );
                    (&c).write_all(b"{\"ok\": true}\n").unwrap();
                    let (mut audio, mut next, mut buf) = (Vec::new(), 0, [0u8; 640]);
                    while let Ok(n) = r.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        audio.extend_from_slice(&buf[..n]);
                        while audio.len() / 2 >= next + hop + lookahead {
                            if (&c).write_all(&audio[next * 2..(next + hop) * 2]).is_err() {
                                return;
                            }
                            next += hop;
                        }
                    }
                });
            }
        });
        ports
    }

    #[test]
    fn plays_the_server_s_hops_a_fixed_delay_behind() {
        let (http, port) = fake_server();
        let sample = std::env::temp_dir().join(format!("remote-tse-{}.wav", std::process::id()));
        std::fs::write(&sample, crate::record::wav_bytes(&[100; 16_000])).unwrap();
        let mut settings =
            Settings::parse(&format!("[voice]\nurl = http://127.0.0.1:{http}\n")).unwrap();
        settings.port = port;
        let delay = settings.window.delay_frames();
        let mut s = Streaming::start(delay, feed(settings, sample.clone()));
        // A slow ramp, in real time: each moment has its own level.
        let ramp = |k: usize| {
            let mut f = AudioFrame::new(1);
            f.seq = k as u64;
            for (i, v) in f.samples_mut().iter_mut().enumerate() {
                *v = (k * 480 + i) as f32 / 48_000.0 * 0.1;
            }
            f
        };
        let mean = |x: &[f32]| x.iter().sum::<f32>() / x.len() as f32;
        let started = Instant::now();
        let mut outs = Vec::new();
        for k in 0..500 {
            outs.push(s.next(&ramp(k)));
            let due = started + Duration::from_millis(10 * (k as u64 + 1));
            thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        // Once going: on time, and the microphone from `delay` frames before
        // (the resampling filters' 2 ms aside).
        for (k, (out, mic, status)) in outs.iter().enumerate().skip(delay + 100) {
            assert_eq!(*status, Status::On, "frame {k}");
            assert_eq!(mic.samples(), ramp(k - delay).samples());
            let off = mean(out.samples()) - mean(mic.samples());
            assert!(off.abs() < 0.0005, "frame {k}: {off}");
        }
        let _ = std::fs::remove_file(sample);
    }

    #[test]
    fn plays_the_microphone_while_the_server_is_unreachable() {
        // Nothing listens on this port.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut settings =
            Settings::parse(&format!("[voice]\nurl = http://127.0.0.1:{port}\n")).unwrap();
        settings.port = port;
        let delay = settings.window.delay_frames();
        let mut s = Streaming::start(delay, feed(settings, PathBuf::from("no such sample.wav")));
        let level = |k: usize, v: f32| {
            let mut f = AudioFrame::new(1);
            f.seq = k as u64;
            f.samples_mut().fill(v);
            f
        };
        for k in 0..delay + 30 {
            let (out, mic, status) = s.next(&level(k, 0.2));
            assert_eq!(out.samples(), mic.samples());
            if k > 20 {
                assert_eq!(status, Status::Down);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
