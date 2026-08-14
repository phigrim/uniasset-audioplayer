//! macOS CoreAudio endpoint and default-output monitoring.

use std::ffi::c_void;
use std::sync::Arc;

use coreaudio_sys::{
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, kAudioUnitSubType_DefaultOutput,
    kAudioUnitType_Output, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioStreamBasicDescription,
    AudioUnitGetProperty,
};

use crate::error::AudioError;
use crate::types::AudioFormat;

use super::{configure_float_output, default_format, RebuildSignal, NO_ERR};

const AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = u32::from_be_bytes(*b"dOut");
const AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");

pub(super) struct RouteMonitor {
    signal: Arc<RebuildSignal>,
}

impl RouteMonitor {
    pub(super) fn install(signal: Arc<RebuildSignal>) -> Result<Self, AudioError> {
        let address = default_output_property_address();
        let status = unsafe {
            sys::AudioObjectAddPropertyListener(
                AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                default_output_device_changed,
                Arc::as_ptr(&signal) as *mut c_void,
            )
        };
        if status != NO_ERR {
            return Err(AudioError::BackendError(format!(
                "AudioObjectAddPropertyListener failed: {status}"
            )));
        }
        Ok(Self { signal })
    }
}

impl Drop for RouteMonitor {
    fn drop(&mut self) {
        let address = default_output_property_address();
        unsafe {
            let _ = sys::AudioObjectRemovePropertyListener(
                AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                default_output_device_changed,
                Arc::as_ptr(&self.signal) as *mut c_void,
            );
        }
    }
}

pub(super) fn open_audio_unit(
    signal: Arc<RebuildSignal>,
) -> Result<(sys::AudioUnit, AudioFormat), AudioError> {
    let description = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_DefaultOutput,
        componentManufacturer: 0x6170_706c,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
    if component.is_null() {
        return Err(AudioError::DeviceNotFound);
    }

    let mut audio_unit = std::ptr::null_mut();
    let status = unsafe { AudioComponentInstanceNew(component, &mut audio_unit) };
    if status != NO_ERR || audio_unit.is_null() {
        return Err(AudioError::DeviceBusy);
    }

    let format = query_hardware_format(audio_unit);
    if let Err(error) = configure_float_output(audio_unit, format) {
        unsafe { AudioComponentInstanceDispose(audio_unit) };
        return Err(error);
    }

    let status = unsafe {
        super::sys::AudioUnitAddPropertyListener(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            super::audio_unit_property_changed,
            Arc::as_ptr(&signal) as *mut c_void,
        )
    };
    if status != NO_ERR {
        unsafe { AudioComponentInstanceDispose(audio_unit) };
        return Err(AudioError::BackendError(format!(
            "AudioUnitAddPropertyListener failed: {status}"
        )));
    }

    Ok((audio_unit, format))
}

fn query_hardware_format(audio_unit: sys::AudioUnit) -> AudioFormat {
    let mut description: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let status = unsafe {
        AudioUnitGetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &mut description as *mut _ as *mut c_void,
            &mut size,
        )
    };
    if status == NO_ERR
        && description.mSampleRate.is_finite()
        && description.mSampleRate > 0.0
        && description.mChannelsPerFrame > 0
        && description.mChannelsPerFrame <= u16::MAX as u32
    {
        AudioFormat::new(
            description.mSampleRate as u32,
            description.mChannelsPerFrame as u16,
        )
    } else {
        default_format()
    }
}

fn default_output_property_address() -> sys::AudioObjectPropertyAddress {
    sys::AudioObjectPropertyAddress {
        selector: AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
        scope: AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: 0,
    }
}

extern "C" fn default_output_device_changed(
    user_data: *mut c_void,
    _object_id: u32,
    _address_count: u32,
    _addresses: *const sys::AudioObjectPropertyAddress,
) -> i32 {
    if !user_data.is_null() {
        let signal = unsafe { &*(user_data.cast::<RebuildSignal>()) };
        signal.request();
    }
    NO_ERR
}

mod sys {
    pub use coreaudio_sys::*;

    #[repr(C)]
    pub struct AudioObjectPropertyAddress {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    pub type AudioObjectPropertyListenerProc = extern "C" fn(
        user_data: *mut std::ffi::c_void,
        object_id: u32,
        address_count: u32,
        addresses: *const AudioObjectPropertyAddress,
    ) -> i32;

    extern "C" {
        pub fn AudioObjectAddPropertyListener(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            listener: AudioObjectPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
        pub fn AudioObjectRemovePropertyListener(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            listener: AudioObjectPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
    }
}
