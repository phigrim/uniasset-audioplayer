use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::Mutex;

use crate::hal::AudioManager;
use crate::mixer::{AudioStream, PlayHandle, StreamState};
use crate::AudioFormat;

/// A stream entry in the control-thread's live list.
struct MixerEntry {
    stream: Arc<dyn AudioStream>,
    state: Arc<StreamState>,
}

/// A frozen snapshot of all streams for lock-free audio thread access.
struct MixerSnapshot {
    /// Output format and derived resampling ratios are published together.
    format: AudioFormat,
    entries: Vec<SnapshotEntry>,
}

/// One stream entry inside a snapshot. All per-stream mutable data is
/// behind atomics so the audio thread can read/write without locks.
struct SnapshotEntry {
    stream: Arc<dyn AudioStream>,
    state: Arc<StreamState>,

    /// Cached stream channel count.
    stream_channels: u16,

    /// Resample ratio: `stream_rate / mixer_rate`.
    /// > 1.0 → stream has higher rate; < 1.0 → stream has lower rate.
    ratio: f64,

    /// Current resample position as `f64::to_bits()`.
    /// Only the audio thread writes this.
    position: AtomicU64,
}

/// Scratch buffers reused across audio callbacks to avoid per-call
/// allocation on the hot path.
struct ScratchBuf {
    /// Temporary buffer for reading from one stream.
    stream_buf: Vec<f32>,
    /// Accumulation buffer for mixing all streams together.
    mix_buf: Vec<f32>,
}

/// A lock-free audio mixer that combines multiple [`AudioStream`]s into
/// a single output.
///
/// The mixer implements [`AudioManager`] so it can be passed directly to
/// [`AudioDevice::start`](crate::hal::AudioDevice::start).
///
/// # Example
///
/// ```ignore
/// use uniasset::mixer::{Mixer, AudioStream};
///
/// let mixer = Mixer::new(48000, 2);
/// let stream: Arc<dyn AudioStream> = ...;
/// let handle = mixer.add_stream(stream);
/// handle.set_volume(0.5);
/// ```
pub struct Mixer {
    /// Current read-only snapshot, atomically swapped via `ArcSwap`.
    /// `load()` returns a temporary `Arc` that keeps the snapshot alive
    /// for the duration of the audio callback — no unsafe, no deferred-free.
    snapshot: ArcSwap<MixerSnapshot>,

    /// Live stream list — only accessed from the control thread.
    /// Protected by a mutex; uncontended (only the control thread writes,
    /// and only during add/remove/cleanup).
    entries: Mutex<Vec<MixerEntry>>,

    /// Serializes snapshot publication from control and device-maintenance
    /// threads. The audio callback never takes this lock.
    snapshot_update: Mutex<()>,

    /// Scratch buffers for the audio thread.
    /// Wrapped in `UnsafeCell` — only the audio thread accesses this.
    /// Pre-allocated capacity removes allocation from the hot path.
    scratch: UnsafeCell<ScratchBuf>,

    /// Device-change epoch (§ unity parity): bumped whenever the underlying
    /// audio endpoint is invalidated, reformatted or recovered (e.g. speaker →
    /// Bluetooth switch). Hosts poll this to auto-pause gameplay like Unity's
    /// `AudioSettings.OnAudioConfigurationChanged`.
    device_change_epoch: AtomicU64,
}

// Safety: Mixer requires Send + Sync for AudioManager.
// All mutable state is behind atomics, ArcSwap, UnsafeCell
// (single-thread access), or Mutex (control-thread-only).
unsafe impl Send for Mixer {}
unsafe impl Sync for Mixer {}

impl Mixer {
    /// Create a new mixer with the given target format.
    ///
    /// `sample_rate` and `channels` should match the hardware format
    /// reported by [`AudioDevice::format`](crate::hal::AudioDevice::format).
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let format = AudioFormat::new(sample_rate, channels);

        // Pre-allocate scratch buffers for the audio hot path.
        // 8192 samples for stream_buf covers any realistic callback size
        // (256–1024 frames) with upsampling ratio (e.g. 96 kHz → 44.1 kHz).
        // 4096 samples for mix_buf covers 1024 frames × 4 channels.
        // Growth beyond these is a one-time allocation, not steady-state.
        let stream_buf = Vec::with_capacity(8192);
        let mix_buf = Vec::with_capacity(4096);

        Self {
            snapshot: ArcSwap::new(Arc::new(MixerSnapshot {
                format,
                entries: Vec::new(),
            })),
            entries: Mutex::new(Vec::new()),
            snapshot_update: Mutex::new(()),
            scratch: UnsafeCell::new(ScratchBuf {
                stream_buf,
                mix_buf,
            }),
            device_change_epoch: AtomicU64::new(0),
        }
    }

    /// Current device-change epoch. Increments on every endpoint invalidation,
    /// format change or recovery; poll from the host thread to detect switches.
    pub fn device_change_epoch(&self) -> u64 {
        self.device_change_epoch.load(Ordering::Acquire)
    }

    fn bump_device_change_epoch(&self) {
        self.device_change_epoch.fetch_add(1, Ordering::Release);
    }

    /// Return the target audio format.
    pub fn format(&self) -> AudioFormat {
        self.snapshot.load().format
    }

    /// Update the target format after the HAL has stopped the old endpoint.
    ///
    /// Rebuilding the snapshot updates each stream's resampling ratio before
    /// playback resumes on the replacement endpoint.
    fn reconfigure_format(&self, format: AudioFormat) {
        let _update = self.snapshot_update.lock();
        if self.snapshot.load().format == format {
            return;
        }
        self.publish_snapshot(format);
    }

    /// Number of streams currently in the mixer.
    pub fn stream_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// Add an audio stream to the mixer.
    ///
    /// Returns a [`PlayHandle`] that can be used to control playback
    /// (volume, pause, seek, modifier).
    pub fn add_stream(&self, stream: Arc<dyn AudioStream>, play_immediate: bool) -> PlayHandle {
        let state = Arc::new(StreamState {
            paused: AtomicBool::new(!play_immediate),
            ..Default::default()
        });

        let handle = PlayHandle {
            state: Arc::clone(&state),
            stream: Arc::clone(&stream),
        };

        self.entries.lock().push(MixerEntry { stream, state });

        self.rebuild_snapshot();
        handle
    }

    /// Remove streams that have reached EOF.
    ///
    /// Call this periodically from the control thread (e.g., on a timer or
    /// at application idle). The audio thread only sets the `eof` flag; it
    /// never deallocates.
    pub fn cleanup_eof(&self) {
        let mut entries = self.entries.lock();
        let prev_len = entries.len();
        entries.retain(|entry| {
            if entry.state.eof.load(Ordering::Relaxed) {
                entry.state.alive.store(false, Ordering::Release);
                false
            } else {
                true
            }
        });
        if entries.len() != prev_len {
            drop(entries);
            self.rebuild_snapshot();
        }
    }

    /// Rebuild the snapshot from the current live entry list and publish it
    /// via `ArcSwap::store`. The old snapshot is retained as long as any
    /// in-flight audio callback holds a reference via `load()`.
    fn rebuild_snapshot(&self) {
        let _update = self.snapshot_update.lock();
        let format = self.snapshot.load().format;
        self.publish_snapshot(format);
    }

    /// Build and publish a complete format + stream snapshot. The caller
    /// holds `snapshot_update`, so concurrent control operations cannot
    /// publish an older format after a device reconfiguration.
    fn publish_snapshot(&self, format: AudioFormat) {
        let mixer_rate = format.sample_rate as f64;

        // Carry forward resample positions from the current snapshot so
        // existing streams don't reset to 0 on every rebuild.
        let old_snapshot = self.snapshot.load();

        let new_entries = {
            let entries_guard = self.entries.lock();
            entries_guard
                .iter()
                .map(|entry| {
                    let stream_rate = entry.stream.sample_rate();
                    let ratio = stream_rate as f64 / mixer_rate;

                    // FIXME: performance
                    // Clone the old position if this stream was already
                    // in the previous snapshot; new streams start at 0.
                    let old_pos = old_snapshot
                        .entries
                        .iter()
                        .find(|old| Arc::ptr_eq(&old.state, &entry.state))
                        .map(|old| old.position.load(Ordering::Relaxed))
                        .unwrap_or(0u64);

                    SnapshotEntry {
                        stream: Arc::clone(&entry.stream),
                        state: Arc::clone(&entry.state),
                        stream_channels: entry.stream.channels(),
                        ratio,
                        position: AtomicU64::new(old_pos),
                    }
                })
                .collect::<Vec<_>>()
        };

        self.snapshot.store(Arc::new(MixerSnapshot {
            format,
            entries: new_entries,
        }));
    }
}

impl AudioManager for Mixer {
    fn pull(&self, buffer: &mut [f32]) -> usize {
        // ── Load snapshot (lock-free, memory-safe via ArcSwap) ──────────
        let snapshot = self.snapshot.load();
        let mixer_channels = snapshot.format.channels as usize;
        let frame_count = buffer.len() / mixer_channels;

        // Zero the output — we'll add (mix) into it.
        buffer.fill(0.0);

        if snapshot.entries.is_empty() {
            return frame_count;
        }

        // ── Acquire scratch buffers (lock-free) ──────────────────────────
        // Safety: Only the audio thread calls pull(). Calls are serialized
        // by the OS audio callback, so there is never concurrent access.
        let scratch = unsafe { &mut *self.scratch.get() };

        // Ensure the mix accumulation buffer is large enough and zeroed.
        scratch.mix_buf.resize(buffer.len(), 0.0);

        for entry in &snapshot.entries {
            // ── Skip paused / dead streams ─────────────────────────────
            if !entry.state.alive.load(Ordering::Relaxed) {
                continue;
            }
            if entry.state.paused.load(Ordering::Relaxed) {
                continue;
            }

            let stream_channels = entry.stream_channels as usize;
            let ratio = entry.ratio;

            // ── Calculate how many input frames we need ────────────────
            // Start from the current resample position.
            let pos_bits = entry.position.load(Ordering::Relaxed);
            let pos = f64::from_bits(pos_bits);

            // Start frame (floor) and fractional offset.
            let start_frame = pos as u64;
            let frac = pos - (start_frame as f64);

            // End position after processing frame_count mixer frames.
            let end_pos = pos + (frame_count as f64) * ratio;
            let end_frame = (end_pos.ceil() as u64).max(1);

            // Total input frames needed from this stream.
            let need_frames = (end_frame - start_frame) as usize;
            let need_samples = need_frames * stream_channels;

            // ── Read from stream ───────────────────────────────────────
            scratch.stream_buf.resize(need_samples, 0.0);
            let samples_read = entry
                .stream
                .read(&mut scratch.stream_buf, need_frames as u64);

            if samples_read == 0 {
                if entry.stream.is_eof() {
                    // Stream has reached its end — flag for cleanup.
                    entry.state.eof.store(true, Ordering::Release);
                    // Update position for next call.
                    entry
                        .position
                        .store(f64::to_bits(end_pos), Ordering::Relaxed);
                }
                // If not eof, just skip this round (no data available yet).
                continue;
            }

            let read_frames = samples_read / stream_channels;
            let read_samples = read_frames * stream_channels;

            // If we read fewer frames than needed, pad with zeros.
            let effective_frames = read_frames.max(need_frames);
            let effective_samples = effective_frames * stream_channels;
            scratch.stream_buf.resize(effective_samples, 0.0);

            // ── Apply modifier if set (lock-free, ArcSwap) ──────────────
            let volume = f32::from_bits(entry.state.volume.load(Ordering::Relaxed));

            let modifier_guard = entry.state.modifier.load();
            if let Some(ref modifier) = **modifier_guard {
                modifier(&mut scratch.stream_buf[..read_samples]);
            }

            // ── Resample + channel-convert + mix ───────────────────────
            if (ratio - 1.0).abs() < 1e-9 && stream_channels == mixer_channels {
                // ── Fast path: no resampling, no channel conversion ────
                mix_direct(
                    &scratch.stream_buf,
                    &mut scratch.mix_buf,
                    read_samples,
                    mixer_channels,
                    volume,
                );
            } else {
                // ── Generic path: linear interpolation resampling ──────
                mix_resampled(
                    &scratch.stream_buf,
                    &mut scratch.mix_buf,
                    stream_channels,
                    mixer_channels,
                    ratio,
                    frame_count,
                    start_frame,
                    frac,
                    effective_frames,
                    volume,
                );
            }

            // ── Update position ────────────────────────────────────────
            entry
                .position
                .store(f64::to_bits(end_pos), Ordering::Relaxed);
        }

        // ── Copy accumulated mix_buf to output and zero mix_buf ────────
        for (out, &mix) in buffer.iter_mut().zip(scratch.mix_buf.iter()) {
            *out = mix;
        }
        scratch.mix_buf.fill(0.0);

        frame_count
    }

    fn on_device_invalidated(&self) {
        // Endpoint lost (e.g. Bluetooth headset disconnected): notify hosts so
        // they can auto-pause gameplay while the endpoint is being rebuilt.
        self.bump_device_change_epoch();
    }

    fn on_device_format_changed(&self, format: AudioFormat) {
        self.reconfigure_format(format);
        // Device switch typically comes with a format change (speaker → BT
        // headset); count it as a device change for auto-pause hosts.
        self.bump_device_change_epoch();
    }

    fn on_device_recovered(&self) {
        // Replacement endpoint is up again; bump so hosts that pause on
        // invalidation can resume their polling state.
        self.bump_device_change_epoch();
    }
}

/// Fast path: no resampling, no channel conversion. Just apply volume
/// and add into the output.
fn mix_direct(src: &[f32], dst: &mut [f32], src_len: usize, _channels: usize, volume: f32) {
    let samples = src_len.min(dst.len());
    for i in 0..samples {
        dst[i] += src[i] * volume;
    }
}

/// Mix a stream with optional resampling and channel conversion.
///
/// Uses linear interpolation for resampling. Handles channel count
/// mismatch: extra channels are trimmed, missing channels are filled
/// by repeating the available channels.
fn mix_resampled(
    src: &[f32],
    dst: &mut [f32],
    src_channels: usize,
    dst_channels: usize,
    ratio: f64,
    frame_count: usize,
    start_frame: u64,
    frac: f64,
    src_frames: usize,
    volume: f32,
) {
    for out_frame in 0..frame_count {
        // Position in the source stream (absolute, as f64).
        // Include the fractional offset carried over from the previous
        // callback to avoid phase discontinuities at buffer boundaries.
        let src_pos = (start_frame as f64) + frac + (out_frame as f64) * ratio;

        // Convert absolute stream position to a relative index into the
        // src buffer, which holds frames [start_frame, start_frame+src_frames).
        let rel_pos = src_pos - (start_frame as f64);
        let rel_pos_clamped = rel_pos.min((src_frames - 1) as f64).max(0.0);

        let idx0 = rel_pos_clamped as usize;
        let idx1 = (idx0 + 1).min(src_frames - 1);
        let frac = rel_pos_clamped - (idx0 as f64);

        let out_offset = out_frame * dst_channels;

        // For each output channel, interpolate from source channels.
        for ch in 0..dst_channels {
            // Map output channel to source channel:
            // - If src has more channels than dst, use the first dst_channels.
            // - If src has fewer, repeat the last available channel.
            let src_ch = if ch < src_channels {
                ch
            } else {
                src_channels - 1
            };

            let s0 = src[idx0 * src_channels + src_ch];
            let s1 = src[idx1 * src_channels + src_ch];
            let sample = s0 + (s1 - s0) * frac as f32;

            dst[out_offset + ch] += sample * volume;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Mixer;
    use crate::hal::AudioManager;
    use crate::mixer::AudioStream;
    use crate::{AudioError, AudioFormat};

    struct TestStream {
        sample_rate: u32,
    }

    impl AudioStream for TestStream {
        fn read(&self, buffer: &mut [f32], _frame_count: u64) -> usize {
            buffer.fill(0.0);
            buffer.len()
        }

        fn seek(&self, _frame: u64) -> Result<(), AudioError> {
            Ok(())
        }

        fn is_eof(&self) -> bool {
            false
        }

        fn channels(&self) -> u16 {
            2
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
    }

    #[test]
    fn format_and_resample_ratio_publish_together() {
        let mixer = Mixer::new(48_000, 2);
        mixer.add_stream(
            Arc::new(TestStream {
                sample_rate: 48_000,
            }),
            true,
        );

        mixer.on_device_format_changed(AudioFormat::new(44_100, 2));

        let snapshot = mixer.snapshot.load();
        assert_eq!(snapshot.format, AudioFormat::new(44_100, 2));
        assert_eq!(snapshot.entries.len(), 1);
        assert!((snapshot.entries[0].ratio - (48_000.0 / 44_100.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn format_reconfiguration_preserves_stream_position() {
        let mixer = Mixer::new(48_000, 2);
        mixer.add_stream(
            Arc::new(TestStream {
                sample_rate: 48_000,
            }),
            true,
        );

        mixer.snapshot.load().entries[0]
            .position
            .store(123.5_f64.to_bits(), std::sync::atomic::Ordering::Relaxed);
        mixer.on_device_format_changed(AudioFormat::new(44_100, 2));

        let snapshot = mixer.snapshot.load();
        assert_eq!(
            f64::from_bits(
                snapshot.entries[0]
                    .position
                    .load(std::sync::atomic::Ordering::Relaxed)
            ),
            123.5
        );
    }
}
