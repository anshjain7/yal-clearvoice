//! Hear DeepFilterNet3 on your own voice: microphone -> DeepFilterNet3 -> headphones.
//!
//! WEAR HEADPHONES. With an open microphone next to speakers this will howl.
//! No virtual devices, no Chrome — just the noise suppression, so you can test it
//! before the driver and installer exist.
//!
//! Run (plays for 20 s, or pass a number of seconds):
//!   cargo run --release --example denoise_demo
//!   cargo run --release --example denoise_demo -- 30

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use clearvoice::capture::Mic;
use clearvoice::denoise::{self, Control, DeepFilter};
use clearvoice::playback::Speaker;
use clearvoice::ring;
use clearvoice::stream::supervise;

const MODEL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/DeepFilterNet3_onnx.tar.gz"
);
const RETRY: Duration = Duration::from_millis(500);

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!("Loading DeepFilterNet3...");
    let (models, models_rx) = mpsc::channel();
    match DeepFilter::new(Path::new(MODEL)) {
        Ok(model) => {
            let _ = models.send(model);
        }
        Err(e) => eprintln!("model load failed: {e:#}"),
    }
    drop(models);

    println!("WEAR HEADPHONES. Speak now — you'll hear your denoised voice for {secs} s.");
    let (mut raw_tx, mut raw_rx) = ring::new(8);
    let (mut clean_tx, mut clean_rx) = ring::new(8);
    let (mic_stop, spk_stop) = (AtomicBool::new(false), AtomicBool::new(false));
    let control = Control::default();

    thread::scope(|s| {
        let control = &control;
        let df = s.spawn(move || denoise::run(models_rx, &mut raw_rx, &mut clean_tx, control));
        let spk = s.spawn(|| {
            supervise(
                &spk_stop,
                RETRY,
                || Speaker::open(None),
                |sp| {
                    while clean_rx.pop().is_some() {}
                    let result = sp.run(&mut clean_rx, &spk_stop);
                    (sp.stats, result)
                },
            )
        });
        let mic = s.spawn(|| {
            supervise(
                &mic_stop,
                RETRY,
                || Mic::open(None),
                |m| {
                    let result = m.run(&mut raw_tx, &mic_stop, None);
                    (m.stats, result)
                },
            )
        });

        thread::sleep(Duration::from_secs(secs));
        spk_stop.store(true, Relaxed);
        control.stop.store(true, Relaxed);
        mic_stop.store(true, Relaxed);
        let _ = (mic.join(), df.join(), spk.join());
    });
    println!("done.");
}
