use crate::error::AudioError;
use crate::types::AudioFormat;

/// A platform audio output device.
///
/// An `AudioDevice` wraps the platform-specific audio API and drives an
/// [`AudioManager`] on the audio thread.
pub trait AudioDevice: Send {
    /// Return the hardware's actual audio format (sample rate + channels).
    ///
    /// The device auto-detects the best format for lowest latency.
    /// Read this before setting up your audio pipeline.
    fn format(&self) -> AudioFormat;

    /// Start playback. The device will begin calling `manager.pull()` on
    /// the audio thread to fetch samples.
    fn start(&mut self) -> Result<(), AudioError>;

    /// Stop playback and release the audio resources.
    fn stop(&mut self) -> Result<(), AudioError>;

    /// Pause playback. The callback will no longer be invoked.
    fn pause(&mut self) -> Result<(), AudioError>;

    /// Resume playback after a pause.
    fn resume(&mut self) -> Result<(), AudioError>;
}
