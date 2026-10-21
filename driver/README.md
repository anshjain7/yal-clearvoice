# YAL ClearVoice virtual audio driver (`yalcv.sys`)

A PortCls/WaveRT kernel driver that publishes two endpoints Chrome, Edge, Windows
Sound settings and Genesys see as ordinary hardware:

- **YAL ClearVoice Microphone** (capture) — the engine writes processed audio to it
  (`IOCTL_YALCV_WRITE`); Chrome captures it.
- **YAL ClearVoice Speaker** (render) — Chrome plays the call's far end to it; the
  engine reads it (`IOCTL_YALCV_READ`) and plays it to the headset.

No DSP, files, or network in the driver — it stores samples and keeps time.

> **Status: source only.** A kernel driver can't be built, signed, or loaded in
> the engine's sandbox. Build it on a machine with the WDK and load it on a test
> PC with test-signing on (both are your steps — see below). The engine side
> (`src/vmic.rs`, `src/vspk.rs`) is built and tested; it opens `\\.\YalClearVoice`,
> retries every 500 ms until the driver is present, and streams once it is.

## What's in this folder (ours)

| File | Role |
|---|---|
| `abi/yalcv_ioctl.h` | The whole engine⇄driver contract. `src/driver.rs` mirrors it. |
| `yalcv_ring.{h,cpp}` | Lock-protected ring (one per direction): one side writes, the other reads. |
| `yalcv_control.{h,cpp}` | Secured control device + the `IOCTL_YALCV_WRITE`/`_READ` handlers. |
| `yalcv.inf` | Registers the service and names the two endpoints. |

## What you take from SimpleAudioSample (theirs)

The WaveRT miniport scaffold — the hardest, most boilerplate kernel code — is
Microsoft's, so we don't rewrite it. Clone
[Windows-driver-samples/audio/simpleaudiosample](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample)
(MS-PL) and use it as the project. It already has a render and a capture endpoint,
so keep both; make these changes:

1. **Build these files into it:** add `yalcv_ring.cpp` and `yalcv_control.cpp` to
   the `.vcxproj`, put this folder on the include path so `abi/yalcv_ioctl.h`
   resolves, and link `wdmsec.lib` (for `IoCreateDeviceSecure`).
2. **DriverEntry:** right after `PcInitializeAdapterDriver(...)` succeeds, call
   `YalcvInitControl(DriverObject);`. Keep PortCls's `DriverUnload`, and from it
   call `YalcvDeleteControl();`.
3. **Capture stream ↔ mic ring.** In the capture stream's buffer-fill path
   (`CMiniportWaveRTStream` — where the sample copies from its tone generator /
   `SaveData` reader into the WaveRT cyclic buffer), replace that source with
   `YalcvRingRead(&g_YalcvRing, dst, samplesThisCopy);`, and in its `SetState` call
   `YalcvRingSetRunning(&g_YalcvRing, state == KSSTATE_RUN);`. **Delete the sample's
   `SaveData`/WAV-writing code** — we never write audio to disk.
4. **Render stream ↔ speaker ring.** In the render stream's buffer-drain path
   (where the sample consumes the WaveRT cyclic buffer it was given), write what
   Chrome rendered into our ring: `YalcvRingWrite(&g_YalcvSpkRing, src,
   samplesThisCopy);`, and in its `SetState` call
   `YalcvRingSetRunning(&g_YalcvSpkRing, state == KSSTATE_RUN);`.

Set the formats (in the filter/endpoint tables, `minwavert.cpp` and the pin data
ranges) so no client can pick a rate that forces resampling:

- Capture pin: bridge node `KSNODETYPE_MICROPHONE`, data range **48000 Hz, 16-bit,
  1 channel**.
- Render pin: bridge node `KSNODETYPE_SPEAKER`, data range **48000 Hz, 16-bit,
  2 channels**.

## Build

Use the EWDK (self-contained) or VS 2022 + WDK.

```cmd
REM EWDK: run its LaunchBuildEnv.cmd, then from the SimpleAudioSample dir:
msbuild /p:Configuration=Release /p:Platform=x64
```

Output: `yalcv.sys`, `yalcv.inf`, and (after `inf2cat`) `yalcv.cat`.

## Test-sign and load (your test PC)

These change Windows settings, so they're yours to run on a test machine, not the
build box:

```cmd
REM One-time test certificate, then sign the driver + catalog:
inf2cat /driver:. /os:10_X64
signtool sign /fd sha256 /a /v yalcv.sys
signtool sign /fd sha256 /a /v yalcv.cat

REM Enable test-signing (Secure Boot must be OFF), then reboot:
bcdedit /set testsigning on
```

## Install

Create the root devnode once, then install the package:

```cmd
devgen /add /hardwareid "ROOT\YALCLEARVOICE"
pnputil /add-driver yalcv.inf /install
```

`devgen` ships with the WDK; the production MSI does the same with SetupAPI. On a
driver update, re-run only `pnputil /add-driver yalcv.inf /install` — the devnode
(and so the endpoint's identity) stays, so Genesys keeps the saved device.

## Test that Chrome sees them

1. **Windows:** Settings → System → Sound lists **YAL ClearVoice Microphone** under
   Input and **YAL ClearVoice Speaker** under Output. (`mmsys.cpl` shows both too.)
2. **Chrome/Edge:** open any page, grant microphone permission, then in DevTools:

   ```js
   (await navigator.mediaDevices.enumerateDevices())
     .filter(d => d.kind === 'audioinput' || d.kind === 'audiooutput')
     .forEach(d => console.log(d.kind, d.label));
   // expect: audioinput  "YAL ClearVoice Microphone (YAL ClearVoice)"
   //         audiooutput "YAL ClearVoice Speaker (YAL ClearVoice)"
   ```

3. **End to end:** run the engine (`cargo run --release`). In Genesys → WebRTC
   settings (or a test page) pick the virtual mic as input and the virtual speaker
   as output. Speak into the headset and confirm denoised audio reaches the far
   end; play audio to the virtual speaker and confirm it comes out of the headset.
   `chrome://media-internals` shows both active devices.

## Uninstall

```cmd
pnputil /remove-device "ROOT\YALCLEARVOICE\0000"   REM instance id from: pnputil /enum-devices
pnputil /delete-driver yalcv.inf /uninstall
bcdedit /set testsigning off
```
