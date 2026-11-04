package org.zerolink.clipboard

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import org.zerolink.MainActivity
import org.zerolink.ZeroLinkService
import org.zerolink.protocol.ZeroLinkProtocol

class ZeroAccessibilityService : AccessibilityService() {

    companion object {
        private const val TAG = "ZeroLink.A11y"

        @Volatile
        var isRunning: Boolean = false
            private set

        @Volatile
        var instance: ZeroAccessibilityService? = null
            private set
    }

    private val mainHandler = Handler(Looper.getMainLooper())
    private var activePath: Path? = null
    private var activeStartTime = 0L

    override fun onServiceConnected() {
        super.onServiceConnected()
        isRunning = true
        instance = this
        Log.i(TAG, "ZeroAccessibilityService connected — zero-root clipboard sync + gesture injection active!")
        MainActivity.appendLogStatic("✅ Accessibility Service active (clipboard auto-sync + rootless input)")
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        if (event == null) return

        when (event.eventType) {
            AccessibilityEvent.TYPE_VIEW_CLICKED,
            AccessibilityEvent.TYPE_VIEW_TEXT_SELECTION_CHANGED,
            AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED -> {
                // When user interacts with text, copies, or switches apps, sync clipboard
                triggerClipboardSync()
            }
        }
    }

    private fun triggerClipboardSync() {
        try {
            val cm = getSystemService(CLIPBOARD_SERVICE) as? android.content.ClipboardManager ?: return
            if (cm.hasPrimaryClip()) {
                val clip = cm.primaryClip
                if (clip != null && clip.itemCount > 0) {
                    val text = clip.getItemAt(0).text?.toString()
                    if (!text.isNullOrEmpty()) {
                        val service = ZeroLinkService.instance
                        if (service != null) {
                            service.sendClipboard(text)
                        }
                    }
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error in triggerClipboardSync: ${e.message}", e)
        }
    }

    /**
     * Applies a normalized screen-input event received from the Windows client.
     * Uses AccessibilityService.dispatchGesture(), which is 100% root-free.
     */
    fun handleRemoteInput(payload: ByteArray) {
        val input = ZeroLinkProtocol.deserializeScreenInput(payload) ?: return
        mainHandler.post { injectInput(input.action, input.x, input.y, input.scrollDelta) }
    }

    private fun injectInput(action: Int, normalizedX: Float, normalizedY: Float, scrollDelta: Int) {
        val metrics = resources.displayMetrics
        val x = normalizedX.coerceIn(0f, 1f) * metrics.widthPixels
        val y = normalizedY.coerceIn(0f, 1f) * metrics.heightPixels

        when (action) {
            0 -> {
                activePath = Path().apply { moveTo(x, y) }
                activeStartTime = SystemClock.uptimeMillis()
            }
            1 -> {
                activePath?.lineTo(x, y)
            }
            2 -> {
                val path = activePath ?: return
                path.lineTo(x, y)
                val duration = (SystemClock.uptimeMillis() - activeStartTime).coerceIn(1L, 5_000L)
                dispatchPath(path, duration)
                activePath = null
            }
            3 -> {
                val distance = (scrollDelta / 120f) * (metrics.heightPixels * 0.25f)
                if (distance == 0f) return
                val endY = (y - distance).coerceIn(0f, metrics.heightPixels.toFloat())
                val path = Path().apply {
                    moveTo(x, y)
                    lineTo(x, endY)
                }
                dispatchPath(path, 120L)
            }
            else -> Log.w(TAG, "Unknown screen-input action: $action")
        }
    }

    private fun dispatchPath(path: Path, durationMs: Long) {
        try {
            val gesture = GestureDescription.Builder()
                .addStroke(
                    GestureDescription.StrokeDescription(path, 0, durationMs.coerceAtLeast(1L))
                )
                .build()
            if (!dispatchGesture(gesture, null, null)) {
                Log.w(TAG, "Gesture injection was rejected by the system")
            }
        } catch (e: Exception) {
            Log.e(TAG, "Gesture injection failed: ${e.message}", e)
        }
    }

    override fun onInterrupt() {
        Log.i(TAG, "ZeroAccessibilityService interrupted")
    }

    override fun onDestroy() {
        super.onDestroy()
        isRunning = false
        if (instance === this) {
            instance = null
        }
        Log.i(TAG, "ZeroAccessibilityService destroyed")
        MainActivity.appendLogStatic("⚠️ Accessibility Service disconnected")
    }
}
