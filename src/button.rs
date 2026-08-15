//! macOS-style push buttons, shared by every surface that draws one.
//!
//! Recent AppKit buttons are flat.  The gradient-plus-highlight-plus-border
//! stack that used to say "button" is Aqua, and it dates a window instantly:
//! today it's a solid fill, a smooth corner, and a soft shadow to lift it off
//! the surface — nothing else.  The default button is plain accent with no
//! border at all; a secondary button is a light grey with the faintest
//! top-to-bottom fall so it doesn't read as a flat sticker.
//!
//! Only the body is drawn here.  Text is left to the caller: the capture
//! overlay and the settings window measure and position it differently, and
//! each owns its own cached fonts.

use crate::paint;
use crate::theme::{self, darken, lighten};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

/// AppKit's "default button" — the one Return activates — carries the accent
/// colour; every other button is neutral grey.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Normal,
    Hover,
    Pressed,
}

/// Corner radius, kept inside the range AppKit actually uses: about 6 px on a
/// compact button, about 9 px on a tall one.
pub fn radius(h: i32) -> i32 {
    (h / 3).clamp(6, 9)
}

pub fn text_color(variant: Variant, state: State) -> u32 {
    match (variant, state) {
        (Variant::Primary, _) => 0x00FF_FFFF,
        // A pressed grey button dims its label along with its fill.
        (Variant::Secondary, State::Pressed) => theme::CLR_TEXT_DIM,
        (Variant::Secondary, _) => theme::CLR_TEXT_BRIGHT,
    }
}

/// Paints the button into `rc`, shadow included.
pub unsafe fn draw(hdc: HDC, rc: &RECT, accent: u32, variant: Variant, state: State) {
    unsafe {
        // Barely a gradient — six levels across the whole height, just enough
        // that the fill isn't dead flat.
        let (base, fall) = match variant {
            Variant::Primary => (accent, 8),
            Variant::Secondary => (theme::CLR_CTRL_TOP, 8),
        };

        let base = match state {
            State::Normal => base,
            State::Hover => lighten(base, 14),
            State::Pressed => darken(base, 22),
        };

        paint::round_rect(
            hdc,
            rc,
            &paint::Style::flat(radius(rc.bottom - rc.top), base)
                .gradient(lighten(base, fall / 2), darken(base, fall / 2))
                .shadow(theme::CLR_SHADOW),
        );
    }
}
