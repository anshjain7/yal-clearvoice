//! WeSep speaker extraction on a recording, with the app's own code (tse.rs):
//! keeps only the voice in the voice sample, as the transcript hears it.
//! Both files 16 kHz mono 16-bit. Nothing is played or recorded.
//!
//!   cargo run --release --example tse_file -- <dir with onnxruntime.dll and wesep\> \
//!       <voice sample.wav> <in.wav> <out.wav> [remix, default 0.2]

use std::path::Path;

use clearvoice::tse::{self, Extractor, Files};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir, sample, input, output, ..] = &args[..] else {
        eprintln!("usage: tse_file <dir> <voice sample.wav> <in.wav> <out.wav> [remix]");
        std::process::exit(2);
    };
    let remix: f32 = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(0.2);
    let files = Files::find(Path::new(dir)).expect("onnxruntime.dll and wesep\\ in that folder");
    let mut model = Extractor::load(&files, 2).expect("model");
    let embedding = model.enroll(&read(sample)).expect("voice sample");
    let mix = read(input);
    let est = model.extract(&embedding, &mix).expect("extraction");
    let out = tse::remix(&est, &mix, remix);
    write(output, &out);
    println!(
        "{output}: {:.2} s, remix {remix}",
        out.len() as f32 / 16_000.0
    );
}

/// A 16 kHz mono 16-bit WAV's samples, in -1..1.
fn read(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).expect("read wav");
    let (mut at, mut data) = (12, None);
    while at + 8 <= b.len() {
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        if &b[at..at + 4] == b"fmt " {
            let rate = u32::from_le_bytes(b[at + 12..at + 16].try_into().unwrap());
            let channels = u16::from_le_bytes([b[at + 10], b[at + 11]]);
            assert_eq!((rate, channels), (16_000, 1), "{path}: give 16 kHz mono");
        } else if &b[at..at + 4] == b"data" {
            data = Some(&b[at + 8..(at + 8 + len).min(b.len())]);
        }
        at += 8 + len + (len & 1);
    }
    data.expect("no audio")
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| f32::from(i16::from_le_bytes(c)) / 32768.0)
        .collect()
}

fn write(path: &str, x: &[f32]) {
    let data: Vec<u8> = x
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let mut b = Vec::with_capacity(44 + data.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&16_000u32.to_le_bytes());
    b.extend_from_slice(&32_000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    std::fs::write(path, b).expect("write wav");
}
