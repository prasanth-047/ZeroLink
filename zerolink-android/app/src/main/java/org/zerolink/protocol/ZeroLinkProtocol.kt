package org.zerolink.protocol

import java.nio.ByteBuffer
import java.nio.ByteOrder

data class CatalogItem(
    val fileId: String,
    val relativePath: String,
    val fileSize: Long,
    val modifiedTimestamp: Long,
    val mimeType: String,
    val isDirectory: Boolean = false
)

object ZeroLinkProtocol {

    /**
     * Serializes Android Heartbeat telemetry into the 6-byte binary payload format expected by Windows.
     */
    fun serializeHeartbeat(
        batteryPercentage: Int,
        isCharging: Boolean,
        wifiConnected: Boolean = true,
        quicPort: Int = 8443,
        devicePowerMode: Int = 0
    ): ByteArray {
        val buf = ByteBuffer.allocate(6).order(ByteOrder.BIG_ENDIAN)
        buf.put(batteryPercentage.coerceIn(0, 100).toByte())
        buf.put(if (isCharging) 1.toByte() else 0.toByte())
        buf.put(if (wifiConnected) 1.toByte() else 0.toByte())
        buf.putShort(quicPort.toShort())
        buf.put(devicePowerMode.toByte())
        return buf.array()
    }

    /**
     * Serializes a collection of files/photos into a FileCatalog binary payload.
     * Generates Cloud Filter placeholders in the Windows virtual drive.
     */
    fun serializeFileCatalog(syncRootId: String, files: List<CatalogItem>): ByteArray {
        val rootBytes = syncRootId.toByteArray(Charsets.UTF_8)
        val serializedFiles = files.map { file ->
            val idBytes = file.fileId.toByteArray(Charsets.UTF_8)
            val pathBytes = file.relativePath.toByteArray(Charsets.UTF_8)
            val mimeBytes = file.mimeType.toByteArray(Charsets.UTF_8)

            val itemLen = 8 + 8 + 1 + 2 + idBytes.size + 2 + pathBytes.size + 2 + mimeBytes.size
            val itemBuf = ByteBuffer.allocate(itemLen).order(ByteOrder.BIG_ENDIAN)
            itemBuf.putLong(file.fileSize)
            itemBuf.putLong(file.modifiedTimestamp)
            itemBuf.put(if (file.isDirectory) 1.toByte() else 0.toByte())
            itemBuf.putShort(idBytes.size.toShort())
            itemBuf.put(idBytes)
            itemBuf.putShort(pathBytes.size.toShort())
            itemBuf.put(pathBytes)
            itemBuf.putShort(mimeBytes.size.toShort())
            itemBuf.put(mimeBytes)
            itemBuf.array()
        }

        val totalFileBytes = serializedFiles.sumOf { 4 + it.size }
        val catalogBuf = ByteBuffer.allocate(2 + rootBytes.size + 4 + totalFileBytes).order(ByteOrder.BIG_ENDIAN)
        catalogBuf.putShort(rootBytes.size.toShort())
        catalogBuf.put(rootBytes)
        catalogBuf.putInt(files.size)

        for (fBytes in serializedFiles) {
            catalogBuf.putInt(fBytes.size)
            catalogBuf.put(fBytes)
        }

        return catalogBuf.array()
    }

    /**
     * Serializes a VideoFrameHeader (23 bytes) prepended to HEVC NAL data.
     */
    fun serializeScreenFrame(
        sequence: Long,
        ptsUs: Long,
        isKeyframe: Boolean,
        nalData: ByteArray
    ): ByteArray {
        val headerBuf = ByteBuffer.allocate(23).order(ByteOrder.BIG_ENDIAN)
        headerBuf.putLong(sequence)
        headerBuf.putLong(ptsUs)
        headerBuf.put(if (isKeyframe) 1.toByte() else 0.toByte())
        headerBuf.putShort(1.toShort()) // nal_unit_count
        headerBuf.putInt(nalData.size)

        val payload = ByteArray(23 + nalData.size)
        System.arraycopy(headerBuf.array(), 0, payload, 0, 23)
        System.arraycopy(nalData, 0, payload, 23, nalData.size)
        return payload
    }

    /**
     * Serializes Android Notification payload matching Windows NotificationPayload wire format.
     */
    fun serializeNotification(
        id: String,
        packageName: String,
        appName: String,
        title: String,
        content: String,
        timestampMs: Long = System.currentTimeMillis(),
        isOngoing: Boolean = false
    ): ByteArray {
        val idBytes = id.toByteArray(Charsets.UTF_8)
        val pkgBytes = packageName.toByteArray(Charsets.UTF_8)
        val appBytes = appName.toByteArray(Charsets.UTF_8)
        val titleBytes = title.toByteArray(Charsets.UTF_8)
        val contentBytes = content.toByteArray(Charsets.UTF_8)

        val totalLen = 8 + 1 + 2 + idBytes.size + 2 + pkgBytes.size + 2 + appBytes.size + 2 + titleBytes.size + 2 + contentBytes.size
        val buf = ByteBuffer.allocate(totalLen).order(ByteOrder.BIG_ENDIAN)
        buf.putLong(timestampMs)
        buf.put(if (isOngoing) 1.toByte() else 0.toByte())

        buf.putShort(idBytes.size.toShort())
        buf.put(idBytes)
        buf.putShort(pkgBytes.size.toShort())
        buf.put(pkgBytes)
        buf.putShort(appBytes.size.toShort())
        buf.put(appBytes)
        buf.putShort(titleBytes.size.toShort())
        buf.put(titleBytes)
        buf.putShort(contentBytes.size.toShort())
        buf.put(contentBytes)

        return buf.array()
    }
}
