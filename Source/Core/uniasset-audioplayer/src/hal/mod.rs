//! Hardware Abstraction Layer for cross-platform audio output.
//!
//! This module defines the [`AudioManager`] and [`AudioDevice`] traits that
//! abstract over platform-specific audio APIs (CoreAudio, WASAPI, Oboe).
//!
//! # Pull Model
//!
//! The audio device "pulls" PCM samples from the application via the
//! [`AudioManager::pull`] method whenever the OS audio subsystem needs
//! data to fill its output buffer. The callback should produce samples
//! directly on the audio thread — no intermediate buffering is mandated
//! by the HAL.
//!
//! ```text
//! Mixer / AudioStream          Audio Callback             OS Audio
//!        │                         │                         │
//!        │<──── pull(buffer) ──────│                         │
//!        │── fill buffer ─────────>│                         │
//!        │                         │── output samples ──────>│
//! ```
//!
//! # Hardware Format
//!
//! [`open_device`] auto-detects the hardware's native format (lowest
//! latency). Call [`AudioDevice::format`] to read the actual sample rate
//! and channel count. The callback's `buffer` size is always a multiple
//! of `format().channels`.

mod device;
mod manager;
pub mod platform;
use crate::error::AudioError;
pub use device::*;
pub use manager::*;

/// Open the default audio output device.
///
/// The hardware format (sample rate + channels) is auto-detected for
/// lowest latency. Call [`AudioDevice::format`] to read it.
///
/// Selects the appropriate platform backend at compile time:
///
/// | Platform   | Backend    |
/// |------------|-----------|
/// | macOS/iOS  | CoreAudio |
/// | Windows    | WASAPI    |
/// | Android    | Oboe      |
/// | Other      | Dummy     |
pub fn open_device<M>(manager: M) -> Result<platform::PlatformAudioDevice<M>, AudioError>
where
    M: AudioManager,
{
    platform::PlatformAudioDevice::new(manager)
}
