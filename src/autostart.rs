use crate::utils::to_wide;
use windows::Win32::System::Registry::*;
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "ScreenTranslator";

pub fn is_enabled() -> bool {
    unsafe {
        let key_path = to_wide(RUN_KEY);
        let value_name = to_wide(VALUE_NAME);

        let mut hkey = HKEY::default();
        let err = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if err.0 != 0 { return false; }

        let exists = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            None,
        ).0 == 0;

        let _ = RegCloseKey(hkey);
        exists
    }
}

pub fn set_enabled(enable: bool) {
    unsafe {
        let key_path = to_wide(RUN_KEY);
        let value_name = to_wide(VALUE_NAME);

        let mut hkey = HKEY::default();
        let err = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if err.0 != 0 { return; }

        if enable {
            if let Ok(exe) = std::env::current_exe() {
                let exe_str = exe.to_string_lossy().to_string();
                let exe_wide = to_wide(&exe_str);
                let bytes = std::slice::from_raw_parts(
                    exe_wide.as_ptr() as *const u8,
                    exe_wide.len() * 2,
                );
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR(value_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(bytes),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}
