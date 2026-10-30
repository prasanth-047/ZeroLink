use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ZeroLinkError {
    #[error("Invalid magic byte sequence in packet header: expected 0x5A4C, got {0:#06x}")]
    InvalidMagic(u16),

    #[error("Unsupported protocol version: {0}")]
    UnsupportedVersion(u8),

    #[error("Header CRC32 checksum mismatch: expected {expected:#010x}, calculated {calculated:#010x}")]
    HeaderChecksumMismatch { expected: u32, calculated: u32 },

    #[error("Frame length exceeds maximum allowed buffer size: {0} bytes")]
    FrameTooLarge(usize),

    #[error("Incomplete frame buffer: needed {needed} bytes, have {available} bytes")]
    IncompleteFrame { needed: usize, available: usize },

    #[error("Cryptographic authentication failed: invalid GCM tag or corrupted ciphertext")]
    AuthenticationFailed,

    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("Session key exchange failed")]
    KeyExchangeFailed,

    #[error("Sequence number replay attack detected: received {received}, expected > {last_seen}")]
    SequenceReplay { received: u64, last_seen: u64 },

    #[error("Invalid state transition in dual-radio state machine from {from:?} to {to:?}")]
    InvalidStateTransition { from: String, to: String },

    #[error("Serialization error: {0}")]
    SerializationError(String),
}

pub type Result<T> = std::result::Result<T, ZeroLinkError>;
