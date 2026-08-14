//! iOS CoreAudio endpoint, RemoteIO setup and AVAudioSession monitoring.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use block2::RcBlock;
use coreaudio_sys::{
    kAudioOutputUnitProperty_EnableIO, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_RemoteIO, kAudioUnitType_Output,
    AudioComponentDescription, AudioComponentFindNext, AudioComponentInstanceDispose,
    AudioComponentInstanceNew, AudioStreamBasicDescription, AudioUnitGetProperty,
    AudioUnitSetProperty,
};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue, NSString};

use crate::error::AudioError;
use crate::types::AudioFormat;

use super::{configure_float_output, default_format, RebuildSignal, NO_ERR};

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

const AV_AUDIO_SESSION_CLASS: &std::ffi::CStr = c"AVAudioSession";

pub(super) struct RouteMonitor {
    center: Retained<NSNotificationCenter>,
    observers:
        Vec<Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>,
    _names: Vec<Retained<NSString>>,
    _session: SessionGuard,
}

impl RouteMonitor {
    pub(super) fn install(signal: Arc<RebuildSignal>) -> Result<Self, AudioError> {
        let session = SessionGuard::prepare()?;
        let center = NSNotificationCenter::defaultCenter();
        let names = vec![
            NSString::from_str("AVAudioSessionRouteChangeNotification"),
            NSString::from_str("AVAudioSessionInterruptionNotification"),
            NSString::from_str("AVAudioSessionMediaServicesWereResetNotification"),
        ];
        let mut observers = Vec::with_capacity(3);
        for name in &names {
            let signal = Arc::clone(&signal);
            let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                signal.request();
            });
            let observer = unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(name),
                    None,
                    Option::<&NSOperationQueue>::None,
                    &block,
                )
            };
            observers.push(observer);
        }

        Ok(Self {
            center,
            observers,
            _names: names,
            _session: session,
        })
    }
}

impl Drop for RouteMonitor {
    fn drop(&mut self) {
        for observer in &self.observers {
            unsafe { self.center.removeObserver(observer.as_ref()) };
        }
        self.observers.clear();
    }
}

struct SessionGuard {
    session: Retained<AnyObject>,
    changed: bool,
    previous_category: Option<Retained<objc2_foundation::NSString>>,
    previous_mode: Option<Retained<objc2_foundation::NSString>>,
    previous_options: usize,
}

impl SessionGuard {
    fn prepare() -> Result<Self, AudioError> {
        let class = AnyClass::get(AV_AUDIO_SESSION_CLASS).ok_or(AudioError::DeviceNotFound)?;
        let session: Retained<AnyObject> = unsafe { msg_send![class, sharedInstance] };
        let is_active: bool = unsafe { msg_send![&*session, isActive] };
        if is_active {
            return Ok(Self {
                session,
                changed: false,
                previous_category: None,
                previous_mode: None,
                previous_options: 0,
            });
        }

        let previous_category: Retained<objc2_foundation::NSString> =
            unsafe { msg_send![&*session, category] };
        let previous_mode: Retained<objc2_foundation::NSString> =
            unsafe { msg_send![&*session, mode] };
        let previous_options: usize = unsafe { msg_send![&*session, categoryOptions] };

        let category = objc2_foundation::NSString::from_str("AVAudioSessionCategoryPlayback");
        let mode = objc2_foundation::NSString::from_str("AVAudioSessionModeDefault");
        let guard = Self {
            session,
            changed: true,
            previous_category: Some(previous_category),
            previous_mode: Some(previous_mode),
            previous_options,
        };
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let configured: bool = unsafe {
            msg_send![
                &*guard.session,
                setCategory: &*category,
                mode: &*mode,
                options: 0usize,
                error: &mut error
            ]
        };
        if !configured {
            drop(guard);
            return Err(AudioError::BackendError(
                "AVAudioSession setCategory failed".into(),
            ));
        }

        let active: bool =
            unsafe { msg_send![&*guard.session, setActive: true, error: &mut error] };
        if !active {
            drop(guard);
            return Err(AudioError::BackendError(
                "AVAudioSession setActive failed".into(),
            ));
        }

        Ok(guard)
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.changed {
            return;
        }

        let Some(category) = self.previous_category.take() else {
            return;
        };
        let Some(mode) = self.previous_mode.take() else {
            return;
        };
        let mut error: *mut AnyObject = std::ptr::null_mut();
        unsafe {
            let _: bool = msg_send![
                &*self.session,
                setCategory: &*category,
                mode: &*mode,
                options: self.previous_options,
                error: &mut error
            ];
            let _: bool = msg_send![&*self.session, setActive: false, error: &mut error];
        }
    }
}

pub(super) fn open_audio_unit(
    signal: Arc<RebuildSignal>,
) -> Result<(sys::AudioUnit, AudioFormat), AudioError> {
    let description = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_RemoteIO,
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

    let enable: u32 = 1;
    let status = unsafe {
        AudioUnitSetProperty(
            audio_unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            0,
            &enable as *const _ as *const c_void,
            std::mem::size_of_val(&enable) as u32,
        )
    };
    if status != NO_ERR {
        unsafe { AudioComponentInstanceDispose(audio_unit) };
        return Err(AudioError::DeviceNotFound);
    }

    // Playback-only RemoteIO: keep output enabled and disable the input bus
    // so the session never opens a microphone path for this crate.
    let disable: u32 = 0;
    let status = unsafe {
        AudioUnitSetProperty(
            audio_unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            1,
            &disable as *const _ as *const c_void,
            std::mem::size_of_val(&disable) as u32,
        )
    };
    if status != NO_ERR {
        unsafe { AudioComponentInstanceDispose(audio_unit) };
        return Err(AudioError::DeviceNotFound);
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

mod sys {
    pub use coreaudio_sys::*;
}
