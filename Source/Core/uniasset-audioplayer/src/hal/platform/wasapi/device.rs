//! WASAPI audio device with automatic endpoint switching.
//!
//! # Architecture
//!
//! A dedicated worker thread manages the full WASAPI lifecycle internally.
//! WASAPI has no pull API — the thread uses event-driven buffer filling
//! (`WaitForMultipleObjects` + `IAudioRenderClient`).
//!
//! # Device Switching
//!
//! Device disconnection is detected via WASAPI errors during buffer
//! processing (e.g. `AUDCLNT_E_DEVICE_INVALIDATED`). The audio thread
//! sets [`DeviceInner::device_changed`], then transparently tears down
//! the old endpoint and rebuilds on the current default device — all
//! without the caller's involvement. If no endpoint is available, the
//! thread retries periodically until one appears.
//!
//! # Lock-Free Hot Path
//!
//! The audio thread uses atomics for `running` / `paused` /
//! `device_changed`. A `RwLock` protects the format (written only on
//! endpoint switch). No mutex is held during buffer processing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::RwLock as PLRwLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE,
};

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};
use crate::types::AudioFormat;

use super::com::ensure_com_initialized;

// ── Constants ─────────────────────────────────────────────────────────────

/// Default format when the hardware mix format cannot be queried.
const DEFAULT_SAMPLE_RATE: u32 = 48000;
const DEFAULT_CHANNELS: u16 = 2;

/// Buffer duration in 100-nanosecond units (~21 ms).
const HNS_BUFFER_DURATION: i64 = 210_000;

/// CLSID for the `MMDeviceEnumerator` COM class.
/// `IMMDeviceEnumerator::IID` is the *interface* IID —
/// `CoCreateInstance` requires the *class* CLSID.
const CLSID_MMDEVICE_ENUMERATOR: windows::core::GUID =
    windows::core::GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

/// Maximum consecutive rebuild attempts before giving up.
const MAX_REBUILD_ATTEMPTS: u32 = 5;

/// Sleep between rebuild retries on failure.
const REBUILD_RETRY_MS: u64 = 500;

// ── Helpers ───────────────────────────────────────────────────────────────

fn wasapi_err(e: impl std::fmt::Display) -> AudioError {
    AudioError::BackendError(format!("WASAPI: {e}"))
}

/// Parse sample rate and channels from a `WAVEFORMATEX` pointer.
fn parse_wave_format(ptr: *const WAVEFORMATEX) -> (u32, u16) {
    if ptr.is_null() {
        return (DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS);
    }
    let wf = unsafe { &*ptr };
    if wf.nSamplesPerSec > 0 && wf.nChannels > 0 {
        (wf.nSamplesPerSec, wf.nChannels)
    } else {
        (DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS)
    }
}

// ── DeviceInner — lock-free shared state ──────────────────────────────────

struct DeviceInner {
    /// Current hardware format. Written only on endpoint switch.
    format: PLRwLock<AudioFormat>,

    /// Set by `start()` / cleared by `stop()`.
    running: AtomicBool,

    /// Toggled by `pause()` / `resume()`.
    paused: AtomicBool,

    /// Set by the notification client when the default endpoint changes.
    device_changed: AtomicBool,
}

// ── Commands ──────────────────────────────────────────────────────────────

enum Command {
    /// Stop playback and terminate the audio thread.
    Stop,
    /// Pause (IAudioClient::Stop + skip buffer fills).
    Pause,
    /// Resume (IAudioClient::Start).
    Resume,
}

// ── WasapiClient — manages one WASAPI endpoint ────────────────────────────

/// Bundle of WASAPI resources for a single audio endpoint.
///
/// Created and owned by the audio thread. Not `Send` / `Sync` —
/// it lives and dies on the audio thread.
struct WasapiClient {
    audio_client: IAudioClient,
    render_client: IAudioRenderClient,
    /// Auto-reset event signalled by WASAPI when buffer space is available.
    buffer_event: HANDLE,
    /// Number of frames obtained from `GetBufferSize()`.
    buffer_frame_count: u32,
}

impl WasapiClient {
    /// Build a new WASAPI client on the **default audio endpoint**.
    ///
    /// Must be called on the thread that will own this client
    /// (COM per-thread affinity).
    fn build() -> Result<(Self, AudioFormat), AudioError> {
        ensure_com_initialized();

        // 1. Enumerator
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL) }
                .map_err(|_| AudioError::DeviceNotFound)?;

        // 2. Default endpoint
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| AudioError::DeviceNotFound)?;

        // 3. Activate IAudioClient
        let audio_client: IAudioClient =
            unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .map_err(|_| AudioError::DeviceBusy)?;

        // 4. Mix format
        let mix_ptr = unsafe { audio_client.GetMixFormat() }.map_err(|e| wasapi_err(e))?;
        let (sample_rate, channels) = parse_wave_format(mix_ptr);
        let format = AudioFormat::new(sample_rate, channels);

        // 5. Initialize (shared + event-driven)
        let init_res = unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                HNS_BUFFER_DURATION,
                0, // periodicity: engine decides
                mix_ptr,
                None, // AudioSessionGuid
            )
        };
        unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
        init_res.map_err(|e| wasapi_err(e))?;

        // 6. Buffer size
        let buffer_frame_count =
            unsafe { audio_client.GetBufferSize() }.map_err(|e| wasapi_err(e))?;

        // 7. Buffer event (auto-reset)
        let buffer_event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|e| wasapi_err(e))?;

        // 8. Register event
        unsafe { audio_client.SetEventHandle(buffer_event) }.map_err(|e| wasapi_err(e))?;

        // 9. Render client
        let render_client: IAudioRenderClient =
            unsafe { audio_client.GetService::<IAudioRenderClient>() }
                .map_err(|e| wasapi_err(e))?;

        Ok((
            Self {
                audio_client,
                render_client,
                buffer_event,
                buffer_frame_count,
            },
            format,
        ))
    }

    /// Start the audio stream.
    fn start(&self) -> Result<(), AudioError> {
        unsafe { self.audio_client.Start() }.map_err(|e| wasapi_err(e))
    }

    /// Stop the audio stream.
    fn stop(&self) -> Result<(), AudioError> {
        unsafe { self.audio_client.Stop() }.map_err(|e| wasapi_err(e))
    }

    /// Fill one buffer period.
    ///
    /// Returns `Ok(())` on success, `Err(())` on device error
    /// (caller should trigger endpoint rebuild).
    fn fill_buffer<M: AudioManager>(
        &self,
        manager: &M,
        channels: u16,
        temp_buf: &mut Vec<f32>,
    ) -> Result<(), ()> {
        let padding = unsafe { self.audio_client.GetCurrentPadding() }.map_err(|_| ())?;
        let frames_available = self.buffer_frame_count.saturating_sub(padding);
        if frames_available == 0 {
            return Ok(());
        }

        let required = frames_available as usize * channels as usize;

        // Grow buffer without zero-initialising — pull() writes the
        // first N samples and we zero-fill the remainder below.
        if temp_buf.len() < required {
            temp_buf.reserve(required - temp_buf.len());
        }
        // SAFETY: all elements will be initialised before they are read.
        unsafe {
            temp_buf.set_len(required);
        }

        // Pull from manager.
        let frames_written = manager.pull(&mut temp_buf[..required]);
        // Clamp: buggy callbacks may return more frames than requested.
        let frames_written = frames_written.min(frames_available as usize);

        // Zero-fill remainder.
        let written_samples = frames_written * channels as usize;
        if written_samples < required {
            temp_buf[written_samples..required].fill(0.0);
        }

        // Get WASAPI buffer.
        let data_ptr = unsafe { self.render_client.GetBuffer(frames_available) }.map_err(|_| ())?;

        // Copy into WASAPI buffer.
        unsafe {
            let dst = std::slice::from_raw_parts_mut(data_ptr as *mut f32, required);
            dst.copy_from_slice(&temp_buf[..required]);
        }

        // Release.
        unsafe { self.render_client.ReleaseBuffer(frames_available, 0) }.map_err(|_| ())?;

        Ok(())
    }
}

impl Drop for WasapiClient {
    fn drop(&mut self) {
        if !self.buffer_event.is_invalid() {
            let _ = unsafe { CloseHandle(self.buffer_event) };
        }
        // IAudioClient and IAudioRenderClient drop via their own Drop impl
        // (calls Release()).
    }
}

// ── Audio thread ──────────────────────────────────────────────────────────

/// Build a client with retry-on-failure.
fn try_build_client(client: &mut Option<WasapiClient>, format: &mut AudioFormat) {
    for attempt in 0..MAX_REBUILD_ATTEMPTS {
        match WasapiClient::build() {
            Ok((c, f)) => {
                *client = Some(c);
                *format = f;
                return;
            }
            Err(_) if attempt + 1 < MAX_REBUILD_ATTEMPTS => {
                thread::sleep(Duration::from_millis(REBUILD_RETRY_MS));
            }
            Err(_) => return,
        }
    }
}

fn run_audio_thread<M: AudioManager>(
    inner: Arc<DeviceInner>,
    cmd_rx: mpsc::Receiver<Command>,
    cmd_event: HANDLE,
    manager: M,
) {
    ensure_com_initialized();

    let mut client: Option<WasapiClient> = None;
    let mut temp_buf: Vec<f32> = Vec::new();
    let mut current_format = inner.format.read().clone();
    let mut should_exit = false;

    loop {
        // ── Drain commands ───────────────────────────────────────────
        drain_commands(
            &cmd_rx,
            &mut client,
            &mut current_format,
            &inner,
            &mut should_exit,
        );

        if should_exit {
            if let Some(ref c) = client {
                let _ = c.stop();
            }
            return;
        }

        // Reset the wake event after draining.
        let _ = unsafe { ResetEvent(cmd_event) };

        // ── Device switch ────────────────────────────────────────────
        if inner.device_changed.swap(false, Ordering::Acquire) {
            manager.on_device_invalidated();

            if let Some(ref c) = client {
                let _ = c.stop();
            }
            drop(client.take());

            try_build_client(&mut client, &mut current_format);
            if let Some(ref c) = client {
                *inner.format.write() = current_format;
                manager.on_device_format_changed(current_format);

                let running = inner.running.load(Ordering::Acquire);
                let paused = inner.paused.load(Ordering::Acquire);
                if running && !paused {
                    let _ = c.start();
                }
                manager.on_device_recovered();
            }
        }

        // ── Determine wait strategy ──────────────────────────────────
        let running = inner.running.load(Ordering::Acquire);
        let paused = inner.paused.load(Ordering::Acquire);

        if running && !paused {
            // ── Active audio ──────────────────────────────────────────
            match &client {
                Some(c) => {
                    let handles = [c.buffer_event, cmd_event];
                    let result = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };

                    if result == WAIT_OBJECT_0 {
                        if c.fill_buffer(&manager, current_format.channels, &mut temp_buf)
                            .is_err()
                        {
                            inner.device_changed.store(true, Ordering::Release);
                        }
                    }
                    // WAIT_OBJECT_0 + 1: cmd_event → loop to drain.
                }
                None => {
                    // Shouldn't happen — try to rebuild.
                    try_build_client(&mut client, &mut current_format);
                    if let Some(ref c) = client {
                        *inner.format.write() = current_format;
                        let _ = c.start();
                    } else {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        } else {
            // ── Idle / paused ────────────────────────────────────────
            // Wait for next command with a timeout.
            match cmd_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(cmd) => {
                    handle_single_command(
                        cmd,
                        &mut client,
                        &mut current_format,
                        &inner,
                        &mut should_exit,
                    );
                    if should_exit {
                        if let Some(ref c) = client {
                            let _ = c.stop();
                        }
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Check device_changed flag.
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Process all pending commands from the channel.
fn drain_commands(
    rx: &mpsc::Receiver<Command>,
    client: &mut Option<WasapiClient>,
    format: &mut AudioFormat,
    inner: &DeviceInner,
    should_exit: &mut bool,
) {
    loop {
        match rx.try_recv() {
            Ok(cmd) => handle_single_command(cmd, client, format, inner, should_exit),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                *should_exit = true;
                break;
            }
        }
    }
}

/// Process one command.
fn handle_single_command(
    cmd: Command,
    client: &mut Option<WasapiClient>,
    format: &mut AudioFormat,
    inner: &DeviceInner,
    should_exit: &mut bool,
) {
    match cmd {
        Command::Stop => {
            inner.running.store(false, Ordering::Release);
            inner.paused.store(false, Ordering::Release);
            *should_exit = true;
        }
        Command::Pause => {
            inner.paused.store(true, Ordering::Release);
            if let Some(ref c) = client {
                let _ = c.stop();
            }
        }
        Command::Resume => {
            inner.paused.store(false, Ordering::Release);
            if let Some(ref c) = client {
                let _ = c.start();
            }
        }
    }
}

// ── WasapiDevice ──────────────────────────────────────────────────────────

/// Windows audio output device using WASAPI shared-mode, event-driven
/// buffering, and automatic endpoint switching.
///
/// # Internals
///
/// A worker thread manages `IAudioClient`, `IAudioRenderClient`, and
/// the event-based buffer loop. WASAPI errors (e.g. device unplugged)
/// are detected during buffer processing — the thread then transparently
/// tears down the old endpoint and rebuilds on the current default
/// device. If the previous device is no longer available, the thread
/// retries until a new endpoint is found.
pub struct WasapiDevice<M: AudioManager> {
    inner: Arc<DeviceInner>,
    manager: Option<M>,

    /// Command sender. `None` after the audio thread stops.
    cmd_tx: Option<mpsc::Sender<Command>>,

    /// Command receiver. Moved into the audio thread in `start()`.
    cmd_rx: Option<mpsc::Receiver<Command>>,

    /// Manual-reset event: main thread sets this after sending a
    /// command; the audio thread resets it after draining.
    cmd_event: HANDLE,

    /// Audio worker thread handle.
    thread: Option<thread::JoinHandle<()>>,
}

// Safety: COM in MTA; audio-thread resources transferred via
// raw pointers inside WasapiThreadContext.
unsafe impl<M: AudioManager> Send for WasapiDevice<M> {}

// ── Thread context (Send-safe bundle) ─────────────────────────────────────

struct WasapiThreadContext {
    inner: Arc<DeviceInner>,
    cmd_rx: mpsc::Receiver<Command>,
    cmd_event_ptr: usize,
}

unsafe impl Send for WasapiThreadContext {}

impl<M: AudioManager> WasapiDevice<M> {
    /// Create a new WASAPI device.
    ///
    /// Queries the initial hardware format and prepares the command
    /// channel. The audio thread is **not** started yet — call
    /// [`start`](AudioDevice::start) to begin playback.
    pub fn new(manager: M) -> Result<Self, AudioError> {
        ensure_com_initialized();

        // ── Query initial format ──────────────────────────────────────
        let (sample_rate, channels) = {
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL) }
                    .map_err(|_| AudioError::DeviceNotFound)?;
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|_| AudioError::DeviceNotFound)?;
            let ac: IAudioClient = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .map_err(|_| AudioError::DeviceBusy)?;
            let mix_ptr = unsafe { ac.GetMixFormat() }.map_err(|e| wasapi_err(e))?;
            let fmt = parse_wave_format(mix_ptr);
            unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
            fmt
        };

        // ── Shared state ──────────────────────────────────────────────
        let inner = Arc::new(DeviceInner {
            format: PLRwLock::new(AudioFormat::new(sample_rate, channels)),
            running: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            device_changed: AtomicBool::new(false),
        });

        // ── Command channel ───────────────────────────────────────────
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let cmd_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| wasapi_err(e))?;

        Ok(Self {
            inner,
            manager: Some(manager),
            cmd_tx: Some(cmd_tx),
            cmd_rx: Some(cmd_rx),
            cmd_event,
            thread: None,
        })
    }
}

// ── AudioDevice impl ──────────────────────────────────────────────────────

impl<M: AudioManager> AudioDevice for WasapiDevice<M> {
    fn format(&self) -> AudioFormat {
        self.inner.format.read().clone()
    }

    fn start(&mut self) -> Result<(), AudioError> {
        // Already running?
        if self.thread.is_some() {
            return Ok(());
        }

        // Take ownership of the command channel.
        let cmd_tx = self
            .cmd_tx
            .take()
            .ok_or_else(|| wasapi_err("device already started"))?;
        let cmd_rx = self
            .cmd_rx
            .take()
            .ok_or_else(|| wasapi_err("device already started"))?;
        let manager = self
            .manager
            .take()
            .ok_or_else(|| wasapi_err("device manager already started"))?;

        let cmd_event = self.cmd_event;
        let cmd_event_ptr = cmd_event.0 as usize;
        let inner = Arc::clone(&self.inner);

        let ctx = WasapiThreadContext {
            inner,
            cmd_rx,
            cmd_event_ptr,
        };

        // Spawn the audio thread.
        let handle = thread::Builder::new()
            .name("uniasset-wasapi".into())
            .spawn(move || {
                let WasapiThreadContext {
                    inner,
                    cmd_rx,
                    cmd_event_ptr,
                } = ctx;
                let cmd_event = HANDLE(cmd_event_ptr as *mut std::ffi::c_void);
                run_audio_thread(inner, cmd_rx, cmd_event, manager);
                // Let cmd_rx, HANDLE drop here; COM objects dropped in run_audio_thread.
            })
            .map_err(|e| wasapi_err(e))?;

        // Store the sender back for subsequent control calls.
        self.cmd_tx = Some(cmd_tx);
        self.thread = Some(handle);
        self.inner.running.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        // Send Stop command.
        if let Some(ref tx) = self.cmd_tx {
            let _ = tx.send(Command::Stop);
            let _ = unsafe { SetEvent(self.cmd_event) };
        }

        // Join thread.
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }

        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        if let Some(ref tx) = self.cmd_tx {
            let _ = tx.send(Command::Pause);
            let _ = unsafe { SetEvent(self.cmd_event) };
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        if let Some(ref tx) = self.cmd_tx {
            let _ = tx.send(Command::Resume);
            let _ = unsafe { SetEvent(self.cmd_event) };
        }
        Ok(())
    }
}

// ── Drop ──────────────────────────────────────────────────────────────────

impl<M: AudioManager> Drop for WasapiDevice<M> {
    fn drop(&mut self) {
        // 1. Stop the audio thread.
        if let Some(ref tx) = self.cmd_tx {
            let _ = tx.send(Command::Stop);
            let _ = unsafe { SetEvent(self.cmd_event) };
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }

        // 2. Drop command channel (closes sender, receiver already in thread).
        drop(self.cmd_tx.take());
        drop(self.cmd_rx.take());

        // 3. Close event handle.
        if !self.cmd_event.is_invalid() {
            let _ = unsafe { CloseHandle(self.cmd_event) };
        }
    }
}
