/*
 * yalcv_ring.h - the driver's audio ring: the engine writes processed frames in,
 * the WaveRT stream reads them out to the endpoint's cyclic buffer. One writer
 * (the IOCTL thread, PASSIVE_LEVEL), one reader (the stream timer/DPC,
 * DISPATCH_LEVEL); a spinlock covers the few-sample index updates. Non-paged,
 * fixed size, no allocation after init. The driver does no DSP: it only stores
 * samples and keeps time.
 */
#pragma once

#include <ntddk.h>
#include "abi/yalcv_ioctl.h"

/* Power of two, ~170 ms at 48 kHz mono: generous, so a hiccup costs a counter,
 * not a gap. Latency is set by the engine's target fill, not this capacity. */
#define YALCV_RING_SAMPLES 8192u
#define YALCV_RING_MASK    (YALCV_RING_SAMPLES - 1u)

typedef struct _YALCV_RING {
    KSPIN_LOCK lock;
    LONG       running;   /* 1 while an application is capturing */
    ULONG      head;      /* read index (stream) */
    ULONG      tail;      /* write index (engine) */
    ULONG      underruns; /* cumulative */
    ULONG      overruns;  /* cumulative */
    INT16      data[YALCV_RING_SAMPLES];
} YALCV_RING;

/* Zeroes indices and counters. Call once at device add. */
VOID YalcvRingInit(_Out_ YALCV_RING *ring);

/* Marks whether an application is capturing. A 0->1 transition flushes the ring
 * so a new call never starts on stale audio. Called from the stream's
 * Run/Stop transitions. */
VOID YalcvRingSetRunning(_Inout_ YALCV_RING *ring, _In_ BOOLEAN running);

/* Engine -> ring. Copies `samples` in, dropping the oldest audio (and counting
 * an overrun) if the ring is full, so the engine never blocks. Returns the
 * number of samples queued after the write. */
_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG YalcvRingWrite(_Inout_ YALCV_RING *ring, _In_reads_(samples) const INT16 *pcm, _In_ ULONG samples);

/* Reads `samples` out. A shortfall is zero-filled and, if an application is
 * active, counted as an underrun. Returns the samples left queued after the read.
 * Mic path: the capture stream calls it to fill the endpoint buffer. Speaker path:
 * the IOCTL_YALCV_READ handler calls it to hand the engine a frame. */
_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG YalcvRingRead(_Inout_ YALCV_RING *ring, _Out_writes_(samples) INT16 *out, _In_ ULONG samples);
