use byteorder::{BigEndian, ByteOrder};

use crate::error::{Result, ZeroLinkError};

/// Handshake/key-schedule version. Bumped when directional keys and key confirmation were added.
pub const HANDSHAKE_PROTOCOL_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeInitPayload {
    pub client_ephemeral_pubkey: [u8; 32],
    pub salt: [u8; 32],
    pub protocol_version: u32,
    pub client_device_name: String,
    /// Zero when unpaired; otherwise an HMAC proof under the stored pairing secret.
    pub pairing_auth_tag: [u8; 16],
}

impl HandshakeInitPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.client_device_name.as_bytes();
        let mut out = Vec::with_capacity(32 + 32 + 4 + 4 + name_bytes.len() + 16);
        out.extend_from_slice(&self.client_ephemeral_pubkey);
        out.extend_from_slice(&self.salt);
        let mut ver_buf = [0u8; 4];
        BigEndian::write_u32(&mut ver_buf, self.protocol_version);
        out.extend_from_slice(&ver_buf);

        let mut len_buf = [0u8; 4];
        BigEndian::write_u32(&mut len_buf, name_bytes.len() as u32);
        out.extend_from_slice(&len_buf);
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&self.pairing_auth_tag);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 88 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 88,
                available: buf.len(),
            });
        }
        let mut client_ephemeral_pubkey = [0u8; 32];
        let mut salt = [0u8; 32];
        client_ephemeral_pubkey.copy_from_slice(&buf[0..32]);
        salt.copy_from_slice(&buf[32..64]);
        let protocol_version = BigEndian::read_u32(&buf[64..68]);
        let name_len = BigEndian::read_u32(&buf[68..72]) as usize;

        let expected_len = 72usize
            .checked_add(name_len)
            .and_then(|len| len.checked_add(16))
            .ok_or_else(|| {
                ZeroLinkError::SerializationError("HandshakeInit length overflow".into())
            })?;
        if buf.len() < expected_len {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: expected_len,
                available: buf.len(),
            });
        }
        if buf.len() != expected_len {
            return Err(ZeroLinkError::SerializationError(
                "HandshakeInit has trailing bytes".into(),
            ));
        }
        let client_device_name = String::from_utf8_lossy(&buf[72..72 + name_len]).to_string();
        let mut pairing_auth_tag = [0u8; 16];
        pairing_auth_tag.copy_from_slice(&buf[72 + name_len..72 + name_len + 16]);

        Ok(Self {
            client_ephemeral_pubkey,
            salt,
            protocol_version,
            client_device_name,
            pairing_auth_tag,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeReplyPayload {
    pub server_ephemeral_pubkey: [u8; 32],
    pub auth_tag: [u8; 16],
    pub l2cap_psm: u16,
    pub server_device_name: String,
    /// Zero when unpaired; otherwise an HMAC proof under the stored pairing secret.
    pub pairing_auth_tag: [u8; 16],
}

impl HandshakeReplyPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.server_device_name.as_bytes();
        let mut out = Vec::with_capacity(32 + 16 + 2 + 4 + name_bytes.len() + 16);
        out.extend_from_slice(&self.server_ephemeral_pubkey);
        out.extend_from_slice(&self.auth_tag);
        let mut psm_buf = [0u8; 2];
        BigEndian::write_u16(&mut psm_buf, self.l2cap_psm);
        out.extend_from_slice(&psm_buf);

        let mut len_buf = [0u8; 4];
        BigEndian::write_u32(&mut len_buf, name_bytes.len() as u32);
        out.extend_from_slice(&len_buf);
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&self.pairing_auth_tag);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 70 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 70,
                available: buf.len(),
            });
        }
        let mut server_ephemeral_pubkey = [0u8; 32];
        let mut auth_tag = [0u8; 16];
        server_ephemeral_pubkey.copy_from_slice(&buf[0..32]);
        auth_tag.copy_from_slice(&buf[32..48]);
        let l2cap_psm = BigEndian::read_u16(&buf[48..50]);
        let name_len = BigEndian::read_u32(&buf[50..54]) as usize;

        let expected_len = 54usize
            .checked_add(name_len)
            .and_then(|len| len.checked_add(16))
            .ok_or_else(|| {
                ZeroLinkError::SerializationError("HandshakeReply length overflow".into())
            })?;
        if buf.len() < expected_len {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: expected_len,
                available: buf.len(),
            });
        }
        if buf.len() != expected_len {
            return Err(ZeroLinkError::SerializationError(
                "HandshakeReply has trailing bytes".into(),
            ));
        }
        let server_device_name = String::from_utf8_lossy(&buf[54..54 + name_len]).to_string();
        let mut pairing_auth_tag = [0u8; 16];
        pairing_auth_tag.copy_from_slice(&buf[54 + name_len..54 + name_len + 16]);

        Ok(Self {
            server_ephemeral_pubkey,
            auth_tag,
            l2cap_psm,
            server_device_name,
            pairing_auth_tag,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    PlainText = 0,
    HtmlText = 1,
    PngImage = 2,
    UriList = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub format: ClipboardType,
    pub payload_hash: u64,
    pub timestamp_ms: u64,
    pub data: Vec<u8>,
}

impl ClipboardPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 8 + 8 + 4 + self.data.len());
        out.push(self.format as u8);
        let mut hash_buf = [0u8; 8];
        BigEndian::write_u64(&mut hash_buf, self.payload_hash);
        out.extend_from_slice(&hash_buf);

        let mut time_buf = [0u8; 8];
        BigEndian::write_u64(&mut time_buf, self.timestamp_ms);
        out.extend_from_slice(&time_buf);

        let mut len_buf = [0u8; 4];
        BigEndian::write_u32(&mut len_buf, self.data.len() as u32);
        out.extend_from_slice(&len_buf);
        out.extend_from_slice(&self.data);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 21 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 21,
                available: buf.len(),
            });
        }
        let format = match buf[0] {
            0 => ClipboardType::PlainText,
            1 => ClipboardType::HtmlText,
            2 => ClipboardType::PngImage,
            3 => ClipboardType::UriList,
            _ => {
                return Err(ZeroLinkError::SerializationError(
                    "Invalid clipboard type".into(),
                ))
            }
        };
        let payload_hash = BigEndian::read_u64(&buf[1..9]);
        let timestamp_ms = BigEndian::read_u64(&buf[9..17]);
        let data_len = BigEndian::read_u32(&buf[17..21]) as usize;

        if buf.len() < 21 + data_len {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 21 + data_len,
                available: buf.len(),
            });
        }
        let data = buf[21..21 + data_len].to_vec();

        Ok(Self {
            format,
            payload_hash,
            timestamp_ms,
            data,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationPayload {
    pub id: String,
    pub package_name: String,
    pub app_name: String,
    pub title: String,
    pub content: String,
    pub timestamp_ms: u64,
    pub is_ongoing: bool,
}

impl NotificationPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let id_b = self.id.as_bytes();
        let pkg_b = self.package_name.as_bytes();
        let app_b = self.app_name.as_bytes();
        let title_b = self.title.as_bytes();
        let content_b = self.content.as_bytes();

        let mut out = Vec::new();
        let mut time_buf = [0u8; 8];
        BigEndian::write_u64(&mut time_buf, self.timestamp_ms);
        out.extend_from_slice(&time_buf);
        out.push(if self.is_ongoing { 1 } else { 0 });

        let append_str = |vec: &mut Vec<u8>, s: &[u8]| {
            let mut len_b = [0u8; 2];
            BigEndian::write_u16(&mut len_b, s.len() as u16);
            vec.extend_from_slice(&len_b);
            vec.extend_from_slice(s);
        };

        append_str(&mut out, id_b);
        append_str(&mut out, pkg_b);
        append_str(&mut out, app_b);
        append_str(&mut out, title_b);
        append_str(&mut out, content_b);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 9 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 9,
                available: buf.len(),
            });
        }
        let timestamp_ms = BigEndian::read_u64(&buf[0..8]);
        let is_ongoing = buf[8] != 0;
        let mut offset = 9;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let id = read_str(&mut offset)?;
        let package_name = read_str(&mut offset)?;
        let app_name = read_str(&mut offset)?;
        let title = read_str(&mut offset)?;
        let content = read_str(&mut offset)?;

        Ok(Self {
            id,
            package_name,
            app_name,
            title,
            content,
            timestamp_ms,
            is_ongoing,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationActionPayload {
    pub notification_id: String,
    pub action_id: String,
    pub reply_text: String,
}

impl NotificationActionPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let append_str = |vec: &mut Vec<u8>, s: &str| {
            let b = s.as_bytes();
            let mut len_b = [0u8; 2];
            BigEndian::write_u16(&mut len_b, b.len() as u16);
            vec.extend_from_slice(&len_b);
            vec.extend_from_slice(b);
        };

        append_str(&mut out, &self.notification_id);
        append_str(&mut out, &self.action_id);
        append_str(&mut out, &self.reply_text);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let mut offset = 0;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let notification_id = read_str(&mut offset)?;
        let action_id = read_str(&mut offset)?;
        let reply_text = read_str(&mut offset)?;

        Ok(Self {
            notification_id,
            action_id,
            reply_text,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play = 0,
    Pause = 1,
    TogglePlayPause = 2,
    Next = 3,
    Previous = 4,
    VolumeUp = 5,
    VolumeDown = 6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaControlPayload {
    pub command: MediaCommand,
}

impl MediaControlPayload {
    pub fn serialize(&self) -> Vec<u8> {
        vec![self.command as u8]
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.is_empty() {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 1,
                available: 0,
            });
        }
        let command = match buf[0] {
            0 => MediaCommand::Play,
            1 => MediaCommand::Pause,
            2 => MediaCommand::TogglePlayPause,
            3 => MediaCommand::Next,
            4 => MediaCommand::Previous,
            5 => MediaCommand::VolumeUp,
            6 => MediaCommand::VolumeDown,
            _ => {
                return Err(ZeroLinkError::SerializationError(
                    "Invalid media command".into(),
                ))
            }
        };
        Ok(Self { command })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStatePayload {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub app_name: String,
    pub is_playing: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
}

impl MediaStatePayload {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut num_buf = [0u8; 8];
        BigEndian::write_u64(&mut num_buf, self.position_ms);
        out.extend_from_slice(&num_buf);
        BigEndian::write_u64(&mut num_buf, self.duration_ms);
        out.extend_from_slice(&num_buf);
        out.push(if self.is_playing { 1 } else { 0 });

        let append_str = |vec: &mut Vec<u8>, s: &str| {
            let b = s.as_bytes();
            let mut len_b = [0u8; 2];
            BigEndian::write_u16(&mut len_b, b.len() as u16);
            vec.extend_from_slice(&len_b);
            vec.extend_from_slice(b);
        };

        append_str(&mut out, &self.title);
        append_str(&mut out, &self.artist);
        append_str(&mut out, &self.album);
        append_str(&mut out, &self.app_name);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 17 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 17,
                available: buf.len(),
            });
        }
        let position_ms = BigEndian::read_u64(&buf[0..8]);
        let duration_ms = BigEndian::read_u64(&buf[8..16]);
        let is_playing = buf[16] != 0;
        let mut offset = 17;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let title = read_str(&mut offset)?;
        let artist = read_str(&mut offset)?;
        let album = read_str(&mut offset)?;
        let app_name = read_str(&mut offset)?;

        Ok(Self {
            title,
            artist,
            album,
            app_name,
            is_playing,
            position_ms,
            duration_ms,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenInputAction {
    Down = 0,
    Move = 1,
    Up = 2,
    Scroll = 3,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenInputPayload {
    pub action: ScreenInputAction,
    /// Normalized horizontal position in the range 0.0..=1.0.
    pub x: f32,
    /// Normalized vertical position in the range 0.0..=1.0.
    pub y: f32,
    /// Wheel delta (negative scrolls up, positive scrolls down).
    pub scroll_delta: i16,
}

impl ScreenInputPayload {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(11);
        out.push(self.action as u8);
        let mut f_buf = [0u8; 4];
        BigEndian::write_f32(&mut f_buf, self.x.clamp(0.0, 1.0));
        out.extend_from_slice(&f_buf);
        BigEndian::write_f32(&mut f_buf, self.y.clamp(0.0, 1.0));
        out.extend_from_slice(&f_buf);
        let mut i_buf = [0u8; 2];
        BigEndian::write_i16(&mut i_buf, self.scroll_delta);
        out.extend_from_slice(&i_buf);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 11 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 11,
                available: buf.len(),
            });
        }
        let action = match buf[0] {
            0 => ScreenInputAction::Down,
            1 => ScreenInputAction::Move,
            2 => ScreenInputAction::Up,
            3 => ScreenInputAction::Scroll,
            _ => {
                return Err(ZeroLinkError::SerializationError(
                    "Invalid screen input action".into(),
                ))
            }
        };
        let x = BigEndian::read_f32(&buf[1..5]).clamp(0.0, 1.0);
        let y = BigEndian::read_f32(&buf[5..9]).clamp(0.0, 1.0);
        let scroll_delta = BigEndian::read_i16(&buf[9..11]);

        Ok(Self {
            action,
            x,
            y,
            scroll_delta,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiWakePayload {
    pub target_ip: String,
    pub port: u16,
    pub session_token: String,
    pub reason: String,
}

impl WifiWakePayload {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut port_b = [0u8; 2];
        BigEndian::write_u16(&mut port_b, self.port);
        out.extend_from_slice(&port_b);

        let append_str = |vec: &mut Vec<u8>, s: &str| {
            let b = s.as_bytes();
            let mut len_b = [0u8; 2];
            BigEndian::write_u16(&mut len_b, b.len() as u16);
            vec.extend_from_slice(&len_b);
            vec.extend_from_slice(b);
        };

        append_str(&mut out, &self.target_ip);
        append_str(&mut out, &self.session_token);
        append_str(&mut out, &self.reason);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 2 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 2,
                available: buf.len(),
            });
        }
        let port = BigEndian::read_u16(&buf[0..2]);
        let mut offset = 2;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let target_ip = read_str(&mut offset)?;
        let session_token = read_str(&mut offset)?;
        let reason = read_str(&mut offset)?;

        Ok(Self {
            target_ip,
            port,
            session_token,
            reason,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub file_id: String,
    pub relative_path: String,
    pub file_size: u64,
    pub modified_timestamp: u64,
    pub mime_type: String,
    pub is_directory: bool,
}

impl FileMetadata {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut num_buf = [0u8; 8];

        BigEndian::write_u64(&mut num_buf, self.file_size);
        out.extend_from_slice(&num_buf);
        BigEndian::write_u64(&mut num_buf, self.modified_timestamp);
        out.extend_from_slice(&num_buf);
        out.push(if self.is_directory { 1 } else { 0 });

        let append_str = |vec: &mut Vec<u8>, s: &str| {
            let b = s.as_bytes();
            let mut len_b = [0u8; 2];
            BigEndian::write_u16(&mut len_b, b.len() as u16);
            vec.extend_from_slice(&len_b);
            vec.extend_from_slice(b);
        };

        append_str(&mut out, &self.file_id);
        append_str(&mut out, &self.relative_path);
        append_str(&mut out, &self.mime_type);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 17 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 17,
                available: buf.len(),
            });
        }
        let file_size = BigEndian::read_u64(&buf[0..8]);
        let modified_timestamp = BigEndian::read_u64(&buf[8..16]);
        let is_directory = buf[16] != 0;
        let mut offset = 17;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let file_id = read_str(&mut offset)?;
        let relative_path = read_str(&mut offset)?;
        let mime_type = read_str(&mut offset)?;

        Ok(Self {
            file_id,
            relative_path,
            file_size,
            modified_timestamp,
            mime_type,
            is_directory,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCatalog {
    pub sync_root_id: String,
    pub files: Vec<FileMetadata>,
}

impl FileCatalog {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let root_b = self.sync_root_id.as_bytes();
        let mut len_b = [0u8; 2];
        BigEndian::write_u16(&mut len_b, root_b.len() as u16);
        out.extend_from_slice(&len_b);
        out.extend_from_slice(root_b);

        let mut count_b = [0u8; 4];
        BigEndian::write_u32(&mut count_b, self.files.len() as u32);
        out.extend_from_slice(&count_b);

        for file in &self.files {
            let serialized_file = file.serialize();
            let mut file_len_b = [0u8; 4];
            BigEndian::write_u32(&mut file_len_b, serialized_file.len() as u32);
            out.extend_from_slice(&file_len_b);
            out.extend_from_slice(&serialized_file);
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 6 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 6,
                available: buf.len(),
            });
        }
        let root_len = BigEndian::read_u16(&buf[0..2]) as usize;
        let mut offset = 2;
        if buf.len() < offset + root_len + 4 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: offset + root_len + 4,
                available: buf.len(),
            });
        }
        let sync_root_id = String::from_utf8_lossy(&buf[offset..offset + root_len]).to_string();
        offset += root_len;

        let count = BigEndian::read_u32(&buf[offset..offset + 4]) as usize;
        offset += 4;

        let mut files = Vec::with_capacity(count);
        for _ in 0..count {
            if buf.len() < offset + 4 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: offset + 4,
                    available: buf.len(),
                });
            }
            let item_len = BigEndian::read_u32(&buf[offset..offset + 4]) as usize;
            offset += 4;
            if buf.len() < offset + item_len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: offset + item_len,
                    available: buf.len(),
                });
            }
            let file = FileMetadata::deserialize(&buf[offset..offset + item_len])?;
            files.push(file);
            offset += item_len;
        }

        Ok(Self {
            sync_root_id,
            files,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFetchRequest {
    pub file_id: String,
    pub byte_offset: u64,
    pub byte_length: u32,
    pub transfer_ticket: String,
}

impl FileFetchRequest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut offset_b = [0u8; 8];
        BigEndian::write_u64(&mut offset_b, self.byte_offset);
        out.extend_from_slice(&offset_b);

        let mut len_b = [0u8; 4];
        BigEndian::write_u32(&mut len_b, self.byte_length);
        out.extend_from_slice(&len_b);

        let append_str = |vec: &mut Vec<u8>, s: &str| {
            let b = s.as_bytes();
            let mut str_len_b = [0u8; 2];
            BigEndian::write_u16(&mut str_len_b, b.len() as u16);
            vec.extend_from_slice(&str_len_b);
            vec.extend_from_slice(b);
        };

        append_str(&mut out, &self.file_id);
        append_str(&mut out, &self.transfer_ticket);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 12 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 12,
                available: buf.len(),
            });
        }
        let byte_offset = BigEndian::read_u64(&buf[0..8]);
        let byte_length = BigEndian::read_u32(&buf[8..12]);
        let mut offset = 12;

        let read_str = |offset: &mut usize| -> Result<String> {
            if buf.len() < *offset + 2 {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + 2,
                    available: buf.len(),
                });
            }
            let len = BigEndian::read_u16(&buf[*offset..*offset + 2]) as usize;
            *offset += 2;
            if buf.len() < *offset + len {
                return Err(ZeroLinkError::IncompleteFrame {
                    needed: *offset + len,
                    available: buf.len(),
                });
            }
            let s = String::from_utf8_lossy(&buf[*offset..*offset + len]).to_string();
            *offset += len;
            Ok(s)
        };

        let file_id = read_str(&mut offset)?;
        let transfer_ticket = read_str(&mut offset)?;

        Ok(Self {
            file_id,
            byte_offset,
            byte_length,
            transfer_ticket,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFetchChunk {
    pub file_id: String,
    pub byte_offset: u64,
    pub is_eof: bool,
    pub data: Vec<u8>,
}

impl FileFetchChunk {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut offset_b = [0u8; 8];
        BigEndian::write_u64(&mut offset_b, self.byte_offset);
        out.extend_from_slice(&offset_b);
        out.push(if self.is_eof { 1 } else { 0 });

        let id_b = self.file_id.as_bytes();
        let mut id_len_b = [0u8; 2];
        BigEndian::write_u16(&mut id_len_b, id_b.len() as u16);
        out.extend_from_slice(&id_len_b);
        out.extend_from_slice(id_b);

        let mut data_len_b = [0u8; 4];
        BigEndian::write_u32(&mut data_len_b, self.data.len() as u32);
        out.extend_from_slice(&data_len_b);
        out.extend_from_slice(&self.data);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 9 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 9,
                available: buf.len(),
            });
        }
        let byte_offset = BigEndian::read_u64(&buf[0..8]);
        let is_eof = buf[8] != 0;
        let mut offset = 9;

        if buf.len() < offset + 2 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: offset + 2,
                available: buf.len(),
            });
        }
        let id_len = BigEndian::read_u16(&buf[offset..offset + 2]) as usize;
        offset += 2;

        if buf.len() < offset + id_len + 4 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: offset + id_len + 4,
                available: buf.len(),
            });
        }
        let file_id = String::from_utf8_lossy(&buf[offset..offset + id_len]).to_string();
        offset += id_len;

        let data_len = BigEndian::read_u32(&buf[offset..offset + 4]) as usize;
        offset += 4;

        if buf.len() < offset + data_len {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: offset + data_len,
                available: buf.len(),
            });
        }
        let data = buf[offset..offset + data_len].to_vec();

        Ok(Self {
            file_id,
            byte_offset,
            is_eof,
            data,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenAction {
    Start = 0,
    Stop = 1,
    RequestKeyframe = 2,
    SetBitrate = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenControlRequest {
    pub action: ScreenAction,
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub bitrate_bps: u32,
}

impl ScreenControlRequest {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(17);
        out.push(self.action as u8);
        let mut buf = [0u8; 4];
        BigEndian::write_u32(&mut buf, self.width);
        out.extend_from_slice(&buf);
        BigEndian::write_u32(&mut buf, self.height);
        out.extend_from_slice(&buf);
        BigEndian::write_u32(&mut buf, self.max_fps);
        out.extend_from_slice(&buf);
        BigEndian::write_u32(&mut buf, self.bitrate_bps);
        out.extend_from_slice(&buf);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 17 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 17,
                available: buf.len(),
            });
        }
        let action = match buf[0] {
            0 => ScreenAction::Start,
            1 => ScreenAction::Stop,
            2 => ScreenAction::RequestKeyframe,
            3 => ScreenAction::SetBitrate,
            _ => {
                return Err(ZeroLinkError::SerializationError(
                    "Invalid screen action".into(),
                ))
            }
        };
        let width = BigEndian::read_u32(&buf[1..5]);
        let height = BigEndian::read_u32(&buf[5..9]);
        let max_fps = BigEndian::read_u32(&buf[9..13]);
        let bitrate_bps = BigEndian::read_u32(&buf[13..17]);

        Ok(Self {
            action,
            width,
            height,
            max_fps,
            bitrate_bps,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameHeader {
    pub sequence_number: u64,
    pub pts_microseconds: u64,
    pub is_keyframe: bool,
    pub nal_unit_count: u16,
    pub frame_size_bytes: u32,
    pub width: u16,
    pub height: u16,
}

impl VideoFrameHeader {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(27);
        let mut u64_buf = [0u8; 8];
        BigEndian::write_u64(&mut u64_buf, self.sequence_number);
        out.extend_from_slice(&u64_buf);
        BigEndian::write_u64(&mut u64_buf, self.pts_microseconds);
        out.extend_from_slice(&u64_buf);
        out.push(if self.is_keyframe { 1 } else { 0 });

        let mut u16_buf = [0u8; 2];
        BigEndian::write_u16(&mut u16_buf, self.nal_unit_count);
        out.extend_from_slice(&u16_buf);

        let mut u32_buf = [0u8; 4];
        BigEndian::write_u32(&mut u32_buf, self.frame_size_bytes);
        out.extend_from_slice(&u32_buf);
        let mut u16_buf = [0u8; 2];
        BigEndian::write_u16(&mut u16_buf, self.width);
        out.extend_from_slice(&u16_buf);
        BigEndian::write_u16(&mut u16_buf, self.height);
        out.extend_from_slice(&u16_buf);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 27 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 27,
                available: buf.len(),
            });
        }
        let sequence_number = BigEndian::read_u64(&buf[0..8]);
        let pts_microseconds = BigEndian::read_u64(&buf[8..16]);
        let is_keyframe = buf[16] != 0;
        let nal_unit_count = BigEndian::read_u16(&buf[17..19]);
        let frame_size_bytes = BigEndian::read_u32(&buf[19..23]);
        let width = BigEndian::read_u16(&buf[23..25]);
        let height = BigEndian::read_u16(&buf[25..27]);

        Ok(Self {
            sequence_number,
            pts_microseconds,
            is_keyframe,
            nal_unit_count,
            frame_size_bytes,
            width,
            height,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub battery_percentage: u8,
    pub is_charging: bool,
    pub wifi_connected: bool,
    pub bulk_port_active: u16,
    pub device_power_mode: u8,
}

impl Heartbeat {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.push(self.battery_percentage);
        out.push(if self.is_charging { 1 } else { 0 });
        out.push(if self.wifi_connected { 1 } else { 0 });
        let mut port_b = [0u8; 2];
        BigEndian::write_u16(&mut port_b, self.bulk_port_active);
        out.extend_from_slice(&port_b);
        out.push(self.device_power_mode);
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < 6 {
            return Err(ZeroLinkError::IncompleteFrame {
                needed: 6,
                available: buf.len(),
            });
        }
        let battery_percentage = buf[0];
        let is_charging = buf[1] != 0;
        let wifi_connected = buf[2] != 0;
        let bulk_port_active = BigEndian::read_u16(&buf[3..5]);
        let device_power_mode = buf[5];

        Ok(Self {
            battery_percentage,
            is_charging,
            wifi_connected,
            bulk_port_active,
            device_power_mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handshake_payloads() {
        let init = HandshakeInitPayload {
            client_ephemeral_pubkey: [7u8; 32],
            salt: [42u8; 32],
            protocol_version: HANDSHAKE_PROTOCOL_VERSION,
            client_device_name: "Desktop-PC".to_string(),
            pairing_auth_tag: [0u8; 16],
        };
        let b = init.serialize();
        let parsed = HandshakeInitPayload::deserialize(&b).unwrap();
        assert_eq!(init, parsed);
        let mut init_with_trailing_byte = b.clone();
        init_with_trailing_byte.push(0);
        assert!(HandshakeInitPayload::deserialize(&init_with_trailing_byte).is_err());

        let reply = HandshakeReplyPayload {
            server_ephemeral_pubkey: [9u8; 32],
            auth_tag: [12u8; 16],
            l2cap_psm: 0x1001,
            server_device_name: "Pixel-Phone".to_string(),
            pairing_auth_tag: [0u8; 16],
        };
        let b_reply = reply.serialize();
        let parsed_reply = HandshakeReplyPayload::deserialize(&b_reply).unwrap();
        assert_eq!(reply, parsed_reply);
        let mut reply_with_trailing_byte = b_reply.clone();
        reply_with_trailing_byte.push(0);
        assert!(HandshakeReplyPayload::deserialize(&reply_with_trailing_byte).is_err());
    }

    #[test]
    fn test_clipboard_payload() {
        let clip = ClipboardPayload {
            format: ClipboardType::PlainText,
            payload_hash: 0xDEADBEEFCAFEBABE,
            timestamp_ms: 1700000000000,
            data: b"Hello ZeroLink Clipboard".to_vec(),
        };
        let b = clip.serialize();
        let parsed = ClipboardPayload::deserialize(&b).unwrap();
        assert_eq!(clip, parsed);
    }

    #[test]
    fn test_notification_payload() {
        let notif = NotificationPayload {
            id: "msg_12345".to_string(),
            package_name: "com.whatsapp".to_string(),
            app_name: "WhatsApp".to_string(),
            title: "Alice".to_string(),
            content: "Hey, are you free?".to_string(),
            timestamp_ms: 1700000050000,
            is_ongoing: false,
        };
        let b = notif.serialize();
        let parsed = NotificationPayload::deserialize(&b).unwrap();
        assert_eq!(notif, parsed);
    }

    #[test]
    fn test_notification_action_payload() {
        let action = NotificationActionPayload {
            notification_id: "0|com.whatsapp|123|null|10123".to_string(),
            action_id: "reply_0".to_string(),
            reply_text: "On my way!".to_string(),
        };
        let b = action.serialize();
        let parsed = NotificationActionPayload::deserialize(&b).unwrap();
        assert_eq!(action, parsed);
    }

    #[test]
    fn test_media_payloads() {
        let cmd = MediaControlPayload {
            command: MediaCommand::TogglePlayPause,
        };
        let b = cmd.serialize();
        assert_eq!(MediaControlPayload::deserialize(&b).unwrap(), cmd);

        let state = MediaStatePayload {
            title: "Song Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            app_name: "Spotify.exe".to_string(),
            is_playing: true,
            position_ms: 65_000,
            duration_ms: 210_000,
        };
        let b_state = state.serialize();
        let parsed_state = MediaStatePayload::deserialize(&b_state).unwrap();
        assert_eq!(state, parsed_state);
    }

    #[test]
    fn test_screen_input_payload() {
        let input = ScreenInputPayload {
            action: ScreenInputAction::Down,
            x: 0.25,
            y: 0.75,
            scroll_delta: -120,
        };
        let b = input.serialize();
        let parsed = ScreenInputPayload::deserialize(&b).unwrap();
        assert_eq!(input, parsed);

        // Out-of-range coordinates are clamped on the wire.
        let clamped = ScreenInputPayload {
            action: ScreenInputAction::Move,
            x: 1.5,
            y: -0.5,
            scroll_delta: 0,
        };
        let parsed_clamped = ScreenInputPayload::deserialize(&clamped.serialize()).unwrap();
        assert_eq!(parsed_clamped.x, 1.0);
        assert_eq!(parsed_clamped.y, 0.0);
    }

    #[test]
    fn test_wifi_wake_payload() {
        let wake = WifiWakePayload {
            target_ip: "192.168.1.120".to_string(),
            port: 8443,
            session_token: "secret_token_abc".to_string(),
            reason: "PHOTO_FETCH".to_string(),
        };
        let b = wake.serialize();
        let parsed = WifiWakePayload::deserialize(&b).unwrap();
        assert_eq!(wake, parsed);
    }

    #[test]
    fn test_file_catalog_and_fetch() {
        let meta1 = FileMetadata {
            file_id: "photo_001".to_string(),
            relative_path: "DCIM/Camera/IMG_001.jpg".to_string(),
            file_size: 4_500_123,
            modified_timestamp: 1700000099999,
            mime_type: "image/jpeg".to_string(),
            is_directory: false,
        };
        let meta2 = FileMetadata {
            file_id: "folder_001".to_string(),
            relative_path: "Documents".to_string(),
            file_size: 0,
            modified_timestamp: 1700000000000,
            mime_type: "inode/directory".to_string(),
            is_directory: true,
        };
        let catalog = FileCatalog {
            sync_root_id: "android_storage_root".to_string(),
            files: vec![meta1.clone(), meta2],
        };
        let b = catalog.serialize();
        let parsed = FileCatalog::deserialize(&b).unwrap();
        assert_eq!(catalog, parsed);

        let req = FileFetchRequest {
            file_id: "photo_001".to_string(),
            byte_offset: 65536,
            byte_length: 32768,
            transfer_ticket: "ticket_xyz".to_string(),
        };
        let b_req = req.serialize();
        let parsed_req = FileFetchRequest::deserialize(&b_req).unwrap();
        assert_eq!(req, parsed_req);

        let chunk = FileFetchChunk {
            file_id: "photo_001".to_string(),
            byte_offset: 65536,
            is_eof: true,
            data: vec![0xAA; 512],
        };
        let b_chunk = chunk.serialize();
        let parsed_chunk = FileFetchChunk::deserialize(&b_chunk).unwrap();
        assert_eq!(chunk, parsed_chunk);
    }

    #[test]
    fn test_screen_and_heartbeat() {
        let screen = ScreenControlRequest {
            action: ScreenAction::Start,
            width: 1080,
            height: 2400,
            max_fps: 60,
            bitrate_bps: 8_000_000,
        };
        let b = screen.serialize();
        let parsed = ScreenControlRequest::deserialize(&b).unwrap();
        assert_eq!(screen, parsed);

        let frame = VideoFrameHeader {
            sequence_number: 105,
            pts_microseconds: 5_200_000,
            is_keyframe: true,
            nal_unit_count: 2,
            frame_size_bytes: 45_120,
            width: 1080,
            height: 2400,
        };
        let b_frame = frame.serialize();
        let parsed_frame = VideoFrameHeader::deserialize(&b_frame).unwrap();
        assert_eq!(frame, parsed_frame);

        let hb = Heartbeat {
            battery_percentage: 87,
            is_charging: false,
            wifi_connected: true,
            bulk_port_active: 8443,
            device_power_mode: 0,
        };
        let b_hb = hb.serialize();
        let parsed_hb = Heartbeat::deserialize(&b_hb).unwrap();
        assert_eq!(hb, parsed_hb);
    }
}
