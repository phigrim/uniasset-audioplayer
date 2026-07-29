use std::sync::Arc;

use crate::AudioFormat;

/// Manager trait for providing PCM audio samples on demand.
///
/// Implementations are invoked by the platform audio thread whenever the
/// output device needs more samples. The manager fills `buffer` with
/// interleaved `f32` samples and returns the number of frames written.
pub trait AudioManager: Send + Sync + 'static {
    /// Called when the audio device needs PCM samples.
    ///
    /// This method takes `&self` (not `&mut self`) so the audio thread can
    /// share the manager without locks. Returning fewer frames than requested,
    /// or returning zero, causes the backend to fill the remainder with silence.
    fn pull(&self, buffer: &mut [f32]) -> usize;

    /// Called after the active output endpoint becomes unusable.
    ///
    /// This is never invoked from the PCM pull callback. Implementations may
    /// prepare their non-real-time state for the endpoint to be rebuilt.
    fn on_device_invalidated(&self) {}

    /// Called when a rebuilt output endpoint reports a different format.
    ///
    /// The format is final for the newly opened endpoint and should be used
    /// for any resampling or channel-layout reconfiguration.
    fn on_device_format_changed(&self, _format: AudioFormat) {}

    /// Called after a replacement output endpoint has been made ready.
    ///
    /// The endpoint can still be paused. This is never invoked from the PCM
    /// pull callback.
    fn on_device_recovered(&self) {}
}

impl<T> AudioManager for Arc<T>
where
    T: AudioManager + ?Sized,
{
    fn pull(&self, buffer: &mut [f32]) -> usize {
        (**self).pull(buffer)
    }

    fn on_device_invalidated(&self) {
        (**self).on_device_invalidated();
    }

    fn on_device_format_changed(&self, format: AudioFormat) {
        (**self).on_device_format_changed(format);
    }

    fn on_device_recovered(&self) {
        (**self).on_device_recovered();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::AudioManager;
    use crate::AudioFormat;

    struct RecordingManager {
        invalidated: AtomicUsize,
        format_changes: AtomicUsize,
        recovered: AtomicUsize,
    }

    impl AudioManager for RecordingManager {
        fn pull(&self, _buffer: &mut [f32]) -> usize {
            0
        }

        fn on_device_invalidated(&self) {
            self.invalidated.fetch_add(1, Ordering::Relaxed);
        }

        fn on_device_format_changed(&self, _format: AudioFormat) {
            self.format_changes.fetch_add(1, Ordering::Relaxed);
        }

        fn on_device_recovered(&self) {
            self.recovered.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn arc_forwards_device_events() {
        let manager = Arc::new(RecordingManager {
            invalidated: AtomicUsize::new(0),
            format_changes: AtomicUsize::new(0),
            recovered: AtomicUsize::new(0),
        });

        manager.on_device_invalidated();
        manager.on_device_format_changed(AudioFormat::new(44_100, 2));
        manager.on_device_recovered();

        assert_eq!(manager.invalidated.load(Ordering::Relaxed), 1);
        assert_eq!(manager.format_changes.load(Ordering::Relaxed), 1);
        assert_eq!(manager.recovered.load(Ordering::Relaxed), 1);
    }
}
