package org.zerolink

import android.content.Context
import android.content.ContentUris
import android.content.ContentValues
import android.content.Intent
import android.content.pm.PackageManager
import android.Manifest
import android.app.PendingIntent
import android.app.RemoteInput
import android.os.Build
import android.os.Environment
import android.os.SystemClock
import android.provider.MediaStore
import android.test.InstrumentationTestCase
import android.view.accessibility.AccessibilityNodeInfo
import java.io.File
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.ServerSocket
import java.net.Socket
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.SecureRandom
import java.security.KeyStore
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import androidx.appcompat.app.AlertDialog
import javax.crypto.Cipher
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec
import java.util.UUID
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import org.zerolink.media.PhotoAccess
import org.zerolink.jni.ZeroLinkNative
import org.zerolink.clipboard.ZeroClipboardManager
import org.zerolink.notifications.CachedReplyAction
import org.zerolink.notifications.NotificationActionCache
import org.zerolink.wifi.EphemeralWifiManager

@Suppress("DEPRECATION")
class PairingSecretStorePersistenceTest : InstrumentationTestCase() {
    fun testPhotoAccessDeclaresAndRequestsVersionAppropriatePermissions() {
        val appContext = instrumentation.targetContext
        val packageInfo = appContext.packageManager.getPackageInfo(
            appContext.packageName,
            PackageManager.GET_PERMISSIONS
        )
        val declared = packageInfo.requestedPermissions.orEmpty().toSet()

        assertTrue(declared.contains(Manifest.permission.READ_MEDIA_IMAGES))
        if (Build.VERSION.SDK_INT <= Build.VERSION_CODES.S_V2) {
            assertTrue(declared.contains(Manifest.permission.READ_EXTERNAL_STORAGE))
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            assertTrue(declared.contains(Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED))
        }

        assertEquals(
            setOf(Manifest.permission.READ_EXTERNAL_STORAGE),
            PhotoAccess.permissionsToRequest(Build.VERSION_CODES.S_V2).toSet()
        )
        assertEquals(
            setOf(Manifest.permission.READ_MEDIA_IMAGES),
            PhotoAccess.permissionsToRequest(Build.VERSION_CODES.TIRAMISU).toSet()
        )
        assertEquals(
            setOf(
                Manifest.permission.READ_MEDIA_IMAGES,
                Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED
            ),
            PhotoAccess.permissionsToRequest(Build.VERSION_CODES.UPSIDE_DOWN_CAKE).toSet()
        )
        assertEquals(
            PhotoAccess.permissionsToRequest(Build.VERSION.SDK_INT).toSet(),
            PhotoAccess.permissionsToRequest().toSet()
        )
        assertFalse(PhotoAccess.hasImageAccess(Build.VERSION_CODES.TIRAMISU, false, true))
        assertTrue(PhotoAccess.hasImageAccess(Build.VERSION_CODES.TIRAMISU, true, false))
        assertTrue(PhotoAccess.hasImageAccess(Build.VERSION_CODES.UPSIDE_DOWN_CAKE, false, true))
        assertFalse(PhotoAccess.hasPartialImageAccess(Build.VERSION_CODES.UPSIDE_DOWN_CAKE, true, true))
        assertTrue(PhotoAccess.hasPartialImageAccess(Build.VERSION_CODES.UPSIDE_DOWN_CAKE, false, true))
    }

    fun testRemoteClipboardDecodesOnlyPlainTextFormat() {
        val text = "synthetic clipboard value"
        assertEquals(
            text,
            ZeroClipboardManager.decodeRemoteClipboard(
                ZeroClipboardManager.FORMAT_PLAIN_TEXT,
                text.toByteArray(Charsets.UTF_8)
            )
        )
        assertNull(ZeroClipboardManager.decodeRemoteClipboard(1, byteArrayOf(0x01, 0x02)))
        assertNull(ZeroClipboardManager.decodeRemoteClipboard(2, byteArrayOf(0x89.toByte(), 0x50)))
        assertNull(ZeroClipboardManager.decodeRemoteClipboard(3, "file:///photo.jpg".toByteArray()))
    }

    fun testPairingSecretSurvivesStoreRecreationAndClear() {
        val appContext = instrumentation.targetContext.applicationContext
        clearProbeArtifacts(appContext)

        val suffix = UUID.randomUUID().toString().replace("-", "")
        val prefsName = "$PREFS_PREFIX$suffix"
        val keyAlias = "$KEY_ALIAS_PREFIX$suffix"
        val store = PairingSecretStore(appContext, prefsName, keyAlias)
        val secret = ByteArray(PairingSecretStore.SECRET_SIZE) { index ->
            (index * 29 + 7).toByte()
        }

        try {
            store.save(secret)
            val reloaded = PairingSecretStore(appContext, prefsName, keyAlias).load()
            val reloadedSecret = requireNotNull(reloaded) {
                "Android Keystore wrapped secret did not survive store recreation"
            }
            assertTrue("Reloaded secret differs from the saved test secret", secret.contentEquals(reloadedSecret))
            reloadedSecret.fill(0)

            store.clear()
            assertNull("Cleared pairing secret was still readable", PairingSecretStore(appContext, prefsName, keyAlias).load())
        } finally {
            secret.fill(0)
            store.clear()
            clearProbeArtifacts(appContext)
        }
    }

    fun testLostWrappingKeyFailsClosedAndCanBeForgotten() {
        val appContext = instrumentation.targetContext.applicationContext
        clearProbeArtifacts(appContext)

        val suffix = UUID.randomUUID().toString().replace("-", "")
        val prefsName = "$PREFS_PREFIX$suffix"
        val keyAlias = "$KEY_ALIAS_PREFIX$suffix"
        val store = PairingSecretStore(appContext, prefsName, keyAlias)
        val secret = ByteArray(PairingSecretStore.SECRET_SIZE) { index ->
            (index * 17 + 11).toByte()
        }

        try {
            store.save(secret)
            val keyStore = KeyStore.getInstance(PairingSecretStore.ANDROID_KEYSTORE).apply { load(null) }
            assertTrue("Test wrapping key was not created", keyStore.containsAlias(keyAlias))
            keyStore.deleteEntry(keyAlias)

            var loadFailed = false
            try {
                PairingSecretStore(appContext, prefsName, keyAlias).load()
            } catch (_: Exception) {
                loadFailed = true
            }
            assertTrue("A lost wrapping key must not accept or replace the saved pairing secret", loadFailed)

            store.clear()
            assertNull("Forgotten pairing secret was still readable", PairingSecretStore(appContext, prefsName, keyAlias).load())
        } finally {
            secret.fill(0)
            store.clear()
            clearProbeArtifacts(appContext)
        }
    }

    fun testAndroidPairingProofAcceptsOnlyExpectedSecretTranscriptAndUnpairedZeroTag() {
        val secret = ByteArray(PairingSecretStore.SECRET_SIZE) { index -> (index * 13 + 3).toByte() }
        val wrongSecret = ByteArray(PairingSecretStore.SECRET_SIZE) { index -> (index * 7 + 19).toByte() }
        val transcript = ByteArray(96) { index -> index.toByte() }
        val validTag = PairingSecretStore.authTag(secret, "client", transcript)

        assertTrue(PairingSecretStore.verifyPeerProof(secret, validTag, "client", transcript))
        assertFalse(PairingSecretStore.verifyPeerProof(wrongSecret, validTag, "client", transcript))
        assertFalse(PairingSecretStore.verifyPeerProof(secret, validTag, "server", transcript))
        assertFalse(PairingSecretStore.verifyPeerProof(secret, validTag, "client", transcript + byteArrayOf(1)))
        assertFalse(PairingSecretStore.verifyPeerProof(secret, validTag.copyOf(15), "client", transcript))
        assertTrue(PairingSecretStore.verifyPeerProof(null, ByteArray(16), "client", transcript))
        assertFalse(PairingSecretStore.verifyPeerProof(null, validTag, "client", transcript))
    }

    fun testPairingApprovalTimeoutRespondsFalse() {
        val callback = CountDownLatch(1)
        val decision = AtomicReference<Boolean?>()

        MainActivity.requestPairingApprovalStatic(
            "Synthetic timeout test peer",
            "123456",
            { approved ->
                decision.set(approved)
                callback.countDown()
            },
            timeoutMs = 25L
        )

        assertTrue("Pairing timeout did not resolve the approval callback", callback.await(3, TimeUnit.SECONDS))
        assertEquals("A timed-out pairing request must be rejected", false, decision.get())
    }

    fun testPairingApprovalDialogRejectButtonReturnsFalse() {
        val appContext = instrumentation.targetContext
        val intent = Intent(appContext, PairingApprovalDialogTestActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        val activity = instrumentation.startActivitySync(intent) as PairingApprovalDialogTestActivity
        val callback = CountDownLatch(1)
        val decision = AtomicReference<Boolean?>()
        var dialog: AlertDialog? = null

        try {
            instrumentation.runOnMainSync {
                dialog = PairingApprovalDialog.show(activity, "Synthetic reject test peer", "123456") { approved ->
                    decision.set(approved)
                    callback.countDown()
                }
            }

            val deadline = SystemClock.uptimeMillis() + 5_000L
            var rejectButton: AccessibilityNodeInfo? = null
            while (rejectButton == null && SystemClock.uptimeMillis() < deadline) {
                instrumentation.waitForIdleSync()
                val root = instrumentation.uiAutomation.rootInActiveWindow
                if (root != null) {
                    rejectButton = root.findAccessibilityNodeInfosByText("Reject")
                        .firstOrNull { it.isClickable }
                    root.recycle()
                }
                if (rejectButton == null) SystemClock.sleep(50L)
            }

            assertNotNull("Pairing approval dialog did not expose its Reject button", rejectButton)
            assertTrue(
                "Reject button could not be activated",
                rejectButton!!.performAction(AccessibilityNodeInfo.ACTION_CLICK)
            )
            assertTrue("Reject button did not resolve the approval callback", callback.await(3, TimeUnit.SECONDS))
            assertEquals("Reject must return false", false, decision.get())
        } finally {
            instrumentation.runOnMainSync {
                dialog?.dismiss()
                activity.finish()
            }
        }
    }

    fun testCachedNotificationReplyDispatchesRemoteInputToPendingIntent() {
        val appContext = instrumentation.targetContext
        val notificationId = "synthetic-${UUID.randomUUID()}"
        val pendingIntent = PendingIntent.getBroadcast(
            appContext,
            SystemClock.uptimeMillis().toInt(),
            Intent(appContext, NotificationReplyCaptureReceiver::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        )
        val remoteInput = RemoteInput.Builder(NotificationReplyCaptureReceiver.RESULT_KEY)
            .setLabel("Reply")
            .build()

        NotificationActionCache.clear()
        NotificationReplyCaptureReceiver.reset()
        try {
            NotificationActionCache.put(
                notificationId,
                CachedReplyAction(pendingIntent, arrayOf(remoteInput))
            )

            assertTrue(
                "Cached reply did not dispatch to the originating PendingIntent",
                NotificationActionCache.sendReply(appContext, notificationId, "synthetic reply")
            )
            assertTrue(
                "Originating PendingIntent did not receive the RemoteInput result",
                NotificationReplyCaptureReceiver.awaitReply(3, TimeUnit.SECONDS)
            )
            assertEquals("synthetic reply", NotificationReplyCaptureReceiver.receivedReply)
        } finally {
            NotificationActionCache.remove(notificationId)
            NotificationActionCache.clear()
            pendingIntent.cancel()
            NotificationReplyCaptureReceiver.reset()
        }
    }

    fun testCanceledNotificationReplyPendingIntentIsRemovedFromCache() {
        val appContext = instrumentation.targetContext
        val notificationId = "canceled-${UUID.randomUUID()}"
        val pendingIntent = PendingIntent.getBroadcast(
            appContext,
            SystemClock.uptimeMillis().toInt(),
            Intent(appContext, NotificationReplyCaptureReceiver::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE
        )
        val remoteInput = RemoteInput.Builder(NotificationReplyCaptureReceiver.RESULT_KEY)
            .setLabel("Reply")
            .build()
        NotificationActionCache.clear()
        try {
            NotificationActionCache.put(
                notificationId,
                CachedReplyAction(pendingIntent, arrayOf(remoteInput))
            )
            pendingIntent.cancel()

            assertFalse(
                "A canceled originating PendingIntent should not accept a reply",
                NotificationActionCache.sendReply(appContext, notificationId, "synthetic reply")
            )
            assertFalse(
                "The canceled action should be removed from the reply cache",
                NotificationActionCache.sendReply(appContext, notificationId, "synthetic reply")
            )
        } finally {
            NotificationActionCache.remove(notificationId)
            NotificationActionCache.clear()
            pendingIntent.cancel()
        }
    }

    fun testEncryptedBulkServerReturnsSyntheticMediaStoreRangeAndRejectsTampering() {
        val appContext = instrumentation.targetContext.applicationContext
        val resolver = appContext.contentResolver
        val sourceBytes = ByteArray(513) { index -> ((index * 37 + 9) % 251).toByte() }
        val imageUri = insertSyntheticImage(resolver, sourceBytes)
        val fileId = ContentUris.parseId(imageUri).toString()
        val token = ByteArray(32) { index -> (index * 5 + 1).toByte() }
        val tokenHex = token.joinToString("") { "%02x".format(it.toInt() and 0xff) }
        val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        val manager = EphemeralWifiManager(appContext, scope)
        val port = ServerSocket(0).use { it.localPort }
        val ready = CountDownLatch(1)
        val offset = 400L
        val requestedLength = 256
        val idBytes = fileId.toByteArray(Charsets.UTF_8)
        val requestBody = ByteBuffer.allocate(1 + 2 + idBytes.size + 8 + 4)
            .order(ByteOrder.BIG_ENDIAN)
            .put(1)
            .putShort(idBytes.size.toShort())
            .put(idBytes)
            .putLong(offset)
            .putInt(requestedLength)
            .array()
        var keys: ByteArray? = null
        var deletedRows = 0

        try {
            manager.wakeAndListen(port, tokenHex) { _, _ -> ready.countDown() }
            assertTrue("Bulk server did not become ready", ready.await(10, TimeUnit.SECONDS))

            Socket("127.0.0.1", port).use { socket ->
                socket.soTimeout = 10_000
                val input = DataInputStream(socket.getInputStream())
                val output = DataOutputStream(socket.getOutputStream())
                val challenge = ByteArray(32)
                input.readFully(challenge)
                keys = requireNotNull(ZeroLinkNative.deriveWifiBulkKeys(token, challenge))

                val requestNonce = ByteArray(12).also(SecureRandom()::nextBytes)
                val encryptedRequest = cryptBulk(
                    Cipher.ENCRYPT_MODE,
                    keys!!.copyOfRange(0, 32),
                    requestNonce,
                    "ZeroLink-v1-wifi-bulk-request".toByteArray(Charsets.UTF_8) + challenge,
                    requestBody
                )
                output.write(requestNonce)
                output.writeInt(encryptedRequest.size)
                output.write(encryptedRequest)
                output.flush()

                assertEquals("Android rejected a valid authenticated range", 0, input.readInt())
                val responseNonce = ByteArray(12)
                input.readFully(responseNonce)
                val encryptedResponseLength = input.readInt()
                assertTrue(encryptedResponseLength in 28..(requestedLength + 28))
                val encryptedResponse = ByteArray(encryptedResponseLength)
                input.readFully(encryptedResponse)
                val responseBody = cryptBulk(
                    Cipher.DECRYPT_MODE,
                    keys!!.copyOfRange(32, 64),
                    responseNonce,
                    "ZeroLink-v1-wifi-bulk-response".toByteArray(Charsets.UTF_8) + challenge + requestBody,
                    encryptedResponse
                )
                val response = ByteBuffer.wrap(responseBody).order(ByteOrder.BIG_ENDIAN)
                val reportedFileSize = response.long
                val returnedLength = response.int
                assertEquals(sourceBytes.size.toLong(), reportedFileSize)
                assertEquals(sourceBytes.size - offset.toInt(), returnedLength)
                assertEquals(returnedLength, response.remaining())
                val returnedBytes = ByteArray(returnedLength)
                response.get(returnedBytes)
                assertTrue(
                    "Android returned bytes that differ from the synthetic MediaStore row",
                    sourceBytes.copyOfRange(offset.toInt(), sourceBytes.size).contentEquals(returnedBytes)
                )
            }

            keys?.fill(0)
            keys = null
            Socket("127.0.0.1", port).use { socket ->
                socket.soTimeout = 10_000
                val input = DataInputStream(socket.getInputStream())
                val output = DataOutputStream(socket.getOutputStream())
                val challenge = ByteArray(32)
                input.readFully(challenge)
                val tamperKeys = requireNotNull(ZeroLinkNative.deriveWifiBulkKeys(token, challenge))
                try {
                    val requestNonce = ByteArray(12).also(SecureRandom()::nextBytes)
                    val encryptedRequest = cryptBulk(
                        Cipher.ENCRYPT_MODE,
                        tamperKeys.copyOfRange(0, 32),
                        requestNonce,
                        "ZeroLink-v1-wifi-bulk-request".toByteArray(Charsets.UTF_8) + challenge,
                        requestBody
                    )
                    encryptedRequest[encryptedRequest.lastIndex] = (encryptedRequest.last().toInt() xor 1).toByte()
                    output.write(requestNonce)
                    output.writeInt(encryptedRequest.size)
                    output.write(encryptedRequest)
                    output.flush()
                    assertEquals("Android accepted an unauthenticated range request", -1, input.readInt())
                } finally {
                    tamperKeys.fill(0)
                }
            }
        } finally {
            keys?.fill(0)
            token.fill(0)
            manager.teardown()
            scope.cancel()
            deletedRows = resolver.delete(imageUri, null, null)
        }
        assertEquals("Synthetic MediaStore row was not removed after the transfer test", 1, deletedRows)
    }

    private fun insertSyntheticImage(resolver: android.content.ContentResolver, bytes: ByteArray): android.net.Uri {
        val values = ContentValues().apply {
            put(MediaStore.Images.Media.DISPLAY_NAME, "zerolink-range-${UUID.randomUUID()}.jpg")
            put(MediaStore.Images.Media.MIME_TYPE, "image/jpeg")
            put(MediaStore.Images.Media.RELATIVE_PATH, Environment.DIRECTORY_PICTURES)
            put(MediaStore.Images.Media.IS_PENDING, 1)
        }
        val uri = requireNotNull(resolver.insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, values)) {
            "Could not create synthetic MediaStore row"
        }
        try {
            requireNotNull(resolver.openOutputStream(uri, "w")).use { it.write(bytes) }
            val publish = ContentValues().apply { put(MediaStore.Images.Media.IS_PENDING, 0) }
            resolver.update(uri, publish, null, null)
            return uri
        } catch (error: Exception) {
            resolver.delete(uri, null, null)
            throw error
        }
    }

    private fun cryptBulk(mode: Int, key: ByteArray, nonce: ByteArray, aad: ByteArray, data: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(mode, SecretKeySpec(key, "AES"), GCMParameterSpec(128, nonce))
        cipher.updateAAD(aad)
        return cipher.doFinal(data)
    }

    private fun clearProbeArtifacts(context: Context) {
        val keyStore = KeyStore.getInstance(PairingSecretStore.ANDROID_KEYSTORE).apply { load(null) }
        val aliases = keyStore.aliases()
        while (aliases.hasMoreElements()) {
            val alias = aliases.nextElement()
            if (alias.startsWith(KEY_ALIAS_PREFIX)) keyStore.deleteEntry(alias)
        }

        val prefsDirectory = File(context.applicationInfo.dataDir, "shared_prefs")
        prefsDirectory.listFiles()
            ?.filter { it.name.startsWith(PREFS_PREFIX) && it.name.endsWith(".xml") }
            ?.forEach { file ->
                val prefsName = file.name.removeSuffix(".xml")
                context.getSharedPreferences(prefsName, Context.MODE_PRIVATE).edit().clear().commit()
                file.delete()
            }
    }

    companion object {
        private const val PREFS_PREFIX = "zerolink_pairing_probe_"
        private const val KEY_ALIAS_PREFIX = "zerolink.pairing.probe."
    }
}
