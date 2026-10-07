use hbb_common::{anyhow::anyhow, log, ResultType};
use std::ffi::c_void;
use windows::{
    core::{IUnknown_Vtbl, Interface, GUID, HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE},
        Media::Audio::{
            eCapture, eCommunications, eConsole, eMultimedia, ERole, IMMDeviceEnumerator,
            MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
        },
    },
};
const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];
const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
const POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
// Same PolicyConfig ABI as AudioDeviceCmdlets. The ten unused slots are never called.
#[repr(transparent)]
#[derive(Clone)]
struct PolicyConfig(windows::core::IUnknown);
// Transparent IUnknown ownership with the native PolicyConfig IID and matching vtable.
unsafe impl Interface for PolicyConfig {
    type Vtable = PolicyConfigVTable;
    const IID: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
}
#[repr(C)]
struct PolicyConfigVTable {
    base: IUnknown_Vtbl,
    unused: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    set_endpoint_visibility: usize,
}
struct Apartment(bool);
impl Apartment {
    fn enter() -> ResultType<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        result.ok()?;
        Ok(Self(true))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
fn take_string(value: PWSTR) -> ResultType<String> {
    if value.is_null() {
        return Err(anyhow!("Windows returned an empty audio endpoint string"));
    }
    // These strings come from GetId or PropVariantToStringAlloc; both transfer CoTaskMem ownership.
    let result = unsafe { value.to_string() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    let text = result?;
    if text.is_empty() {
        return Err(anyhow!("Windows returned an empty audio endpoint string"));
    }
    Ok(text)
}
fn default_id(devices: &IMMDeviceEnumerator, role: ERole) -> ResultType<String> {
    take_string(unsafe { devices.GetDefaultAudioEndpoint(eCapture, role)?.GetId()? })
}
fn set_default(policy: &PolicyConfig, id: &str, role: ERole) -> ResultType<()> {
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    // IID fixes the vtable ABI; the UTF-16 buffer remains alive until the call returns.
    unsafe {
        (policy.vtable().set_default_endpoint)(policy.as_raw(), PCWSTR(wide.as_ptr()), role)
            .ok()?;
    }
    Ok(())
}
pub struct Route {
    previous: [String; 3],
    target: String,
    restored: bool,
}
impl Route {
    pub fn open() -> ResultType<Self> {
        let _apartment = Apartment::enter()?;
        let devices: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let policy: PolicyConfig = unsafe { CoCreateInstance(&POLICY_CONFIG, None, CLSCTX_ALL)? };
        let endpoints = unsafe { devices.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)? };
        let mut target = None;
        for index in 0..unsafe { endpoints.GetCount()? } {
            let endpoint = unsafe { endpoints.Item(index)? };
            let store = unsafe { endpoint.OpenPropertyStore(STGM_READ)? };
            let mut property = unsafe { store.GetValue(&FRIENDLY_NAME)? };
            let name = unsafe { PropVariantToStringAlloc(&property) }
                .map_err(hbb_common::anyhow::Error::from)
                .and_then(take_string);
            unsafe { PropVariantClear(&mut property)? };
            if name?.starts_with("CABLE Output") {
                if target.is_some() {
                    return Err(anyhow!(
                        "Exactly one active VB-CABLE recording endpoint is required"
                    ));
                }
                target = Some(take_string(unsafe { endpoint.GetId()? })?);
            }
        }
        let target = target.ok_or_else(|| {
            anyhow!("Install VB-CABLE: no active CABLE Output recording endpoint")
        })?;
        let route = Self {
            previous: [
                default_id(&devices, ROLES[0])?,
                default_id(&devices, ROLES[1])?,
                default_id(&devices, ROLES[2])?,
            ],
            target,
            restored: false,
        };
        let switch = || -> ResultType<()> {
            for (role, previous) in ROLES.iter().zip(&route.previous) {
                if previous != &route.target {
                    set_default(&policy, &route.target, *role)?;
                }
                if default_id(&devices, *role)? != route.target {
                    return Err(anyhow!("VB-CABLE capture switch was not applied"));
                }
            }
            Ok(())
        };
        if let Err(error) = switch() {
            route.restore_roles(&devices, &policy);
            return Err(error);
        }
        Ok(route)
    }
    pub fn output(&self) -> &str {
        "CABLE Input"
    }
    fn restore_roles(&self, devices: &IMMDeviceEnumerator, policy: &PolicyConfig) {
        for (role, previous) in ROLES.iter().zip(&self.previous) {
            let restore = || -> ResultType<()> {
                if default_id(devices, *role)? == self.target && previous != &self.target {
                    set_default(policy, previous, *role)?;
                }
                Ok(())
            };
            if let Err(error) = restore() {
                log::warn!("Could not restore microphone role {}: {error}", role.0);
            }
        }
    }
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        let restore = || -> ResultType<()> {
            let _apartment = Apartment::enter()?;
            let devices: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
            let policy: PolicyConfig =
                unsafe { CoCreateInstance(&POLICY_CONFIG, None, CLSCTX_ALL)? };
            self.restore_roles(&devices, &policy);
            Ok(())
        };
        if let Err(error) = restore() {
            log::warn!("Could not restore forwarded microphone: {error}");
        }
        self.restored = true;
    }
}
