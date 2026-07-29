using System;
using System.ComponentModel;

namespace Uniasset.AudioPlayer.Unsafe
{
    /// <summary>
    /// Factory for creating native buffered audio streams.
    /// Wraps the <c>UAP_BufferedAudioStream_*</c> C functions.
    /// </summary>
    /// <remarks>
    /// The returned handle is an <see cref="UnsafeInternalAudioStream"/>
    /// (the buffered wrapper is an <c>AudioStreamWrapper</c> internally).
    /// Destroy with <see cref="UnsafeInternalAudioStream.Destroy"/>.
    /// </remarks>
    [EditorBrowsable(EditorBrowsableState.Never)]
    public static unsafe class UnsafeBufferedAudioStream
    {
        /// <summary>
        /// Wrap a native audio stream handle in a buffered stream with the
        /// requested buffer duration.
        /// </summary>
        public static UnsafeInternalAudioStream Create(void* innerHandle, TimeSpan bufferDuration)
        {
            var handle = Interop.UAP_BufferedAudioStream_Create(
                innerHandle, ToMilliseconds(bufferDuration));
            NativeException.ThrowIfNeeded();
            if (handle == null)
                throw new NativeException(
                    "Failed to create BufferedAudioStream: native returned null");
            return new UnsafeInternalAudioStream(handle);
        }

        /// <summary>
        /// Wrap a native audio stream in a buffered stream with the requested
        /// buffer duration.
        /// </summary>
        public static UnsafeInternalAudioStream CreateFromNative(
            ref NativeAudioStream stream,
            TimeSpan bufferDuration)
        {
            fixed (NativeAudioStream* streamPtr = &stream)
            {
                var handle = Interop.UAP_BufferedAudioStream_CreateFromNative(
                    streamPtr, ToMilliseconds(bufferDuration));
                NativeException.ThrowIfNeeded();
                if (handle == null)
                    throw new NativeException(
                        "Failed to create BufferedAudioStream: native returned null");
                return new UnsafeInternalAudioStream(handle);
            }
        }

        private static uint ToMilliseconds(TimeSpan duration)
        {
            if (duration <= TimeSpan.Zero)
                throw new ArgumentOutOfRangeException(nameof(duration),
                    "Buffer duration must be positive.");
            if (duration.TotalMilliseconds > uint.MaxValue)
                throw new ArgumentOutOfRangeException(nameof(duration),
                    "Buffer duration is too large.");
            return checked((uint)Math.Ceiling(duration.TotalMilliseconds));
        }
    }
}
