use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm,
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};

use crate::error::{Result, ZeroLinkError};

pub const KEY_SIZE: usize = 32;
pub const NONCE_SIZE: usize = 12;
pub const TAG_SIZE: usize = 16;
pub const SALT_SIZE: usize = 32;

/// Ephemeral key exchange manager for initial Curve25519 handshake.
pub struct KeyExchange {
    secret: Option<EphemeralSecret>,
    pub public_key: [u8; KEY_SIZE],
}

impl KeyExchange {
    /// Generates a new ephemeral Curve25519 keypair using OS entropy.
    pub fn new() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public_key = X25519PublicKey::from(&secret);
        Self {
            secret: Some(secret),
            public_key: *public_key.as_bytes(),
        }
    }

    /// Computes the Diffie-Hellman shared secret and derives session keys using HKDF-SHA256.
    pub fn complete_handshake(
        mut self,
        peer_public_key: &[u8; KEY_SIZE],
        salt: &[u8; SALT_SIZE],
    ) -> Result<SessionCipher> {
        let secret = self.secret.take().ok_or(ZeroLinkError::KeyExchangeFailed)?;
        let peer_point = X25519PublicKey::from(*peer_public_key);
        let shared_secret = secret.diffie_hellman(&peer_point);

        let hk = Hkdf::<Sha256>::new(Some(salt), shared_secret.as_bytes());

        let mut okm = [0u8; KEY_SIZE + NONCE_SIZE];
        hk.expand(b"ZeroLink-v1-AEAD-Session-Keys", &mut okm)
            .map_err(|_| ZeroLinkError::KeyExchangeFailed)?;

        let mut session_key = [0u8; KEY_SIZE];
        let mut iv_base = [0u8; NONCE_SIZE];
        session_key.copy_from_slice(&okm[..KEY_SIZE]);
        iv_base.copy_from_slice(&okm[KEY_SIZE..]);

        Ok(SessionCipher::new(session_key, iv_base))
    }
}

impl Default for KeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

/// Generates a cryptographically secure 32-byte salt for HKDF.
pub fn generate_salt() -> [u8; SALT_SIZE] {
    let mut salt = [0u8; SALT_SIZE];
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Stateful authenticated encryption/decryption channel.
/// Manages monotonic packet sequence counters to strictly mitigate replay attacks.
pub struct SessionCipher {
    cipher: Aes256Gcm,
    iv_base: [u8; NONCE_SIZE],
    tx_sequence: u64,
    last_rx_sequence: u64,
}

impl SessionCipher {
    pub fn new(key: [u8; KEY_SIZE], iv_base: [u8; NONCE_SIZE]) -> Self {
        let cipher = Aes256Gcm::new_from_slice(&key).expect("Key size is valid 256 bits");
        Self {
            cipher,
            iv_base,
            tx_sequence: 0,
            last_rx_sequence: 0,
        }
    }

    /// Derives a deterministic 12-byte nonce for a specific 64-bit sequence counter.
    /// Nonce = IV_Base ^ SequenceNumber (little-endian padded).
    fn derive_nonce(&self, seq: u64) -> aes_gcm::Nonce<aes_gcm::aead::consts::U12> {
        let mut nonce_bytes = self.iv_base;
        let seq_bytes = seq.to_le_bytes();
        for (i, b) in seq_bytes.iter().enumerate() {
            nonce_bytes[i] ^= b;
        }
        *aes_gcm::Nonce::<aes_gcm::aead::consts::U12>::from_slice(&nonce_bytes)
    }
    /// Encrypts plaintext payload in-place or returns ciphertext with appended GCM tag.
    /// Increments monotonic TX counter.
    pub fn encrypt(&mut self, plaintext: &[u8], associated_data: &[u8]) -> Result<(u64, Vec<u8>)> {
        if self.tx_sequence == u64::MAX {
            return Err(ZeroLinkError::EncryptionFailed);
        }
        self.tx_sequence += 1;
        let seq = self.tx_sequence;
        let nonce = self.derive_nonce(seq);

        let payload = aes_gcm::aead::Payload {
            msg: plaintext,
            aad: associated_data,
        };

        let ciphertext = self
            .cipher
            .encrypt(&nonce, payload)
            .map_err(|_| ZeroLinkError::EncryptionFailed)?;

        Ok((seq, ciphertext))
    }

    /// Decrypts ciphertext and validates monotonic sequence counter against replay attacks.
    pub fn decrypt(
        &mut self,
        sequence: u64,
        ciphertext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>> {
        if sequence <= self.last_rx_sequence {
            return Err(ZeroLinkError::SequenceReplay {
                received: sequence,
                last_seen: self.last_rx_sequence,
            });
        }

        let nonce = self.derive_nonce(sequence);
        let payload = aes_gcm::aead::Payload {
            msg: ciphertext,
            aad: associated_data,
        };

        let plaintext = self
            .cipher
            .decrypt(&nonce, payload)
            .map_err(|_| ZeroLinkError::AuthenticationFailed)?;

        self.last_rx_sequence = sequence;
        Ok(plaintext)
    }

    pub fn tx_sequence(&self) -> u64 {
        self.tx_sequence
    }

    pub fn last_rx_sequence(&self) -> u64 {
        self.last_rx_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ecdh_and_session_cipher_roundtrip() {
        let alice = KeyExchange::new();
        let bob = KeyExchange::new();
        let salt = generate_salt();

        let alice_pub = alice.public_key;
        let bob_pub = bob.public_key;

        let mut alice_cipher = alice.complete_handshake(&bob_pub, &salt).unwrap();
        let mut bob_cipher = bob.complete_handshake(&alice_pub, &salt).unwrap();

        let message = b"ZeroLink ultra-low-power frame payload";
        let aad = b"HeaderAssociatedData";

        let (seq, ciphertext) = alice_cipher.encrypt(message, aad).unwrap();
        assert_eq!(seq, 1);

        let decrypted = bob_cipher.decrypt(seq, &ciphertext, aad).unwrap();
        assert_eq!(decrypted, message);

        // Anti-replay test
        let replay_err = bob_cipher.decrypt(seq, &ciphertext, aad);
        assert!(matches!(
            replay_err,
            Err(ZeroLinkError::SequenceReplay { .. })
        ));
    }
}
