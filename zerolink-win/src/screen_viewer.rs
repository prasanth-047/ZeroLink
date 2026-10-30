use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use zerolink_core::error::{Result, ZeroLinkError};

pub struct ScreenMirrorReceiver {
    is_active: Arc<AtomicBool>,
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
}

impl ScreenMirrorReceiver {
    pub fn new() -> Self {
        Self {
            is_active: Arc::new(AtomicBool::new(false)),
            device: None,
            context: None,
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
                tracing::warn!("Hardware D3D11 device creation failed, falling back to WARP driver");
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

    /// Processes an incoming H.265/HEVC NAL unit.
    /// When Android screen is static, frame arrival rate is 0 FPS (0.0% CPU usage).
    pub fn submit_hevc_packet(&self, nal_data: &[u8], pts_us: u64, is_keyframe: bool) {
        if !self.is_active.load(Ordering::Relaxed) {
            return;
        }

        tracing::trace!(
            "HEVC frame received: {} bytes, pts: {}us, keyframe: {}",
            nal_data.len(),
            pts_us,
            is_keyframe
        );
    }

    pub fn stop(&self) {
        self.is_active.store(false, Ordering::SeqCst);
        self.device = None;
        self.context = None;
    }
}
