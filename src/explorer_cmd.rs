//! Windows Explorer integration:
//!   (A) "Open CMD here" context-menu items registered under
//!       HKCU\Software\Classes — no admin rights required, no DLL.
//!   (B) Opens cmd.exe in the folder shown by the foreground Explorer
//!       window (falls back to %USERPROFILE% if the foreground window
//!       isn't an Explorer window).

use crate::utils::to_wide;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, VARIANT};

#[derive(Clone, Debug)]
pub struct ExplorerWindowInfo {
    pub hwnd: isize,
    pub path: String,
}

// ============================================================
// Registry-backed context-menu integration (Variant A)
// ============================================================

/// Sub-key we own inside HKCU\Software\Classes. Unique enough to avoid
/// clashing with anything else.
const MENU_KEY_NAME: &str = "ScreenTranslatorOpenCmd";

/// User-visible caption in the context menu.
const MENU_TITLE: &str = "Open CMD here";

/// `%V` expands to the folder path, `/s` lets the quoted path contain
/// spaces, `/k` keeps the CMD window open after the `pushd`. `pushd`
/// (vs `cd /d`) also handles UNC paths correctly.
const MENU_COMMAND: &str = "cmd.exe /s /k pushd \"%V\"";

/// Roots where we install the menu:
///   - Directory             → right-click on a folder
///   - Directory\Background  → right-click on empty space inside a folder
///   - Drive                 → right-click on a drive letter
const ROOTS: &[&str] = &[
    "Software\\Classes\\Directory",
    "Software\\Classes\\Directory\\Background",
    "Software\\Classes\\Drive",
];

/// Returns `true` if the first of the three menu entries exists. We
/// treat that as the on/off signal — the three keys are written and
/// deleted together, so one probe is enough.
pub fn is_menu_enabled() -> bool {
    unsafe {
        let path = format!("{}\\shell\\{}", ROOTS[0], MENU_KEY_NAME);
        let wide = to_wide(&path);
        let mut hkey = HKEY::default();
        let err = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if err.0 != 0 {
            return false;
        }
        let _ = RegCloseKey(hkey);
        true
    }
}

/// Installs or removes the three context-menu entries.
pub fn set_menu_enabled(enable: bool) {
    for root in ROOTS {
        unsafe {
            if enable {
                install_one(root);
            } else {
                remove_one(root);
            }
        }
    }
}

unsafe fn install_one(root: &str) {
    unsafe {
        // HKCU\{root}\shell\ScreenTranslatorOpenCmd
        let shell_path = format!("{root}\\shell\\{MENU_KEY_NAME}");
        let Some(hkey) = create_key(&shell_path) else {
            return;
        };
        set_default_string(hkey, MENU_TITLE);
        set_value(hkey, "Icon", "cmd.exe");
        let _ = RegCloseKey(hkey);

        // HKCU\{root}\shell\ScreenTranslatorOpenCmd\command
        let cmd_path = format!("{root}\\shell\\{MENU_KEY_NAME}\\command");
        let Some(cmd_key) = create_key(&cmd_path) else {
            return;
        };
        set_default_string(cmd_key, MENU_COMMAND);
        let _ = RegCloseKey(cmd_key);
    }
}

unsafe fn remove_one(root: &str) {
    unsafe {
        // Delete leaf first — RegDeleteKeyW fails if subkeys exist.
        let cmd_path = format!("{root}\\shell\\{MENU_KEY_NAME}\\command");
        let wide = to_wide(&cmd_path);
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()));

        let shell_path = format!("{root}\\shell\\{MENU_KEY_NAME}");
        let wide = to_wide(&shell_path);
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()));
    }
}

/// Creates-or-opens the given sub-key under HKCU with default access
/// (sufficient for HKCU; no SECURITY_ATTRIBUTES needed).
unsafe fn create_key(subpath: &str) -> Option<HKEY> {
    unsafe {
        let wide = to_wide(subpath);
        let mut hkey = HKEY::default();
        let err = RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()), &mut hkey);
        if err.0 != 0 {
            return None;
        }
        Some(hkey)
    }
}

unsafe fn set_default_string(hkey: HKEY, value: &str) {
    unsafe { write_string(hkey, PCWSTR::null(), value) }
}

unsafe fn set_value(hkey: HKEY, name: &str, value: &str) {
    unsafe {
        let name_wide = to_wide(name);
        write_string(hkey, PCWSTR(name_wide.as_ptr()), value);
    }
}

unsafe fn write_string(hkey: HKEY, name: PCWSTR, value: &str) {
    unsafe {
        let value_wide = to_wide(value);
        let bytes =
            std::slice::from_raw_parts(value_wide.as_ptr() as *const u8, value_wide.len() * 2);
        let _ = RegSetValueExW(hkey, name, 0, REG_SZ, Some(bytes));
    }
}

// ============================================================
// Hotkey action: open CMD in the active Explorer folder (Variant B)
// ============================================================

/// Opens CMD in the folder shown by the foreground Explorer window.
/// Falls back to the user's home directory when no Explorer window
/// is focused or the path can't be resolved.
pub fn open_in_active_folder() {
    let path = active_explorer_path().unwrap_or_else(home_dir);
    open_cmd_in(&path);
}

/// Returns all open filesystem-backed Explorer windows.
pub fn list_open_folders() -> Vec<ExplorerWindowInfo> {
    unsafe {
        let shell_windows: IShellWindows = match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let Ok(count) = shell_windows.Count() else {
            return Vec::new();
        };

        let mut items = Vec::new();
        for i in 0..count {
            let Ok(disp) = shell_windows.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(app) = disp.cast::<IWebBrowserApp>() else {
                continue;
            };
            let Ok(hwnd_handle) = app.HWND() else {
                continue;
            };
            let Some(path) = current_folder_for_app(&app) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            items.push(ExplorerWindowInfo {
                hwnd: hwnd_handle.0,
                path,
            });
        }
        items
    }
}

fn home_dir() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
}

fn open_cmd_in(folder: &str) {
    unsafe {
        let verb = to_wide("open");
        let exe = to_wide("cmd.exe");
        let dir = to_wide(folder);
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(exe.as_ptr()),
            PCWSTR::null(),
            PCWSTR(dir.as_ptr()),
            SW_SHOWNORMAL,
        );
    }
}

/// Walks the `IShellWindows` collection, finds the entry whose HWND
/// matches `GetForegroundWindow()`, and returns the filesystem path of
/// its active folder.
fn active_explorer_path() -> Option<String> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    let fg_value = foreground.0 as isize;
    list_open_folders()
        .into_iter()
        .find(|item| item.hwnd == fg_value)
        .map(|item| item.path)
}

fn current_folder_for_app(app: &IWebBrowserApp) -> Option<String> {
    unsafe {
        let Ok(sp) = app.cast::<IServiceProvider>() else {
            return None;
        };
        let Ok(browser) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
            return None;
        };
        let Ok(view) = browser.QueryActiveShellView() else {
            return None;
        };
        let Ok(folder_view) = view.cast::<IFolderView>() else {
            return None;
        };
        let Ok(persist) = folder_view.GetFolder::<IPersistFolder2>() else {
            return None;
        };
        let Ok(pidl) = persist.GetCurFolder() else {
            return None;
        };

        let mut buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(pidl, &mut buf);
        CoTaskMemFree(Some(pidl as *const ITEMIDLIST as *const _));
        if !ok.as_bool() {
            return None;
        }

        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let path = String::from_utf16_lossy(&buf[..len]);
        if path.is_empty() { None } else { Some(path) }
    }
}
