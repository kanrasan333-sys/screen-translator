//! Push buttons, shared by every surface that draws one.
//!
//! Flat: a solid fill and a smooth corner, nothing else.  The gradient-plus-
//! highlight-plus-shadow stack that used to say "button" dates a window
//! instantly.  The default button carries the accent; every other button is a
//! neutral fill that brightens under the pointer.
//!
//! Only the body is drawn here.  Text is left to the caller: the capture
//! overlay and the settings window measure and position it differently.

use crate::paint;
use crate::theme::{self, darken, lighten};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

/// The default button — the one Return activates — carries the accent colour;
/// every other button is neutral.
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

/// Corner radius: about 6 px on a compact button, 8 on a tall one.
pub fn radius(h: i32) -> i32 {
    (h / 4).clamp(5, 8)
}

pub fn text_color(variant: Variant, state: State) -> u32 {
    match (variant, state) {
        (Variant::Primary, _) => 0x00FF_FFFF,
        (Variant::Secondary, State::Pressed) => theme::CLR_TEXT_DIM,
        (Variant::Secondary, _) => theme::CLR_TEXT_BRIGHT,
    }
}

/// The fill a button of this kind has in this state.
pub fn fill(accent: u32, variant: Variant, state: State) -> u32 {
    let base = match variant {
        Variant::Primary => accent,
        Variant::Secondary => theme::CLR_CTRL,
    };
    match state {
        State::Normal => base,
        State::Hover => lighten(base, 16),
        State::Pressed => darken(base, 18),
    }
}

/// Paints the button body into `rc`.
pub unsafe fn draw(hdc: HDC, rc: &RECT, accent: u32, variant: Variant, state: State) {
    unsafe {
        paint::round_rect(
            hdc,
            rc,
            &paint::Style::flat(radius(rc.bottom - rc.top), fill(accent, variant, state)),
        );
    }
}
