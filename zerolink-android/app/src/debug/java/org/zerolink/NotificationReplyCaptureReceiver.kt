package org.zerolink

import android.app.RemoteInput
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Debug-only receiver used to verify cached notification RemoteInput delivery. */
class NotificationReplyCaptureReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        receivedReply = RemoteInput.getResultsFromIntent(intent)
            ?.getCharSequence(RESULT_KEY)
            ?.toString()
        replyReceived.countDown()
    }

    companion object {
        const val RESULT_KEY = "org.zerolink.test.reply"

        @Volatile
        var receivedReply: String? = null
            private set

        @Volatile
        private var replyReceived = CountDownLatch(1)

        fun reset() {
            receivedReply = null
            replyReceived = CountDownLatch(1)
        }

        fun awaitReply(timeout: Long, unit: TimeUnit): Boolean =
            replyReceived.await(timeout, unit)
    }
}
