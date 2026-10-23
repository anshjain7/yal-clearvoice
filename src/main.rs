//! YAL ClearVoice. Send path: microphone -> WASAPI capture -> DeepFilterNet3 ->
//! YAL ClearVoice virtual microphone, which Chrome/Edge and Genesys capture from.
//! Return path: YAL ClearVoice virtual speaker (what Genesys plays) -> headset.
//! If DeepFilterNet3 can't load or fails, audio keeps flowing in bypass. If the
//! virtual-audio driver isn't installed yet, the engine retries until it is.
//!
//! cargo run --release -- [seconds, 0 = until Enter] [mic-id]
//! (IDs come from `cargo test lists_devices -- --nocapture`; omitted = Windows default.)
//! While running: `b` + Enter toggles DeepFilterNet, `r` + Enter restarts it,
//! Enter alone stops.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clearvoice::capture::Mic;
use clearvoice::denoise::{self, Control, DeepFilter};
use clearvoice::devices::{self, Direction};
use clearvoice::format::AudioFormat;
use clearvoice::ipc::{self, UiCommand};
use clearvoice::ring;
use clearvoice::stream::supervise;
use clearvoice::vmic::Vmic;
use clearvoice::vspk::Vspk;

/// Diagnostics for a (re)opened endpoint, printed as it comes up.
struct Opened {
    /// 0 mic, 1 virtual mic, 2 virtual speaker -> headset.
    side: usize,
    name: String,
    sample_rate: u32,
    channels: u16,
    /// Endpoint buffer in frames (0 for the driver endpoint, which has no WASAPI buffer).
    buffer_frames: u32,
}

/// How often to retry a missing device or driver.
const RETRY: Duration = Duration::from_millis(500);
// ponytail: model path fixed at build time; read it from config once there's an installer.
const MODEL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/DeepFilterNet3_onnx.tar.gz"
);

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    // Chosen devices: an explicit ID, or None to follow the Windows default. The
    // UI changes these over IPC; the default-follow poll applies only while None.
    let requested_mic = Mutex::new(args.next());
    let requested_out: Mutex<Option<String>> = Mutex::new(None);

    // Capture -> DeepFilterNet and DeepFilterNet -> virtual mic. 80 ms of slack
    // each; the consumers keep them near empty, so they add no latency.
    let (mut raw_tx, mut raw_rx) = ring::new(8);
    let (mut clean_tx, mut clean_rx) = ring::new(8);
    let (mic_stop, vmic_stop, vspk_stop, quit) = (
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    );
    // Set to make the mic/speaker reopen: a default-device change, or a device
    // picked in the UI.
    let (mic_default, out_default) = (AtomicBool::new(false), AtomicBool::new(false));
    let control = Control::default();
    let (models, models_rx) = mpsc::channel();
    // Each (re)opened endpoint, for the diagnostics line.
    let (opened, opened_rx) = mpsc::channel::<Opened>();
    // UI commands from the IPC thread, applied by the main loop.
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>();
    thread::spawn(move || ipc::serve(cmd_tx));
    let run_start = Instant::now();
    let cpu_start = ipc::process_cpu();

    thread::scope(|s| {
        let (control, quit, models) = (&control, &quit, &models);
        // Loads DeepFilterNet3 off the audio threads (about 1 s) and hands it
        // to the stage, which bypasses until then. A restart is another load.
        let load = move || {
            let models = models.clone();
            s.spawn(move || {
                match DeepFilter::new(Path::new(MODEL), denoise::DEFAULT_ATTENUATION_DB) {
                    Ok(model) => {
                        let _ = models.send(model);
                    }
                    Err(e) => eprintln!("model    DeepFilterNet3 failed to load: {e:#}"),
                }
            })
        };
        // Start in pipeline order so nothing queues up as latency: the model,
        // the stage, the virtual mic, then the microphone.
        let _ = load().join();
        let df = s.spawn(move || denoise::run(models_rx, &mut raw_rx, &mut clean_tx, control));
        let vmic = s.spawn(|| {
            supervise(
                &vmic_stop,
                RETRY,
                || {
                    let vmic = Vmic::open()?;
                    let _ = opened.send(Opened {
                        side: 1,
                        name: "YAL ClearVoice Microphone".into(),
                        sample_rate: AudioFormat::MIC.sample_rate,
                        channels: AudioFormat::MIC.channels,
                        buffer_frames: 0,
                    });
                    Ok(vmic)
                },
                |vmic| {
                    // Audio queued while the driver was away is stale.
                    while clean_rx.pop().is_some() {}
                    let result = vmic.run(&mut clean_rx, &vmic_stop);
                    (vmic.stats, result)
                },
            )
        });
        // Return path: play the virtual speaker (what Genesys renders) to the
        // headset. Independent of the send path; needs only the driver and headset.
        let vspk = s.spawn(|| {
            supervise(
                &vspk_stop,
                RETRY,
                || {
                    let id = requested_out.lock().unwrap().clone();
                    let vspk = Vspk::open(id.as_deref())?;
                    ipc::TELEMETRY.spk_connected.store(true, Relaxed);
                    let _ = opened.send(Opened {
                        side: 2,
                        name: format!("YAL ClearVoice Speaker -> {}", vspk.device.name),
                        sample_rate: vspk.format.sample_rate,
                        channels: vspk.format.channels,
                        buffer_frames: vspk.buffer_frames,
                    });
                    Ok(vspk)
                },
                |vspk| {
                    let result = vspk.run(&vspk_stop, Some(&out_default));
                    if result.is_err() {
                        ipc::TELEMETRY.spk_connected.store(false, Relaxed);
                    }
                    (vspk.stats, result)
                },
            )
        });
        let mut show = |o: Opened| {
            let buf = if o.buffer_frames > 0 {
                format!(
                    ", buffer {} frames ({:.0} ms)",
                    o.buffer_frames,
                    o.buffer_frames as f64 * 1000.0 / o.sample_rate as f64
                )
            } else {
                String::new()
            };
            println!(
                "{:<8} {} ({} Hz, {} ch{})",
                ["mic", "vmic", "vspk"][o.side],
                o.name,
                o.sample_rate,
                o.channels,
                buf
            );
        };
        if let Ok(event) = opened_rx.recv_timeout(Duration::from_secs(2)) {
            show(event);
        }
        let mic = s.spawn(|| {
            supervise(
                &mic_stop,
                RETRY,
                || {
                    let id = requested_mic.lock().unwrap().clone();
                    let mic = Mic::open(id.as_deref())?;
                    ipc::TELEMETRY.mic_connected.store(true, Relaxed);
                    let _ = opened.send(Opened {
                        side: 0,
                        name: mic.device.name.clone(),
                        sample_rate: mic.format.sample_rate,
                        channels: mic.format.channels,
                        buffer_frames: mic.buffer_frames,
                    });
                    Ok(mic)
                },
                |mic| {
                    let result = mic.run(&mut raw_tx, &mic_stop, Some(&mic_default));
                    if result.is_err() {
                        ipc::TELEMETRY.mic_connected.store(false, Relaxed);
                    }
                    (mic.stats, result)
                },
            )
        });

        let deadline = Instant::now() + Duration::from_secs(seconds);
        if seconds == 0 {
            println!(
                "running; b + Enter toggles DeepFilterNet, r + Enter restarts it, Enter stops"
            );
            s.spawn(move || {
                for line in std::io::stdin().lines() {
                    match line.as_deref().map(str::trim) {
                        Ok("b") => {
                            let on = !control.enabled.fetch_xor(true, Relaxed);
                            println!("model    {}", if on { "on" } else { "bypassed" });
                        }
                        Ok("r") => {
                            println!("model    restarting DeepFilterNet3");
                            load();
                        }
                        _ => break,
                    }
                }
                quit.store(true, Relaxed);
            });
        } else {
            println!("running for {seconds} s");
        }
        let mut active = false;
        // Poll the default devices about once a second (every tenth 100 ms tick).
        let (mut last_in, mut last_out, mut poll) = (None::<String>, None::<String>, 0u32);
        while !quit.load(Relaxed) && (seconds == 0 || Instant::now() < deadline) {
            opened_rx.try_iter().for_each(&mut show);
            // Apply commands from the UI (IPC thread).
            for c in cmd_rx.try_iter() {
                match c {
                    UiCommand::Nc(on) => control.enabled.store(on, Relaxed),
                    UiCommand::SetMic(id) => {
                        *requested_mic.lock().unwrap() = id;
                        mic_default.store(true, Relaxed);
                    }
                    UiCommand::SetSpk(id) => {
                        *requested_out.lock().unwrap() = id;
                        out_default.store(true, Relaxed);
                    }
                    // Call recordings exist in the VB-CABLE build only.
                    UiCommand::Record(_) | UiCommand::RecordVoice | UiCommand::ImportVoice(_) => {}
                }
            }
            ipc::TELEMETRY
                .nc_on
                .store(control.enabled.load(Relaxed), Relaxed);
            if control.active.load(Relaxed) != active {
                active = !active;
                println!(
                    "model    {}",
                    if active {
                        "DeepFilterNet3 running"
                    } else {
                        "DeepFilterNet3 failed; bypassing (r + Enter restarts it)"
                    }
                );
            }
            poll += 1;
            if poll >= 10 {
                poll = 0;
                // Follow the Windows default only while the UI hasn't pinned a device.
                if requested_mic.lock().unwrap().is_none() {
                    watch_default(Direction::Input, &mut last_in, &mic_default);
                }
                if requested_out.lock().unwrap().is_none() {
                    watch_default(Direction::Output, &mut last_out, &out_default);
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        // Outputs first, so they stop while audio is still flowing instead of
        // writing gaps behind a stopped source.
        vmic_stop.store(true, Relaxed);
        vspk_stop.store(true, Relaxed);
        control.stop.store(true, Relaxed);
        mic_stop.store(true, Relaxed);
        let (
            Ok((mic, mic_failures, _)),
            Ok(df),
            Ok((vm, vmic_failures, _)),
            Ok((sp, vspk_failures, _)),
        ) = (mic.join(), df.join(), vmic.join(), vspk.join())
        else {
            eprintln!("an audio thread panicked");
            return ExitCode::FAILURE;
        };

        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        println!(
            "mic      {} frames, {} dropped (ring full), {} glitches, {mic_failures} reopens",
            mic.frames, mic.dropped, mic.glitches
        );
        println!(
            "denoise  {} frames ({} bypassed), avg {:.2} ms, max {:.2} ms per frame, \
             {} errors, {} model loads{}, {} dropped (ring full)",
            df.frames,
            df.bypassed,
            ms(df.busy_total) / df.frames.max(1) as f64,
            ms(df.busy_max),
            df.errors,
            df.loads,
            match (df.failed, df.overloaded) {
                (true, true) => ", failed over and overloaded to bypass",
                (true, false) => ", failed over to bypass",
                (false, true) => ", overloaded to bypass",
                (false, false) => "",
            },
            df.dropped
        );
        println!(
            "vmic     {} frames, {} dropped (driver ring full), {} gaps (ran dry), \
             {} skipped (stale), {} drift-corrected, {vmic_failures} reopens",
            vm.frames, vm.dropped, vm.underruns, vm.skipped, vm.resampled
        );
        println!(
            "vspk     {} frames played, {} dropped (fell behind), {} gaps (ran dry), \
             {} drift-corrected, {vspk_failures} reopens",
            sp.frames, sp.dropped, sp.underruns, sp.resampled
        );
        let avg = |total: Duration, n: u64| ms(total) / n.max(1) as f64;
        let end_to_end = avg(vm.latency_total, vm.frames);
        println!(
            "latency  capture     avg {:.1} ms, max {:.1} ms (sample recorded -> frame handed on, incl. 10 ms fill)\n\
             \x20        denoise     {} ms model delay + avg {:.1} ms, max {:.1} ms processing\n\
             \x20        virtual mic avg {:.1} ms, max {:.1} ms queued in the driver ring\n\
             \x20        send total  avg {end_to_end:.1} ms, max {:.1} ms measured (recorded -> handed to the driver)\n\
             \x20        return path avg {:.1} ms, max {:.1} ms queued to the headset (virtual speaker -> headset)",
            avg(mic.latency_total, mic.frames),
            ms(mic.latency_max),
            df.delay_frames * 10,
            avg(df.busy_total, df.frames),
            ms(df.busy_max),
            avg(vm.queue_total, vm.frames),
            ms(vm.queue_max),
            ms(vm.latency_max),
            avg(sp.queue_total, sp.frames),
            ms(sp.queue_max),
        );
        let cpu = ipc::process_cpu().saturating_sub(cpu_start);
        let wall = run_start.elapsed().as_secs_f64().max(0.001);
        println!(
            "cpu      {:.2} s process CPU over {:.1} s = {:.1}% of one core (all threads)",
            cpu.as_secs_f64(),
            wall,
            100.0 * cpu.as_secs_f64() / wall
        );
        if mic.frames > 0 && df.frames > 0 && vm.frames > 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    })
}

/// Flags `flag` when the default communications device for `dir` has changed
/// since the last poll. Uses `select`, so a default that points at one of
/// ClearVoice's own virtual endpoints is ignored and the stream stays put.
fn watch_default(dir: Direction, last: &mut Option<String>, flag: &AtomicBool) {
    if let Ok(device) = devices::select(dir, None) {
        if last.as_ref().is_some_and(|prev| prev != &device.id) {
            flag.store(true, Relaxed);
        }
        *last = Some(device.id);
    }
}
