//! Platform detection and backend selection.
//!
//! Conditionally compiles the appropriate audio backend for the target OS.

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod coreaudio;

#[cfg(target_os = "windows")]
mod wasapi;
#[cfg(target_os = "windows")]
pub use wasapi::WasapiDevice;

#[cfg(target_os = "android")]
mod oboe;
#[cfg(target_os = "android")]
pub use oboe::OboeDevice;

#[cfg(target_os = "ohos")]
mod ohos;
#[cfg(target_os = "ohos")]
pub use ohos::OhosDevice;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "android",
    target_os = "ohos"
)))]
mod dummy;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "android",
    target_os = "ohos"
)))]
pub use dummy::DummyDevice;

/// A platform-appropriate audio device with static dispatch to the selected
/// native backend.
pub enum PlatformAudioDevice<M: AudioManager> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    CoreAudio(coreaudio::CoreAudioDevice<M>),
    #[cfg(target_os = "windows")]
    Wasapi(WasapiDevice<M>),
    #[cfg(target_os = "android")]
    Oboe(OboeDevice<M>),
    #[cfg(target_os = "ohos")]
    Ohos(OhosDevice<M>),
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        target_os = "android",
        target_os = "ohos"
    )))]
    Dummy(DummyDevice<M>),
}

impl<M: AudioManager> AudioDevice for PlatformAudioDevice<M> {
    fn format(&self) -> crate::types::AudioFormat {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::CoreAudio(device) => device.format(),
            #[cfg(target_os = "windows")]
            Self::Wasapi(device) => device.format(),
            #[cfg(target_os = "android")]
            Self::Oboe(device) => device.format(),
            #[cfg(target_os = "ohos")]
            Self::Ohos(device) => device.format(),
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
                target_os = "ohos"
            )))]
            Self::Dummy(device) => device.format(),
        }
    }

    fn start(&mut self) -> Result<(), AudioError> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::CoreAudio(device) => device.start(),
            #[cfg(target_os = "windows")]
            Self::Wasapi(device) => device.start(),
            #[cfg(target_os = "android")]
            Self::Oboe(device) => device.start(),
            #[cfg(target_os = "ohos")]
            Self::Ohos(device) => device.start(),
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
                target_os = "ohos"
            )))]
            Self::Dummy(device) => device.start(),
        }
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::CoreAudio(device) => device.stop(),
            #[cfg(target_os = "windows")]
            Self::Wasapi(device) => device.stop(),
            #[cfg(target_os = "android")]
            Self::Oboe(device) => device.stop(),
            #[cfg(target_os = "ohos")]
            Self::Ohos(device) => device.stop(),
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
                target_os = "ohos"
            )))]
            Self::Dummy(device) => device.stop(),
        }
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::CoreAudio(device) => device.pause(),
            #[cfg(target_os = "windows")]
            Self::Wasapi(device) => device.pause(),
            #[cfg(target_os = "android")]
            Self::Oboe(device) => device.pause(),
            #[cfg(target_os = "ohos")]
            Self::Ohos(device) => device.pause(),
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
                target_os = "ohos"
            )))]
            Self::Dummy(device) => device.pause(),
        }
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::CoreAudio(device) => device.resume(),
            #[cfg(target_os = "windows")]
            Self::Wasapi(device) => device.resume(),
            #[cfg(target_os = "android")]
            Self::Oboe(device) => device.resume(),
            #[cfg(target_os = "ohos")]
            Self::Ohos(device) => device.resume(),
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "android",
                target_os = "ohos"
            )))]
            Self::Dummy(device) => device.resume(),
        }
    }
}

impl<M: AudioManager> PlatformAudioDevice<M> {
    /// Create a platform-appropriate audio device.
    /// The device auto-detects the hardware's native format.
    pub fn new(manager: M) -> Result<Self, AudioError> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            return coreaudio::CoreAudioDevice::new(manager).map(PlatformAudioDevice::CoreAudio);
        }

        #[cfg(target_os = "windows")]
        {
            return WasapiDevice::new(manager).map(PlatformAudioDevice::Wasapi);
        }

        #[cfg(target_os = "android")]
        {
            return OboeDevice::new(manager).map(PlatformAudioDevice::Oboe);
        }

        #[cfg(target_os = "ohos")]
        {
            return OhosDevice::new(manager).map(PlatformAudioDevice::Ohos);
        }

        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "windows",
            target_os = "android",
            target_os = "ohos"
        )))]
        {
            return Ok(PlatformAudioDevice::Dummy(DummyDevice::new(manager)));
        }
    }
}
