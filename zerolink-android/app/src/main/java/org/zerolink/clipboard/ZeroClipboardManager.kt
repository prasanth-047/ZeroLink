package org.zerolink.clipboard

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import org.zerolink.ble.L2capServer
import org.zerolink.jni.ZeroLinkNative
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicLong

class ZeroClipboardManager(
    private val context: Context,
    private val l2capServer: L2capServer
) {

    companion object {
        private const val TAG = "ZeroLink.Clipboard"
        const val FORMAT_PLAIN_TEXT = 0
    }

    private val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    private val mainHandler = Handler(Looper.getMainLooper())

    private val lastRemoteHash = AtomicLong(0L)
    private val lastLocalHash = AtomicLong(0L)

    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener {
        handleClipboardChange()
    }

    fun start() {
        mainHandler.post {
            try {
                clipboard.addPrimaryClipChangedListener(clipListener)
                Log.i(TAG, "Clipboard listener registered")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to register clip listener", e)
            }
        }
    }

    fun stop() {
        mainHandler.post {
            try {
                clipboard.removePrimaryClipChangedListener(clipListener)
            } catch (_: Exception) {}
        }
    }

    /**
     * Explicitly syncs the clipboard on user demand (e.g. from notification action, quick setting tile,
     * or foreground resume) to comply with Android 10+ background restrictions without requiring
     * battery-draining AccessibilityService.
     */
    fun syncClipboardNow() {
        mainHandler.post {
            handleClipboardChange()
        }
    }

    private fun handleClipboardChange() {
        if (!clipboard.hasPrimaryClip()) return

        val clip = clipboard.primaryClip ?: return
        if (clip.itemCount == 0) return

        val item = clip.getItemAt(0)
        val text = item.text?.toString() ?: return

        val textBytes = text.toByteArray(Charsets.UTF_8)
        val hash = computeHash(textBytes)

        val remoteH = lastRemoteHash.get()
        val localH = lastLocalHash.get()

        // Anti-echo protection
        if (hash == remoteH || hash == localH) {
            return
        }

        lastLocalHash.set(hash)
        val now = System.currentTimeMillis()

        val serialized = ZeroLinkNative.serializeClipboard(
            FORMAT_PLAIN_TEXT,
            hash,
            now,
            textBytes
        ) ?: return

        val sent = l2capServer.sendPacket(L2capServer.PACKET_TYPE_CLIPBOARD, serialized)
        if (sent) {
            Log.i(TAG, "Local clipboard synced to Windows (hash: ${java.lang.Long.toHexString(hash)}, len: ${textBytes.size})")
        }
    }

    /**
     * Injects remote clipboard content received from Windows.
     */
    fun injectRemoteClipboard(format: Int, hash: Long, data: ByteArray) {
        lastRemoteHash.set(hash)
        mainHandler.post {
            try {
                val text = String(data, Charsets.UTF_8)
                val clipData = ClipData.newPlainText("ZeroLink Remote", text)
                clipboard.setPrimaryClip(clipData)
                Log.i(TAG, "Injected remote clipboard from Windows (hash: ${java.lang.Long.toHexString(hash)})")
            } catch (e: Exception) {
                Log.e(TAG, "Failed to inject remote clipboard", e)
            }
        }
    }

    private fun computeHash(data: ByteArray): Long {
        val digest = MessageDigest.getInstance("SHA-256")
        val hashBytes = digest.digest(data)
        var result = 0L
        for (i in 0..7) {
            result = (result shl 8) or (hashBytes[i].toLong() and 0xFF)
        }
        return result
    }
}
