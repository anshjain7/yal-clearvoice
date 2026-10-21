# YAL ClearVoice — Production-Readiness Report

**Date:** 2026-10-08

## Verdict: not production-ready

The **user-mode engine is strong and verified** — WASAPI I/O, DeepFilterNet3,
failover, device-follow, diagnostics and the IPC surface are built, lint-clean,
unit-tested, and proven stable over a 1-hour soak. But the product cannot ship or
even fully function yet, because its three other halves are **source-only**: the
kernel driver (unbuilt, unsigned, unverified), the installer (unbuilt), and the
supervisor service (not written). Those are the BLOCKERs, and none is safe to fix
automatically — each needs a WDK build, code signing, HLK/driver-verifier passes,
a clean test machine, or a new component.

**Counts:** 4 BLOCKER · 5 HIGH · 6 MEDIUM · 2 LOW · 5 READY (areas can carry more
than one finding; the table lists each area's dominant class).

## Area-by-area

| # | Area | Class | Finding |
|---|------|-------|---------|
| 1 | WASAPI audio engine | **READY** | Shared-mode event-driven, MMCSS, no locks; unit-tested; 1-hour soak clean (0 drops). |
| 2 | DeepFilterNet3 integration | **MEDIUM** | Works; failover/delay/overload tested. libDF allocates per frame on the RT thread (jitter risk, not a leak); `unsafe impl Send` leans on tract internals — recheck each upgrade. |
| 3 | Virtual microphone | **BLOCKER** | Engine side built + tested; the driver (`yalcv.sys`) is source only — not built, signed, installed, or tested. No endpoint exists. |
| 4 | Virtual speaker | **BLOCKER** | Same driver as #3 — render endpoint is source only. |
| 5 | Chrome compatibility | **HIGH** | Method verified (live `enumerateDevices` works); the virtual mic is absent because the driver isn't installed, so the end-to-end path is unverified. Blocked by #3/#4. |
| 6 | Genesys Cloud CX compatibility | **HIGH** | Device selection needs no API work, but headset hardware call-controls almost certainly break when a virtual mic is selected (known gap); full call path untested (no driver, no org login). |
| 7 | Latency | **MEDIUM** | ~50 ms core (measured), ~53 ms through the virtual path (estimated) — at the edge of the <50 ms target. The low-latency model path is unblocked in code but the LL model isn't shipped or tested. |
| 8 | CPU usage | **READY** | 7.8% of one core avg over 1 hour (p95 15.6%, peak 27.8%). |
| 9 | Memory usage | **READY** | Flat over 1 hour — working set 15.8–30 MB, net −5.4 MB. No leak; no growth. |
| 10 | Device reconnect | **MEDIUM** | `supervise` + device-invalidation + default-follow; `supervise` unit-tested. Live unplug/replug and the 1 s default-poll aren't live-verified. |
| 11 | Failure recovery | **MEDIUM** | DFN3 failover + overload→bypass tested. Engine-crash→restart depends on the **unbuilt supervisor**; power/session/audio-service-restart handling is designed, not implemented. |
| 12 | Long-running stability | **READY** | Core pipeline: 1 hour, ~360k frames, 0 drops, 0 glitches, no leak, no deadlock. (Virtual-device path not soak-tested — see #3.) |
| 13 | Installer | **BLOCKER** | WiX source is well-formed and the devnode helper compiles, but the MSI is unbuilt/untested, and it references `yalcv-supervisor.exe`, which doesn't exist yet. |
| 14 | Windows compatibility | **HIGH** | One x64 build targets Win10 1809+/11, but **driver signing isn't done** (attestation or WHQL) and HVCI/Memory-Integrity compatibility is unverified — both gate loading on real machines. |
| 15 | Driver security | **BLOCKER** | The kernel driver is the main attack surface and is unverified: no Driver Verifier, IOCTL fuzzing, CodeQL, or HLK. (IPC pipe also uses a default ACL — HIGH, harden with an SDDL.) |
| 16 | Logging / privacy | **MEDIUM** | Privacy is sound — no raw audio is written to disk anywhere (engine, driver, diagnostics are metadata only). But there's no persistent log/ETW for field support; diagnostics are console-only. |
| 17 | Code quality | **READY** | Rust: clippy `-D warnings` clean, rustfmt clean, lock-free, well-documented. (C++ devnode helper compiles; the driver and C# UI aren't built here — see #3/#18.) |
| 18 | Tests | **HIGH** | Strong Rust unit coverage + a 1-hour soak, but **zero automated tests** for the driver, the UI, and the installer, and no HLK. |
| 19 | Documentation | **LOW** | Good inline docs, component READMEs, an architecture doc, and an MVP test report. Missing: ops runbook, admin/user guide, and a written threat model. |

## BLOCKERs — and why none is auto-fixed

Per the instruction, I fix a BLOCKER only if it's *clearly safe to fix
automatically*. None of these qualifies — each needs tooling, hardware, signing, or
a new component that doesn't exist in this environment:

1. **Driver not built/signed/installed (#3, #4).** Needs the WDK, an EV cert +
   attestation/WHQL submission, and a test machine. Cannot be produced or verified
   by editing files here.
2. **Driver security unverified (#15).** Needs Driver Verifier, IOCTL fuzzing,
   CodeQL, and HLK runs — none available here. Shipping an unverified kernel driver
   is the single biggest risk.
3. **Installer unbuilt/untested (#13).** Needs the WiX toolset, the signed driver,
   and a clean VM. The `yalcv-supervisor.exe` it installs isn't written yet.
4. **Supervisor service missing (#11, #13).** A new component — writing it is new
   work, not an automatic fix.

**Fixes applied this review: none.** Every BLOCKER is outside what can be safely
changed by editing source here, so making code changes would risk harm without
resolving the blocker. No files were modified.

## Shortest path to ready

1. Write the supervisor service (unblocks #11, #13).
2. Build the driver on a WDK machine; pass Driver Verifier + a 24-hour soak; fuzz
   the IOCTLs (#3, #4, #15).
3. Procure the EV cert; attestation-sign for a pilot, start WHQL for production (#14).
4. Build + test the MSI on a clean VM: install/upgrade/uninstall/rollback (#13).
5. Run the real Chrome + Genesys call tests on a signed, installed build; decide the
   headset-call-control approach (#5, #6).
6. Ship the low-latency model and measure; add persistent ETW logging (#7, #16).
7. Add driver/UI/installer tests and an ops runbook (#18, #19).

## What is genuinely ready now

The user-mode engine: capture, DeepFilterNet3 with click-free bypass/failover/
overload handling and model-derived delay, drift correction, supervised reconnect
with default-follow, the virtual-mic/speaker client code, live diagnostics, and the
control IPC — all lint-clean, unit-tested, and stable across a 1-hour run with no
leak, no deadlock, and ~8% CPU. That half is production-quality; the kernel, service,
installer, and end-to-end call path are not yet.
