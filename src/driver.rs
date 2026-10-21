//! Shared plumbing for the YAL ClearVoice virtual-audio driver's control device:
//! one open and one generic IOCTL round-trip. `vmic` writes the microphone
//! endpoint; `vspk` reads the speaker endpoint. Both open their own handle to the
//! one control device (it allows shared read/write). Mirrors
//! `driver/abi/yalcv_ioctl.h`; the codes are checked against CTL_CODE in tests.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{Owned, w};

use crate::devices::SelectError;

/// User-mode path of the driver's control device (`\DosDevices\YalClearVoice`).
const DEVICE_PATH: windows::core::PCWSTR = w!(r"\\.\YalClearVoice");
/// GENERIC_READ | GENERIC_WRITE.
const GENERIC_RW: u32 = 0xC000_0000;
/// FILE_SHARE_READ | FILE_SHARE_WRITE: the mic and speaker paths each hold a handle.
const SHARE_RW: FILE_SHARE_MODE = FILE_SHARE_MODE(0x3);

pub(crate) const YALCV_ABI: u32 = 1;
/// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_ACCESS).
pub(crate) const IOCTL_YALCV_WRITE: u32 = 0x0022_A000;
/// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS).
pub(crate) const IOCTL_YALCV_READ: u32 = 0x0022_6004;

/// Opens the control device. A missing driver (not installed, or being upgraded)
/// comes back as `Disconnected`, so a supervisor retries.
pub(crate) fn open() -> Result<Owned<HANDLE>, SelectError> {
    // SAFETY: a plain Win32 open of a named device; the handle is owned (closed
    // on drop) as soon as it exists.
    let handle = unsafe {
        CreateFileW(
            DEVICE_PATH,
            GENERIC_RW,
            SHARE_RW,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| {
        if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
            SelectError::Disconnected
        } else {
            SelectError::Windows(e)
        }
    })?;
    // SAFETY: a file handle, which Owned closes with CloseHandle.
    Ok(unsafe { Owned::new(handle) })
}

/// One IOCTL round-trip with a fixed-size request and reply (METHOD_BUFFERED).
/// Any failure means the driver went away or is upgrading; it comes back as
/// `Disconnected` so the caller's supervisor reopens.
pub(crate) fn ioctl<I, O>(
    handle: HANDLE,
    code: u32,
    input: &I,
    out: &mut O,
) -> Result<(), SelectError> {
    let mut returned = 0u32;
    // SAFETY: both buffers outlive the call and their sizes are passed.
    unsafe {
        DeviceIoControl(
            handle,
            code,
            Some((input as *const I).cast()),
            size_of::<I>() as u32,
            Some((out as *mut O).cast()),
            size_of::<O>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|_| SelectError::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The IOCTL codes are what CTL_CODE expands to in the driver header.
    #[test]
    fn ioctl_codes_match_ctl_code_macro() {
        // CTL_CODE(t,f,m,a) = (t<<16)|(a<<14)|(f<<2)|m
        let ctl = |function: u32, access: u32| (0x22u32 << 16) | (access << 14) | (function << 2);
        assert_eq!(ctl(0x800, 2 /* FILE_WRITE_ACCESS */), IOCTL_YALCV_WRITE);
        assert_eq!(ctl(0x801, 1 /* FILE_READ_ACCESS */), IOCTL_YALCV_READ);
    }

    /// A missing driver is reported as Disconnected, not a hard error, so a
    /// supervisor keeps retrying until it is installed.
    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn missing_driver_is_disconnected() {
        assert!(matches!(open(), Err(SelectError::Disconnected)));
    }
}
