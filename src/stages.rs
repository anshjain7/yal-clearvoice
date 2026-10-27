//! The agent's voice at each stage of the live chain, saved for each
//! recorded call: what the microphone heard, after TSE, and after speech
//! enhancement (what goes out through VB-CABLE, to the customer and the
//! transcript), with each step's real-time factor.
//!
//! The live stage (denoise.rs) hands every 10 ms frame it sends out to `run`
//! here as a copy, without waiting; this thread, below normal priority,
//! brings the copies down to 16 kHz and saves them in 2 s blocks while a call
//! is being recorded (record.rs). If it falls behind, copies are dropped,
//! never the call's audio. Until the live chain has a TSE model, `audio_tse`
//! is the microphone as it was.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;
use std::{fs, thread};

use crate::audiofile;
use crate::denoise::Control;
use crate::enhance::Fir;
use crate::record;
use crate::ring::Consumer;
use crate::transcribe::RATE;
use crate::voice;

/// Audio saved at a time: 2 s at 16 kHz.
const BLOCK: usize = 2 * RATE as usize;
/// The live chain's rate over the saved files' (48 kHz to 16 kHz).
const DOWN: usize = 3;

/// Saves the live chain's copies from `tap` (stereo 48 kHz frames: the
/// microphone, then what went out) while a call is being recorded
/// (`recording`: record::recording, the recording under way), with the
/// real-time factor of the live stage from `control`. Runs until `stop`.
pub fn run(
    tap: &mut Consumer,
    control: &Control,
    stop: &AtomicBool,
    recording: impl Fn() -> Option<PathBuf>,
) {
    voice::below_normal_priority();
    let (mut down_mic, mut down_out) = (Fir::new(), Fir::new());
    let mut gathered = Gathered::default();
    let mut dump = Dump::new(true);
    let mut current: Option<PathBuf> = None;
    while !stop.load(Relaxed) {
        let Some(frame) = tap.pop() else {
            thread::sleep(Duration::from_millis(5));
            continue;
        };
        let now = recording();
        if now != current {
            // What's gathered belongs to the recording before; finish it.
            gathered.save(&mut dump, current.take(), control);
            gathered = Gathered::starting(control);
            if now.is_none() {
                let _ = dump.close();
            }
            current = now;
        }
        if current.is_none() {
            continue;
        }
        let (mic, out): (Vec<f32>, Vec<f32>) = frame
            .samples()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| (p[0], p[1]))
            .unzip();
        gathered.mic.extend(to_i16(&down_mic.filter(&mic, DOWN)));
        gathered.out.extend(to_i16(&down_out.filter(&out, DOWN)));
        if gathered.mic.len() >= BLOCK {
            gathered.save(&mut dump, current.clone(), control);
            gathered = Gathered::starting(control);
        }
    }
    gathered.save(&mut dump, current, control);
    let _ = dump.close();
}

/// One block on its way to the files, and where the live stage's counters
/// stood when it started.
#[derive(Default)]
struct Gathered {
    mic: Vec<i16>,
    out: Vec<i16>,
    handled: u64,
    busy_us: u64,
}

impl Gathered {
    fn starting(control: &Control) -> Gathered {
        Gathered {
            handled: control.handled.load(Relaxed),
            busy_us: control.busy_us.load(Relaxed),
            ..Gathered::default()
        }
    }

    /// Hands the block to `dump` for `recording`, with how long the live
    /// stage spent on it.
    fn save(&self, dump: &mut Dump, recording: Option<PathBuf>, control: &Control) {
        if self.mic.is_empty() {
            return;
        }
        let busy = control.busy_us.load(Relaxed).saturating_sub(self.busy_us);
        let frames = control.handled.load(Relaxed).saturating_sub(self.handled);
        let se = (frames > 0).then(|| Duration::from_micros(busy));
        let n = self.mic.len().min(self.out.len());
        // No TSE stage in the live chain yet: after TSE is the microphone.
        dump.add(
            recording,
            &self.mic[..n],
            &self.mic[..n],
            None,
            &self.out[..n],
            se,
        );
    }
}

fn to_i16(x: &[f32]) -> Vec<i16> {
    x.iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

/// The stages saved for each recorded call, in `Agent voice\<recording>\`.
pub(crate) const STAGES: [&str; 3] = ["input", "audio_tse", "audio_tse_se"];

/// What's saved while a call is recorded, in a folder beside the recording:
/// the agent's voice at each stage (`input`: what the models got;
/// `audio_tse`: after TSE; `audio_tse_se`: after speech enhancement too,
/// what the transcript gets), written as WAV during the call and made M4A
/// once it's over; and `rtf.csv`, each step's real-time factor (time it
/// took / length of the audio) block by block and for the whole call.
pub(crate) struct Dump {
    /// Whether the input is the raw microphone (on this PC). A server gets
    /// the call's cleaned voice and does its own TSE, so then only the last
    /// stage is saved.
    raw_input: bool,
    open: Option<Stages>,
}

/// One recorded call's files.
struct Stages {
    recording: PathBuf,
    dir: PathBuf,
    /// One per stage (None: not saved).
    wavs: [Option<record::Wav>; 3],
    rtf: fs::File,
    /// Seconds of audio so far.
    at: f64,
    /// Seconds of audio through each step, and seconds it took.
    tse: (f64, f64),
    se: (f64, f64),
}

impl Dump {
    pub(crate) fn new(raw_input: bool) -> Dump {
        Dump {
            raw_input,
            open: None,
        }
    }

    /// Adds one block's stages for the recording `recording` (None: no call
    /// being recorded), starting or finishing a call's files as recordings
    /// come and go.
    pub(crate) fn add(
        &mut self,
        recording: Option<PathBuf>,
        raw: &[i16],
        tse: &[i16],
        tse_took: Option<Duration>,
        tse_se: &[i16],
        se_took: Option<Duration>,
    ) {
        if self.open.as_ref().map(|s| &s.recording) != recording.as_ref() {
            let _ = self.close();
            self.open = recording.and_then(|r| Stages::start(r, self.raw_input).ok());
        }
        let Some(s) = &mut self.open else {
            return;
        };
        for (wav, audio) in s.wavs.iter_mut().zip([raw, tse, tse_se]) {
            if let Some(w) = wav
                && w.write(audio).is_err()
            {
                *wav = None;
            }
        }
        let secs = raw.len() as f64 / f64::from(RATE);
        let step = |took: Option<Duration>, total: &mut (f64, f64)| match took {
            Some(t) => {
                total.0 += secs;
                total.1 += t.as_secs_f64();
                format!(
                    "{:.3},{:.3}",
                    t.as_secs_f64(),
                    t.as_secs_f64() / secs.max(1e-9)
                )
            }
            None => String::from(","),
        };
        let tse_cols = step(tse_took.filter(|_| self.raw_input), &mut s.tse);
        let se_cols = step(se_took, &mut s.se);
        let _ = writeln!(s.rtf, "{:.1},{secs:.2},{tse_cols},{se_cols}", s.at);
        s.at += secs;
    }

    /// Finishes the open call's files: the whole call's real-time factors,
    /// then the stages made M4A (in the background: it takes a few seconds).
    pub(crate) fn close(&mut self) -> Option<std::thread::JoinHandle<()>> {
        let mut s = self.open.take()?;
        let whole = |(audio, took): (f64, f64)| {
            if audio > 0.0 {
                format!("{took:.3},{:.3}", took / audio)
            } else {
                String::from(",")
            }
        };
        let _ = writeln!(
            s.rtf,
            "whole call,{:.2},{},{}",
            s.at,
            whole(s.tse),
            whole(s.se)
        );
        for w in s.wavs.iter_mut().flatten() {
            let _ = w.patch();
        }
        drop(s.wavs);
        let dir = s.dir;
        Some(std::thread::spawn(move || to_m4a(&dir)))
    }
}

impl Stages {
    fn start(recording: PathBuf, raw_input: bool) -> std::io::Result<Stages> {
        let dir = record::agent_voice_dir(&recording);
        fs::create_dir_all(&dir)?;
        let wav = |name: &str, keep: bool| {
            keep.then(|| record::Wav::create(&dir.join(format!("{name}.wav"))).ok())
                .flatten()
        };
        let wavs = [
            wav(STAGES[0], raw_input),
            wav(STAGES[1], raw_input),
            wav(STAGES[2], true),
        ];
        let mut rtf = fs::File::create(dir.join("rtf.csv"))?;
        writeln!(rtf, "seconds,audio_s,tse_s,tse_rtf,se_s,se_rtf")?;
        Ok(Stages {
            recording,
            dir,
            wavs,
            rtf,
            at: 0.0,
            tse: (0.0, 0.0),
            se: (0.0, 0.0),
        })
    }
}

/// Makes each stage's WAV in `dir` an M4A, removing the WAV once its M4A is
/// written. Where Windows can't encode M4A, the WAV stays.
fn to_m4a(dir: &Path) {
    voice::below_normal_priority();
    for name in STAGES {
        let wav = dir.join(format!("{name}.wav"));
        let Ok(bytes) = fs::read(&wav) else {
            continue;
        };
        let samples: Vec<i16> = bytes
            .get(44..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        if audiofile::write_m4a(&samples, &dir.join(format!("{name}.m4a"))).is_ok() {
            let _ = fs::remove_file(&wav);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::AudioFrame;
    use crate::ring;

    #[test]
    fn run_saves_the_live_chain_while_a_call_is_recorded() {
        let dir = std::env::temp_dir().join(format!("clearvoice-stages-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let call = dir.join("2026-10-27 10-00-00.wav");
        let (control, stop, on) = (
            Control::default(),
            AtomicBool::new(false),
            AtomicBool::new(true),
        );
        let (mut tap_tx, mut tap_rx) = ring::new(512);
        // 4.5 s of copies: a 300 Hz tone from the microphone, and the same at
        // half the level going out.
        let frame = |k: usize| {
            let mut f = AudioFrame::new(2);
            f.seq = k as u64;
            for (i, p) in f
                .samples_mut()
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .enumerate()
            {
                let t = (k * 480 + i) as f32 / 48_000.0;
                let v = 0.4 * (std::f32::consts::TAU * 300.0 * t).sin();
                (p[0], p[1]) = (v, v / 2.0);
            }
            f
        };
        let ms = Duration::from_millis;
        let folder = record::agent_voice_dir(&call);
        let names = STAGES.map(|n| folder.join(format!("{n}.m4a")));
        thread::scope(|s| {
            let (control, stop, on, call) = (&control, &stop, &on, &call);
            let tap_rx = &mut tap_rx;
            let recording = move || on.load(Relaxed).then(|| call.clone());
            let writer = s.spawn(move || run(tap_rx, control, stop, recording));
            for k in 0..450 {
                // As the live stage does: each frame counted, with its time
                // (3 ms of its 10), then copied.
                control.handled.fetch_add(1, Relaxed);
                control.busy_us.fetch_add(3000, Relaxed);
                assert!(tap_tx.push(&frame(k)));
                thread::sleep(ms(1));
            }
            thread::sleep(ms(200));
            // The recording stops: its files are finished and made M4A.
            on.store(false, Relaxed);
            assert!(tap_tx.push(&frame(450)));
            let started = std::time::Instant::now();
            while names.iter().any(|p| !p.exists()) {
                assert!(started.elapsed() < Duration::from_secs(20), "no M4A files");
                thread::sleep(ms(100));
            }
            stop.store(true, Relaxed);
            writer.join().unwrap();
        });
        let rtf = fs::read_to_string(folder.join("rtf.csv")).unwrap();
        let rows: Vec<Vec<&str>> = rtf.lines().map(|l| l.split(',').collect()).collect();
        assert_eq!(rows.len(), 5, "{rtf}");
        // Two whole blocks and the last half second; no TSE in the live chain
        // yet, so its columns stay empty.
        assert_eq!(
            rows[1..4].iter().map(|r| r[1]).collect::<Vec<_>>(),
            ["2.00", "2.00", "0.50"]
        );
        assert!(
            rows[1..4]
                .iter()
                .all(|r| r[2].is_empty() && r[3].is_empty())
        );
        let whole = &rows[4];
        assert_eq!((whole[0], whole[1]), ("whole call", "4.50"));
        let se_rtf: f64 = whole[5].parse().unwrap();
        assert!((0.25..=0.31).contains(&se_rtf), "SE RTF {se_rtf}");
        // input and audio_tse are the microphone; audio_tse_se what went out,
        // 6 dB quieter.
        let level = |p: &Path| {
            let x = audiofile::read(p).unwrap();
            let x = &x[RATE as usize..x.len() - RATE as usize];
            let rms =
                (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
            20.0 * (rms / 32768.0).log10()
        };
        let (mic, tse, se) = (level(&names[0]), level(&names[1]), level(&names[2]));
        assert!((mic + 11.0).abs() < 1.0, "input at {mic:.1} dB");
        assert!((tse - mic).abs() < 0.1);
        assert!((mic - se - 6.0).abs() < 0.5, "{mic:.1} vs {se:.1} dB");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dump_saves_each_stage_and_its_rtf() {
        let dir = std::env::temp_dir().join(format!("clearvoice-dump-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let call = dir.join("2026-10-26 15-40-02.wav");
        let mut d = Dump::new(true);
        let ms = Duration::from_millis;
        let (raw, tse, se) = (vec![100i16; BLOCK], vec![50i16; BLOCK], vec![40i16; BLOCK]);
        d.add(
            Some(call.clone()),
            &raw,
            &tse,
            Some(ms(1000)),
            &se,
            Some(ms(200)),
        );
        d.add(Some(call.clone()), &raw, &raw, None, &se, Some(ms(300))); // TSE didn't run
        d.add(None, &raw, &raw, None, &raw, None); // the recording stopped
        let folder = dir.join("Agent voice").join("2026-10-26 15-40-02");
        // Real-time factors: per block, then for the whole call (TSE over the
        // block it ran on, SE over both).
        let rtf = fs::read_to_string(folder.join("rtf.csv")).unwrap();
        let lines: Vec<&str> = rtf.lines().collect();
        assert_eq!(lines[0], "seconds,audio_s,tse_s,tse_rtf,se_s,se_rtf");
        assert_eq!(lines[1], "0.0,2.00,1.000,0.500,0.200,0.100");
        assert_eq!(lines[2], "2.0,2.00,,,0.300,0.150");
        assert_eq!(lines[3], "whole call,4.00,1.000,0.500,0.500,0.125");
        // Nothing more once the recording has stopped; the stages become M4A.
        assert!(
            d.close().is_none(),
            "already closed when the recording stopped"
        );
        let (mut tries, names) = (0, STAGES.map(|n| folder.join(format!("{n}.m4a"))));
        while names.iter().any(|p| !p.exists()) && tries < 100 {
            thread::sleep(ms(100));
            tries += 1;
        }
        for (name, p) in STAGES.iter().zip(&names) {
            assert!(p.exists(), "{name}.m4a");
        }
        let back = audiofile::read(&names[0]).unwrap();
        assert!(
            (back.len() as i64 - 2 * BLOCK as i64).abs() < 1600,
            "{} samples",
            back.len()
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
