/*
 * yalcv_ring.cpp - see yalcv_ring.h. All index work is under the spinlock; the
 * copies are at most one frame, so the lock is held for microseconds.
 */
#include "yalcv_ring.h"

static ULONG YalcvCount(const YALCV_RING *ring)
{
    return (ring->tail - ring->head) & YALCV_RING_MASK;
}

VOID YalcvRingInit(_Out_ YALCV_RING *ring)
{
    RtlZeroMemory(ring, sizeof(*ring));
    KeInitializeSpinLock(&ring->lock);
}

VOID YalcvRingSetRunning(_Inout_ YALCV_RING *ring, _In_ BOOLEAN running)
{
    KIRQL irql;
    KeAcquireSpinLock(&ring->lock, &irql);
    /* A fresh capture starts from empty, never on audio left from the last call. */
    if (running && ring->running == 0) {
        ring->head = ring->tail;
    }
    ring->running = running ? 1 : 0;
    KeReleaseSpinLock(&ring->lock, irql);
}

_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG YalcvRingWrite(_Inout_ YALCV_RING *ring, _In_reads_(samples) const INT16 *pcm, _In_ ULONG samples)
{
    KIRQL irql;
    ULONG queued;

    if (samples == 0 || samples > YALCV_MAX_SAMPLES) {
        KeAcquireSpinLock(&ring->lock, &irql);
        queued = YalcvCount(ring);
        KeReleaseSpinLock(&ring->lock, irql);
        return queued;
    }

    KeAcquireSpinLock(&ring->lock, &irql);
    /* Keep one slot free so a full ring (tail+1==head) is distinct from empty.
     * If the newcomer won't fit, drop the oldest to make room: freshest audio
     * wins, and the engine never stalls. */
    {
        ULONG free = YALCV_RING_SAMPLES - 1u - YalcvCount(ring);
        if (samples > free) {
            ring->head = (ring->head + (samples - free)) & YALCV_RING_MASK;
            ring->overruns++;
        }
    }
    for (ULONG i = 0; i < samples; i++) {
        ring->data[ring->tail] = pcm[i];
        ring->tail = (ring->tail + 1u) & YALCV_RING_MASK;
    }
    queued = YalcvCount(ring);
    KeReleaseSpinLock(&ring->lock, irql);
    return queued;
}

_IRQL_requires_max_(DISPATCH_LEVEL)
ULONG YalcvRingRead(_Inout_ YALCV_RING *ring, _Out_writes_(samples) INT16 *out, _In_ ULONG samples)
{
    KIRQL irql;
    ULONG have, i, left;

    KeAcquireSpinLock(&ring->lock, &irql);
    have = YalcvCount(ring);
    if (have > samples) {
        have = samples;
    }
    for (i = 0; i < have; i++) {
        out[i] = ring->data[ring->head];
        ring->head = (ring->head + 1u) & YALCV_RING_MASK;
    }
    /* Silence for anything not delivered yet. A shortfall while an app is active
     * is an underrun; before it starts it is just the idle ring. */
    if (i < samples) {
        if (ring->running) {
            ring->underruns++;
        }
        RtlZeroMemory(&out[i], (samples - i) * sizeof(INT16));
    }
    left = YalcvCount(ring);
    KeReleaseSpinLock(&ring->lock, irql);
    return left;
}
