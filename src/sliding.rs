//! The live TSE step's sliding window (livetse.rs), the same with WeSep on
//! this PC's CPU and on a GPU voice server (remote_tse.rs):
//!
//!   |<------ left context (2.0 s) ------>|<- hop (0.32 s) ->|<- lookahead (0.2 s) ->|
//!   model input = the whole window; output = only the hop
//!
//! Every hop, once the lookahead after it has come in, WeSep runs on
//! [history + hop + lookahead] and only the hop is kept, its first 20 ms
//! crossfaded with what the window before made of them. WeSep's model is
//! bidirectional and worst at the very end of its input; the lookahead keeps
//! the hop away from it, and the history gives it the context it has
//! offline. The hops play out a fixed delay behind the microphone: hop +
//! lookahead + a budget for the compute (and the network), so the latency
//! is about hop + lookahead + compute time.
//!
//! A hop that isn't ready in time plays the microphone as recorded. When
//! WeSep can't keep up (on a laptop CPU, each 2.5 s window takes longer than
//! a hop), it goes to the newest hop that's ready and skips the ones whose
//! time has passed, so the delay stays fixed and only some hops are cleaned.
//!
//! `[voice]` in settings.ini: `context_ms` (2000), `hop_ms` (320),
//! `lookahead_ms` (200) and `budget_ms` (1800 on this PC, 150 on a server).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::enhance::Fir;
use crate::frame::AudioFrame;
use crate::tse;

/// 48 kHz to 16 kHz.
const DOWN: usize = 3;
const FRAME: usize = 480;
/// Microphone handed to the feed this much at a time: 20 ms at 16 kHz.
const PACKET: usize = 320;
/// Each hop's first 20 ms are crossfaded with the window before's version.
const FADE: usize = 320;
/// WeSep's level on the agent is measured on this much of the voice sample.
const GAIN_SAMPLE: usize = 6 * 16_000;

/// The window's shape, and the time allowed for the compute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub context_ms: u32,
    pub hop_ms: u32,
    pub lookahead_ms: u32,
    pub budget_ms: u32,
}

impl Window {
    /// From settings.ini's `[voice]`; `budget_ms` defaults to `budget`.
    pub fn parse(ini: &str, budget: u32) -> Window {
        let mut w = Window {
            context_ms: 2000,
            hop_ms: 320,
            lookahead_ms: 200,
            budget_ms: budget,
        };
        let mut section = "";
        for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim();
            } else if section == "voice"
                && !line.starts_with(';')
                && let Some((k, v)) = line.split_once('=')
            {
                let v = v.split(';').next().unwrap_or("").trim();
                let ms = |d: u32, lo: u32, hi: u32| v.parse().unwrap_or(d).clamp(lo, hi);
                match k.trim() {
                    "context_ms" => w.context_ms = ms(w.context_ms, 0, 10_000),
                    "hop_ms" => w.hop_ms = ms(w.hop_ms, 50, 2000),
                    "lookahead_ms" => w.lookahead_ms = ms(w.lookahead_ms, 0, 1000),
                    "budget_ms" => w.budget_ms = ms(w.budget_ms, 20, 5000),
                    _ => {}
                }
            }
        }
        w
    }

    /// 10 ms frames the voice plays behind the microphone: hop + lookahead
    /// + budget.
    pub fn delay_frames(&self) -> usize {
        ((self.hop_ms + self.lookahead_ms + self.budget_ms) as usize).div_ceil(10)
    }

    /// The parts, in 16 kHz samples.
    pub(crate) fn context(&self) -> usize {
        self.context_ms as usize * 16
    }

    pub(crate) fn hop(&self) -> usize {
        self.hop_ms as usize * 16
    }

    pub(crate) fn lookahead(&self) -> usize {
        self.lookahead_ms as usize * 16
    }
}

/// The microphone for a feed: where each packet starts (16 kHz samples
/// since the stream started) and its audio.
pub type Packets = Receiver<(u64, Vec<i16>)>;

/// What a feed tells the player.
pub enum Event {
    /// WeSep is ready (the model loaded, or the server connected).
    Up,
    /// It can't run (no voice sample, the model failed, the server down).
    Down,
    /// A cleaned hop: where it starts (16 kHz samples since the stream
    /// started), its audio, and the compute time (zero on a server).
    Piece(u64, Vec<i16>, Duration),
}

/// How the step stands, for the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    /// Running, and the hops are ready in time.
    On,
    /// A hop wasn't ready in time: the microphone played instead.
    Late,
    /// WeSep can't run, or the server can't be reached: the microphone plays.
    Down,
}

/// The player, on the audio thread: hands the microphone to a feed (WeSep
/// on this PC, or the server) and plays the hops back a fixed delay behind.
pub struct Streaming {
    packets: Sender<(u64, Vec<i16>)>,
    events: Receiver<Event>,
    down: Fir,
    up: Fir,
    packet: Vec<i16>,
    /// 16 kHz samples handed on, and microphone frames taken in.
    sent: u64,
    frames: u64,
    delay: usize,
    /// The microphone, waiting to be played out (the delay line).
    mics: VecDeque<AudioFrame>,
    /// Cleaned audio, 48 kHz, its first sample at `ready_at` (48 kHz samples
    /// since the stream started); NaN where a hop was skipped.
    ready: VecDeque<f32>,
    ready_at: u64,
    /// Where the next hop is expected (16 kHz), once one has come.
    next_piece: Option<u64>,
    up_now: bool,
    /// Hops cleaned since last asked, in 10 ms frames, and their compute.
    work: (u64, Duration),
}

impl Streaming {
    /// Starts the stream: `feed` runs on its own thread with the microphone
    /// and sends back the hops; they play `delay` frames behind.
    pub(crate) fn start(
        delay: usize,
        feed: impl FnOnce(Packets, Sender<Event>) + Send + 'static,
    ) -> Streaming {
        let (packets, packets_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        thread::spawn(move || feed(packets_rx, events_tx));
        let mut silent = AudioFrame::new(1);
        silent.captured = None;
        Streaming {
            packets,
            events,
            down: Fir::new(),
            up: Fir::new(),
            packet: Vec::with_capacity(PACKET),
            sent: 0,
            frames: 0,
            delay,
            mics: (0..delay).map(|_| silent).collect(),
            ready: VecDeque::new(),
            ready_at: 0,
            next_piece: None,
            up_now: false,
            work: (0, Duration::ZERO),
        }
    }

    /// Takes a microphone frame; returns the frame to play now (cleaned, the
    /// delay behind), the microphone frame it came from, and the status.
    pub fn next(&mut self, frame: &AudioFrame) -> (AudioFrame, AudioFrame, Status) {
        for e in self.events.try_iter().collect::<Vec<_>>() {
            match e {
                Event::Up => self.up_now = true,
                Event::Down => self.up_now = false,
                Event::Piece(start, pcm, took) => {
                    self.work.0 += (pcm.len() / 160) as u64;
                    self.work.1 += took;
                    self.add(start, &pcm);
                }
            }
        }
        self.mics.push_back(*frame);
        let low = self.down.filter(frame.samples(), DOWN);
        self.packet
            .extend(low.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16));
        if self.packet.len() >= PACKET {
            let pcm = std::mem::replace(&mut self.packet, Vec::with_capacity(PACKET));
            let len = pcm.len() as u64;
            let _ = self.packets.send((self.sent, pcm));
            self.sent += len;
        }
        let mic = self.mics.pop_front().unwrap_or(AudioFrame::new(1));
        let frame_no = self.frames;
        self.frames += 1;
        let mut out = mic;
        let mut late = false;
        if let Some(at) = (frame_no as usize)
            .checked_sub(self.delay)
            .map(|f| (f * FRAME) as u64)
        {
            // What's before this moment has had its time.
            let stale = at
                .saturating_sub(self.ready_at)
                .min(self.ready.len() as u64);
            self.ready.drain(..stale as usize);
            self.ready_at += stale;
            let ready = self.ready_at == at
                && self.ready.len() >= FRAME
                && self.ready.range(..FRAME).all(|v| !v.is_nan());
            if ready {
                for (o, r) in out.samples_mut().iter_mut().zip(self.ready.drain(..FRAME)) {
                    *o = r;
                }
                self.ready_at += FRAME as u64;
            } else {
                // Late, unless it's from before the first hop.
                late = self.next_piece.is_some() && at >= self.ready_at;
            }
        }
        out.seq = frame.seq;
        let status = match (self.up_now, late) {
            (false, _) => Status::Down,
            (true, true) => Status::Late,
            (true, false) => Status::On,
        };
        (out, mic, status)
    }

    /// Hops cleaned since the last call (10 ms frames) and their compute
    /// time: the step's real-time factor, for stages.rs's rtf.csv.
    pub fn take_work(&mut self) -> (u64, Duration) {
        std::mem::replace(&mut self.work, (0, Duration::ZERO))
    }

    /// A cleaned hop: back up to 48 kHz, after what came before. Hops skipped
    /// in between are marked (NaN) so the microphone plays there; one from
    /// before what's queued (a new connection) starts afresh.
    fn add(&mut self, start: u64, pcm: &[i16]) {
        let at = start * DOWN as u64;
        let end = self.ready_at + self.ready.len() as u64;
        if self.next_piece != Some(start) {
            self.up = Fir::new();
            if self.next_piece.is_none() || at < end {
                self.ready.clear();
                self.ready_at = at;
            } else {
                self.ready
                    .extend(std::iter::repeat_n(f32::NAN, (at - end) as usize));
            }
        }
        let mut stuffed = vec![0f32; pcm.len() * DOWN];
        for (i, &v) in pcm.iter().enumerate() {
            stuffed[i * DOWN] = f32::from(v) / 32768.0 * DOWN as f32;
        }
        self.ready.extend(self.up.filter(&stuffed, 1));
        self.next_piece = Some(start + pcm.len() as u64);
    }
}

/// The sliding window over the microphone from `packets`: every hop, once
/// its lookahead is in, `model` (a window at 16 kHz -> WeSep's output, the
/// same length) runs on [context + hop + lookahead] and the hop goes to
/// `events`. Behind, it goes straight to the newest hop that's ready.
pub(crate) fn slide(
    window: &Window,
    mut model: impl FnMut(&[f32]) -> Result<Vec<f32>, String>,
    packets: &Packets,
    events: &Sender<Event>,
) {
    let (context, hop, lookahead) = (window.context(), window.hop(), window.lookahead());
    let fade = FADE.min(lookahead);
    let mut audio: VecDeque<f32> = VecDeque::new();
    let mut base = 0u64; // the stream sample audio[0] is
    let mut next: Option<u64> = None; // where the next hop starts
    let mut tail: Option<Vec<f32>> = None; // the last window's take on its start
    let mut up = false;
    while let Ok(first) = packets.recv() {
        for (start, pcm) in std::iter::once(first).chain(packets.try_iter()) {
            if next.is_none() || base + audio.len() as u64 != start {
                // The first packet, or a gap: start afresh here.
                (audio, base, next, tail) = (VecDeque::new(), start, Some(start), None);
            }
            audio.extend(pcm.iter().map(|&s| f32::from(s) / 32768.0));
        }
        let end = base + audio.len() as u64;
        let Some(mut p) = next else { continue };
        if end < p + (hop + lookahead) as u64 {
            continue;
        }
        let newest = p + (end - p - (hop + lookahead) as u64) / hop as u64 * hop as u64;
        if newest != p {
            (p, tail) = (newest, None);
        }
        let from = p.saturating_sub(context as u64).max(base);
        let to = p + (hop + lookahead) as u64;
        let input: Vec<f32> = audio
            .range((from - base) as usize..(to - base) as usize)
            .copied()
            .collect();
        let started = Instant::now();
        match model(&input) {
            Ok(est) if est.len() >= input.len() => {
                if !up {
                    up = true;
                    let _ = events.send(Event::Up);
                }
                let at = (p - from) as usize;
                let mut piece = est[at..at + hop + fade].to_vec();
                if let Some(t) = &tail {
                    for (i, (v, old)) in piece.iter_mut().zip(t).enumerate() {
                        let w = (i as f32 + 0.5) / fade as f32;
                        *v = old * (1.0 - w) + *v * w;
                    }
                }
                tail = Some(piece[hop..].to_vec());
                let pcm = piece[..hop]
                    .iter()
                    .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                if events
                    .send(Event::Piece(p, pcm, started.elapsed()))
                    .is_err()
                {
                    return;
                }
            }
            _ => {
                if up || next == Some(base) {
                    up = false;
                    let _ = events.send(Event::Down);
                }
                tail = None;
            }
        }
        next = Some(p + hop as u64);
        let keep = (p + hop as u64).saturating_sub(context as u64);
        if keep > base {
            audio.drain(..((keep - base) as usize).min(audio.len()));
            base = keep;
        }
    }
}

/// The feed for WeSep on this PC's CPU: the sliding window with the
/// separator from `files` on `threads`, against the voice sample at
/// `sample`, `remix` of the microphone mixed back in.
pub fn local(
    files: tse::Files,
    threads: usize,
    sample: PathBuf,
    window: Window,
    remix: f32,
) -> impl FnOnce(Packets, Sender<Event>) + Send + 'static {
    move |packets, events| {
        let mut wesep = Wesep {
            files,
            threads,
            sample,
            model: None,
            voice: None,
        };
        // Load now, not on the first window.
        match wesep.clean(&[], remix) {
            Ok(_) => {
                let _ = events.send(Event::Up);
            }
            Err(_) => {
                let _ = events.send(Event::Down);
            }
        }
        slide(&window, |w| wesep.clean(w, remix), &packets, &events);
    }
}

/// WeSep on this PC, with the voice sample (enrolled again when it changes).
struct Wesep {
    files: tse::Files,
    threads: usize,
    sample: PathBuf,
    model: Option<tse::Extractor>,
    /// The voice sample's embedding, WeSep's gain on it, and its time.
    voice: Option<(Vec<f32>, f32, SystemTime)>,
}

impl Wesep {
    /// WeSep's output for `window` (16 kHz) at the agent's level (empty: just
    /// load and enroll).
    fn clean(&mut self, window: &[f32], remix: f32) -> Result<Vec<f32>, String> {
        if self.model.is_none() {
            self.model = Some(tse::Extractor::load(&self.files, self.threads)?);
        }
        let m = self.model.as_mut().expect("loaded above");
        let modified = std::fs::metadata(&self.sample)
            .and_then(|x| x.modified())
            .map_err(|_| "no voice sample".to_string())?;
        if self.voice.as_ref().is_none_or(|(.., at)| *at != modified) {
            let b = std::fs::read(&self.sample).map_err(|e| e.to_string())?;
            let voice: Vec<f32> = b
                .get(44..)
                .unwrap_or_default()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| f32::from(i16::from_le_bytes(c)) / 32768.0)
                .collect();
            let embedding = m.enroll(&voice)?;
            // WeSep's output is much quieter than its input, and can be
            // upside down. Its level and sign on the agent alone (the
            // sample), applied as a fixed gain, bring the agent back to their
            // own level while other voices stay down; scaling each window to
            // its input's loudness would bring them back up.
            let part = &voice[..voice.len().min(GAIN_SAMPLE)];
            let est = m.extract(&embedding, part)?;
            let rms =
                |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt();
            let dot: f32 = est.iter().zip(part).map(|(a, b)| a * b).sum();
            let gain = dot.signum() * rms(part) / rms(&est).max(1e-6);
            self.voice = Some((embedding, gain, modified));
        }
        if window.is_empty() {
            return Ok(Vec::new());
        }
        let (embedding, gain, _) = self.voice.as_ref().expect("enrolled above");
        let est = m.extract(embedding, window)?;
        Ok(est
            .iter()
            .zip(window)
            .map(|(e, x)| (1.0 - remix) * e * gain + remix * x)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(k: usize) -> AudioFrame {
        let mut f = AudioFrame::new(1);
        f.seq = k as u64;
        for (i, v) in f.samples_mut().iter_mut().enumerate() {
            *v = (k * FRAME + i) as f32 / 48_000.0 * 0.1;
        }
        f
    }

    fn mean(x: &[f32]) -> f32 {
        x.iter().sum::<f32>() / x.len() as f32
    }

    #[test]
    fn parses_the_window() {
        let w = Window::parse("[voice]\nhop_ms = 250\n", 1800);
        assert_eq!(
            (w.context_ms, w.hop_ms, w.lookahead_ms, w.budget_ms),
            (2000, 250, 200, 1800)
        );
        assert_eq!(w.delay_frames(), 225);
        assert_eq!(Window::parse("", 150).delay_frames(), 67);
    }

    /// Runs the player in real time over `frames` frames of a ramp, with
    /// `model` on the sliding window; returns each frame out and its status.
    fn play(
        window: Window,
        frames: usize,
        model: impl FnMut(&[f32]) -> Result<Vec<f32>, String> + Send + 'static,
    ) -> Vec<(AudioFrame, AudioFrame, Status)> {
        let mut s = Streaming::start(window.delay_frames(), move |packets, events| {
            let _ = events.send(Event::Up);
            slide(&window, model, &packets, &events);
        });
        let started = Instant::now();
        (0..frames)
            .map(|k| {
                let out = s.next(&ramp(k));
                let due = started + Duration::from_millis(10 * (k as u64 + 1));
                thread::sleep(due.saturating_duration_since(Instant::now()));
                out
            })
            .collect()
    }

    #[test]
    fn plays_each_hop_a_fixed_delay_behind() {
        // A "model" that halves its input: cleaned frames show at half level.
        let window = Window::parse("", 100);
        let delay = window.delay_frames();
        let outs = play(window, 400, |w| Ok(w.iter().map(|v| v / 2.0).collect()));
        for (k, (out, mic, status)) in outs.iter().enumerate().skip(delay + 60) {
            assert_eq!(*status, Status::On, "frame {k}");
            assert_eq!(mic.samples(), ramp(k - delay).samples());
            // The resampling filters' 2 ms aside.
            let off = mean(out.samples()) - mean(mic.samples()) / 2.0;
            assert!(off.abs() < 0.0005, "frame {k}: {off}");
        }
    }

    #[test]
    fn a_slow_model_cleans_some_hops_and_the_delay_stays() {
        // Each window takes 0.5 s, longer than a hop (0.32 s): WeSep skips to
        // the newest hop each time, and the rest play as recorded.
        let window = Window::parse("", 600);
        let delay = window.delay_frames();
        let outs = play(window, 700, |w| {
            thread::sleep(Duration::from_millis(500));
            Ok(w.iter().map(|v| v / 2.0).collect())
        });
        let (mut cleaned, mut recorded, mut edges) = (0, 0, 0);
        for (k, (out, mic, _)) in outs.iter().enumerate().skip(delay + 60) {
            assert_eq!(mic.samples(), ramp(k - delay).samples());
            let (o, m) = (mean(out.samples()), mean(mic.samples()));
            if (o - m / 2.0).abs() < 0.0005 {
                cleaned += 1;
            } else if out.samples() == mic.samples() {
                recorded += 1;
            } else {
                // The first frame of a hop after a skipped one: the
                // resampling filter starting afresh.
                edges += 1;
            }
        }
        let all = (cleaned + recorded + edges) as f32;
        let share = cleaned as f32 / all;
        assert!(
            (0.3..=0.8).contains(&share),
            "cleaned {share:.2} of the time"
        );
        assert!(edges as f32 / all < 0.1, "{edges} edge frames");
    }
}
