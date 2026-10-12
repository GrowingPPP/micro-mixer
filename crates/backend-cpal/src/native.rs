//! Native identity inventory, in the same system enumeration order used by cpal
//! 0.15.3. The caller verifies two snapshots around cpal enumeration so a hotplug
//! cannot associate an endpoint ID with a different audio device.
use anyhow::Result;
use audio_core::device::{Direction, StreamFormat};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub id: String,
    pub name: String,
    pub default_input: bool,
    pub default_output: bool,
    /// Direction and metadata come from native properties, never audio-unit/client activation.
    pub input: Option<StreamFormat>,
    pub output: Option<StreamFormat>,
}
impl Endpoint {
    pub fn format(&self, direction: Direction) -> Option<StreamFormat> {
        match direction {
            Direction::Input => self.input,
            Direction::Output => self.output,
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn wave_format(bytes: &[u8]) -> Option<StreamFormat> {
    if bytes.len() < 16 {
        return None;
    }
    let channels = u16::from_le_bytes(bytes[2..4].try_into().ok()?);
    let sample_rate = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    (channels > 0 && sample_rate > 0).then(|| StreamFormat::new(sample_rate, channels))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::ffi::{c_void, CStr};
    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }
    fn address(selector: &[u8; 4]) -> Address {
        Address {
            selector: u32::from_be_bytes(*selector),
            scope: u32::from_be_bytes(*b"glob"),
            element: 0,
        }
    }
    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            id: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            id: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetLength(value: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            value: *const c_void,
            data: *mut i8,
            size: isize,
            encoding: u32,
        ) -> bool;
        fn CFRelease(value: *const c_void);
    }
    fn number(selector: &[u8; 4]) -> Option<u32> {
        let mut number = 0u32;
        let mut size = 4u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                1,
                &address(selector),
                0,
                std::ptr::null(),
                &mut size,
                (&mut number as *mut u32).cast(),
            )
        };
        (status == 0).then_some(number)
    }
    fn string(id: u32, selector: &[u8; 4]) -> Result<String> {
        let mut value: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &address(selector),
                0,
                std::ptr::null(),
                &mut size,
                (&mut value as *mut *const c_void).cast(),
            )
        };
        anyhow::ensure!(
            status == 0 && !value.is_null(),
            "CoreAudio string query failed ({status})"
        );
        // Both lnam and uid properties transfer a CFString reference to the caller.
        let capacity =
            unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), 0x0800_0100) + 1 };
        let mut buffer = vec![0i8; capacity.max(1) as usize];
        let ok = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                0x0800_0100,
            )
        };
        unsafe { CFRelease(value) };
        anyhow::ensure!(ok, "CoreAudio string conversion failed");
        Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }
    fn channels(id: u32, scope: &[u8; 4]) -> Result<u16> {
        // kAudioDevicePropertyStreamConfiguration is a passive HAL property.
        // CPAL's config queries additionally initialize an AudioUnit, so do not use them here.
        let mut addr = address(b"slay");
        addr.scope = u32::from_be_bytes(*scope);
        let mut size = 0u32;
        let status =
            unsafe { AudioObjectGetPropertyDataSize(id, &addr, 0, std::ptr::null(), &mut size) };
        anyhow::ensure!(status == 0, "CoreAudio channel query failed ({status})");
        let mut bytes = vec![0u8; size as usize];
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                bytes.as_mut_ptr().cast(),
            )
        };
        anyhow::ensure!(status == 0, "CoreAudio channel query failed ({status})");
        parse_channels(&bytes).ok_or_else(|| anyhow::anyhow!("CoreAudio channel list is invalid"))
    }
    pub(super) fn parse_channels(bytes: &[u8]) -> Option<u16> {
        #[repr(C)]
        struct AudioBuffer {
            channels: u32,
            byte_size: u32,
            data: *mut c_void,
        }
        let count = u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        let alignment = std::mem::align_of::<AudioBuffer>();
        let header = (4 + alignment - 1) & !(alignment - 1);
        let stride = std::mem::size_of::<AudioBuffer>();
        let mut total = 0u32;
        for index in 0..count {
            let offset = header.checked_add(index.checked_mul(stride)?)?;
            let n = u32::from_ne_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?);
            total = total.checked_add(n)?;
        }
        u16::try_from(total).ok()
    }
    fn sample_rate(id: u32) -> Result<u32> {
        // kAudioDevicePropertyNominalSampleRate, shared by the device's directions.
        let addr = address(b"nsrt");
        let mut rate = 0f64;
        let mut size = std::mem::size_of::<f64>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                id,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                (&mut rate as *mut f64).cast(),
            )
        };
        anyhow::ensure!(status == 0, "CoreAudio sample-rate query failed ({status})");
        anyhow::ensure!(
            rate.is_finite() && rate >= 0.0 && rate <= u32::MAX as f64,
            "CoreAudio sample rate is invalid"
        );
        Ok(rate.round() as u32)
    }
    fn device_objects() -> Result<Vec<u32>> {
        let addr = address(b"dev#");
        let mut size = 0;
        let status =
            unsafe { AudioObjectGetPropertyDataSize(1, &addr, 0, std::ptr::null(), &mut size) };
        anyhow::ensure!(status == 0, "CoreAudio enumeration failed ({status})");
        let mut ids = vec![0u32; size as usize / 4];
        let status = unsafe {
            AudioObjectGetPropertyData(
                1,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                ids.as_mut_ptr().cast(),
            )
        };
        anyhow::ensure!(status == 0, "CoreAudio enumeration failed ({status})");
        ids.truncate(size as usize / 4);
        Ok(ids)
    }
    pub(super) fn available_ids() -> Result<Vec<String>> {
        device_objects()?
            .into_iter()
            .map(|id| Ok(format!("coreaudio:{}", string(id, b"uid ")?)))
            .collect()
    }
    pub(super) fn default_id(direction: Direction) -> Result<Option<String>> {
        let selector = match direction {
            Direction::Input => b"dIn ",
            Direction::Output => b"dOut",
        };
        number(selector)
            .filter(|id| *id != 0)
            .map(|id| string(id, b"uid ").map(|uid| format!("coreaudio:{uid}")))
            .transpose()
    }
    pub(super) fn inventory() -> Result<Vec<Endpoint>> {
        let default_input = number(b"dIn ");
        let default_output = number(b"dOut");
        device_objects()?
            .into_iter()
            .map(|id| {
                let input_channels = channels(id, b"inpt")?;
                let output_channels = channels(id, b"outp")?;
                let rate = if input_channels > 0 || output_channels > 0 {
                    sample_rate(id)?
                } else {
                    0
                };
                Ok(Endpoint {
                    id: format!("coreaudio:{}", string(id, b"uid ")?),
                    name: string(id, b"lnam")?,
                    default_input: default_input == Some(id),
                    default_output: default_output == Some(id),
                    input: (input_channels > 0).then(|| StreamFormat::new(rate, input_channels)),
                    output: (output_channels > 0).then(|| StreamFormat::new(rate, output_channels)),
                })
            })
            .collect()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::{
        Devices::Properties::DEVPKEY_Device_FriendlyName,
        Foundation::RPC_E_CHANGED_MODE,
        Media::Audio::{
            eAll, eCapture, eConsole, eRender, IMMDevice, IMMDeviceEnumerator, IMMEndpoint,
            MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat, DEVICE_STATE_ACTIVE,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                StructuredStorage::PropVariantClear, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
                STGM_READ,
            },
            Variant::{VT_BLOB, VT_LPWSTR},
        },
    };
    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
    thread_local! { static COM_INITIALIZED: std::result::Result<ComGuard, windows::core::HRESULT> = {
        let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if init.is_ok() || init == RPC_E_CHANGED_MODE { Ok(ComGuard(init.is_ok())) } else { Err(init) }
    }; }
    fn endpoint_id(device: &IMMDevice) -> Result<String> {
        let value = unsafe { device.GetId()? };
        let result = unsafe { value.to_string() };
        unsafe { CoTaskMemFree(Some(value.0.cast())) };
        Ok(format!("wasapi:{}", result?))
    }
    fn name(device: &IMMDevice) -> Result<String> {
        let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
        let mut property =
            unsafe { store.GetValue(&DEVPKEY_Device_FriendlyName as *const _ as *const _)? };
        let raw = unsafe { &property.as_raw().Anonymous.Anonymous };
        let result = if raw.vt == VT_LPWSTR.0 {
            let pointer = unsafe { *(&raw.Anonymous as *const _ as *const *const u16) };
            if pointer.is_null() {
                Err(anyhow::anyhow!("WASAPI endpoint friendly name is null"))
            } else {
                unsafe { windows::core::PCWSTR(pointer).to_string() }.map_err(anyhow::Error::from)
            }
        } else {
            Err(anyhow::anyhow!(
                "WASAPI endpoint friendly name is not a string"
            ))
        };
        unsafe { PropVariantClear(&mut property)? };
        result
    }
    fn format(device: &IMMDevice) -> Result<Option<StreamFormat>> {
        let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
        let mut property = unsafe { store.GetValue(&PKEY_AudioEngine_DeviceFormat)? };
        let raw = unsafe { &property.as_raw().Anonymous.Anonymous };
        let result = if raw.vt == VT_BLOB.0 {
            let blob = unsafe { raw.Anonymous.blob };
            if blob.pBlobData.is_null() {
                None
            } else {
                wave_format(unsafe {
                    std::slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize)
                })
            }
        } else {
            None
        };
        unsafe { PropVariantClear(&mut property)? };
        Ok(result)
    }
    fn enumerator() -> Result<IMMDeviceEnumerator> {
        COM_INITIALIZED.with(|state| {
            state
                .as_ref()
                .map(|_| ())
                .map_err(|status| anyhow::anyhow!("COM initialization failed: {status:?}"))
        })?;
        Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
    }
    pub(super) fn available_ids() -> Result<Vec<String>> {
        let collection = unsafe { enumerator()?.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)? };
        (0..unsafe { collection.GetCount()? })
            .map(|index| endpoint_id(&unsafe { collection.Item(index)? }))
            .collect()
    }
    pub(super) fn default_id(direction: Direction) -> Result<Option<String>> {
        let flow = match direction {
            Direction::Input => eCapture,
            Direction::Output => eRender,
        };
        unsafe { enumerator()?.GetDefaultAudioEndpoint(flow, eConsole) }
            .ok()
            .map(|device| endpoint_id(&device))
            .transpose()
    }
    pub(super) fn inventory() -> Result<Vec<Endpoint>> {
        let enumerator = enumerator()?;
        let default_input = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }
            .ok()
            .and_then(|d| endpoint_id(&d).ok());
        let default_output = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .ok()
            .and_then(|d| endpoint_id(&d).ok());
        let collection = unsafe { enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)? };
        (0..unsafe { collection.GetCount()? })
            .map(|index| {
                let device = unsafe { collection.Item(index)? };
                let id = endpoint_id(&device)?;
                let endpoint: IMMEndpoint = device.cast()?;
                let direction = unsafe { endpoint.GetDataFlow()? };
                // Keep endpoints visible even if a driver omits the optional format property.
                // Zero metadata means unknown; format negotiation only touches the selected endpoint.
                let format = format(&device)
                    .ok()
                    .flatten()
                    .unwrap_or(StreamFormat::new(0, 0));
                Ok(Endpoint {
                    default_input: default_input.as_ref() == Some(&id),
                    default_output: default_output.as_ref() == Some(&id),
                    id,
                    name: name(&device)?,
                    input: (direction == eCapture).then_some(format),
                    output: (direction == eRender).then_some(format),
                })
            })
            .collect()
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn inventory() -> Result<Vec<Endpoint>> {
    platform::inventory()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn available_ids() -> Result<Vec<String>> {
    platform::available_ids()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn default_id(direction: Direction) -> Result<Option<String>> {
    platform::default_id(direction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_wave_format_extracts_real_rate_and_channels_without_activating_a_client() {
        let mut bytes = [0u8; 40]; // WAVEFORMATEXTENSIBLE contains the same leading fields.
        bytes[..2].copy_from_slice(&0xfffeu16.to_le_bytes());
        bytes[2..4].copy_from_slice(&6u16.to_le_bytes());
        bytes[4..8].copy_from_slice(&44_100u32.to_le_bytes());
        assert_eq!(wave_format(&bytes), Some(StreamFormat::new(44_100, 6)));
        assert_eq!(wave_format(&bytes[..15]), None);
        bytes[2..4].fill(0);
        assert_eq!(wave_format(&bytes), None);
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        bytes[4..8].fill(0);
        assert_eq!(wave_format(&bytes), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hal_buffer_list_counts_direction_channels_and_rejects_truncated_properties() {
        // AudioBufferList has a 4-byte count followed by 4-byte alignment padding
        // on both supported macOS architectures, then 16-byte AudioBuffer entries.
        let mut bytes = vec![0u8; 8 + 16 * 2];
        bytes[..4].copy_from_slice(&2u32.to_ne_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_ne_bytes());
        bytes[24..28].copy_from_slice(&2u32.to_ne_bytes());
        assert_eq!(platform::parse_channels(&bytes), Some(3));
        assert_eq!(platform::parse_channels(&bytes[..26]), None);
        bytes[..4].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(platform::parse_channels(&bytes[..4]), Some(0));
    }
}
