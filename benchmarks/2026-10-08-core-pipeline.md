# Core pipeline benchmark, 2026-10-08

Microphone → WASAPI capture → DeepFilterNet3 → WASAPI playback → speaker, live, 15 minutes.

## Setup

| | |
|---|---|
| Machine | Laptop, 12 logical cores, Windows 11 (build 26200) |
| Devices | USB-C headset as both microphone and speaker (one device, one clock), 48 kHz |
| Engine | `clearvoice.exe 900`: release build, MSVC toolchain, Rust's default release profile (no LTO) |
| Model | DeepFilterNet3 standard (libDF `d375b2d`, tract 0.21.4), 30 ms algorithmic delay |
| Load | Live microphone audio; nothing else running |
| Measurement | Engine counters printed at exit, plus CPU sampled once a second from outside the process (`TotalProcessorTime`) |

## BEFORE results

| Metric | Result |
|---|---|
| Run length | 903.5 s; 89,918 frames captured (about 899 s of audio) |
| Capture latency | avg 3.3 ms, max 19.1 ms (WASAPI packet timestamp → frame handed to DeepFilterNet) |
| DeepFilterNet processing | avg 0.94 ms, max 5.97 ms per 10 ms frame |
| DeepFilterNet delay | 30 ms, fixed by the model (10 ms STFT overlap + 20 ms lookahead) |
| Playback latency | avg 10.4 ms, max 12.0 ms of audio queued ahead in the WASAPI buffer; Windows reports 0 ms device latency for this headset |
| **Total added latency** | **avg 50.3 ms, max 61.4 ms** (packet timestamp → speaker) |
| CPU, average | 10.9% of one core (0.91% of the whole machine) |
| CPU, per-second peak | p99 26%, max 29.5% of one core; 90.7% during the first seconds while the model loads |
| Memory | peak working set 30.5 MB |
| Underruns (speaker ran dry) | 2 (two 10 ms silences in 15 minutes) |
| Overruns (ring full) | 0 at capture, 0 after DeepFilterNet |
| Dropped frames | 0 |
| Capture glitches (WASAPI discontinuities) | 0 |
| DeepFilterNet errors | 0 (3 frames bypassed: the model's warm-up) |
| Drift corrections | 525 frames played a sample longer or shorter (0.6%) |
| Stale frames skipped / device reopens | 0 / 0 |

### Where the latency goes (averages)

| Stage | ms |
|---|---|
| Capture (timestamp → handed on) | 3.3 |
| DeepFilterNet delay (model) | 30.0 |
| DeepFilterNet processing | 0.9 |
| Waiting in the rings (residual: the stage's 1 ms poll, then waiting for the speaker's next 10 ms period) | ~5.7 |
| Playback queue (one-frame cushion + device buffer) | 10.4 |
| **Total** | **50.3** |

### Caveats

- **Capture may be under-measured:** the figure starts at WASAPI's packet timestamp, and its 3.3 ms average is shorter than a 10 ms packet. That suggests Windows stamps packets when they complete, not at their first sample. If so, the earliest sample of each frame is up to 10 ms older than the totals show.
- **Hardware not included:** the headset's own converter latency isn't in these numbers; Windows reports 0 ms for both directions.
- **Gaps are counted, not heard:** "underruns" are moments the speaker had nothing queued and played 10 ms of silence. No one listened critically for artifacts.

## Against the targets

- **Under 50 ms added latency:** borderline. The average is 50.3 ms, and DeepFilterNet's fixed 30 ms leaves about 20 ms for everything else. Of that 20 ms, 10.4 ms is the playback cushion that absorbs late frames, and about 5.7 ms is waiting for the speaker's next period.
- **No audible interruptions:** 2 gaps of 10 ms in 15 minutes. The 19.1 ms maximum capture latency shows at least one capture-side delay about 16 ms longer than usual. That's more than the one-frame (10 ms) playback cushion absorbs, which is the likely cause; this run doesn't prove it.
- **Stability:** no drops, overruns, glitches, model errors or device reopens over 15 minutes. Drift correction stayed active and kept the speaker's backlog in range.

## What the data says about optimizing

- **CPU is DeepFilterNet:** 0.94 ms per 10 ms frame is about 9.4% of a core out of 10.9% total, so inference is roughly 86% of the engine's CPU. Compiler optimization of libDF and tract is the only CPU lever with real headroom.
- **Latency slack is small:** the playback cushion is already too small to absorb every hiccup (2 gaps), so shrinking it would trade latency for gaps. The ring waits come mostly from the speaker's 10 ms period; the stage's 1 ms poll accounts for 1 ms at most.
- **Copies and allocations don't show up:** each frame is about 3.8 KB and is copied a few times per hop, about 20 KB per 10 ms in all. The only per-frame allocations are inside libDF, on its own thread.

## Optimization attempts

The bar was "only measurable performance improvements". One candidate was worth measuring; none were kept.

### Whole-program optimization (`lto = "fat"`, `codegen-units = 1`): rejected

This targets DeepFilterNet inference, about 86% of the engine's CPU. It was measured with the `processing_time` diagnostic: 10 s of fixed synthetic audio through the real model. Default and LTO test binaries were run alternately in 12 paired rounds on an otherwise idle machine.

| Build | Avg per frame | Std dev | Range | Median | p99 |
|---|---|---|---|---|---|
| Default release profile | 0.897 ms | 0.057 | 0.827–1.046 | 0.914 ms | 1.846 ms |
| LTO + 1 codegen unit | 0.881 ms | 0.038 | 0.830–0.952 | 0.903 ms | 1.790 ms |

- **Gain within the noise:** 1.8% faster on average, less than the run-to-run spread. LTO won only 8 of the 12 pairs.
- **Cost:** release build time rises from about 2.5 minutes to about 7.
- **Why pair the runs:** three back-to-back LTO runs first suggested 17%, but the laptop's speed drifts between sessions, which accounted for most of that.

### Not attempted: the data shows no measurable gain

- **Waking the DeepFilterNet stage on frame arrival** instead of its 1 ms poll could save about 0.5 ms of a 50 ms total, and polling costs a fraction of a percent of CPU. The speaker's 10 ms period dominates when a frame actually plays, so no run could measure that saving.
- **Shrinking the playback cushion** (10.4 ms) would cut latency but turn the capture hiccups behind the 2 gaps into more gaps.
- **Frame copies:** about 20 KB of memcpy per 10 ms; nothing to measure.

## AFTER

No optimization was kept, so the engine is the same as BEFORE except a `devices.rs` fix: device listing now skips an endpoint that fails instead of failing entirely. That code runs only when a device opens, not on the audio path. A repeat 15-minute run would measure run-to-run variance, not an optimization. It wasn't run: the USB-C headset was no longer connected, and the laptop's built-in microphone and speaker feed back.

## What would actually move the numbers

These all go beyond "optimization without changing the algorithm or adding features":

- **Latency:** the DeepFilterNet3 low-latency model has 0 frames of lookahead, which would remove 20 ms of the model's 30 ms delay. That's a model change and needs its own quality check.
- **Gaps:** a one-frame larger playback cushion would absorb capture hiccups like the one behind the 2 gaps, at +10 ms latency.
- **CPU:** already low at 10.9% of one core; DeepFilterNet's own decoder-skipping thresholds are the remaining lever, and they affect quality.
