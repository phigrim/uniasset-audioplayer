//! A retriggerable, polyphonic PCM stream for short sound effects.
//!
//! [`SoundFx`] keeps a fixed pool of voices so repeated [`SoundFx::trigger`]
//! calls can overlap without allocating or locking on the audio thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::{mixer::AudioStream, AudioError};

const INACTIVE: u64 = u64::MAX;
const FAST_LIMIT: f32 = 0.98;
const SUSTAIN_LIMIT: f32 = 0.90;
const ENVELOPE_RELEASE: f32 = 0.995;

struct Voice {
    /// Current PCM frame, or [`INACTIVE`].
    position: AtomicU64,
}

/// A polyphonic, retriggerable PCM sound effect.
///
/// The stream is deliberately never at EOF: it remains in the mixer and
/// produces silence between triggers. `trigger()` is lock-free and can be
/// called concurrently with [`AudioStream::read`]. When every voice is busy,
/// the voice nearest to its end is restarted, which favours the newest impact.
pub struct SoundFx {
    pcm: Arc<[f32]>,
    channels: u16,
    sample_rate: u32,
    frames: u64,
    voices: Box<[Voice]>,
    /// Linked-channel peak follower used by the slow sustain limiter.
    envelope: AtomicU32,
}

impl SoundFx {
    /// Create a retriggerable sound effect from interleaved f32 PCM samples.
    ///
    /// `voice_count` bounds simultaneous playback and all work in `read()`.
    /// A zero voice count, zero channel count, zero sample rate, or partial
    /// final frame is rejected.
    pub fn new(
        pcm: impl Into<Arc<[f32]>>,
        channels: u16,
        sample_rate: u32,
        voice_count: usize,
    ) -> Result<Self, AudioError> {
        if channels == 0 {
            return Err(AudioError::StreamError(
                "SoundFx requires at least one channel".into(),
            ));
        }
        if sample_rate == 0 {
            return Err(AudioError::StreamError(
                "SoundFx requires a non-zero sample rate".into(),
            ));
        }
        if voice_count == 0 {
            return Err(AudioError::StreamError(
                "SoundFx requires at least one voice".into(),
            ));
        }

        let pcm = pcm.into();
        if pcm.is_empty() || pcm.len() % channels as usize != 0 {
            return Err(AudioError::StreamError(
                "SoundFx PCM must contain complete, non-empty frames".into(),
            ));
        }

        let frames = (pcm.len() / channels as usize) as u64;
        let voices = (0..voice_count)
            .map(|_| Voice {
                position: AtomicU64::new(INACTIVE),
            })
            .collect();

        Ok(Self {
            pcm,
            channels,
            sample_rate,
            frames,
            voices,
            envelope: AtomicU32::new(0.0_f32.to_bits()),
        })
    }

    /// Start a new instance of the effect.
    ///
    /// Returns `true` when an idle voice was used. If the pool is full, the
    /// most progressed voice is restarted and `false` is returned.
    pub fn trigger(&self) -> bool {
        for voice in self.voices.iter() {
            if voice
                .position
                .compare_exchange(INACTIVE, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }

        // All voices are active. Reuse the tail-most one so short effects do
        // not suppress newer impacts merely because the pool is momentarily full.
        let mut replacement = &self.voices[0];
        let mut farthest = replacement.position.load(Ordering::Acquire);
        for voice in &self.voices[1..] {
            let position = voice.position.load(Ordering::Acquire);
            if position > farthest {
                replacement = voice;
                farthest = position;
            }
        }
        replacement.position.store(0, Ordering::Release);
        false
    }

    fn apply_limiter(&self, frame: &mut [f32]) {
        let peak = frame
            .iter()
            .fold(0.0_f32, |peak, &sample| peak.max(sample.abs()));
        let fast_gain = if peak > FAST_LIMIT {
            FAST_LIMIT / peak
        } else {
            1.0
        };

        for sample in frame.iter_mut() {
            *sample = (*sample * fast_gain).tanh();
        }

        let clipped_peak = frame
            .iter()
            .fold(0.0_f32, |peak, &sample| peak.max(sample.abs()));
        let previous = f32::from_bits(self.envelope.load(Ordering::Relaxed));
        let envelope = clipped_peak.max(previous * ENVELOPE_RELEASE);
        self.envelope.store(envelope.to_bits(), Ordering::Relaxed);

        if envelope > SUSTAIN_LIMIT {
            let gain = SUSTAIN_LIMIT / envelope;
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }
    }
}

impl AudioStream for SoundFx {
    /// Mix active voices into `buffer` without locking or allocating.
    fn read(&self, buffer: &mut [f32], _frame_count: u64) -> usize {
        buffer.fill(0.0);
        let channels = self.channels as usize;
        let frames = buffer.len() / channels;

        for frame in buffer[..frames * channels].chunks_exact_mut(channels) {
            let mut voice_count = 0usize;

            for voice in self.voices.iter() {
                let position = voice.position.load(Ordering::Acquire);
                if position == INACTIVE || position >= self.frames {
                    continue;
                }

                let source = &self.pcm[position as usize * channels..][..channels];
                for (output, &input) in frame.iter_mut().zip(source) {
                    *output += input;
                }
                voice_count += 1;

                let next = if position + 1 == self.frames {
                    INACTIVE
                } else {
                    position + 1
                };
                let _ = voice.position.compare_exchange(
                    position,
                    next,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
            }

            if voice_count != 0 {
                let gain = 1.0 / (voice_count as f32).sqrt();
                for sample in frame.iter_mut() {
                    *sample *= gain;
                }
            }
            // Also process silence so the slow limiter releases in real time.
            self.apply_limiter(frame);
        }

        buffer.len()
    }

    /// Move every active voice to the same PCM frame.
    fn seek(&self, frame: u64) -> Result<(), AudioError> {
        let position = if frame < self.frames { frame } else { INACTIVE };
        for voice in self.voices.iter() {
            if voice.position.load(Ordering::Acquire) != INACTIVE {
                voice.position.store(position, Ordering::Release);
            }
        }
        Ok(())
    }

    /// Sound effects remain available for later `trigger()` calls.
    fn is_eof(&self) -> bool {
        false
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_voices_use_root_n_gain() {
        let sound = SoundFx::new(vec![0.1], 1, 48_000, 2).unwrap();
        assert!(sound.trigger());
        assert!(sound.trigger());

        let mut output = [0.0];
        assert_eq!(sound.read(&mut output, 1), 1);
        assert!((output[0] / 0.1 - 2.0_f32.sqrt()).abs() < 0.02);
    }

    #[test]
    fn retriggers_after_a_voice_finishes() {
        let sound = SoundFx::new(vec![0.25], 1, 48_000, 1).unwrap();
        assert!(sound.trigger());

        let mut output = [0.0];
        sound.read(&mut output, 1);
        sound.read(&mut output, 1);

        assert_eq!(output, [0.0]);
        assert!(!sound.is_eof());
        assert!(sound.trigger());
    }

    #[test]
    fn limiter_bounds_large_transients() {
        let sound = SoundFx::new(vec![100.0], 1, 48_000, 1).unwrap();
        sound.trigger();

        let mut output = [0.0];
        sound.read(&mut output, 1);

        assert!(output[0].is_finite());
        assert!(output[0].abs() <= SUSTAIN_LIMIT);
    }
}
