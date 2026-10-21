//! YAL ClearVoice engine, VB-CABLE mode: microphone -> DeepFilterNet3 -> VB-CABLE,
//! so your cleaned voice reaches Zoom/Teams/Meet (choose "CABLE Output" as the
//! microphone there). The YAL ClearVoice window starts this with --headless and
//! controls it over the local control pipe; double-clicked on its own, it runs in
//! a console until you press Enter.
//!
//! This is the test bridge, not the final product: it uses the third-party
//! VB-CABLE driver instead of our own virtual microphone. The model file
//! (DeepFilterNet3_onnx.tar.gz) is read from the same folder as this program.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clearvoice::capture::Mic;
use clearvoice::denoise::{self, Control, DeepFilter};
use clearvoice::devices::{self, Device, Direction, SelectError};
use clearvoice::ipc::{self, TELEMETRY, UiCommand};
use clearvoice::playback::Speaker;
use clearvoice::record;
use clearvoice::ring;
use clearvoice::stream::{self, supervise};
use clearvoice::transcribe::{self, Side};

const RETRY: Duration = Duration::from_millis(500);

/// A file next to this executable: the model, settings.ini.
fn beside_exe(name: &str) -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_default();
    p.pop();
    p.push(name);
    p
}

/// Devices that must never be recorded as the real microphone: recording from
/// VB-CABLE (or our own endpoint) would feed the engine its own output.
fn is_virtual(name: &str) -> bool {
    ["VB-Audio", "CABLE", "YAL ClearVoice", "SoliCall"]
        .iter()
        .any(|v| name.contains(v))
}

/// VB-CABLE's input endpoint (what we play the cleaned voice into), by name.
fn cable_input() -> Option<String> {
    devices::list()
        .ok()?
        .into_iter()
        .find(|d| d.direction == Direction::Output && d.name.contains("CABLE Input"))
        .map(|d| d.id)
}

/// VB-CABLE's output endpoint (what the calling app records), by name.
fn cable_output() -> Option<String> {
    devices::list()
        .ok()?
        .into_iter()
        .find(|d| d.direction == Direction::Input && d.name.contains("CABLE Output"))
        .map(|d| d.id)
}

/// Headsets, by name: agents wear one (the call center's are Detecta), so it
/// beats the PC's own microphone and speakers whatever Windows' defaults say.
const HEADSETS: [&str; 4] = ["Detecta", "Headset", "Headphone", "Earphone"];

/// A real (not virtual) device: the requested one if it's real and present,
/// else a headset (Windows' default one first), else Windows' default, else
/// any real one.
fn choose_real(direction: Direction, requested: Option<&str>) -> Option<Device> {
    let mut real: Vec<_> = devices::list()
        .ok()?
        .into_iter()
        .filter(|d| d.direction == direction && !is_virtual(&d.name))
        .collect();
    if let Some(id) = requested
        && let Some(i) = real.iter().position(|d| d.id.eq_ignore_ascii_case(id))
    {
        return Some(real.swap_remove(i));
    }
    let headset = |d: &Device| HEADSETS.iter().any(|h| d.name.contains(h));
    real.into_iter()
        .max_by_key(|d| (headset(d), d.default_comms, d.default))
}

/// The real microphone to record.
fn choose_mic(requested: Option<&str>) -> Option<String> {
    choose_real(Direction::Input, requested).map(|d| d.id)
}

/// The agent's real headset or speaker: where the customer is heard.
fn agent_speaker() -> Option<String> {
    choose_real(Direction::Output, None).map(|d| d.id)
}

/// True when Windows' default speaker is VB-CABLE: the user then hears nothing
/// and the PC's sounds leak into the call.
fn cable_is_default_speaker() -> bool {
    devices::list().is_ok_and(|list| {
        list.iter().any(|d| {
            d.direction == Direction::Output
                && (d.default || d.default_comms)
                && d.name.contains("VB-Audio")
        })
    })
}

/// Claims a named mutex for the life of the process; false if another engine
/// already holds it.
fn first_instance() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    // SAFETY: creates (or opens) a named mutex. The handle is deliberately left
    // open until the process exits, so the name stays claimed.
    unsafe {
        CreateMutexW(None, true, windows::core::w!("Local\\YalClearVoiceEngine")).is_ok()
            && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn wait_enter() {
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn main() {
    let headless = std::env::args().any(|a| a == "--headless");
    // Console runs explain a problem and wait; the window shows notices instead.
    let say = |msg: &str| {
        if !headless {
            eprintln!("{msg}");
            println!("Press Enter to close.");
            wait_enter();
        }
    };

    if !first_instance() {
        say("YAL ClearVoice is already running.");
        return;
    }
    // Windowless, this would otherwise be a background process Windows slows.
    stream::keep_full_speed();

    // From here on the window can read status and send commands.
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>();
    thread::spawn(move || ipc::serve(cmd_tx));
    TELEMETRY.spk_selectable.store(false, Relaxed);

    // VB-CABLE has to exist; the window shows a notice until it does.
    let cable = loop {
        if let Some(id) = cable_input() {
            break id;
        }
        TELEMETRY.notice.store(ipc::NOTICE_NO_CABLE, Relaxed);
        if !headless {
            say(
                "VB-CABLE isn't installed. Get it from https://vb-audio.com/Cable/, \
                 run the installer, reboot, then start this again.",
            );
            return;
        }
        cmd_rx.try_iter().for_each(drop);
        thread::sleep(Duration::from_secs(2));
    };
    TELEMETRY.notice.store(ipc::NOTICE_NONE, Relaxed);

    let (models, models_rx) = mpsc::channel();
    match DeepFilter::new(&beside_exe("DeepFilterNet3_onnx.tar.gz")) {
        Ok(model) => {
            let _ = models.send(model);
        }
        Err(e) => {
            TELEMETRY.notice.store(ipc::NOTICE_NO_MODEL, Relaxed);
            say(&format!(
                "Couldn't load the model ({e:#}). Keep DeepFilterNet3_onnx.tar.gz \
                 next to this program."
            ));
            if headless {
                // Keep serving the notice until the window closes us.
                loop {
                    thread::sleep(Duration::from_secs(3600));
                }
            }
            return;
        }
    }
    drop(models);

    // Live transcription runs only when settings.ini has a Sarvam key.
    let transcription = std::fs::read_to_string(beside_exe("settings.ini"))
        .ok()
        .and_then(|ini| transcribe::Settings::parse(&ini));
    if transcription.is_some() {
        transcribe::enable();
    }

    if !headless {
        println!("YAL ClearVoice is running.");
        println!(
            "In Zoom / Teams / Meet, choose \"CABLE Output (VB-Audio Virtual Cable)\" as your microphone."
        );
        println!("Press Enter here to stop.");
    }

    let requested_mic: Mutex<Option<String>> = Mutex::new(None);
    let mic_in_use = Mutex::new(String::new());
    let mic_reopen = AtomicBool::new(false);
    // 160 ms each: room for noise removal to catch up after a stall without
    // the microphone dropping frames, and for the send path's cushion.
    let (mut raw_tx, mut raw_rx) = ring::new(16);
    let (mut clean_tx, mut clean_rx) = ring::new(16);
    let (mic_stop, spk_stop, listen_stop, quit) = (
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    );
    let control = Control::default();

    thread::scope(|s| {
        let control = &control;
        let df = s.spawn(move || denoise::run(models_rx, &mut raw_rx, &mut clean_tx, control));
        let spk = s.spawn(|| {
            supervise(
                &spk_stop,
                RETRY,
                || {
                    let sp = Speaker::open(Some(cable.as_str()))?;
                    TELEMETRY.spk_connected.store(true, Relaxed);
                    Ok(sp)
                },
                |sp| {
                    while clean_rx.pop().is_some() {}
                    let result = sp.run(&mut clean_rx, &spk_stop);
                    if result.is_err() {
                        TELEMETRY.spk_connected.store(false, Relaxed);
                    }
                    (sp.stats, result)
                },
            )
        });
        let mic = s.spawn(|| {
            supervise(
                &mic_stop,
                RETRY,
                || {
                    let wanted = requested_mic.lock().unwrap().clone();
                    let id = choose_mic(wanted.as_deref()).ok_or(SelectError::NoDevice)?;
                    let m = Mic::open(Some(id.as_str()))?;
                    TELEMETRY.mic_connected.store(true, Relaxed);
                    TELEMETRY.set_mic_name(&m.device.name);
                    *mic_in_use.lock().unwrap() = id;
                    Ok(m)
                },
                |m| {
                    let result = m.run(&mut raw_tx, &mic_stop, Some(&mic_reopen));
                    if result.is_err() {
                        TELEMETRY.mic_connected.store(false, Relaxed);
                    }
                    (m.stats, result)
                },
            )
        });
        // Both sides of the call, each recorded by loopback and relayed to Sarvam:
        // the customer from the agent's headset, the agent from VB-CABLE (their
        // cleaned voice). Off the audio path: nothing here touches the rings.
        if let Some(settings) = &transcription {
            let listen_stop = &listen_stop;
            let customer = agent_speaker as fn() -> Option<String>;
            for (side, endpoint) in [(Side::Customer, customer), (Side::Agent, cable_input)] {
                let (tx, rx) = mpsc::channel();
                s.spawn(move || transcribe::listen(endpoint, tx, listen_stop));
                s.spawn(move || transcribe::relay(side, settings, rx));
            }
        }
        // Call recordings, when the window asks: the customer (the agent's
        // speaker) and the agent (CABLE Output) in one file, off the audio path.
        let (rec_tx, rec_rx) = mpsc::channel();
        let sources = record::Sources {
            customer: agent_speaker,
            agent: cable_output,
        };
        s.spawn(move || record::run(rec_rx, &sources));
        if !headless {
            s.spawn(|| {
                wait_enter();
                quit.store(true, Relaxed);
            });
        }

        // Apply the window's commands. Every few seconds: check Windows hasn't
        // made VB-CABLE the default speaker, move an automatic mic to a headset
        // plugged in since, and tell the window which headset the customer is on.
        let mut next_check = Instant::now();
        while !quit.load(Relaxed) {
            for c in cmd_rx.try_iter() {
                match c {
                    UiCommand::Nc(on) => control.enabled.store(on, Relaxed),
                    UiCommand::SetMic(id) => {
                        *requested_mic.lock().unwrap() = id;
                        mic_reopen.store(true, Relaxed);
                    }
                    UiCommand::SetSpk(_) => {} // fixed to VB-CABLE in this mode
                    UiCommand::Record(c) => {
                        let _ = rec_tx.send(c);
                    }
                }
            }
            TELEMETRY
                .nc_on
                .store(control.enabled.load(Relaxed), Relaxed);
            if Instant::now() >= next_check {
                next_check = Instant::now() + Duration::from_secs(3);
                let notice = if cable_is_default_speaker() {
                    ipc::NOTICE_CABLE_IS_SPEAKER
                } else {
                    ipc::NOTICE_NONE
                };
                TELEMETRY.notice.store(notice, Relaxed);
                if TELEMETRY.mic_connected.load(Relaxed)
                    && requested_mic.lock().unwrap().is_none()
                    && choose_mic(None)
                        .is_some_and(|id| !id.eq_ignore_ascii_case(&mic_in_use.lock().unwrap()))
                {
                    mic_reopen.store(true, Relaxed);
                }
                let headset = choose_real(Direction::Output, None).map(|d| d.name);
                TELEMETRY.set_headset_name(headset.as_deref().unwrap_or(""));
            }
            thread::sleep(Duration::from_millis(100));
        }
        drop(rec_tx); // the recorder finishes any recording and exits
        listen_stop.store(true, Relaxed);
        spk_stop.store(true, Relaxed);
        control.stop.store(true, Relaxed);
        mic_stop.store(true, Relaxed);
        let _ = (mic.join(), df.join(), spk.join());
    });
    if !headless {
        println!("stopped.");
    }
}
