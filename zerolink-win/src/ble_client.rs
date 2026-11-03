use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use windows::core::HSTRING;
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
    BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
use zerolink_core::crypto::{generate_salt, KeyExchange, SessionCipher};
use zerolink_core::error::{Result, ZeroLinkError};
use zerolink_core::framing::{Frame, PacketType};
use zerolink_core::protocol::{HandshakeInitPayload, HandshakeReplyPayload};

// Service UUID for ZeroLink BLE L2CAP advertisement: 0000FE2C-0000-1000-8000-00805F9B34FB
const ZEROLINK_UUID_16: u16 = 0xFE2C;

pub struct BleL2capClient {
    device_address: Arc<Mutex<u64>>,
    psm: Arc<Mutex<u16>>,
    cipher: Arc<Mutex<Option<SessionCipher>>>,
    pending_handshake: Arc<Mutex<Option<(KeyExchange, [u8; 32])>>>,
    watcher: Arc<Mutex<Option<BluetoothLEAdvertisementWatcher>>>,
}

#[allow(dead_code)]
impl BleL2capClient {
    pub fn new(device_address: u64, psm: u16) -> Self {
        Self {
            device_address: Arc::new(Mutex::new(device_address)),
            psm: Arc::new(Mutex::new(psm)),
            cipher: Arc::new(Mutex::new(None)),
            pending_handshake: Arc::new(Mutex::new(None)),
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
        tracing::info!("WinRT BLE Advertisement Watcher active (Scanning for ZeroLink service 0xFE2C)");
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

        *self.pending_handshake.lock().await = Some((kx, salt));
        tracing::info!("HandshakeInit dispatched to Android peer, awaiting HandshakeReply");
        Ok(())
    }

    /// Completes the handshake upon receiving HandshakeReply from Android.
    pub async fn complete_handshake(&self, handshake_reply_bytes: &[u8]) -> Result<String> {
        let (kx, salt) = self
            .pending_handshake
            .lock()
            .await
            .take()
            .ok_or(ZeroLinkError::KeyExchangeFailed)?;

        let reply = HandshakeReplyPayload::deserialize(handshake_reply_bytes)?;
        let session_cipher = kx.complete_handshake(&reply.server_ephemeral_pubkey, &salt)?;

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
        self.complete_handshake(handshake_reply_bytes).await?;
        Ok(())
    }

    /// Encrypts and frames a payload for low-power transport.
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

    /// Decrypts an incoming framed packet received from Android.
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

    /// Resets the cryptographic session on disconnect.
    pub async fn reset_session(&self) {
        *self.cipher.lock().await = None;
        *self.pending_handshake.lock().await = None;
        tracing::info!("BLE session reset to unauthenticated state");
    }
}
