package org.zerolink.jni

import android.util.Log

object ZeroLinkNative {

    private const val TAG = "ZeroLink.Native"

    init {
        try {
            System.loadLibrary("zerolink_jni")
            Log.i(TAG, "Successfully loaded libzerolink_jni.so")
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "Failed to load libzerolink_jni.so", e)
        }
    }

    /**
     * Generates a new ephemeral Curve25519 keypair for the server and returns the 32-byte public key.
     */
    external fun generateServerEphemeralKey(): ByteArray?

    /**
     * Completes ECDH handshake with the peer public key and salt, initializing AES-256-GCM session cipher.
     */
    external fun completeHandshake(clientPubBytes: ByteArray, saltBytes: ByteArray): Boolean

    /**
     * Encrypts and frames plaintext data into a ZeroLink binary wire packet.
     */
    external fun encryptAndFramePacket(packetTypeVal: Int, plaintextData: ByteArray): ByteArray?

    /**
     * Parses frame header, decrypts ciphertext, validates CRC32 and monotonic sequence number.
     */
    external fun decryptFramedPacket(framedData: ByteArray): ByteArray?

    /**
     * Serializes Android notification fields into ZeroLink binary payload format.
     */
    external fun serializeNotification(
        id: String,
        packageName: String,
        appName: String,
        title: String,
        content: String,
        timestampMs: Long,
        isOngoing: Boolean
    ): ByteArray?

    /**
     * Serializes clipboard payload into ZeroLink binary format.
     */
    external fun serializeClipboard(
        formatVal: Int,
        payloadHash: Long,
        timestampMs: Long,
        data: ByteArray
    ): ByteArray?
}
