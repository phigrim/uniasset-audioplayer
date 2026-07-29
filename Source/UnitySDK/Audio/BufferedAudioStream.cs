using System;
using Uniasset.AudioPlayer.Unsafe;

namespace Uniasset.AudioPlayer
{
    /// <summary>
    /// An <see cref="InternalAudioStream"/> that wraps an inner stream in a
    /// native buffered stream for smooth playback.
    /// </summary>
    /// <remarks>
    /// If <paramref name="stream"/> is an <see cref="InternalAudioStream"/>,
    /// the fast path is used (passes the native handle directly to
    /// <c>UAP_BufferedAudioStream_Create</c>). Otherwise a native callback
    /// bridge is created via <c>UAP_BufferedAudioStream_CreateFromNative</c>
    /// and kept alive for the stream's lifetime.
    /// </remarks>
    public sealed class BufferedAudioStream : InternalAudioStream
    {
        /// <summary>Default amount of audio retained by a buffered stream.</summary>
        public static readonly TimeSpan DefaultBufferDuration = TimeSpan.FromSeconds(12);

        private StreamBinding? _innerBinding;

        /// <summary>
        /// Wrap <paramref name="stream"/> in a native buffered stream.
        /// </summary>
        /// <param name="stream">
        /// The audio stream to buffer. Must not be null.
        /// </param>
        /// <param name="bufferDuration">
        /// Amount of audio retained by the native buffer. Defaults to 12 seconds.
        /// </param>
        /// <exception cref="NativeException">
        /// Thrown if the native buffered stream could not be created.
        /// </exception>
        public unsafe BufferedAudioStream(IAudioStream stream, TimeSpan? bufferDuration = null)
        {
            var duration = bufferDuration ?? DefaultBufferDuration;
            if (stream is InternalAudioStream internalStream)
            {
                // Fast path: use the existing native handle.
                SetHandle(UnsafeBufferedAudioStream.Create(
                    internalStream.UnsafeHandle.Instance, duration));
            }
            else
            {
                // Fallback: create a NativeAudioStream callback bridge.
                var binding = AudioStreamFactory.CreateBinding(stream);
                SetHandle(UnsafeBufferedAudioStream.CreateFromNative(
                    ref binding.NativeStream, duration));
                // Keep the binding alive — the native side copied the struct,
                // but the GCHandle must remain for the callbacks to work.
                _innerBinding = binding;
            }
        }

        /// <inheritdoc />
        protected override void DisposeCore()
        {
            if (_innerBinding.HasValue)
            {
                _innerBinding.Value.Free();
                _innerBinding = null;
            }
        }
    }
}
