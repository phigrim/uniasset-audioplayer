//! High-level audio player that wires the HAL ([`AudioDevice`]) and
//! [`Mixer`] together into a simple playback API.
//!
//! # Architecture
//!
//! ```text
//! AudioStream(s) → Mixer → AudioManager → AudioDevice → OS Audio
//!                    ↑                        ↑
//!              [audio-player-worker]     AudioPlayer (owns both,
//!              (periodic EOF cleanup)     wires them together)
//! ```
//!
//! # Example
//!
//! ```ignore
//! use uniasset::player::AudioPlayer;
//! use uniasset::mixer::AudioStream;
//!
//! let player = AudioPlayer::new().expect("Failed to open audio device");
//! let stream: Arc<dyn AudioStream> = ...;
//! let handle = player.add_stream(stream);
//! handle.set_volume(0.5);
//!
//! // Player starts playing immediately.
//! // EOF streams are cleaned up automatically by the worker thread.
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;

use crate::hal::platform::PlatformAudioDevice;
use crate::hal::{open_device, AudioDevice, AudioManager};
use crate::mixer::{AudioStream, Mixer, PlayHandle};
use crate::AudioError;
use crate::AudioFormat;

/// A high-level audio player that combines platform audio output ([`AudioDevice`])
/// with lock-free mixing ([`Mixer`]).
///
/// `AudioPlayer` opens the default audio device on construction, creates a
/// [`Mixer`] that matches the hardware format, and starts playback immediately.
/// Streams can be added at any time through [`add_stream`](AudioPlayer::add_stream),
/// which returns a [`PlayHandle`] for per-stream control (volume, pause, seek).
///
/// # Lifecycle
///
/// - **Construction** — opens device, starts audio thread, spawns a worker
///   thread (`audio-player-worker`) that periodically removes finished streams.
/// - **Playback** — add streams, control them via [`PlayHandle`].
/// - **Cleanup** — finished (EOF) streams are automatically removed by the
///   worker thread; no manual cleanup is needed.
/// - **Shutdown** — drop the player; the worker thread is signaled to exit
///   and the audio device is stopped.
///
/// # Thread Safety
///
/// All methods take `&self` and can be called from any thread. The internal
/// device is protected by a mutex (uncontended — only used for lifecycle calls).
/// The mixer is lock-free on the audio hot path.
pub struct AudioPlayer {
    /// The mixer, shared with the audio callback running on the device thread.
    mixer: Arc<Mixer>,

    /// The platform audio device. Behind a mutex because `start`/`stop`/
    /// `pause`/`resume` take `&mut self`.
    device: Mutex<PlatformAudioDevice<Arc<Mixer>>>,
    /// Signal to the worker thread that it should exit.
    shutdown: Arc<AtomicBool>,

    /// Handle to the worker thread (`audio-player-worker`).
    /// `Option` so `Drop` can take ownership to join.
    worker: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Open the default audio device and start playback.
    ///
    /// The mixer is automatically configured to match the hardware's native
    /// sample rate and channel count for lowest latency.
    ///
    /// Returns an error if no output device is available or if the device
    /// cannot be started.
    pub fn new() -> Result<Self, AudioError> {
        // The device's initial format is known before it starts invoking pull.
        let mixer = Arc::new(Mixer::new(48_000, 2));
        let mut device = open_device(Arc::clone(&mixer))?;
        let format = device.format();
        mixer.on_device_format_changed(format);
        device.start()?;

        // ── Worker thread for periodic EOF cleanup ────────────────────────
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_mixer = Arc::clone(&mixer);
        let worker_shutdown = Arc::clone(&shutdown);

        let worker = thread::Builder::new()
            .name("audio-player-worker".into())
            .spawn(move || {
                // Check shutdown flag every 10ms; run cleanup every 500ms.
                let mut ticks: u32 = 0;
                while !worker_shutdown.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                    ticks += 1;

                    if ticks.is_multiple_of(50) {
                        // ~500ms elapsed
                        worker_mixer.cleanup_eof();
                    }
                }
            })
            .ok(); // Best-effort: if thread spawn fails, playback still works.

        Ok(Self {
            mixer,
            device: Mutex::new(device),
            shutdown,
            worker,
        })
    }

    /// Return the hardware audio format (sample rate + channels).
    ///
    /// This matches the mixer's target format. Streams with different rates
    /// are automatically resampled.
    pub fn format(&self) -> AudioFormat {
        self.mixer.format()
    }

    /// Add an audio stream to the player.
    ///
    /// Returns a [`PlayHandle`] that can be used to control playback
    /// (volume, pause/resume, seek, per-stream effects).
    ///
    /// The stream will begin playing immediately (unless paused via the handle).
    pub fn add_stream(&self, stream: Arc<dyn AudioStream>, play_immediate: bool) -> PlayHandle {
        self.mixer.add_stream(stream, play_immediate)
    }

    /// Number of streams currently in the mixer.
    pub fn stream_count(&self) -> usize {
        self.mixer.stream_count()
    }

    /// Pause playback (device-level, not per-stream).
    ///
    /// The audio callback will no longer be invoked. Use [`resume`](Self::resume)
    /// to continue playback.
    ///
    /// Returns an error if the underlying device operation fails.
    pub fn pause(&self) -> Result<(), AudioError> {
        self.device.lock().pause()
    }

    /// Resume playback after a [`pause`](Self::pause).
    ///
    /// Returns an error if the underlying device operation fails.
    pub fn resume(&self) -> Result<(), AudioError> {
        self.device.lock().resume()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Signal the worker thread to exit.
        self.shutdown.store(true, Ordering::Release);

        // Wait for the worker thread to finish (at most ~510ms).
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }

        // Best-effort stop on drop. Ignore errors — the OS will clean up.
        let _ = self.device.lock().stop();
    }
}
