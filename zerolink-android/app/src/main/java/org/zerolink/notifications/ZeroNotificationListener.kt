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

class ZeroNotificationListener : NotificationListenerService() {

    companion object {
        private const val TAG = "ZeroLink.NotificationListener"
        const val PACKET_TYPE_NOTIFICATION = 0x03
        private const val DEBOUNCE_DELAY_MS = 100L
    }

    private val scope = CoroutineScope(Dispatchers.Default + Job())
    private val debounceJobs = mutableMapOf<String, Job>()

    override fun onListenerConnected() {
        super.onListenerConnected()
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

    override fun onDestroy() {
        debounceJobs.values.forEach { it.cancel() }
        debounceJobs.clear()
        super.onDestroy()
    }
}
