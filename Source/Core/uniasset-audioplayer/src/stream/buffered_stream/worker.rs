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

static WORKER: Mutex<WorkerState> = Mutex::new(WorkerState::new());
static WORKER_CV: Condvar = Condvar::new();

/// State shared by the control threads and the one buffer-filling thread.
///
/// The mutex is only used for registry and lifecycle changes. The worker takes
/// an `Arc` snapshot before it reads streams, so slow stream I/O never blocks
/// stream construction, destruction, or seek notifications.
struct WorkerState {
    handle_count: usize,
    running: bool,
    refill_requested: bool,
    groups: Vec<Arc<BufferGroup>>,
}

impl WorkerState {
    const fn new() -> Self {
        Self {
            handle_count: 0,
            running: false,
            refill_requested: false,
            groups: Vec::new(),
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
    discard_before: AtomicU64,
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
            discard_before: AtomicU64::new(0),
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
    ///
    /// ⚠️ 有效读游标 = max(read_ptr, discard_before)，与 `read()` 保持一致：
    /// seek 的 discard 请求只前移 discard_before、不动 read_ptr，若此处仍用裸
    /// read_ptr，seek 后可用量仍显示为旧缓冲水位 → worker 的 free_space ≈ 0
    /// 不再填充，而消费端已从 discard_before 位置读取（读不到数据），
    /// 缓冲永久"满但不可读"（等待期预填时 seek 必现，画面/音乐冻结）。
    #[inline]
    pub fn available(&self) -> usize {
        let write = self.write_ptr.load(Ordering::Acquire);
        let read = self
            .read_ptr
            .load(Ordering::Relaxed)
            .max(self.discard_before.load(Ordering::Acquire));
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

    /// Total samples consumed since creation (`read_ptr` is monotonic).
    ///
    /// Note: a seek does not reset this counter — callers combine it with
    /// their own seek base to obtain an absolute stream position.
    #[inline]
    pub fn total_consumed_samples(&self) -> u64 {
        let read = self.read_ptr.load(Ordering::Relaxed);
        let discard = self.discard_before.load(Ordering::Acquire);
        read.max(discard)
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
        let read = self
            .read_ptr
            .load(Ordering::Acquire)
            .max(self.discard_before.load(Ordering::Acquire));
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

        // `read_ptr` has exactly one writer: this consumer. A seek only
        // advances `discard_before`; the consumer applies that request before
        // copying, so the producer can never overwrite a slot being read.
        let read = self
            .read_ptr
            .load(Ordering::Relaxed)
            .max(self.discard_before.load(Ordering::Acquire));
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

    /// Request that the audio-thread consumer discard samples buffered at the
    /// time of this call, before its next read. This is safe to call from a
    /// control thread.
    ///
    /// The request deliberately does not modify `read_ptr`: advancing it while
    /// the consumer is copying samples could let the producer overwrite a slot
    /// that is still being read.
    pub fn discard_buffered_samples(&self) {
        let write = self.write_ptr.load(Ordering::Acquire);
        self.discard_before.fetch_max(write, Ordering::Release);
    }
}

struct BufferGroup {
    stream: Arc<dyn AudioStream>,
    buffer: Arc<AudioBuffer>,
    io_gate: Arc<Mutex<()>>,
}

/// Keeps the shared worker thread alive while a buffered stream exists.
pub struct WorkerHandle;

impl WorkerHandle {
    /// Wake the worker thread so it re-evaluates watermark levels.
    pub fn notify(&self) {
        let mut registry = WORKER.lock();
        registry.refill_requested = true;
        WORKER_CV.notify_one();
    }

    /// Register a stream/buffer pair for background filling.
    pub fn add_buffer_group(
        &self,
        stream: Arc<dyn AudioStream>,
        buffer: Arc<AudioBuffer>,
        io_gate: Arc<Mutex<()>>,
    ) {
        let mut registry = WORKER.lock();
        registry.groups.push(Arc::new(BufferGroup {
            stream,
            buffer,
            io_gate,
        }));
        registry.refill_requested = true;
        WORKER_CV.notify_one();
    }

    /// Unregister the group associated with `buffer`.
    pub fn remove_buffer_group(&self, buffer: &Arc<AudioBuffer>) {
        let mut registry = WORKER.lock();
        registry
            .groups
            .retain(|group| !Arc::ptr_eq(&group.buffer, buffer));
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let mut registry = WORKER.lock();
        debug_assert!(registry.handle_count > 0);
        registry.handle_count -= 1;
        if registry.handle_count == 0 {
            WORKER_CV.notify_all();
        }
    }
}

pub fn acquire_worker_handle() -> Result<WorkerHandle, io::Error> {
    let mut registry = WORKER.lock();
    registry.handle_count += 1;

    if !registry.running {
        registry.running = true;
        if let Err(error) = thread::Builder::new()
            .name("buffered-stream-worker".to_owned())
            .spawn(worker_thread)
        {
            registry.handle_count -= 1;
            registry.running = false;
            return Err(error);
        }
    }

    Ok(WorkerHandle)
}

/// Background thread that keeps all registered ring buffers above the
/// watermark by reading ahead from their inner streams.
fn worker_thread() {
    // Reusable temporary buffer for stream reads; grows on demand.
    let mut temp_buf: Vec<f32> = vec![0.0f32; 4096];

    loop {
        // The snapshot isolates control-plane synchronization from arbitrary
        // stream I/O. It is also safe if a wrapper is dropped during a pass:
        // the snapshot keeps its resources alive until this iteration ends.
        let groups = {
            let mut registry = WORKER.lock();
            while registry.handle_count != 0 && !registry.refill_requested {
                WORKER_CV.wait(&mut registry);
            }

            if registry.handle_count == 0 {
                registry.running = false;
                registry.refill_requested = false;
                return;
            }

            registry.refill_requested = false;
            registry.groups.clone()
        };

        // Keep filling until every buffer is above the watermark or dry.
        loop {
            let mut all_above = true;
            let mut made_progress = false;

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

                // A seek must not interleave with an in-flight read or with
                // publishing that read's samples into the ring buffer.
                let samples_read = {
                    let _io_guard = group.io_gate.lock();
                    let samples_read = group.stream.read(&mut temp_buf[..free], frame_count);
                    if samples_read > 0 {
                        group.buffer.write(&temp_buf[..samples_read]);
                    }
                    samples_read
                };

                if samples_read > 0 {
                    made_progress = true;
                }

                // Still below watermark?  Skip groups whose inner stream
                // has already ended — no more data will ever arrive, so
                // the buffer can never reach the watermark.
                if !group.stream.is_eof() && group.buffer.fill_level() < BUFFER_WATERMARK {
                    all_above = false;
                }
            }

            if all_above || !made_progress {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AudioBuffer;

    #[test]
    fn discard_preserves_samples_published_after_the_request() {
        let buffer = AudioBuffer::new(8);
        assert_eq!(buffer.write(&[1.0, 2.0, 3.0, 4.0]), 4);

        buffer.discard_buffered_samples();
        assert_eq!(buffer.write(&[5.0, 6.0]), 2);

        let mut output = [0.0; 2];
        assert_eq!(buffer.read(&mut output), 2);
        assert_eq!(output, [5.0, 6.0]);
    }

    #[test]
    fn latest_discard_request_wins() {
        let buffer = AudioBuffer::new(8);
        assert_eq!(buffer.write(&[1.0, 2.0, 3.0, 4.0]), 4);
        buffer.discard_buffered_samples();
        assert_eq!(buffer.write(&[5.0, 6.0]), 2);
        buffer.discard_buffered_samples();
        assert_eq!(buffer.write(&[7.0, 8.0]), 2);

        let mut output = [0.0; 2];
        assert_eq!(buffer.read(&mut output), 2);
        assert_eq!(output, [7.0, 8.0]);
    }
}
