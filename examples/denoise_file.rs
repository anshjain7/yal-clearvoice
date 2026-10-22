//! Noise removal on a recording, to hear an attenuation limit before setting it.
//! Reads a 16 or 48 kHz mono WAV, runs DeepFilterNet3 as the engine does, and
//! writes the result at the input's rate. Nothing is played or recorded.
//!
//!   cargo run --release --example denoise_file -- <in.wav> <out.wav> [attenuation_db]
//!
//! attenuation_db is settings.ini's [noise] attenuation_db: 100 removes all the
//! noise it can, lower keeps more of the original (default: the engine's).

use std::fs;
use std::path::Path;

use clearvoice::denoise::{DEFAULT_ATTENUATION_DB, DeepFilter};

const MODEL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/DeepFilterNet3_onnx.tar.gz"
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, ..] = &args[..] else {
        eprintln!("usage: denoise_file <in.wav> <out.wav> [attenuation_db]");
        std::process::exit(2);
    };
    let db = args
        .get(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ATTENUATION_DB);
    let (rate, samples) = read_wav(Path::new(input));
    let factor = match rate {
        48_000 => 1,
        16_000 => 3,
        r => panic!("{input} is {r} Hz; give 16 or 48 kHz"),
    };
    let audio = resample(&samples, factor, true);

    let mut df = DeepFilter::new(Path::new(MODEL), db).expect("model");
    let delay = df.delay_frames as usize * 480;
    let mut padded = audio.clone();
    padded.resize(audio.len() + delay + 480, 0.0);
    let mut cleaned = vec![0.0; padded.len()];
    for (i, o) in padded
        .as_chunks::<480>()
        .0
        .iter()
        .zip(cleaned.as_chunks_mut::<480>().0)
    {
        df.process(i, o).expect("process");
    }
    // Line the output up with the input (the model runs 30 ms behind).
    let cleaned = &cleaned[delay..delay + audio.len()];
    let out = resample(cleaned, factor, false);
    write_wav(Path::new(output), rate, &out);
    println!(
        "{output}: {:.2} s at {rate} Hz, attenuation limit {db} dB",
        out.len() as f32 / rate as f32
    );
}

/// Up (`up`) or down by a whole `factor`, through a windowed-sinc low-pass at
/// the lower rate's Nyquist frequency.
fn resample(x: &[f32], factor: usize, up: bool) -> Vec<f32> {
    if factor == 1 {
        return x.to_vec();
    }
    let taps = 32 * factor + 1;
    let mid = (taps / 2) as f32;
    let cutoff = 0.5 / factor as f32;
    let h: Vec<f32> = (0..taps)
        .map(|n| {
            let t = n as f32 - mid;
            let sinc = if t == 0.0 {
                2.0 * cutoff
            } else {
                (std::f32::consts::TAU * cutoff * t).sin() / (std::f32::consts::PI * t)
            };
            let window = 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / (taps - 1) as f32).cos();
            sinc * window
        })
        .collect();
    let filter = |s: &[f32], gain: f32| -> Vec<f32> {
        (0..s.len())
            .map(|i| {
                let mut acc = 0.0;
                for (k, &c) in h.iter().enumerate() {
                    let j = i as isize + k as isize - mid as isize;
                    if j >= 0 && (j as usize) < s.len() {
                        acc += c * s[j as usize];
                    }
                }
                acc * gain
            })
            .collect()
    };
    if up {
        let mut z = vec![0.0; x.len() * factor];
        for (i, &v) in x.iter().enumerate() {
            z[i * factor] = v;
        }
        filter(&z, factor as f32)
    } else {
        filter(x, 1.0).into_iter().step_by(factor).collect()
    }
}

fn read_wav(path: &Path) -> (u32, Vec<f32>) {
    let b = fs::read(path).expect("read wav");
    assert!(
        &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE",
        "not a WAV file"
    );
    let (mut at, mut rate, mut channels, mut data) = (12, 0, 0, None);
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &b[at + 8..(at + 8 + len).min(b.len())];
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes([body[2], body[3]]) as usize;
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                assert_eq!(
                    u16::from_le_bytes([body[14], body[15]]),
                    16,
                    "16-bit WAV only"
                );
            }
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    let data = data.expect("no audio in the WAV");
    let samples = data
        .chunks_exact(2 * channels)
        .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
        .collect();
    (rate, samples)
}

fn write_wav(path: &Path, rate: u32, x: &[f32]) {
    let data: Vec<u8> = x
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let mut b = Vec::with_capacity(44 + data.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    fs::write(path, b).expect("write wav");
}
