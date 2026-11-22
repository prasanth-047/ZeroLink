package org.zerolink

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.os.Build

class ZeroLinkApplication : Application() {

    companion object {
        const val CHANNEL_ID = "zerolink_service_channel"
        const val CHANNEL_NAME = "ZeroLink Background Service"
        const val PAIRING_CHANNEL_ID = "zerolink_pairing_requests"
        const val PAIRING_CHANNEL_NAME = "ZeroLink Pairing Requests"

        @Volatile
        lateinit var instance: ZeroLinkApplication
            private set
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                CHANNEL_NAME,
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = "ZeroLink ultra-low-power BLE connectivity channel"
                setShowBadge(false)
            }
            val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            manager.createNotificationChannel(channel)
            manager.createNotificationChannel(
                NotificationChannel(
                    PAIRING_CHANNEL_ID,
                    PAIRING_CHANNEL_NAME,
                    NotificationManager.IMPORTANCE_HIGH
                ).apply {
                    description = "Requests that need your approval to pair a new Windows device"
                    setShowBadge(false)
                }
            )
        }
    }
}
