package org.zerolink.media

import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer

class H265ScreenEncoder(
    private val width: Int = 1080,
    private val height: Int = 2400,
    private val dpi: Int = 400,
    private val bitrateBps: Int = 6_000_000,
    private val maxFps: Int = 60,
    private val onNalUnitEncoded: (nalData: ByteArray, ptsUs: Long, isKeyframe: Boolean) -> Unit
) {

    companion object {
        private const val TAG = "ZeroLink.H265Encoder"
        private const val MIME_TYPE = MediaFormat.MIMETYPE_VIDEO_HEVC
        private const val I_FRAME_INTERVAL_SEC = 2
    }

    private var codec: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var handlerThread: HandlerThread? = null
    private var handler: Handler? = null

    @Volatile
    var isRunning: Boolean = false
        private set

    fun start(mediaProjection: MediaProjection) {
        if (isRunning) return

        handlerThread = HandlerThread("ZeroLinkEncoderThread").apply { start() }
        handler = Handler(handlerThread!!.looper)

        try {
            val format = MediaFormat.createVideoFormat(MIME_TYPE, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, bitrateBps)
                setInteger(MediaFormat.KEY_FRAME_RATE, maxFps)
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, I_FRAME_INTERVAL_SEC)
                // Real-time low-latency mode
                setInteger(MediaFormat.KEY_LATENCY, 0)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                // Dynamic frame rate throttling: drop duplicate/static frames
                setFloat(MediaFormat.KEY_MAX_FPS_TO_ENCODER, maxFps.toFloat())
            }

            val encoder = MediaCodec.createEncoderByType(MIME_TYPE)
            encoder.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                    // Surface input is managed directly by VirtualDisplay
                }

                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    if (!isRunning) return

                    val outputBuffer = codec.getOutputBuffer(index) ?: return
                    if (info.size > 0) {
                        outputBuffer.position(info.offset)
                        outputBuffer.limit(info.offset + info.size)

                        val nalData = ByteArray(info.size)
                        outputBuffer.get(nalData)

                        val isKeyframe = (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                        onNalUnitEncoded(nalData, info.presentationTimeUs, isKeyframe)
                    }
                    codec.releaseOutputBuffer(index, false)
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    Log.e(TAG, "MediaCodec encoder error: ${e.message}", e)
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                    Log.i(TAG, "MediaCodec output format changed: $format")
                }
            }, handler)

            encoder.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = encoder.createInputSurface()
            encoder.start()
            codec = encoder

            virtualDisplay = mediaProjection.createVirtualDisplay(
                "ZeroLinkScreenCast",
                width,
                height,
                dpi,
                DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                inputSurface,
                null,
                handler
            )

            isRunning = true
            Log.i(TAG, "H.265 hardware encoder started (${width}x${height} @ ${maxFps}fps)")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start H.265 screen encoder", e)
            stop()
        }
    }

    /**
     * Request an instantaneous IDR/Keyframe from the hardware encoder upon client request.
     */
    fun requestKeyframe() {
        val params = Bundle().apply {
            putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0)
        }
        codec?.setParameters(params)
    }

    fun stop() {
        isRunning = false
        try {
            virtualDisplay?.release()
            inputSurface?.release()
            codec?.stop()
            codec?.release()
        } catch (e: Exception) {
            Log.w(TAG, "Exception during encoder release: ${e.message}")
        }
        virtualDisplay = null
        inputSurface = null
        codec = null

        handlerThread?.quitSafely()
        handlerThread = null
        handler = null
        Log.i(TAG, "H.265 hardware encoder stopped")
    }
}
