package org.zerolink

import android.Manifest
import android.app.NotificationManager
import android.app.PendingIntent
import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.core.app.NotificationCompat
import androidx.appcompat.app.AlertDialog
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import org.zerolink.media.PhotoAccess
import org.zerolink.protocol.MediaCommand
import org.zerolink.protocol.MediaState
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

class MainActivity : AppCompatActivity() {

    companion object {
        @Volatile
        var activeActivity: MainActivity? = null
            private set

        private val mainHandler = Handler(Looper.getMainLooper())
        private const val PAIRING_NOTIFICATION_ID = 2001
        private const val PAIRING_APPROVAL_TIMEOUT_MS = 120_000L
        private const val PAIRING_APPROVAL_WAIT_GRACE_MS = 5_000L

        private class PairingApprovalRequest(
            val peerName: String,
            val code: String,
            val respond: (Boolean) -> Unit
        ) {
            val answered = AtomicBoolean(false)
            var dialog: AlertDialog? = null
            var timeout: Runnable? = null
        }

        @Volatile
        private var pendingPairingApproval: PairingApprovalRequest? = null

        fun appendLogStatic(msg: String) {
            mainHandler.post {
                activeActivity?.appendLog(msg)
            }
        }

        fun updateConnectionStatusStatic(isConnected: Boolean, endpoint: String) {
            mainHandler.post {
                activeActivity?.updateConnectionStatus(isConnected, endpoint)
            }
        }

        fun updateBleStatusStatic(isConnected: Boolean) {
            mainHandler.post {
                activeActivity?.updateBleStatus(isConnected)
            }
        }

        fun refreshServiceStateStatic() {
            mainHandler.post {
                activeActivity?.updateUiState()
            }
        }

        fun updateMediaStateStatic(state: MediaState) {
            mainHandler.post {
                activeActivity?.updateMediaState(state)
            }
        }

        fun requestPairingApprovalStatic(
            peerName: String,
            code: String,
            respond: (Boolean) -> Unit,
            timeoutMs: Long = PAIRING_APPROVAL_TIMEOUT_MS,
        ) {
            require(timeoutMs > 0L)
            val request = PairingApprovalRequest(peerName, code, respond)
            val previous = synchronized(this) {
                val old = pendingPairingApproval
                pendingPairingApproval = request
                old
            }
            previous?.let { old -> mainHandler.post { finishPairingApproval(old, false) } }
            mainHandler.post {
                request.timeout = Runnable { finishPairingApproval(request, false) }
                mainHandler.postDelayed(request.timeout!!, timeoutMs)
                showPairingApproval(request)
            }
        }

        private fun showPairingApproval(request: PairingApprovalRequest) {
            if (request.answered.get() || pendingPairingApproval !== request) return
            if (request.dialog?.isShowing == true) return
            val activity = activeActivity
            if (activity == null || activity.isFinishing || activity.isDestroyed) {
                val app = ZeroLinkApplication.instance
                val launchIntent = Intent(app, MainActivity::class.java).apply {
                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                }
                val pendingIntent = PendingIntent.getActivity(
                    app,
                    PAIRING_NOTIFICATION_ID,
                    launchIntent,
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
                )
                val notification = NotificationCompat.Builder(app, ZeroLinkApplication.PAIRING_CHANNEL_ID)
                    .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
                    .setContentTitle("Approve a ZeroLink device")
                    .setContentText("Tap to compare the pairing code for ${request.peerName}.")
                    .setContentIntent(pendingIntent)
                    .setAutoCancel(true)
                    .setPriority(NotificationCompat.PRIORITY_HIGH)
                    .build()
                val manager = app.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                try {
                    manager.notify(PAIRING_NOTIFICATION_ID, notification)
                } catch (e: SecurityException) {
                    Log.w("ZeroLink.Pairing", "Notification permission is unavailable; waiting for the app to open", e)
                }
                return
            }
            request.dialog = PairingApprovalDialog.show(activity, request.peerName, request.code) { approved ->
                finishPairingApproval(request, approved)
            }
        }

        private fun finishPairingApproval(request: PairingApprovalRequest, approved: Boolean) {
            if (!request.answered.compareAndSet(false, true)) return
            request.timeout?.let { mainHandler.removeCallbacks(it) }
            request.dialog?.dismiss()
            request.dialog = null
            if (pendingPairingApproval === request) pendingPairingApproval = null
            val manager = ZeroLinkApplication.instance.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            manager.cancel(PAIRING_NOTIFICATION_ID)
            request.respond(approved)
        }

        fun showPendingPairingApproval() {
            pendingPairingApproval?.let { request -> showPairingApproval(request) }
        }

        fun awaitPairingApprovalStatic(
            peerName: String,
            code: String,
            timeoutMs: Long = PAIRING_APPROVAL_TIMEOUT_MS,
        ): Boolean {
            val latch = CountDownLatch(1)
            var approved = false
            requestPairingApprovalStatic(
                peerName,
                code,
                { decision ->
                    approved = decision
                    latch.countDown()
                },
                timeoutMs
            )
            return try {
                latch.await(timeoutMs + PAIRING_APPROVAL_WAIT_GRACE_MS, TimeUnit.MILLISECONDS) && approved
            } catch (_: InterruptedException) {
                Thread.currentThread().interrupt()
                false
            }
        }
    }

    private lateinit var tvServiceStatus: TextView
    private lateinit var tvBleStatus: TextView
    private lateinit var tvConnectionStatus: TextView
    private lateinit var tvLogs: TextView
    private lateinit var etHostIp: EditText
    private lateinit var btnConnectHost: Button
    private lateinit var btnToggleService: Button
    private lateinit var btnGrantPermissions: Button
    private lateinit var btnNotificationAccess: Button
    private lateinit var btnBatteryOptimization: Button
    private lateinit var btnAccessibilityAccess: Button
    private lateinit var btnTestClipboard: Button
    private lateinit var btnSendNotif: Button
    private lateinit var btnSyncPhotos: Button
    private lateinit var btnScreenMirror: Button
    private lateinit var btnForgetPairing: Button
    private lateinit var tvMediaTitle: TextView
    private lateinit var tvMediaArtist: TextView
    private lateinit var tvMediaApp: TextView
    private lateinit var btnMediaPrev: Button
    private lateinit var btnMediaToggle: Button
    private lateinit var btnMediaNext: Button

    private var mediaProjectionManager: MediaProjectionManager? = null

    private val permissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { permissions ->
        val allGranted = permissions.entries.all { it.value }
        if (allGranted) {
            appendLog("✅ All runtime permissions granted!")
            Toast.makeText(this, "Permissions granted! Starting service...", Toast.LENGTH_SHORT).show()
            startZeroLinkService()
        } else {
            appendLog("⚠️ Some permissions were denied: $permissions")
        }
        updateUiState()
    }

    private val photoPermissionLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) {
        if (PhotoAccess.hasImageAccess(this)) {
            appendLog("📁 Photo access granted. Syncing the available photo catalog.")
            sendPhotoCatalogToPc()
        } else {
            appendLog("📁 Photo access was not granted; other ZeroLink features remain available.")
            Toast.makeText(this, "Photo access is needed to sync the catalog", Toast.LENGTH_LONG).show()
        }
    }

    private val screenCaptureLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        val service = ZeroLinkService.instance
        if (result.resultCode != RESULT_OK || result.data == null) {
            service?.finishScreenMirroringPreparation()
            appendLog("⚠️ Screen capture permission was not granted")
            return@registerForActivityResult
        }

        if (service == null) {
            appendLog("⚠️ ZeroLinkService is not running")
            return@registerForActivityResult
        }

        if (!service.prepareForScreenMirroring()) {
            appendLog("❌ Could not promote ZeroLinkService for screen capture")
            return@registerForActivityResult
        }

        try {
            val mp = mediaProjectionManager?.getMediaProjection(result.resultCode, result.data!!)
            if (mp != null) {
                val dm = resources.displayMetrics
                val w = if (dm.widthPixels > 0) dm.widthPixels else 720
                val h = if (dm.heightPixels > 0) dm.heightPixels else 1600
                val dpi = if (dm.densityDpi > 0) dm.densityDpi else 300
                service.startScreenMirroring(mp, w, h, dpi)
                updateUiState()
            } else {
                service.finishScreenMirroringPreparation()
                appendLog("❌ Failed to obtain MediaProjection handle")
            }
        } catch (e: SecurityException) {
            service.finishScreenMirroringPreparation()
            Log.e("ZeroLink.ScreenMirror", "Could not create MediaProjection", e)
            appendLog("❌ Android denied the screen capture session: ${e.message}")
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        tvServiceStatus = findViewById(R.id.tv_service_status)
        tvBleStatus = findViewById(R.id.tv_ble_status)
        tvConnectionStatus = findViewById(R.id.tv_connection_status)
        tvLogs = findViewById(R.id.tv_logs)
        etHostIp = findViewById(R.id.et_host_ip)
        btnConnectHost = findViewById(R.id.btn_connect_host)
        btnToggleService = findViewById(R.id.btn_toggle_service)
        btnGrantPermissions = findViewById(R.id.btn_grant_permissions)
        btnNotificationAccess = findViewById(R.id.btn_notification_access)
        btnAccessibilityAccess = findViewById(R.id.btn_accessibility_service)
        btnBatteryOptimization = findViewById(R.id.btn_battery_optimization)
        btnTestClipboard = findViewById(R.id.btn_test_clipboard)
        btnSendNotif = findViewById(R.id.btn_send_notif)
        btnSyncPhotos = findViewById(R.id.btn_sync_photos)
        btnScreenMirror = findViewById(R.id.btn_screen_mirror)
        btnForgetPairing = findViewById(R.id.btn_forget_pairing)
        tvMediaTitle = findViewById(R.id.tv_media_title)
        tvMediaArtist = findViewById(R.id.tv_media_artist)
        tvMediaApp = findViewById(R.id.tv_media_app)
        btnMediaPrev = findViewById(R.id.btn_media_prev)
        btnMediaToggle = findViewById(R.id.btn_media_toggle)
        btnMediaNext = findViewById(R.id.btn_media_next)

        mediaProjectionManager = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager

        setupListeners()
        updateUiState()

        // Check and prompt for permissions on startup if not yet granted
        if (!hasRequiredPermissions()) {
            appendLog("Requesting required runtime permissions...")
            requestRequiredPermissions()
        } else if (ZeroLinkService.instance == null) {
            appendLog("Permissions present. Starting ZeroLinkService...")
            startZeroLinkService()
        }
    }

    override fun onStart() {
        super.onStart()
        activeActivity = this
    }

    override fun onResume() {
        super.onResume()
        activeActivity = this
        showPendingPairingApproval()
        updateUiState()
    }

    override fun onStop() {
        if (activeActivity == this) {
            activeActivity = null
        }
        super.onStop()
    }

    override fun onDestroy() {
        if (activeActivity == this) {
            activeActivity = null
        }
        super.onDestroy()
    }

    private fun setupListeners() {
        btnConnectHost.setOnClickListener {
            val raw = etHostIp.text.toString().trim()
            val host: String
            val port: Int
            if (raw.contains(":")) {
                val parts = raw.split(":")
                host = parts[0]
                port = parts[1].toIntOrNull() ?: 8989
            } else {
                host = if (raw.isNotEmpty()) raw else "127.0.0.1"
                port = 8989
            }

            appendLog("Connecting to Windows host $host:$port...")
            ZeroLinkService.instance?.networkBridge?.start(host, port)
                ?: run {
                    appendLog("⚠️ ZeroLinkService is not running. Starting service first...")
                    startZeroLinkService()
                }
        }

        btnToggleService.setOnClickListener {
            if (ZeroLinkService.instance != null) {
                stopZeroLinkService()
            } else {
                if (hasRequiredPermissions()) {
                    startZeroLinkService()
                } else {
                    requestRequiredPermissions()
                }
            }
        }

        btnGrantPermissions.setOnClickListener {
            requestRequiredPermissions()
        }

        btnNotificationAccess.setOnClickListener {
            appendLog("Opening Notification Listener Settings...")
            try {
                val intent = Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
                startActivity(intent)
            } catch (e: Exception) {
                appendLog("Error opening notification settings: ${e.message}")
            }
        }

        btnAccessibilityAccess.setOnClickListener {
            appendLog("Opening Accessibility Settings for Background Clipboard Auto-Sync...")
            try {
                val intent = Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
                startActivity(intent)
                Toast.makeText(
                    this,
                    "Enable 'ZeroLink Clipboard Auto-Sync' in Installed Apps",
                    Toast.LENGTH_LONG
                ).show()
            } catch (e: Exception) {
                appendLog("Error opening accessibility settings: ${e.message}")
            }
        }

        btnBatteryOptimization.setOnClickListener {
            appendLog("Opening Battery Optimization Settings...")
            try {
                val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
                val isIgnoring = powerManager.isIgnoringBatteryOptimizations(packageName)
                if (!isIgnoring) {
                    @SuppressLint("BatteryLife")
                    val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
                        data = Uri.parse("package:$packageName")
                    }
                    startActivity(intent)
                } else {
                    Toast.makeText(this, "Battery optimization already disabled!", Toast.LENGTH_SHORT).show()
                    appendLog("Battery optimization is already disabled for ZeroLink.")
                }
            } catch (e: Exception) {
                val fallbackIntent = Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
                startActivity(fallbackIntent)
            }
        }

        btnTestClipboard.setOnClickListener {
            val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            val testText = "ZeroLink Android Sync @ ${SimpleDateFormat("HH:mm:ss", Locale.getDefault()).format(Date())}"
            val clip = ClipData.newPlainText("ZeroLink", testText)
            cm.setPrimaryClip(clip)
            appendLog("📋 Test clipboard value copied")
            val s = ZeroLinkService.instance
            if (s != null) {
                val ok = s.sendClipboard(testText)
                if (ok) {
                    Toast.makeText(this, "Copied and synced to PC!", Toast.LENGTH_SHORT).show()
                } else {
                    appendLog("⚠️ Failed to sync clipboard (device not connected)")
                }
            } else {
                appendLog("⚠️ ZeroLinkService is not running")
            }
        }

        btnSendNotif.setOnClickListener {
            val s = ZeroLinkService.instance
            if (s != null) {
                val ok = s.sendTestNotification(
                    "Hello from Galaxy Phone!",
                    "ZeroLink real-time notification forwarding active."
                )
                if (ok) {
                    Toast.makeText(this, "Notification sent to PC!", Toast.LENGTH_SHORT).show()
                } else {
                    appendLog("⚠️ Failed to send notification (device not connected)")
                }
            } else {
                appendLog("⚠️ ZeroLinkService is not running")
            }
        }

        btnSyncPhotos.setOnClickListener {
            syncPhotosOrRequestAccess()
        }

        btnScreenMirror.setOnClickListener {
            val s = ZeroLinkService.instance
            if (s != null) {
                if (s.isScreenMirroringActive) {
                    s.stopScreenMirroring()
                    updateUiState()
                } else {
                    appendLog("Requesting screen capture permission...")
                    screenCaptureLauncher.launch(mediaProjectionManager!!.createScreenCaptureIntent())
                }
            } else {
                appendLog("⚠️ ZeroLinkService is not running")
            }
        }

        btnForgetPairing.setOnClickListener {
            AlertDialog.Builder(this)
                .setTitle("Forget paired Windows device?")
                .setMessage("This removes the saved pairing secret. If the devices are offline, use Forget on both devices before pairing again.")
                .setPositiveButton("Forget") { _, _ ->
                    val removed = ZeroLinkService.instance?.forgetPairedDevice()
                        ?: runCatching { PairingSecretStore(this).clear() }.isSuccess
                    if (!removed) appendLog("⚠️ Could not remove the saved pairing")
                }
                .setNegativeButton("Cancel", null)
                .show()
        }

        btnMediaPrev.setOnClickListener {
            sendMediaCommand(MediaCommand.PREVIOUS, "⏮ Previous")
        }

        btnMediaToggle.setOnClickListener {
            sendMediaCommand(MediaCommand.TOGGLE_PLAY_PAUSE, "⏯ Play/Pause")
        }

        btnMediaNext.setOnClickListener {
            sendMediaCommand(MediaCommand.NEXT, "⏭ Next")
        }
    }

    private fun sendMediaCommand(command: Int, label: String) {
        val service = ZeroLinkService.instance
        if (service != null) {
            val sent = service.sendMediaCommand(command)
            if (!sent) {
                appendLog("⚠️ Failed to send $label (PC not connected)")
            }
        } else {
            appendLog("⚠️ ZeroLinkService is not running")
        }
    }

    fun updateMediaState(state: MediaState) {
        tvMediaTitle.text = state.title.ifBlank { "No media playing" }
        tvMediaArtist.text = listOf(state.artist, state.album)
            .filter { it.isNotBlank() }
            .joinToString(" • ")
            .ifBlank { "—" }
        tvMediaApp.text = state.appName.ifBlank { "Windows media session" }
        btnMediaToggle.text = if (state.isPlaying) "⏸" else "▶"
    }

    private fun startZeroLinkService() {
        appendLog("Starting ZeroLink foreground service...")
        val intent = Intent(this, ZeroLinkService::class.java)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                startForegroundService(intent)
            } else {
                startService(intent)
            }
            appendLog("ZeroLinkService launch command dispatched.")
        } catch (e: Exception) {
            appendLog("❌ Failed to start service: ${e.message}")
        }
        updateUiState()
    }

    private fun stopZeroLinkService() {
        appendLog("Stopping ZeroLinkService...")
        val intent = Intent(this, ZeroLinkService::class.java)
        stopService(intent)
        appendLog("ZeroLinkService stopped.")
        updateUiState()
    }

    private fun updateUiState() {
        val s = ZeroLinkService.instance
        val isRunning = s != null
        if (isRunning) {
            tvServiceStatus.text = "Service Status: 🟢 Running (Standby)"
            tvServiceStatus.setTextColor(0xFF4ADE80.toInt()) // Neon green
            btnToggleService.text = "Stop ZeroLink Service"
            btnToggleService.setBackgroundColor(0xFFDC2626.toInt()) // Red

            tvBleStatus.text = "BLE Radio: 🟢 Advertising (UUID 0xFE2C, PSM: ${currentL2capPsmLabel()})"
            tvBleStatus.setTextColor(0xFF4ADE80.toInt())

            if (s?.networkBridge?.isConnected == true) {
                updateConnectionStatus(true, "Connected")
            } else {
                tvConnectionStatus.text = "Windows Client: 🟡 Standby (Connecting...)"
                tvConnectionStatus.setTextColor(0xFFFBBF24.toInt()) // Amber
            }

            if (s?.isScreenMirroringActive == true) {
                btnScreenMirror.text = "📱 Stop Screen Mirroring"
                btnScreenMirror.setBackgroundColor(0xFFDC2626.toInt())
            } else {
                btnScreenMirror.text = "📱 Start Screen Mirroring"
                btnScreenMirror.setBackgroundColor(0xFF334155.toInt())
            }
        } else {
            tvServiceStatus.text = "Service Status: 🔴 Stopped"
            tvServiceStatus.setTextColor(0xFFEF4444.toInt()) // Red
            btnToggleService.text = "Start ZeroLink Service"
            btnToggleService.setBackgroundColor(0xFF2563EB.toInt()) // Blue

            tvBleStatus.text = "BLE Radio: Inactive"
            tvBleStatus.setTextColor(0xFF94A3B8.toInt())

            tvConnectionStatus.text = "Windows Client: Disconnected"
            tvConnectionStatus.setTextColor(0xFF94A3B8.toInt())

            btnScreenMirror.text = "📱 Start Screen Mirroring"
            btnScreenMirror.setBackgroundColor(0xFF334155.toInt())

            tvMediaTitle.text = "No media playing"
            tvMediaArtist.text = "—"
            tvMediaApp.text = "Windows media session"
            btnMediaToggle.text = "▶"
        }

        val hasPerms = hasRequiredPermissions()
        btnGrantPermissions.isEnabled = !hasPerms
        btnGrantPermissions.text = if (hasPerms) "✅ Permissions Granted" else "🔑 Grant Bluetooth Permissions"

        val a11yActive = org.zerolink.clipboard.ZeroAccessibilityService.isRunning
        if (a11yActive) {
            btnAccessibilityAccess.text = "⚡ Clipboard Auto-Sync: 🟢 Active (No Root)"
            btnAccessibilityAccess.setBackgroundColor(0xFF16A34A.toInt()) // Green
        } else {
            btnAccessibilityAccess.text = "⚡ Enable Clipboard Auto-Sync (No Root)"
            btnAccessibilityAccess.setBackgroundColor(0xFF334155.toInt())
        }
    }

    fun updateConnectionStatus(isConnected: Boolean, endpoint: String) {
        if (isConnected) {
            tvConnectionStatus.text = "Windows Client: 🟢 Connected ($endpoint)"
            tvConnectionStatus.setTextColor(0xFF4ADE80.toInt()) // Green
            btnConnectHost.text = "Connected"
            btnConnectHost.setBackgroundColor(0xFF16A34A.toInt())
        } else {
            tvConnectionStatus.text = "Windows Client: 🔴 Disconnected"
            tvConnectionStatus.setTextColor(0xFFEF4444.toInt()) // Red
            btnConnectHost.text = "Connect"
            btnConnectHost.setBackgroundColor(0xFF0EA5E9.toInt())
        }
    }

    fun updateBleStatus(isConnected: Boolean) {
        if (isConnected) {
            tvBleStatus.text = "BLE Radio: 🟢 Connected via L2CAP CoC"
            tvBleStatus.setTextColor(0xFF4ADE80.toInt())
        } else {
            tvBleStatus.text = "BLE Radio: 🟢 Advertising (UUID 0xFE2C, PSM: ${currentL2capPsmLabel()})"
            tvBleStatus.setTextColor(0xFF4ADE80.toInt())
        }
    }

    private fun currentL2capPsmLabel(): String {
        val psm = ZeroLinkService.instance?.activeL2capPsm ?: 0
        return if (psm > 0) "0x${psm.toString(16).padStart(4, '0')}" else "pending"
    }

    private fun hasRequiredPermissions(): Boolean {
        val required = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            required.add(Manifest.permission.BLUETOOTH_SCAN)
            required.add(Manifest.permission.BLUETOOTH_ADVERTISE)
            required.add(Manifest.permission.BLUETOOTH_CONNECT)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            required.add(Manifest.permission.POST_NOTIFICATIONS)
        }
        return required.all {
            ContextCompat.checkSelfPermission(this, it) == PackageManager.PERMISSION_GRANTED
        }
    }

    private fun requestRequiredPermissions() {
        val toRequest = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            toRequest.add(Manifest.permission.BLUETOOTH_SCAN)
            toRequest.add(Manifest.permission.BLUETOOTH_ADVERTISE)
            toRequest.add(Manifest.permission.BLUETOOTH_CONNECT)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            toRequest.add(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
            toRequest.add(Manifest.permission.ACCESS_FINE_LOCATION)
        }
        permissionLauncher.launch(toRequest.toTypedArray())
    }

    private fun syncPhotosOrRequestAccess() {
        if (ZeroLinkService.instance == null) {
            appendLog("⚠️ ZeroLinkService is not running")
            return
        }
        if (!PhotoAccess.hasImageAccess(this)) {
            appendLog("Requesting photo access for the catalog only...")
            photoPermissionLauncher.launch(PhotoAccess.permissionsToRequest())
            return
        }
        if (PhotoAccess.hasPartialImageAccess(this)) {
            AlertDialog.Builder(this)
                .setTitle("Photo access is limited")
                .setMessage("ZeroLink can currently see only the photos you selected. Sync that selection or choose more photos.")
                .setPositiveButton("Sync selection") { _, _ -> sendPhotoCatalogToPc() }
                .setNegativeButton("Choose more") { _, _ ->
                    photoPermissionLauncher.launch(PhotoAccess.permissionsToRequest())
                }
                .setNeutralButton("Cancel", null)
                .show()
            return
        }
        sendPhotoCatalogToPc()
    }

    private fun sendPhotoCatalogToPc() {
        val service = ZeroLinkService.instance
        if (service == null) {
            appendLog("⚠️ ZeroLinkService is not running")
            return
        }
        if (service.sendPhotoCatalog()) {
            Toast.makeText(this, "Photos catalog synced to PC!", Toast.LENGTH_SHORT).show()
        } else {
            appendLog("⚠️ Could not sync photos; check the PC connection and photo access")
        }
    }

    fun appendLog(msg: String) {
        val time = SimpleDateFormat("HH:mm:ss", Locale.getDefault()).format(Date())
        val line = "[$time] $msg\n"
        tvLogs.append(line)
    }
}
