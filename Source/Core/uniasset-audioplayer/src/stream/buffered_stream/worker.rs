use std::{
    cell::UnsafeCell,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};

use parking_lot::{Condvar, Mutex};

use super::BUFFER_WATERMARK;
use crate::mixer::AudioStream;

static WORKER_STATE: Mutex<WorkerState> = Mutex::new(WorkerState::new());
static WORKER_CV: Condvar = Condvar::new();
static BUFFER_GROUPS: Mutex<Vec<BufferGroup>> = Mutex::new(Vec::new());

/// Lifecycle state for the single shared buffer-filling worker.
///
/// All transitions that create, reuse, or stop the worker happen while this
/// mutex is held. This prevents a newly acquired handle from spawning another
/// worker while a previous one is waking up to exit.
struct WorkerState {
    handle_count: usize,
    running: bool,
    work_pending: bool,
}

impl WorkerState {
    const fn new() -> Self {
        Self {
            handle_count: 0,
            running: false,
            work_pending: false,
        }
    }
}

/// A lock-free single-producer single-consumer ring buffer for `f32` audio samples.
///
/// The **producer** (worker thread) calls [`write`](AudioBuffer::write) and the
/// **consumer** (audio thread) calls [`read`](AudioBuffer::read). Pointers are
/// monotonically increasing `u64` values — the actual index is `ptr % capacity`.
pub struct AudioBuffer {
    data: Box<UnsafeCell<[f32]>>,
    read_ptr: AtomicU64,
    write_ptr: AtomicU64,
    capacity: usize,
}

unsafe impl Send for AudioBuffer {}
unsafe impl Sync for AudioBuffer {}

impl AudioBuffer {
    /// Create a zero-initialised ring buffer that holds `size` `f32` samples.
    pub fn new(size: usize) -> Self {
        let data: Box<[f32]> = vec![0.0f32; size].into_boxed_slice();
        Self {
            data: unsafe { Box::from_raw(Box::into_raw(data) as *mut UnsafeCell<[f32]>) },
            write_ptr: AtomicU64::new(0),
            read_ptr: AtomicU64::new(0),
            capacity: size,
        }
    }

    // /// Total number of `f32` samples the buffer can hold.
    // #[inline]
    // pub fn capacity(&self) -> usize {
    //     self.capacity
    // }

    /// Number of samples currently available to read.
    ///
    /// Safe to call from any thread but the result is a snapshot — the other
    /// side may advance between the call and the next operation.
    #[inline]
    pub fn available(&self) -> usize {
        let write = self.write_ptr.load(Ordering::Acquire);
        let read = self.read_ptr.load(Ordering::Relaxed);
        (write - read) as usize
    }

    /// Number of sample slots that can still be written.
    #[inline]
    pub fn free_space(&self) -> usize {
        self.capacity - self.available()
    }

    /// Fill level as a fraction in `[0.0, 1.0]`.
    #[inline]
    pub fn fill_level(&self) -> f32 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.available() as f32 / self.capacity as f32
    }

    /// Write samples into the ring buffer (producer side — worker thread).
    ///
    /// Returns the number of samples actually written (may be less than
    /// `samples.len()` if the buffer is nearly full).
    pub fn write(&self, samples: &[f32]) -> usize {
        let cap = self.capacity;
        if cap == 0 {
            return 0;
        }

        let write = self.write_ptr.load(Ordering::Relaxed);
        let read = self.read_ptr.load(Ordering::Acquire);
        let available = (write - read) as usize;
        let free = cap - available;
        let to_write = samples.len().min(free);

        if to_write == 0 {
            return 0;
        }

        let start = (write as usize) % cap;
        let data = unsafe { &mut *self.data.get() };

        if start + to_write <= cap {
            data[start..start + to_write].copy_from_slice(&samples[..to_write]);
        } else {
            let first = cap - start;
            data[start..].copy_from_slice(&samples[..first]);
            data[..to_write - first].copy_from_slice(&samples[first..to_write]);
        }

        self.write_ptr
            .store(write + to_write as u64, Ordering::Release);
        to_write
    }

    /// Read samples from the ring buffer (consumer side — audio thread).
    ///
    /// Returns the number of samples actually copied into `buffer` (may be
    /// less than `buffer.len()` if not enough data is available).
    ///
    /// Must be **wait-free** — called from the real-time audio thread.
    pub fn read(&self, buffer: &mut [f32]) -> usize {
        let cap = self.capacity;
        if cap == 0 {
            return 0;
        }

        let read = self.read_ptr.load(Ordering::Relaxed);
        let write = self.write_ptr.load(Ordering::Acquire);
        let available = (write - read) as usize;
        let to_read = buffer.len().min(available);

        if to_read == 0 {
            return 0;
        }

        let start = (read as usize) % cap;
        let data = unsafe { &*self.data.get() };

        if start + to_read <= cap {
            buffer[..to_read].copy_from_slice(&data[start..start + to_read]);
        } else {
            let first = cap - start;
            buffer[..first].copy_from_slice(&data[start..]);
            buffer[first..to_read].copy_from_slice(&data[..to_read - first]);
        }

        self.read_ptr
            .store(read + to_read as u64, Ordering::Release);
        to_read
    }

    /// Discard all buffered data by advancing the read pointer to the write
    /// pointer. Safe to call from the control thread (e.g., after a seek).
    pub fn reset(&self) {
        let write = self.write_ptr.load(Ordering::Acquire);
        self.read_ptr.store(write, Ordering::Release);
    }

    // /// Atomically drain all available samples and return how many were
    // /// discarded. Useful when tearing down a stream.
    // pub fn drain(&self) -> u64 {
    //     let write = self.write_ptr.load(Ordering::Acquire);
    //     let read = self.read_ptr.load(Ordering::Relaxed);
    //     let drained = write - read;
    //     self.read_ptr.store(write, Ordering::Release);
    //     drained
    // }
}

struct BufferGroup {
    stream: Arc<dyn AudioStream>,
    buffer: Arc<AudioBuffer>,
}

pub struct WorkerHandle();

impl WorkerHandle {
    /// Wake the worker thread so it re-evaluates watermark levels.
    pub fn notify(&self) {
        let mut state = WORKER_STATE.lock();
        state.work_pending = true;
        WORKER_CV.notify_one();
    }

    /// Register a stream/buffer pair for background filling.
    pub fn add_buffer_group(&self, stream: Arc<dyn AudioStream>, buffer: Arc<AudioBuffer>) {
        let mut groups = BUFFER_GROUPS.lock();
        groups.push(BufferGroup { stream, buffer });
    }

    /// Unregister all buffer groups that share the same inner stream pointer.
    pub fn remove_buffer_group(&self, stream: &Arc<dyn AudioStream>) {
        let ptr = Arc::as_ptr(stream);
        let mut groups = BUFFER_GROUPS.lock();
        groups.retain(|g| !std::ptr::addr_eq(Arc::as_ptr(&g.stream), ptr));
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let mut state = WORKER_STATE.lock();
        debug_assert!(state.handle_count > 0);
        state.handle_count -= 1;
        if state.handle_count == 0 {
            WORKER_CV.notify_all();
        }
    }
}

pub fn acquire_worker_handle() -> Result<WorkerHandle, io::Error> {
    let mut state = WORKER_STATE.lock();
    state.handle_count += 1;

    if !state.running {
        state.running = true;
        if let Err(error) = thread::Builder::new()
            .name("buffered-stream-worker".to_owned())
            .spawn(worker_thread)
        {
            state.handle_count -= 1;
            state.running = false;
            return Err(error);
        }
    }

    Ok(WorkerHandle())
}

/// Background thread that keeps all registered ring buffers above the
/// watermark by reading ahead from their inner streams.
fn worker_thread() {
    // Reusable temporary buffer for stream reads; grows on demand.
    let mut temp_buf: Vec<f32> = vec![0.0f32; 4096];

    loop {
        // Wait until there is new work, or terminate after the final handle
        // has been released. The predicate prevents notifications from being
        // lost between a fill pass and the next wait.
        {
            let mut state = WORKER_STATE.lock();
            while state.handle_count != 0 && !state.work_pending {
                WORKER_CV.wait(&mut state);
            }

            if state.handle_count == 0 {
                state.running = false;
                state.work_pending = false;
                return;
            }

            state.work_pending = false;
        }

        // Keep filling until every buffer is above the watermark or dry.
        loop {
            let groups = BUFFER_GROUPS.lock();
            let mut all_above = true;

            for group in groups.iter() {
                let free = group.buffer.free_space();
                if free == 0 {
                    continue;
                }

                // Grow the temp buffer if needed.
                if free > temp_buf.len() {
                    temp_buf.resize(free, 0.0f32);
                }

                let channels = group.stream.channels() as u64;
                let frame_count = (free as u64) / channels.max(1);

                let samples_read = group.stream.read(&mut temp_buf[..free], frame_count);

                if samples_read > 0 {
                    group.buffer.write(&temp_buf[..samples_read]);
                }

                // Still below watermark?  Skip groups whose inner stream
                // has already ended — no more data will ever arrive, so
                // the buffer can never reach the watermark.
                if !group.stream.is_eof() && group.buffer.fill_level() < BUFFER_WATERMARK {
                    all_above = false;
                }
            }

            // Drop the lock before the next round so add/remove can proceed.
            if all_above {
                break;
            }
        }
    }
}
