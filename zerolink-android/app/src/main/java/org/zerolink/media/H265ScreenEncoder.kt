package org.zerolink.media

import android.content.Context
import android.graphics.ImageFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.Image
import android.media.ImageReader
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.util.Log
import android.util.DisplayMetrics
import android.view.Display
import android.view.Surface
import java.util.ArrayDeque

class H265ScreenEncoder(
    private val context: Context,
    private val width: Int = 1080,
    private val height: Int = 2400,
    private val dpi: Int = 400,
    private val bitrateBps: Int = 6_000_000,
    private val maxFps: Int = 60,
    private val onNalUnitEncoded: (nalData: ByteArray, ptsUs: Long, isKeyframe: Boolean, width: Int, height: Int) -> Unit,
    private val onStarted: (Boolean) -> Unit = {},
    private val onStopped: () -> Unit = {}
) {

    companion object {
        private const val TAG = "ZeroLink.H265Encoder"
        private const val MIME_TYPE = MediaFormat.MIMETYPE_VIDEO_HEVC
        private const val I_FRAME_INTERVAL_SEC = 2
    }

    private var codec: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var imageReader: ImageReader? = null
    private var virtualDisplay: VirtualDisplay? = null
    private var handlerThread: HandlerThread? = null
    private var handler: Handler? = null
    private var mediaProjection: MediaProjection? = null
    private var selectedCodecName: String? = null
    private var projectionCallback: MediaProjection.Callback? = null
    private var displayManager: DisplayManager? = null
    private var displayListener: DisplayManager.DisplayListener? = null
    @Volatile private var activeWidth = width
    @Volatile private var activeHeight = height
    @Volatile private var activeFrameRate = maxFps
    @Volatile private var activeBitrateBps = bitrateBps
    @Volatile private var configured = false
    @Volatile private var activeInputMode = InputMode.SURFACE
    @Volatile private var startRequested = false
    @Volatile private var stopScheduled = false
    @Volatile private var cleanupFinished = true
    private val availableInputBuffers = ArrayDeque<Int>()
    private var pendingYuvFrame: Image? = null

    private enum class InputMode(val colorFormat: Int) {
        SURFACE(MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface),
        YUV(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible)
    }

    private data class EncoderConfig(
        val codecName: String,
        val inputMode: InputMode,
        val width: Int,
        val height: Int,
        val frameRate: Int,
        val bitrateBps: Int
    )

    @Volatile
    var isRunning: Boolean = false
        private set

    fun start(mediaProjection: MediaProjection) {
        if (isRunning || startRequested) return
        startRequested = true
        stopScheduled = false
        cleanupFinished = false
        this.mediaProjection = mediaProjection
        handlerThread = HandlerThread("ZeroLinkEncoderThread").apply { start() }
        handler = Handler(handlerThread!!.looper)
        val callbackHandler = handler ?: error("Encoder callback handler is unavailable")
        if (!callbackHandler.post { startInternal() }) {
            Log.e(TAG, "Could not schedule HEVC encoder startup")
            onStarted(false)
            stop()
        }
    }

    private fun startInternal() {
        if (!startRequested) {
            stopInternal()
            return
        }
        try {
            val encoderConfig = findEncoderConfig(width, height)
            if (!startRequested) return
            if (encoderConfig == null) {
                Log.e(TAG, "No compatible HEVC encoder supports ${width}x${height} at up to ${maxFps}fps")
                startRequested = false
                onStarted(false)
                stopInternal()
                return
            }
            selectedCodecName = encoderConfig.codecName
            activeWidth = encoderConfig.width
            activeHeight = encoderConfig.height
            activeInputMode = encoderConfig.inputMode
            activeFrameRate = encoderConfig.frameRate
            activeBitrateBps = encoderConfig.bitrateBps
            if (activeWidth != width || activeHeight != height || activeFrameRate != maxFps) {
                Log.i(
                    TAG,
                    "Using ${activeWidth}x${activeHeight} at ${activeFrameRate}fps on ${encoderConfig.codecName} " +
                        "(requested ${width}x${height} at ${maxFps}fps)"
                )
            } else {
                Log.i(TAG, "Selected ${encoderConfig.codecName} for ${activeWidth}x${activeHeight} at ${activeFrameRate}fps")
            }
            Log.i(TAG, "HEVC input mode: ${activeInputMode.name}")

            val callbackHandler = handler ?: error("Encoder callback handler is unavailable")
            val callback = object : MediaProjection.Callback() {
                override fun onStop() {
                    Log.i(TAG, "MediaProjection stopped; releasing the screen encoder")
                    stop()
                }

                override fun onCapturedContentResize(width: Int, height: Int) {
                    if (Build.VERSION.SDK_INT >= 34 && width > 0 && height > 0) {
                        callbackHandler.post { resizeCapture(width, height) }
                    }
                }
            }
            projectionCallback = callback
            val projection = mediaProjection ?: error("MediaProjection is unavailable")
            projection.registerCallback(callback, callbackHandler)
            val manager = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            val listener = object : DisplayManager.DisplayListener {
                override fun onDisplayAdded(displayId: Int) = Unit
                override fun onDisplayRemoved(displayId: Int) = Unit

                override fun onDisplayChanged(displayId: Int) {
                    if (displayId == Display.DEFAULT_DISPLAY && Build.VERSION.SDK_INT < 34) {
                        callbackHandler.post {
                            readDefaultDisplaySize()?.let { (newWidth, newHeight) ->
                                resizeCapture(newWidth, newHeight)
                            }
                        }
                    }
                }
            }
            displayManager = manager
            displayListener = listener
            manager.registerDisplayListener(listener, callbackHandler)
            if (!startRequested) return
            isRunning = true
            if (!createEncoderAndAttachDisplay()) {
                error("Could not create the HEVC encoder or virtual display")
            }
            configured = true
            startRequested = false
            Log.i(TAG, "H.265 encoder started (${activeWidth}x${activeHeight} @ ${activeFrameRate}fps, ${activeInputMode.name} input)")
            onStarted(true)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start H.265 screen encoder", e)
            isRunning = false
            startRequested = false
            onStarted(false)
            stopInternal()
        }
    }

    private fun createEncoderAndAttachDisplay(): Boolean {
        var encoder: MediaCodec? = null
        var surface: Surface? = null
        try {
            val format = MediaFormat.createVideoFormat(MIME_TYPE, activeWidth, activeHeight).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, activeInputMode.colorFormat)
                setInteger(MediaFormat.KEY_BIT_RATE, activeBitrateBps)
                setInteger(MediaFormat.KEY_FRAME_RATE, activeFrameRate)
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, I_FRAME_INTERVAL_SEC)
            }

            val codecName = selectedCodecName ?: error("No HEVC encoder was selected")
            val createdEncoder = MediaCodec.createByCodecName(codecName)
            encoder = createdEncoder
            val callbackCodec = createdEncoder
            createdEncoder.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                    if (activeInputMode != InputMode.YUV || this@H265ScreenEncoder.codec !== callbackCodec) return
                    availableInputBuffers.addLast(index)
                    pumpPendingYuvFrame()
                }

                override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    try {
                        if (!isRunning || this@H265ScreenEncoder.codec !== callbackCodec || info.size <= 0) return
                        val outputBuffer = codec.getOutputBuffer(index) ?: return
                        outputBuffer.position(info.offset)
                        outputBuffer.limit(info.offset + info.size)
                        val nalData = ByteArray(info.size)
                        outputBuffer.get(nalData)
                        val isKeyframe = (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                        onNalUnitEncoded(nalData, info.presentationTimeUs, isKeyframe, activeWidth, activeHeight)
                    } catch (e: Exception) {
                        Log.e(TAG, "Failed to forward an encoded HEVC buffer", e)
                    } finally {
                        try {
                            codec.releaseOutputBuffer(index, false)
                        } catch (e: Exception) {
                            Log.w(TAG, "Could not release encoded HEVC buffer: ${e.message}")
                        }
                    }
                }

                override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                    Log.e(TAG, "MediaCodec encoder error: ${e.message}", e)
                }

                override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                    Log.i(TAG, "MediaCodec output format changed: $format")
                }
            }, handler)

            createdEncoder.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            this.codec = createdEncoder
            val captureSurface = when (activeInputMode) {
                InputMode.SURFACE -> {
                    surface = createdEncoder.createInputSurface()
                    inputSurface = surface
                    surface
                }
                InputMode.YUV -> {
                    val reader = ImageReader.newInstance(
                        activeWidth,
                        activeHeight,
                        ImageFormat.YUV_420_888,
                        3
                    )
                    imageReader = reader
                    reader.setOnImageAvailableListener({ source -> onYuvFrameAvailable(source) }, handler)
                    reader.surface
                }
            } ?: error("Could not create an encoder input surface")
            createdEncoder.start()

            val display = virtualDisplay
            if (display != null) {
                display.resize(activeWidth, activeHeight, dpi)
                display.setSurface(captureSurface)
            } else {
                val projection = mediaProjection ?: error("MediaProjection is unavailable")
                virtualDisplay = projection.createVirtualDisplay(
                    "ZeroLinkScreenCast",
                    activeWidth,
                    activeHeight,
                    dpi,
                    DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                    captureSurface,
                    null,
                    handler
                )
            }
            return true
        } catch (e: Exception) {
            Log.e(
                TAG,
                "Could not configure HEVC encoder ${selectedCodecName ?: "unknown"} " +
                    "at ${activeWidth}x${activeHeight} @ ${activeFrameRate}fps",
                e
            )
            try { encoder?.stop() } catch (_: Exception) { }
            try { encoder?.release() } catch (_: Exception) { }
            try { surface?.release() } catch (_: Exception) { }
            val reader = imageReader
            imageReader = null
            try { reader?.setOnImageAvailableListener(null, null) } catch (_: Exception) { }
            try { reader?.close() } catch (_: Exception) { }
            if (this.codec === encoder) this.codec = null
            if (inputSurface === surface) inputSurface = null
            return false
        }
    }

    private fun findEncoderConfig(requestedWidth: Int, requestedHeight: Int): EncoderConfig? {
        val hevcEncoders = try {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .filter { info ->
                    info.isEncoder && info.supportedTypes.any { type -> type.equals(MIME_TYPE, ignoreCase = true) }
                }
        } catch (e: Exception) {
            Log.e(TAG, "Could not enumerate HEVC encoders", e)
            return null
        }

        if (hevcEncoders.isEmpty()) {
            Log.w(TAG, "The device exposes no regular HEVC encoder")
            return null
        }

        val scaleFactors = listOf(1.0, 0.875, 0.75, 0.625, 0.5, 0.375, 0.25)
        val frameRates = listOf(maxFps, minOf(maxFps, 24), minOf(maxFps, 15)).distinct()
        val sizes = (listOf(requestedWidth to requestedHeight) + scaleFactors.map { scale ->
            alignDown((requestedWidth * scale).toInt(), 2) to
                alignDown((requestedHeight * scale).toInt(), 2)
        }).distinct()
        // Prefer direct virtual-display-to-codec capture; try flexible YUV only when
        // no compatible surface encoder is exposed (common on emulator codecs).
        for (inputMode in InputMode.entries) {
            for (info in hevcEncoders) {
                val capabilities = try {
                    info.getCapabilitiesForType(MIME_TYPE)
                } catch (e: Exception) {
                    Log.w(TAG, "Could not read HEVC capabilities for ${info.name}: ${e.message}")
                    continue
                }
                val videoCaps = capabilities.videoCapabilities
                if (inputMode == InputMode.SURFACE) {
                    Log.i(
                        TAG,
                        "HEVC ${info.name}: colorFormats=${capabilities.colorFormats.joinToString { "0x${it.toString(16)}" }}, " +
                            "widths=${videoCaps?.supportedWidths}, heights=${videoCaps?.supportedHeights}, " +
                            "frameRates=${videoCaps?.supportedFrameRates}"
                    )
                }
                if (!capabilities.colorFormats.contains(inputMode.colorFormat)) {
                    Log.i(TAG, "HEVC encoder ${info.name} does not support ${inputMode.name} input")
                    continue
                }
                val videoCapabilities = videoCaps ?: continue
                val widthAlignment = videoCapabilities.widthAlignment.coerceAtLeast(1)
                val heightAlignment = videoCapabilities.heightAlignment.coerceAtLeast(1)

                for ((unadjustedWidth, unadjustedHeight) in sizes) {
                    val candidateWidth = alignDown(unadjustedWidth, widthAlignment)
                    val candidateHeight = alignDown(unadjustedHeight, heightAlignment)
                    if (candidateWidth <= 0 || candidateHeight <= 0) continue

                    for (frameRate in frameRates) {
                        val supported = try {
                            videoCapabilities.areSizeAndRateSupported(
                                candidateWidth,
                                candidateHeight,
                                frameRate.toDouble()
                            )
                        } catch (_: IllegalArgumentException) {
                            false
                        }
                        if (supported) {
                            val bitrateRange = videoCapabilities.bitrateRange
                            return EncoderConfig(
                                info.name,
                                inputMode,
                                candidateWidth,
                                candidateHeight,
                                frameRate,
                                bitrateBps.coerceIn(bitrateRange.lower, bitrateRange.upper)
                            )
                        }
                    }
                }
            }
        }

        Log.w(TAG, "No HEVC surface or flexible-YUV encoder supports ${requestedWidth}x${requestedHeight} or the tested lower capture sizes")
        return null
    }

    private fun onYuvFrameAvailable(reader: ImageReader) {
        val frame = try {
            reader.acquireLatestImage()
        } catch (e: Exception) {
            Log.w(TAG, "Could not acquire a screen-capture frame: ${e.message}")
            null
        } ?: return

        pendingYuvFrame?.close()
        pendingYuvFrame = frame
        pumpPendingYuvFrame()
    }

    private fun pumpPendingYuvFrame() {
        val currentCodec = codec ?: return
        while (pendingYuvFrame != null && availableInputBuffers.isNotEmpty()) {
            val frame = pendingYuvFrame ?: return
            pendingYuvFrame = null
            queueYuvFrame(currentCodec, availableInputBuffers.removeFirst(), frame)
        }
    }

    private fun queueYuvFrame(currentCodec: MediaCodec, inputIndex: Int, frame: Image) {
        val presentationTimeUs = frame.timestamp / 1_000L
        var queued = false
        try {
            val inputImage = currentCodec.getInputImage(inputIndex)
                ?: error("HEVC codec returned no writable input image")
            require(inputImage.planes.size >= 3 && frame.planes.size >= 3) {
                "Flexible YUV capture requires three image planes"
            }
            val planeWidths = intArrayOf(activeWidth, activeWidth / 2, activeWidth / 2)
            val planeHeights = intArrayOf(activeHeight, activeHeight / 2, activeHeight / 2)
            for (planeIndex in 0..2) {
                copyPlane(
                    frame.planes[planeIndex],
                    inputImage.planes[planeIndex],
                    planeWidths[planeIndex],
                    planeHeights[planeIndex]
                )
            }
            currentCodec.queueInputBuffer(
                inputIndex,
                0,
                activeWidth * activeHeight * 3 / 2,
                presentationTimeUs,
                0
            )
            queued = true
        } catch (e: Exception) {
            Log.w(TAG, "Could not queue a flexible-YUV screen frame: ${e.message}")
        } finally {
            frame.close()
            if (!queued) {
                try {
                    currentCodec.queueInputBuffer(inputIndex, 0, 0, presentationTimeUs, 0)
                } catch (_: Exception) {
                    // The codec may already have reclaimed the slot after an input error.
                }
            }
        }
    }

    private fun copyPlane(source: Image.Plane, destination: Image.Plane, width: Int, height: Int) {
        val sourceBuffer = source.buffer
        val destinationBuffer = destination.buffer
        val sourceBase = sourceBuffer.position()
        val destinationBase = destinationBuffer.position()
        for (row in 0 until height) {
            val sourceRow = sourceBase + row * source.rowStride
            val destinationRow = destinationBase + row * destination.rowStride
            for (column in 0 until width) {
                destinationBuffer.put(
                    destinationRow + column * destination.pixelStride,
                    sourceBuffer.get(sourceRow + column * source.pixelStride)
                )
            }
        }
    }

    private fun alignDown(value: Int, alignment: Int): Int = value / alignment * alignment

    private fun resizeCapture(newWidth: Int, newHeight: Int) {
        if (!isRunning || !configured || (newWidth == activeWidth && newHeight == activeHeight)) return
        val display = virtualDisplay ?: return
        Log.i(TAG, "Captured content resized to ${newWidth}x${newHeight}; restarting HEVC encoder")
        configured = false
        try {
            display.setSurface(null)
        } catch (e: Exception) {
            Log.w(TAG, "Could not detach the old encoder surface: ${e.message}")
        }
        releaseCodecOnly()
        val encoderConfig = findEncoderConfig(newWidth, newHeight)
        if (encoderConfig == null) {
            Log.e(TAG, "No HEVC encoder supports the resized capture at ${newWidth}x${newHeight}")
            stop()
            return
        }
        selectedCodecName = encoderConfig.codecName
        activeWidth = encoderConfig.width
        activeHeight = encoderConfig.height
        activeInputMode = encoderConfig.inputMode
        activeFrameRate = encoderConfig.frameRate
        activeBitrateBps = encoderConfig.bitrateBps
        if (createEncoderAndAttachDisplay()) {
            configured = true
            requestKeyframe()
        } else {
            Log.e(TAG, "Screen mirroring stopped because the resized HEVC encoder could not start")
            stop()
        }
    }

    @Suppress("DEPRECATION")
    private fun readDefaultDisplaySize(): Pair<Int, Int>? {
        val display = displayManager?.getDisplay(Display.DEFAULT_DISPLAY) ?: return null
        val metrics = DisplayMetrics()
        display.getRealMetrics(metrics)
        if (metrics.widthPixels <= 0 || metrics.heightPixels <= 0) return null
        return metrics.widthPixels to metrics.heightPixels
    }

    private fun releaseCodecOnly() {
        val reader = imageReader
        imageReader = null
        try { reader?.setOnImageAvailableListener(null, null) } catch (e: Exception) { Log.w(TAG, "Could not detach YUV frame listener: ${e.message}") }
        pendingYuvFrame?.close()
        pendingYuvFrame = null
        availableInputBuffers.clear()
        try { reader?.close() } catch (e: Exception) { Log.w(TAG, "Could not close YUV image reader: ${e.message}") }

        val oldCodec = codec
        codec = null
        try { oldCodec?.stop() } catch (e: Exception) { Log.w(TAG, "Could not stop old HEVC encoder: ${e.message}") }
        try { oldCodec?.release() } catch (e: Exception) { Log.w(TAG, "Could not release old HEVC encoder: ${e.message}") }
        val oldSurface = inputSurface
        inputSurface = null
        try { oldSurface?.release() } catch (e: Exception) { Log.w(TAG, "Could not release old encoder surface: ${e.message}") }
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
        if (cleanupFinished || stopScheduled) return
        isRunning = false
        startRequested = false
        stopScheduled = true
        val callbackHandler = handler
        if (callbackHandler != null && Looper.myLooper() != callbackHandler.looper) {
            if (callbackHandler.post { stopInternal() }) return
        }
        stopInternal()
    }

    private fun stopInternal() {
        if (cleanupFinished) return
        cleanupFinished = true
        stopScheduled = true
        isRunning = false
        startRequested = false
        configured = false
        val projection = mediaProjection
        val callback = projectionCallback
        if (projection != null && callback != null) {
            try {
                projection.unregisterCallback(callback)
            } catch (e: Exception) {
                Log.w(TAG, "Could not unregister MediaProjection callback: ${e.message}")
            }
        }
        projectionCallback = null
        mediaProjection = null
        val manager = displayManager
        val listener = displayListener
        if (manager != null && listener != null) {
            try {
                manager.unregisterDisplayListener(listener)
            } catch (e: Exception) {
                Log.w(TAG, "Could not unregister display listener: ${e.message}")
            }
        }
        displayManager = null
        displayListener = null
        try {
            virtualDisplay?.release()
        } catch (e: Exception) {
            Log.w(TAG, "Could not release virtual display: ${e.message}")
        }
        virtualDisplay = null
        releaseCodecOnly()

        handlerThread?.quitSafely()
        handlerThread = null
        handler = null
        try {
            projection?.stop()
        } catch (e: Exception) {
            Log.w(TAG, "Could not stop the MediaProjection session: ${e.message}")
        }
        Log.i(TAG, "H.265 hardware encoder stopped")
        onStopped()
    }
}
