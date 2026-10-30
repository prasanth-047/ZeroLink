package org.zerolink

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import org.zerolink.ble.L2capServer
import org.zerolink.clipboard.ZeroClipboardManager
import org.zerolink.wifi.EphemeralWifiManager
import java.nio.ByteBuffer
import java.nio.ByteOrder

class ZeroLinkService : Service() {

    companion object {
        private const val TAG = "ZeroLink.Service"
        private const val NOTIFICATION_ID = 1001

        @Volatile
        var instance: ZeroLinkService? = null
            private set
    }

    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private lateinit var l2capServer: L2capServer
    private lateinit var clipboardManager: ZeroClipboardManager
    private lateinit var wifiManager: EphemeralWifiManager

    override fun onCreate() {
        super.onCreate()
        instance = this
        Log.i(TAG, "Creating ZeroLinkService (Doze Mode zero-wakelock design)...")

        startForegroundNotification()

        wifiManager = EphemeralWifiManager(this, serviceScope)

        l2capServer = L2capServer(
            context = this,
            scope = serviceScope,
            onPacketReceived = ::handleIncomingPacket,
            onConnectionChanged = ::handleConnectionState
        )

        clipboardManager = ZeroClipboardManager(this, l2capServer)
        clipboardManager.start()

        l2capServer.start()
        Log.i(TAG, "ZeroLinkService running in ultra-low-power idle state (< 100 mAh / day)")
    }

    private fun startForegroundNotification() {
        val notificationIntent = packageManager.getLaunchIntentForPackage(packageName)
        val pendingIntent = if (notificationIntent != null) {
            PendingIntent.getActivity(
                this,
                0,
                notificationIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
        } else null

        val notification: Notification = NotificationCompat.Builder(this, ZeroLinkApplication.CHANNEL_ID)
            .setContentTitle("ZeroLink Active")
            .setContentText("Ultra-low-power BLE standby")
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_MIN) // Minimize battery impact
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    private fun handleConnectionState(isConnected: Boolean) {
        Log.i(TAG, "L2CAP connection state changed: isConnected = $isConnected")
    }

    private fun handleIncomingPacket(packetType: Int, payload: ByteArray) {
        when (packetType) {
            L2capServer.PACKET_TYPE_CLIPBOARD -> {
                // Clipboard payload format: format (1B) + hash (8B) + timestamp (8B) + len (4B) + data
                if (payload.size >= 21) {
                    val format = payload[0].toInt()
                    val hash = ByteBuffer.wrap(payload, 1, 8).order(ByteOrder.BIG_ENDIAN).long
                    val dataLen = ByteBuffer.wrap(payload, 17, 4).order(ByteOrder.BIG_ENDIAN).int
                    if (payload.size >= 21 + dataLen) {
                        val data = ByteArray(dataLen)
                        System.arraycopy(payload, 21, data, 0, dataLen)
                        clipboardManager.injectRemoteClipboard(format, hash, data)
                    }
                }
            }
            L2capServer.PACKET_TYPE_WIFI_WAKE -> {
                // WifiWakeRequest from Windows Cloud Filter / Screen cast
                Log.i(TAG, "Received BLE Magic Packet: Waking ephemeral Wi-Fi socket")
                wifiManager.wakeAndListen { localIp, port ->
                    Log.i(TAG, "Ephemeral Wi-Fi server ready on $localIp:$port")
                }
            }
            else -> {
                Log.d(TAG, "Received packet type: $packetType (size: ${payload.size})")
            }
        }
    }

    fun sendPacket(packetType: Int, plaintextData: ByteArray): Boolean {
        return l2capServer.sendPacket(packetType, plaintextData)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Return START_STICKY to guarantee restart if killed by the OS under extreme memory pressure
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        Log.i(TAG, "ZeroLinkService destroying...")
        clipboardManager.stop()
        l2capServer.stop()
        wifiManager.teardown()
        serviceScope.cancel()
        instance = null
        super.onDestroy()
    }
}
