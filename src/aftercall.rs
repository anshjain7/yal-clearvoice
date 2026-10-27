//! After a recorded call: the team's offline pass on the agent's side, where
//! it does best, with the whole call to work on and nobody waiting.
//!
//!   run_tse.sh --enroll <voice sample> --input input.wav   (10 s at a time)
//!   run_se.sh <that>
//!
//! WeSep on the call's microphone (`input.wav` in its stage folder), 10 s at
//! a time (as good as the whole file in the team's test, and bounded in
//! memory), then DeepFilterNet3. The results go beside the live stages as
//! `after_call_tse` and `after_call_tse_se`, with their real-time factors in
//! rtf.csv. Then the cleaned voice, cut into utterances, goes to Sarvam's
//! batch service (batch.rs), and the agent's new lines, merged in time with
//! the customer's, become the call's improved transcript (transcribe.rs),
//! which the window summarizes again.
//!
//! It runs on its own thread below normal priority once the call is over.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::batch;
use crate::enhance::Enhancer;
use crate::record;
use crate::transcribe::{self, RATE, Side};
use crate::tse;

/// WeSep's piece of the call.
const PIECE: usize = 10 * RATE as usize;
/// Sarvam's limit is 30 s a file; utterances are cut shorter.
const MAX_UTTERANCE: usize = 25 * RATE as usize;
/// Longest a Sarvam job may take.
const SARVAM_WAIT: Duration = Duration::from_secs(600);

/// What the pass needs, set once at startup.
pub struct Config {
    pub files: tse::Files,
    pub threads: usize,
    /// The agent's voice sample.
    pub sample: PathBuf,
    /// DeepFilterNet3, and its attenuation limit.
    pub dfn3: PathBuf,
    pub attenuation: f32,
    /// Sarvam, for the improved transcript; None leaves it out.
    pub sarvam: Option<transcribe::Settings>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Turns the pass on for this run of the engine.
pub fn configure(config: Config) {
    let _ = CONFIG.set(config);
}

/// settings.ini's `[transcription] after_call` (on unless off/no/false/0).
pub fn enabled(ini: &str) -> bool {
    let mut section = "";
    for line in ini.trim_start_matches('\u{feff}').lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim();
        } else if section == "transcription"
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == "after_call"
        {
            let v = v
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            return !matches!(v.as_str(), "off" | "no" | "false" | "0");
        }
    }
    true
}

/// The pass on the call whose stage files are in `dir`, when configured;
/// `call` is that call's transcript, for the improved one.
pub(crate) fn run(dir: &Path, call: Option<transcribe::Finished>) {
    let Some(config) = CONFIG.get() else {
        return;
    };
    let mut log = fs::File::create(dir.join("after_call.log")).ok();
    let mut note = |line: String| {
        if let Some(f) = log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    };
    match pass(config, dir, call, &mut note) {
        Ok(()) => note("done".into()),
        Err(e) => note(format!("stopped: {e}")),
    }
}

fn pass(
    config: &Config,
    dir: &Path,
    call: Option<transcribe::Finished>,
    note: &mut impl FnMut(String),
) -> Result<(), String> {
    let input = read_wav(&dir.join("input.wav"))?;
    let secs = input.len() as f64 / f64::from(RATE);
    note(format!("input: {secs:.1} s"));

    // TSE: WeSep with the voice sample, a piece at a time.
    let started = Instant::now();
    let tse = match extract(config, &input) {
        Ok(tse) => Some(tse),
        Err(e) => {
            note(format!("TSE left out: {e}"));
            None
        }
    };
    let tse_took = started.elapsed();
    let tse = tse.unwrap_or_else(|| input.clone());

    // SE: DeepFilterNet3 over the whole result, then its tail brought out.
    let started = Instant::now();
    let mut se = Enhancer::new(&config.dfn3, config.attenuation)?;
    let mut cleaned: Vec<i16> = tse
        .chunks(transcribe::CHUNK)
        .flat_map(|c| se.process(c))
        .collect();
    cleaned.extend(se.process(&[0; transcribe::CHUNK]));
    cleaned.truncate(tse.len());
    let se_took = started.elapsed();

    record::write_wav(&dir.join("after_call_tse.wav"), &tse).map_err(|e| e.to_string())?;
    record::write_wav(&dir.join("after_call_tse_se.wav"), &cleaned).map_err(|e| e.to_string())?;
    let rtf = |took: Duration| took.as_secs_f64() / secs.max(1e-9);
    if let Ok(mut f) = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("rtf.csv"))
    {
        let _ = writeln!(
            f,
            "after call,{secs:.2},{:.3},{:.3},{:.3},{:.3}",
            tse_took.as_secs_f64(),
            rtf(tse_took),
            se_took.as_secs_f64(),
            rtf(se_took)
        );
    }
    note(format!(
        "TSE RTF {:.2}, SE RTF {:.2} (whole call)",
        rtf(tse_took),
        rtf(se_took)
    ));

    // The improved transcript: the agent's utterances through Sarvam, in
    // time with the customer's lines from the live transcript.
    let (Some(sarvam), Some(call)) = (&config.sarvam, call) else {
        note("no improved transcript: Sarvam or the call's transcript missing".into());
        return Ok(());
    };
    let spans = utterances(&cleaned);
    let wavs: Vec<Vec<u8>> = spans
        .iter()
        .map(|&(a, b)| record::wav_bytes(&cleaned[a..b]))
        .collect();
    let started = Instant::now();
    let texts = batch::transcribe(&sarvam.api_key, &sarvam.language, &wavs, SARVAM_WAIT)?;
    note(format!(
        "Sarvam: {} utterances in {:.0} s",
        spans.len(),
        started.elapsed().as_secs_f64()
    ));
    let agent: Vec<(f64, String)> = spans
        .iter()
        .zip(texts)
        .filter(|(_, t)| !t.is_empty())
        .map(|(&(_, end), t)| (end as f64 / f64::from(RATE), t))
        .collect();
    transcribe::improve(call.id, merge(&call.customer, &agent));
    Ok(())
}

/// WeSep on `input`, PIECE at a time, with the voice sample.
fn extract(config: &Config, input: &[i16]) -> Result<Vec<i16>, String> {
    let voice: Vec<f32> = read_wav(&config.sample)?
        .iter()
        .map(|&s| f32::from(s) / 32768.0)
        .collect();
    let mut model = tse::Extractor::load(&config.files, config.threads)?;
    let embedding = model.enroll(&voice)?;
    let mut out = Vec::with_capacity(input.len());
    for piece in input.chunks(PIECE) {
        let mix: Vec<f32> = piece.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let est = model.extract(&embedding, &mix)?;
        out.extend(
            tse::remix(&est, &mix, 0.0)
                .iter()
                .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
    }
    Ok(out)
}

/// A 16 kHz mono WAV's samples (the app's own files: 44-byte header).
fn read_wav(path: &Path) -> Result<Vec<i16>, String> {
    let b = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(b.get(44..)
        .ok_or("empty WAV")?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| i16::from_le_bytes(c))
        .collect())
}

/// Where someone is talking in `x` (16 kHz), as sample ranges: 20 ms frames
/// louder than both -45 dBFS and 30 dB under the loudest speech, joined
/// across pauses under 0.6 s, at least 0.25 s long, with 0.15 s either side,
/// and cut to MAX_UTTERANCE.
fn utterances(x: &[i16]) -> Vec<(usize, usize)> {
    const FRAME: usize = RATE as usize / 50;
    let db: Vec<f64> = x
        .chunks(FRAME)
        .map(|f| {
            let p = f.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / f.len().max(1) as f64;
            10.0 * (p / 32768f64.powi(2) + 1e-12).log10()
        })
        .collect();
    let mut sorted = db.clone();
    sorted.sort_by(f64::total_cmp);
    let loud = sorted
        .get(sorted.len() * 95 / 100)
        .copied()
        .unwrap_or(-120.0);
    let threshold = (-45f64).max(loud - 30.0);
    let (gap, pad, shortest) = (30, RATE as usize * 15 / 100, RATE as usize / 4);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, &d) in db.iter().enumerate() {
        if d < threshold {
            continue;
        }
        match spans.last_mut() {
            Some((_, end)) if i - *end / FRAME <= gap => *end = (i + 1) * FRAME,
            _ => spans.push((i * FRAME, (i + 1) * FRAME)),
        }
    }
    spans
        .into_iter()
        .filter(|(a, b)| b - a >= shortest)
        .map(|(a, b)| (a.saturating_sub(pad), (b + pad).min(x.len())))
        .flat_map(|(a, b)| {
            (a..b)
                .step_by(MAX_UTTERANCE)
                .map(move |s| (s, (s + MAX_UTTERANCE).min(b)))
        })
        .collect()
}

/// The call's lines in time: the customer's (seconds into the call when
/// the live transcript finalized them) and the agent's (seconds into the
/// recording when each utterance ended), compared one second apart, about
/// the live transcript's delay in finalizing a line.
fn merge(customer: &[(f64, String)], agent: &[(f64, String)]) -> Vec<(Side, String)> {
    let mut all: Vec<(f64, Side, String)> = customer
        .iter()
        .map(|(t, s)| (*t, Side::Customer, s.clone()))
        .chain(agent.iter().map(|(t, s)| (t + 1.0, Side::Agent, s.clone())))
        .collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    all.into_iter()
        .map(|(_, side, text)| (side, text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole pass on a recording, as after a recorded call, with the real
    /// models and Sarvam (it costs a little). The recording becomes the
    /// folder's input.wav; the folder gets after_call_tse(_se).wav, rtf.csv
    /// and after_call.log, and the improved transcript is printed. Set
    /// YAL_AFTERCALL_TEST to "<dir with onnxruntime.dll and wesep\\>|<voice
    /// sample.wav>|<recording.wav>|<settings.ini with the Sarvam key>|<out dir>".
    /// cargo test --release after_call_pass_on_a_recording -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep files, a recording and Sarvam's paid API"]
    fn after_call_pass_on_a_recording() {
        let spec = std::env::var("YAL_AFTERCALL_TEST").expect("set YAL_AFTERCALL_TEST");
        let [dir, sample, recording, ini, out] = spec.split('|').collect::<Vec<_>>()[..] else {
            panic!("YAL_AFTERCALL_TEST needs 5 parts");
        };
        let out = Path::new(out);
        fs::create_dir_all(out).unwrap();
        fs::copy(recording, out.join("input.wav")).unwrap();
        fs::write(
            out.join("rtf.csv"),
            "seconds,audio_s,tse_s,tse_rtf,se_s,se_rtf\n",
        )
        .unwrap();
        let ini = fs::read_to_string(ini).unwrap();
        configure(Config {
            files: tse::Files::find(Path::new(dir)).expect("WeSep files"),
            threads: 2,
            sample: PathBuf::from(sample),
            dfn3: Path::new(env!("CARGO_MANIFEST_DIR")).join("models/DeepFilterNet3_onnx.tar.gz"),
            attenuation: crate::denoise::DEFAULT_ATTENUATION_DB,
            sarvam: transcribe::Settings::parse(&ini),
        });
        // A customer line as the live transcript would have it, 7 s in.
        let call = transcribe::Finished {
            id: 1,
            customer: vec![(7.0, "(the customer's line from the live transcript)".into())],
        };
        let started = Instant::now();
        run(out, Some(call));
        println!("pass took {:.0} s", started.elapsed().as_secs_f32());
        println!(
            "{}",
            fs::read_to_string(out.join("after_call.log")).unwrap()
        );
        println!("{}", fs::read_to_string(out.join("rtf.csv")).unwrap());
        let json: serde_json::Value = serde_json::from_str(&transcribe::json(1, 0)).unwrap();
        for l in json["improved"]["lines"]
            .as_array()
            .expect("an improved transcript")
        {
            println!(
                "{:>8}: {}",
                l["who"].as_str().unwrap(),
                l["text"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn after_call_is_on_unless_turned_off() {
        assert!(enabled(""));
        assert!(enabled("[transcription]\napi_key = k\nafter_call = on\n"));
        assert!(!enabled("[transcription]\nafter_call = Off ; costs\n"));
        assert!(enabled("[summary]\nafter_call = off\n"));
    }

    #[test]
    fn utterances_follow_speech_and_pauses() {
        let rate = RATE as usize;
        // Silence, 1 s of speech, a 0.3 s pause, 1 s more, 2 s of silence,
        // a 0.1 s click, silence, then 30 s of speech.
        let tone = |n: usize| -> Vec<i16> {
            (0..n)
                .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
                .collect()
        };
        let quiet = |n: usize| vec![0i16; n];
        let x: Vec<i16> = [
            quiet(rate),
            tone(rate),
            quiet(rate * 3 / 10),
            tone(rate),
            quiet(2 * rate),
            tone(rate / 10),
            quiet(rate),
            tone(30 * rate),
        ]
        .concat();
        let u = utterances(&x);
        let secs: Vec<(f64, f64)> = u
            .iter()
            .map(|&(a, b)| (a as f64 / rate as f64, b as f64 / rate as f64))
            .collect();
        // The first two bursts are one utterance; the click is dropped; the
        // long one is cut at 25 s.
        assert_eq!(u.len(), 3, "{secs:?}");
        assert!(
            (secs[0].0 - 0.85).abs() < 0.03 && (secs[0].1 - 3.45).abs() < 0.03,
            "{secs:?}"
        );
        assert!((secs[1].1 - secs[1].0 - 25.0).abs() < 0.01, "{secs:?}");
        assert!(
            (secs[2].1 - x.len() as f64 / rate as f64).abs() < 0.01,
            "{secs:?}"
        );
        assert!(utterances(&quiet(rate)).is_empty());
    }

    #[test]
    fn lines_merge_in_time() {
        let customer = [(3.0, "Hello?".to_string()), (9.5, "Thanks.".to_string())];
        let agent = [
            (1.5, "Hi, YAL support.".to_string()),
            (7.0, "Your order ships today.".to_string()),
        ];
        assert_eq!(
            merge(&customer, &agent),
            [
                (Side::Agent, "Hi, YAL support.".to_string()),
                (Side::Customer, "Hello?".to_string()),
                (Side::Agent, "Your order ships today.".to_string()),
                (Side::Customer, "Thanks.".to_string()),
            ]
        );
    }
}
