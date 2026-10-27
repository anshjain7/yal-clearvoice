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
after_call = on         ; off: no after-call pass (see below)
[summary]
url = http://<server>/v1/chat/completions
model = <model name>
api_key = <key>
[noise]                ; optional: how much noise removal takes out at most
attenuation_db = 15    ; 6..100 dB; 100 = all it can (sounds muffled with a
                       ; noise-cancelling headset like the Detecta 201)
[voice]                ; optional
remix = 0              ; how much of the original is mixed back in (0 = none;
                       ; with a voice server the default is 0.2)
threads = 2            ; for WeSep on this PC (1-8)
url = http://<server>:8090   ; only to use a voice server instead
api_key = <key>
```

The window shows **Register my voice** (a 20 s voice sample): the voice TSE
keeps. With a voice server (`[voice] url`, see `voice-server/README.md`), the
agent's side of the transcript goes through WeSep there.

On the PC the agent's voice goes through one live chain, in the team's
order (TSE as in `run_tse.sh`, then speech enhancement as in `run_se.sh`):

    microphone -> TSE -> DeepFilterNet3 (at the [noise] limit) -> VB-CABLE

What comes out goes to the calling app, so to the customer, and the
transcript listens to it too, by loopback of VB-CABLE, like the customer's
side from the headset. The TSE step needs a causal (live) target speaker
model; until one ships, it passes the voice on unchanged, so the chain is
DeepFilterNet3 alone. (WeSep's public model looks at audio after each
moment as well as before, so it can't run live without delaying the voice
by seconds.)

To try today's WeSep on a call anyway, tick **Live voice filter (test)**
(shown when the WeSep files ship with the engine; off at every start, and
it asks first). The TSE step then runs WeSep on the microphone 2 s at a
time, with 0.5 s before each piece for context, and plays the result 4 s
behind: the customer hears the agent 4 s late. A piece WeSep hasn't
finished by then is sent as silence (the window shows "PC too slow"); one
it fails on, as recorded. Unticking it goes back to the normal chain at
once. On the development PC (i5-12450H) it needs `[voice] threads = 2` or
more: real-time factor 0.67 at 2 threads, 1.05 (falling behind) at 1.

While a call is recorded, the agent's own voice is saved at each step of the
live chain, each covering the same stretch, in a folder per call,
`Recordings\Agent voice\<date time>\`:

- `input.m4a`: the microphone;
- `audio_tse.m4a`: after TSE (the microphone, while the live voice filter
  is off; with it on, WeSep's output, lined up with `input`);
- `audio_tse_se.m4a`: after DeepFilterNet3 too: what the customer and the
  transcript get;
- `rtf.csv`: each step's real-time factor (time it took / length of the
  audio; under 1 keeps up), for every 2 s and for the whole call.

After a recorded call, an after-call pass works on the agent's side the
way the team's offline setup does, where nothing is waiting: WeSep on the
call's `input`, 10 s at a time, then DeepFilterNet3, saved beside the
stages as `after_call_tse.m4a` and `after_call_tse_se.m4a` (their
real-time factors go in `rtf.csv`, what happened in `after_call.log`). The
cleaned voice, cut into utterances, then goes to Sarvam's batch service,
and the agent's new lines, merged in time with the customer's from the live
transcript, make the call's improved transcript. The window summarizes it
again: the improved summary replaces the call's note and saved copy (and
the window's view, if it still shows that call). The first summary, from
the live transcript, comes at Stop as before. `[transcription] after_call =
off` turns the pass off (it sends the agent's side to Sarvam a second time).

The live stage hands a copy of every 10 ms it sends out to a separate
thread below normal priority, without waiting; if that thread falls behind,
copies are dropped, never the call's audio.

They're written as WAV during the call and made M4A once it's over, so the
encoding takes nothing from the call; where Windows can't encode M4A ("N"
editions without the Media Feature Pack), they stay WAV.

## Protocol (the pipe contract)

Request/response, one line each, UTF-8, newline-delimited. The UI sends a command
and the engine replies with one JSON line. No audio crosses the pipe.

| Command | Reply |
|---|---|
| `poll` | telemetry |
| `list` | device list |
| `nc on` / `nc off` | telemetry |
| `tse on` / `tse off` | telemetry; the live voice filter (test) |
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

Newer engines add `voice` and `live_tse` (the live voice filter: `off`,
`on`, `late`, `failed`, or `unavailable` without the WeSep files).

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
