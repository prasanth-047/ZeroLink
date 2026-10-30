use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zerolink_core::error::{Result, ZeroLinkError};

pub struct EphemeralBulkClient {
    stream: Option<TcpStream>,
    remote_addr: Option<SocketAddr>,
}

impl EphemeralBulkClient {
    pub fn new() -> Self {
        Self {
            stream: None,
            remote_addr: None,
        }
    }

    /// Spawns the ephemeral bulk transfer socket connecting to the target Android IP and port
    /// upon receiving a BLE Wi-Fi wake acknowledgement.
    pub async fn connect_ephemeral(&mut self, remote_addr: SocketAddr) -> Result<()> {
        let stream = TcpStream::connect(remote_addr)
            .await
            .map_err(|e| ZeroLinkError::ConnectionError(format!("Socket connect error to {}: {}", remote_addr, e)))?;

        // Optimize socket for low-latency high-throughput transfer
        let _ = stream.set_nodelay(true);

        self.stream = Some(stream);
        self.remote_addr = Some(remote_addr);
        tracing::info!("Ephemeral bulk socket connected to Android server at {}", remote_addr);
        Ok(())
    }

    /// Fetches a specific byte range of a remote file from Android's ephemeral socket.
    /// Used by Cloud Filter to hydrate virtual placeholder files.
    pub async fn fetch_file_chunk(
        &mut self,
        relative_path: &str,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| ZeroLinkError::ConnectionError("Ephemeral socket is not connected".into()))?;

        // Protocol command: 0x01 (Fetch file)
        let mut request = Vec::new();
        request.extend_from_slice(&1u32.to_be_bytes()); // Command ID

        let path_bytes = relative_path.as_bytes();
        request.extend_from_slice(&(path_bytes.len() as u16).to_be_bytes());
        request.extend_from_slice(path_bytes);

        request.extend_from_slice(&offset.to_be_bytes());
        request.extend_from_slice(&length.to_be_bytes());

        stream
            .write_all(&request)
            .await
            .map_err(|e| ZeroLinkError::ConnectionError(format!("Failed to write fetch request: {}", e)))?;

        // Read total file size header (8 bytes)
        let mut file_size_buf = [0u8; 8];
        stream
            .read_exact(&mut file_size_buf)
            .await
            .map_err(|e| ZeroLinkError::ConnectionError(format!("Failed to read file size header: {}", e)))?;

        // Read chunk data
        let mut chunk_data = vec![0u8; length as usize];
        stream
            .read_exact(&mut chunk_data)
            .await
            .map_err(|e| ZeroLinkError::ConnectionError(format!("Failed to read chunk payload: {}", e)))?;

        Ok(chunk_data)
    }

    /// Tears down the socket immediately after transfer completes,
    /// signalling the Android radio to drop back to deep Doze sleep.
    pub fn teardown(&mut self) {
        if self.stream.is_some() {
            tracing::info!("Tearing down ephemeral Wi-Fi socket to preserve phone battery");
            self.stream = None;
            self.remote_addr = None;
        }
    }
}
