package org.zerolink.network

import android.annotation.SuppressLint
import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.zerolink.ble.L2capServer
import org.zerolink.MainActivity
import org.zerolink.PairingSecretStore
import org.zerolink.jni.ZeroLinkNative
import java.io.InputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.MessageDigest
import java.util.zip.CRC32

class NetworkTransportBridge(
    private val context: Context,
    private val scope: CoroutineScope,
    private val onPacketReceived: (packetType: Int, payload: ByteArray) -> Unit,
    private val onConnectionChanged: (isConnected: Boolean, endpoint: String) -> Unit,
    private val onLog: (message: String) -> Unit,
    private val l2capPsmProvider: () -> Int
) {

    companion object {
        private const val TAG = "ZeroLink.NetworkBridge"
        const val HEADER_SIZE = 20
        const val MAGIC = 0x5A4C
        const val DEFAULT_PORT = 8989
    }

    private var clientSocket: Socket? = null
    private var inputStream: InputStream? = null
    private var outputStream: OutputStream? = null

    private var transportJob: Job? = null
    private var writerJob: Job? = null
    private val outboundChannel = Channel<ByteArray>(Channel.UNLIMITED)

    private var currentHost: String = "127.0.0.1"
    private var currentPort: Int = DEFAULT_PORT
    private val pairingSecretStore = PairingSecretStore(context)
    @Volatile private var expectedClientConfirmation: ByteArray? = null
    @Volatile private var pendingFirstPairingApproval: Boolean = false

    @Volatile
    var isConnected: Boolean = false
        private set

    @Volatile
    private var isRunning: Boolean = false

    fun start(host: String = "127.0.0.1", port: Int = DEFAULT_PORT) {
        if (isRunning && host == currentHost && port == currentPort) return
        stop()

        currentHost = host
        currentPort = port
        isRunning = true

        // Outbound writer loop (runs strictly on Dispatchers.IO, avoids NetworkOnMainThreadException)
        writerJob = scope.launch(Dispatchers.IO) {
            for (packet in outboundChannel) {
                if (!isActive || !isRunning) break
                val out = outputStream ?: continue
                try {
                    synchronized(out) {
                        out.write(packet)
                        out.flush()
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "Error writing outbound frame: ${e.message}")
                    handleDisconnect()
                }
            }
        }

        transportJob = scope.launch(Dispatchers.IO) {
            while (isActive && isRunning) {
                try {
                    onLog("Connecting to Windows Daemon at $currentHost:$currentPort...")
                    Log.i(TAG, "Connecting to Windows Daemon at $currentHost:$currentPort...")

                    val socket = Socket()
                    socket.connect(InetSocketAddress(currentHost, currentPort), 3000)
                    socket.tcpNoDelay = true

                    clientSocket = socket
                    inputStream = socket.getInputStream()
                    outputStream = socket.getOutputStream()

                    onLog("🌐 Socket connected to $currentHost:$currentPort. Awaiting secure handshake...")
                    Log.i(TAG, "Connected to $currentHost:$currentPort, awaiting handshake")

                    runReaderLoop()
                } catch (e: Exception) {
                    if (isRunning && isActive) {
                        Log.d(TAG, "Network connection attempt failed: ${e.message}")
                    }
                } finally {
                    handleDisconnect()
                    if (isRunning && isActive) {
                        delay(3000) // Reconnect delay
                    }
                }
            }
        }
    }

    private fun runReaderLoop() {
        val inStream = inputStream ?: return
        val headerBuffer = ByteArray(HEADER_SIZE)

        while (isRunning && !clientSocket!!.isClosed) {
            readFully(inStream, headerBuffer, 0, HEADER_SIZE)

            val magic = ByteBuffer.wrap(headerBuffer, 0, 2).order(ByteOrder.BIG_ENDIAN).short.toInt() and 0xFFFF
            if (magic != MAGIC) {
                Log.w(TAG, "Invalid magic byte 0x${magic.toString(16)}, terminating connection")
                break
            }

            val packetType = headerBuffer[3].toInt() and 0xFF
            val payloadLen = ByteBuffer.wrap(headerBuffer, 4, 4).order(ByteOrder.BIG_ENDIAN).int

            if (payloadLen < 0 || payloadLen > 16 * 1024 * 1024) {
                Log.e(TAG, "Invalid payload length: $payloadLen")
                break
            }

            val payloadBuffer = ByteArray(payloadLen)
            readFully(inStream, payloadBuffer, 0, payloadLen)

            if (packetType == L2capServer.PACKET_TYPE_HANDSHAKE) {
                if (payloadBuffer.size >= 88) handleHandshake(payloadBuffer)
                else handleClientConfirmation(payloadBuffer)
            } else {
                if (!isConnected) {
                    Log.w(TAG, "Ignoring data packet before client key confirmation")
                    continue
                }
                val fullFrame = ByteArray(HEADER_SIZE + payloadLen)
                System.arraycopy(headerBuffer, 0, fullFrame, 0, HEADER_SIZE)
                System.arraycopy(payloadBuffer, 0, fullFrame, HEADER_SIZE, payloadLen)

                val decrypted = ZeroLinkNative.decryptFramedPacket(fullFrame)
                if (decrypted != null) {
                    onPacketReceived(packetType, decrypted)
                } else {
                    Log.w(TAG, "Failed to decrypt network frame (type $packetType)")
                }
            }
        }
    }

    @SuppressLint("MissingPermission")
    private fun handleHandshake(initPayload: ByteArray) {
        if (initPayload.size < 88) {
            Log.e(TAG, "Handshake payload too short: ${initPayload.size}")
            return
        }
        val handshakeVersion = ByteBuffer.wrap(initPayload, 64, 4).order(ByteOrder.BIG_ENDIAN).int
        if (handshakeVersion != 3) {
            Log.e(TAG, "Unsupported handshake version: $handshakeVersion")
            return
        }

        val clientPubkey = ByteArray(32)
        val salt = ByteArray(32)
        System.arraycopy(initPayload, 0, clientPubkey, 0, 32)
        System.arraycopy(initPayload, 32, salt, 0, 32)
        val clientNameLen = ByteBuffer.wrap(initPayload, 68, 4).order(ByteOrder.BIG_ENDIAN).int
        if (clientNameLen < 0 || initPayload.size != 72 + clientNameLen + 16) {
            Log.e(TAG, "Malformed HandshakeInit name or pairing proof")
            return
        }
        val clientPairingTag = initPayload.copyOfRange(72 + clientNameLen, initPayload.size)
        val clientName = String(initPayload, 72, clientNameLen, Charsets.UTF_8)
        val storedPairingSecret = try {
            pairingSecretStore.load()
        } catch (e: Exception) {
            Log.e(TAG, "Could not load paired-device secret; refusing fallback pairing", e)
            return
        }
        val clientPairingTranscript = clientPubkey + salt
        if (!PairingSecretStore.verifyPeerProof(storedPairingSecret, clientPairingTag, "client", clientPairingTranscript)) {
            Log.e(TAG, "Windows pairing proof failed or pairing state is inconsistent")
            return
        }

        // 1. Generate server ephemeral keypair
        val serverPubkey = ZeroLinkNative.generateServerEphemeralKey() ?: run {
            Log.e(TAG, "Failed to generate server ephemeral Curve25519 key")
            return
        }

        // 2. Complete Diffie-Hellman handshake & derive AES-256-GCM session keys
        val success = ZeroLinkNative.completeHandshake(clientPubkey, salt)
        if (!success) {
            Log.e(TAG, "Diffie-Hellman key derivation failed")
            return
        }

        val transcript = "ZeroLink-v3-server-key-confirmation".toByteArray(Charsets.UTF_8) +
            clientPubkey + salt + serverPubkey
        val authTag = ZeroLinkNative.keyConfirmationTag(transcript) ?: run {
            Log.e(TAG, "Failed to generate handshake key confirmation")
            return
        }
        val clientTranscript = "ZeroLink-v3-client-key-confirmation".toByteArray(Charsets.UTF_8) +
            clientPubkey + salt + serverPubkey
        expectedClientConfirmation = ZeroLinkNative.keyConfirmationTag(clientTranscript) ?: run {
            Log.e(TAG, "Failed to create expected client key confirmation")
            return
        }

        // 3. Serialize and send HandshakeReply
        val deviceName = (Build.MODEL ?: "Android-ZeroLink").toByteArray(Charsets.UTF_8)
        val serverPairingTranscript = clientPubkey + salt + serverPubkey
        val serverPairingTag = storedPairingSecret?.let {
            PairingSecretStore.authTag(it, "server", serverPairingTranscript)
        } ?: ByteArray(16)
        val replyBuf = ByteBuffer.allocate(32 + 16 + 2 + 4 + deviceName.size + 16).order(ByteOrder.BIG_ENDIAN)
        replyBuf.put(serverPubkey)
        replyBuf.put(authTag)
        replyBuf.putShort(l2capPsmProvider().toShort())
        replyBuf.putInt(deviceName.size)
        replyBuf.put(deviceName)
        replyBuf.put(serverPairingTag)

        val replyPayload = replyBuf.array()
        val frameBuf = ByteBuffer.allocate(HEADER_SIZE).order(ByteOrder.BIG_ENDIAN)
        frameBuf.putShort(0x5A4C.toShort()) // Magic
        frameBuf.put(1.toByte())           // Version
        frameBuf.put(L2capServer.PACKET_TYPE_HANDSHAKE.toByte())
        frameBuf.putInt(replyPayload.size)
        frameBuf.putLong(0L)               // Sequence 0

        // Compute valid CRC32 over the 16-byte header prefix
        val crc32 = CRC32()
        crc32.update(frameBuf.array(), 0, 16)
        frameBuf.putInt(crc32.value.toInt())

        val rawHeader = frameBuf.array()
        val out = outputStream ?: return
        synchronized(out) {
            out.write(rawHeader)
            out.write(replyPayload)
            out.flush()
        }

        if (storedPairingSecret == null) {
            val code = ZeroLinkNative.pairingCode(clientPubkey, salt, serverPubkey) ?: run {
                Log.e(TAG, "Could not calculate pairing code")
                handleDisconnect()
                return
            }
            onLog("🔐 Waiting for local approval of first pairing with $clientName")
            if (!MainActivity.awaitPairingApprovalStatic(clientName, code)) {
                Log.w(TAG, "First pairing was rejected, timed out, or no approval screen was available")
                handleDisconnect()
                return
            }
            pendingFirstPairingApproval = true
        }

        onLog("🔐 Waiting for Windows key confirmation...")
        Log.i(TAG, "Handshake reply sent; waiting for Windows key confirmation")
    }

    private fun handleClientConfirmation(payload: ByteArray) {
        val expected = expectedClientConfirmation
        if (payload.size != 17 || payload[0] != 0x02.toByte() || expected == null ||
            !MessageDigest.isEqual(expected, payload.copyOfRange(1, 17))) {
            Log.e(TAG, "Windows client key confirmation failed")
            handleDisconnect()
            return
        }
        expectedClientConfirmation = null
        if (pendingFirstPairingApproval) {
            val secret = ZeroLinkNative.pairingSecret() ?: run {
                Log.e(TAG, "Could not retrieve the new pairing secret")
                handleDisconnect()
                return
            }
            try {
                pairingSecretStore.save(secret)
            } catch (e: Exception) {
                Log.e(TAG, "Could not persist the approved pairing", e)
                handleDisconnect()
                return
            }
            val acknowledgement = ZeroLinkNative.encryptAndFramePacket(
                0x02,
                byteArrayOf(0x50, 0x01)
            ) ?: run {
                Log.e(TAG, "Could not create pairing acknowledgement")
                handleDisconnect()
                return
            }
            if (outboundChannel.trySend(acknowledgement).isFailure) {
                Log.e(TAG, "Could not queue pairing acknowledgement")
                handleDisconnect()
                return
            }
            pendingFirstPairingApproval = false
        }
        isConnected = true
        onConnectionChanged(true, "$currentHost:$currentPort")
        onLog("🔒 Mutual key confirmation succeeded")
        Log.i(TAG, "Mutual handshake key confirmation succeeded")
    }

    fun sendPacket(packetType: Int, plaintextData: ByteArray): Boolean {
        if (!isConnected || outputStream == null) {
            return false
        }

        val framedPacket = ZeroLinkNative.encryptAndFramePacket(packetType, plaintextData)
            ?: run {
                Log.e(TAG, "Failed to encrypt and frame packet (type $packetType)")
                return false
            }

        return outboundChannel.trySend(framedPacket).isSuccess
    }

    private fun handleDisconnect() {
        expectedClientConfirmation = null
        pendingFirstPairingApproval = false
        if (isConnected) {
            onConnectionChanged(false, "$currentHost:$currentPort")
            onLog("⚠️ Disconnected from Windows daemon ($currentHost:$currentPort)")
        }
        isConnected = false
        try {
            inputStream?.close()
            outputStream?.close()
            clientSocket?.close()
        } catch (_: Exception) {}
        inputStream = null
        outputStream = null
        clientSocket = null
    }

    private fun readFully(stream: InputStream, buffer: ByteArray, offset: Int, length: Int) {
        var bytesRead = 0
        while (bytesRead < length) {
            val count = stream.read(buffer, offset + bytesRead, length - bytesRead)
            if (count < 0) {
                throw java.io.EOFException("Premature EOF while reading frame bytes")
            }
            bytesRead += count
        }
    }

    fun stop() {
        isRunning = false
        writerJob?.cancel()
        transportJob?.cancel()
        handleDisconnect()
    }
}
