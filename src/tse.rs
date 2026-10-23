//! WeSep target speaker extraction on this PC: keeps only the agent's voice
//! (the one in their voice sample) out of their side of the call, for the
//! transcript. WeSep's pretrained BSRNN + ECAPA model, exported to ONNX:
//!
//! - `wesep\speaker.onnx`: Kaldi fbank features of the voice sample ->
//!   the speaker embedding, once per sample;
//! - `wesep\separator.int8.onnx`: the mixture's spectrum + the embedding ->
//!   the agent's spectrum (8-bit weights: a quarter of the size, 25% faster,
//!   output within 0.9996 of full precision).
//!
//! The STFT and ISTFT around the separator, and the fbank, are done here,
//! the same way WeSep's PyTorch code does them. ONNX Runtime is loaded from
//! `onnxruntime.dll` next to the engine.
//!
//! The model looks at its whole input at once (it isn't causal), so it works
//! on blocks: see voice.rs, which also keeps it off the audio path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex32;

/// The model's rate: 16 kHz mono.
const RATE: usize = 16_000;
/// STFT window and hop: 32 ms and 8 ms, as the model was trained.
const WIN: usize = 512;
const HOP: usize = 128;
const BINS: usize = WIN / 2 + 1;

/// The files extraction needs, next to the engine.
pub struct Files {
    pub runtime: PathBuf,
    pub speaker: PathBuf,
    pub separator: PathBuf,
}

impl Files {
    /// The files in `dir`, if all of them are there.
    pub fn find(dir: &Path) -> Option<Files> {
        let f = Files {
            runtime: dir.join("onnxruntime.dll"),
            speaker: dir.join("wesep").join("speaker.onnx"),
            separator: dir.join("wesep").join("separator.int8.onnx"),
        };
        [&f.runtime, &f.speaker, &f.separator]
            .iter()
            .all(|p| p.is_file())
            .then_some(f)
    }
}

/// The loaded model.
pub struct Extractor {
    speaker: Session,
    separator: Session,
    fft: Arc<dyn realfft::RealToComplex<f32>>,
    ifft: Arc<dyn realfft::ComplexToReal<f32>>,
}

impl Extractor {
    /// Loads ONNX Runtime and both models; `threads` for the separator.
    /// Slow (a second or two): call it off the audio threads.
    pub fn load(files: &Files, threads: usize) -> Result<Extractor, String> {
        let env = ort::init_from(&files.runtime)
            .map_err(|e| format!("ONNX Runtime: {e}"))?
            .with_name("yal-clearvoice")
            .build()
            .map_err(|e| e.to_string())?;
        let session = |path: &Path, threads: usize| -> Result<Session, String> {
            Session::builder(&env)
                .and_then(|b| Ok(b.with_intra_threads(threads)?))
                .and_then(|b| Ok(b.with_inter_threads(1)?))
                // No spinning: idle pool threads would otherwise keep cores busy.
                .and_then(|b| Ok(b.with_intra_op_spinning(false)?))
                .and_then(|mut b| b.commit_from_file(path))
                .map_err(|e| format!("{}: {e}", path.display()))
        };
        let mut planner = RealFftPlanner::<f32>::new();
        Ok(Extractor {
            speaker: session(&files.speaker, 1)?,
            separator: session(&files.separator, threads.max(1))?,
            fft: planner.plan_fft_forward(WIN),
            ifft: planner.plan_fft_inverse(WIN),
        })
    }

    /// The speaker embedding of a voice sample (16 kHz, -1..1).
    pub fn enroll(&mut self, voice: &[f32]) -> Result<Vec<f32>, String> {
        let feats = fbank(voice);
        let frames = feats.len() / MEL_BINS;
        if frames < 10 {
            return Err("voice sample too short".into());
        }
        let input =
            Tensor::from_array(([1usize, frames, MEL_BINS], feats)).map_err(|e| e.to_string())?;
        let out = self
            .speaker
            .run(ort::inputs![input])
            .map_err(|e| e.to_string())?;
        let (_, emb) = out[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        Ok(emb.to_vec())
    }

    /// The voice matching `embedding` in `mix` (16 kHz, -1..1), as long as
    /// `mix`, before any remix.
    pub fn extract(&mut self, embedding: &[f32], mix: &[f32]) -> Result<Vec<f32>, String> {
        let (real, imag, frames) = stft(&self.fft, mix);
        let shape = [1usize, BINS, frames];
        let inputs = ort::inputs![
            Tensor::from_array((shape, real)).map_err(|e| e.to_string())?,
            Tensor::from_array((shape, imag)).map_err(|e| e.to_string())?,
            Tensor::from_array(([1usize, embedding.len()], embedding.to_vec()))
                .map_err(|e| e.to_string())?,
        ];
        let out = self.separator.run(inputs).map_err(|e| e.to_string())?;
        let (_, r) = out[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let (_, i) = out[1]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        Ok(istft(&self.ifft, r, i, frames, mix.len()))
    }
}

/// run_tse.sh's --remix: the extracted voice at the input's loudness, the
/// right way up (the model can return it inverted, which would cancel part of
/// the voice when the original is mixed back in), with `amount` of the
/// original mixed back in.
pub fn remix(est: &[f32], mix: &[f32], amount: f32) -> Vec<f32> {
    let rms =
        |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt() + 1e-8;
    let mut gain = rms(mix) / rms(est);
    if est.iter().zip(mix).map(|(a, b)| a * b).sum::<f32>() < 0.0 {
        gain = -gain;
    }
    let out: Vec<f32> = est
        .iter()
        .zip(mix)
        .map(|(e, m)| (1.0 - amount) * e * gain + amount * m)
        .collect();
    let peak = out.iter().fold(0f32, |p, v| p.max(v.abs()));
    if peak > 0.99 {
        out.iter().map(|v| v * 0.99 / peak).collect()
    } else {
        out
    }
}

/// torch.stft(n_fft=512, hop=128, hann window, center=True, reflect padding):
/// real and imaginary parts, bins x frames (row-major), and the frame count.
fn stft(fft: &Arc<dyn realfft::RealToComplex<f32>>, x: &[f32]) -> (Vec<f32>, Vec<f32>, usize) {
    let pad = WIN / 2;
    let n = x.len();
    let reflect = |i: isize| -> f32 {
        let mut i = i;
        // Reflect without repeating the edge sample, like torch's "reflect".
        while i < 0 || i >= n as isize {
            if i < 0 {
                i = -i;
            }
            if i >= n as isize {
                i = 2 * (n as isize - 1) - i;
            }
        }
        x[i as usize]
    };
    let frames = 1 + n / HOP;
    let window = hann();
    let (mut real, mut imag) = (vec![0f32; BINS * frames], vec![0f32; BINS * frames]);
    let mut buf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    for t in 0..frames {
        let start = (t * HOP) as isize - pad as isize;
        for (k, b) in buf.iter_mut().enumerate() {
            *b = reflect(start + k as isize) * window[k];
        }
        let _ = fft.process(&mut buf, &mut spec);
        for (f, c) in spec.iter().enumerate() {
            real[f * frames + t] = c.re;
            imag[f * frames + t] = c.im;
        }
    }
    (real, imag, frames)
}

/// torch.istft with the same settings, trimmed to `len` samples.
fn istft(
    ifft: &Arc<dyn realfft::ComplexToReal<f32>>,
    real: &[f32],
    imag: &[f32],
    frames: usize,
    len: usize,
) -> Vec<f32> {
    let window = hann();
    let total = WIN + HOP * (frames - 1);
    let (mut out, mut norm) = (vec![0f32; total], vec![0f32; total]);
    let mut spec = ifft.make_input_vec();
    let mut buf = ifft.make_output_vec();
    for t in 0..frames {
        for (f, c) in spec.iter_mut().enumerate() {
            *c = Complex32::new(real[f * frames + t], imag[f * frames + t]);
        }
        // The DC and Nyquist bins of a real signal have no imaginary part.
        spec[0].im = 0.0;
        spec[BINS - 1].im = 0.0;
        let _ = ifft.process(&mut spec, &mut buf);
        for k in 0..WIN {
            out[t * HOP + k] += buf[k] / WIN as f32 * window[k];
            norm[t * HOP + k] += window[k] * window[k];
        }
    }
    let pad = WIN / 2;
    (0..len)
        .map(|i| {
            let j = i + pad;
            if j < total && norm[j] > 1e-11 {
                out[j] / norm[j]
            } else {
                0.0
            }
        })
        .collect()
}

/// The periodic Hann window torch.hann_window(512) makes.
fn hann() -> Vec<f32> {
    (0..WIN)
        .map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / WIN as f32).cos())
        .collect()
}

const MEL_BINS: usize = 80;
const FRAME_LEN: usize = 400; // 25 ms
const FRAME_SHIFT: usize = 160; // 10 ms
const FFT_LEN: usize = 512;

/// Kaldi fbank as torchaudio.compliance.kaldi.fbank(num_mel_bins=80,
/// frame_length=25, frame_shift=10) computes it, then with the mean of each
/// bin taken out, as WeSep feeds ECAPA. Frames x 80, row-major.
fn fbank(x: &[f32]) -> Vec<f32> {
    let mut feats = log_mel(x);
    let frames = feats.len() / MEL_BINS;
    for m in 0..MEL_BINS {
        let mean = (0..frames).map(|t| feats[t * MEL_BINS + m]).sum::<f32>() / frames.max(1) as f32;
        for t in 0..frames {
            feats[t * MEL_BINS + m] -= mean;
        }
    }
    feats
}

/// Kaldi's log mel energies: no dither, DC removed, pre-emphasis 0.97, Povey
/// window, power spectrum, 80 mel bins from 20 Hz to 8 kHz, log.
fn log_mel(x: &[f32]) -> Vec<f32> {
    if x.len() < FRAME_LEN {
        return Vec::new();
    }
    let frames = 1 + (x.len() - FRAME_LEN) / FRAME_SHIFT;
    let povey: Vec<f32> = (0..FRAME_LEN)
        .map(|n| {
            let h = 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / (FRAME_LEN - 1) as f64).cos();
            h.powf(0.85) as f32
        })
        .collect();
    let banks = mel_banks();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
    let (mut buf, mut spec) = (fft.make_input_vec(), fft.make_output_vec());
    let mut out = vec![0f32; frames * MEL_BINS];
    for t in 0..frames {
        let frame = &x[t * FRAME_SHIFT..t * FRAME_SHIFT + FRAME_LEN];
        let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
        // Pre-emphasis; the first sample is emphasized against itself.
        let mut prev = frame[0] - mean;
        buf.fill(0.0);
        for (k, &v) in frame.iter().enumerate() {
            let v = v - mean;
            buf[k] = (v - 0.97 * prev) * povey[k];
            prev = v;
        }
        let _ = fft.process(&mut buf, &mut spec);
        for (m, bank) in banks.iter().enumerate() {
            let e: f32 = bank.iter().map(|&(i, w)| spec[i].norm_sqr() * w).sum();
            out[t * MEL_BINS + m] = e.max(f32::EPSILON).ln();
        }
    }
    out
}

/// Kaldi's triangular mel filters, 20 Hz to 8 kHz: for each bin, the FFT bins
/// it covers and their weights.
fn mel_banks() -> Vec<Vec<(usize, f32)>> {
    let mel = |f: f64| 1127.0 * (1.0 + f / 700.0).ln();
    let (low, high) = (mel(20.0), mel(RATE as f64 / 2.0));
    let delta = (high - low) / (MEL_BINS + 1) as f64;
    let bin_width = RATE as f64 / FFT_LEN as f64;
    (0..MEL_BINS)
        .map(|b| {
            let left = low + b as f64 * delta;
            let (center, right) = (left + delta, left + 2.0 * delta);
            (0..FFT_LEN / 2)
                .filter_map(|i| {
                    let m = mel(bin_width * i as f64);
                    let w = ((m - left) / (center - left)).min((right - m) / (right - center));
                    (w > 0.0).then_some((i, w as f32))
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.1 s of two tones, the signal the reference values below came from
    /// (torchaudio's kaldi.fbank and torch.stft, before mean removal).
    fn tones() -> Vec<f32> {
        (0..1600)
            .map(|n| {
                let t = n as f64 / RATE as f64;
                (0.3 * (std::f64::consts::TAU * 440.0 * t).sin()
                    + 0.1 * (std::f64::consts::TAU * 1234.0 * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn fbank_matches_kaldi() {
        let x = tones();
        // torchaudio's kaldi.fbank of the same signal, before mean removal.
        let raw = log_mel(&x);
        assert_eq!(raw.len(), 8 * MEL_BINS);
        let want = [
            (0, 0, -13.0341),
            (0, 1, -11.7747),
            (0, 2, -12.3604),
            (0, 3, -13.523),
            (0, 10, -5.5929),
            (3, 40, -13.385),
            (7, 79, -15.9424),
        ];
        for (t, m, v) in want {
            let got = raw[t * MEL_BINS + m];
            assert!(
                (got - v).abs() < 0.01,
                "f[{t}][{m}] = {got}, torchaudio {v}"
            );
        }
        // And fbank takes each bin's mean out.
        let feats = fbank(&x);
        for m in 0..MEL_BINS {
            let s: f32 = (0..8).map(|t| feats[t * MEL_BINS + m]).sum();
            assert!(s.abs() < 1e-3);
        }
    }

    #[test]
    fn stft_matches_torch() {
        let x = tones();
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(WIN);
        let (r, i, frames) = stft(&fft, &x);
        assert_eq!(frames, 13);
        let at = |f: usize, t: usize| (r[f * frames + t], i[f * frames + t]);
        for ((f, t), (re, im)) in [
            ((14, 3), (-14.0779, 35.5566)),
            ((40, 6), (10.7086, -1.2164)),
            ((0, 0), (3.8859, 0.0)),
        ] {
            let (gr, gi) = at(f, t);
            assert!(
                (gr - re).abs() < 1e-3 && (gi - im).abs() < 1e-3,
                "s[{f},{t}] = {gr}+{gi}i, torch {re}+{im}i"
            );
        }
    }

    #[test]
    fn istft_undoes_stft() {
        let x: Vec<f32> = (0..16_000)
            .map(|n| ((n * 7919 % 1000) as f32 / 500.0 - 1.0) * 0.3)
            .collect();
        let mut planner = RealFftPlanner::<f32>::new();
        let (fft, ifft) = (planner.plan_fft_forward(WIN), planner.plan_fft_inverse(WIN));
        let (r, i, frames) = stft(&fft, &x);
        let y = istft(&ifft, &r, &i, frames, x.len());
        let worst = x
            .iter()
            .zip(&y)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-4, "round trip off by {worst}");
    }

    #[test]
    fn remix_matches_loudness_and_fixes_an_inverted_voice() {
        let mix: Vec<f32> = (0..1000).map(|n| (n as f32 * 0.05).sin() * 0.4).collect();
        let inverted: Vec<f32> = mix.iter().map(|v| -v * 0.1).collect();
        let out = remix(&inverted, &mix, 0.2);
        // Turned the right way up and brought to the input's level, the
        // extracted voice here is the input itself.
        assert!(out.iter().zip(&mix).all(|(o, m)| (o - m).abs() < 1e-3));
        let loud: Vec<f32> = mix.iter().map(|v| v * 5.0).collect();
        assert!(
            remix(&loud, &loud, 0.0)
                .iter()
                .all(|v| v.abs() <= 0.99 + 1e-6)
        );
    }

    /// The model on the Odia sample, against the same model run from Python
    /// (WeSep's PyTorch code, whole file, remix 0.2). Needs the model files
    /// beside the engine's folder layout in YAL_TSE_DIR and the two samples.
    /// cargo test --release extracts_like_wesep -- --ignored --nocapture
    #[test]
    #[ignore = "needs the WeSep ONNX files"]
    fn extracts_like_wesep() {
        let dir = std::env::var("YAL_TSE_DIR").expect("set YAL_TSE_DIR");
        let files = Files::find(Path::new(&dir)).expect("model files");
        let read = |p: &str| -> Vec<f32> {
            let b = std::fs::read(p).unwrap();
            b[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| i16::from_le_bytes(c) as f32 / 32768.0)
                .collect()
        };
        let enroll = read(r"C:\Users\HP\Desktop\00005148.wav");
        let mix = read(r"C:\Users\HP\Desktop\00005147.wav");
        let reference = read(r"C:\Users\HP\Desktop\WeSep test\00005147_tse_remix0.2.wav");
        let t = std::time::Instant::now();
        let mut ex = Extractor::load(&files, 2).unwrap();
        println!("loaded in {:.2} s", t.elapsed().as_secs_f32());
        let t = std::time::Instant::now();
        let emb = ex.enroll(&enroll).unwrap();
        println!(
            "enrolled in {:.2} s ({} values)",
            t.elapsed().as_secs_f32(),
            emb.len()
        );
        let t = std::time::Instant::now();
        let est = ex.extract(&emb, &mix).unwrap();
        let secs = t.elapsed().as_secs_f32();
        let out = remix(&est, &mix, 0.2);
        let n = out.len().min(reference.len());
        let c = |a: &[f32], b: &[f32]| {
            let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            d / (a.iter().map(|x| x * x).sum::<f32>().sqrt()
                * b.iter().map(|x| x * x).sum::<f32>().sqrt())
        };
        let corr = c(&out[..n], &reference[..n]);
        println!(
            "extracted {:.2} s in {secs:.2} s ({:.2}x real time, 2 threads); vs WeSep in Python: {corr:.4}",
            mix.len() as f32 / RATE as f32,
            secs / (mix.len() as f32 / RATE as f32)
        );
        assert!(corr > 0.98, "{corr}");
    }
}
