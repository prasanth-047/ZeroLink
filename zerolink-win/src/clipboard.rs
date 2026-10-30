use std::hash::Hasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassExW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WM_CLIPBOARDUPDATE,
    WM_DESTROY, WNDCLASSEXW,
};
use zerolink_core::protocol::{ClipboardPayload, ClipboardType};

const CF_UNICODETEXT: u32 = 13;

pub struct ClipboardManager {
    last_remote_hash: Arc<AtomicU64>,
    last_local_hash: Arc<AtomicU64>,
    tx_channel: UnboundedSender<ClipboardPayload>,
}

impl ClipboardManager {
    pub fn new(tx_channel: UnboundedSender<ClipboardPayload>) -> Self {
        Self {
            last_remote_hash: Arc::new(AtomicU64::new(0)),
            last_local_hash: Arc::new(AtomicU64::new(0)),
            tx_channel,
        }
    }

    /// Spawns the dedicated Win32 message-only window loop.
    /// Operates at strictly 0.0% CPU when clipboard is untouched.
    pub fn start_listener(&self) -> std::thread::JoinHandle<()> {
        let last_remote = Arc::clone(&self.last_remote_hash);
        let last_local = Arc::clone(&self.last_local_hash);
        let sender = self.tx_channel.clone();

        std::thread::spawn(move || unsafe {
            let class_name = w!("ZeroLinkClipboardListenerClass");
            let window_name = w!("ZeroLinkClipboardListenerWindow");

            let wnd_class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };

            RegisterClassExW(&wnd_class);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                window_name,
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE, // Message-only window
                None,
                None,
                None,
            )
            .expect("CreateWindowExW failed");

            AddClipboardFormatListener(hwnd).expect("AddClipboardFormatListener failed");

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
                if msg.message == WM_CLIPBOARDUPDATE {
                    if let Some(text) = read_clipboard_text(hwnd) {
                        let mut hasher = siphasher::sip::SipHasher24::new();
                        hasher.write(text.as_bytes());
                        let hash = hasher.finish();

                        // Avoid echo loop if clipboard update was triggered by remote sync
                        let remote_h = last_remote.load(Ordering::Relaxed);
                        let local_h = last_local.load(Ordering::Relaxed);

                        if hash != remote_h && hash != local_h {
                            last_local.store(hash, Ordering::Relaxed);
                            let payload = ClipboardPayload {
                                format: ClipboardType::PlainText,
                                payload_hash: hash,
                                timestamp_ms: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64,
                                data: text.into_bytes(),
                            };
                            let _ = sender.send(payload);
                        }
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let _ = RemoveClipboardFormatListener(hwnd);
            let _ = DestroyWindow(hwnd);
        })
    }

    /// Sets clipboard content received from Android, recording hash to prevent echo loops.
    pub fn inject_remote_clipboard(&self, payload: &ClipboardPayload) {
        self.last_remote_hash
            .store(payload.payload_hash, Ordering::Relaxed);

        unsafe {
            if OpenClipboard(HWND(std::ptr::null_mut())).is_err() {
                return;
            }

            let _ = EmptyClipboard();

            let text = String::from_utf8_lossy(&payload.data);
            let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes_needed = utf16.len() * std::mem::size_of::<u16>();

            if let Ok(h_mem) = GlobalAlloc(GMEM_MOVEABLE, bytes_needed) {
                let ptr = GlobalLock(h_mem);
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(
                        utf16.as_ptr() as *const u8,
                        ptr as *mut u8,
                        bytes_needed,
                    );
                    let _ = GlobalUnlock(h_mem);
                    let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(h_mem.0 as _));
                }
            }

            let _ = CloseClipboard();
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn read_clipboard_text(hwnd: HWND) -> Option<String> {
    if OpenClipboard(hwnd).is_err() {
        return None;
    }

    let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
    let hglobal = HGLOBAL(handle.0 as _);
    let size = GlobalSize(hglobal);
    if size < 2 {
        let _ = CloseClipboard();
        return None;
    }

    let ptr = GlobalLock(hglobal);
    if ptr.is_null() {
        let _ = CloseClipboard();
        return None;
    }

    let max_u16 = size / 2;
    let wide_slice = std::slice::from_raw_parts(ptr as *const u16, max_u16);
    let len = wide_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(max_u16);

    let text = String::from_utf16_lossy(&wide_slice[..len]);

    let _ = GlobalUnlock(hglobal);
    let _ = CloseClipboard();
    Some(text)
}
