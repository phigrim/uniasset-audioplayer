//! Event-driven WASAPI shared-mode output.
//!
//! The worker thread owns the COM apartment, endpoint interfaces, notification
//! registration and all endpoint handles. Control calls only enqueue commands
//! and wake that worker. The PCM path performs no locking or allocation.

use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows_core::Interface;

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};
use crate::types::AudioFormat;

use super::com::ComGuard;
use super::notification::{into_interface, DeviceChangeSignal, NotificationClient};

const DEFAULT_RETRY: Duration = Duration::from_millis(50);
const MAX_RETRY: Duration = Duration::from_secs(1);
const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

/// A single-owner Windows event. `SetEvent` and waiting on an event are safe
/// concurrently; the handle remains alive because this value is shared by
/// the device, worker and notification callback through an `Arc`.
pub(super) struct WakeEvent {
    handle: HANDLE,
}

// SAFETY: the kernel handle is an independent synchronization object. It is
// never closed while any Arc<WakeEvent> exists.
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> Result<Arc<Self>, AudioError> {
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|error| wasapi_error("CreateEventW", error))?;
        Ok(Arc::new(Self { handle }))
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.handle
    }

    pub(super) fn signal(&self) -> Result<(), AudioError> {
        unsafe { SetEvent(self.handle) }.map_err(|error| wasapi_error("SetEvent", error))
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

struct CoTaskMemFormat(*mut WAVEFORMATEX);

impl CoTaskMemFormat {
    fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.0
    }
}

impl Drop for CoTaskMemFormat {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
        }
    }
}

fn wasapi_error(operation: &str, error: impl std::fmt::Display) -> AudioError {
    AudioError::BackendError(format!("WASAPI {operation}: {error}"))
}

fn parse_float_format(ptr: *const WAVEFORMATEX) -> Result<AudioFormat, AudioError> {
    if ptr.is_null() {
        return Err(AudioError::FormatNotSupported);
    }

    // WAVEFORMATEX is packed. Copy the complete value first so no reference to
    // an unaligned field is ever created.
    let format = unsafe { ptr.read_unaligned() };
    let channels = format.nChannels;
    let sample_rate = format.nSamplesPerSec;
    let block_align = channels
        .checked_mul(4)
        .ok_or(AudioError::FormatNotSupported)?;
    let is_float = if format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16 {
        format.wBitsPerSample == 32 && format.nBlockAlign == block_align
    } else if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16
        && usize::from(format.cbSize) + std::mem::size_of::<WAVEFORMATEX>()
            >= std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
    {
        let extended = unsafe { (ptr as *const WAVEFORMATEXTENSIBLE).read_unaligned() };
        let sub_format =
            unsafe { std::ptr::read_unaligned(std::ptr::addr_of!(extended.SubFormat)) };
        sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            && format.wBitsPerSample == 32
            && format.nBlockAlign == block_align
    } else {
        false
    };

    if !is_float || channels == 0 || sample_rate == 0 {
        return Err(AudioError::FormatNotSupported);
    }

    Ok(AudioFormat::new(sample_rate, channels))
}

/// Resources for one endpoint. Field order is intentional: the render service
/// is released before its parent audio client.
struct WasapiClient {
    render_client: IAudioRenderClient,
    audio_client: IAudioClient,
    buffer_event: HANDLE,
    buffer_frame_count: u32,
    channels: u16,
}

impl WasapiClient {
    fn open(enumerator: &IMMDeviceEnumerator) -> Result<(Self, AudioFormat), AudioError> {
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| AudioError::DeviceNotFound)?;
        let audio_client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|_| AudioError::DeviceBusy)?;

        let mix_format = CoTaskMemFormat(
            unsafe { audio_client.GetMixFormat() }
                .map_err(|error| wasapi_error("GetMixFormat", error))?,
        );
        let format = parse_float_format(mix_format.as_ptr())?;

        let audio_client3 = audio_client.cast::<IAudioClient3>().ok();
        if let Some(ref client3) = audio_client3 {
            let mut default_period = 0;
            let mut fundamental_period = 0;
            let mut min_period = 0;
            let mut max_period = 0;
            let period_result = unsafe {
                client3.GetSharedModeEnginePeriod(
                    mix_format.as_ptr(),
                    &mut default_period,
                    &mut fundamental_period,
                    &mut min_period,
                    &mut max_period,
                )
            };
            let period = fundamental_period.max(min_period).max(1);
            if period_result.is_ok() {
                unsafe {
                    client3
                        .InitializeSharedAudioStream(
                            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                            period,
                            mix_format.as_ptr(),
                            None,
                        )
                        .map_err(|error| wasapi_error("InitializeSharedAudioStream", error))?;
                }
            } else {
                unsafe {
                    audio_client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                            0,
                            0,
                            mix_format.as_ptr(),
                            None,
                        )
                        .map_err(|error| wasapi_error("IAudioClient::Initialize", error))?;
                }
            }
        } else {
            unsafe {
                audio_client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        0,
                        0,
                        mix_format.as_ptr(),
                        None,
                    )
                    .map_err(|error| wasapi_error("IAudioClient::Initialize", error))?;
            }
        }

        let buffer_frame_count = unsafe { audio_client.GetBufferSize() }
            .map_err(|error| wasapi_error("GetBufferSize", error))?;
        if buffer_frame_count == 0 {
            return Err(AudioError::BackendError(
                "WASAPI returned a zero-sized render buffer".into(),
            ));
        }

        let buffer_event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|error| wasapi_error("CreateEventW", error))?;
        if let Err(error) = unsafe { audio_client.SetEventHandle(buffer_event) } {
            let _ = unsafe { CloseHandle(buffer_event) };
            return Err(wasapi_error("SetEventHandle", error));
        }

        let render_client = match unsafe { audio_client.GetService::<IAudioRenderClient>() } {
            Ok(render_client) => render_client,
            Err(error) => {
                let _ = unsafe { CloseHandle(buffer_event) };
                return Err(wasapi_error("GetService(IAudioRenderClient)", error));
            }
        };

        Ok((
            Self {
                render_client,
                audio_client,
                buffer_event,
                buffer_frame_count,
                channels: format.channels,
            },
            format,
        ))
    }

    fn start(&self) -> Result<(), AudioError> {
        unsafe { self.audio_client.Start() }.map_err(|error| wasapi_error("Start", error))
    }

    fn stop(&self) -> Result<(), AudioError> {
        unsafe { self.audio_client.Stop() }.map_err(|error| wasapi_error("Stop", error))
    }

    fn fill<M: AudioManager>(&self, manager: &M, temp: &mut [f32]) -> Result<(), RenderError> {
        let padding =
            unsafe { self.audio_client.GetCurrentPadding() }.map_err(RenderError::from_windows)?;
        if padding > self.buffer_frame_count {
            return Err(RenderError::Device);
        }
        let available = self.buffer_frame_count - padding;
        if available == 0 {
            return Ok(());
        }

        let sample_count = (available as usize)
            .checked_mul(self.channels as usize)
            .ok_or(RenderError::Device)?;
        if temp.len() < sample_count {
            return Err(RenderError::Device);
        }

        let frames_written =
            catch_unwind(AssertUnwindSafe(|| manager.pull(&mut temp[..sample_count])))
                .unwrap_or(0)
                .min(available as usize);
        temp[frames_written * self.channels as usize..sample_count].fill(0.0);

        let data = unsafe { self.render_client.GetBuffer(available) }
            .map_err(RenderError::from_windows)?;
        if data.is_null() {
            return Err(RenderError::Device);
        }

        let mut render_buffer = RenderBuffer {
            client: &self.render_client,
            data: data as *mut f32,
            frames: available,
            released: false,
        };
        // SAFETY: the format was validated as interleaved 32-bit float and
        // WASAPI returned storage for exactly `available` frames.
        unsafe { render_buffer.samples(sample_count) }.copy_from_slice(&temp[..sample_count]);
        render_buffer.release().map_err(RenderError::from_windows)
    }
}

impl Drop for WasapiClient {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.buffer_event) };
        // COM fields are dropped after this method. Rust drops them in field
        // declaration order, with render_client before audio_client.
    }
}

struct RenderBuffer<'a> {
    client: &'a IAudioRenderClient,
    data: *mut f32,
    frames: u32,
    released: bool,
}

impl RenderBuffer<'_> {
    unsafe fn samples(&mut self, sample_count: usize) -> &mut [f32] {
        std::slice::from_raw_parts_mut(self.data, sample_count)
    }

    fn release(&mut self) -> Result<(), windows::core::Error> {
        let result = unsafe { self.client.ReleaseBuffer(self.frames, 0) };
        self.released = true;
        result
    }
}

impl Drop for RenderBuffer<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = unsafe { self.client.ReleaseBuffer(self.frames, 0) };
        }
    }
}

enum RenderError {
    Device,
}

impl RenderError {
    fn from_windows(_error: windows::core::Error) -> Self {
        Self::Device
    }
}

#[derive(Clone, Copy)]
enum ControlCommand {
    Start,
    Stop,
    Pause,
    Resume,
}

enum Command {
    Start(mpsc::Sender<Result<(), AudioError>>),
    Stop(mpsc::Sender<Result<(), AudioError>>),
    Pause(mpsc::Sender<Result<(), AudioError>>),
    Resume(mpsc::Sender<Result<(), AudioError>>),
    Shutdown(mpsc::Sender<()>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaybackState {
    Stopped,
    Running,
    Paused,
}

struct Worker<M: AudioManager> {
    manager: M,
    enumerator: IMMDeviceEnumerator,
    signal: Arc<DeviceChangeSignal>,
    cmd_rx: mpsc::Receiver<Command>,
    wake: Arc<WakeEvent>,
    format: Arc<RwLock<AudioFormat>>,
    client: Option<WasapiClient>,
    temp_buffer: Vec<f32>,
    state: PlaybackState,
    recovery_pending: bool,
    notify_recovery: bool,
    recovery_notified: bool,
    retry_at: Instant,
    retry_delay: Duration,
}

impl<M: AudioManager> Worker<M> {
    fn install_client(
        &mut self,
        client: WasapiClient,
        format: AudioFormat,
    ) -> Result<(), AudioError> {
        let samples = (client.buffer_frame_count as usize)
            .checked_mul(format.channels as usize)
            .ok_or(AudioError::FormatNotSupported)?;
        self.temp_buffer.resize(samples, 0.0);
        *self.format.write() = format;
        self.client = Some(client);
        Ok(())
    }

    fn open_client(&mut self) -> Result<(), AudioError> {
        let (client, format) = WasapiClient::open(&self.enumerator)?;
        self.install_client(client, format)
    }

    fn begin_recovery(&mut self) {
        if self.state == PlaybackState::Stopped {
            return;
        }
        if let Some(client) = self.client.take() {
            let _ = client.stop();
        }
        self.recovery_pending = true;
        self.notify_recovery = true;
        self.retry_at = Instant::now();
        self.retry_delay = DEFAULT_RETRY;
    }

    fn try_recover(&mut self) {
        if !self.recovery_pending || Instant::now() < self.retry_at {
            return;
        }
        if self.notify_recovery && !self.recovery_notified {
            self.manager.on_device_invalidated();
            self.recovery_notified = true;
        }

        match self.open_client() {
            Ok(()) => {
                let format = *self.format.read();
                self.manager.on_device_format_changed(format);
                if self.state == PlaybackState::Running {
                    if let Some(client) = &self.client {
                        if client.start().is_err() {
                            self.client = None;
                            self.retry_at = Instant::now() + self.retry_delay;
                            self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
                            return;
                        }
                    }
                }
                self.recovery_pending = false;
                let notify_recovery = self.notify_recovery;
                self.notify_recovery = false;
                self.recovery_notified = false;
                self.retry_delay = DEFAULT_RETRY;
                if notify_recovery {
                    self.manager.on_device_recovered();
                }
            }
            Err(_) => {
                self.retry_at = Instant::now() + self.retry_delay;
                self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
            }
        }
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Start(reply) => {
                let result = if self.state == PlaybackState::Running {
                    Ok(())
                } else {
                    self.state = PlaybackState::Running;
                    if self.client.is_none() {
                        self.recovery_pending = true;
                        self.notify_recovery = false;
                        self.retry_at = Instant::now();
                    }
                    self.try_recover();
                    match &self.client {
                        Some(client) if !self.recovery_pending => match client.start() {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                self.begin_recovery();
                                Err(error)
                            }
                        },
                        Some(_) => Ok(()),
                        None => Ok(()),
                    }
                };
                let _ = reply.send(result);
            }
            Command::Stop(reply) => {
                self.state = PlaybackState::Stopped;
                self.recovery_pending = false;
                self.notify_recovery = false;
                self.recovery_notified = false;
                self.temp_buffer.clear();
                let result = self
                    .client
                    .take()
                    .map(|client| client.stop())
                    .unwrap_or(Ok(()));
                let _ = reply.send(result);
            }
            Command::Pause(reply) => {
                let mut result = Ok(());
                if self.state == PlaybackState::Running {
                    if let Some(client) = &self.client {
                        result = client.stop();
                    }
                    self.state = PlaybackState::Paused;
                }
                let _ = reply.send(result);
            }
            Command::Resume(reply) => {
                if self.state == PlaybackState::Paused {
                    self.state = PlaybackState::Running;
                    if let Some(client) = &self.client {
                        if let Err(error) = client.start() {
                            self.begin_recovery();
                            let _ = reply.send(Err(error));
                            return true;
                        }
                    } else {
                        self.recovery_pending = true;
                        self.notify_recovery = false;
                        self.retry_at = Instant::now();
                    }
                }
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown(reply) => {
                self.state = PlaybackState::Stopped;
                self.recovery_pending = false;
                if let Some(client) = self.client.take() {
                    let _ = client.stop();
                }
                let _ = reply.send(());
                return false;
            }
        }
        true
    }

    fn drain_commands(&mut self) -> bool {
        while let Ok(command) = self.cmd_rx.try_recv() {
            if !self.handle_command(command) {
                return false;
            }
        }
        true
    }

    fn run(&mut self) {
        loop {
            if !self.drain_commands() {
                return;
            }
            if self.signal.take() {
                self.begin_recovery();
            }
            self.try_recover();

            if self.state != PlaybackState::Running || self.recovery_pending {
                let timeout = if self.recovery_pending {
                    self.retry_at
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(u128::from(u32::MAX)) as u32
                } else {
                    INFINITE
                };
                let _ = unsafe { WaitForSingleObject(self.wake.raw(), timeout) };
                continue;
            }

            let Some(client) = self.client.as_ref() else {
                self.recovery_pending = true;
                self.retry_at = Instant::now();
                continue;
            };
            let handles = [client.buffer_event, self.wake.raw()];
            let result = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            if result == WAIT_OBJECT_0 {
                if client.fill(&self.manager, &mut self.temp_buffer).is_err() {
                    self.begin_recovery();
                }
            } else if result.0 != WAIT_OBJECT_0.0 + 1 && result != WAIT_TIMEOUT {
                self.begin_recovery();
            }
        }
    }
}

fn worker_main<M: AudioManager>(
    manager: M,
    cmd_rx: mpsc::Receiver<Command>,
    wake: Arc<WakeEvent>,
    format: Arc<RwLock<AudioFormat>>,
    ready: mpsc::SyncSender<Result<(), AudioError>>,
) {
    let Ok(_com) = ComGuard::initialize() else {
        let _ = ready.send(Err(AudioError::BackendError(
            "WASAPI COM initialization failed".into(),
        )));
        return;
    };

    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL) } {
            Ok(enumerator) => enumerator,
            Err(error) => {
                let _ = ready.send(Err(wasapi_error("CoCreateInstance", error)));
                return;
            }
        };

    let signal = DeviceChangeSignal::new(Arc::clone(&wake));
    let notification = into_interface(NotificationClient::new(Arc::clone(&signal)));
    if let Err(error) = unsafe { enumerator.RegisterEndpointNotificationCallback(&notification) } {
        let _ = ready.send(Err(wasapi_error(
            "RegisterEndpointNotificationCallback",
            error,
        )));
        return;
    }

    let mut worker = Worker {
        manager,
        enumerator,
        signal,
        cmd_rx,
        wake,
        format,
        client: None,
        temp_buffer: Vec::new(),
        state: PlaybackState::Stopped,
        recovery_pending: false,
        notify_recovery: false,
        recovery_notified: false,
        retry_at: Instant::now(),
        retry_delay: DEFAULT_RETRY,
    };

    let initial_result = worker.open_client();
    if let Err(error) = initial_result {
        let _ = unsafe {
            worker
                .enumerator
                .UnregisterEndpointNotificationCallback(&notification)
        };
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));

    worker.run();
    let _ = unsafe {
        worker
            .enumerator
            .UnregisterEndpointNotificationCallback(&notification)
    };
}

/// Windows output device backed by a dedicated WASAPI worker.
pub struct WasapiDevice<M: AudioManager> {
    format: Arc<RwLock<AudioFormat>>,
    cmd_tx: mpsc::Sender<Command>,
    wake: Arc<WakeEvent>,
    worker: Option<thread::JoinHandle<()>>,
    _manager: PhantomData<fn() -> M>,
}

impl<M: AudioManager> WasapiDevice<M> {
    pub fn new(manager: M) -> Result<Self, AudioError> {
        let wake = WakeEvent::new()?;
        let format = Arc::new(RwLock::new(AudioFormat::new(48_000, 2)));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_wake = Arc::clone(&wake);
        let worker_format = Arc::clone(&format);
        let worker = thread::Builder::new()
            .name("uniasset-wasapi".into())
            .spawn(move || worker_main(manager, cmd_rx, worker_wake, worker_format, ready_tx))
            .map_err(|error| wasapi_error("spawn worker", error))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                format,
                cmd_tx,
                wake,
                worker: Some(worker),
                _manager: PhantomData,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(AudioError::BackendError(
                    "WASAPI worker exited during initialization".into(),
                ))
            }
        }
    }

    fn request(&self, command: ControlCommand) -> Result<(), AudioError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let command = match command {
            ControlCommand::Start => Command::Start(reply_tx),
            ControlCommand::Stop => Command::Stop(reply_tx),
            ControlCommand::Pause => Command::Pause(reply_tx),
            ControlCommand::Resume => Command::Resume(reply_tx),
        };
        self.cmd_tx
            .send(command)
            .map_err(|_| AudioError::BackendError("WASAPI worker is not running".into()))?;
        self.wake.signal()?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::BackendError("WASAPI worker stopped unexpectedly".into()))?
    }
}

impl<M: AudioManager> AudioDevice for WasapiDevice<M> {
    fn format(&self) -> AudioFormat {
        *self.format.read()
    }

    fn start(&mut self) -> Result<(), AudioError> {
        self.request(ControlCommand::Start)
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        self.request(ControlCommand::Stop)
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        self.request(ControlCommand::Pause)
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        self.request(ControlCommand::Resume)
    }
}

impl<M: AudioManager> Drop for WasapiDevice<M> {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let (reply_tx, reply_rx) = mpsc::channel();
            if self.cmd_tx.send(Command::Shutdown(reply_tx)).is_ok() {
                let _ = self.wake.signal();
                let _ = reply_rx.recv();
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_null_and_non_float_formats() {
        assert!(parse_float_format(std::ptr::null()).is_err());
        let format = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 192_000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        assert!(parse_float_format(&format).is_err());
    }

    #[test]
    fn accepts_interleaved_float_format() {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 384_000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        assert_eq!(
            parse_float_format(&format).unwrap(),
            AudioFormat::new(48_000, 2)
        );
    }
}
