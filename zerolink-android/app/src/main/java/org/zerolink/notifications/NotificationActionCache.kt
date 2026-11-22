package org.zerolink.notifications

import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.util.Log

data class CachedReplyAction(
    val pendingIntent: PendingIntent,
    val remoteInputs: Array<RemoteInput>
)

/**
 * Bounded cache of notification reply actions.
 *
 * Android's NotificationListenerService can observe `RemoteInput` actions, but the
 * actual `PendingIntent` belongs to the originating app. We retain the intent and the
 * input descriptors so a reply typed in a Windows toast can be delivered later without
 * the notification being in the foreground.
 */
object NotificationActionCache {
    private const val TAG = "ZeroLink.ActionCache"
    private const val MAX_ENTRIES = 32

    private val actions = LinkedHashMap<String, CachedReplyAction>()

    @Synchronized
    fun put(notificationId: String, action: CachedReplyAction) {
        actions.remove(notificationId)
        actions[notificationId] = action
        while (actions.size > MAX_ENTRIES) {
            val oldest = actions.keys.firstOrNull() ?: break
            actions.remove(oldest)
        }
    }

    @Synchronized
    fun remove(notificationId: String) {
        actions.remove(notificationId)
    }

    @Synchronized
    fun clear() {
        actions.clear()
    }

    /**
     * Delivers an inline reply to the originating application's PendingIntent.
     * Returns true when the intent was dispatched successfully.
     */
    fun sendReply(context: Context, notificationId: String, replyText: String): Boolean {
        val cached = synchronized(this) { actions[notificationId] } ?: return false

        return try {
            val intent = Intent()
            val results = Bundle()
            for (input in cached.remoteInputs) {
                results.putCharSequence(input.resultKey, replyText)
            }
            RemoteInput.addResultsToIntent(cached.remoteInputs, intent, results)
            cached.pendingIntent.send(context, 0, intent)
            Log.i(TAG, "Delivered inline notification reply")
            true
        } catch (e: PendingIntent.CanceledException) {
            Log.w(TAG, "Reply PendingIntent was cancelled", e)
            remove(notificationId)
            false
        } catch (e: Exception) {
            Log.e(TAG, "Failed to deliver inline notification reply", e)
            false
        }
    }
}
