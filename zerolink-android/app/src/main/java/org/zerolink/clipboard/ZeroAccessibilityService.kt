package org.zerolink.clipboard

import android.accessibilityservice.AccessibilityService
import android.util.Log
import android.view.accessibility.AccessibilityEvent
import org.zerolink.MainActivity
import org.zerolink.ZeroLinkService

class ZeroAccessibilityService : AccessibilityService() {

    companion object {
        private const val TAG = "ZeroLink.A11y"

        @Volatile
        var isRunning: Boolean = false
            private set
    }

    override fun onServiceConnected() {
        super.onServiceConnected()
        isRunning = true
        Log.i(TAG, "ZeroAccessibilityService connected — zero-root background clipboard sync active!")
        MainActivity.appendLogStatic("✅ Clipboard Accessibility Service active (zero-root auto-sync enabled)")
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

    override fun onInterrupt() {
        Log.i(TAG, "ZeroAccessibilityService interrupted")
    }

    override fun onDestroy() {
        super.onDestroy()
        isRunning = false
        Log.i(TAG, "ZeroAccessibilityService destroyed")
        MainActivity.appendLogStatic("⚠️ Clipboard Accessibility Service disconnected")
    }
}

