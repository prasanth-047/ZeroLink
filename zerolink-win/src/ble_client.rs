use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use windows::core::HSTRING;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
#[cfg(not(test))]
use windows::Win32::Foundation::HWND;
#[cfg(not(test))]
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
};
#[cfg(not(test))]
use zerolink_core::crypto::pairing_code;
use zerolink_core::crypto::{
    generate_salt, pairing_auth_tag, KeyExchange, SessionCipher, SessionRole,
};
use zerolink_core::error::{Result, ZeroLinkError};
use zerolink_core::framing::{Frame, PacketType};
use zerolink_core::protocol::{
    HandshakeInitPayload, HandshakeReplyPayload, HANDSHAKE_PROTOCOL_VERSION,
};

// Service UUID for ZeroLink BLE L2CAP advertisement: 0000FE2C-0000-1000-8000-00805F9B34FB
const ZEROLINK_UUID_16: u16 = 0xFE2C;
type PendingHandshake = Arc<Mutex<Option<(KeyExchange, [u8; 32])>>>;

pub struct BleL2capClient {
    device_address: Arc<Mutex<u64>>,
    psm: Arc<Mutex<u16>>,
    cipher: Arc<Mutex<Option<SessionCipher>>>,
    pending_handshake: PendingHandshake,
    pairing_secret: Arc<Mutex<Option<[u8; 32]>>>,
    pairing_store_error: Option<String>,
    watcher: Arc<Mutex<Option<BluetoothLEAdvertisementWatcher>>>,
}

#[allow(dead_code)]
impl BleL2capClient {
    pub fn new(device_address: u64, psm: u16) -> Self {
        let (pairing_secret, pairing_store_error) =
            match crate::pairing_store::load_pairing_secret() {
                Ok(Some(secret)) if secret.len() == 32 => {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&secret);
                    (Some(key), None)
                }
                Ok(None) => (None, None),
                Ok(Some(_)) => (
                    None,
                    Some("Stored pairing secret has invalid size".to_string()),
                ),
                Err(error) => (None, Some(error)),
            };
        Self {
            device_address: Arc::new(Mutex::new(device_address)),
            psm: Arc::new(Mutex::new(psm)),
            cipher: Arc::new(Mutex::new(None)),
            pending_handshake: Arc::new(Mutex::new(None)),
            pairing_secret: Arc::new(Mutex::new(pairing_secret)),
            pairing_store_error,
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn device_address(&self) -> u64 {
        *self.device_address.lock().await
    }

    pub async fn psm(&self) -> u16 {
        *self.psm.lock().await
    }

    pub async fn update_device_target(&self, address: u64, psm: u16) {
        *self.device_address.lock().await = address;
        *self.psm.lock().await = psm;
    }

    /// Starts WinRT BLE Advertisement Watcher to automatically discover nearby Android devices
    /// advertising ZeroLink service (UUID 0xFE2C) with assigned dynamic L2CAP PSM.
    pub async fn start_advertisement_watcher(
        &self,
        discovery_tx: mpsc::UnboundedSender<(u64, u16, String)>,
    ) -> std::result::Result<(), String> {
        let watcher = BluetoothLEAdvertisementWatcher::new().map_err(|e| e.to_string())?;

        let _ = watcher.SetScanningMode(BluetoothLEScanningMode::Active);

        let tx = discovery_tx.clone();
        watcher
            .Received(&TypedEventHandler::new(
                move |_sender, args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                    if let Some(args) = args {
                        let address = args.BluetoothAddress().unwrap_or(0);
                        if let Ok(ad) = args.Advertisement() {
                            let name = ad.LocalName().unwrap_or(HSTRING::new()).to_string();

                            // Check service UUIDs for 0xFE2C
                            let mut is_zerolink = false;
                            if let Ok(uuids) = ad.ServiceUuids() {
                                for uuid in uuids {
                                    // 0000FE2C-0000-1000-8000-00805F9B34FB
                                    if uuid.data1 == 0x0000FE2C {
                                        is_zerolink = true;
                                        break;
                                    }
                                }
                            }

                            // Check service data for advertised PSM
                            let mut psm: u16 = 0x1001; // Default operational PSM fallback
                            if let Ok(data_sections) = ad.DataSections() {
                                for section in data_sections {
                                    if let Ok(buf) = section.Data() {
                                        let len = buf.Length().unwrap_or(0) as usize;
                                        if len >= 4 {
                                            // Service data format: 2-byte UUID + 2-byte PSM
                                            let reader =
                                                windows::Storage::Streams::DataReader::FromBuffer(
                                                    &buf,
                                                );
                                            if let Ok(reader) = reader {
                                                if let Ok(uuid_part) = reader.ReadUInt16() {
                                                    if uuid_part == ZEROLINK_UUID_16 {
                                                        is_zerolink = true;
                                                        if let Ok(psm_val) = reader.ReadUInt16() {
                                                            psm = psm_val;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            if is_zerolink || name.contains("ZeroLink") {
                                tracing::info!(
                                    "Discovered ZeroLink Android device: {} (addr: 0x{:012X}, PSM: 0x{:04X})",
                                    name,
                                    address,
                                    psm
                                );
                                let _ = tx.send((address, psm, name));
                            }
                        }
                    }
                    Ok(())
                },
            ))
            .map_err(|e| e.to_string())?;

        watcher.Start().map_err(|e| e.to_string())?;
        *self.watcher.lock().await = Some(watcher);
        tracing::info!(
            "WinRT BLE Advertisement Watcher active (Scanning for ZeroLink service 0xFE2C)"
        );
        Ok(())
    }

    /// Stops the WinRT BLE Advertisement Watcher.
    pub async fn stop_advertisement_watcher(&self) {
        let mut lock = self.watcher.lock().await;
        if let Some(watcher) = lock.take() {
            let _ = watcher.Stop();
            tracing::info!("WinRT BLE Advertisement Watcher stopped");
        }
    }

    /// Initiates the Curve25519 ECDH secure handshake by transmitting HandshakeInit.
    pub async fn initiate_handshake(
        &self,
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        if let Some(error) = &self.pairing_store_error {
            return Err(ZeroLinkError::ConnectionError(format!(
                "Cannot load trusted peer state: {error}"
            )));
        }
        let kx = KeyExchange::new();
        let salt = generate_salt();
        let client_pairing_transcript = [kx.public_key.as_slice(), salt.as_slice()].concat();
        let pairing_auth_tag = self
            .pairing_secret
            .lock()
            .await
            .as_ref()
            .map(|secret| pairing_auth_tag(secret, b"client", &client_pairing_transcript))
            .unwrap_or([0u8; 16]);

        let init = HandshakeInitPayload {
            client_ephemeral_pubkey: kx.public_key,
            salt,
            protocol_version: HANDSHAKE_PROTOCOL_VERSION,
            client_device_name: "ZeroLink-Windows-Daemon".to_string(),
            pairing_auth_tag,
        };

        let init_bytes = init.serialize();
        let frame = Frame::new(PacketType::Handshake, 0, init_bytes)?;
        raw_io_tx.send(frame.serialize()).map_err(|_| {
            ZeroLinkError::ConnectionError("Failed to dispatch HandshakeInit frame".into())
        })?;

        *self.pending_handshake.lock().await = Some((kx, salt));
        tracing::info!("HandshakeInit dispatched to Android peer, awaiting HandshakeReply");
        Ok(())
    }

    /// Completes the handshake upon receiving HandshakeReply from Android.
    pub async fn complete_handshake(
        &self,
        handshake_reply_bytes: &[u8],
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<String> {
        let (kx, salt) = self
            .pending_handshake
            .lock()
            .await
            .take()
            .ok_or(ZeroLinkError::KeyExchangeFailed)?;

        let reply = HandshakeReplyPayload::deserialize(handshake_reply_bytes)?;
        let client_public_key = kx.public_key;
        let server_pairing_transcript = [
            client_public_key.as_slice(),
            salt.as_slice(),
            reply.server_ephemeral_pubkey.as_slice(),
        ]
        .concat();
        let stored_pairing_secret = *self.pairing_secret.lock().await;
        match stored_pairing_secret {
            Some(secret)
                if pairing_auth_tag(&secret, b"server", &server_pairing_transcript)
                    == reply.pairing_auth_tag => {}
            Some(_) => return Err(ZeroLinkError::AuthenticationFailed),
            None if reply.pairing_auth_tag == [0u8; 16] => {}
            None => return Err(ZeroLinkError::AuthenticationFailed),
        }
        let session_cipher = kx.complete_handshake(
            &reply.server_ephemeral_pubkey,
            &salt,
            SessionRole::Initiator,
        )?;
        let mut transcript = b"ZeroLink-v3-server-key-confirmation".to_vec();
        transcript.extend_from_slice(&client_public_key);
        transcript.extend_from_slice(&salt);
        transcript.extend_from_slice(&reply.server_ephemeral_pubkey);
        if session_cipher.key_confirmation_tag(&transcript) != reply.auth_tag {
            return Err(ZeroLinkError::AuthenticationFailed);
        }

        if stored_pairing_secret.is_none() {
            #[cfg(not(test))]
            {
                let code = pairing_code(&client_public_key, &salt, &reply.server_ephemeral_pubkey);
                let message = format!(
                "A new ZeroLink pairing was requested by {}.\n\nCompare this six-digit code with the Android screen before approving:\n\n{}\n\nThe code must match on both devices.",
                reply.server_device_name, code
            );
                let answer = tokio::task::spawn_blocking(move || {
                    let message = HSTRING::from(message);
                    let title = HSTRING::from("Approve ZeroLink pairing");
                    unsafe {
                        MessageBoxW(
                            HWND::default(),
                            &message,
                            &title,
                            MB_YESNO
                                | MB_ICONWARNING
                                | MB_DEFBUTTON2
                                | MB_SETFOREGROUND
                                | MB_TOPMOST,
                        )
                    }
                })
                .await
                .map_err(|error| {
                    ZeroLinkError::ConnectionError(format!(
                        "Pairing approval dialog task failed: {error}"
                    ))
                })?;
                if answer != IDYES {
                    tracing::warn!("Windows user rejected the first-pair approval prompt");
                    return Err(ZeroLinkError::AuthenticationFailed);
                }
                tracing::info!("Windows user approved the first-pair request");
            }
        }

        let mut client_transcript = b"ZeroLink-v3-client-key-confirmation".to_vec();
        client_transcript.extend_from_slice(&client_public_key);
        client_transcript.extend_from_slice(&salt);
        client_transcript.extend_from_slice(&reply.server_ephemeral_pubkey);
        let client_tag = session_cipher.key_confirmation_tag(&client_transcript);
        let mut confirmation = Vec::with_capacity(17);
        confirmation.push(0x02);
        confirmation.extend_from_slice(&client_tag);
        let confirmation_frame = Frame::new(PacketType::Handshake, 1, confirmation)?;
        raw_io_tx
            .send(confirmation_frame.serialize())
            .map_err(|_| {
                ZeroLinkError::ConnectionError("Failed to dispatch client key confirmation".into())
            })?;

        if reply.l2cap_psm != 0 {
            *self.psm.lock().await = reply.l2cap_psm;
        }
        let current_psm = *self.psm.lock().await;
        *self.cipher.lock().await = Some(session_cipher);

        tracing::info!(
            "Secure Curve25519 session established with Android device: {} (PSM: 0x{:04x})",
            reply.server_device_name,
            current_psm
        );
        Ok(reply.server_device_name)
    }

    /// Executes synchronous Curve25519 ECDH handshake with the Android server.
    pub async fn perform_handshake(
        &self,
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
        handshake_reply_bytes: &[u8],
    ) -> Result<()> {
        self.initiate_handshake(raw_io_tx).await?;
        self.complete_handshake(handshake_reply_bytes, raw_io_tx)
            .await?;
        Ok(())
    }

    /// Encrypts and frames a payload for low-power transport.
    pub async fn send_packet(
        &self,
        packet_type: PacketType,
        raw_data: &[u8],
        raw_io_tx: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        if !self.is_paired().await && packet_type != PacketType::Signaling {
            return Err(ZeroLinkError::AuthenticationFailed);
        }
        let mut lock = self.cipher.lock().await;
        let cipher = lock.as_mut().ok_or(ZeroLinkError::AuthenticationFailed)?;

        let (seq, ciphertext) = cipher.encrypt(raw_data, &[packet_type as u8])?;
        let frame = Frame::new(packet_type, seq, ciphertext)?;

        raw_io_tx.send(frame.serialize()).map_err(|_| {
            ZeroLinkError::ConnectionError("Failed to transmit framed packet".into())
        })?;

        Ok(())
    }

    /// Decrypts an incoming framed packet received from Android.
    pub async fn receive_frame(&self, frame: &Frame) -> Result<Vec<u8>> {
        let mut lock = self.cipher.lock().await;
        let cipher = lock.as_mut().ok_or(ZeroLinkError::AuthenticationFailed)?;

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

    /// Resets the cryptographic session on disconnect.
    pub async fn reset_session(&self) {
        *self.cipher.lock().await = None;
        *self.pending_handshake.lock().await = None;
        tracing::info!("BLE session reset to unauthenticated state");
    }

    pub async fn save_pairing_secret(&self, secret: [u8; 32]) -> Result<()> {
        crate::pairing_store::save_pairing_secret(&secret)
            .map_err(ZeroLinkError::ConnectionError)?;
        *self.pairing_secret.lock().await = Some(secret);
        Ok(())
    }

    pub async fn is_paired(&self) -> bool {
        self.pairing_secret.lock().await.is_some()
    }

    pub async fn forget_pairing(&self) -> Result<()> {
        crate::pairing_store::clear_pairing_secret().map_err(ZeroLinkError::ConnectionError)?;
        *self.pairing_secret.lock().await = None;
        Ok(())
    }

    pub async fn session_pairing_secret(&self) -> Result<[u8; 32]> {
        self.cipher
            .lock()
            .await
            .as_ref()
            .map(SessionCipher::pairing_secret)
            .ok_or(ZeroLinkError::AuthenticationFailed)
    }

    #[cfg(test)]
    pub async fn mark_paired_for_test(&self) {
        if let Ok(secret) = self.session_pairing_secret().await {
            *self.pairing_secret.lock().await = Some(secret);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BleL2capClient;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};
    use zerolink_core::crypto::{pairing_auth_tag, KeyExchange, SessionRole};
    use zerolink_core::framing::{Frame, PacketType};
    use zerolink_core::protocol::{HandshakeInitPayload, HandshakeReplyPayload};

    fn client_with_pairing_secret(secret: [u8; 32]) -> BleL2capClient {
        BleL2capClient {
            device_address: Arc::new(Mutex::new(0)),
            psm: Arc::new(Mutex::new(0)),
            cipher: Arc::new(Mutex::new(None)),
            pending_handshake: Arc::new(Mutex::new(None)),
            pairing_secret: Arc::new(Mutex::new(Some(secret))),
            pairing_store_error: None,
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    async fn server_reply(
        client: &BleL2capClient,
        pairing_proof_secret: [u8; 32],
    ) -> (
        Vec<u8>,
        mpsc::UnboundedSender<Vec<u8>>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();
        client.initiate_handshake(&outbound_tx).await.unwrap();
        let init_frame_bytes = outbound_rx.recv().await.unwrap();
        let (init_frame, consumed) = Frame::try_parse(&init_frame_bytes).unwrap().unwrap();
        assert_eq!(consumed, init_frame_bytes.len());
        assert_eq!(init_frame.header.packet_type, PacketType::Handshake);
        let init = HandshakeInitPayload::deserialize(&init_frame.payload).unwrap();

        let server_exchange = KeyExchange::new();
        let server_public_key = server_exchange.public_key;
        let server_pairing_transcript = [
            init.client_ephemeral_pubkey.as_slice(),
            init.salt.as_slice(),
            server_public_key.as_slice(),
        ]
        .concat();
        let server_cipher = server_exchange
            .complete_handshake(
                &init.client_ephemeral_pubkey,
                &init.salt,
                SessionRole::Responder,
            )
            .unwrap();
        let mut server_key_transcript = b"ZeroLink-v3-server-key-confirmation".to_vec();
        server_key_transcript.extend_from_slice(&init.client_ephemeral_pubkey);
        server_key_transcript.extend_from_slice(&init.salt);
        server_key_transcript.extend_from_slice(&server_public_key);
        let reply = HandshakeReplyPayload {
            server_ephemeral_pubkey: server_public_key,
            auth_tag: server_cipher.key_confirmation_tag(&server_key_transcript),
            l2cap_psm: 0,
            server_device_name: "test Android peer".to_string(),
            pairing_auth_tag: pairing_auth_tag(
                &pairing_proof_secret,
                b"server",
                &server_pairing_transcript,
            ),
        };

        (reply.serialize(), outbound_tx, outbound_rx)
    }

    #[tokio::test]
    async fn paired_handshake_rejects_a_mismatched_persisted_secret() {
        let trusted_secret = [0xA5; 32];
        let wrong_secret = [0x5A; 32];
        let client = client_with_pairing_secret(trusted_secret);
        let (reply, outbound_tx, mut outbound_rx) = server_reply(&client, wrong_secret).await;

        let error = client
            .complete_handshake(&reply, &outbound_tx)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            zerolink_core::error::ZeroLinkError::AuthenticationFailed
        ));
        assert!(!client.is_session_active().await);
        assert!(outbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn paired_handshake_accepts_a_matching_persisted_secret() {
        let trusted_secret = [0xA5; 32];
        let client = client_with_pairing_secret(trusted_secret);
        let (reply, outbound_tx, mut confirmation_rx) = server_reply(&client, trusted_secret).await;
        let result = client.complete_handshake(&reply, &outbound_tx).await;

        assert_eq!(result.unwrap(), "test Android peer");
        assert!(client.is_session_active().await);
        let confirmation_bytes = confirmation_rx.recv().await.unwrap();
        let (confirmation, consumed) = Frame::try_parse(&confirmation_bytes).unwrap().unwrap();
        assert_eq!(consumed, confirmation_bytes.len());
        assert_eq!(confirmation.header.packet_type, PacketType::Handshake);
        assert_eq!(confirmation.header.sequence, 1);
        assert_eq!(confirmation.payload.first(), Some(&0x02));
    }
}
