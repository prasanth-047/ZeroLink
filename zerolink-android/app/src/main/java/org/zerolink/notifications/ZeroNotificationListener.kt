package org.zerolink.notifications

import android.app.Notification
import android.content.pm.PackageManager
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.zerolink.ZeroLinkService
import org.zerolink.jni.ZeroLinkNative
import org.zerolink.notifications.CachedReplyAction
import org.zerolink.notifications.NotificationActionCache

class ZeroNotificationListener : NotificationListenerService() {

    companion object {
        private const val TAG = "ZeroLink.NotificationListener"
        const val PACKET_TYPE_NOTIFICATION = 0x03
        private const val DEBOUNCE_DELAY_MS = 100L

        @Volatile
        private var instance: ZeroNotificationListener? = null

        /**
         * Cancels a remote notification by its StatusBarNotification key.
         * Safe to call when the listener is not connected.
         */
        fun cancelNotificationByKey(key: String): Boolean {
            val listener = instance ?: return false
            return try {
                listener.cancelNotification(key)
                true
            } catch (e: Exception) {
                Log.w(TAG, "Failed to cancel notification $key: ${e.message}")
                false
            }
        }
    }

    private val scope = CoroutineScope(Dispatchers.Default + Job())
    private val debounceJobs = mutableMapOf<String, Job>()

    override fun onListenerConnected() {
        super.onListenerConnected()
        instance = this
        Log.i(TAG, "ZeroNotificationListener connected and active")
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        val packageName = sbn.packageName
        if (packageName == applicationContext.packageName) {
            // Ignore internal ZeroLink service notifications
            return
        }

        val key = sbn.key
        // Debounce frequent rapid updates from the same notification key
        debounceJobs[key]?.cancel()
        debounceJobs[key] = scope.launch {
            delay(DEBOUNCE_DELAY_MS)
            dispatchNotification(sbn)
        }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        val packageName = sbn.packageName
        if (packageName == applicationContext.packageName) return
        debounceJobs.remove(sbn.key)?.cancel()
        NotificationActionCache.remove(sbn.key)
    }

    private fun dispatchNotification(sbn: StatusBarNotification) {
        val extras = sbn.notification.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() ?: ""
        val text = extras.getCharSequence(Notification.EXTRA_TEXT)?.toString()
            ?: extras.getCharSequence(Notification.EXTRA_BIG_TEXT)?.toString()
            ?: ""

        // Skip completely empty notifications
        if (title.isBlank() && text.isBlank()) {
            return
        }

        cacheReplyAction(sbn)

        val packageName = sbn.packageName
        val appName = try {
            val appInfo = packageManager.getApplicationInfo(packageName, 0)
            packageManager.getApplicationLabel(appInfo).toString()
        } catch (_: PackageManager.NameNotFoundException) {
            packageName
        }

        val isOngoing = (sbn.notification.flags and Notification.FLAG_ONGOING_EVENT) != 0
        val timestamp = sbn.postTime

        val serialized = ZeroLinkNative.serializeNotification(
            id = sbn.key,
            packageName = packageName,
            appName = appName,
            title = title,
            content = text,
            timestampMs = timestamp,
            isOngoing = isOngoing
        ) ?: return

        ZeroLinkService.instance?.sendPacket(PACKET_TYPE_NOTIFICATION, serialized)
    }

    /**
     * Caches the first notification action that supports free-form RemoteInput so
     * Windows inline replies can be delivered back to the originating app.
     */
    private fun cacheReplyAction(sbn: StatusBarNotification) {
        val actions = sbn.notification.actions ?: return
        for (action in actions) {
            val remoteInputs = action.remoteInputs ?: continue
            if (remoteInputs.isEmpty()) continue
            if (remoteInputs.none { it.allowFreeFormInput }) continue
            val pendingIntent = action.actionIntent ?: continue
            NotificationActionCache.put(
                sbn.key,
                CachedReplyAction(pendingIntent, remoteInputs)
            )
            return
        }
    }

    override fun onDestroy() {
        if (instance === this) {
            instance = null
        }
        debounceJobs.values.forEach { it.cancel() }
        debounceJobs.clear()
        super.onDestroy()
    }
}
