# YAL ClearVoice

A Windows app for call-center agents. It removes background noise from the
agent's voice on every call (DeepFilterNet3, on the PC), writes the call down as
it happens (Sarvam AI), saves a short summary note when the call ends, and can
record calls.

It runs on **Windows 10/11 only**: the audio engine uses Windows audio (WASAPI)
and the window is a .NET Framework app. The code can be read and edited on any
computer, but building and running it needs a Windows PC.

## What's where

| Folder | What it is |
|---|---|
| `src/` | The audio engine (Rust): microphone → noise removal → VB-CABLE, live transcription, call recording, and the control pipe the window talks to. `src/bin/yalcv_cable.rs` is the engine that ships. |
| `ui/` | The YAL ClearVoice window (C#, WinForms, .NET Framework 4.8). See `ui/README.md`. |
| `installer/setup/` | The one-file setup, `YAL ClearVoice Setup.exe`. |
| `models/` | The DeepFilterNet3 model the engine loads. |
| `docs/user-guide/` | The user guide for agents (HTML source and pictures). |
| `docs/client-brief/` | The page explaining the app to the client. |
| `driver/`, `installer/Package.wxs` | Our own virtual microphone driver and MSI. Not used yet: today the app uses VB-CABLE. |

## Building (on Windows)

Needs the Rust MSVC toolchain and Visual Studio Build Tools (for `csc.exe`).
Keep build output in a short path: very long exe paths break the microphone.

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\cvvmic"
cargo +stable-x86_64-pc-windows-msvc test --release --lib --bins
cargo +stable-x86_64-pc-windows-msvc build --release --bin yalcv_cable   # the engine
ui\build.ps1                                                            # the window
installer\setup\build.ps1                                               # the setup
```

`installer/setup/build.ps1` needs a `settings.ini` (by default
`%LOCALAPPDATA%\YAL ClearVoice\setup-settings.ini`) and the VB-CABLE driver pack
(by default `Desktop\VBCABLE_Driver_Pack45`). See the top of that script.

## Keys

The Sarvam and summary-server keys live only in `settings.ini` next to the
installed app; the format is in `ui/README.md`. They are never in this
repository, and `.gitignore` keeps `settings.ini` and built setups (which embed
it) out of Git.
