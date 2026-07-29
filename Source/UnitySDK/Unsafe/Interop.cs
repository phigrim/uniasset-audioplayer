using System.Runtime.InteropServices;

namespace Uniasset.AudioPlayer.Unsafe
{
    public static unsafe partial class Interop
    {
        // ==================================================================
        // Error (2 functions)
        // ==================================================================

        /// <summary>Returns true if the last FFI call on this thread produced an error.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("bool")]
        public static extern byte UAP_HasError();

        /// <summary>
        /// Returns a pointer to a null-terminated error message string, or null if
        /// there is no error. The pointer is valid until the next FFI call on this
        /// thread (most FFI functions clear the error slot on entry). Calling this
        /// function does <b>not</b> clear the error.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("const char *")]
        public static extern byte* UAP_GetError();

        // ==================================================================
        // Player Lifecycle (2 functions)
        // ==================================================================

        /// <summary>
        /// Create a new AudioPlayer and open the platform audio device.
        /// Returns a handle on success, or null on failure.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void* UAP_AudioPlayer_New();

        /// <summary>
        /// Destroy an AudioPlayer. Drops the C caller's reference.
        /// When the last reference is dropped, playback stops and the audio device
        /// is released.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_AudioPlayer_Destroy(void* handle);

        // ==================================================================
        // Device Query (1 function)
        // ==================================================================

        /// <summary>
        /// Query the audio format (sample rate / channel count) of the output device.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_AudioPlayer_Format(
            void* handle,
            [NativeTypeName("uint32_t *")] uint* out_sample_rate,
            [NativeTypeName("uint16_t *")] ushort* out_channels);

        // ==================================================================
        // Stream Management (3 functions)
        // ==================================================================

        /// <summary>
        /// Add an audio stream backed by C function pointers to the player.
        /// <c>stream</c> must point to a valid <see cref="NativeAudioStream"/>
        /// struct that the caller keeps alive.
        /// If <c>playImmediate</c> is non-zero, the stream begins playing
        /// immediately; otherwise it is added in a paused state.
        /// Returns a <see cref="UnsafePlayHandle"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void* UAP_AudioPlayer_AddNativeStream(
            void* handle,
            void* stream,
            [NativeTypeName("uint8_t")] byte playImmediate);

        /// <summary>
        /// Add an audio stream to the player.
        /// <c>stream</c> must be a valid native handle encoding a
        /// <c>Box&lt;Arc&lt;dyn AudioStream&gt;&gt;</c>. The handle is <b>not</b>
        /// consumed.
        /// If <c>playImmediate</c> is non-zero, the stream begins playing
        /// immediately; otherwise it is added in a paused state.
        /// Returns a <see cref="UnsafePlayHandle"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void* UAP_AudioPlayer_AddStream(
            void* handle,
            void* stream,
            [NativeTypeName("uint8_t")] byte playImmediate);

        /// <summary>
        /// Return the number of currently active streams.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("uint32_t")]
        public static extern uint UAP_AudioPlayer_StreamCount(void* handle);

        // ==================================================================
        // Buffered Stream (2 functions)
        // ==================================================================

        /// <summary>
        /// Wrap a native audio stream in a buffered stream for smooth playback.
        /// <c>stream</c> must be a valid native handle. The handle is <b>not</b>
        /// consumed. Returns a new native handle encoding the buffered stream,
        /// or null on failure.
        /// Destroy the returned handle with <see cref="UAP_InternalAudioStream_Destroy"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void* UAP_BufferedAudioStream_Create(
            void* stream,
            [NativeTypeName("uint32_t")] uint bufferDurationMs);

        /// <summary>
        /// Wrap a native audio stream (callbacks struct) in a buffered stream.
        /// <c>stream</c> must point to a valid <see cref="NativeAudioStream"/>.
        /// The struct is copied — the caller retains ownership.
        /// Returns a new native handle, or null on failure.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void* UAP_BufferedAudioStream_CreateFromNative(
            [NativeTypeName("const NativeAudioStream *")] NativeAudioStream* stream,
            [NativeTypeName("uint32_t")] uint bufferDurationMs);

        // ==================================================================
        // Internal Audio Stream (6 functions)
        // ==================================================================

        /// <summary>
        /// Read interleaved f32 samples from an internal audio stream.
        /// <c>buffer</c> must be at least <c>frameCount * channels</c> samples.
        /// Returns the number of <b>samples</b> written, or 0 at EOF.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern ulong UAP_InternalAudioStream_Read(
            void* handle, float* buffer, ulong frameCount);

        /// <summary>
        /// Seek to the given frame position. Returns non-zero on success.
        /// On failure, the error is reported via <see cref="UAP_HasError"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("bool")]
        public static extern byte UAP_InternalAudioStream_Seek(void* handle, ulong frame);

        /// <summary>Returns non-zero if the stream has reached its end.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("bool")]
        public static extern byte UAP_InternalAudioStream_IsEof(void* handle);

        /// <summary>Return the number of channels (1 = mono, 2 = stereo).</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("uint16_t")]
        public static extern ushort UAP_InternalAudioStream_Channels(void* handle);

        /// <summary>Return the sample rate in Hz (e.g., 44100, 48000).</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("uint32_t")]
        public static extern uint UAP_InternalAudioStream_SampleRate(void* handle);

        /// <summary>
        /// Destroy an internal audio stream handle.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_InternalAudioStream_Destroy(void* handle);

        // ==================================================================
        // Device Control (2 functions)
        // ==================================================================

        /// <summary>
        /// Pause playback on the audio device.
        /// On failure, the error is reported via <see cref="UAP_HasError"/> /
        /// <see cref="UAP_GetError"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_AudioPlayer_Pause(void* handle);

        /// <summary>
        /// Resume playback on the audio device.
        /// On failure, the error is reported via <see cref="UAP_HasError"/> /
        /// <see cref="UAP_GetError"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_AudioPlayer_Resume(void* handle);

        // ==================================================================
        // PlayHandle (9 functions)
        // ==================================================================

        /// <summary>
        /// Destroy a PlayHandle. Drops the C caller's reference.
        /// The mixer holds its own references independently; the underlying
        /// stream continues playing until it reaches EOF or
        /// <see cref="UAP_PlayHandle_Stop"/> is called.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_Destroy(void* handle);

        /// <summary>Pause playback for this stream. No-op if the stream is no longer alive.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_Pause(void* handle);

        /// <summary>Resume playback for this stream. No-op if the stream is no longer alive.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_Resume(void* handle);

        /// <summary>Returns true if the stream is currently paused.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("bool")]
        public static extern byte UAP_PlayHandle_IsPaused(void* handle);

        /// <summary>Returns true if the stream is still active in the mixer.</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [return: NativeTypeName("bool")]
        public static extern byte UAP_PlayHandle_IsAlive(void* handle);

        /// <summary>
        /// Signal the stream to stop. The mixer will remove it from the
        /// active stream set once it observes the stop signal.
        /// No-op if the stream is no longer alive.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_Stop(void* handle);

        /// <summary>Set the volume for this stream. <c>volume</c> is clamped to [0.0, 1.0].</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_SetVolume(void* handle, float volume);

        /// <summary>Return the current volume in [0.0, 1.0].</summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern float UAP_PlayHandle_GetVolume(void* handle);

        /// <summary>
        /// Seek the stream to the given frame position.
        /// On failure, the error is reported via <see cref="UAP_HasError"/> /
        /// <see cref="UAP_GetError"/>.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_Seek(void* handle, ulong frame);

        /// <summary>
        /// Install a pre-mix modifier callback for this stream.
        /// <c>modifier</c> points to a <see cref="NativeModifier"/> containing
        /// the callback function pointer and opaque user data. The callback is
        /// called on the audio thread with the interleaved PCM buffer for this
        /// stream before it is mixed into the output.
        /// The callback must be wait-free. <c>user_data</c> must remain valid
        /// for as long as the modifier is installed.
        /// </summary>
        [DllImport(LibraryName, CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void UAP_PlayHandle_SetModifier(
            void* handle,
            [NativeTypeName("const NativeModifier *")] NativeModifier* modifier);
    }
}
