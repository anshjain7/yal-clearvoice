//! Pass-through: microphone -> WASAPI capture -> ring -> WASAPI playback -> speaker.
//! No processing. Use a headset: an open microphone next to speakers will howl.
//! A device that goes away is reopened once it's back (or once there's a new default).
//!
//! cargo run --release --example passthrough -- [seconds=10] [mic-id] [speaker-id]
//! (IDs come from `cargo test lists_devices -- --nocapture`; omitted = Windows default.)

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clearvoice::capture::Mic;
use clearvoice::devices::SelectError;
use clearvoice::playback::Speaker;
use clearvoice::ring;
use clearvoice::stream::{Stats, supervise};

/// How often to retry a missing device.
const RETRY: Duration = Duration::from_millis(500);

type Outcome = thread::Result<(Stats, u32, Option<SelectError>)>;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let (mic_id, speaker_id) = (args.next(), args.next());

    // 80 ms of slack. The speaker keeps it near empty, so it adds no latency.
    let (mut tx, mut rx) = ring::new(8);
    let (mic_stop, speaker_stop) = (AtomicBool::new(false), AtomicBool::new(false));
    // Each (re)opened device reports its name.
    let (opened, opened_rx) = mpsc::channel();

    let (mic, speaker): (Outcome, Outcome) = thread::scope(|s| {
        let speaker = s.spawn(|| {
            supervise(
                &speaker_stop,
                RETRY,
                || {
                    let speaker = Speaker::open(speaker_id.as_deref())?;
                    let _ = opened.send(("speaker", speaker.device.name.clone()));
                    Ok(speaker)
                },
                |speaker| {
                    // Audio queued while the speaker was away is stale: it
                    // would only add latency.
                    while rx.pop().is_some() {}
                    let result = speaker.run(&mut rx, &speaker_stop);
                    (speaker.stats, result)
                },
            )
        });
        let show = |(side, name): (&str, String)| println!("{side:<8} {name}");
        // Speaker first: frames captured before playback starts would stay
        // queued in the ring for the whole run, as extra latency.
        if let Ok(event) = opened_rx.recv_timeout(Duration::from_secs(2)) {
            show(event);
        }
        let mic = s.spawn(|| {
            supervise(
                &mic_stop,
                RETRY,
                || {
                    let mic = Mic::open(mic_id.as_deref())?;
                    let _ = opened.send(("mic", mic.device.name.clone()));
                    Ok(mic)
                },
                |mic| {
                    let result = mic.run(&mut tx, &mic_stop, None);
                    (mic.stats, result)
                },
            )
        });

        println!("passing audio through for {seconds} s...");
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            opened_rx.try_iter().for_each(show);
            thread::sleep(Duration::from_millis(100));
        }
        // Both before joining either: a mic still running while the speaker shuts
        // down overflows the ring, and a speaker still running on a stopped mic
        // plays gaps. Speaker first, so its last look at the ring finds audio.
        speaker_stop.store(true, Relaxed);
        mic_stop.store(true, Relaxed);
        (mic.join(), speaker.join())
    });

    let (Ok((mic, mic_failures, mic_error)), Ok((speaker, speaker_failures, speaker_error))) =
        (mic, speaker)
    else {
        eprintln!("a stream thread panicked");
        return ExitCode::FAILURE;
    };
    println!(
        "mic      {} frames, {} dropped (ring full), {} glitches, max {:.3} ms per wakeup, \
         {mic_failures} failures{}",
        mic.frames,
        mic.dropped,
        mic.glitches,
        ms(mic.busy_max),
        last(mic_error),
    );
    println!(
        "speaker  {} frames, {} gaps (ran dry), {} skipped (stale), {} drift-corrected, \
         max {:.3} ms per wakeup, {speaker_failures} failures{}",
        speaker.frames,
        speaker.underruns,
        speaker.skipped,
        speaker.resampled,
        ms(speaker.busy_max),
        last(speaker_error),
    );
    let average = speaker.latency_total / speaker.frames.max(1) as u32;
    println!(
        "latency  capture to speaker avg {:.1} ms, max {:.1} ms; about {:.0} ms end to end \
         with the 10 ms frame fill, plus the hardware's own converter latency",
        ms(average),
        ms(speaker.latency_max),
        ms(average + Duration::from_millis(10)),
    );
    if mic.frames > 0 && speaker.frames > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn last(error: Option<SelectError>) -> String {
    error.map_or(String::new(), |e| format!(" (last: {e:?})"))
}
