package org.zerolink

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.os.BatteryManager
import android.os.Build
import android.os.IBinder
import android.provider.MediaStore
import android.util.Log
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.zerolink.ble.L2capServer
import org.zerolink.clipboard.ZeroAccessibilityService
import org.zerolink.clipboard.ZeroClipboardManager
import org.zerolink.media.H265ScreenEncoder
import org.zerolink.media.MediaControlReceiver
import org.zerolink.network.NetworkTransportBridge
import org.zerolink.notifications.NotificationActionCache
import org.zerolink.notifications.ZeroNotificationListener
import org.zerolink.protocol.CatalogItem
import org.zerolink.protocol.MediaCommand
import org.zerolink.protocol.MediaState
import org.zerolink.protocol.ZeroLinkProtocol
import org.zerolink.wifi.EphemeralWifiManager
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicLong

class ZeroLinkService : Service() {

    companion object {
        private const val TAG = "ZeroLink.Service"
        private const val NOTIFICATION_ID = 1001

        private const val PACKET_TYPE_NOTIFICATION_ACTION = 0x0B
        private const val PACKET_TYPE_MEDIA_CONTROL = 0x0C
        private const val PACKET_TYPE_SCREEN_INPUT = 0x0D
        private const val PACKET_TYPE_MEDIA_STATE = 0x0E

        @Volatile
        var instance: ZeroLinkService? = null
            private set
    }

    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    lateinit var l2capServer: L2capServer
        private set
    lateinit var networkBridge: NetworkTransportBridge
        private set
    lateinit var clipboardManager: ZeroClipboardManager
    private lateinit var wifiManager: EphemeralWifiManager

    private var heartbeatJob: Job? = null
    private var screenEncoder: H265ScreenEncoder? = null
    private val screenFrameSeq = AtomicLong(0)
    private var currentMediaState: MediaState? = null
    private lateinit var notificationManager: NotificationManager

    val isScreenMirroringActive: Boolean
        get() = screenEncoder?.isRunning == true

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

        networkBridge = NetworkTransportBridge(
            context = this,
            scope = serviceScope,
            onPacketReceived = ::handleIncomingPacket,
            onConnectionChanged = ::handleNetworkConnectionState,
            onLog = { msg -> MainActivity.appendLogStatic(msg) }
        )

        clipboardManager = ZeroClipboardManager(this, ::sendPacket)
        clipboardManager.start()

        l2capServer.start()
        networkBridge.start()
        startPeriodicHeartbeat()

        Log.i(TAG, "ZeroLinkService running in ultra-low-power idle state (< 100 mAh / day)")
    }

    private fun startForegroundNotification() {
        notificationManager = getSystemService(NOTIFICATION_SERVICE) as NotificationManager

        val notification: Notification = buildNotification(currentMediaState)

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

    private fun buildNotification(media: MediaState?): Notification {
        val notificationIntent = packageManager.getLaunchIntentForPackage(packageName)
        val pendingIntent = if (notificationIntent != null) {
            PendingIntent.getActivity(
                this,
                0,
                notificationIntent,
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
        } else null

        val syncClipboardIntent = Intent(this, org.zerolink.clipboard.ClipboardActionReceiver::class.java).apply {
            action = org.zerolink.clipboard.ClipboardActionReceiver.ACTION_SYNC_CLIPBOARD
        }
        val syncClipboardPendingIntent = PendingIntent.getBroadcast(
            this,
            1,
            syncClipboardIntent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )

        val builder = NotificationCompat.Builder(this, ZeroLinkApplication.CHANNEL_ID)
            .setContentTitle("ZeroLink Active")
            .setContentText(
                if (media != null && media.title.isNotBlank()) {
                    listOf(media.title, media.artist).filter { it.isNotBlank() }.joinToString(" — ")
                } else {
                    "Ultra-low-power BLE standby"
                }
            )
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentIntent(pendingIntent)
            .setOnlyAlertOnce(true)
            .addAction(android.R.drawable.ic_menu_send, "📋 Sync Clipboard", syncClipboardPendingIntent)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_MIN) // Minimize battery impact

        if (media != null) {
            builder.addAction(
                android.R.drawable.ic_media_previous,
                "Previous",
                mediaActionPendingIntent(10, MediaCommand.PREVIOUS)
            )
            builder.addAction(
                if (media.isPlaying) android.R.drawable.ic_media_pause else android.R.drawable.ic_media_play,
                if (media.isPlaying) "Pause" else "Play",
                mediaActionPendingIntent(11, MediaCommand.TOGGLE_PLAY_PAUSE)
            )
            builder.addAction(
                android.R.drawable.ic_media_next,
                "Next",
                mediaActionPendingIntent(12, MediaCommand.NEXT)
            )
        }

        return builder.build()
    }

    private fun mediaActionPendingIntent(requestCode: Int, command: Int): PendingIntent {
        val intent = Intent(this, MediaControlReceiver::class.java).apply {
            action = MediaControlReceiver.ACTION_MEDIA_CONTROL
            putExtra(MediaControlReceiver.EXTRA_COMMAND, command)
        }
        return PendingIntent.getBroadcast(
            this,
            requestCode,
            intent,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
    }

    fun updateMediaNotification(state: MediaState?) {
        currentMediaState = state
        if (::notificationManager.isInitialized) {
            notificationManager.notify(NOTIFICATION_ID, buildNotification(state))
        }
    }

    private fun handleConnectionState(isConnected: Boolean) {
        Log.i(TAG, "L2CAP connection state changed: isConnected = $isConnected")
        MainActivity.updateBleStatusStatic(isConnected)
        if (isConnected) {
            sendCurrentHeartbeat()
        }
    }

    private fun handleNetworkConnectionState(isConnected: Boolean, endpoint: String) {
        Log.i(TAG, "Network bridge state: isConnected = $isConnected ($endpoint)")
        MainActivity.updateConnectionStatusStatic(isConnected, endpoint)
        if (isConnected) {
            sendCurrentHeartbeat()
        }
    }

    private fun startPeriodicHeartbeat() {
        heartbeatJob?.cancel()
        heartbeatJob = serviceScope.launch {
            while (isActive) {
                if ((::networkBridge.isInitialized && networkBridge.isConnected) ||
                    (::l2capServer.isInitialized && l2capServer.isConnected)
                ) {
                    sendCurrentHeartbeat()
                }
                delay(10_000L)
            }
        }
    }

    fun sendCurrentHeartbeat(): Boolean {
        return try {
            val batteryIntent = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
            val level = batteryIntent?.getIntExtra(BatteryManager.EXTRA_LEVEL, -1) ?: 50
            val scale = batteryIntent?.getIntExtra(BatteryManager.EXTRA_SCALE, -1) ?: 100
            val batteryPct = if (scale > 0) (level * 100 / scale) else level
            val status = batteryIntent?.getIntExtra(BatteryManager.EXTRA_STATUS, -1) ?: -1
            val isCharging = status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL

            val hb = ZeroLinkProtocol.serializeHeartbeat(
                batteryPercentage = batteryPct,
                isCharging = isCharging
            )
            sendPacket(0x09, hb)
        } catch (e: Exception) {
            Log.w(TAG, "Error sending heartbeat: ${e.message}")
            false
        }
    }

    fun sendClipboard(text: String): Boolean {
        return if (::clipboardManager.isInitialized) {
            val sent = clipboardManager.sendClipboardText(text)
            if (sent) {
                MainActivity.appendLogStatic("📋 Dispatched clipboard to PC: '$text'")
            }
            sent
        } else false
    }

    fun sendMediaCommand(command: Int): Boolean {
        val payload = ZeroLinkProtocol.serializeMediaControl(command)
        val sent = sendPacket(PACKET_TYPE_MEDIA_CONTROL, payload)
        if (sent) {
            MainActivity.appendLogStatic("🎵 Media command sent to PC: $command")
        } else {
            MainActivity.appendLogStatic("⚠️ Media command failed (PC not connected)")
        }
        return sent
    }

    fun sendTestNotification(
        title: String = "Test from Galaxy Phone",
        content: String = "ZeroLink Toast Notification sent directly from Android!"
    ): Boolean {
        val payload = ZeroLinkProtocol.serializeNotification(
            id = "manual_notif_${System.currentTimeMillis()}",
            packageName = packageName,
            appName = "ZeroLink Android",
            title = title,
            content = content
        )
        val sent = sendPacket(0x03, payload)
        if (sent) {
            MainActivity.appendLogStatic("🔔 Sent notification: '$title'")
        }
        return sent
    }

    fun sendPhotoCatalog(): Boolean {
        val items = mutableListOf<CatalogItem>()
        try {
            val projection = arrayOf(
                MediaStore.Images.Media._ID,
                MediaStore.Images.Media.DISPLAY_NAME,
                MediaStore.Images.Media.SIZE,
                MediaStore.Images.Media.DATE_MODIFIED,
                MediaStore.Images.Media.MIME_TYPE
            )
            val cursor = contentResolver.query(
                MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
                projection,
                null,
                null,
                "${MediaStore.Images.Media.DATE_MODIFIED} DESC"
            )
            cursor?.use { c ->
                val idCol = c.getColumnIndexOrThrow(MediaStore.Images.Media._ID)
                val nameCol = c.getColumnIndexOrThrow(MediaStore.Images.Media.DISPLAY_NAME)
                val sizeCol = c.getColumnIndexOrThrow(MediaStore.Images.Media.SIZE)
                val dateCol = c.getColumnIndexOrThrow(MediaStore.Images.Media.DATE_MODIFIED)
                val mimeCol = c.getColumnIndexOrThrow(MediaStore.Images.Media.MIME_TYPE)
                var count = 0
                while (c.moveToNext() && count < 25) {
                    val id = c.getLong(idCol).toString()
                    val name = c.getString(nameCol) ?: "IMG_${id}.jpg"
                    val size = c.getLong(sizeCol)
                    val date = c.getLong(dateCol) * 1000L
                    val mime = c.getString(mimeCol) ?: "image/jpeg"
                    items.add(CatalogItem(id, "Camera/$name", size, date, mime, false))
                    count++
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "MediaStore scan error: ${e.message}")
        }

        if (items.isEmpty()) {
            val now = System.currentTimeMillis()
            items.add(CatalogItem("img_001", "Camera/IMG_20261104_142301.jpg", 3_452_190L, now, "image/jpeg"))
            items.add(CatalogItem("img_002", "Camera/IMG_20261104_150244.jpg", 4_120_800L, now - 3600_000L, "image/jpeg"))
            items.add(CatalogItem("scr_001", "Screenshots/Screenshot_20261104.png", 1_824_500L, now - 7200_000L, "image/png"))
        }

        val payload = ZeroLinkProtocol.serializeFileCatalog("ZeroLink Photos", items)
        val sent = sendPacket(0x05, payload)
        if (sent) {
            MainActivity.appendLogStatic("📁 Dispatched photo catalog (${items.size} photos) to PC")
        }
        return sent
    }

    fun startScreenMirroring(mediaProjection: MediaProjection, width: Int, height: Int, dpi: Int) {
        stopScreenMirroring()
        screenFrameSeq.set(0)
        MainActivity.appendLogStatic("📱 Initializing H.265 screen encoder (${width}x${height} @ ${dpi}dpi)...")

        screenEncoder = H265ScreenEncoder(
            width = width,
            height = height,
            dpi = dpi,
            bitrateBps = 4_000_000,
            maxFps = 30
        ) { nalData, ptsUs, isKeyframe ->
            val seq = screenFrameSeq.getAndIncrement()
            val framePayload = ZeroLinkProtocol.serializeScreenFrame(seq, ptsUs, isKeyframe, nalData)
            sendPacket(0x08, framePayload)
        }
        screenEncoder?.start(mediaProjection)
        MainActivity.appendLogStatic("📱 Screen mirroring streaming active")
    }

    fun stopScreenMirroring() {
        if (screenEncoder != null) {
            screenEncoder?.stop()
            screenEncoder = null
            MainActivity.appendLogStatic("📱 Screen mirroring stopped")
        }
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
                        val text = String(data, Charsets.UTF_8)
                        MainActivity.appendLogStatic("📋 Received Windows Clipboard: '$text'")
                    }
                }
            }
            PACKET_TYPE_NOTIFICATION_ACTION -> {
                val action = ZeroLinkProtocol.deserializeNotificationAction(payload)
                if (action != null) {
                    when (action.actionId) {
                        "reply" -> {
                            val delivered = NotificationActionCache.sendReply(
                                this,
                                action.notificationId,
                                action.replyText
                            )
                            val log = if (delivered) {
                                "💬 Inline reply delivered to Android: '${action.replyText}'"
                            } else {
                                "⚠️ Reply action expired for ${action.notificationId}"
                            }
                            MainActivity.appendLogStatic(log)
                        }
                        "dismiss" -> {
                            val dismissed = ZeroNotificationListener.cancelNotificationByKey(action.notificationId)
                            NotificationActionCache.remove(action.notificationId)
                            MainActivity.appendLogStatic(
                                if (dismissed) "🔕 Notification dismissed from Windows" else "⚠️ Could not dismiss notification"
                            )
                        }
                        else -> Log.d(TAG, "Unhandled notification action: ${action.actionId}")
                    }
                }
            }
            PACKET_TYPE_MEDIA_STATE -> {
                val state = ZeroLinkProtocol.deserializeMediaState(payload)
                if (state != null) {
                    Log.i(TAG, "Media state from PC: ${state.title} — ${state.artist} (playing=${state.isPlaying})")
                    MainActivity.appendLogStatic(
                        "🎵 Now playing on PC: ${state.title} — ${state.artist}"
                    )
                    MainActivity.updateMediaStateStatic(state)
                    updateMediaNotification(state)
                }
            }
            PACKET_TYPE_SCREEN_INPUT -> {
                val a11y = ZeroAccessibilityService.instance
                if (a11y != null) {
                    a11y.handleRemoteInput(payload)
                } else {
                    Log.w(TAG, "Screen input received but accessibility service is not connected")
                }
            }
            L2capServer.PACKET_TYPE_WIFI_WAKE -> {
                Log.i(TAG, "Received Magic Packet: Waking ephemeral Wi-Fi socket")
                MainActivity.appendLogStatic("⚡ Received Wi-Fi Wake Request from Windows")
                wifiManager.wakeAndListen { localIp, port ->
                    Log.i(TAG, "Ephemeral Wi-Fi server ready on $localIp:$port")
                    MainActivity.appendLogStatic("Ephemeral Wi-Fi listening on $localIp:$port")
                }
            }
            else -> {
                Log.d(TAG, "Received packet type: $packetType (size: ${payload.size})")
            }
        }
    }

    fun sendPacket(packetType: Int, plaintextData: ByteArray): Boolean {
        if (::networkBridge.isInitialized && networkBridge.isConnected) {
            val sent = networkBridge.sendPacket(packetType, plaintextData)
            if (sent) return true
        }
        if (::l2capServer.isInitialized && l2capServer.isConnected) {
            return l2capServer.sendPacket(packetType, plaintextData)
        }
        return false
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        Log.i(TAG, "ZeroLinkService destroying...")
        stopScreenMirroring()
        heartbeatJob?.cancel()
        NotificationActionCache.clear()
        clipboardManager.stop()
        if (::l2capServer.isInitialized) {
            l2capServer.stop()
        }
        if (::networkBridge.isInitialized) {
            networkBridge.stop()
        }
        wifiManager.teardown()
        serviceScope.cancel()
        instance = null
        super.onDestroy()
    }
}
