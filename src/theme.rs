//! Shared palette, fonts and window chrome helpers (COLORREF = 0x00BBGGRR).
//!
//! One neutral dark scale — a slightly cool zinc rather than pure grey, so the
//! surfaces read as a family instead of four greys picked by eye — with a
//! single blue accent and the three system status colours.  Every surface
//! draws from here: the settings window, the translation popup, the capture
//! overlay and the ask window, so a change here changes the whole app.

use std::sync::Mutex;
use windows::Win32::Foundation::{BOOL, HWND, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::core::w;

// ============================================================
// Palette
// ============================================================

/// Window background, #1F1F23.
pub const CLR_BG: u32 = 0x0023_1F1F;
/// Navigation column, a step below the window background, #18181B.
pub const CLR_SIDEBAR: u32 = 0x001B_1818;
/// Grouped-list card sitting on the window background, #29292E.
pub const CLR_CARD: u32 = 0x002E_2929;
/// Floating surfaces — the translation popup — #27272C.
pub const CLR_ELEVATED: u32 = 0x002C_2727;
/// Hairline between rows of a card, and around floating surfaces, #35353B.
pub const CLR_SEPARATOR: u32 = 0x003B_3535;

/// System blue, dark appearance — #0A84FF.
pub const CLR_ACCENT: u32 = 0x00FF_840A;

/// Primary label, #F4F4F5.
pub const CLR_TEXT_BRIGHT: u32 = 0x00F5_F4F4;
/// Body text, a step down from primary, #E4E4E7.
pub const CLR_TEXT: u32 = 0x00E7_E4E4;
/// Secondary label — group titles, disabled values, #A1A1AA.
pub const CLR_TEXT_DIM: u32 = 0x00AA_A1A1;
/// Tertiary label — footnotes and hints, #8A8A93.
pub const CLR_HINT: u32 = 0x0093_8A8A;

/// Text-field and popup-list background, recessed below the card, #1C1C20.
pub const CLR_FIELD: u32 = 0x0020_1C1C;
/// Text-field border at rest, #3F3F46.
pub const CLR_FIELD_BORDER: u32 = 0x0046_3F3F;

/// Neutral control fill — secondary buttons, switch tracks, keycaps, #3A3A41.
pub const CLR_CTRL: u32 = 0x0041_3A3A;

/// Navigation row under the pointer, and the selected one.
pub const CLR_NAV_HOVER: u32 = 0x002B_2626;
pub const CLR_NAV_SELECTED: u32 = 0x0034_2E2E;

// Status colours: system green, red and orange, dark appearance.
pub const CLR_GREEN: u32 = 0x0058_D130;
pub const CLR_RED: u32 = 0x003A_45FF;
pub const CLR_ORANGE: u32 = 0x000A_9FFF;

/// Windows' own close-button red, for the one place we draw a caption button.
pub const CLR_CLOSE_HOVER: u32 = 0x001C_2BC4;

// ============================================================
// Colour arithmetic
// ============================================================

/// Shifts every channel up by `amount`, clamping at full.
pub fn lighten(c: u32, amount: u32) -> u32 {
    let r = ((c & 0xFF) + amount).min(0xFF);
    let g = (((c >> 8) & 0xFF) + amount).min(0xFF);
    let b = (((c >> 16) & 0xFF) + amount).min(0xFF);
    r | (g << 8) | (b << 16)
}

/// Shifts every channel down by `amount`, clamping at zero.
pub fn darken(c: u32, amount: u32) -> u32 {
    let r = (c & 0xFF).saturating_sub(amount);
    let g = ((c >> 8) & 0xFF).saturating_sub(amount);
    let b = ((c >> 16) & 0xFF).saturating_sub(amount);
    r | (g << 8) | (b << 16)
}

/// `a` blended towards `b` by `t` (0 = all `a`, 255 = all `b`).
pub fn mix(a: u32, b: u32, t: u32) -> u32 {
    let ch = |shift: u32| {
        let x = (a >> shift) & 0xFF;
        let y = (b >> shift) & 0xFF;
        ((x * (255 - t) + y * t) / 255) << shift
    };
    ch(0) | ch(8) | ch(16)
}

// ============================================================
// Fonts — created once per (size, weight) and kept for the process lifetime.
// A handful of distinct faces exist in the whole app, so the cache never grows
// past a dozen entries and there's nothing to free.
// ============================================================

static FONTS: Mutex<Vec<(i32, i32, bool, isize)>> = Mutex::new(Vec::new());

fn cached_font(height: i32, weight: i32, icons: bool) -> HFONT {
    let mut cache = FONTS.lock().unwrap();
    if let Some(&(_, _, _, f)) = cache
        .iter()
        .find(|&&(h, w, i, _)| h == height && w == weight && i == icons)
    {
        return HFONT(f as *mut _);
    }
    let face = if icons {
        w!("Segoe MDL2 Assets")
    } else {
        w!("Segoe UI")
    };
    // CLEARTYPE_QUALITY (5): the one GDI setting that makes text look current.
    let f = unsafe { CreateFontW(-height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face) };
    cache.push((height, weight, icons, f.0 as isize));
    f
}

/// Segoe UI at `height` px (cell height, not points) and `weight`.
pub fn ui_font(height: i32, weight: i32) -> HFONT {
    cached_font(height, weight, false)
}

/// Segoe MDL2 Assets — the Windows 10 icon font — at `height` px.
pub fn icon_font(height: i32) -> HFONT {
    cached_font(height, 400, true)
}

// Glyphs from Segoe MDL2 Assets used across the app.
pub const ICON_SETTINGS: char = '\u{E713}';
pub const ICON_KEYBOARD: char = '\u{E765}';
pub const ICON_TRANSLATE: char = '\u{E8C1}';
pub const ICON_CHAT: char = '\u{E8BD}';
pub const ICON_EYE: char = '\u{E7B3}';
pub const ICON_CLOSE: char = '\u{E8BB}';
pub const ICON_CHEVRON_DOWN: char = '\u{E70D}';
pub const ICON_COPY: char = '\u{E8C8}';
pub const ICON_CHECK: char = '\u{E73E}';
pub const ICON_INFO: char = '\u{E946}';
pub const ICON_WARNING: char = '\u{E7BA}';

// ============================================================
// Text
// ============================================================

// DrawText flags the windows crate doesn't name as plain integers.
pub const DT_LEFT_VCENTER: u32 = 0x0824; // SINGLELINE | VCENTER | NOPREFIX
pub const DT_CENTER_VCENTER: u32 = 0x0825; // + CENTER
pub const DT_RIGHT_VCENTER: u32 = 0x0826; // + RIGHT
pub const DT_ELLIPSIS: u32 = 0x8000; // END_ELLIPSIS
pub const DT_WRAP: u32 = 0x2810; // WORDBREAK | NOPREFIX | EDITCONTROL
pub const DT_CALC: u32 = 0x0400;

/// Draws `text` into `rc` with a transparent background.
pub unsafe fn text(hdc: HDC, s: &str, rc: &RECT, font: HFONT, color: u32, flags: u32) {
    unsafe {
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        if wide.is_empty() {
            return;
        }
        let old = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(color));
        let mut r = *rc;
        DrawTextW(hdc, &mut wide, &mut r, DRAW_TEXT_FORMAT(flags));
        SelectObject(hdc, old);
    }
}

/// Width and height `text` takes up on one line in `font`.
pub unsafe fn measure(hdc: HDC, s: &str, font: HFONT) -> (i32, i32) {
    unsafe {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let old = SelectObject(hdc, font);
        let mut size = windows::Win32::Foundation::SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
        SelectObject(hdc, old);
        (size.cx, size.cy)
    }
}

/// Height `text` needs when word-wrapped to `width`.
pub unsafe fn measure_wrapped(hdc: HDC, s: &str, font: HFONT, width: i32) -> i32 {
    unsafe {
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        if wide.is_empty() {
            return 0;
        }
        let old = SelectObject(hdc, font);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        DrawTextW(hdc, &mut wide, &mut rc, DRAW_TEXT_FORMAT(DT_WRAP | DT_CALC));
        SelectObject(hdc, old);
        rc.bottom - rc.top
    }
}

/// Draws one icon glyph centred in `rc`.
pub unsafe fn glyph(hdc: HDC, g: char, rc: &RECT, size: i32, color: u32) {
    unsafe { text(hdc, &g.to_string(), rc, icon_font(size), color, DT_CENTER_VCENTER) }
}

// ============================================================
// Window chrome
// ============================================================

unsafe fn dwm_set(hwnd: HWND, attr: u32, value: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(attr as i32),
            &value as *const _ as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
    }
}

/// Asks DWM for a dark caption.  Without it the system paints a white title
/// bar on top of a dark window, which is the first thing anyone notices.
///
/// The attribute id moved from 19 to 20 in Windows 10 20H1; both are tried
/// because setting the wrong one simply fails.
pub unsafe fn dark_titlebar(hwnd: HWND) {
    unsafe {
        let on: BOOL = TRUE;
        for attr in [20u32, 19] {
            dwm_set(hwnd, attr, on.0 as u32);
        }
    }
}

/// Rounded window corners on Windows 11 (DWMWA_WINDOW_CORNER_PREFERENCE =
/// DWMWCP_ROUND).  Windows 10 doesn't know the attribute and ignores it, which
/// is what we want: square corners are native there.
pub unsafe fn round_corners(hwnd: HWND) {
    unsafe { dwm_set(hwnd, 33, 2) }
}

/// Dark context menus — the tray menu included — on Windows 10 1903 and later.
///
/// There is no documented switch for this.  `uxtheme.dll` exports
/// `SetPreferredAppMode` by ordinal only (135), and `FlushMenuThemes` (136)
/// makes it take effect for menus already themed.  Every dark-mode Win32 app
/// goes through the same door; when the ordinal isn't there the lookup fails
/// and menus simply stay light.
pub fn enable_dark_menus() {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::PCSTR;
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(set_mode) = GetProcAddress(ux, PCSTR(135usize as *const u8)) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            set_mode(2); // ForceDark
        }
        if let Some(flush) = GetProcAddress(ux, PCSTR(136usize as *const u8)) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

// ============================================================
// App icon — drawn at runtime, so there's no resource compiler in the build.
// ============================================================

/// The app mark at `size` px: the translate glyph in white on an accent
/// rounded square — the same mark the settings window draws in its corner.
/// The glyph fills most of the square: at tray size every pixel counts.
pub fn app_icon(size: i32) -> HICON {
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
    unsafe {
        let Some(mut px) = crate::paint::render_argb(size, size, |dc, ss| {
            let r = RECT {
                left: 0,
                top: 0,
                right: size * ss,
                bottom: size * ss,
            };
            let rad = size * ss * 2 / 9;
            let rgn = CreateRoundRectRgn(0, 0, r.right + 1, r.bottom + 1, rad * 2, rad * 2);
            let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(CLR_ACCENT));
            let _ = FillRgn(dc, rgn, brush);
            let _ = DeleteObject(brush);
            let _ = DeleteObject(rgn);
            glyph(dc, ICON_TRANSLATE, &r, size * ss * 13 / 16, 0x00FF_FFFF);
        }) else {
            return HICON::default();
        };

        // Icons take straight (not premultiplied) alpha.
        crate::paint::unpremultiply(&mut px);
        let Some(color) = crate::paint::argb_bitmap(size, size, &px) else {
            return HICON::default();
        };
        let mask = CreateBitmap(size, size, 1, 1, None);
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        icon
    }
}
