package org.zerolink

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import java.security.MessageDigest
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec
import javax.crypto.spec.GCMParameterSpec

/** Stores the paired-device secret encrypted by a non-exportable Android Keystore AES key. */
class PairingSecretStore(
    context: Context,
    prefsName: String = PREFS,
    private val keyAlias: String = KEY_ALIAS,
) {
    private val prefs = context.applicationContext.getSharedPreferences(prefsName, Context.MODE_PRIVATE)

    @Synchronized
    fun load(): ByteArray? {
        val encodedIv = prefs.getString(KEY_IV, null) ?: return null
        val encodedCiphertext = prefs.getString(KEY_CIPHERTEXT, null)
            ?: error("Paired-device secret storage is incomplete; explicitly forget and pair again")
        val iv = Base64.decode(encodedIv, Base64.NO_WRAP)
        val ciphertext = Base64.decode(encodedCiphertext, Base64.NO_WRAP)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, loadWrappingKey(), GCMParameterSpec(128, iv))
        cipher.updateAAD(AAD)
        val secret = cipher.doFinal(ciphertext)
        require(secret.size == SECRET_SIZE) { "Stored paired-device secret has an invalid length" }
        return secret
    }

    @Synchronized
    fun save(secret: ByteArray) {
        require(secret.size == SECRET_SIZE) { "Paired-device secret must be 32 bytes" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, loadWrappingKey())
        cipher.updateAAD(AAD)
        val ciphertext = cipher.doFinal(secret)
        check(prefs.edit()
            .putString(KEY_IV, Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
            .putString(KEY_CIPHERTEXT, Base64.encodeToString(ciphertext, Base64.NO_WRAP))
            .commit()) { "Could not persist paired-device secret" }
    }

    @Synchronized
    fun clear() {
        check(prefs.edit().remove(KEY_IV).remove(KEY_CIPHERTEXT).commit()) {
            "Could not remove paired-device secret"
        }
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        keyStore.deleteEntry(keyAlias)
    }

    private fun loadWrappingKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(keyAlias, null) as? SecretKey)?.let { return it }

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                keyAlias,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setRandomizedEncryptionRequired(true)
                .build()
        )
        return generator.generateKey()
    }

    companion object {
        const val PREFS = "zerolink_pairing"
        const val KEY_ALIAS = "zerolink.pairing.wrap.v1"
        const val KEY_IV = "paired_secret_iv"
        const val KEY_CIPHERTEXT = "paired_secret_ciphertext"
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val SECRET_SIZE = 32
        val AAD = "ZeroLink paired-device secret v1".toByteArray(Charsets.UTF_8)

        fun authTag(secret: ByteArray, label: String, transcript: ByteArray): ByteArray {
            require(secret.size == 32)
            val mac = Mac.getInstance("HmacSHA256")
            mac.init(SecretKeySpec(secret, "HmacSHA256"))
            mac.update("ZeroLink-v3-pairing-proof".toByteArray(Charsets.UTF_8))
            mac.update(label.toByteArray(Charsets.UTF_8))
            return mac.doFinal(transcript).copyOfRange(0, 16)
        }

        fun verifyPeerProof(
            storedSecret: ByteArray?,
            presentedTag: ByteArray,
            label: String,
            transcript: ByteArray,
        ): Boolean {
            if (presentedTag.size != 16) return false
            val expectedTag = storedSecret?.let { authTag(it, label, transcript) } ?: ByteArray(16)
            return MessageDigest.isEqual(expectedTag, presentedTag)
        }
    }
}
