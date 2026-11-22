use crate::d3d_presenter::D3dScreenPresenter;
use crate::hevc_decoder::{DecodedFrame, HevcDecoder};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, ScreenToClient, SetStretchBltMode, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HBRUSH, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetWindowLongPtrW, ShowWindow, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, HMENU,
    IDC_ARROW, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSEXW, WS_CAPTION, WS_OVERLAPPED, WS_SYSMENU,
    WS_VISIBLE,
};
use zerolink_core::error::{Result, ZeroLinkError};
use zerolink_core::protocol::{ScreenInputAction, ScreenInputPayload};

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
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    frame_count: Arc<AtomicU64>,
    keyframe_count: Arc<AtomicU64>,
    total_bytes: Arc<AtomicU64>,
    last_pts_us: Arc<AtomicU64>,
    last_width: Arc<AtomicU64>,
    last_height: Arc<AtomicU64>,
    decoder: Option<HevcDecoder>,
    decoder_dimensions: Option<(u16, u16)>,
}

#[allow(dead_code)]
impl ScreenMirrorReceiver {
    pub fn new() -> Self {
        Self {
            device: None,
            context: None,
            frame_count: Arc::new(AtomicU64::new(0)),
            keyframe_count: Arc::new(AtomicU64::new(0)),
            total_bytes: Arc::new(AtomicU64::new(0)),
            last_pts_us: Arc::new(AtomicU64::new(0)),
            last_width: Arc::new(AtomicU64::new(0)),
            last_height: Arc::new(AtomicU64::new(0)),
            decoder: None,
            decoder_dimensions: None,
        }
    }

    /// Initializes the shared D3D11 context for decoding and direct presentation.
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
        if let Some(context) = self.context.as_ref() {
            if let Ok(multithread) = context.cast::<ID3D10Multithread>() {
                unsafe {
                    let _ = multithread.SetMultithreadProtected(BOOL(1));
                }
            }
        }
        self.decoder = match HevcDecoder::new(self.device.clone(), self.context.clone()) {
            Ok(decoder) => {
                tracing::info!("Windows Media Foundation HEVC decoder initialized");
                Some(decoder)
            }
            Err(error) => {
                tracing::warn!("HEVC screen decoding is unavailable: {error}");
                None
            }
        };
        tracing::info!("D3D11 graphics context initialized for Media Foundation hardware decode");
        Ok(())
    }

    /// Tracks an incoming H.265/HEVC access unit and parses its first NAL header.
    /// The separate decoder is invoked only while the viewer window is open.
    pub fn submit_hevc_packet(
        &self,
        nal_data: &[u8],
        pts_us: u64,
        is_keyframe_hint: bool,
        width: u16,
        height: u16,
    ) {
        if nal_data.is_empty() {
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
        self.last_width.store(width as u64, Ordering::Relaxed);
        self.last_height.store(height as u64, Ordering::Relaxed);

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

    pub fn dimensions(&self) -> (u16, u16) {
        (
            self.last_width.load(Ordering::Relaxed) as u16,
            self.last_height.load(Ordering::Relaxed) as u16,
        )
    }

    pub fn d3d11_handles(&self) -> (Option<ID3D11Device>, Option<ID3D11DeviceContext>) {
        (self.device.clone(), self.context.clone())
    }

    /// Decodes a frame only while the viewer is open, avoiding background video work.
    pub fn decode_hevc_packet(
        &mut self,
        nal_data: &[u8],
        pts_us: u64,
        width: u16,
        height: u16,
        direct_gpu: bool,
    ) -> Option<DecodedFrame> {
        let apartment = match ComMtaGuard::initialize() {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!("Cannot decode HEVC on this COM thread: {error}");
                return None;
            }
        };
        if width > 0 && height > 0 {
            if self
                .decoder_dimensions
                .is_some_and(|old_dimensions| old_dimensions != (width, height))
            {
                tracing::info!(
                    "Restarting the HEVC decoder for remote display size {}x{}",
                    width,
                    height
                );
                self.decoder.take();
                self.decoder = match HevcDecoder::new(self.device.clone(), self.context.clone()) {
                    Ok(decoder) => Some(decoder),
                    Err(error) => {
                        tracing::warn!(
                            "Could not restart HEVC decoder after a display-size change: {error}"
                        );
                        self.decoder_dimensions = None;
                        drop(apartment);
                        return None;
                    }
                };
            }
            self.decoder_dimensions = Some((width, height));
        }
        if self.decoder.is_none() {
            self.decoder = HevcDecoder::new(self.device.clone(), self.context.clone()).ok();
        }
        let decoder = self.decoder.as_mut()?;
        let result = decoder.decode(nal_data, pts_us, width, height, direct_gpu);
        match result {
            Ok(frame) => {
                drop(apartment);
                frame
            }
            Err(error) => {
                tracing::warn!("Disabling the HEVC decoder after a decode error: {error}");
                self.decoder.take();
                self.decoder_dimensions = None;
                drop(apartment);
                None
            }
        }
    }

    pub fn stop(&mut self) {
        self.device = None;
        self.context = None;
        self.decoder = None;
        tracing::info!("ScreenMirrorReceiver stopped and D3D11 context released");
    }
}

// ---------------------------------------------------------------------------
// Screen input viewer window
// ---------------------------------------------------------------------------

struct ViewerState {
    input_tx: UnboundedSender<ScreenInputPayload>,
    dragging: bool,
    remote_width: u16,
    remote_height: u16,
    latest_frame: Option<DecodedFrame>,
    frame_mailbox: Arc<FrameMailbox>,
    gpu_renderer: Option<D3dScreenPresenter>,
    gpu_enabled: Arc<AtomicBool>,
}

struct FrameMailbox {
    latest: Mutex<Option<DecodedFrame>>,
    message_pending: AtomicBool,
}

const WM_APP_UPDATE_REMOTE_SIZE: u32 = 0x0400 + 117;
const WM_APP_PRESENT_FRAME: u32 = 0x0400 + 118;

/// Native Win32 window that renders decoded frames and forwards normalized input.
pub struct ScreenViewerWindow {
    hwnd_val: isize,
    thread: Option<std::thread::JoinHandle<()>>,
    frame_mailbox: Arc<FrameMailbox>,
    gpu_enabled: Arc<AtomicBool>,
}

impl ScreenViewerWindow {
    pub fn spawn(
        input_tx: UnboundedSender<ScreenInputPayload>,
        device: Option<ID3D11Device>,
        context: Option<ID3D11DeviceContext>,
    ) -> Self {
        let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel::<isize>();
        let frame_mailbox = Arc::new(FrameMailbox {
            latest: Mutex::new(None),
            message_pending: AtomicBool::new(false),
        });
        let frame_mailbox_for_window = Arc::clone(&frame_mailbox);
        let gpu_enabled = Arc::new(AtomicBool::new(false));
        let gpu_enabled_for_window = Arc::clone(&gpu_enabled);

        let thread = std::thread::spawn(move || unsafe {
            let com_guard = ComMtaGuard::initialize().ok();
            let class_name = w!("ZeroLinkScreenViewerWindowClass");
            let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

            let wnd_class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(viewer_wnd_proc),
                hInstance: HINSTANCE(std::ptr::null_mut()),
                hCursor: cursor,
                hbrBackground: HBRUSH(16 as *mut core::ffi::c_void), // COLOR_BTNFACE + 1
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassExW(&wnd_class);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("ZeroLink Screen Viewer \u{2014} click/drag to control Android"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                480,
                900,
                HWND(std::ptr::null_mut()),
                HMENU(std::ptr::null_mut()),
                HINSTANCE(std::ptr::null_mut()),
                None,
            )
            .expect("Failed to create ZeroLink screen viewer window");

            let gpu_renderer = match (com_guard.is_some(), device, context) {
                (true, Some(device), Some(context)) => {
                    match D3dScreenPresenter::new(hwnd, device, context) {
                        Ok(renderer) => {
                            gpu_enabled_for_window.store(true, Ordering::Release);
                            tracing::info!("Direct D3D11 screen presentation enabled");
                            Some(renderer)
                        }
                        Err(error) => {
                            tracing::info!(
                                "Direct D3D11 presentation unavailable; using CPU/GDI: {error}"
                            );
                            None
                        }
                    }
                }
                _ => None,
            };

            let state = Box::new(ViewerState {
                input_tx,
                dragging: false,
                remote_width: 0,
                remote_height: 0,
                latest_frame: None,
                frame_mailbox: frame_mailbox_for_window,
                gpu_renderer,
                gpu_enabled: gpu_enabled_for_window,
            });
            let state_ptr = Box::into_raw(state);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = hwnd_tx.send(hwnd.0 as isize);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState;
            if !state_ptr.is_null() {
                let _ = Box::from_raw(state_ptr);
            }
        });

        let hwnd_val = hwnd_rx
            .recv()
            .expect("Screen viewer thread failed to produce an HWND");
        Self {
            hwnd_val,
            thread: Some(thread),
            frame_mailbox,
            gpu_enabled,
        }
    }

    pub fn gpu_presentation_enabled(&self) -> bool {
        self.gpu_enabled.load(Ordering::Acquire)
    }

    pub fn close(&mut self) {
        if let Some(thread) = self.thread.take() {
            unsafe {
                let _ = PostMessageW(
                    HWND(self.hwnd_val as *mut core::ffi::c_void),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            let _ = thread.join();
        }
    }

    pub fn update_remote_dimensions(&self, width: u16, height: u16) {
        if width == 0 || height == 0 {
            return;
        }
        unsafe {
            let _ = PostMessageW(
                HWND(self.hwnd_val as *mut core::ffi::c_void),
                WM_APP_UPDATE_REMOTE_SIZE,
                WPARAM(width as usize),
                LPARAM(height as isize),
            );
        }
    }

    pub fn present_frame(&self, frame: DecodedFrame) {
        if let Ok(mut latest) = self.frame_mailbox.latest.lock() {
            *latest = Some(frame);
        } else {
            return;
        }
        if !self
            .frame_mailbox
            .message_pending
            .swap(true, Ordering::AcqRel)
        {
            let posted = unsafe {
                PostMessageW(
                    HWND(self.hwnd_val as *mut core::ffi::c_void),
                    WM_APP_PRESENT_FRAME,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
            if posted.is_err() {
                self.frame_mailbox
                    .message_pending
                    .store(false, Ordering::Release);
            }
        }
    }
}

impl Drop for ScreenViewerWindow {
    fn drop(&mut self) {
        self.close();
    }
}

unsafe extern "system" fn viewer_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState;

    if !state_ptr.is_null() {
        let state = &mut *state_ptr;
        match msg {
            WM_APP_UPDATE_REMOTE_SIZE => {
                state.remote_width = wparam.0.min(u16::MAX as usize) as u16;
                state.remote_height = lparam.0.max(0).min(u16::MAX as isize) as u16;
                return LRESULT(0);
            }
            WM_APP_PRESENT_FRAME => {
                state
                    .frame_mailbox
                    .message_pending
                    .store(false, Ordering::Release);
                if let Ok(mut latest) = state.frame_mailbox.latest.lock() {
                    if let Some(frame) = latest.take() {
                        state.remote_width = frame.width.min(u16::MAX as u32) as u16;
                        state.remote_height = frame.height.min(u16::MAX as u32) as u16;
                        state.latest_frame = Some(frame);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
                return LRESULT(0);
            }
            WM_PAINT => {
                paint_latest_frame(hwnd, state);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                state.dragging = true;
                SetCapture(hwnd);
                send_screen_input(hwnd, state, ScreenInputAction::Down, lparam, 0);
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                if state.dragging {
                    send_screen_input(hwnd, state, ScreenInputAction::Move, lparam, 0);
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                state.dragging = false;
                let _ = ReleaseCapture();
                send_screen_input(hwnd, state, ScreenInputAction::Up, lparam, 0);
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let mut point = POINT {
                    x: (lparam.0 & 0xFFFF) as u16 as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut point);
                if let Some((x, y)) = normalize_point(
                    hwnd,
                    point.x,
                    point.y,
                    state.remote_width,
                    state.remote_height,
                ) {
                    let _ = state.input_tx.send(ScreenInputPayload {
                        action: ScreenInputAction::Scroll,
                        x,
                        y,
                        scroll_delta: delta,
                    });
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn paint_latest_frame(hwnd: HWND, state: &mut ViewerState) {
    let mut paint = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut paint);
    if let Some(frame) = state.latest_frame.as_ref() {
        if let (Some(surface), Some(renderer)) =
            (frame.surface.as_ref(), state.gpu_renderer.as_mut())
        {
            match renderer.present(hwnd, surface, frame.width, frame.height) {
                Ok(()) => {
                    let _ = EndPaint(hwnd, &paint);
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        "Direct D3D11 frame presentation failed; switching to CPU/GDI: {error}"
                    );
                    state.gpu_enabled.store(false, Ordering::Release);
                    state.gpu_renderer = None;
                }
            }
        }
        if frame.bgra.is_empty() {
            let _ = EndPaint(hwnd, &paint);
            return;
        }
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() && frame.width > 0 && frame.height > 0 {
            let client_width = (rect.right - rect.left).max(1);
            let client_height = (rect.bottom - rect.top).max(1);
            let scale = (client_width as f32 / frame.width as f32)
                .min(client_height as f32 / frame.height as f32);
            let draw_width = (frame.width as f32 * scale).round().max(1.0) as i32;
            let draw_height = (frame.height as f32 * scale).round().max(1.0) as i32;
            let draw_x = (client_width - draw_width) / 2;
            let draw_y = (client_height - draw_height) / 2;
            let bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: frame.width as i32,
                    biHeight: -(frame.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: frame.bgra.len().min(u32::MAX as usize) as u32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let _ = SetStretchBltMode(hdc, HALFTONE);
            let _ = StretchDIBits(
                hdc,
                draw_x,
                draw_y,
                draw_width,
                draw_height,
                0,
                0,
                frame.width as i32,
                frame.height as i32,
                Some(frame.bgra.as_ptr().cast()),
                &bitmap_info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    let _ = EndPaint(hwnd, &paint);
}

struct ComMtaGuard(bool);

impl ComMtaGuard {
    fn initialize() -> std::result::Result<Self, String> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            Ok(Self(true))
        } else {
            Err(result.to_string())
        }
    }
}

impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

unsafe fn send_screen_input(
    hwnd: HWND,
    state: &ViewerState,
    action: ScreenInputAction,
    lparam: LPARAM,
    scroll_delta: i16,
) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;

    if let Some((normalized_x, normalized_y)) =
        normalize_point(hwnd, x, y, state.remote_width, state.remote_height)
    {
        let _ = state.input_tx.send(ScreenInputPayload {
            action,
            x: normalized_x,
            y: normalized_y,
            scroll_delta,
        });
    }
}

unsafe fn normalize_point(
    hwnd: HWND,
    x: i32,
    y: i32,
    remote_width: u16,
    remote_height: u16,
) -> Option<(f32, f32)> {
    let mut rect = RECT::default();
    GetClientRect(hwnd, &mut rect).ok()?;

    let width = (rect.right - rect.left).max(1) as f32;
    let height = (rect.bottom - rect.top).max(1) as f32;

    normalize_aspect_point(
        x as f32,
        y as f32,
        width,
        height,
        remote_width,
        remote_height,
    )
}

fn normalize_aspect_point(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    remote_width: u16,
    remote_height: u16,
) -> Option<(f32, f32)> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    if remote_width == 0 || remote_height == 0 {
        return Some(((x / width).clamp(0.0, 1.0), (y / height).clamp(0.0, 1.0)));
    }

    let remote_aspect = remote_width as f32 / remote_height as f32;
    let client_aspect = width / height;
    let (view_x, view_y, view_width, view_height) = if client_aspect > remote_aspect {
        let view_width = height * remote_aspect;
        ((width - view_width) / 2.0, 0.0, view_width, height)
    } else {
        let view_height = width / remote_aspect;
        (0.0, (height - view_height) / 2.0, width, view_height)
    };

    let normalized_x = (x - view_x) / view_width;
    let normalized_y = (y - view_y) / view_height;
    if !(0.0..=1.0).contains(&normalized_x) || !(0.0..=1.0).contains(&normalized_y) {
        return None;
    }
    Some((normalized_x, normalized_y))
}

#[cfg(test)]
mod input_mapping_tests {
    use super::normalize_aspect_point;

    #[test]
    fn maps_points_inside_letterboxed_portrait_view() {
        let center = normalize_aspect_point(300.0, 450.0, 600.0, 900.0, 1080, 2400).unwrap();
        let left_edge = normalize_aspect_point(97.5, 450.0, 600.0, 900.0, 1080, 2400).unwrap();
        assert!((center.0 - 0.5).abs() < 0.001);
        assert!((center.1 - 0.5).abs() < 0.001);
        assert!(left_edge.0.abs() < 0.001);
        assert!(normalize_aspect_point(20.0, 450.0, 600.0, 900.0, 1080, 2400).is_none());
    }

    #[test]
    fn defaults_to_full_client_area_without_remote_dimensions() {
        let point = normalize_aspect_point(25.0, 75.0, 100.0, 100.0, 0, 0).unwrap();
        assert_eq!(point, (0.25, 0.75));
        assert!(normalize_aspect_point(0.0, 0.0, 0.0, 10.0, 1, 1).is_none());
    }
}
