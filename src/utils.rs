use windows::Win32::Foundation::LPARAM;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

// ============================================================
// String utilities
// ============================================================

/// Converts a Rust &str to a null-terminated UTF-16 Vec for Win32 APIs.
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Safely truncates a string at a char boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Percent-encodes a string for URL query / form-data.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ============================================================
// Unicode helpers
// ============================================================

/// Returns `true` if the character belongs to the Cyrillic Unicode block.
pub fn is_cyrillic(c: char) -> bool {
    matches!(c, '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}')
}

// ============================================================
// Win32 input helpers
// ============================================================

/// Stamped into `dwExtraInfo` of every keystroke this app injects, so the
/// low-level keyboard hook can recognise its own synthetic input.
///
/// The alternative — counting how many events we expect to see back — silently
/// desyncs whenever the user keeps typing while a replacement is in flight (the
/// counter then eats real keystrokes). A tag is exact and stateless.
pub const INJECTED_TAG: usize = 0x5354_5257; // "STRW"

/// Creates a keyboard `INPUT` for `SendInput` (key press or release).
pub fn make_key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    make_key_input_tagged(vk, key_up, 0)
}

/// Same as [`make_key_input`], but stamps `dwExtraInfo` so our own hook can
/// tell the event apart from real user input.
pub fn make_key_input_tagged(vk: VIRTUAL_KEY, key_up: bool, extra: usize) -> INPUT {
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        dwFlags: if key_up {
            KEYEVENTF_KEYUP
        } else {
            KEYBD_EVENT_FLAGS(0)
        },
        dwExtraInfo: extra,
        ..Default::default()
    };
    input
}

/// Creates a Unicode character `INPUT` for `SendInput`, stamping `dwExtraInfo`
/// so our own hook can tell the event apart from real user input.
pub fn make_unicode_input_tagged(ch: u16, key_up: bool, extra: usize) -> INPUT {
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(0),
        wScan: ch,
        dwFlags: if key_up {
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
        } else {
            KEYEVENTF_UNICODE
        },
        dwExtraInfo: extra,
        ..Default::default()
    };
    input
}

/// Extracts `(x, y)` from a Win32 `LPARAM` (mouse messages, etc.).
pub fn lparam_to_point(lp: LPARAM) -> (i32, i32) {
    let x = (lp.0 & 0xFFFF) as i16 as i32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}
