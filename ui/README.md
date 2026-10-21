# YAL ClearVoice UI

A minimal control/status window for the audio engine. It does **no audio**. It
talks to the engine over a local named pipe and renders telemetry. The engine is a
separate process.

In this test build the window starts the engine (`YAL ClearVoice Engine.exe
--headless`, from its own folder) and stops it when closed. In the product the
engine is a service and the window only attaches to it.

## Build

```powershell
ui\build.ps1
```

This compiles `YalClearVoice.exe` (about 20 KB) with the C# compiler from Visual
Studio / Build Tools, the same install the Rust MSVC toolchain needs. It targets .NET
Framework 4.8, which is part of Windows 10/11, so there's no .NET SDK to install and
no runtime to bundle.

Put it in one folder with the engine and the model:

```
YAL ClearVoice\
  YAL ClearVoice.exe              <- this UI, renamed (double-click this)
  YAL ClearVoice Engine.exe       <- cargo build --release --bin yalcv_cable
  DeepFilterNet3_onnx.tar.gz
```

The engine binary is the VB-CABLE test bridge: mic -> DeepFilterNet3 -> "CABLE
Input". Call apps pick "CABLE Output" as their microphone. VB-CABLE is a separate
install, for personal testing only. It is not bundled.

## What it shows and controls

- **Controls:**
  - Microphone. VB-CABLE is never offered here: recording it would loop.
  - Output. Locked to VB-CABLE when the engine reports `spk_selectable: false`.
  - Noise cancellation on/off.
- **Devices in use:** "Automatic (Detecta 201 USB)" names the microphone the
  engine picked. **Headset** names where the customer is heard. Automatic prefers
  a headset (Detecta, or any "Headset"/"Headphone" device) over the PC's own mic
  and speakers, and moves to one plugged in later. The lists refresh every ~5 s.
- **Status:** connection, noise removal (DeepFilterNet) state and latency. The
  window is for agents, so CPU and the audio-error count stay in the telemetry
  only. Noise removal reads Active, Off (switched off), Starting…, Paused (PC
  busy) or Stopped (error). In every state but Active, the voice still reaches
  the call, just not cleaned.
- **Hint line:** setup tips, or how to fix the current `notice`.
- **Live transcript** (right). The engine records both sides of the call by
  loopback and transcribes them with Sarvam AI: the customer from the headset, the
  agent from VB-CABLE. The agent starts and stops it with **Start / Stop
  transcription**. A forgotten Stop happens by itself after 10 minutes with
  nobody speaking. Audio goes to Sarvam only while it's on.
- **Saved calls.** While **Save calls** is ticked (it's on by default and
  remembered per Windows user under `HKCU\Software\YAL ClearVoice\SaveCalls`), each
  finished call is saved as a text file: summary, then transcript, named by start
  time. The folder is `%LOCALAPPDATA%\YAL ClearVoice\Transcripts`, private and not
  synced by OneDrive. The transcript is saved at Stop, and the file is rewritten
  once the summary arrives. **Previous calls** lists them, newest first, with
  search, Copy and Open folder. Nothing is deleted automatically.
- **After-call summary.** When a call ends, the window sends its transcript to the
  `[summary]` server in settings.ini (an OpenAI-compatible chat endpoint) and shows
  the notes, with a Copy button. The next call replaces both on screen. Saving is
  described above.

`settings.ini` sits next to the app. Without a `[transcription]` `api_key`, the
transcript pane says transcription is off and no audio leaves the PC.

```ini
[transcription]
api_key = <Sarvam key>
language = auto        ; or en-IN, hi-IN, ...
[summary]
url = http://<server>/v1/chat/completions
model = <model name>
api_key = <key>
```

## Protocol (the pipe contract)

Request/response, one line each, UTF-8, newline-delimited. The UI sends a command
and the engine replies with one JSON line. No audio crosses the pipe.

| Command | Reply |
|---|---|
| `poll` | telemetry |
| `list` | device list |
| `nc on` / `nc off` | telemetry |
| `mic <id>` / `mic default` | telemetry |
| `spk <id>` / `spk default` | telemetry |
| `strength <0-100>` | telemetry (accepted, but has no effect until the engine exposes the attenuation limit) |
| `transcript <call> <lines>` | transcript: the lines after the first `<lines>` of call `<call>`, or all lines if a newer call has started |
| `startcall` | telemetry; starts transcribing a new call (no-op if one is on or there's no Sarvam key) |
| `endcall` | telemetry; stops transcribing; the window then writes the summary |

Transcript (key order varies):

```json
{"type":"transcript","enabled":true,"call":3,"active":true,"total":2,
 "lines":[{"who":"customer","text":"..."},{"who":"agent","text":"..."}],
 "live":{"customer":"words being said now","agent":""},"error":""}
```

Telemetry:

```json
{"type":"telemetry","mic_connected":true,"spk_connected":true,"spk_selectable":false,
 "nc":true,"dfn":"running","notice":0,"latency_ms":53,"processing_ms":0.70,
 "cpu_pct":8,"errors":0,"strength_supported":false,
 "mic_name":"Microphone (Detecta 201 USB)","headset_name":"Speakers (Detecta 201 USB)"}
```

`notice` codes:

| Code | Meaning |
|---|---|
| 0 | none |
| 1 | VB-CABLE not installed |
| 2 | model file missing |
| 3 | Windows' default speaker is VB-CABLE |

Device list:

```json
{"type":"devices",
 "inputs":[{"id":"...","name":"Headset (USBC Headset)","default":true}],
 "outputs":[{"id":"...","name":"Headset (USBC Headset)","default":true}]}
```

## Not included (deliberately minimal)

- Tray icon (window only for now).
- Pipe ACL hardening. The engine creates the pipe with default security (current
  user). Add an explicit SDDL before shipping.
- No strength control. A slider was removed because it never had an effect: the
  engine doesn't expose DeepFilterNet's attenuation limit. The `strength` command
  is still accepted and still does nothing.
- Engine auto-restart. If it crashes, the window says so; reopen the app.
