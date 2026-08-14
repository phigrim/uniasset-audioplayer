//! CoreAudio backend shared by macOS and iOS.
//!
//! The platform modules only own native endpoint setup and route/session
//! notifications. This module owns the pull callback, endpoint lifetime and
//! non-realtime recovery worker.

#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "ios")]
use ios as platform;
#[cfg(target_os = "macos")]
use macos as platform;

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use coreaudio_sys::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input,
    AudioComponentInstanceDispose, AudioOutputUnitStart, AudioOutputUnitStop,
    AudioStreamBasicDescription, AudioUnitInitialize, AudioUnitRenderActionFlags,
    AudioUnitSetProperty, AudioUnitUninitialize,
};
use parking_lot::{Condvar, Mutex};

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};
use crate::types::AudioFormat;

const NO_ERR: i32 = 0;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u16 = 2;
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Shared by native callbacks and the recovery worker.
///
/// Native callbacks only set an atomic flag and wake the worker. No native
/// endpoint operation is performed from a callback.
pub(super) struct RebuildSignal {
    requested: AtomicBool,
    shutdown: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

impl RebuildSignal {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            wait_lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    pub(super) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    fn take_request(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }

    fn wait(&self) -> bool {
        let mut guard = self.wait_lock.lock();
        while !self.shutdown.load(Ordering::Acquire) && !self.requested.load(Ordering::Acquire) {
            self.wake.wait(&mut guard);
        }
        !self.shutdown.load(Ordering::Acquire)
    }

    fn wait_for_retry(&self) -> bool {
        let mut guard = self.wait_lock.lock();
        if !self.requested.load(Ordering::Acquire) && !self.shutdown.load(Ordering::Acquire) {
            self.wake.wait_for(&mut guard, RETRY_INTERVAL);
        }
        !self.shutdown.load(Ordering::Acquire)
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

/// Thin indirection used as `AURenderCallbackStruct.inputProcRefCon`.
struct CallbackRef<M: AudioManager> {
    manager: *const M,
}

// SAFETY: The callback reference is created before the AudioUnit starts and
// is kept alive until the AudioUnit has stopped and been uninitialized.
unsafe impl<M: AudioManager> Send for CallbackRef<M> {}
unsafe impl<M: AudioManager> Sync for CallbackRef<M> {}

/// A callback-safe AudioUnit owner.
///
/// The callback reference is deliberately stored before starting the unit and
/// is dropped only after stop/uninitialize. This avoids the raw-pointer
/// ownership gap present when a Box is reconstructed after `Start` returns.
struct AudioUnitOwner<M: AudioManager> {
    audio_unit: sys::AudioUnit,
    signal: Arc<RebuildSignal>,
    callback_ref: Option<Box<CallbackRef<M>>>,
    initialized: bool,
    started: bool,
}

// SAFETY: CoreAudio owns the native endpoint and all operations on it are
// serialized by the CoreAudioState mutex. The callback only reads the stable
// manager pointer through CallbackRef.
unsafe impl<M: AudioManager> Send for AudioUnitOwner<M> {}

impl<M: AudioManager> AudioUnitOwner<M> {
    fn open(signal: Arc<RebuildSignal>) -> Result<(Self, AudioFormat), AudioError> {
        let (audio_unit, format) = platform::open_audio_unit(Arc::clone(&signal))?;
        Ok((
            Self {
                audio_unit,
                signal,
                callback_ref: None,
                initialized: false,
                started: false,
            },
            format,
        ))
    }

    fn start(&mut self, manager: &M) -> Result<(), AudioError> {
        if self.started {
            return Ok(());
        }

        let status = unsafe { AudioUnitInitialize(self.audio_unit) };
        if status != NO_ERR {
            return Err(backend_error("AudioUnitInitialize", status));
        }
        self.initialized = true;

        let callback_ref = Box::new(CallbackRef {
            manager: manager as *const M,
        });
        let ref_con = (&*callback_ref as *const CallbackRef<M>)
            .cast_mut()
            .cast::<c_void>();
        let callback = sys::AURenderCallbackStruct {
            inputProc: Some(render_callback::<M>),
            inputProcRefCon: ref_con,
        };

        let status = unsafe {
            AudioUnitSetProperty(
                self.audio_unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &callback as *const _ as *const c_void,
                std::mem::size_of::<sys::AURenderCallbackStruct>() as u32,
            )
        };
        if status != NO_ERR {
            self.callback_ref = Some(callback_ref);
            self.stop_and_uninitialize();
            return Err(backend_error(
                "AudioUnitSetProperty(SetRenderCallback)",
                status,
            ));
        }

        // Store the Box before Start. From this point on every possible
        // callback context pointer remains backed by live allocation.
        self.callback_ref = Some(callback_ref);

        let status = unsafe { AudioOutputUnitStart(self.audio_unit) };
        if status != NO_ERR {
            self.stop_and_uninitialize();
            return Err(backend_error("AudioOutputUnitStart", status));
        }

        self.started = true;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        if !self.started {
            return Ok(());
        }
        let status = unsafe { AudioOutputUnitStop(self.audio_unit) };
        if status != NO_ERR {
            return Err(backend_error("AudioOutputUnitStop", status));
        }
        self.started = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        if self.started {
            return Ok(());
        }
        let status = unsafe { AudioOutputUnitStart(self.audio_unit) };
        if status != NO_ERR {
            return Err(backend_error("AudioOutputUnitStart", status));
        }
        self.started = true;
        Ok(())
    }

    fn stop_and_uninitialize(&mut self) {
        if self.started {
            let _ = unsafe { AudioOutputUnitStop(self.audio_unit) };
            self.started = false;
        }
        if self.initialized {
            let _ = unsafe { AudioUnitUninitialize(self.audio_unit) };
            self.initialized = false;
        }
    }
}

impl<M: AudioManager> Drop for AudioUnitOwner<M> {
    fn drop(&mut self) {
        self.stop_and_uninitialize();

        unsafe {
            let _ = sys::AudioUnitRemovePropertyListenerWithUserData(
                self.audio_unit,
                kAudioUnitProperty_StreamFormat,
                audio_unit_property_changed,
                Arc::as_ptr(&self.signal) as *mut c_void,
            );
            AudioComponentInstanceDispose(self.audio_unit);
        }

        // Drop callback_ref only after the native callback has been stopped.
        self.callback_ref = None;
    }
}

fn backend_error(operation: &str, status: i32) -> AudioError {
    AudioError::BackendError(format!("{operation} failed: {status}"))
}

fn pcm_format(format: AudioFormat) -> AudioStreamBasicDescription {
    let channels = u32::from(format.channels);
    AudioStreamBasicDescription {
        mSampleRate: f64::from(format.sample_rate),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: channels * 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: channels * 4,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// The C-callable render callback installed on the AudioUnit.
///
/// This path deliberately contains no mutex, allocation, Objective-C call,
/// or device-management operation.
extern "C" fn render_callback<M: AudioManager>(
    ref_con: *mut c_void,
    _action_flags: *mut AudioUnitRenderActionFlags,
    _time_stamp: *const sys::AudioTimeStamp,
    _bus_number: u32,
    frame_count: u32,
    io_data: *mut sys::AudioBufferList,
) -> i32 {
    if ref_con.is_null() || io_data.is_null() {
        return NO_ERR;
    }

    let buffers = unsafe { &mut *io_data };
    if buffers.mNumberBuffers == 0 {
        return NO_ERR;
    }

    let buffer = &mut buffers.mBuffers[0];
    let byte_count = buffer.mDataByteSize as usize;
    let available_samples = byte_count / std::mem::size_of::<f32>();
    if buffer.mData.is_null() || available_samples == 0 {
        return NO_ERR;
    }

    let channels = buffer.mNumberChannels as usize;
    let frames = frame_count as usize;
    let requested_samples = match frames.checked_mul(channels) {
        Some(value) if channels != 0 => value,
        _ => return NO_ERR,
    };
    let sample_count = requested_samples.min(available_samples);
    let sample_count = sample_count - sample_count % channels;
    if sample_count == 0 {
        return NO_ERR;
    }

    let output =
        unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), sample_count) };
    let callback_ref = unsafe { &*(ref_con.cast::<CallbackRef<M>>()) };
    if callback_ref.manager.is_null() {
        output.fill(0.0);
        return NO_ERR;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: callback_ref is owned by AudioUnitOwner and remains alive
        // until after the AudioUnit has stopped and been uninitialized.
        let manager = unsafe { &*callback_ref.manager };
        let frames_written = manager.pull(output).min(sample_count / channels);
        let written_samples = frames_written * channels;
        output[written_samples..].fill(0.0);
    }));

    if result.is_err() {
        output.fill(0.0);
    }

    NO_ERR
}

extern "C" fn audio_unit_property_changed(
    user_data: *mut c_void,
    _audio_unit: sys::AudioUnit,
    _property_id: u32,
    _scope: u32,
    _element: u32,
) -> i32 {
    if !user_data.is_null() {
        let signal = unsafe { &*(user_data.cast::<RebuildSignal>()) };
        signal.request();
    }
    NO_ERR
}

struct CoreAudioState<M: AudioManager> {
    format: AudioFormat,
    audio_unit: Option<AudioUnitOwner<M>>,
    manager: Box<M>,
    _monitor: platform::RouteMonitor,
    signal: Arc<RebuildSignal>,
    running: bool,
    paused: bool,
    rebuilding: bool,
}

// SAFETY: All state access is serialized by the outer mutex. The native
// monitor only retains an Arc<RebuildSignal> and never accesses this state.
unsafe impl<M: AudioManager> Send for CoreAudioState<M> {}

impl<M: AudioManager> CoreAudioState<M> {
    fn new(manager: M) -> Result<Self, AudioError> {
        let signal = Arc::new(RebuildSignal::new());
        let monitor = platform::RouteMonitor::install(Arc::clone(&signal))?;
        let (audio_unit, format) = match AudioUnitOwner::open(Arc::clone(&signal)) {
            Ok(value) => value,
            Err(error) => {
                drop(monitor);
                return Err(error);
            }
        };

        Ok(Self {
            format,
            audio_unit: Some(audio_unit),
            manager: Box::new(manager),
            _monitor: monitor,
            signal,
            running: false,
            paused: false,
            rebuilding: false,
        })
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn start(&mut self) -> Result<(), AudioError> {
        let audio_unit = self.audio_unit.as_mut().ok_or(AudioError::DeviceNotFound)?;
        if self.running {
            return Ok(());
        }
        audio_unit.start(&self.manager)?;
        self.running = true;
        self.paused = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(audio_unit) = self.audio_unit.as_mut() {
            audio_unit.stop_and_uninitialize();
        }
        self.running = false;
        self.paused = false;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        if self.running && !self.paused {
            if let Some(audio_unit) = self.audio_unit.as_mut() {
                audio_unit.pause()?;
            }
            self.paused = true;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        if self.running && self.paused {
            if let Some(audio_unit) = self.audio_unit.as_mut() {
                audio_unit.resume()?;
            }
            self.paused = false;
        }
        Ok(())
    }

    /// Returns whether the worker should retry after a delay.
    fn rebuild(&mut self) -> bool {
        if !self.rebuilding {
            self.manager.on_device_invalidated();
            self.rebuilding = true;
        }

        let was_running = self.running;
        let was_paused = self.paused;
        self.audio_unit.take();
        self.running = false;
        self.paused = false;

        let (mut audio_unit, format) = match AudioUnitOwner::<M>::open(Arc::clone(&self.signal)) {
            Ok(value) => value,
            Err(_) => return true,
        };

        self.manager.on_device_format_changed(format);
        if was_running {
            if audio_unit.start(&*self.manager).is_err() {
                return true;
            }
            self.running = true;
            self.paused = false;
            if was_paused && audio_unit.pause().is_err() {
                self.running = false;
                self.paused = false;
                return true;
            }
            self.paused = was_paused;
        }

        self.audio_unit = Some(audio_unit);
        self.format = format;
        self.rebuilding = false;
        self.manager.on_device_recovered();
        false
    }
}

impl<M: AudioManager> Drop for CoreAudioState<M> {
    fn drop(&mut self) {
        self.audio_unit.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PartialManager;

    impl AudioManager for PartialManager {
        fn pull(&self, buffer: &mut [f32]) -> usize {
            buffer[..2].fill(1.0);
            1
        }
    }

    struct PanicManager;

    impl AudioManager for PanicManager {
        fn pull(&self, _buffer: &mut [f32]) -> usize {
            panic!("test callback panic");
        }
    }

    fn invoke_callback<M: AudioManager>(manager: &M, data: &mut [f32]) {
        let callback_ref = Box::new(CallbackRef {
            manager: manager as *const M,
        });
        let ref_con = (&*callback_ref as *const CallbackRef<M>)
            .cast_mut()
            .cast::<c_void>();
        let mut buffer_list = sys::AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [sys::AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (std::mem::size_of_val(data)) as u32,
                mData: data.as_mut_ptr().cast::<c_void>(),
            }],
        };

        render_callback::<M>(
            ref_con,
            std::ptr::null_mut(),
            std::ptr::null(),
            0,
            (data.len() / 2) as u32,
            &mut buffer_list,
        );
        drop(callback_ref);
    }

    #[test]
    fn callback_zero_fills_partial_manager_output() {
        let mut data = [9.0; 6];
        invoke_callback(&PartialManager, &mut data);
        assert_eq!(data, [1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn callback_converts_manager_panic_to_silence() {
        let mut data = [9.0; 4];
        invoke_callback(&PanicManager, &mut data);
        assert_eq!(data, [0.0; 4]);
    }
}

/// An audio output device backed by CoreAudio.
pub struct CoreAudioDevice<M: AudioManager> {
    state: Arc<Mutex<CoreAudioState<M>>>,
    signal: Arc<RebuildSignal>,
    worker: Option<JoinHandle<()>>,
}

impl<M: AudioManager> CoreAudioDevice<M> {
    pub fn new(manager: M) -> Result<Self, AudioError> {
        let state = Arc::new(Mutex::new(CoreAudioState::new(manager)?));
        let signal = Arc::clone(&state.lock().signal);
        let worker_state = Arc::clone(&state);
        let worker_signal = Arc::clone(&signal);
        let worker = thread::Builder::new()
            .name("uniasset-coreaudio".into())
            .spawn(move || run_device_worker(worker_state, worker_signal))
            .map_err(|error| {
                AudioError::BackendError(format!("failed to spawn CoreAudio worker: {error}"))
            })?;

        Ok(Self {
            state,
            signal,
            worker: Some(worker),
        })
    }
}

fn run_device_worker<M: AudioManager>(
    state: Arc<Mutex<CoreAudioState<M>>>,
    signal: Arc<RebuildSignal>,
) {
    let mut retry = false;
    loop {
        let active = if retry {
            signal.wait_for_retry()
        } else {
            signal.wait()
        };
        if !active {
            return;
        }

        signal.take_request();
        retry = state.lock().rebuild();
    }
}

impl<M: AudioManager> AudioDevice for CoreAudioDevice<M> {
    fn format(&self) -> AudioFormat {
        self.state.lock().format()
    }

    fn start(&mut self) -> Result<(), AudioError> {
        self.state.lock().start()
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        self.state.lock().stop()
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        self.state.lock().pause()
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        self.state.lock().resume()
    }
}

impl<M: AudioManager> Drop for CoreAudioDevice<M> {
    fn drop(&mut self) {
        self.signal.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn default_format() -> AudioFormat {
    AudioFormat::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS)
}

pub(super) fn configure_float_output(
    audio_unit: sys::AudioUnit,
    format: AudioFormat,
) -> Result<(), AudioError> {
    let asbd = pcm_format(format);
    let status = unsafe {
        AudioUnitSetProperty(
            audio_unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            0,
            &asbd as *const _ as *const c_void,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        )
    };
    if status == NO_ERR {
        Ok(())
    } else {
        Err(AudioError::FormatNotSupported)
    }
}

mod sys {
    pub use coreaudio_sys::*;

    pub type AudioUnitPropertyListenerProc = extern "C" fn(
        user_data: *mut std::ffi::c_void,
        audio_unit: AudioUnit,
        property_id: u32,
        scope: u32,
        element: u32,
    ) -> i32;

    extern "C" {
        pub fn AudioUnitAddPropertyListener(
            audio_unit: AudioUnit,
            property_id: u32,
            listener: AudioUnitPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
        pub fn AudioUnitRemovePropertyListenerWithUserData(
            audio_unit: AudioUnit,
            property_id: u32,
            listener: AudioUnitPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
    }
}
