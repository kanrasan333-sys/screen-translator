//! The floating card that shows a translation, a status message or an error.
//!
//! It never takes focus and dismisses itself: after a few seconds, or on any
//! click outside it.  The card is a per-pixel-alpha layered window
//! (`paint::present_card`), which is what buys it smooth corners and a soft
//! shadow — so there is no `WM_PAINT` path: every change of content, hover or
//! fade re-renders the card and hands it to `UpdateLayeredWindow`.

use crate::paint;
use crate::theme;
use crate::utils::lparam_to_point;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

// ============================================================
// Geometry / timing constants
// ============================================================

const CARD_W: i32 = 440;
const PAD_X: i32 = 18;
const PAD_Y: i32 = 16;
const CORNER_R: i32 = 12;
/// The language-pair chip and the copy button share a header row.
const HEADER_H: i32 = 24;
const HEADER_GAP: i32 = 10;
/// Status messages carry an icon to the left of the text.
const ICON_W: i32 = 26;
const COPY_BTN_W: i32 = 30;
const COPY_BTN_H: i32 = 26;
const MAX_CARD_H: i32 = 500;

const FONT_MAIN: i32 = 16;
const FONT_SMALL: i32 = 13;
const FONT_CHIP: i32 = 11;

const TIMER_FADEIN: usize = 100;
const TIMER_SPINNER: usize = 101;
const TIMER_AUTOHIDE: usize = 102;
const TIMER_COPY_RESET: usize = 103;
const FADE_STEP: u8 = 36;
const FADE_INTERVAL_MS: u32 = 12;
const SPINNER_INTERVAL_MS: u32 = 30;
const AUTOHIDE_MS: u32 = 8000;
const SPINNER_R: i32 = 8;
/// Fully opaque: a translucent card over busy content is harder to read, and
/// the soft shadow already lifts it off the page.
const OPAQUE: u8 = 255;

const WM_MOUSELEAVE: u32 = 0x02A3;

// ============================================================
// Global state (HWND stored as raw isize for Send)
// ============================================================

static POPUP_RAW: Mutex<isize> = Mutex::new(0);
static CURRENT_ALPHA: Mutex<u8> = Mutex::new(0);
static SPINNER_ANGLE: Mutex<i32> = Mutex::new(0);
static COPY_HOVER: Mutex<bool> = Mutex::new(false);
static COPY_DONE: Mutex<bool> = Mutex::new(false);
/// Where the card's top-left sits on screen.
static CARD_POS: Mutex<(i32, i32)> = Mutex::new((0, 0));

/// Global low-level mouse hook installed while the popup is visible, so a
/// click anywhere outside the popup dismisses it.  SetCapture is fragile
/// here (cross-thread activation, SW_SHOWNOACTIVATE popups don't always
/// get WM_CAPTURECHANGED in time) — a hook is guaranteed to see every
/// click regardless of which window owns focus.
static MOUSE_HOOK: Mutex<isize> = Mutex::new(0);

struct PopupText {
    translated: String,
    original: String,
    direction: String,
    loading: bool,
}
static POPUP_TEXT: Mutex<Option<PopupText>> = Mutex::new(None);

fn store_hwnd(m: &Mutex<isize>, hwnd: HWND) {
    *m.lock().unwrap() = hwnd.0 as isize;
}

fn load_hwnd(m: &Mutex<isize>) -> Option<HWND> {
    let v = *m.lock().unwrap();
    (v != 0).then_some(HWND(v as *mut _))
}

fn is_loading() -> bool {
    POPUP_TEXT
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|t| t.loading)
}

fn should_show_on_create() -> bool {
    POPUP_TEXT
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|t| t.loading || !t.translated.is_empty() || !t.original.is_empty())
}

// ============================================================
// Public API
// ============================================================

pub fn init() {
    unsafe {
        if load_hwnd(&POPUP_RAW).is_some() {
            return;
        }
        *POPUP_TEXT.lock().unwrap() = Some(PopupText {
            translated: String::new(),
            original: String::new(),
            direction: String::new(),
            loading: false,
        });
        if let Some(hwnd) = create_popup() {
            store_hwnd(&POPUP_RAW, hwnd);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

pub fn show_loading(msg: &str) {
    set_popup_text(msg, "", "", true);
    *COPY_DONE.lock().unwrap() = false;

    unsafe {
        if let Some(hwnd) = load_hwnd(&POPUP_RAW) {
            if IsWindow(hwnd).as_bool() {
                kill_all_timers(hwnd);
                *CURRENT_ALPHA.lock().unwrap() = OPAQUE;
                reposition_and_repaint(hwnd);
                let _ = SetTimer(hwnd, TIMER_SPINNER, SPINNER_INTERVAL_MS, None);
                raise_topmost(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                return;
            }
        }
        if let Some(hwnd) = create_popup() {
            store_hwnd(&POPUP_RAW, hwnd);
            let _ = SetTimer(hwnd, TIMER_SPINNER, SPINNER_INTERVAL_MS, None);
        }
    }
}

pub fn show(original: &str, translated: &str, direction: &str) {
    let was_loading = is_loading();

    set_popup_text(translated, original, direction, false);
    *COPY_DONE.lock().unwrap() = false;
    *COPY_HOVER.lock().unwrap() = false;

    unsafe {
        if let Some(hwnd) = load_hwnd(&POPUP_RAW) {
            if IsWindow(hwnd).as_bool() {
                kill_all_timers(hwnd);
                if was_loading {
                    // Smooth hand-over from the loading card: no fade.
                    *CURRENT_ALPHA.lock().unwrap() = OPAQUE;
                    reposition_and_repaint(hwnd);
                } else {
                    *CURRENT_ALPHA.lock().unwrap() = 0;
                    reposition_and_repaint(hwnd);
                    let _ = SetTimer(hwnd, TIMER_FADEIN, FADE_INTERVAL_MS, None);
                }
                let _ = SetTimer(hwnd, TIMER_AUTOHIDE, AUTOHIDE_MS, None);
                raise_topmost(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                install_mouse_hook();
                return;
            }
        }
        if let Some(hwnd) = create_popup() {
            store_hwnd(&POPUP_RAW, hwnd);
            install_mouse_hook();
        }
    }
}

// ============================================================
// Helpers
// ============================================================

fn set_popup_text(translated: &str, original: &str, direction: &str, loading: bool) {
    *POPUP_TEXT.lock().unwrap() = Some(PopupText {
        translated: translated.to_string(),
        original: original.to_string(),
        direction: direction.to_string(),
        loading,
    });
}

/// Re-asserts HWND_TOPMOST so we stay above any other topmost window
/// that became active after us (e.g. IME candidate windows, other
/// utilities, OSD overlays).
unsafe fn raise_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

unsafe fn tick_fadein(hwnd: HWND) {
    unsafe {
        let new_a = {
            let mut a = CURRENT_ALPHA.lock().unwrap();
            *a = (*a as u16 + FADE_STEP as u16).min(OPAQUE as u16) as u8;
            *a
        };
        present(hwnd);
        if new_a == OPAQUE {
            let _ = KillTimer(hwnd, TIMER_FADEIN);
        }
    }
}

// ============================================================
// Layout calculation
// ============================================================

/// What kind of message the card is carrying — it decides the header and
/// the icon.  `direction` is "en -> ru" for a translation, or one of the
/// markers the callers use for everything else.
#[derive(Clone, PartialEq)]
enum Kind {
    Loading,
    Translation(String),
    Info,
    Error,
    Plain,
}

fn kind_of(t: &PopupText) -> Kind {
    if t.loading {
        return Kind::Loading;
    }
    match t.direction.as_str() {
        "error" => Kind::Error,
        "info" => Kind::Info,
        d if d.contains("->") => {
            let pair = d
                .split("->")
                .map(|s| s.trim().to_uppercase())
                .collect::<Vec<_>>()
                .join(" \u{2192} ");
            Kind::Translation(pair)
        }
        _ => Kind::Plain,
    }
}

struct Layout {
    total_h: i32,
    kind: Kind,
    /// Header row — only a translation has one.
    chip_rect: RECT,
    copy_btn_rect: RECT,
    icon_rect: RECT,
    text_rect: RECT,
    separator_y: i32,
    original_rect: RECT,
    has_original: bool,
    show_copy: bool,
}

fn calc_layout(hdc: HDC) -> Layout {
    let guard = POPUP_TEXT.lock().unwrap();
    let text = guard.as_ref().unwrap();
    let kind = kind_of(text);
    let main_font = theme::ui_font(FONT_MAIN, 500);
    let small_font = theme::ui_font(FONT_SMALL, 400);

    let has_original = !text.original.is_empty() && kind != Kind::Loading;
    let show_copy = kind != Kind::Loading && !text.translated.is_empty();
    let inner_w = CARD_W - 2 * PAD_X;

    let mut y = PAD_Y;
    let mut chip_rect = RECT::default();
    let mut copy_btn_rect = RECT::default();
    let mut icon_rect = RECT::default();

    let text_rect = match &kind {
        Kind::Loading => {
            let h = 22;
            icon_rect = RECT {
                left: PAD_X,
                top: y,
                right: PAD_X + ICON_W - 4,
                bottom: y + h,
            };
            let r = RECT {
                left: PAD_X + ICON_W + 2,
                top: y,
                right: CARD_W - PAD_X,
                bottom: y + h,
            };
            y += h;
            r
        }
        Kind::Translation(pair) => {
            // Chip on the left, copy on the right, then the text full-width.
            let chip_font = theme::ui_font(FONT_CHIP, 700);
            let (cw, _) = unsafe { theme::measure(hdc, pair, chip_font) };
            chip_rect = RECT {
                left: PAD_X,
                top: y + 2,
                right: PAD_X + cw + 16,
                bottom: y + HEADER_H - 2,
            };
            if show_copy {
                copy_btn_rect = RECT {
                    left: CARD_W - PAD_X - COPY_BTN_W + 6,
                    top: y + (HEADER_H - COPY_BTN_H) / 2,
                    right: CARD_W - PAD_X + 6,
                    bottom: y + (HEADER_H - COPY_BTN_H) / 2 + COPY_BTN_H,
                };
            }
            y += HEADER_H + HEADER_GAP;
            let h = unsafe { theme::measure_wrapped(hdc, &text.translated, main_font, inner_w) }
                .max(20);
            let r = RECT {
                left: PAD_X,
                top: y,
                right: CARD_W - PAD_X,
                bottom: y + h,
            };
            y += h;
            r
        }
        Kind::Info | Kind::Error | Kind::Plain => {
            // Icon on the left, copy on the right, text between.
            let left = if kind == Kind::Plain {
                PAD_X
            } else {
                PAD_X + ICON_W
            };
            let right = if show_copy {
                CARD_W - PAD_X - COPY_BTN_W - 4
            } else {
                CARD_W - PAD_X
            };
            let h = unsafe {
                theme::measure_wrapped(hdc, &text.translated, main_font, right - left)
            }
            .max(22);
            if kind != Kind::Plain {
                icon_rect = RECT {
                    left: PAD_X - 2,
                    top: y,
                    right: PAD_X + ICON_W - 6,
                    bottom: y + 22,
                };
            }
            if show_copy {
                copy_btn_rect = RECT {
                    left: CARD_W - PAD_X - COPY_BTN_W + 6,
                    top: y - 2,
                    right: CARD_W - PAD_X + 6,
                    bottom: y - 2 + COPY_BTN_H,
                };
            }
            let r = RECT {
                left,
                top: y,
                right,
                bottom: y + h,
            };
            y += h;
            r
        }
    };

    let (separator_y, original_rect) = if has_original {
        y += 12;
        let sep = y;
        y += 12;
        let h = unsafe { theme::measure_wrapped(hdc, &text.original, small_font, inner_w) }.max(16);
        let r = RECT {
            left: PAD_X,
            top: y,
            right: CARD_W - PAD_X,
            bottom: y + h,
        };
        y += h;
        (sep, r)
    } else {
        (0, RECT::default())
    };

    y += PAD_Y;

    Layout {
        total_h: y.min(MAX_CARD_H),
        kind,
        chip_rect,
        copy_btn_rect,
        icon_rect,
        text_rect,
        separator_y,
        original_rect,
        has_original,
        show_copy,
    }
}

unsafe fn layout_now(hwnd: HWND) -> Layout {
    unsafe {
        let hdc = GetDC(hwnd);
        let layout = calc_layout(hdc);
        ReleaseDC(hwnd, hdc);
        layout
    }
}

// ============================================================
// Copy-to-clipboard action
// ============================================================

fn copy_translated(hwnd: HWND) {
    let text = POPUP_TEXT.lock().unwrap();
    if let Some(t) = text.as_ref() {
        if !t.translated.is_empty() {
            let s = t.translated.clone();
            drop(text);
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(&s);
            }
            *COPY_DONE.lock().unwrap() = true;
            unsafe {
                present(hwnd);
                let _ = SetTimer(hwnd, TIMER_COPY_RESET, 1500, None);
            }
        }
    }
}

// ============================================================
// Window creation
// ============================================================

fn create_popup() -> Option<HWND> {
    unsafe {
        let hmodule = GetModuleHandleW(None).ok()?;
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransPopup7");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(popup_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            CARD_W,
            200,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .ok()?;

        if !should_show_on_create() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else if is_loading() {
            *CURRENT_ALPHA.lock().unwrap() = OPAQUE;
            reposition_and_repaint(hwnd);
            raise_topmost(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        } else {
            *CURRENT_ALPHA.lock().unwrap() = 0;
            reposition_and_repaint(hwnd);
            let _ = SetTimer(hwnd, TIMER_FADEIN, FADE_INTERVAL_MS, None);
            let _ = SetTimer(hwnd, TIMER_AUTOHIDE, AUTOHIDE_MS, None);
            raise_topmost(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }

        Some(hwnd)
    }
}

/// Places the card next to the pointer — below and to the right, flipped to
/// whichever side has room on the monitor the pointer is on — and renders it.
unsafe fn reposition_and_repaint(hwnd: HWND) {
    unsafe {
        let h = layout_now(hwnd).total_h;

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mon = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            }
        };

        let mut x = cursor.x + 16;
        let mut y = cursor.y + 18;
        if x + CARD_W > work.right - 8 {
            x = cursor.x - CARD_W - 8;
        }
        if y + h > work.bottom - 8 {
            y = cursor.y - h - 10;
        }
        x = x.max(work.left + 8);
        y = y.max(work.top + 8);

        *CARD_POS.lock().unwrap() = (x, y);
        present(hwnd);
    }
}

/// Renders the card as it currently stands and puts it on screen.
unsafe fn present(hwnd: HWND) {
    unsafe {
        let layout = layout_now(hwnd);
        let (x, y) = *CARD_POS.lock().unwrap();
        let alpha = *CURRENT_ALPHA.lock().unwrap();
        let card = paint::Card {
            w: CARD_W,
            h: layout.total_h,
            radius: CORNER_R,
            fill: theme::CLR_ELEVATED,
            border: theme::CLR_SEPARATOR,
        };
        paint::present_card(hwnd, x, y, &card, alpha, |dc| paint_content(dc, &layout));
    }
}

/// A point in window coordinates, moved into the card's own.
fn to_card(lp: LPARAM) -> (i32, i32) {
    let (x, y) = lparam_to_point(lp);
    (x - paint::CARD_MARGIN, y - paint::CARD_MARGIN)
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                match wp.0 {
                    TIMER_FADEIN => tick_fadein(hwnd),
                    TIMER_SPINNER => {
                        {
                            let mut angle = SPINNER_ANGLE.lock().unwrap();
                            *angle = (*angle + 18) % 360;
                        }
                        present(hwnd);
                    }
                    TIMER_AUTOHIDE => hide_popup(hwnd),
                    TIMER_COPY_RESET => {
                        let _ = KillTimer(hwnd, TIMER_COPY_RESET);
                        *COPY_DONE.lock().unwrap() = false;
                        present(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // A card never takes focus from the window being read.
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONDOWN => {
                if !is_loading() {
                    let (x, y) = to_card(lp);
                    let layout = layout_now(hwnd);
                    if layout.show_copy && point_in_rect(x, y, &layout.copy_btn_rect) {
                        copy_translated(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if !is_loading() {
                    let (x, y) = to_card(lp);
                    let layout = layout_now(hwnd);
                    let hover = layout.show_copy && point_in_rect(x, y, &layout.copy_btn_rect);
                    let old = std::mem::replace(&mut *COPY_HOVER.lock().unwrap(), hover);
                    if hover != old {
                        present(hwnd);
                    }
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    // Reading takes as long as it takes: while the pointer is
                    // on the card, it doesn't go away on its own.
                    let _ = SetTimer(hwnd, TIMER_AUTOHIDE, AUTOHIDE_MS, None);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                if std::mem::replace(&mut *COPY_HOVER.lock().unwrap(), false) {
                    present(hwnd);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                // Layered: content goes up through UpdateLayeredWindow.
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                hide_popup(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                kill_all_timers(hwnd);
                *POPUP_RAW.lock().unwrap() = 0;
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn point_in_rect(x: i32, y: i32, r: &RECT) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn kill_all_timers(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(hwnd, TIMER_FADEIN);
        let _ = KillTimer(hwnd, TIMER_SPINNER);
        let _ = KillTimer(hwnd, TIMER_AUTOHIDE);
        let _ = KillTimer(hwnd, TIMER_COPY_RESET);
    }
}

/// Centralised hide path.  Tears down the mouse hook so we stop
/// receiving global click events while the popup isn't visible.
unsafe fn hide_popup(hwnd: HWND) {
    unsafe {
        kill_all_timers(hwnd);
        let _ = ShowWindow(hwnd, SW_HIDE);
        uninstall_mouse_hook();
    }
}

// ============================================================
// Low-level mouse hook — "click outside to dismiss"
// ============================================================

unsafe extern "system" fn mouse_hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let msg = wp.0 as u32;
            if msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN {
                let info = &*(lp.0 as *const MSLLHOOKSTRUCT);
                if let Some(hwnd) = load_hwnd(&POPUP_RAW) {
                    if IsWindowVisible(hwnd).as_bool() {
                        // The window is the card plus room for its shadow;
                        // only the card itself counts as "inside".
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        let m = paint::CARD_MARGIN;
                        let (px, py) = (info.pt.x, info.pt.y);
                        let inside = px >= rc.left + m
                            && px < rc.right - m
                            && py >= rc.top + m
                            && py < rc.bottom - m;
                        if !inside {
                            // Don't call hide_popup from the hook thread —
                            // post back to the popup's thread and let the
                            // window proc handle teardown cleanly.  The click
                            // itself continues to whatever app it targeted
                            // (CallNextHookEx below).
                            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
            }
        }
        CallNextHookEx(HHOOK::default(), code, wp, lp)
    }
}

unsafe fn install_mouse_hook() {
    unsafe {
        let mut g = MOUSE_HOOK.lock().unwrap();
        if *g != 0 {
            return;
        }
        let Ok(hmodule) = GetModuleHandleW(None) else {
            return;
        };
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), HINSTANCE(hmodule.0), 0)
            .unwrap_or_default();
        *g = hook.0 as isize;
    }
}

unsafe fn uninstall_mouse_hook() {
    unsafe {
        let mut g = MOUSE_HOOK.lock().unwrap();
        if *g == 0 {
            return;
        }
        let _ = UnhookWindowsHookEx(HHOOK(*g as *mut _));
        *g = 0;
    }
}

// ============================================================
// Painting — into the card's own DC, card coordinates
// ============================================================

unsafe fn paint_content(hdc: HDC, layout: &Layout) {
    unsafe {
        let text_guard = POPUP_TEXT.lock().unwrap();
        let Some(text) = text_guard.as_ref() else {
            return;
        };

        match &layout.kind {
            Kind::Loading => {
                let rc = layout.icon_rect;
                let cx = (rc.left + rc.right) / 2;
                let cy = (rc.top + rc.bottom) / 2;
                draw_spinner(hdc, cx, cy, SPINNER_R, *SPINNER_ANGLE.lock().unwrap());
            }
            Kind::Translation(pair) => {
                let chip = layout.chip_rect;
                let h = chip.bottom - chip.top;
                paint::round_rect(
                    hdc,
                    &chip,
                    &paint::Style::flat(h / 2, theme::mix(theme::CLR_ELEVATED, theme::CLR_ACCENT, 60)),
                );
                theme::text(
                    hdc,
                    pair,
                    &chip,
                    theme::ui_font(FONT_CHIP, 700),
                    theme::lighten(theme::CLR_ACCENT, 70),
                    theme::DT_CENTER_VCENTER,
                );
            }
            Kind::Info => theme::glyph(
                hdc,
                theme::ICON_INFO,
                &layout.icon_rect,
                16,
                theme::lighten(theme::CLR_ACCENT, 40),
            ),
            Kind::Error => theme::glyph(hdc, theme::ICON_WARNING, &layout.icon_rect, 16, theme::CLR_RED),
            Kind::Plain => {}
        }

        let main_color = match layout.kind {
            Kind::Loading => theme::CLR_TEXT_DIM,
            _ => theme::CLR_TEXT_BRIGHT,
        };
        let flags = if layout.kind == Kind::Loading {
            theme::DT_LEFT_VCENTER
        } else {
            theme::DT_WRAP
        };
        theme::text(
            hdc,
            &text.translated,
            &layout.text_rect,
            theme::ui_font(FONT_MAIN, 500),
            main_color,
            flags,
        );

        if layout.show_copy {
            draw_copy_button(
                hdc,
                &layout.copy_btn_rect,
                *COPY_HOVER.lock().unwrap(),
                *COPY_DONE.lock().unwrap(),
            );
        }

        if layout.has_original {
            paint::hairline(
                hdc,
                PAD_X,
                CARD_W - PAD_X,
                layout.separator_y,
                theme::CLR_SEPARATOR,
            );
            theme::text(
                hdc,
                &text.original,
                &layout.original_rect,
                theme::ui_font(FONT_SMALL, 400),
                theme::CLR_TEXT_DIM,
                theme::DT_WRAP,
            );
        }
    }
}

/// A ring with a three-quarter arc running round it, antialiased.
unsafe fn draw_spinner(hdc: HDC, cx: i32, cy: i32, r: i32, angle: i32) {
    unsafe {
        let pad = 3;
        let rc = RECT {
            left: cx - r - pad,
            top: cy - r - pad,
            right: cx + r + pad,
            bottom: cy + r + pad,
        };
        paint::supersampled(hdc, &rc, |dc, ss| {
            let c = (r + pad) * ss;
            let rr = r * ss;
            let ring = CreatePen(PS_SOLID, 2 * ss, COLORREF(theme::CLR_SEPARATOR));
            let old_pen = SelectObject(dc, ring);
            let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            let _ = Ellipse(dc, c - rr, c - rr, c + rr, c + rr);

            let arc = CreatePen(PS_SOLID, 2 * ss + ss / 2, COLORREF(theme::CLR_ACCENT));
            SelectObject(dc, arc);
            let _ = DeleteObject(ring);
            let a0 = (angle as f64).to_radians();
            let a1 = a0 + 270f64.to_radians();
            let far = (rr * 2) as f64;
            // Arc runs counter-clockwise from the start radial to the end one.
            let _ = Arc(
                dc,
                c - rr,
                c - rr,
                c + rr,
                c + rr,
                c + (far * a1.cos()) as i32,
                c - (far * a1.sin()) as i32,
                c + (far * a0.cos()) as i32,
                c - (far * a0.sin()) as i32,
            );
            SelectObject(dc, old_pen);
            SelectObject(dc, old_brush);
            let _ = DeleteObject(arc);
        });
    }
}

/// Ghost button: nothing but the glyph at rest, a soft plate under the
/// pointer, and a green tick for a moment after copying.
unsafe fn draw_copy_button(hdc: HDC, rc: &RECT, hover: bool, done: bool) {
    unsafe {
        if hover {
            paint::round_rect(
                hdc,
                rc,
                &paint::Style::flat(6, theme::lighten(theme::CLR_ELEVATED, 16)),
            );
        }
        if done {
            theme::glyph(hdc, theme::ICON_CHECK, rc, 14, theme::CLR_GREEN);
        } else {
            let color = if hover {
                theme::CLR_TEXT_BRIGHT
            } else {
                theme::CLR_TEXT_DIM
            };
            theme::glyph(hdc, theme::ICON_COPY, rc, 14, color);
        }
    }
}
