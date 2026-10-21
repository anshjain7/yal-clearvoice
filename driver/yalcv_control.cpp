/*
 * yalcv_control.cpp - secured control device + IOCTL_YALCV_WRITE handler.
 *
 * PortCls owns the driver's dispatch table (every MajorFunction points at
 * PcDispatchIrp). To add our own control device without disturbing the audio
 * stack, we save PortCls's dispatch and install a thin shim that handles IRPs
 * for our device object and forwards everything else to PortCls - the same
 * pattern SysVAD uses. The driver does no DSP: this path only copies samples
 * into the ring and reports its fill.
 */
#include <ntddk.h>
#include <wdmsec.h> /* IoCreateDeviceSecure; link wdmsec.lib */
#include "yalcv_control.h"

YALCV_RING g_YalcvRing;
YALCV_RING g_YalcvSpkRing;

static PDEVICE_OBJECT g_ControlDevice;
static PDRIVER_DISPATCH g_PcDispatch; /* PortCls's handler, forwarded to */

/* Security: SYSTEM and Administrators full, interactive users read+write. */
static const WCHAR g_Sddl[] = L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/* Class GUID for the control device object's security/registry. Placeholder:
 * regenerate once (guidgen) and keep it stable across releases.
 * {7F9A6E10-0C21-4C4E-9E2D-5B2A1E6C0001} */
static const GUID GUID_YALCV_CONTROL_CLASS = {
    0x7f9a6e10, 0x0c21, 0x4c4e, {0x9e, 0x2d, 0x5b, 0x2a, 0x1e, 0x6c, 0x00, 0x01}};

/* Validate and service one IOCTL_YALCV_WRITE. METHOD_BUFFERED: in and out share
 * SystemBuffer, so the whole request is read before the reply is written. */
static NTSTATUS YalcvWrite(_In_ PIRP irp, _In_ PIO_STACK_LOCATION stack, _Out_ ULONG *info)
{
    ULONG inLen = stack->Parameters.DeviceIoControl.InputBufferLength;
    ULONG outLen = stack->Parameters.DeviceIoControl.OutputBufferLength;
    YALCV_WRITE_IN *in;
    YALCV_WRITE_OUT *out;
    ULONG samples, queued;

    *info = 0;
    if (inLen < FIELD_OFFSET(YALCV_WRITE_IN, pcm) || outLen < sizeof(YALCV_WRITE_OUT)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    in = (YALCV_WRITE_IN *)irp->AssociatedIrp.SystemBuffer;
    if (in->abi != YALCV_ABI) {
        return STATUS_REVISION_MISMATCH;
    }
    samples = in->samples;
    if (samples == 0 || samples > YALCV_MAX_SAMPLES) {
        return STATUS_INVALID_PARAMETER;
    }
    if (inLen < FIELD_OFFSET(YALCV_WRITE_IN, pcm) + samples * sizeof(INT16)) {
        return STATUS_BUFFER_TOO_SMALL;
    }

    queued = YalcvRingWrite(&g_YalcvRing, in->pcm, samples);

    /* in->pcm is now consumed; reuse the shared buffer for the reply. The status
     * counters are read without the lock: a stale read of a status field is
     * harmless and never races a copy. */
    out = (YALCV_WRITE_OUT *)irp->AssociatedIrp.SystemBuffer;
    out->queued = queued;
    out->running = (ULONG)g_YalcvRing.running;
    out->underruns = g_YalcvRing.underruns;
    out->overruns = g_YalcvRing.overruns;
    *info = sizeof(YALCV_WRITE_OUT);
    return STATUS_SUCCESS;
}

/* Service one IOCTL_YALCV_READ: hand the engine one speaker frame from the ring.
 * METHOD_BUFFERED, so in and out share SystemBuffer; the request is read before
 * the reply (which includes the frame) is written. */
static NTSTATUS YalcvRead(_In_ PIRP irp, _In_ PIO_STACK_LOCATION stack, _Out_ ULONG *info)
{
    ULONG inLen = stack->Parameters.DeviceIoControl.InputBufferLength;
    ULONG outLen = stack->Parameters.DeviceIoControl.OutputBufferLength;
    YALCV_READ_IN *in;
    YALCV_READ_OUT *out;
    ULONG samples;

    *info = 0;
    if (inLen < sizeof(YALCV_READ_IN) || outLen < sizeof(YALCV_READ_OUT)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    in = (YALCV_READ_IN *)irp->AssociatedIrp.SystemBuffer;
    if (in->abi != YALCV_ABI) {
        return STATUS_REVISION_MISMATCH;
    }
    samples = in->samples;
    if (samples == 0 || samples > YALCV_SPK_MAX_SAMPLES) {
        return STATUS_INVALID_PARAMETER;
    }

    /* `samples` is captured; reuse the shared buffer for the reply. pcm starts
     * past the request fields, so the read fills it without clobbering them. */
    out = (YALCV_READ_OUT *)irp->AssociatedIrp.SystemBuffer;
    out->queued = YalcvRingRead(&g_YalcvSpkRing, out->pcm, samples);
    out->samples = samples;
    out->running = (ULONG)g_YalcvSpkRing.running;
    out->underruns = g_YalcvSpkRing.underruns;
    out->overruns = g_YalcvSpkRing.overruns;
    *info = sizeof(YALCV_READ_OUT);
    return STATUS_SUCCESS;
}

/* Dispatch shim: our control device here, everything else to PortCls. */
static NTSTATUS YalcvDispatch(_In_ PDEVICE_OBJECT device, _Inout_ PIRP irp)
{
    PIO_STACK_LOCATION stack;
    NTSTATUS status;
    ULONG info = 0;

    if (device != g_ControlDevice) {
        return g_PcDispatch(device, irp);
    }
    stack = IoGetCurrentIrpStackLocation(irp);
    switch (stack->MajorFunction) {
    case IRP_MJ_CREATE:
    case IRP_MJ_CLOSE:
        status = STATUS_SUCCESS; /* the engine opens one handle per direction */
        break;
    case IRP_MJ_DEVICE_CONTROL:
        switch (stack->Parameters.DeviceIoControl.IoControlCode) {
        case IOCTL_YALCV_WRITE:
            status = YalcvWrite(irp, stack, &info);
            break;
        case IOCTL_YALCV_READ:
            status = YalcvRead(irp, stack, &info);
            break;
        default:
            status = STATUS_INVALID_DEVICE_REQUEST;
            break;
        }
        break;
    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        break;
    }
    irp->IoStatus.Status = status;
    irp->IoStatus.Information = info;
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return status;
}

NTSTATUS YalcvInitControl(_In_ PDRIVER_OBJECT driverObject)
{
    UNICODE_STRING deviceName, dosName, sddl;
    NTSTATUS status;

    YalcvRingInit(&g_YalcvRing);
    YalcvRingInit(&g_YalcvSpkRing);

    RtlInitUnicodeString(&deviceName, YALCV_NT_DEVICE_NAME);
    RtlInitUnicodeString(&dosName, YALCV_DOS_DEVICE_NAME);
    RtlInitUnicodeString(&sddl, g_Sddl);

    status = IoCreateDeviceSecure(
        driverObject, 0, &deviceName, FILE_DEVICE_UNKNOWN, FILE_DEVICE_SECURE_OPEN,
        FALSE, /* the one engine holds two handles: mic write + speaker read.
                * Single-engine is enforced by the supervisor and the SDDL. */
        &sddl, &GUID_YALCV_CONTROL_CLASS, &g_ControlDevice);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = IoCreateSymbolicLink(&dosName, &deviceName);
    if (!NT_SUCCESS(status)) {
        IoDeleteDevice(g_ControlDevice);
        g_ControlDevice = NULL;
        return status;
    }
    g_ControlDevice->Flags |= DO_BUFFERED_IO;

    /* Hook after PortCls has filled the table; forward the rest to it. */
    g_PcDispatch = driverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL];
    driverObject->MajorFunction[IRP_MJ_CREATE] = YalcvDispatch;
    driverObject->MajorFunction[IRP_MJ_CLOSE] = YalcvDispatch;
    driverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL] = YalcvDispatch;
    return STATUS_SUCCESS;
}

VOID YalcvDeleteControl(VOID)
{
    UNICODE_STRING dosName;
    if (g_ControlDevice != NULL) {
        RtlInitUnicodeString(&dosName, YALCV_DOS_DEVICE_NAME);
        IoDeleteSymbolicLink(&dosName);
        IoDeleteDevice(g_ControlDevice);
        g_ControlDevice = NULL;
    }
}
