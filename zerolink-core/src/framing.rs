use byteorder::{BigEndian, ByteOrder};
use crc32fast::Hasher as Crc32Hasher;

use crate::error::{Result, ZeroLinkError};

pub const MAGIC_BYTES: u16 = 0x5A4C; // 'Z', 'L'
pub const PROTOCOL_VERSION: u8 = 0x01;
pub const HEADER_SIZE: usize = 20;
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024; // 16 MB maximum payload limit

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Handshake = 0x01,
    Signaling = 0x02,
    Notification = 0x03,
    Clipboard = 0x04,
    FileMetadata = 0x05,
    FileChunk = 0x06,
    WifiWake = 0x07,
    ScreenCast = 0x08,
    Heartbeat = 0x09,
    KeepAlive = 0x0A,
}

impl PacketType {
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0x01 => Ok(Self::Handshake),
            0x02 => Ok(Self::Signaling),
            0x03 => Ok(Self::Notification),
            0x04 => Ok(Self::Clipboard),
            0x05 => Ok(Self::FileMetadata),
            0x06 => Ok(Self::FileChunk),
            0x07 => Ok(Self::WifiWake),
            0x08 => Ok(Self::ScreenCast),
            0x09 => Ok(Self::Heartbeat),
            0x0A => Ok(Self::KeepAlive),
            _ => Err(ZeroLinkError::SerializationError(format!(
                "Unknown packet type byte: {:#04x}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub magic: u16,
    pub version: u8,
    pub packet_type: PacketType,
    pub payload_len: u32,
    pub sequence: u64,
    pub header_crc: u32,
}

impl FrameHeader {
    pub fn new(packet_type: PacketType, payload_len: u32, sequence: u64) -> Self {
        let mut header = Self {
            magic: MAGIC_BYTES,
            version: PROTOCOL_VERSION,
            packet_type,
            payload_len,
            sequence,
            header_crc: 0,
        };
        header.header_crc = header.compute_crc();
        header
    }

    /// Computes CRC32 checksum over the first 16 bytes of the header.
    fn compute_crc(&self) -> u32 {
        let mut buf = [0u8; 16];
        BigEndian::write_u16(&mut buf[0..2], self.magic);
        buf[2] = self.version;
        buf[3] = self.packet_type as u8;
        BigEndian::write_u32(&mut buf[4..8], self.payload_len);
        BigEndian::write_u64(&mut buf[8..16], self.sequence);

        let mut hasher = Crc32Hasher::new();
        hasher.update(&buf);
        hasher.finalize()
    }

    /// Serializes the 20-byte header into a fixed buffer.
    pub fn serialize(&self, out: &mut [u8; HEADER_SIZE]) {
        BigEndian::write_u16(&mut out[0..2], self.magic);
        out[2] = self.version;
        out[3] = self.packet_type as u8;
        BigEndian::write_u32(&mut out[4..8], self.payload_len);
        BigEndian::write_u64(&mut out[8..16], self.sequence);
        BigEndian::write_u32(&mut out[16..20], self.header_crc);
    }

    /// Parses and verifies a 20-byte slice as a valid frame header.
    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_SIZE {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: HEADER_SIZE,
                available: buf.len(),
            });
        }

        let magic = BigEndian::read_u16(&buf[0..2]);
        if magic != MAGIC_BYTES {
            return Err(ZeroLinkError::InvalidMagic(magic));
        }

        let version = buf[2];
        if version != PROTOCOL_VERSION {
            return Err(ZeroLinkError::UnsupportedVersion(version));
        }

        let packet_type = PacketType::from_u8(buf[3])?;
        let payload_len = BigEndian::read_u32(&buf[4..8]);
        if (payload_len as usize) > MAX_FRAME_SIZE {
            return Err(ZeroLinkError::FrameTooLarge(payload_len as usize));
        }

        let sequence = BigEndian::read_u64(&buf[8..16]);
        let header_crc = BigEndian::read_u32(&buf[16..20]);

        let candidate = Self {
            magic,
            version,
            packet_type,
            payload_len,
            sequence,
            header_crc,
        };

        let calculated_crc = candidate.compute_crc();
        if calculated_crc != header_crc {
            return Err(ZeroLinkError::HeaderChecksumMismatch {
                expected: header_crc,
                calculated: calculated_crc,
            });
        }

        Ok(candidate)
    }
}

/// In-memory framed packet representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(packet_type: PacketType, sequence: u64, payload: Vec<u8>) -> Result<Self> {
        if payload.len() > MAX_FRAME_SIZE {
            return Err(ZeroLinkError::FrameTooLarge(payload.len()));
        }

        let header = FrameHeader::new(packet_type, payload.len() as u32, sequence);
        Ok(Self { header, payload })
    }

    /// Serializes entire frame (header + payload) into a contiguous buffer.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        let mut header_bytes = [0u8; HEADER_SIZE];
        self.header.serialize(&mut header_bytes);
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Attempts to parse a complete frame from a stream buffer.
    /// Returns Ok(Some((frame, bytes_consumed))) or Ok(None) if more bytes are required.
    pub fn try_parse(buf: &[u8]) -> Result<Option<(Self, usize)>> {
        if buf.len() < HEADER_SIZE {
            return Ok(None);
        }

        let header = FrameHeader::deserialize(&buf[..HEADER_SIZE])?;
        let total_size = HEADER_SIZE + header.payload_len as usize;

        if buf.len() < total_size {
            return Ok(None);
        }

        let payload = buf[HEADER_SIZE..total_size].to_vec();
        Ok(Some((Self { header, payload }, total_size)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_serialization_roundtrip() {
        let payload = b"FlatBuffers-zero-copy-payload-data".to_vec();
        let frame = Frame::new(PacketType::Notification, 42, payload.clone()).unwrap();

        let bytes = frame.serialize();
        assert_eq!(bytes.len(), HEADER_SIZE + payload.len());

        let (parsed_frame, consumed) = Frame::try_parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed_frame.header.sequence, 42);
        assert_eq!(parsed_frame.header.packet_type, PacketType::Notification);
        assert_eq!(parsed_frame.payload, payload);
    }
}
