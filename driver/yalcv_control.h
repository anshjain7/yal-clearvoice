/*
 * yalcv_control.h - the secured control device and its one IOCTL. Separate from
 * the audio endpoint PortCls publishes; only this channel carries audio in.
 */
#pragma once

#include <ntddk.h>
#include "yalcv_ring.h"

/* Mic ring: the IOCTL_YALCV_WRITE handler writes it, the capture stream reads it. */
extern YALCV_RING g_YalcvRing;
/* Speaker ring: the render stream writes it, the IOCTL_YALCV_READ handler reads it. */
extern YALCV_RING g_YalcvSpkRing;

/* Called once from DriverEntry, right after PcInitializeAdapterDriver: creates
 * \Device\YalClearVoice (SYSTEM/Admin full, interactive users read+write; the
 * engine holds two shared handles - mic write and speaker read) with its \\.\
 * symlink, and hooks the driver's CREATE/CLOSE/DEVICE_CONTROL dispatch so IRPs
 * for the control device come here and all others still go to PortCls. */
NTSTATUS YalcvInitControl(_In_ PDRIVER_OBJECT driverObject);

/* Called from DriverUnload: removes the symlink and the device. */
VOID YalcvDeleteControl(VOID);
