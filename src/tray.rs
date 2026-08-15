use crate::i18n;
use crate::utils::to_wide;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ============================================================
// Constants
// ============================================================

const WM_TRAYICON: u32 = WM_APP + 100;
const TRAY_UID: u32 = 1;

const IDM_SETTINGS: usize = 3001;
const IDM_EXIT: usize = 3002;

/// Posted to the main thread when the user clicks the tray icon
/// or chooses "Settings" from the context menu.
pub const WM_TRAY_OPEN_SETTINGS: u32 = WM_APP + 1;

// ============================================================
// State
// ============================================================

static TRAY_HWND: Mutex<isize> = Mutex::new(0);

// ============================================================
// Public API
// ============================================================

pub fn create() {
    unsafe {
        let Some(hmodule) = GetModuleHandleW(None).ok() else {
            return;
        };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransTray");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_proc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap_or_default();

        if hwnd.0.is_null() {
            return;
        }
        *TRAY_HWND.lock().unwrap() = hwnd.0 as isize;

        add_icon(hwnd);
    }
}

pub fn destroy() {
    unsafe {
        let v = *TRAY_HWND.lock().unwrap();
        if v == 0 {
            return;
        }
        let hwnd = HWND(v as *mut _);
        remove_icon(hwnd);
        let _ = DestroyWindow(hwnd);
        *TRAY_HWND.lock().unwrap() = 0;
    }
}

// ============================================================
// Icon management
// ============================================================

unsafe fn add_icon(hwnd: HWND) {
    unsafe {
        let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = icon;

        let tip = "Screen Translator";
        for (i, c) in tip.encode_utf16().enumerate() {
            if i >= 127 {
                break;
            }
            nid.szTip[i] = c;
        }

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

unsafe fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

// ============================================================
// Context menu
// ============================================================

unsafe fn show_context_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };

        let text_settings = to_wide(i18n::t("tray.menu.settings"));
        let text_exit = to_wide(i18n::t("tray.menu.exit"));

        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_SETTINGS,
            PCWSTR(text_settings.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, PCWSTR(text_exit.as_ptr()));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        // Required: make the menu dismiss when clicking elsewhere
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        // Standard tray menu fix
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAYICON => {
                let event = lp.0 as u32;
                match event {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                        let _ = PostThreadMessageW(
                            GetCurrentThreadId(),
                            WM_TRAY_OPEN_SETTINGS,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                    WM_RBUTTONUP => {
                        show_context_menu(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wp.0 & 0xFFFF {
                    IDM_SETTINGS => {
                        let _ = PostThreadMessageW(
                            GetCurrentThreadId(),
                            WM_TRAY_OPEN_SETTINGS,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                    IDM_EXIT => {
                        PostQuitMessage(0);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
