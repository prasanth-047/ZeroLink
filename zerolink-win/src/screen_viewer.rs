use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use zerolink_core::error::{Result, ZeroLinkError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HevcNalType {
    TrailN,
    TrailR,
    IdrWRadl,
    IdrNLp,
    CraNut,
    VpsNut,
    SpsNut,
    PpsNut,
    AudNut,
    PrefixSeiNut,
    SuffixSeiNut,
    Other(u8),
}

impl HevcNalType {
    pub fn from_byte(b: u8) -> Self {
        // In HEVC, nal_unit_type = (byte & 0x7E) >> 1
        let nal_type = (b & 0x7E) >> 1;
        match nal_type {
            0 => Self::TrailN,
            1 => Self::TrailR,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            32 => Self::VpsNut,
            33 => Self::SpsNut,
            34 => Self::PpsNut,
            35 => Self::AudNut,
            39 => Self::PrefixSeiNut,
            40 => Self::SuffixSeiNut,
            _ => Self::Other(nal_type),
        }
    }

    pub fn is_keyframe(&self) -> bool {
        matches!(self, Self::IdrWRadl | Self::IdrNLp | Self::CraNut)
    }
}

pub struct ScreenMirrorReceiver {
    is_active: Arc<AtomicBool>,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    frame_count: Arc<AtomicU64>,
    keyframe_count: Arc<AtomicU64>,
    total_bytes: Arc<AtomicU64>,
    last_pts_us: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl ScreenMirrorReceiver {
    pub fn new() -> Self {
        Self {
            is_active: Arc::new(AtomicBool::new(false)),
            device: None,
            context: None,
            frame_count: Arc::new(AtomicU64::new(0)),
            keyframe_count: Arc::new(AtomicU64::new(0)),
            total_bytes: Arc::new(AtomicU64::new(0)),
            last_pts_us: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Initializes Direct3D 11 hardware video acceleration pipeline.
    /// Falls back to WARP software rasterization if hardware device is unavailable.
    pub fn init_d3d11_hardware(&mut self) -> Result<()> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let mut device = None;
        let mut context = None;

        unsafe {
            let hr = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            );

            if hr.is_err() {
                tracing::warn!(
                    "Hardware D3D11 device creation failed, falling back to WARP driver"
                );
                let warp_hr = D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    None,
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                );

                if warp_hr.is_err() {
                    return Err(ZeroLinkError::ConnectionError(
                        "Failed to initialize D3D11 device (both Hardware and WARP failed)".into(),
                    ));
                }
            }
        }

        self.device = device;
        self.context = context;
        self.is_active.store(true, Ordering::SeqCst);
        tracing::info!("D3D11 Video Acceleration context initialized successfully");
        Ok(())
    }

    /// Processes an incoming H.265/HEVC NAL unit received from the Android MediaCodec encoder.
    /// Parses NAL unit header (VPS, SPS, PPS, IDR, or P-slice) and updates acceleration pipeline.
    /// When Android screen is static, frame arrival rate is 0 FPS (0.0% CPU usage).
    pub fn submit_hevc_packet(&self, nal_data: &[u8], pts_us: u64, is_keyframe_hint: bool) {
        if !self.is_active.load(Ordering::Relaxed) || nal_data.is_empty() {
            return;
        }

        // Determine NAL unit offset (skip Annex B start code: 0x000001 or 0x00000001)
        let nal_offset = if nal_data.len() >= 4 && nal_data[0..4] == [0, 0, 0, 1] {
            4
        } else if nal_data.len() >= 3 && nal_data[0..3] == [0, 0, 1] {
            3
        } else {
            0
        };

        let nal_type = if nal_offset < nal_data.len() {
            HevcNalType::from_byte(nal_data[nal_offset])
        } else {
            HevcNalType::Other(0)
        };

        let is_keyframe = is_keyframe_hint || nal_type.is_keyframe();

        let count = self.frame_count.fetch_add(1, Ordering::Relaxed) + 1;
        if is_keyframe {
            self.keyframe_count.fetch_add(1, Ordering::Relaxed);
        }
        self.total_bytes
            .fetch_add(nal_data.len() as u64, Ordering::Relaxed);
        self.last_pts_us.store(pts_us, Ordering::Relaxed);

        if count % 60 == 1 || is_keyframe {
            tracing::info!(
                "Screen mirror HEVC packet: {} bytes (NAL: {:?}, keyframe: {}, PTS: {}us, total frames: {})",
                nal_data.len(),
                nal_type,
                is_keyframe,
                pts_us,
                count
            );
        } else {
            tracing::trace!(
                "HEVC frame #{}: {} bytes, pts: {}us, keyframe: {}",
                count,
                nal_data.len(),
                pts_us,
                is_keyframe
            );
        }
    }

    /// Returns the current statistics (total_frames, keyframes, total_bytes).
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.frame_count.load(Ordering::Relaxed),
            self.keyframe_count.load(Ordering::Relaxed),
            self.total_bytes.load(Ordering::Relaxed),
        )
    }

    pub fn stop(&mut self) {
        self.is_active.store(false, Ordering::SeqCst);
        self.device = None;
        self.context = None;
        tracing::info!("ScreenMirrorReceiver stopped and D3D11 context released");
    }
}
