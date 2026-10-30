use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use zerolink_core::crypto::{generate_salt, KeyExchange, SessionCipher};
use zerolink_core::error::{Result, ZeroLinkError};
use zerolink_core::framing::{Frame, PacketType};
use zerolink_core::protocol::{HandshakeInitPayload, HandshakeReplyPayload};

pub struct BleL2capClient {
    device_address: u64,
    psm: u16,
    cipher: Arc<Mutex<Option<SessionCipher>>>,
}

impl BleL2capClient {
    pub fn new(device_address: u64, psm: u16) -> Self {
        Self {
            device_address,
            psm,
            cipher: Arc::new(Mutex::new(None)),
        }
    }

    pub fn device_address(&self) -> u64 {
        self.device_address
    }

    pub fn psm(&self) -> u16 {
        self.psm
    }

    /// Executes Curve25519 ECDH secure handshake with the Android L2CAP server.
    pub async fn perform_handshake(
        &self,
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
        handshake_reply_bytes: &[u8],
    ) -> Result<()> {
        let kx = KeyExchange::new();
        let salt = generate_salt();

        let init = HandshakeInitPayload {
            client_ephemeral_pubkey: kx.public_key,
            salt,
            protocol_version: 1,
            client_device_name: "ZeroLink-Windows-Daemon".to_string(),
        };

        let init_bytes = init.serialize();
        let frame = Frame::new(PacketType::Handshake, 0, init_bytes)?;
        raw_io_tx
            .send(frame.serialize())
            .map_err(|_| ZeroLinkError::ConnectionError("Failed to dispatch HandshakeInit frame".into()))?;

        // Process HandshakeReply
        let reply = HandshakeReplyPayload::deserialize(handshake_reply_bytes)?;
        let session_cipher = kx.complete_handshake(&reply.server_ephemeral_pubkey, &salt)?;

        let mut lock = self.cipher.lock().await;
        *lock = Some(session_cipher);

        tracing::info!(
            "Secure Curve25519 session established with Android device: {} (PSM: 0x{:04x})",
            reply.server_device_name,
            self.psm
        );
        Ok(())
    }

    /// Encrypts and frames a payload for low-power L2CAP transport.
    pub async fn send_packet(
        &self,
        packet_type: PacketType,
        raw_data: &[u8],
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        let mut lock = self.cipher.lock().await;
        let cipher = lock
            .as_mut()
            .ok_or(ZeroLinkError::AuthenticationFailed)?;

        let (seq, ciphertext) = cipher.encrypt(raw_data, &[packet_type as u8])?;
        let frame = Frame::new(packet_type, seq, ciphertext)?;

        raw_io_tx
            .send(frame.serialize())
            .map_err(|_| ZeroLinkError::ConnectionError("Failed to transmit framed packet".into()))?;

        Ok(())
    }

    /// Decrypts an incoming framed packet received from Android L2CAP CoC.
    pub async fn receive_frame(&self, frame: &Frame) -> Result<Vec<u8>> {
        let mut lock = self.cipher.lock().await;
        let cipher = lock
            .as_mut()
            .ok_or(ZeroLinkError::AuthenticationFailed)?;

        cipher.decrypt(
            frame.header.sequence,
            &frame.payload,
            &[frame.header.packet_type as u8],
        )
    }

    /// Indicates whether a secure encrypted session is currently active.
    pub async fn is_session_active(&self) -> bool {
        let lock = self.cipher.lock().await;
        lock.is_some()
    }
}
