use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm,
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};

use crate::error::{Result, ZeroLinkError};

pub const KEY_SIZE: usize = 32;
pub const NONCE_SIZE: usize = 12;
pub const TAG_SIZE: usize = 16;
pub const SALT_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Initiator,
    Responder,
}

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
        role: SessionRole,
    ) -> Result<SessionCipher> {
        let secret = self.secret.take().ok_or(ZeroLinkError::KeyExchangeFailed)?;
        let peer_point = X25519PublicKey::from(*peer_public_key);
        let shared_secret = secret.diffie_hellman(&peer_point);

        let hk = Hkdf::<Sha256>::new(Some(salt), shared_secret.as_bytes());

        let mut okm = [0u8; 2 * (KEY_SIZE + NONCE_SIZE) + 2 * KEY_SIZE];
        hk.expand(b"ZeroLink-v3-AEAD-Session-Keys", &mut okm)
            .map_err(|_| ZeroLinkError::KeyExchangeFailed)?;

        let mut initiator_key = [0u8; KEY_SIZE];
        let mut initiator_iv = [0u8; NONCE_SIZE];
        let mut responder_key = [0u8; KEY_SIZE];
        let mut responder_iv = [0u8; NONCE_SIZE];
        let mut confirmation_key = [0u8; KEY_SIZE];
        let mut pairing_secret = [0u8; KEY_SIZE];
        initiator_key.copy_from_slice(&okm[..KEY_SIZE]);
        initiator_iv.copy_from_slice(&okm[KEY_SIZE..KEY_SIZE + NONCE_SIZE]);
        let responder_offset = KEY_SIZE + NONCE_SIZE;
        responder_key.copy_from_slice(&okm[responder_offset..responder_offset + KEY_SIZE]);
        responder_iv.copy_from_slice(
            &okm[responder_offset + KEY_SIZE..responder_offset + KEY_SIZE + NONCE_SIZE],
        );
        let key_material_offset = 2 * (KEY_SIZE + NONCE_SIZE);
        confirmation_key.copy_from_slice(&okm[key_material_offset..key_material_offset + KEY_SIZE]);
        pairing_secret.copy_from_slice(&okm[key_material_offset + KEY_SIZE..]);

        let (tx_key, tx_iv, rx_key, rx_iv) = match role {
            SessionRole::Initiator => (initiator_key, initiator_iv, responder_key, responder_iv),
            SessionRole::Responder => (responder_key, responder_iv, initiator_key, initiator_iv),
        };
        Ok(SessionCipher::new(
            tx_key,
            tx_iv,
            rx_key,
            rx_iv,
            confirmation_key,
            pairing_secret,
        ))
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

/// Derives independent request and response AES-GCM keys for one ephemeral Wi-Fi transfer.
/// The token is delivered over the paired encrypted channel; the server challenge is fresh per socket.
pub fn derive_wifi_bulk_keys(token: &[u8; 32], challenge: &[u8; 32]) -> [u8; 64] {
    let hk = Hkdf::<Sha256>::new(Some(challenge), token);
    let mut keys = [0u8; 64];
    hk.expand(b"ZeroLink-v1-wifi-bulk-transfer", &mut keys)
        .expect("64-byte Wi-Fi transfer key output is valid");
    keys
}

fn wifi_bulk_request_aad(challenge: &[u8; 32]) -> Vec<u8> {
    [b"ZeroLink-v1-wifi-bulk-request".as_slice(), challenge].concat()
}

fn wifi_bulk_response_aad(challenge: &[u8; 32], request_body: &[u8]) -> Vec<u8> {
    [
        b"ZeroLink-v1-wifi-bulk-response".as_slice(),
        challenge,
        request_body,
    ]
    .concat()
}

pub fn encrypt_wifi_bulk_request(
    keys: &[u8; 64],
    challenge: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    request_body: &[u8],
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&keys[..32]).expect("key has 32 bytes");
    cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: request_body,
                aad: &wifi_bulk_request_aad(challenge),
            },
        )
        .map_err(|_| ZeroLinkError::EncryptionFailed)
}

pub fn decrypt_wifi_bulk_request(
    keys: &[u8; 64],
    challenge: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&keys[..32]).expect("key has 32 bytes");
    cipher
        .decrypt(
            aes_gcm::Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: ciphertext,
                aad: &wifi_bulk_request_aad(challenge),
            },
        )
        .map_err(|_| ZeroLinkError::AuthenticationFailed)
}

pub fn encrypt_wifi_bulk_response(
    keys: &[u8; 64],
    challenge: &[u8; 32],
    request_body: &[u8],
    nonce: &[u8; NONCE_SIZE],
    response_body: &[u8],
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&keys[32..]).expect("key has 32 bytes");
    cipher
        .encrypt(
            aes_gcm::Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: response_body,
                aad: &wifi_bulk_response_aad(challenge, request_body),
            },
        )
        .map_err(|_| ZeroLinkError::EncryptionFailed)
}

pub fn decrypt_wifi_bulk_response(
    keys: &[u8; 64],
    challenge: &[u8; 32],
    request_body: &[u8],
    nonce: &[u8; NONCE_SIZE],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&keys[32..]).expect("key has 32 bytes");
    cipher
        .decrypt(
            aes_gcm::Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: ciphertext,
                aad: &wifi_bulk_response_aad(challenge, request_body),
            },
        )
        .map_err(|_| ZeroLinkError::AuthenticationFailed)
}

/// Builds a six-digit short authentication string from the ordered v3 pairing transcript.
/// Both peers must display and compare this during first pairing; it is not an identity proof.
pub fn pairing_code(
    client_public_key: &[u8; KEY_SIZE],
    salt: &[u8; SALT_SIZE],
    server_public_key: &[u8; KEY_SIZE],
) -> String {
    use sha2::Digest;

    let mut digest = Sha256::new();
    digest.update(b"ZeroLink-v3-pairing-code");
    digest.update(client_public_key);
    digest.update(salt);
    digest.update(server_public_key);
    let hash = digest.finalize();
    let value = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    format!("{value:06}")
}

/// Stateful authenticated encryption/decryption channel.
/// Manages monotonic packet sequence counters to strictly mitigate replay attacks.
pub struct SessionCipher {
    tx_cipher: Aes256Gcm,
    tx_iv_base: [u8; NONCE_SIZE],
    rx_cipher: Aes256Gcm,
    rx_iv_base: [u8; NONCE_SIZE],
    tx_sequence: u64,
    last_rx_sequence: u64,
    confirmation_key: [u8; KEY_SIZE],
    pairing_secret: [u8; KEY_SIZE],
}

impl SessionCipher {
    pub fn new(
        tx_key: [u8; KEY_SIZE],
        tx_iv: [u8; NONCE_SIZE],
        rx_key: [u8; KEY_SIZE],
        rx_iv: [u8; NONCE_SIZE],
        confirmation_key: [u8; KEY_SIZE],
        pairing_secret: [u8; KEY_SIZE],
    ) -> Self {
        Self {
            tx_cipher: Aes256Gcm::new_from_slice(&tx_key).expect("Key size is valid 256 bits"),
            tx_iv_base: tx_iv,
            rx_cipher: Aes256Gcm::new_from_slice(&rx_key).expect("Key size is valid 256 bits"),
            rx_iv_base: rx_iv,
            tx_sequence: 0,
            last_rx_sequence: 0,
            confirmation_key,
            pairing_secret,
        }
    }

    /// Returns the stable pair secret for storage after the users approve a matching code.
    pub fn pairing_secret(&self) -> [u8; KEY_SIZE] {
        self.pairing_secret
    }

    pub fn pairing_auth_tag(&self, label: &[u8], transcript: &[u8]) -> [u8; 16] {
        pairing_auth_tag(&self.pairing_secret, label, transcript)
    }

    /// Produces a 16-byte HMAC key-confirmation tag for a handshake transcript.
    /// This confirms shared-key possession; it does not authenticate device identity.
    pub fn key_confirmation_tag(&self, transcript: &[u8]) -> [u8; 16] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.confirmation_key)
            .expect("HMAC accepts keys of any length");
        mac.update(transcript);
        let digest = mac.finalize().into_bytes();
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&digest[..16]);
        tag
    }

    /// Derives a deterministic 12-byte nonce for a specific 64-bit sequence counter.
    /// Nonce = IV_Base ^ SequenceNumber (little-endian padded).
    fn derive_nonce(
        iv_base: &[u8; NONCE_SIZE],
        seq: u64,
    ) -> aes_gcm::Nonce<aes_gcm::aead::consts::U12> {
        let mut nonce_bytes = *iv_base;
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
        let nonce = Self::derive_nonce(&self.tx_iv_base, seq);

        let payload = aes_gcm::aead::Payload {
            msg: plaintext,
            aad: associated_data,
        };

        let ciphertext = self
            .tx_cipher
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

        let nonce = Self::derive_nonce(&self.rx_iv_base, sequence);
        let payload = aes_gcm::aead::Payload {
            msg: ciphertext,
            aad: associated_data,
        };

        let plaintext = self
            .rx_cipher
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

/// Produces a truncated, role-labeled proof under a secret established by first pairing.
pub fn pairing_auth_tag(secret: &[u8; KEY_SIZE], label: &[u8], transcript: &[u8]) -> [u8; 16] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(b"ZeroLink-v3-pairing-proof");
    mac.update(label);
    mac.update(transcript);
    let digest = mac.finalize().into_bytes();
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&digest[..16]);
    tag
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

        let mut alice_cipher = alice
            .complete_handshake(&bob_pub, &salt, SessionRole::Initiator)
            .unwrap();
        let mut bob_cipher = bob
            .complete_handshake(&alice_pub, &salt, SessionRole::Responder)
            .unwrap();
        assert_eq!(alice_cipher.pairing_secret(), bob_cipher.pairing_secret());

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

    #[test]
    fn pairing_code_is_ordered_and_six_digits() {
        let client = [0x11; 32];
        let salt = [0x22; 32];
        let server = [0x33; 32];
        let code = pairing_code(&client, &salt, &server);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(code, pairing_code(&client, &salt, &server));
        assert_ne!(code, pairing_code(&server, &salt, &client));
    }

    #[test]
    fn wifi_bulk_transfer_uses_challenge_bound_authenticated_encryption() {
        let token = [0x51; 32];
        let challenge = [0xA6; 32];
        let other_challenge = [0xA7; 32];
        let keys = derive_wifi_bulk_keys(&token, &challenge);
        let request_body = b"version=1;file=123;offset=4096;length=65536";
        let request_nonce = [0x11; NONCE_SIZE];

        let encrypted_request =
            encrypt_wifi_bulk_request(&keys, &challenge, &request_nonce, request_body).unwrap();
        assert_eq!(
            decrypt_wifi_bulk_request(&keys, &challenge, &request_nonce, &encrypted_request)
                .unwrap(),
            request_body
        );
        assert_eq!(
            decrypt_wifi_bulk_request(&keys, &other_challenge, &request_nonce, &encrypted_request),
            Err(ZeroLinkError::AuthenticationFailed)
        );
        assert_ne!(keys, derive_wifi_bulk_keys(&token, &other_challenge));

        let response_body = b"encrypted photo bytes";
        let response_nonce = [0x22; NONCE_SIZE];
        let encrypted_response = encrypt_wifi_bulk_response(
            &keys,
            &challenge,
            request_body,
            &response_nonce,
            response_body,
        )
        .unwrap();
        assert_eq!(
            decrypt_wifi_bulk_response(
                &keys,
                &challenge,
                request_body,
                &response_nonce,
                &encrypted_response,
            )
            .unwrap(),
            response_body
        );

        let mut modified_response = encrypted_response;
        modified_response[0] ^= 1;
        assert_eq!(
            decrypt_wifi_bulk_response(
                &keys,
                &challenge,
                request_body,
                &response_nonce,
                &modified_response,
            ),
            Err(ZeroLinkError::AuthenticationFailed)
        );
    }
}
