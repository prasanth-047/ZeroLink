use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LoadCursorW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowTextW, ShowWindow, TranslateMessage, CW_USEDEFAULT, HMENU, IDC_ARROW, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_DESTROY, WM_USER, WNDCLASSEXW, WS_BORDER,
    WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    WS_VSCROLL,
};

pub const ID_BTN_OPEN_PHOTOS: usize = 2001;
pub const ID_BTN_TEST_TOAST: usize = 2002;
pub const ID_BTN_TEST_CLIPBOARD: usize = 2003;
pub const ID_BTN_SCREEN_MIRROR: usize = 2004;
pub const ID_BTN_FORGET_DEVICE: usize = 2005;

pub const WM_APP_APPEND_LOG: u32 = WM_USER + 101;
pub const WM_APP_UPDATE_STATUS: u32 = WM_USER + 102;

// Win32 Edit control messages
const EM_SETSEL: u32 = 0x00B1;
const EM_REPLACESEL: u32 = 0x00C2;

#[derive(Debug, Clone)]
pub enum UiCommand {
    OpenPhotosFolder,
    TriggerTestToast,
    TriggerTestClipboard,
    ToggleScreenMirror,
    ForgetPairedDevice,
    Quit,
}

#[derive(Clone)]
pub struct UiHandle {
    hwnd_val: isize,
    is_running: Arc<AtomicBool>,
}

unsafe impl Send for UiHandle {}
unsafe impl Sync for UiHandle {}

impl UiHandle {
    fn hwnd(&self) -> HWND {
        HWND(self.hwnd_val as *mut core::ffi::c_void)
    }

    pub fn log(&self, text: &str) {
        if !self.is_running.load(Ordering::Relaxed) {
            return;
        }
        let now = chrono_timestamp();
        let formatted = format!("[{}] {}\0", now, text);
        let wide: Vec<u16> = formatted.encode_utf16().collect();
        let ptr = Box::into_raw(Box::new(wide));
        unsafe {
            let _ = PostMessageW(
                self.hwnd(),
                WM_APP_APPEND_LOG,
                WPARAM(ptr as usize),
                LPARAM(0),
            );
        }
    }

    pub fn update_device_status(&self, name: &str, battery: u8, connected: bool) {
        if !self.is_running.load(Ordering::Relaxed) {
            return;
        }
        let info = format!("{}|{}|{}\0", name, battery, if connected { 1 } else { 0 });
        let wide: Vec<u16> = info.encode_utf16().collect();
        let ptr = Box::into_raw(Box::new(wide));
        unsafe {
            let _ = PostMessageW(
                self.hwnd(),
                WM_APP_UPDATE_STATUS,
                WPARAM(ptr as usize),
                LPARAM(0),
            );
        }
    }
}

fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs() % 86400;
    let hours = (secs / 3600) % 24;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, s)
}

struct WindowState {
    cmd_tx: UnboundedSender<UiCommand>,
    _photos_dir: PathBuf,
    log_edit: HWND,
    status_label: HWND,
    device_label: HWND,
    battery_label: HWND,
}

pub fn spawn_ui_thread(
    cmd_tx: UnboundedSender<UiCommand>,
    photos_dir: PathBuf,
) -> (UiHandle, std::thread::JoinHandle<()>) {
    let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel::<isize>();
    let is_running = Arc::new(AtomicBool::new(true));
    let is_running_clone = Arc::clone(&is_running);

    let handle = std::thread::spawn(move || unsafe {
        let class_name = w!("ZeroLinkDashboardWindowClass");
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: std::mem::size_of::<usize>() as i32,
            hInstance: HINSTANCE(std::ptr::null_mut()),
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
            hCursor: cursor,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(16 as *mut core::ffi::c_void), // COLOR_BTNFACE + 1
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_name,
            hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON(std::ptr::null_mut()),
        };

        RegisterClassExW(&wnd_class);

        let window_title = w!("ZeroLink \u{2014} Android & Windows Native Fabric");
        let width = 740;
        let height = 560;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            window_title,
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            HWND(std::ptr::null_mut()),
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        )
        .expect("Failed to create ZeroLink UI Window");

        // Header Title
        let _title_label = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ZeroLink \u{2014} Android & Windows Native Fabric (v0.1.0)"),
            WS_CHILD | WS_VISIBLE,
            24,
            16,
            680,
            22,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        // Subtitle
        let _sub_label = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Android & Windows link: BLE L2CAP CoC + local TCP / Wi-Fi transfer"),
            WS_CHILD | WS_VISIBLE,
            24,
            40,
            680,
            20,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        // Status Line
        let status_label = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Status: Listening on 0.0.0.0:8989 | BLE Standby"),
            WS_CHILD | WS_VISIBLE,
            24,
            72,
            680,
            20,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        )
        .unwrap();

        // Target Device Line
        let device_label = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Target Device: Scanning for ZeroLink BLE (UUID 0xFE2C)..."),
            WS_CHILD | WS_VISIBLE,
            24,
            96,
            440,
            20,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        )
        .unwrap();

        // Battery Line
        let battery_label = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Battery: --% | Wi-Fi: Standby"),
            WS_CHILD | WS_VISIBLE,
            470,
            96,
            230,
            20,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        )
        .unwrap();

        // Action Buttons
        let _btn_open = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Open ZeroLink Photos"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            24,
            130,
            155,
            32,
            hwnd,
            HMENU(ID_BTN_OPEN_PHOTOS as *mut core::ffi::c_void),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        let _btn_toast = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Test Notification"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            190,
            130,
            155,
            32,
            hwnd,
            HMENU(ID_BTN_TEST_TOAST as *mut core::ffi::c_void),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        let _btn_clip = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Test Clipboard Sync"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            356,
            130,
            155,
            32,
            hwnd,
            HMENU(ID_BTN_TEST_CLIPBOARD as *mut core::ffi::c_void),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        let _btn_screen = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Screen Mirroring"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            522,
            130,
            155,
            32,
            hwnd,
            HMENU(ID_BTN_SCREEN_MIRROR as *mut core::ffi::c_void),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        let _btn_forget = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("Forget Paired Device"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            24,
            166,
            155,
            32,
            hwnd,
            HMENU(ID_BTN_FORGET_DEVICE as *mut core::ffi::c_void),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        // Activity Log Label
        let _log_hdr = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("Live Activity & Security Telemetry:"),
            WS_CHILD | WS_VISIBLE,
            24,
            208,
            680,
            20,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        );

        // Live Log Edit box (Read-only, multi-line, vertical scroll)
        // ES_MULTILINE = 0x0004, ES_AUTOVSCROLL = 0x0040, ES_READONLY = 0x0800
        let log_edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_BORDER | WINDOW_STYLE(0x0004 | 0x0040 | 0x0800),
            24,
            232,
            675,
            270,
            hwnd,
            HMENU(std::ptr::null_mut()),
            HINSTANCE(std::ptr::null_mut()),
            None,
        )
        .unwrap();

        let state = Box::new(WindowState {
            cmd_tx,
            _photos_dir: photos_dir,
            log_edit,
            status_label,
            device_label,
            battery_label,
        });

        // Store state pointer in window user data
        let state_ptr = Box::into_raw(state);
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            state_ptr as isize,
        );

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        hwnd_tx.send(hwnd.0 as isize).unwrap();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Clean up state
        let state_ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *mut WindowState;
        if !state_ptr.is_null() {
            let _ = Box::from_raw(state_ptr);
        }

        is_running_clone.store(false, Ordering::Relaxed);
    });

    let hwnd_val = hwnd_rx.recv().expect("UI thread failed to produce HWND");
    (
        UiHandle {
            hwnd_val,
            is_running,
        },
        handle,
    )
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
    ) as *mut WindowState;

    match msg {
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                match id {
                    ID_BTN_OPEN_PHOTOS => {
                        let _ = state.cmd_tx.send(UiCommand::OpenPhotosFolder);
                    }
                    ID_BTN_TEST_TOAST => {
                        let _ = state.cmd_tx.send(UiCommand::TriggerTestToast);
                    }
                    ID_BTN_TEST_CLIPBOARD => {
                        let _ = state.cmd_tx.send(UiCommand::TriggerTestClipboard);
                    }
                    ID_BTN_SCREEN_MIRROR => {
                        let _ = state.cmd_tx.send(UiCommand::ToggleScreenMirror);
                    }
                    ID_BTN_FORGET_DEVICE => {
                        let _ = state.cmd_tx.send(UiCommand::ForgetPairedDevice);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }

        WM_APP_APPEND_LOG => {
            let ptr = wparam.0 as *mut Vec<u16>;
            if !ptr.is_null() {
                let wide = Box::from_raw(ptr);
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let line_with_break: Vec<u16> = wide
                        .iter()
                        .take_while(|&&c| c != 0)
                        .copied()
                        .chain([b'\r' as u16, b'\n' as u16, 0])
                        .collect();

                    // Move caret to end
                    let _ = SendMessageW(state.log_edit, EM_SETSEL, WPARAM(0xFFFF), LPARAM(0xFFFF));
                    // Append text
                    let _ = SendMessageW(
                        state.log_edit,
                        EM_REPLACESEL,
                        WPARAM(0),
                        LPARAM(line_with_break.as_ptr() as _),
                    );
                }
            }
            LRESULT(0)
        }

        WM_APP_UPDATE_STATUS => {
            let ptr = wparam.0 as *mut Vec<u16>;
            if !ptr.is_null() {
                let wide = Box::from_raw(ptr);
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let text = String::from_utf16_lossy(&wide);
                    let parts: Vec<&str> = text.split('|').collect();
                    if parts.len() >= 3 {
                        let name = parts[0];
                        let battery = parts[1];
                        let is_connected = parts[2].trim() == "1";

                        let status_str = if is_connected {
                            "Status: Connected | Dual-Radio Active\0".to_string()
                        } else {
                            "Status: Listening on 0.0.0.0:8989 | BLE Standby\0".to_string()
                        };
                        let dev_str = format!("Device: {}\0", name);
                        let batt_str = format!("Battery: {}% | Wi-Fi: Ready\0", battery);

                        let s_wide: Vec<u16> = status_str.encode_utf16().collect();
                        let d_wide: Vec<u16> = dev_str.encode_utf16().collect();
                        let b_wide: Vec<u16> = batt_str.encode_utf16().collect();

                        let _ = SetWindowTextW(state.status_label, PCWSTR(s_wide.as_ptr()));
                        let _ = SetWindowTextW(state.device_label, PCWSTR(d_wide.as_ptr()));
                        let _ = SetWindowTextW(state.battery_label, PCWSTR(b_wide.as_ptr()));
                    }
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let _ = state.cmd_tx.send(UiCommand::Quit);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
