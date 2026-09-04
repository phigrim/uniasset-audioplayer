mod buffer;
pub use buffer::*;

mod worker;
pub use worker::AudioBuffer;

/// Fraction of the ring buffer that must be filled before the worker stops
/// topping up.  0.6 = 60 %.
pub(crate) const BUFFER_WATERMARK: f32 = 0.6;
