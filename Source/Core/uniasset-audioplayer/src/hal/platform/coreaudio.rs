//! CoreAudio backend for macOS and iOS.
//!
//! Uses `AudioUnit` with a render callback to drive the pull-based audio
//! pipeline. The render callback is invoked on a high-priority audio thread
//! by the system and calls [`AudioManager::pull`] to fetch PCM samples.
//!

use coreaudio_sys::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, kAudioUnitScope_Output,
    kAudioUnitType_Output, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioStreamBasicDescription, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
};

#[cfg(target_os = "macos")]
use coreaudio_sys::kAudioUnitSubType_DefaultOutput as kOutputUnitSubType;
#[cfg(target_os = "ios")]
use coreaudio_sys::kAudioUnitSubType_GenericOutput as kOutputUnitSubType;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::error::AudioError;
use crate::hal::{AudioDevice, AudioManager};
use crate::types::AudioFormat;

/// CoreAudio `noErr` status code (OSStatus = i32).
const NO_ERR: i32 = 0;

#[cfg(target_os = "macos")]
const AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
#[cfg(target_os = "macos")]
const AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = u32::from_be_bytes(*b"dOut");
#[cfg(target_os = "macos")]
const AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");

/// Shared by native property callbacks and the player's maintenance thread.
/// Native callbacks only set this flag; rebuilding an AudioUnit is never a
/// realtime operation.
struct RebuildSignal {
    requested: AtomicBool,
    shutdown: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

impl RebuildSignal {
    fn request(&self) {
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
        self.wake.wait_for(&mut guard, Duration::from_millis(250));
        !self.shutdown.load(Ordering::Acquire)
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

extern "C" fn audio_unit_property_changed(
    user_data: *mut c_void,
    _audio_unit: sys::AudioUnit,
    _property_id: u32,
    _scope: u32,
    _element: u32,
) -> i32 {
    if !user_data.is_null() {
        let signal = unsafe { &*(user_data as *const RebuildSignal) };
        signal.request();
    }
    NO_ERR
}

#[cfg(target_os = "macos")]
extern "C" fn default_output_device_changed(
    user_data: *mut c_void,
    _object_id: u32,
    _address_count: u32,
    _addresses: *const sys::AudioObjectPropertyAddress,
) -> i32 {
    if !user_data.is_null() {
        let signal = unsafe { &*(user_data as *const RebuildSignal) };
        signal.request();
    }
    NO_ERR
}

/// Holds the manager pointer. Because
/// `AURenderCallbackStruct.inputProcRefCon` is a thin `*mut c_void`,
/// we store an indirection: a `Box<CallbackRef>` whose only field is
/// the manager pointer.
struct CallbackRef<M: AudioManager> {
    ptr: *const M,
}
// Safety: CallbackRef is only accessed from the audio thread via &self.
unsafe impl<M: AudioManager> Send for CallbackRef<M> {}
unsafe impl<M: AudioManager> Sync for CallbackRef<M> {}

/// An audio output device backed by CoreAudio (AudioUnit).
struct CoreAudioState<M: AudioManager> {
    /// Hardware format detected at open time.
    format: AudioFormat,
    audio_unit: Option<sys::AudioUnit>,
    /// Owns the manager Box and the CallbackRef indirection.
    /// Kept alive for the lifetime of the device.
    manager: Box<M>,
    _callback_ref: Option<Box<CallbackRef<M>>>,
    rebuild_signal: Arc<RebuildSignal>,
    running: bool,
    paused: bool,
    rebuilding: bool,
    #[cfg(target_os = "macos")]
    default_output_listener_registered: bool,
}

// Safety: CoreAudio handles are safe to send between threads.
unsafe impl<M: AudioManager> Send for CoreAudioState<M> {}

// ── Render callback trampoline (C ABI) ─────────────────────────────────

/// The C-callable render callback installed on the AudioUnit.
///
/// Bridges from the CoreAudio callback signature to our Rust
/// `AudioManager::pull()` method. **Lock-free**: the manager is
/// accessed via `&self` through a raw typed pointer.
extern "C" fn render_callback<M: AudioManager>(
    _in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const sys::AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut sys::AudioBufferList,
) -> i32 {
    // Safety: _in_ref_con points to a CallbackRef that outlives the
    // AudioUnit. We access it immutably — zero locks.
    if _in_ref_con.is_null() || io_data.is_null() {
        return NO_ERR;
    }

    let cb_ref: &CallbackRef<M> = unsafe { &*(_in_ref_con as *const CallbackRef<M>) };
    if cb_ref.ptr.is_null() {
        return NO_ERR;
    }
    let manager: &M = unsafe { &*cb_ref.ptr };
    let buffers = unsafe { &mut *io_data };
    let frame_count = in_number_frames as usize;

    if buffers.mNumberBuffers == 0 {
        return NO_ERR;
    }
    let buf_ptr = buffers.mBuffers[0].mData as *mut f32;
    if buf_ptr.is_null() {
        return NO_ERR;
    }

    let channel_count = buffers.mBuffers[0].mNumberChannels as usize;
    let sample_count = frame_count * channel_count;
    let output = unsafe { std::slice::from_raw_parts_mut(buf_ptr, sample_count) };

    // Pull samples — zero locks, &self access.
    let frames_written = manager.pull(output);

    // Clamp to the actual frame count to guard against a buggy / malicious
    // callback returning more frames than the buffer can hold (prevents both
    // an out-of-bounds slice panic and a usize overflow below).
    let frames_written = frames_written.min(frame_count);

    // If the callback returned fewer frames, zero-fill the remainder.
    let written_samples = frames_written * channel_count;
    if written_samples < sample_count {
        output[written_samples..].fill(0.0);
    }

    NO_ERR
}

// ── CoreAudioDevice ────────────────────────────────────────────────────

impl<M: AudioManager> CoreAudioState<M> {
    /// Create a new CoreAudio output device.
    ///
    /// Queries the hardware's native stream format (sample rate + channels)
    /// from the default output AudioUnit. The sample format is forced to
    /// 32-bit float interleaved — the lowest-latency path through CoreAudio.
    pub fn new(manager: M) -> Result<Self, AudioError> {
        let rebuild_signal = Arc::new(RebuildSignal {
            requested: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            wait_lock: Mutex::new(()),
            wake: Condvar::new(),
        });
        let (audio_unit, format) = Self::open_audio_unit(&rebuild_signal)?;

        #[cfg(target_os = "macos")]
        if let Err(error) = Self::register_default_output_listener(&rebuild_signal) {
            unsafe { AudioComponentInstanceDispose(audio_unit) };
            return Err(error);
        }

        Ok(Self {
            format,
            audio_unit: Some(audio_unit),
            manager: Box::new(manager),
            _callback_ref: None,
            rebuild_signal,
            running: false,
            paused: false,
            rebuilding: false,
            #[cfg(target_os = "macos")]
            default_output_listener_registered: true,
        })
    }

    #[cfg(target_os = "macos")]
    fn default_output_property_address() -> sys::AudioObjectPropertyAddress {
        sys::AudioObjectPropertyAddress {
            selector: AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
            scope: AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: 0,
        }
    }

    #[cfg(target_os = "macos")]
    fn register_default_output_listener(signal: &RebuildSignal) -> Result<(), AudioError> {
        let address = Self::default_output_property_address();
        let status = unsafe {
            sys::AudioObjectAddPropertyListener(
                AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                default_output_device_changed,
                signal as *const RebuildSignal as *mut c_void,
            )
        };
        if status == NO_ERR {
            Ok(())
        } else {
            Err(AudioError::BackendError(format!(
                "AudioObjectAddPropertyListener failed: {status}"
            )))
        }
    }

    fn open_audio_unit(
        signal: &RebuildSignal,
    ) -> Result<(sys::AudioUnit, AudioFormat), AudioError> {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kOutputUnitSubType,
            componentManufacturer: 0x6170706c, // 'appl'
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        // Find the default output component.
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc as *const _) };
        if component.is_null() {
            return Err(AudioError::DeviceNotFound);
        }

        // Instantiate the AudioUnit.
        let mut audio_unit: sys::AudioUnit = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &mut audio_unit) };
        if status != NO_ERR || audio_unit.is_null() {
            return Err(AudioError::DeviceBusy);
        }

        // Query the hardware's native output stream format to get the
        // optimal sample rate and channel count for lowest latency.
        let mut hw_asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioUnitGetProperty(
                audio_unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                0, // output bus
                &mut hw_asbd as *mut _ as *mut c_void,
                &mut size,
            )
        };
        // Fall back to sensible defaults if the query fails.
        let (sample_rate, channels) = if status == NO_ERR && hw_asbd.mChannelsPerFrame > 0 {
            (hw_asbd.mSampleRate as u32, hw_asbd.mChannelsPerFrame as u16)
        } else {
            (48000, 2)
        };

        let format = AudioFormat::new(sample_rate, channels);

        // Set the input stream format to 32-bit float at the hardware rate.
        let asbd = AudioStreamBasicDescription {
            mSampleRate: format.sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: (format.channels as u32) * 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: (format.channels as u32) * 4,
            mChannelsPerFrame: format.channels as u32,
            mBitsPerChannel: 32,
            mReserved: 0,
        };

        let status = unsafe {
            AudioUnitSetProperty(
                audio_unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0, // output bus
                &asbd as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if status != NO_ERR {
            unsafe { AudioComponentInstanceDispose(audio_unit) };
            return Err(AudioError::FormatNotSupported);
        }

        // The output unit reports route and stream-format changes through
        // this listener on Apple platforms. It only marks work for
        // the device worker; AudioUnit teardown is not callback-safe.
        let status = unsafe {
            sys::AudioUnitAddPropertyListener(
                audio_unit,
                kAudioUnitProperty_StreamFormat,
                audio_unit_property_changed,
                signal as *const RebuildSignal as *mut c_void,
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

    fn close_audio_unit(&mut self) {
        if let Some(audio_unit) = self.audio_unit.take() {
            if self.running {
                unsafe {
                    AudioOutputUnitStop(audio_unit);
                    AudioUnitUninitialize(audio_unit);
                }
            }
            unsafe {
                let _ = sys::AudioUnitRemovePropertyListenerWithUserData(
                    audio_unit,
                    kAudioUnitProperty_StreamFormat,
                    audio_unit_property_changed,
                    Arc::as_ptr(&self.rebuild_signal) as *mut c_void,
                );
                AudioComponentInstanceDispose(audio_unit);
            }
        }
        self._callback_ref = None;
        self.running = false;
    }
}

impl<M: AudioManager> CoreAudioState<M> {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn start(&mut self) -> Result<(), AudioError> {
        let au = self.audio_unit.as_ref().ok_or(AudioError::DeviceNotFound)?;

        if self.running {
            return Ok(());
        }

        // Initialize the AudioUnit (must be done once before start).
        let status = unsafe { AudioUnitInitialize(*au) };
        if status != NO_ERR {
            return Err(AudioError::BackendError(format!(
                "AudioUnitInitialize failed: {status}"
            )));
        }

        // Because `AURenderCallbackStruct.inputProcRefCon` is a thin
        // `*mut c_void`, it cannot hold a fat trait-object pointer
        // that stores a typed manager pointer. The device owns both the
        // manager Box and the CallbackRef Box.
        //
        // AudioManager::pull() takes `&self` — no Mutex needed.
        let manager_ptr: *const M = &*self.manager;
        let cb_ref = Box::new(CallbackRef { ptr: manager_ptr });
        let ref_con: *mut c_void = Box::into_raw(cb_ref) as *mut c_void;

        // Set the render callback.
        let cb_struct = sys::AURenderCallbackStruct {
            inputProc: Some(render_callback::<M>),
            inputProcRefCon: ref_con,
        };

        let status = unsafe {
            AudioUnitSetProperty(
                *au,
                sys::kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0, // output bus
                &cb_struct as *const _ as *const c_void,
                std::mem::size_of::<sys::AURenderCallbackStruct>() as u32,
            )
        };
        if status != NO_ERR {
            // Clean up on failure.
            unsafe {
                let _ = Box::from_raw(ref_con as *mut CallbackRef<M>);
            }
            return Err(AudioError::BackendError(format!(
                "failed to set render callback: {status}"
            )));
        }

        // Start the AudioUnit.
        let status = unsafe { AudioOutputUnitStart(*au) };
        if status != NO_ERR {
            // Clean up on failure.
            unsafe {
                let _ = Box::from_raw(ref_con as *mut CallbackRef<M>);
            }
            return Err(AudioError::BackendError(format!(
                "AudioOutputUnitStart failed: {status}"
            )));
        }

        // Reconstruct the callback indirection so Drop can reclaim it.
        self._callback_ref = Some(unsafe { Box::from_raw(ref_con as *mut CallbackRef<M>) });
        self.running = true;
        self.paused = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(au) = self.audio_unit.as_ref() {
            if self.running {
                unsafe {
                    AudioOutputUnitStop(*au);
                    AudioUnitUninitialize(*au);
                }
            }
        }
        self.running = false;
        self.paused = false;

        // Reclaim the manager after the audio unit has stopped.
        self._callback_ref = None;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        if let Some(au) = self.audio_unit.as_ref() {
            if self.running {
                unsafe {
                    AudioOutputUnitStop(*au);
                }
                self.paused = true;
            }
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        if let Some(au) = self.audio_unit.as_ref() {
            if self.running {
                let status = unsafe { AudioOutputUnitStart(*au) };
                if status != NO_ERR {
                    return Err(AudioError::BackendError(format!(
                        "AudioOutputUnitStart failed: {status}"
                    )));
                }
                self.paused = false;
            }
        }
        Ok(())
    }

    /// Returns whether the device worker should retry after a delay.
    fn rebuild(&mut self) -> bool {
        if !self.rebuilding {
            self.manager.on_device_invalidated();
            self.rebuilding = true;
        }

        let was_running = self.running;
        let was_paused = self.paused;
        self.close_audio_unit();

        match Self::open_audio_unit(&self.rebuild_signal) {
            Ok((audio_unit, format)) => {
                self.audio_unit = Some(audio_unit);
                self.format = format;
                self.manager.on_device_format_changed(format);

                if was_running {
                    if self.start().is_err() {
                        self.close_audio_unit();
                        return true;
                    }
                    if was_paused && self.pause().is_err() {
                        self.close_audio_unit();
                        return true;
                    }
                }

                self.rebuilding = false;
                self.manager.on_device_recovered();
                false
            }
            Err(_) => {
                // A route can disappear transiently during Bluetooth or wired
                // device negotiation. Keep retrying from the maintenance
                // thread without surfacing an asynchronous error to callers.
                true
            }
        }
    }
}

impl<M: AudioManager> Drop for CoreAudioState<M> {
    fn drop(&mut self) {
        self.close_audio_unit();
        #[cfg(target_os = "macos")]
        if self.default_output_listener_registered {
            let address = Self::default_output_property_address();
            unsafe {
                let _ = sys::AudioObjectRemovePropertyListener(
                    AUDIO_OBJECT_SYSTEM_OBJECT,
                    &address,
                    default_output_device_changed,
                    Arc::as_ptr(&self.rebuild_signal) as *mut c_void,
                );
            }
        }
    }
}

/// An audio output device backed by CoreAudio (AudioUnit).
///
/// Native route notifications wake a device-owned worker. The worker owns
/// endpoint recovery; callers only use the regular [`AudioDevice`] controls.
pub struct CoreAudioDevice<M: AudioManager> {
    state: Arc<Mutex<CoreAudioState<M>>>,
    rebuild_signal: Arc<RebuildSignal>,
    worker: Option<JoinHandle<()>>,
}

impl<M: AudioManager> CoreAudioDevice<M> {
    pub fn new(manager: M) -> Result<Self, AudioError> {
        let state = Arc::new(Mutex::new(CoreAudioState::new(manager)?));
        let rebuild_signal = Arc::clone(&state.lock().rebuild_signal);
        let worker_state = Arc::clone(&state);
        let worker_signal = Arc::clone(&rebuild_signal);
        let worker = thread::Builder::new()
            .name("uniasset-coreaudio".into())
            .spawn(move || run_device_worker(worker_state, worker_signal))
            .map_err(|error| {
                AudioError::BackendError(format!("failed to spawn CoreAudio worker: {error}"))
            })?;

        Ok(Self {
            state,
            rebuild_signal,
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

        // Clear a coalesced notification before recovery. If another route
        // change arrives while rebuilding, it remains pending for the next
        // iteration.
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
        self.rebuild_signal.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Internal alias for the coreaudio-sys re-exports, keeping things readable.
mod sys {
    pub use coreaudio_sys::*;

    pub type AudioUnitPropertyListenerProc = extern "C" fn(
        user_data: *mut std::ffi::c_void,
        audio_unit: AudioUnit,
        property_id: u32,
        scope: u32,
        element: u32,
    ) -> i32;

    #[cfg(target_os = "macos")]
    #[repr(C)]
    pub struct AudioObjectPropertyAddress {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    #[cfg(target_os = "macos")]
    pub type AudioObjectPropertyListenerProc = extern "C" fn(
        user_data: *mut std::ffi::c_void,
        object_id: u32,
        address_count: u32,
        addresses: *const AudioObjectPropertyAddress,
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

        #[cfg(target_os = "macos")]
        pub fn AudioObjectAddPropertyListener(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            listener: AudioObjectPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
        #[cfg(target_os = "macos")]
        pub fn AudioObjectRemovePropertyListener(
            object_id: u32,
            address: *const AudioObjectPropertyAddress,
            listener: AudioObjectPropertyListenerProc,
            user_data: *mut std::ffi::c_void,
        ) -> i32;
    }
}
