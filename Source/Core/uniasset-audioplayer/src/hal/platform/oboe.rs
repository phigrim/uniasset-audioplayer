//! Oboe backend for Android.
//!
//! Uses the Oboe (AAudio) native audio library with a data callback
//! that pulls from [`AudioManager::pull`].
//!
//! # Architecture
//!
//! ```text
//! AudioManager::pull()  ←  OboeCallback::on_audio_ready()  ←  Oboe Audio Thread
//! ```
//!
//! The stream is created in [`OboeDevice::new`] with a placeholder (null)
//! callback pointer. The real callback is wired in
//! [`AudioDevice::start`](crate::hal::AudioDevice::start) via a
//! [`CallbackPtr`] indirection — the same pattern used by the CoreAudio
//! backend's `CallbackRef`.

use std::cell::UnsafeCell;
use std::panic::{catch_unwind, AssertUnwindSafe};

use oboe::{
    AudioOutputCallback, AudioOutputStream, AudioStream, AudioStreamAsync, AudioStreamBase,
    AudioStreamBuilder, ContentType, DataCallbackResult, Output, PerformanceMode, SharingMode,
    Stereo, Usage,
};

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};
use crate::types::AudioFormat;

/// Default sample rate fallback: 48 kHz.
const DEFAULT_SAMPLE_RATE: i32 = 48000;

/// Default channel count fallback: stereo.
const DEFAULT_CHANNEL_COUNT: u16 = 2;

// ── CallbackPtr ─────────────────────────────────────────────────────────

/// Stores a typed manager pointer with interior mutability.
///
/// Written once in [`OboeDevice::start`] (happens-before the audio thread
/// is started), then read lock-free by the Oboe audio callback.
struct CallbackPtr<M: AudioManager>(UnsafeCell<*const M>);

// Safety: the pointer is written once before the stream starts (happens-before
// all audio callback invocations). After that it is read-only from the audio
// thread. No concurrent read/write access.
unsafe impl<M: AudioManager> Send for CallbackPtr<M> {}
unsafe impl<M: AudioManager> Sync for CallbackPtr<M> {}

impl<M: AudioManager> CallbackPtr<M> {
    fn new() -> Self {
        Self(UnsafeCell::new(std::ptr::null()))
    }

    fn set(&self, ptr: *const M) {
        unsafe {
            *self.0.get() = ptr;
        }
    }

    fn get(&self) -> *const M {
        unsafe { *self.0.get() }
    }
}

// ── OboeCallback ────────────────────────────────────────────────────────

/// Bridges Oboe's [`AudioOutputCallback`] to our pull-based [`AudioManager`].
///
/// Invoked on Oboe's high-priority audio thread. Reads the callback pointer
/// via [`CallbackPtr`] for lock-free access — the same pattern as CoreAudio's
/// `render_callback`.
struct OboeCallback<M: AudioManager> {
    /// Points to a device-owned [`CallbackPtr`] that holds the manager pointer.
    callback_ptr: *const CallbackPtr<M>,
    /// Number of channels in the stream (determined at open time).
    channel_count: usize,
}

// Safety: OboeCallback is moved into the stream and only accessed from
// the audio thread. The raw pointer outlives the stream because the
// device owns the CallbackPtr (declared after `stream` in OboeDevice).
unsafe impl<M: AudioManager> Send for OboeCallback<M> {}

impl<M: AudioManager> AudioOutputCallback for OboeCallback<M> {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        audio_data: &mut [(f32, f32)],
    ) -> DataCallbackResult {
        // Cast [(f32, f32)] stereo-frame slice to interleaved [f32] once.
        // (f32, f32) and [f32; 2] share the same in-memory layout on all
        // Rust targets: two consecutive f32 values matching interleaved PCM.
        //
        // Safety: the total byte length is preserved (audio_data.len() * 8
        // bytes → interleaved.len() * 4 bytes, same total).
        let interleaved: &mut [f32] = unsafe {
            std::slice::from_raw_parts_mut(
                audio_data.as_mut_ptr() as *mut f32,
                audio_data.len() * 2,
            )
        };

        // Resolve the callback through the indirection.
        if self.callback_ptr.is_null() {
            interleaved.fill(0.0);
            return DataCallbackResult::Continue;
        }

        let callback_ptr: &CallbackPtr<M> = unsafe { &*self.callback_ptr };
        let manager_ptr = callback_ptr.get();

        if manager_ptr.is_null() {
            interleaved.fill(0.0);
            return DataCallbackResult::Continue;
        }

        // Capture by-value copies for the catch_unwind closure so we don't
        // capture `&mut self` (which would require AssertUnwindSafe).
        let channel_count = self.channel_count;

        // Wrap the callback call in catch_unwind so a panic in user code
        // doesn't unwind through the C AAudio stack frames (UB).
        let result = catch_unwind(AssertUnwindSafe(|| {
            // Safety: manager_ptr is valid for the lifetime of the device, which
            // outlives the stream. AudioManager::pull takes `&self` — no
            // locks needed.
            let manager: &M = unsafe { &*manager_ptr };
            let frames_written = manager.pull(interleaved);

            // Zero-fill remaining samples if the callback wrote fewer frames.
            let written_samples = frames_written * channel_count;
            if written_samples < interleaved.len() {
                interleaved[written_samples..].fill(0.0);
            }
        }));

        // If the callback panicked, zero-fill the entire buffer so the
        // audio output is silent rather than playing uninitialized memory.
        if result.is_err() {
            interleaved.fill(0.0);
        }

        DataCallbackResult::Continue
    }
}

// ── OboeDevice ──────────────────────────────────────────────────────────

/// An audio output device backed by Oboe (AAudio).
///
/// Opens a low-latency stereo output stream with 32-bit float samples.
/// The stream is created in [`new`](OboeDevice::new) and the callback is
/// wired up in [`start`](AudioDevice::start).
///
/// # Field Drop Order
///
/// Fields are dropped in declaration order. `stream` must drop before
/// `manager` and `callback_ptr` to ensure the audio thread has
/// stopped invoking the callback before we free its data.
pub struct OboeDevice<M: AudioManager> {
    /// Hardware format detected at open time.
    format: AudioFormat,
    /// The Oboe audio stream (async, callback-driven).
    stream: Option<AudioStreamAsync<Output, OboeCallback<M>>>,
    /// Indirection for the callback pointer. Written in `start()`, read
    /// by the Oboe audio callback lock-free.
    callback_ptr: Option<Box<CallbackPtr<M>>>,
    /// Owns the manager for the lifetime of the device.
    manager: Box<M>,
    running: bool,
}

// Safety: Oboe stream handles are safe to send between threads.
unsafe impl<M: AudioManager> Send for OboeDevice<M> {}

impl<M: AudioManager> OboeDevice<M> {
    /// Create a new Oboe output device.
    ///
    /// Opens a low-latency stereo output stream with 32-bit float samples.
    /// The actual hardware sample rate and channel count are queried after
    /// opening and exposed via [`AudioDevice::format`].
    ///
    /// The stream is opened with a placeholder (null) callback — the real
    /// callback is wired in [`start`](AudioDevice::start).
    pub fn new(manager: M) -> Result<Self, AudioError> {
        // Shared indirection for the callback pointer (see CallbackPtr docs).
        let callback_ptr = Box::new(CallbackPtr::new());
        let callback_ptr_raw: *const CallbackPtr<M> = &*callback_ptr;

        let channel_count = DEFAULT_CHANNEL_COUNT as usize;

        let oboe_callback = OboeCallback {
            callback_ptr: callback_ptr_raw,
            channel_count,
        };

        // Build and open a low-latency stereo output stream.
        let stream: AudioStreamAsync<Output, OboeCallback<M>> = AudioStreamBuilder::default()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Shared)
            .set_usage(Usage::Media)
            .set_content_type(ContentType::Music)
            .set_format::<f32>()
            .set_channel_count::<Stereo>()
            .set_sample_rate(DEFAULT_SAMPLE_RATE)
            .set_callback(oboe_callback)
            .open_stream()
            .map_err(|e| AudioError::BackendError(format!("failed to open oboe stream: {e}")))?;

        // Query the actual hardware format from the opened stream.
        // We requested f32 stereo, so the channel count is known.
        // Only the sample rate may be adjusted by the system.
        let sample_rate = stream.get_sample_rate().max(1) as u32;
        let format = AudioFormat::new(sample_rate, DEFAULT_CHANNEL_COUNT);

        Ok(Self {
            format,
            stream: Some(stream),
            callback_ptr: Some(callback_ptr),
            manager: Box::new(manager),
            running: false,
        })
    }
}

impl<M: AudioManager> AudioDevice for OboeDevice<M> {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), AudioError> {
        let stream = self.stream.as_mut().ok_or(AudioError::DeviceNotFound)?;

        if self.running {
            return Ok(());
        }

        // Convert the manager Box to a raw pointer and store it
        // through the CallbackPtr indirection for lock-free audio-thread access.
        let manager_ptr: *const M = &*self.manager;
        if let Some(ref cb_ptr) = self.callback_ptr {
            cb_ptr.set(manager_ptr);
        }

        // Start the stream. The audio callback will begin firing immediately.
        stream
            .start_with_timeout(oboe::DEFAULT_TIMEOUT_NANOS)
            .map_err(|e| {
                // Clean up on failure: clear the pointer and drop the callback.
                if let Some(ref cb_ptr) = self.callback_ptr {
                    cb_ptr.set(std::ptr::null());
                }
                AudioError::BackendError(format!("failed to start oboe stream: {e}"))
            })?;

        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(ref mut stream) = self.stream {
            if self.running {
                // Stop the stream, then close it.
                let _ = stream.stop_with_timeout(oboe::DEFAULT_TIMEOUT_NANOS);
                let _ = stream.close();
            }
        }

        // Clear the callback pointer so the audio thread won't access
        // freed memory if a spurious callback fires during teardown.
        if let Some(ref cb_ptr) = self.callback_ptr {
            cb_ptr.set(std::ptr::null());
        }

        self.running = false;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        if let Some(ref mut stream) = self.stream {
            if self.running {
                stream
                    .pause_with_timeout(oboe::DEFAULT_TIMEOUT_NANOS)
                    .map_err(|e| {
                        AudioError::BackendError(format!("failed to pause oboe stream: {e}"))
                    })?;
            }
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        if let Some(ref mut stream) = self.stream {
            if self.running {
                stream
                    .start_with_timeout(oboe::DEFAULT_TIMEOUT_NANOS)
                    .map_err(|e| {
                        AudioError::BackendError(format!("failed to resume oboe stream: {e}"))
                    })?;
            }
        }
        Ok(())
    }
}

impl<M: AudioManager> Drop for OboeDevice<M> {
    fn drop(&mut self) {
        let _ = self.stop();
        // Ensure callbacks are dropped before the stream (fields drop in order).
        self.callback_ptr = None;
    }
}
