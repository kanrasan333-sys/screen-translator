//! Shared colour palette (COLORREF = 0x00BBGGRR).
//!
//! Values track macOS dark appearance: a near-black window, grouped content on
//! a slightly raised card, hairline separators, and label colours that step
//! down in three levels of emphasis rather than three shades of grey picked by
//! eye.  Used by the popup, the capture overlay and the settings window.

/// Window background — macOS `windowBackgroundColor`, #1E1E1E.
pub const CLR_BG: u32 = 0x001E_1E1E;

/// Grouped-list card sitting on the window background, #2C2C2E.
pub const CLR_CARD: u32 = 0x002E_2C2C;

/// Hairline between rows of a card, #38383A.
pub const CLR_SEPARATOR: u32 = 0x003A_3838;

/// System blue, dark appearance — #0A84FF.
pub const CLR_ACCENT: u32 = 0x00FF_840A;

/// Primary label, #F5F5F7.
pub const CLR_TEXT_BRIGHT: u32 = 0x00F7_F5F5;
/// Body text, a step down from primary.
pub const CLR_TEXT: u32 = 0x00E6_E4E4;
/// Secondary label — group titles, disabled values, #98989D.
pub const CLR_TEXT_DIM: u32 = 0x009D_9898;
/// Tertiary label — footnotes and hints, #8E8E93.
pub const CLR_HINT: u32 = 0x0093_8E8E;

/// Text-field and popup-list background, recessed below the card, #1C1C1E.
pub const CLR_FIELD: u32 = 0x001E_1C1C;
/// Text-field border at rest, #48484A.
pub const CLR_FIELD_BORDER: u32 = 0x004A_4848;

/// Raised control fill — secondary buttons, switch tracks.  Light enough to
/// read as a control against a card rather than a hole cut into it.
pub const CLR_CTRL_TOP: u32 = 0x0058_5858;
pub const CLR_CTRL_BOTTOM: u32 = 0x004A_4A4A;
pub const CLR_CTRL_BORDER: u32 = 0x0062_6262;

/// Cast by raised controls on whatever is behind them.
pub const CLR_SHADOW: u32 = 0x0014_1414;

/// Asks DWM for a dark caption.  Without it the system paints a white title
/// bar on top of a dark window, which is the first thing anyone notices.
///
/// The attribute id moved from 19 to 20 in Windows 10 20H1; both are tried
/// because setting the wrong one simply fails.
pub unsafe fn dark_titlebar(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{BOOL, TRUE};
    use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
    unsafe {
        let on: BOOL = TRUE;
        for attr in [20u32, 19] {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(attr as i32),
                &on as *const _ as *const core::ffi::c_void,
                size_of::<BOOL>() as u32,
            );
        }
    }
}

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
