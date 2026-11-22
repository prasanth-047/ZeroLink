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
import org.zerolink.media.PhotoAccess
import org.zerolink.network.NetworkTransportBridge
import org.zerolink.notifications.NotificationActionCache
import org.zerolink.notifications.ZeroNotificationListener
import org.zerolink.protocol.CatalogItem
import org.zerolink.protocol.MediaCommand
import org.zerolink.protocol.MediaState
import org.zerolink.protocol.WifiWakePayload
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
        private const val PACKET_TYPE_SIGNALING = 0x02

        @Volatile
        var instance: ZeroLinkService? = null
            private set
    }

    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    lateinit var l2capServer: L2capServer
        private set
    val activeL2capPsm: Int
        get() = if (::l2capServer.isInitialized) l2capServer.psm else 0
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
            onLog = { msg -> MainActivity.appendLogStatic(msg) },
            l2capPsmProvider = { l2capServer.psm }
        )

        clipboardManager = ZeroClipboardManager(this, ::sendPacket)
        clipboardManager.start()

        l2capServer.start()
        networkBridge.start()
        startPeriodicHeartbeat()

        Log.i(TAG, "ZeroLinkService running in ultra-low-power idle state (< 100 mAh / day)")
        MainActivity.refreshServiceStateStatic()
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
                MainActivity.appendLogStatic("📋 Clipboard sent to PC")
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
            MainActivity.appendLogStatic("🔔 Notification sent to PC")
        }
        return sent
    }

    fun sendPhotoCatalog(): Boolean {
        if (!PhotoAccess.hasImageAccess(this)) {
            Log.i(TAG, "Photo catalog not sent because image access has not been granted")
            MainActivity.appendLogStatic("📁 Grant photo access from Sync Photos to share the catalog")
            return false
        }

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

        val payload = ZeroLinkProtocol.serializeFileCatalog("ZeroLink Photos", items)
        val sent = sendPacket(0x05, payload)
        if (sent) {
            MainActivity.appendLogStatic("📁 Dispatched photo catalog (${items.size} photos) to PC")
        }
        return sent
    }

    fun prepareForScreenMirroring(): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return true
        return try {
            startForeground(
                NOTIFICATION_ID,
                buildNotification(currentMediaState),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE or
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            )
            true
        } catch (e: Exception) {
            Log.e(TAG, "Could not enable the media-projection foreground-service type", e)
            false
        }
    }

    fun finishScreenMirroringPreparation() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q || isScreenMirroringActive) return
        try {
            startForeground(
                NOTIFICATION_ID,
                buildNotification(currentMediaState),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
            )
        } catch (e: Exception) {
            Log.e(TAG, "Could not restore the connected-device foreground-service type", e)
        }
    }

    fun startScreenMirroring(mediaProjection: MediaProjection, width: Int, height: Int, dpi: Int) {
        screenEncoder?.stop()
        screenEncoder = null
        screenFrameSeq.set(0)
        MainActivity.appendLogStatic("📱 Initializing H.265 screen encoder (${width}x${height} @ ${dpi}dpi)...")

        lateinit var encoder: H265ScreenEncoder
        encoder = H265ScreenEncoder(
            context = this,
            width = width,
            height = height,
            dpi = dpi,
            bitrateBps = 4_000_000,
            maxFps = 30,
            onNalUnitEncoded = { nalData, ptsUs, isKeyframe, frameWidth, frameHeight ->
                val seq = screenFrameSeq.getAndIncrement()
                val framePayload = ZeroLinkProtocol.serializeScreenFrame(
                    seq, ptsUs, isKeyframe, nalData, frameWidth, frameHeight
                )
                sendPacket(0x08, framePayload)
            },
            onStarted = { started ->
                serviceScope.launch(Dispatchers.Main) {
                    if (started) {
                        if (screenEncoder === encoder) {
                            MainActivity.appendLogStatic("📱 Screen mirroring streaming active")
                        }
                    } else {
                        MainActivity.appendLogStatic("❌ H.265 screen encoder could not start")
                    }
                    MainActivity.refreshServiceStateStatic()
                }
            },
            onStopped = {
                serviceScope.launch(Dispatchers.Main) {
                    if (screenEncoder === encoder) {
                        screenEncoder = null
                        finishScreenMirroringPreparation()
                        MainActivity.refreshServiceStateStatic()
                    }
                }
            }
        )
        screenEncoder = encoder
        encoder.start(mediaProjection)
    }

    fun stopScreenMirroring() {
        if (screenEncoder != null) {
            screenEncoder?.stop()
            screenEncoder = null
            MainActivity.appendLogStatic("📱 Screen mirroring stopped")
        }
        finishScreenMirroringPreparation()
    }

    private fun handleIncomingPacket(packetType: Int, payload: ByteArray) {
        when (packetType) {
            PACKET_TYPE_SIGNALING -> {
                if (payload.contentEquals(byteArrayOf(0x50, 0x02))) {
                    if (sendPacket(PACKET_TYPE_SIGNALING, byteArrayOf(0x50, 0x03))) {
                        try {
                            PairingSecretStore(this).clear()
                            MainActivity.appendLogStatic("🔐 Windows requested pairing removal. This device is now forgotten.")
                        } catch (e: Exception) {
                            Log.e(TAG, "Could not remove paired-device secret", e)
                        }
                    } else {
                        MainActivity.appendLogStatic("⚠️ Could not acknowledge Windows pairing removal")
                    }
                } else if (payload.contentEquals(byteArrayOf(0x50, 0x03))) {
                    try {
                        PairingSecretStore(this).clear()
                        MainActivity.appendLogStatic("🔐 Windows acknowledged pairing removal. This device is now forgotten.")
                    } catch (e: Exception) {
                        Log.e(TAG, "Could not remove paired-device secret", e)
                        MainActivity.appendLogStatic("⚠️ Could not remove the saved Windows pairing")
                    }
                }
            }
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
                        MainActivity.appendLogStatic("📋 Received Windows clipboard")
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
                                "💬 Inline reply delivered to Android"
                            } else {
                                "⚠️ Reply action expired"
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
                    Log.i(TAG, "Media state updated from PC (playing=${state.isPlaying})")
                    MainActivity.appendLogStatic("🎵 Media state updated from PC")
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
                val wake = ZeroLinkProtocol.deserializeWifiWake(payload)
                if (wake == null || wake.reason != "wake") {
                    Log.w(TAG, "Ignoring malformed or unsupported Wi-Fi wake payload")
                    return
                }
                Log.i(TAG, "Received authenticated bulk-transfer wake request")
                MainActivity.appendLogStatic("⚡ Received Wi-Fi Wake Request from Windows")
                wifiManager.wakeAndListen(wake.port, wake.sessionToken) { localIp, port ->
                    Log.i(TAG, "Ephemeral Wi-Fi server ready on $localIp:$port")
                    MainActivity.appendLogStatic("Ephemeral Wi-Fi listening on $localIp:$port")
                    val acknowledgement = ZeroLinkProtocol.serializeWifiWake(
                        WifiWakePayload(localIp, port, wake.sessionToken, "ready")
                    )
                    if (!sendPacket(L2capServer.PACKET_TYPE_WIFI_WAKE, acknowledgement)) {
                        Log.w(TAG, "Could not send bulk-transfer ready acknowledgement")
                    }
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

    fun forgetPairedDevice(): Boolean {
        val connected = (::networkBridge.isInitialized && networkBridge.isConnected) ||
            (::l2capServer.isInitialized && l2capServer.isConnected)
        if (connected) {
            val requested = sendPacket(PACKET_TYPE_SIGNALING, byteArrayOf(0x50, 0x02))
            if (requested) {
                MainActivity.appendLogStatic("🔐 Waiting for Windows to acknowledge pairing removal...")
            }
            return requested
        }

        return try {
            PairingSecretStore(this).clear()
            MainActivity.appendLogStatic("🔐 Android pairing removed locally. Use Forget Paired Device on Windows too before pairing again.")
            true
        } catch (e: Exception) {
            Log.e(TAG, "Could not remove paired-device secret", e)
            false
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        MainActivity.refreshServiceStateStatic()
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
        MainActivity.refreshServiceStateStatic()
        super.onDestroy()
    }
}
