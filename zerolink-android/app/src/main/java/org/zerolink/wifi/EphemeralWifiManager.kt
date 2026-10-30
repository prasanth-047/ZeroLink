package org.zerolink.wifi

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.File
import java.io.FileInputStream
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder

class EphemeralWifiManager(
    private val context: Context,
    private val scope: CoroutineScope
) {

    companion object {
        private const val TAG = "ZeroLink.EphemeralWifi"
        private const val INACTIVITY_TIMEOUT_MS = 30_000L
        const val DEFAULT_QUIC_PORT = 8443
    }

    private val wifiManager = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private var serverSocket: ServerSocket? = null
    private var serverJob: Job? = null
    private var timeoutJob: Job? = null
    private var wasWifiInitiallyEnabled: Boolean = true

    @Volatile
    var isServerActive: Boolean = false
        private set

    /**
     * Woken by BLE Magic Packet (WifiWakeRequest).
     * Spins up an ephemeral local high-throughput socket for bulk data.
     */
    fun wakeAndListen(port: Int = DEFAULT_QUIC_PORT, onReady: (localIp: String, port: Int) -> Unit) {
        if (isServerActive) {
            resetInactivityTimeout()
            return
        }

        wasWifiInitiallyEnabled = wifiManager.isWifiEnabled
        if (!wasWifiInitiallyEnabled) {
            @Suppress("DEPRECATION")
            wifiManager.isWifiEnabled = true
            Log.i(TAG, "Temporarily enabled Wi-Fi radio for bulk transfer")
        }

        serverJob = scope.launch(Dispatchers.IO) {
            try {
                val server = ServerSocket()
                server.reuseAddress = true
                server.bind(InetSocketAddress(port))
                serverSocket = server
                isServerActive = true

                val localIp = getLocalIpAddress() ?: "127.0.0.1"
                Log.i(TAG, "Ephemeral bulk socket active on $localIp:$port")
                onReady(localIp, port)

                resetInactivityTimeout()

                while (isActive && isServerActive) {
                    val client = server.accept()
                    resetInactivityTimeout()
                    handleClient(client)
                }
            } catch (e: Exception) {
                if (isActive) {
                    Log.w(TAG, "Ephemeral Wi-Fi server socket terminated: ${e.message}")
                }
            } finally {
                teardown()
            }
        }
    }

    private fun handleClient(socket: Socket) {
        scope.launch(Dispatchers.IO) {
            try {
                val input = socket.getInputStream()
                val output = socket.getOutputStream()

                // Read 4-byte command (0x01: fetch file, 0x02: stream finish)
                val cmdBuf = ByteArray(4)
                if (input.read(cmdBuf) < 4) return@launch

                val cmd = ByteBuffer.wrap(cmdBuf).order(ByteOrder.BIG_ENDIAN).int
                if (cmd == 1) {
                    // File fetch request: 2-byte path len + path + 8-byte offset + 4-byte len
                    val pathLenBuf = ByteArray(2)
                    input.read(pathLenBuf)
                    val pathLen = ByteBuffer.wrap(pathLenBuf).order(ByteOrder.BIG_ENDIAN).short.toInt() and 0xFFFF
                    val pathBytes = ByteArray(pathLen)
                    input.read(pathBytes)
                    val relativePath = String(pathBytes, Charsets.UTF_8)

                    val numBuf = ByteArray(12)
                    input.read(numBuf)
                    val offset = ByteBuffer.wrap(numBuf, 0, 8).order(ByteOrder.BIG_ENDIAN).long
                    val length = ByteBuffer.wrap(numBuf, 8, 4).order(ByteOrder.BIG_ENDIAN).int

                    streamFileChunk(relativePath, offset, length, output)
                }
            } catch (e: Exception) {
                Log.e(TAG, "Error handling ephemeral client transfer", e)
            } finally {
                try { socket.close() } catch (_: Exception) {}
                resetInactivityTimeout()
            }
        }
    }

    private fun streamFileChunk(path: String, offset: Long, length: Int, output: java.io.OutputStream) {
        val file = File(context.filesDir, path)
        if (!file.exists()) {
            output.write(ByteBuffer.allocate(4).order(ByteOrder.BIG_ENDIAN).putInt(-1).array())
            output.flush()
            return
        }

        val actualLen = if (length > 0) length else file.length().toInt()
        val header = ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN).putLong(file.length()).array()
        output.write(header)

        FileInputStream(file).use { fis ->
            fis.skip(offset)
            val buffer = ByteArray(64 * 1024)
            var remaining = actualLen
            while (remaining > 0) {
                val toRead = minOf(buffer.size, remaining)
                val read = fis.read(buffer, 0, toRead)
                if (read <= 0) break
                output.write(buffer, 0, read)
                remaining -= read
            }
            output.flush()
        }
        Log.i(TAG, "Transferred $actualLen bytes for file: $path")
    }

    private fun resetInactivityTimeout() {
        timeoutJob?.cancel()
        timeoutJob = scope.launch {
            delay(INACTIVITY_TIMEOUT_MS)
            Log.i(TAG, "Ephemeral Wi-Fi inactivity timeout reached (30s) — tearing down radio")
            teardown()
        }
    }

    fun teardown() {
        isServerActive = false
        timeoutJob?.cancel()
        try {
            serverSocket?.close()
        } catch (_: Exception) {}
        serverSocket = null

        // If Wi-Fi was initially off before waking, turn it back off to strictly preserve battery
        if (!wasWifiInitiallyEnabled && wifiManager.isWifiEnabled) {
            @Suppress("DEPRECATION")
            wifiManager.isWifiEnabled = false
            Log.i(TAG, "Restored Wi-Fi radio to off state")
        }
    }

    private fun getLocalIpAddress(): String? {
        val ipInt = wifiManager.connectionInfo.ipAddress
        if (ipInt == 0) return null
        return String.format(
            "%d.%d.%d.%d",
            ipInt and 0xFF,
            (ipInt shr 8) and 0xFF,
            (ipInt shr 16) and 0xFF,
            (ipInt shr 24) and 0xFF
        )
    }
}
