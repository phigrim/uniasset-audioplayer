use std::{io, sync::Arc, time::Duration};

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
        })
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
