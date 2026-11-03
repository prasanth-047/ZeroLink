package org.zerolink

import android.Manifest
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
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class MainActivity : AppCompatActivity() {

    companion object {
        @Volatile
        var activeActivity: MainActivity? = null
            private set

        private val mainHandler = Handler(Looper.getMainLooper())

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
    private lateinit var btnTestClipboard: Button
    private lateinit var btnSendNotif: Button
    private lateinit var btnSyncPhotos: Button
    private lateinit var btnScreenMirror: Button

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

    private val screenCaptureLauncher = registerForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        if (result.resultCode == RESULT_OK && result.data != null) {
            val mp = mediaProjectionManager?.getMediaProjection(result.resultCode, result.data!!)
            if (mp != null) {
                val dm = resources.displayMetrics
                val w = if (dm.widthPixels > 0) dm.widthPixels else 720
                val h = if (dm.heightPixels > 0) dm.heightPixels else 1600
                val dpi = if (dm.densityDpi > 0) dm.densityDpi else 300
                ZeroLinkService.instance?.startScreenMirroring(mp, w, h, dpi)
                updateUiState()
            } else {
                appendLog("❌ Failed to obtain MediaProjection handle")
            }
        } else {
            appendLog("⚠️ Screen capture permission was not granted")
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
        btnBatteryOptimization = findViewById(R.id.btn_battery_optimization)
        btnTestClipboard = findViewById(R.id.btn_test_clipboard)
        btnSendNotif = findViewById(R.id.btn_send_notif)
        btnSyncPhotos = findViewById(R.id.btn_sync_photos)
        btnScreenMirror = findViewById(R.id.btn_screen_mirror)

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
            appendLog("📋 Copied to clipboard: '$testText'")
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
            val s = ZeroLinkService.instance
            if (s != null) {
                val ok = s.sendPhotoCatalog()
                if (ok) {
                    Toast.makeText(this, "Photos catalog synced to PC!", Toast.LENGTH_SHORT).show()
                } else {
                    appendLog("⚠️ Failed to sync photos (device not connected)")
                }
            } else {
                appendLog("⚠️ ZeroLinkService is not running")
            }
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

            tvBleStatus.text = "BLE Radio: 🟢 Advertising (UUID 0xFE2C, PSM: 0x88)"
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
        }

        val hasPerms = hasRequiredPermissions()
        btnGrantPermissions.isEnabled = !hasPerms
        btnGrantPermissions.text = if (hasPerms) "✅ Permissions Granted" else "🔑 Grant Bluetooth Permissions"
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
            tvBleStatus.text = "BLE Radio: 🟢 Advertising (UUID 0xFE2C, PSM: 0x88)"
            tvBleStatus.setTextColor(0xFF4ADE80.toInt())
        }
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

    fun appendLog(msg: String) {
        val time = SimpleDateFormat("HH:mm:ss", Locale.getDefault()).format(Date())
        val line = "[$time] $msg\n"
        tvLogs.append(line)
    }
}
