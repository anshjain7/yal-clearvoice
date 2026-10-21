/*
 * yalcv_ioctl.h - the entire interface between the YAL ClearVoice engine and
 * the YAL ClearVoice virtual-microphone driver.
 *
 * One IOCTL. The engine hands the driver one frame of processed 48 kHz mono
 * PCM and learns how full the driver's ring is, so it can steer out clock drift.
 * The Rust side mirrors these constants in src/vmic.rs (a unit test checks the
 * IOCTL code against the CTL_CODE expansion). Change one, change the other, and
 * bump YALCV_ABI.
 *
 * Include after <ntddk.h> (kernel) or <windows.h> (user mode): both define
 * CTL_CODE, FILE_DEVICE_UNKNOWN, METHOD_BUFFERED, FILE_WRITE_ACCESS and the
 * fixed-width types.
 */
#pragma once

#define YALCV_ABI 1

/* Microphone (send) path: 480 mono samples is one 10 ms frame at 48 kHz; +1 lets
 * the engine stretch a frame by a sample for drift correction. */
#define YALCV_FRAME_SAMPLES 480
#define YALCV_MAX_SAMPLES   481

/* Speaker (return) path: a 10 ms render period is 480 stereo frames = 960
 * interleaved samples; +1 frame (2 samples) for drift. */
#define YALCV_SPK_FRAME_SAMPLES 960
#define YALCV_SPK_MAX_SAMPLES   962

/* Control device. User mode opens \\.\YalClearVoice. */
#define YALCV_NT_DEVICE_NAME  L"\\Device\\YalClearVoice"
#define YALCV_DOS_DEVICE_NAME L"\\DosDevices\\YalClearVoice"

/* CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_ACCESS)
 * == 0x0022A000. The engine passes one mic frame in and reads ring state out. */
#define IOCTL_YALCV_WRITE \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS)
 * == 0x00226004. The engine asks for one speaker frame and reads ring state out. */
#define IOCTL_YALCV_READ \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS)

/* Request: one frame of 48 kHz mono PCM. Plain repr(C); max member alignment is
 * 4, so this lays out identically to the Rust WriteIn. */
typedef struct _YALCV_WRITE_IN {
    UINT32 abi;                    /* must equal YALCV_ABI */
    UINT32 samples;                /* 1..=YALCV_MAX_SAMPLES of valid PCM */
    INT16  pcm[YALCV_MAX_SAMPLES]; /* 48 kHz mono */
} YALCV_WRITE_IN;

/* Reply: ring state after the write. */
typedef struct _YALCV_WRITE_OUT {
    UINT32 queued;    /* samples in the ring after this write */
    UINT32 running;   /* 1 while an application is capturing from the endpoint */
    UINT32 underruns; /* cumulative: ring ran empty, silence was emitted */
    UINT32 overruns;  /* cumulative: ring was full, oldest audio was dropped */
} YALCV_WRITE_OUT;

/* Speaker read request: how many interleaved stereo samples to pull. */
typedef struct _YALCV_READ_IN {
    UINT32 abi;     /* must equal YALCV_ABI */
    UINT32 samples; /* 1..=YALCV_SPK_MAX_SAMPLES interleaved samples wanted */
} YALCV_READ_IN;

/* Speaker read reply: ring state plus the frame. Shorter than requested when the
 * ring is low; the engine resamples what it gets. */
typedef struct _YALCV_READ_OUT {
    UINT32 queued;    /* interleaved samples in the ring after the read (frames = /2) */
    UINT32 running;   /* 1 while an application is rendering to the endpoint */
    UINT32 underruns; /* cumulative: ring ran empty when read */
    UINT32 overruns;  /* cumulative: ring was full, oldest dropped (engine fell behind) */
    UINT32 samples;   /* interleaved samples actually returned in pcm */
    INT16  pcm[YALCV_SPK_MAX_SAMPLES]; /* 48 kHz 16-bit stereo, interleaved */
} YALCV_READ_OUT;
