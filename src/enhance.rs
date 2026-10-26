//! Speech enhancement for the transcript side: the team's run_se.sh step,
//! run after speaker extraction (voice.rs). DeepFilterNet3 (denoise.rs) on a
//! 16 kHz stream: up to 48 kHz, the model in 10 ms frames, back to 16 kHz.
//! Off the audio path, on the voice-cleaning thread; it's a second model,
//! separate from the one cleaning the call itself.

use std::path::Path;

use crate::denoise::DeepFilter;

/// 16 kHz to the model's 48 kHz.
const FACTOR: usize = 3;
/// The model's frame at 48 kHz.
const FRAME: usize = 480;

/// DeepFilterNet3 on a 16 kHz stream. The output runs a fixed ~35 ms behind
/// the input (the model's 30 ms plus the resampling filters).
pub struct Enhancer {
    df: DeepFilter,
    up: Fir,
    down: Fir,
    /// 48 kHz audio waiting for a whole frame.
    pending: Vec<f32>,
    /// 48 kHz output waiting to be brought down: kept a whole multiple of
    /// FACTOR so every output sample lands on the same phase.
    out48: Vec<f32>,
}

impl Enhancer {
    /// Loads the model (slow: a second or two). `attenuation_db` as in
    /// settings.ini's [noise] (see denoise.rs).
    pub fn new(model: &Path, attenuation_db: f32) -> Result<Enhancer, String> {
        Ok(Enhancer {
            df: DeepFilter::new(model, attenuation_db).map_err(|e| format!("{e:#}"))?,
            up: Fir::new(),
            down: Fir::new(),
            pending: Vec::new(),
            out48: Vec::new(),
        })
    }

    /// Cleans the next part of the stream; returns as many samples as it was
    /// given once frames line up (a 100 ms chunk always does).
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let x: Vec<f32> = input.iter().map(|&s| f32::from(s) / 32768.0).collect();
        // Up: zeros between samples, then the low-pass (gain FACTOR).
        let mut stuffed = vec![0f32; x.len() * FACTOR];
        for (i, v) in x.iter().enumerate() {
            stuffed[i * FACTOR] = v * FACTOR as f32;
        }
        self.pending.extend(self.up.filter(&stuffed, 1));
        let whole = self.pending.len() / FRAME * FRAME;
        let mut cleaned = vec![0f32; whole];
        for (i, o) in self.pending[..whole]
            .chunks(FRAME)
            .zip(cleaned.chunks_mut(FRAME))
        {
            // A failed frame stays silent rather than stopping the transcript.
            let _ = self.df.process(i, o);
        }
        self.pending.drain(..whole);
        self.out48.extend(cleaned);
        // Down: the low-pass, keeping every FACTOR-th sample.
        let usable = self.out48.len() / FACTOR * FACTOR;
        let y = self.down.filter(&self.out48[..usable], FACTOR);
        self.out48.drain(..usable);
        y.iter()
            .map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect()
    }
}

/// A windowed-sinc low-pass at 8 kHz for 48 kHz audio, run as a stream: it
/// keeps the last few samples of each call for the next.
struct Fir {
    taps: Vec<f32>,
    /// The input's last `taps.len() - 1` samples.
    history: Vec<f32>,
}

impl Fir {
    fn new() -> Fir {
        let n = 32 * FACTOR + 1;
        let mid = (n / 2) as f32;
        let cutoff = 0.5 / FACTOR as f32;
        let taps = (0..n)
            .map(|k| {
                let t = k as f32 - mid;
                let sinc = if t == 0.0 {
                    2.0 * cutoff
                } else {
                    (std::f32::consts::TAU * cutoff * t).sin() / (std::f32::consts::PI * t)
                };
                let window = 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / (n - 1) as f32).cos();
                sinc * window
            })
            .collect();
        Fir {
            taps,
            history: vec![0.0; n - 1],
        }
    }

    /// Filters `x`, returning every `step`-th output (`x.len()` a multiple of it).
    fn filter(&mut self, x: &[f32], step: usize) -> Vec<f32> {
        let h = self.history.len();
        let mut buf = std::mem::take(&mut self.history);
        buf.extend_from_slice(x);
        let out = (0..x.len())
            .step_by(step)
            .map(|i| {
                // buf[i + h] is x[i]; taps run back from it.
                self.taps
                    .iter()
                    .enumerate()
                    .map(|(k, c)| c * buf[i + h - k])
                    .sum()
            })
            .collect();
        self.history = buf[buf.len() - h..].to_vec();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("models/DeepFilterNet3_onnx.tar.gz")
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn filter_streams_like_one_pass() {
        let x: Vec<f32> = (0..4800)
            .map(|n| ((n * 7919) % 97) as f32 / 97.0 - 0.5)
            .collect();
        let whole = Fir::new().filter(&x, 3);
        let mut f = Fir::new();
        let mut parts = f.filter(&x[..1200], 3);
        parts.extend(f.filter(&x[1200..], 3));
        assert_eq!(whole.len(), 1600);
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn resampling_keeps_a_tone() {
        // 16 kHz up to 48 kHz and back, as process() does around the model.
        let x: Vec<f32> = (0..16_000)
            .map(|n| (std::f32::consts::TAU * 1000.0 * n as f32 / 16_000.0).sin() * 0.5)
            .collect();
        let (mut up, mut down) = (Fir::new(), Fir::new());
        let mut stuffed = vec![0f32; x.len() * FACTOR];
        for (i, v) in x.iter().enumerate() {
            stuffed[i * FACTOR] = v * FACTOR as f32;
        }
        let y = down.filter(&up.filter(&stuffed, 1), FACTOR);
        assert_eq!(y.len(), x.len());
        // Past the filters' start-up, the tone comes back at its level.
        let ratio = rms(&y[1000..]) / rms(&x[1000..]);
        assert!((ratio - 1.0).abs() < 0.02, "level ratio {ratio}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // native inference
    fn removes_noise_from_a_16_khz_stream() {
        let mut e = Enhancer::new(&model(), 100.0).expect("model");
        let mut seed = 7u32;
        let input: Vec<i16> = (0..3 * 16_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (0.05 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * 32767.0) as i16
            })
            .collect();
        let out: Vec<i16> = input.chunks(1600).flat_map(|c| e.process(c)).collect();
        assert_eq!(out.len(), input.len(), "a sample out for each in");
        let f = |s: &[i16]| {
            s.iter()
                .map(|&v| f32::from(v) / 32768.0)
                .collect::<Vec<_>>()
        };
        // The last second, once the model has settled.
        let db = 20.0 * (rms(&f(&out[32_000..])) / rms(&f(&input[32_000..]))).log10();
        assert!(db < -20.0, "noise only {db:.1} dB down");
    }
}
