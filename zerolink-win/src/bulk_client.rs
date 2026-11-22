use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zerolink_core::crypto::{
    decrypt_wifi_bulk_response, derive_wifi_bulk_keys, encrypt_wifi_bulk_request, generate_salt,
    NONCE_SIZE,
};
use zerolink_core::error::{Result, ZeroLinkError};

pub struct EphemeralBulkClient {
    stream: Option<TcpStream>,
    remote_addr: Option<SocketAddr>,
    server_challenge: Option<[u8; 32]>,
}

#[allow(dead_code)]
impl EphemeralBulkClient {
    pub const DEFAULT_PORT: u16 = 8443;

    pub fn new() -> Self {
        Self {
            stream: None,
            remote_addr: None,
            server_challenge: None,
        }
    }

    /// Spawns the ephemeral bulk transfer socket connecting to the target Android IP and port
    /// upon receiving a BLE Wi-Fi wake acknowledgement.
    pub async fn connect_ephemeral(&mut self, remote_addr: SocketAddr) -> Result<()> {
        let mut stream = TcpStream::connect(remote_addr).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!(
                "Socket connect error to {}: {}",
                remote_addr, e
            ))
        })?;

        // Optimize socket for low-latency high-throughput transfer.
        let _ = stream.set_nodelay(true);

        let mut server_challenge = [0u8; 32];
        stream
            .read_exact(&mut server_challenge)
            .await
            .map_err(|e| {
                ZeroLinkError::ConnectionError(format!(
                    "Could not read bulk-transfer challenge: {e}"
                ))
            })?;

        self.stream = Some(stream);
        self.remote_addr = Some(remote_addr);
        self.server_challenge = Some(server_challenge);
        tracing::info!(
            "Ephemeral bulk socket connected to Android server at {}",
            remote_addr
        );
        Ok(())
    }

    /// Fetches a specific byte range of a remote file from Android's ephemeral socket.
    /// Used by Cloud Filter to hydrate virtual placeholder files.
    pub async fn fetch_file_chunk(
        &mut self,
        file_id: &str,
        session_token: &str,
        offset: u64,
        length: u32,
    ) -> Result<(u64, Vec<u8>)> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            ZeroLinkError::ConnectionError("Ephemeral socket is not connected".into())
        })?;
        let token = decode_session_token(session_token)?;
        let id_bytes = file_id.as_bytes();
        if id_bytes.is_empty() || id_bytes.len() > 256 || length == 0 || length > 16 * 1024 * 1024 {
            return Err(ZeroLinkError::SerializationError(
                "Invalid bulk file range request".into(),
            ));
        }

        let challenge = self.server_challenge.as_ref().ok_or_else(|| {
            ZeroLinkError::ConnectionError("Missing bulk-transfer challenge".into())
        })?;
        let keys = derive_wifi_bulk_keys(&token, challenge);

        // Only ciphertext is sent over the Wi-Fi socket. The bearer token stays on the paired channel.
        let mut request_body = Vec::with_capacity(15 + id_bytes.len());
        request_body.push(1); // Protocol version / MediaStore range request.
        request_body.extend_from_slice(&(id_bytes.len() as u16).to_be_bytes());
        request_body.extend_from_slice(id_bytes);
        request_body.extend_from_slice(&offset.to_be_bytes());
        request_body.extend_from_slice(&length.to_be_bytes());
        let random = generate_salt();
        let request_nonce: [u8; NONCE_SIZE] = random[..NONCE_SIZE]
            .try_into()
            .expect("nonce length is fixed");
        let encrypted_request =
            encrypt_wifi_bulk_request(&keys, challenge, &request_nonce, &request_body)?;

        stream.write_all(&request_nonce).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!("Failed to write encrypted fetch request: {e}"))
        })?;
        stream
            .write_all(&(encrypted_request.len() as u32).to_be_bytes())
            .await
            .map_err(|e| {
                ZeroLinkError::ConnectionError(format!("Failed to write request length: {e}"))
            })?;
        stream.write_all(&encrypted_request).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!("Failed to write encrypted request: {e}"))
        })?;

        let mut status_buf = [0u8; 4];
        stream.read_exact(&mut status_buf).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!("Failed to read transfer status: {e}"))
        })?;
        let status = i32::from_be_bytes(status_buf);
        if status != 0 {
            return Err(ZeroLinkError::ConnectionError(
                "Android rejected or could not serve the media range".into(),
            ));
        }

        let mut response_nonce = [0u8; NONCE_SIZE];
        stream.read_exact(&mut response_nonce).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!("Failed to read encrypted response nonce: {e}"))
        })?;

        let mut length_buf = [0u8; 4];
        stream.read_exact(&mut length_buf).await.map_err(|e| {
            ZeroLinkError::ConnectionError(format!("Failed to read chunk length: {e}"))
        })?;
        let encrypted_length = u32::from_be_bytes(length_buf);
        let max_encrypted_length = length.saturating_add(12 + 16);
        if encrypted_length < 12 + 16 || encrypted_length > max_encrypted_length {
            return Err(ZeroLinkError::SerializationError(
                "Android returned an invalid encrypted range length".into(),
            ));
        }

        let mut encrypted_response = vec![0u8; encrypted_length as usize];
        stream
            .read_exact(&mut encrypted_response)
            .await
            .map_err(|e| {
                ZeroLinkError::ConnectionError(format!(
                    "Failed to read encrypted chunk payload: {e}"
                ))
            })?;

        let response_body = decrypt_wifi_bulk_response(
            &keys,
            challenge,
            &request_body,
            &response_nonce,
            &encrypted_response,
        )?;
        if response_body.len() < 12 {
            return Err(ZeroLinkError::SerializationError(
                "Encrypted media response is truncated".into(),
            ));
        }
        let file_size = u64::from_be_bytes(
            response_body[..8]
                .try_into()
                .expect("checked response header"),
        );
        let actual_length = u32::from_be_bytes(
            response_body[8..12]
                .try_into()
                .expect("checked response header"),
        );
        if response_body.len() != 12 + actual_length as usize
            || actual_length > length
            || offset.saturating_add(actual_length as u64) > file_size
        {
            return Err(ZeroLinkError::SerializationError(
                "Android returned an invalid file range".into(),
            ));
        }
        let chunk_data = response_body[12..].to_vec();

        Ok((file_size, chunk_data))
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Tears down the socket immediately after transfer completes,
    /// signalling the Android radio to drop back to deep Doze sleep.
    pub fn teardown(&mut self) {
        if self.stream.is_some() {
            tracing::info!("Tearing down ephemeral Wi-Fi socket to preserve phone battery");
            self.stream = None;
            self.remote_addr = None;
            self.server_challenge = None;
        }
    }
}

fn decode_session_token(value: &str) -> Result<[u8; 32]> {
    let encoded = value.as_bytes();
    if encoded.len() != 64 {
        return Err(ZeroLinkError::SerializationError(
            "Invalid bulk-transfer token".into(),
        ));
    }
    let mut token = [0u8; 32];
    for (index, byte) in token.iter_mut().enumerate() {
        let high = decode_hex_digit(encoded[index * 2]).ok_or_else(|| {
            ZeroLinkError::SerializationError("Invalid bulk-transfer token".into())
        })?;
        let low = decode_hex_digit(encoded[index * 2 + 1]).ok_or_else(|| {
            ZeroLinkError::SerializationError("Invalid bulk-transfer token".into())
        })?;
        *byte = (high << 4) | low;
    }
    Ok(token)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_session_token, EphemeralBulkClient};
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use zerolink_core::crypto::{
        decrypt_wifi_bulk_request, derive_wifi_bulk_keys, encrypt_wifi_bulk_response, NONCE_SIZE,
    };

    #[test]
    fn bulk_session_tokens_must_be_32_bytes_of_hex() {
        assert_eq!(decode_session_token(&"ab".repeat(32)).unwrap(), [0xAB; 32]);
        assert!(decode_session_token("short").is_err());
        assert!(decode_session_token(&"gg".repeat(32)).is_err());
        assert!(decode_session_token(&format!("{}x", "ab".repeat(31))).is_err());
    }

    #[tokio::test]
    async fn bulk_client_fetches_an_authenticated_range_from_android_wire_format() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let token = [0x42; 32];
        let token_hex = token
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let challenge = [0xA6; 32];
        let file: Vec<u8> = (0..1000).map(|index| (index % 251) as u8).collect();

        let server_file = file.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&challenge).await.unwrap();

            let mut request_nonce = [0u8; NONCE_SIZE];
            stream.read_exact(&mut request_nonce).await.unwrap();
            let mut length_buf = [0u8; 4];
            stream.read_exact(&mut length_buf).await.unwrap();
            let encrypted_len = u32::from_be_bytes(length_buf) as usize;
            let mut encrypted_request = vec![0u8; encrypted_len];
            stream.read_exact(&mut encrypted_request).await.unwrap();

            let keys = derive_wifi_bulk_keys(&token, &challenge);
            let request =
                decrypt_wifi_bulk_request(&keys, &challenge, &request_nonce, &encrypted_request)
                    .unwrap();
            assert_eq!(request[0], 1);
            let id_len = u16::from_be_bytes([request[1], request[2]]) as usize;
            assert_eq!(&request[3..3 + id_len], b"media-row-17");
            let offset_at = 3 + id_len;
            let offset = u64::from_be_bytes(request[offset_at..offset_at + 8].try_into().unwrap());
            let requested_len =
                u32::from_be_bytes(request[offset_at + 8..offset_at + 12].try_into().unwrap());
            assert_eq!(offset, 936);
            assert_eq!(requested_len, 64);

            let range = &server_file[offset as usize..];
            let mut response_body = Vec::with_capacity(12 + range.len());
            response_body.extend_from_slice(&(server_file.len() as u64).to_be_bytes());
            response_body.extend_from_slice(&(range.len() as u32).to_be_bytes());
            response_body.extend_from_slice(range);
            let response_nonce = [0x22; NONCE_SIZE];
            let encrypted_response = encrypt_wifi_bulk_response(
                &keys,
                &challenge,
                &request,
                &response_nonce,
                &response_body,
            )
            .unwrap();
            stream.write_all(&0i32.to_be_bytes()).await.unwrap();
            stream.write_all(&response_nonce).await.unwrap();
            stream
                .write_all(&(encrypted_response.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&encrypted_response).await.unwrap();
        });

        let mut client = EphemeralBulkClient::new();
        client.connect_ephemeral(address).await.unwrap();
        let (reported_size, bytes) = client
            .fetch_file_chunk("media-row-17", &token_hex, 936, 64)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(reported_size, 1000);
        assert_eq!(bytes, file[936..]);
        client.teardown();
    }
}
