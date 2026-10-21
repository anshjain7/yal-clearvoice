# YAL ClearVoice — MVP Test Report

**Date:** 2026-10-08
**Scope:** audio compatibility of the full bidirectional MVP
(mic → WASAPI → DFN3 → virtual mic → Chrome → Genesys; Genesys → Chrome →
virtual speaker → headset). No transcription, analytics, CRM, or cloud processing.

## Honest execution status

The end-to-end MVP **could not be run** for this report. Two hard blockers:

1. **The kernel driver `yalcv.sys` is not built or installed.** It can't be built
   (no WDK) or loaded (unsigned kernel driver needs test-signing on a test PC) in
   the engine's environment. Confirmed live: Chrome on this machine enumerates no
   "YAL ClearVoice" endpoint, so there is nothing for Chrome or Genesys to select.
2. **Genesys can't be driven here.** Signing into the org is authenticating as the
   user, and placing a call transmits live audio externally — both are the user's
   actions on a test PC, not automatable here.

Nothing below is a fabricated live result. Each row is marked with how it was
verified, or why it couldn't be. Legend:

- ✅ **Verified** — a test was actually run (this or a prior session; provenance noted).
- 🧩 **Code-complete, unit-verified** — the logic is written and its unit tests pass,
  but the live end-to-end path is unverified (needs the driver / Genesys).
- ⛔ **Not runnable here** — needs the installed driver and/or a Genesys call.
- ⚠️ **Known issue** — a real gap, described in Issues.

What was verified **this session** (directly): `cargo check --all-targets`,
`clippy -D warnings`, and `rustfmt --check` all clean; 23 logic tests pass
(`devices`, `driver`, `vmic`, `vspk`, `ring`, `format`, `frame`, `stream`);
the virtual-mic/speaker "missing driver → Disconnected → retry" behavior; and the
live Chrome device enumeration above.

What was verified in **earlier sessions** (per their results) — all on the **core
path** (mic → DFN3 → *physical* speaker), **not** through the virtual devices:
the 15-minute benchmark (89,918 frames, 0 dropped, 2 speaker gaps, avg 50.3 ms /
max 61.4 ms latency, 10.9% of one core); the 30-test suite; DFN3 failover tests;
the ring under Miri + stress; and the 10-minute pass-through stress with device
disconnect/reconnect.

## Verification matrix

| # | Scenario | Status | Basis / predicted behavior |
|---|----------|--------|----------------------------|
| 1 | **Microphone** path | 🧩 / ⛔ | Physical capture + DFN3 verified on the core path (benchmark). The `vmic` IOCTL-write to a live driver, and Chrome/Genesys capture, are unverified — no driver. |
| 2 | **Speaker** path | 🧩 / ⛔ | Headset playback logic is the verified `Speaker` pump reused in `vspk`. The driver-read (`IOCTL_YALCV_READ`) and Chrome render are unverified — no driver. |
| 3 | **Device selection** | ✅ / ⚠️ | `devices` tests pass (default/explicit/unknown/disconnected). Own-endpoint exclusion is in place but **name-based** (⚠️). Genesys-side selection unverified. |
| 4 | **Startup** | ✅ / 🧩 | Core startup verified (benchmark started clean). New 4-thread startup compiles; missing-driver retry unit-tested. Full startup with driver unverified. |
| 5 | **Shutdown** | ✅ / ⚠️ | Core shutdown verified (benchmark exited 0). New order stops outputs first. ⚠️ stop cuts the last ≤22 ms; no Ctrl+C/console-close/logoff handling. |
| 6 | **Chrome restart** | ⛔ | Needs driver + Chrome. Predicted: driver reports stream-stop → engine keeps endpoints alive, resumes on reopen. Depends on the driver's Run/Stop hooks — unverified. |
| 7 | **Genesys restart** | ⛔ | Same mechanism as Chrome restart (Genesys is a Chrome tab). Unverified. |
| 8 | **Mic disconnect/reconnect** | ✅ / 🧩 | `supervise` unit-tested; core pass-through survived disconnect/reconnect (prior stress). The physical mic reopens the same way here. Through the virtual chain, unverified. |
| 9 | **Speaker disconnect/reconnect** | ✅ / 🧩 | Physical headset (the `vspk` output) reopens via the same supervised playback verified on the core path. Virtual chain unverified. |
| 10 | **Windows default device changes** | ⚠️ | **Not followed.** A stream stays on the device it opened until that device fails. Known gap; not fixed. |
| 11 | **DeepFilterNet failure** | ✅ | Failover verified (prior session): panic caught, 5-failure give-up, bypass with delay-matched crossfade, background restart. Unit tests cover load/NaN/panic/restart. |
| 12 | **Long-running calls** | ✅ / 🧩 | Core path stable for 15 min (0 drops, 2 gaps). Through the virtual devices unverified; mic→driver drift correction is coded (reused `fit`) but not yet measured live. |

## Issues found (ranked)

1. **Blocker — not end-to-end runnable here:** no built/installed driver; Genesys
   login + live call off-limits. The whole matrix above is gated on a test PC with
   the driver installed and a Genesys org.
2. **Latency (~50 ms floor)** with the standard DFN3 model: 10 ms fill + 30 ms model
   delay + ~10 ms driver cushion + ~10 ms Chrome/endpoint hop. The low-latency model
   (~30 ms) is blocked by the hard-coded `DELAY_FRAMES = 3` in `denoise.rs`.
   Affects scenario 12 / "latency acceptable".
3. **No automatic overload → bypass:** if DFN3 exceeds its 10 ms budget for a
   sustained stretch on a slow agent PC (9 ms max already seen), the capture ring
   drops → audible gaps in the call. Affects scenarios 11/12 / "no artifacts".
4. **Windows default-device changes not followed** (scenario 10). Explicit gap.
5. **Genesys headset call controls** (answer/hang-up/mute on Jabra/Poly/EPOS/Yealink)
   will most likely stop working when the virtual mic is selected instead of the
   physical headset — a Genesys compatibility gap, not an engine bug.
6. **Self-exclusion is name-based** (`devices.rs`): renaming the endpoint defeats the
   feedback-loop guard. Robust fix = match the devnode hardware ID.
7. **Chrome/Genesys restart handling** depends on the driver's stream-state (Run/Stop)
   hooks being wired per `driver/README.md`; unverified until the driver is built.
8. **Deployment/robustness:** model path compiled in with no SHA-256 check; needs the
   VC++ runtime unless the CRT is linked statically; mic fails with `0x8007007A` on a
   very long install path; mic-privacy denial has no in-engine hint.
9. **Diagnostics are console-only** — no persistent log/ETW for support.

## Readiness verdict

- **User-mode engine:** MVP-ready and verified for the **core audio path**
  (capture, DFN3, playback, drift, supervised reconnect, failover, 15-min stability).
  The virtual-device bridge (`driver.rs`, `vmic.rs`, `vspk.rs`) is written, builds
  clean, and passes its logic tests.
- **End-to-end MVP:** **not yet runnable.** The single gating item is building and
  installing `yalcv.sys` on a test PC; after that, Genesys is ordinary device
  selection plus a call the user places.

## Next steps

1. Build + test-sign + install the driver (`driver/README.md`), then run the 12
   scenarios with the engine, Chrome, and a Genesys test call.
2. Before that test can pass on latency and artifacts, fix in the engine:
   model-derived DFN3 delay (unblock the low-latency model) and automatic
   overload → bypass. Optionally follow physical default-device changes (scenario 10).
