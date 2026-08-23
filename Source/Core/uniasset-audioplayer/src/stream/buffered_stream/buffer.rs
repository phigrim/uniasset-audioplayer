use std::{
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;

use crate::{
    mixer::AudioStream,
    stream::buffered_stream::worker::{acquire_worker_handle, AudioBuffer, WorkerHandle},
    AudioError,
};

use super::BUFFER_WATERMARK;

/// A stream wrapper that maintains a bounded ring buffer filled ahead of
/// time by a background worker thread.
///
/// The audio thread reads from the ring buffer (wait-free) while the worker
/// thread keeps the buffer above [`BUFFER_WATERMARK`] by reading ahead from
/// `inner`.
pub struct BufferedAudioStream {
    inner: Arc<dyn AudioStream>,
    worker_handle: WorkerHandle,
    buffer: Arc<AudioBuffer>,
    /// Serializes control-thread seeks with the worker's non-real-time reads.
    io_gate: Arc<Mutex<()>>,
    /// Seek 基准帧：`consumed_frames()` = 本值 + ring buffer 已消费样本/声道数。
    /// seek 时更新为本值 + 目标帧，使位置查询跨 seek 连续（游戏音画同步用）。
    consumed_base_frames: AtomicU64,
}

impl BufferedAudioStream {
    /// Wrap `inner` with a ring buffer of `buffer_duration` and register it
    /// with the background worker.
    pub fn new(inner: Arc<dyn AudioStream>, buffer_duration: Duration) -> Result<Self, io::Error> {
        let capacity = buffer_capacity(inner.sample_rate(), inner.channels(), buffer_duration)?;
        let worker_handle = acquire_worker_handle()?;
        let buffer = Arc::new(AudioBuffer::new(capacity));
        let io_gate = Arc::new(Mutex::new(()));

        worker_handle.add_buffer_group(inner.clone(), buffer.clone(), io_gate.clone());

        Ok(Self {
            inner,
            worker_handle,
            buffer,
            io_gate,
            consumed_base_frames: AtomicU64::new(0),
        })
    }

    /// 实际已被音频硬件消费的帧数（含欠载停顿——欠载时该值不推进）。
    ///
    /// 谱面时间基准用此值而非墙钟，可保证音画同步不因渲染卡顿漂移：
    /// 音乐丢样本时谱面随之等待而非继续跑。
    pub fn consumed_frames(&self) -> u64 {
        let channels = self.inner.channels().max(1) as u64;
        self.consumed_base_frames.load(Ordering::Acquire)
            + self.buffer.total_consumed_samples() / channels
    }
}

impl Drop for BufferedAudioStream {
    fn drop(&mut self) {
        self.worker_handle.remove_buffer_group(&self.buffer);
    }
}

fn buffer_capacity(
    sample_rate: u32,
    channels: u16,
    duration: Duration,
) -> Result<usize, io::Error> {
    let samples_per_second = (sample_rate as usize)
        .checked_mul(channels as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audio format is too large"))?;
    let whole_seconds = usize::try_from(duration.as_secs()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer duration is too large for this platform",
        )
    })?;
    let whole_samples = samples_per_second
        .checked_mul(whole_seconds)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "buffer capacity is too large")
        })?;
    let fractional_samples = samples_per_second
        .checked_mul(duration.subsec_nanos() as usize)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "buffer capacity is too large")
        })?
        / 1_000_000_000;
    let capacity = whole_samples
        .checked_add(fractional_samples)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "buffer capacity is too large")
        })?;

    if capacity == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer duration must contain at least one sample",
        ));
    }

    Ok(capacity)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::buffer_capacity;

    #[test]
    fn buffer_capacity_supports_fractional_seconds() {
        assert_eq!(
            buffer_capacity(48_000, 2, Duration::from_millis(1_500)).unwrap(),
            144_000
        );
    }

    #[test]
    fn buffer_capacity_rejects_zero_duration() {
        assert!(buffer_capacity(48_000, 2, Duration::ZERO).is_err());
    }
}

impl AudioStream for BufferedAudioStream {
    /// Read interleaved `f32` samples from the ring buffer.
    ///
    /// Samples that cannot be satisfied from the buffer are filled with
    /// silence (zeros). If after reading the fill level drops below
    /// [`BUFFER_WATERMARK`], the worker thread is notified to top up.
    ///
    /// Always writes exactly `buffer.len()` samples — wait-free.
    fn read(&self, buffer: &mut [f32], _frame_count: u64) -> usize {
        let n = self.buffer.read(buffer);

        // Zero-fill any shortfall (buffer underrun = silence).
        if n < buffer.len() {
            buffer[n..].fill(0.0);
        }

        // Wake the worker if we dipped below the watermark.
        if self.buffer.fill_level() < BUFFER_WATERMARK {
            self.worker_handle.notify();
        }

        buffer.len()
    }

    /// Seek the inner stream to `frame` and discard buffered data so the
    /// worker refills from the new position.
    fn seek(&self, frame: u64) -> Result<(), AudioError> {
        // The worker holds this gate from `inner.read()` through publishing
        // the resulting samples. Therefore an old read cannot be published
        // after this seek has completed.
        {
            let _io_guard = self.io_gate.lock();
            self.inner.seek(frame)?;
            self.buffer.discard_buffered_samples();
        }
        // 位置查询基准跳到目标帧：consumed_frames() = base + 已消费样本/声道数。
        // ⚠️ 绝不能重置 ring buffer 的 read_ptr——它是音频线程的消费游标，
        // 归零会让下一次 read 从环形缓冲最老数据处重新消费（回放旧样本）且
        // 与 write_ptr/discard_before 体系错位，直接破坏播放。位置连续性由
        // base 单独承载。
        self.consumed_base_frames.store(
            frame.wrapping_sub(self.buffer.total_consumed_samples() / self.inner.channels().max(1) as u64),
            Ordering::Release,
        );
        self.worker_handle.notify();
        Ok(())
    }

    /// Returns `true` when the inner stream has ended **and** the ring buffer
    /// has been fully consumed (no more data to deliver).
    fn is_eof(&self) -> bool {
        self.inner.is_eof() && self.buffer.available() == 0
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
}
