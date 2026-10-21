//! Lists Windows audio devices through WASAPI's MMDevice API. Starts no streams.

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATEMASK_ALL, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, eCapture, eCommunications, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::core::{PWSTR, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Endpoint ID, stable across reboots: store it to remember a device choice.
    pub id: String,
    pub name: String,
    pub direction: Direction,
    /// Windows default device for this direction.
    pub default: bool,
    /// Windows default communications device, the one softphones use.
    pub default_comms: bool,
    /// Shared-mode mix format: the rate and channel count Windows runs the
    /// device at. Streams at other rates are resampled by Windows.
    pub sample_rate: u32,
    pub channels: u16,
}

/// Active input and output devices.
pub fn list() -> Result<Vec<Device>> {
    // SAFETY: COM calls on interfaces returned by COM; every COM-allocated
    // string, PROPVARIANT and format below is freed exactly once.
    unsafe {
        let enumerator = enumerator()?;
        let mut devices = Vec::new();
        for (flow, direction) in [(eCapture, Direction::Input), (eRender, Direction::Output)] {
            let default_id = |role| {
                let device = enumerator.GetDefaultAudioEndpoint(flow, role).ok()?;
                device.GetId().ok().map(|p| take(p))
            };
            let (default, comms) = (default_id(eConsole), default_id(eCommunications));
            let all = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            for i in 0..all.GetCount()? {
                let describe = || -> Result<_> {
                    let device = all.Item(i)?;
                    let id = take(device.GetId()?);
                    let mut prop = device
                        .OpenPropertyStore(STGM_READ)?
                        .GetValue(&PKEY_Device_FriendlyName)?;
                    let name = PropVariantToStringAlloc(&prop).map(|p| take(p));
                    let _ = PropVariantClear(&mut prop);
                    let mix = device
                        .Activate::<IAudioClient>(CLSCTX_ALL, None)?
                        .GetMixFormat()?;
                    let (sample_rate, channels) = ((*mix).nSamplesPerSec, (*mix).nChannels);
                    CoTaskMemFree(Some(mix as _));
                    Ok((id, name?, sample_rate, channels))
                };
                // Leave out an endpoint that won't answer (a flaky driver, or one
                // going away right now): failing the whole list would leave the
                // engine without the device it actually wants.
                let Ok((id, name, sample_rate, channels)) = describe() else {
                    continue;
                };
                devices.push(Device {
                    default: default.as_ref() == Some(&id),
                    default_comms: comms.as_ref() == Some(&id),
                    id,
                    name,
                    direction,
                    sample_rate,
                    channels,
                });
            }
        }
        Ok(devices)
    }
}

/// Why `select` found no device.
#[derive(Debug)]
pub enum SelectError {
    /// Nothing of this direction is connected, so Windows has no default.
    NoDevice,
    /// The device is known but unplugged, disabled or not present.
    Disconnected,
    /// No device of this direction has this ID.
    UnknownId,
    Windows(windows::core::Error),
}

impl From<windows::core::Error> for SelectError {
    fn from(e: windows::core::Error) -> Self {
        Self::Windows(e)
    }
}

/// The device to use for `direction`: the one with `id`, or for `None` the
/// Windows default communications device, the one Windows designates for calls.
pub fn select(direction: Direction, id: Option<&str>) -> std::result::Result<Device, SelectError> {
    let mut active = list()?.into_iter().filter(|d| d.direction == direction);
    let Some(id) = id else {
        // Never capture from ClearVoice's own virtual microphone: it carries the
        // engine's own output, so using it as the source would feed back on
        // itself. If it is the Windows default, there is no safe default to pick.
        return active
            .find(|d| d.default_comms)
            .filter(|d| !is_own(d))
            .ok_or(SelectError::NoDevice);
    };
    if let Some(device) = active.find(|d| d.id.eq_ignore_ascii_case(id)) {
        return Ok(device);
    }
    if all_ids(direction)?
        .iter()
        .any(|known| known.eq_ignore_ascii_case(id))
    {
        Err(SelectError::Disconnected)
    } else {
        Err(SelectError::UnknownId)
    }
}

/// True for ClearVoice's own virtual endpoints, which must never be used as a
/// physical capture source.
// ponytail: matches the friendly name, which a user can rename. Match the
// devnode hardware ID (PKEY_Device_InstanceId) once the driver ships.
fn is_own(d: &Device) -> bool {
    d.name.contains("YAL ClearVoice")
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: plain COM setup calls.
    unsafe {
        // Result ignored: a thread that already initialized COM, either apartment, is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
}

/// IDs of every `direction` device Windows knows, connected or not.
fn all_ids(direction: Direction) -> Result<Vec<String>> {
    let flow = if direction == Direction::Input {
        eCapture
    } else {
        eRender
    };
    // SAFETY: as in `list`.
    unsafe {
        let all = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE(DEVICE_STATEMASK_ALL))?;
        (0..all.GetCount()?)
            .map(|i| Ok(take(all.Item(i)?.GetId()?)))
            .collect()
    }
}

/// Copies a COM-allocated string and frees it.
unsafe fn take(p: PWSTR) -> String {
    unsafe {
        let s = String::from_utf16_lossy(p.as_wide());
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic: cargo test lists_devices -- --nocapture
    #[test]
    #[cfg_attr(miri, ignore)] // calls into Windows
    fn lists_devices() {
        let devices = list().unwrap();
        if devices.is_empty() {
            println!("no active audio devices");
        }
        for d in &devices {
            println!(
                "{:<6} {:<7} {:<5} {:>6} Hz {} ch  {}  {}",
                format!("{:?}", d.direction),
                if d.default { "default" } else { "" },
                if d.default_comms { "comms" } else { "" },
                d.sample_rate,
                d.channels,
                d.name,
                d.id
            );
            assert!(!d.id.is_empty() && !d.name.is_empty());
            assert!(d.sample_rate > 0 && d.channels > 0);
        }
        for dir in [Direction::Input, Direction::Output] {
            let defaults = devices
                .iter()
                .filter(|d| d.direction == dir && d.default)
                .count();
            assert!(defaults <= 1, "{dir:?} has {defaults} default devices");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn selects_default_and_explicit_devices() {
        let devices = list().unwrap();
        for dir in [Direction::Input, Direction::Output] {
            let comms = devices
                .iter()
                .find(|d| d.direction == dir && d.default_comms);
            match (select(dir, None), comms) {
                (Ok(got), Some(want)) => assert_eq!(got.id, want.id),
                (Err(SelectError::NoDevice), None) => {}
                (got, want) => panic!("{dir:?} default: got {got:?}, expected {want:?}"),
            }
            for d in devices.iter().filter(|d| d.direction == dir) {
                assert_eq!(select(dir, Some(&d.id)).unwrap().id, d.id);
                assert_eq!(select(dir, Some(&d.id.to_uppercase())).unwrap().id, d.id);
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn rejects_unknown_and_wrong_direction_ids() {
        let unknown = select(Direction::Input, Some("not-a-device"));
        assert!(matches!(unknown, Err(SelectError::UnknownId)));
        assert!(matches!(
            select(Direction::Output, Some("")),
            Err(SelectError::UnknownId)
        ));
        for d in list().unwrap() {
            let other = match d.direction {
                Direction::Input => Direction::Output,
                Direction::Output => Direction::Input,
            };
            let wrong = select(other, Some(&d.id));
            assert!(matches!(wrong, Err(SelectError::UnknownId)), "{}", d.name);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn reports_disconnected_devices() {
        // Uses whatever inactive endpoints this PC has: unplugged jacks, disabled devices.
        let active: Vec<String> = list().unwrap().into_iter().map(|d| d.id).collect();
        let mut checked = 0;
        for dir in [Direction::Input, Direction::Output] {
            for id in all_ids(dir)
                .unwrap()
                .into_iter()
                .filter(|id| !active.contains(id))
            {
                let got = select(dir, Some(&id));
                assert!(
                    matches!(got, Err(SelectError::Disconnected)),
                    "{id}: {got:?}"
                );
                checked += 1;
            }
        }
        println!("checked {checked} disconnected devices");
    }
}
