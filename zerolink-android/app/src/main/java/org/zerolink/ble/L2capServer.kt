package org.zerolink.ble

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothServerSocket
import android.bluetooth.BluetoothSocket
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.content.Context
import android.os.ParcelUuid
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.zerolink.jni.ZeroLinkNative
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.UUID

class L2capServer(
    private val context: Context,
    private val scope: CoroutineScope,
    private val onPacketReceived: (packetType: Int, payload: ByteArray) -> Unit,
    private val onConnectionChanged: (isConnected: Boolean) -> Unit
) {

    companion object {
        private const val TAG = "ZeroLink.L2capServer"
        val ZEROLINK_SERVICE_UUID: UUID = UUID.fromString("0000FE2C-0000-1000-8000-00805F9B34FB")
        const val PACKET_TYPE_HANDSHAKE = 0x01
        const val PACKET_TYPE_CLIPBOARD = 0x04
        const val PACKET_TYPE_WIFI_WAKE = 0x07
        const val HEADER_SIZE = 20
    }

    private val bluetoothManager = context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager
    private val bluetoothAdapter: BluetoothAdapter? = bluetoothManager.adapter

    private var serverSocket: BluetoothServerSocket? = null
    private var clientSocket: BluetoothSocket? = null
    private var inputStream: InputStream? = null
    private var outputStream: OutputStream? = null

    private var serverJob: Job? = null
    private var readerJob: Job? = null
    private var assignedPsm: Int = 0

    @Volatile
    var isConnected: Boolean = false
        private set

    @SuppressLint("MissingPermission")
    fun start() {
        if (bluetoothAdapter == null || !bluetoothAdapter.isEnabled) {
            Log.e(TAG, "Bluetooth is disabled or unavailable")
            return
        }

        try {
            // Android 10+ (API 29+) L2CAP Connection-Oriented Channels
            val server = bluetoothAdapter.listenUsingInsecureL2capChannel()
            serverSocket = server
            assignedPsm = server.psm
            Log.i(TAG, "L2CAP CoC server listening on dynamic PSM: 0x${assignedPsm.toString(16)}")

            startAdvertising(assignedPsm)
            listenForClients()
        } catch (e: Exception) {
            Log.e(TAG, "Failed to start L2CAP server", e)
        }
    }

    @SuppressLint("MissingPermission")
    private fun startAdvertising(psm: Int) {
        val advertiser = bluetoothAdapter?.bluetoothLeAdvertiser ?: run {
            Log.w(TAG, "BLE advertising is not supported on this device")
            return
        }

        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_POWER) // Ultra-low-power idle mode
            .setConnectable(true)
            .setTimeout(0) // Run continuously without waking CPU
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_LOW)
            .build()

        // Embed assigned PSM into manufacturer/service data so Windows client can connect immediately
        val psmBytes = byteArrayOf((psm and 0xFF).toByte(), ((psm shr 8) and 0xFF).toByte())
        val data = AdvertiseData.Builder()
            .setIncludeDeviceName(true)
            .addServiceUuid(ParcelUuid(ZEROLINK_SERVICE_UUID))
            .addServiceData(ParcelUuid(ZEROLINK_SERVICE_UUID), psmBytes)
            .build()

        advertiser.startAdvertising(settings, data, object : AdvertiseCallback() {
            override fun onStartSuccess(settingsInEffect: AdvertiseSettings?) {
                Log.i(TAG, "BLE L2CAP advertisement started successfully (PSM: 0x${psm.toString(16)})")
            }

            override fun onStartFailure(errorCode: Int) {
                Log.e(TAG, "BLE advertisement failed with error code: $errorCode")
            }
        })
    }

    private fun listenForClients() {
        serverJob = scope.launch(Dispatchers.IO) {
            while (isActive) {
                try {
                    Log.i(TAG, "Waiting for Windows L2CAP connection...")
                    val socket = serverSocket?.accept() ?: break
                    clientSocket = socket
                    inputStream = socket.inputStream
                    outputStream = socket.outputStream
                    isConnected = true
                    Log.i(TAG, "Windows client connected via L2CAP CoC: ${socket.remoteDevice.address}")
                    onConnectionChanged(true)

                    startReading()
                } catch (e: Exception) {
                    if (isActive) {
                        Log.w(TAG, "L2CAP accept exception: ${e.message}")
                    }
                }
            }
        }
    }

    private fun startReading() {
        readerJob = scope.launch(Dispatchers.IO) {
            val inStream = inputStream ?: return@launch
            val headerBuffer = ByteArray(HEADER_SIZE)

            try {
                while (isActive) {
                    // Read fixed 20-byte frame header
                    readFully(inStream, headerBuffer, 0, HEADER_SIZE)

                    val magic = ByteBuffer.wrap(headerBuffer, 0, 2).order(ByteOrder.BIG_ENDIAN).short.toInt() and 0xFFFF
                    if (magic != 0x5A4C) {
                        Log.w(TAG, "Invalid magic byte: 0x${magic.toString(16)}, disconnecting")
                        break
                    }

                    val version = headerBuffer[2].toInt()
                    val packetType = headerBuffer[3].toInt() and 0xFF
                    val payloadLen = ByteBuffer.wrap(headerBuffer, 4, 4).order(ByteOrder.BIG_ENDIAN).int

                    if (payloadLen < 0 || payloadLen > 16 * 1024 * 1024) {
                        Log.e(TAG, "Payload length out of bounds: $payloadLen")
                        break
                    }

                    // Read the payload bytes
                    val payloadBuffer = ByteArray(payloadLen)
                    readFully(inStream, payloadBuffer, 0, payloadLen)

                    if (packetType == PACKET_TYPE_HANDSHAKE) {
                        handleHandshake(payloadBuffer)
                    } else {
                        // Construct full frame for JNI decryption
                        val fullFrame = ByteArray(HEADER_SIZE + payloadLen)
                        System.arraycopy(headerBuffer, 0, fullFrame, 0, HEADER_SIZE)
                        System.arraycopy(payloadBuffer, 0, fullFrame, HEADER_SIZE, payloadLen)

                        val decrypted = ZeroLinkNative.decryptFramedPacket(fullFrame)
                        if (decrypted != null) {
                            onPacketReceived(packetType, decrypted)
                        } else {
                            Log.w(TAG, "Failed to decrypt framed packet type: $packetType")
                        }
                    }
                }
            } catch (e: Exception) {
                Log.i(TAG, "L2CAP connection ended: ${e.message}")
            } finally {
                handleDisconnect()
            }
        }
    }

    private fun handleHandshake(initPayload: ByteArray) {
        if (initPayload.len() < 72) {
            Log.e(TAG, "Handshake payload too short: ${initPayload.size}")
            return
        }

        val clientPubkey = ByteArray(32)
        val salt = ByteArray(32)
        System.arraycopy(initPayload, 0, clientPubkey, 0, 32)
        System.arraycopy(initPayload, 32, salt, 0, 32)

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

        // 3. Serialize and send HandshakeReply
        val deviceName = (bluetoothAdapter?.name ?: "Android-ZeroLink").toByteArray(Charsets.UTF_8)
        val replyBuf = ByteBuffer.allocate(32 + 16 + 2 + 4 + deviceName.size).order(ByteOrder.BIG_ENDIAN)
        replyBuf.put(serverPubkey)
        replyBuf.put(ByteArray(16)) // auth_tag placeholder
        replyBuf.putShort(assignedPsm.toShort())
        replyBuf.putInt(deviceName.size)
        replyBuf.put(deviceName)

        val replyPayload = replyBuf.array()
        // HandshakeReply frame has sequence 0, unencrypted
        val frameBuf = ByteBuffer.allocate(HEADER_SIZE + replyPayload.size).order(ByteOrder.BIG_ENDIAN)
        frameBuf.putShort(0x5A4C.toShort()) // Magic
        frameBuf.put(1.toByte())           // Version
        frameBuf.put(PACKET_TYPE_HANDSHAKE.toByte())
        frameBuf.putInt(replyPayload.size)
        frameBuf.putLong(0L)               // Sequence 0
        frameBuf.putInt(0)                 // Header CRC placeholder

        val rawFrame = frameBuf.array()
        val out = outputStream ?: return
        synchronized(out) {
            out.write(rawFrame)
            out.write(replyPayload)
            out.flush()
        }

        Log.i(TAG, "Handshake completed successfully with Windows client!")
    }

    @Synchronized
    fun sendPacket(packetType: Int, plaintextData: ByteArray): Boolean {
        if (!isConnected || outputStream == null) {
            return false
        }

        val framedPacket = ZeroLinkNative.encryptAndFramePacket(packetType, plaintextData)
            ?: run {
                Log.e(TAG, "Failed to encrypt and frame packet (type $packetType)")
                return false
            }

        return try {
            val out = outputStream ?: return false
            synchronized(out) {
                out.write(framedPacket)
                out.flush()
            }
            true
        } catch (e: Exception) {
            Log.e(TAG, "Failed to write frame to L2CAP socket", e)
            handleDisconnect()
            false
        }
    }

    private fun handleDisconnect() {
        isConnected = false
        try {
            inputStream?.close()
            outputStream?.close()
            clientSocket?.close()
        } catch (_: Exception) {}
        inputStream = null
        outputStream = null
        clientSocket = null
        onConnectionChanged(false)
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

    private fun ByteArray.len(): Int = size

    fun stop() {
        readerJob?.cancel()
        serverJob?.cancel()
        handleDisconnect()
        try {
            serverSocket?.close()
        } catch (_: Exception) {}
        serverSocket = null
    }
}
