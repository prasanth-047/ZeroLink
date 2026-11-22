package org.zerolink.wifi

import android.content.Context
import android.content.ContentUris
import android.net.wifi.WifiManager
import android.provider.MediaStore
import android.util.Log
import org.zerolink.jni.ZeroLinkNative
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

class EphemeralWifiManager(
    private val context: Context,
    private val scope: CoroutineScope
) {

    companion object {
        private const val TAG = "ZeroLink.EphemeralWifi"
        private const val INACTIVITY_TIMEOUT_MS = 30_000L
        private const val MAX_RANGE_BYTES = 16 * 1024 * 1024
        private const val SESSION_TOKEN_HEX_LENGTH = 64
        private const val MAX_FILE_ID_BYTES = 256
        private const val BULK_CHALLENGE_BYTES = 32
        private const val GCM_NONCE_BYTES = 12
        private const val GCM_TAG_BYTES = 16
        private const val MAX_ENCRYPTED_REQUEST_BYTES = 512
        const val DEFAULT_BULK_PORT = 8443
    }

    private val wifiManager = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private val secureRandom = SecureRandom()
    private var serverSocket: ServerSocket? = null
    private var serverJob: Job? = null
    private var timeoutJob: Job? = null
    private var wasWifiInitiallyEnabled: Boolean = true
    @Volatile private var activeSessionToken: String? = null
    @Volatile private var activePort: Int = DEFAULT_BULK_PORT

    @Volatile
    var isServerActive: Boolean = false
        private set

    /**
     * Woken by BLE Magic Packet (WifiWakeRequest).
     * Spins up an ephemeral local high-throughput socket for bulk data.
     */
    fun wakeAndListen(
        port: Int = DEFAULT_BULK_PORT,
        sessionToken: String,
        onReady: (localIp: String, port: Int) -> Unit
    ) {
        require(port in 1..65535)
        require(decodeSessionToken(sessionToken) != null)
        activeSessionToken = sessionToken
        if (isServerActive) {
            resetInactivityTimeout()
            onReady(getLocalIpAddress() ?: "127.0.0.1", activePort)
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
                activePort = port
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

    private suspend fun handleClient(socket: Socket) {
            var output: DataOutputStream? = null
            var requestReceived = false
            try {
                socket.soTimeout = 10_000
                val input = DataInputStream(socket.getInputStream())
                val outputStream = DataOutputStream(socket.getOutputStream())
                output = outputStream
                val challenge = ByteArray(BULK_CHALLENGE_BYTES).also(secureRandom::nextBytes)
                outputStream.write(challenge)
                outputStream.flush()

                val token = activeSessionToken?.let(::decodeSessionToken)
                    ?: throw IOException("No active transfer token")
                val keys = ZeroLinkNative.deriveWifiBulkKeys(token, challenge)
                    ?: throw IOException("Could not derive transfer keys")

                val requestNonce = ByteArray(GCM_NONCE_BYTES)
                input.readFully(requestNonce)
                val encryptedRequestLength = input.readInt()
                if (encryptedRequestLength !in (GCM_TAG_BYTES + 1)..MAX_ENCRYPTED_REQUEST_BYTES) {
                    throw IOException("Invalid encrypted request length")
                }
                val encryptedRequest = ByteArray(encryptedRequestLength)
                input.readFully(encryptedRequest)
                requestReceived = true

                val requestBody = cryptAesGcm(
                    Cipher.DECRYPT_MODE,
                    keys.copyOfRange(0, 32),
                    requestNonce,
                    requestAad(challenge),
                    encryptedRequest
                )
                val request = parseRangeRequest(requestBody)
                val (fileSize, bytes) = readMediaRange(request.fileId, request.offset, request.length)
                val responseBody = ByteBuffer.allocate(12 + bytes.size)
                    .order(ByteOrder.BIG_ENDIAN)
                    .putLong(fileSize)
                    .putInt(bytes.size)
                    .put(bytes)
                    .array()
                val responseNonce = ByteArray(GCM_NONCE_BYTES).also(secureRandom::nextBytes)
                val encryptedResponse = cryptAesGcm(
                    Cipher.ENCRYPT_MODE,
                    keys.copyOfRange(32, 64),
                    responseNonce,
                    responseAad(challenge, requestBody),
                    responseBody
                )
                outputStream.writeInt(0)
                outputStream.write(responseNonce)
                outputStream.writeInt(encryptedResponse.size)
                outputStream.write(encryptedResponse)
                outputStream.flush()
                resetInactivityTimeout()
            } catch (e: Exception) {
                Log.w(TAG, "Rejected ephemeral transfer request: ${e.message}")
                if (requestReceived) {
                    try {
                        output?.writeInt(-1)
                        output?.flush()
                    } catch (_: Exception) {}
                }
            } finally {
                try { socket.close() } catch (_: Exception) {}
                resetInactivityTimeout()
            }
    }

    private data class MediaRangeRequest(
        val fileId: String,
        val offset: Long,
        val length: Int
    )

    private fun parseRangeRequest(body: ByteArray): MediaRangeRequest {
        val buffer = ByteBuffer.wrap(body).order(ByteOrder.BIG_ENDIAN)
        if (buffer.remaining() < 15 || buffer.get().toInt() != 1) {
            throw IOException("Unsupported media range request")
        }
        val idLength = buffer.short.toInt() and 0xffff
        if (idLength !in 1..MAX_FILE_ID_BYTES || buffer.remaining() != idLength + 12) {
            throw IOException("Invalid media id length")
        }
        val idBytes = ByteArray(idLength)
        buffer.get(idBytes)
        val fileId = idBytes.toString(Charsets.UTF_8)
        val offset = buffer.long
        val length = buffer.int
        if (buffer.hasRemaining() || offset < 0 || length !in 1..MAX_RANGE_BYTES) {
            throw IOException("Invalid file range")
        }
        return MediaRangeRequest(fileId, offset, length)
    }

    private fun decodeSessionToken(token: String): ByteArray? {
        if (token.length != SESSION_TOKEN_HEX_LENGTH) return null
        val bytes = ByteArray(32)
        for (index in bytes.indices) {
            val high = token[index * 2].digitToIntOrNull(16) ?: return null
            val low = token[index * 2 + 1].digitToIntOrNull(16) ?: return null
            bytes[index] = ((high shl 4) or low).toByte()
        }
        return bytes
    }

    private fun cryptAesGcm(mode: Int, key: ByteArray, nonce: ByteArray, aad: ByteArray, data: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(mode, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        cipher.updateAAD(aad)
        return cipher.doFinal(data)
    }

    private fun requestAad(challenge: ByteArray): ByteArray =
        "ZeroLink-v1-wifi-bulk-request".toByteArray(Charsets.UTF_8) + challenge

    private fun responseAad(challenge: ByteArray, requestBody: ByteArray): ByteArray =
        "ZeroLink-v1-wifi-bulk-response".toByteArray(Charsets.UTF_8) + challenge + requestBody

    private fun readMediaRange(fileId: String, offset: Long, length: Int): Pair<Long, ByteArray> {
        val mediaId = fileId.toLongOrNull() ?: throw IOException("Invalid media id")
        val uri = ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, mediaId)
        val descriptor = context.contentResolver.openFileDescriptor(uri, "r")
            ?: throw IOException("Media item is unavailable")
        val input = android.os.ParcelFileDescriptor.AutoCloseInputStream(descriptor)
        input.use { stream ->
            val fileSize = descriptor.statSize
            if (fileSize < 0 || offset > fileSize) throw IOException("Invalid media range")
            val actualLength = minOf(length.toLong(), fileSize - offset).toInt()
            var skipped = 0L
            while (skipped < offset) {
                val step = stream.skip(offset - skipped)
                if (step > 0) {
                    skipped += step
                } else {
                    if (stream.read() < 0) throw IOException("Could not seek to media range")
                    skipped++
                }
            }
            val bytes = ByteArray(actualLength)
            var readTotal = 0
            while (readTotal < actualLength) {
                val count = stream.read(bytes, readTotal, actualLength - readTotal)
                if (count < 0) throw IOException("Media item ended before requested range")
                readTotal += count
            }
            return fileSize to bytes
        }
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
        activeSessionToken = null

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
