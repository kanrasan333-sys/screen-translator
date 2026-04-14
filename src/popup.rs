use crate::theme;
use crate::utils::{lparam_to_point, to_wide};
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

// ============================================================
// Draw-text format flags (missing from the windows crate)
// ============================================================

const DT_WORDBREAK: u32 = 0x0010;
const DT_CALCRECT: u32 = 0x0400;
const DT_NOPREFIX: u32 = 0x0800;
const DT_EDITCONTROL: u32 = 0x2000;

// ============================================================
// Popup-specific colours (not shared via theme)
// ============================================================

const CLR_COPY_ICON: u32 = 0x0080_8080;
const CLR_COPY_HOVER: u32 = 0x00E0_E0E0;
const CLR_CHECK: u32 = 0x0060_D060;

// ============================================================
// Geometry / timing constants
// ============================================================

const MAX_W: i32 = 440;
const PAD_X: i32 = 18;
const PAD_Y: i32 = 14;
const ACCENT_H: i32 = 4;
const CORNER_R: i32 = 12;

const TIMER_FADEIN: usize = 100;
const TIMER_SPINNER: usize = 101;
const TIMER_COPY_RESET: usize = 103;
const FADE_STEP: u8 = 30;
const FADE_INTERVAL_MS: u32 = 12;
const SPINNER_INTERVAL_MS: u32 = 40;
const SPINNER_R: i32 = 10;
const COPY_BTN_SIZE: i32 = 26;
const MAX_POPUP_H: i32 = 500;

// ============================================================
// Global state (HWND / HFONT stored as raw isize for Send)
// ============================================================

static POPUP_RAW: Mutex<isize> = Mutex::new(0);
static FONT_MAIN_RAW: Mutex<isize> = Mutex::new(0);
static FONT_SMALL_RAW: Mutex<isize> = Mutex::new(0);
static FONT_DIR_RAW: Mutex<isize> = Mutex::new(0);
static CURRENT_ALPHA: Mutex<u8> = Mutex::new(0);
static SPINNER_ANGLE: Mutex<i32> = Mutex::new(0);
static COPY_HOVER: Mutex<bool> = Mutex::new(false);
static COPY_DONE: Mutex<bool> = Mutex::new(false);

struct PopupText {
    translated: String,
    original: String,
    direction: String,
    loading: bool,
}
static POPUP_TEXT: Mutex<Option<PopupText>> = Mutex::new(None);

// ============================================================
// Handle helpers (HWND / HFONT ↔ isize)
// ============================================================

fn store_hwnd(m: &Mutex<isize>, hwnd: HWND) {
    *m.lock().unwrap() = hwnd.0 as isize;
}

fn load_hwnd(m: &Mutex<isize>) -> Option<HWND> {
    let v = *m.lock().unwrap();
    if v == 0 { None } else { Some(HWND(v as *mut _)) }
}

fn hfont(m: &Mutex<isize>) -> HFONT {
    HFONT(*m.lock().unwrap() as *mut _)
}

fn is_loading() -> bool {
    POPUP_TEXT.lock().unwrap().as_ref().is_some_and(|t| t.loading)
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
                set_alpha(hwnd, 245);
                let _ = KillTimer(hwnd, TIMER_COPY_RESET);
                reposition_and_repaint(hwnd);
                let _ = SetTimer(hwnd, TIMER_SPINNER, SPINNER_INTERVAL_MS, None);
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
                reposition_and_repaint(hwnd);
                if was_loading {
                    set_alpha(hwnd, 245); // smooth transition from loading
                } else {
                    start_fadein(hwnd);
                }
                let _ = ShowWindow(hwnd, SW_SHOW);
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }
        if let Some(hwnd) = create_popup() {
            store_hwnd(&POPUP_RAW, hwnd);
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

unsafe fn set_alpha(hwnd: HWND, alpha: u8) {
    unsafe {
        *CURRENT_ALPHA.lock().unwrap() = alpha;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
}

// ============================================================
// Fade-in animation
// ============================================================

unsafe fn start_fadein(hwnd: HWND) {
    unsafe {
        set_alpha(hwnd, 0);
        let _ = SetTimer(hwnd, TIMER_FADEIN, FADE_INTERVAL_MS, None);
    }
}

unsafe fn tick_fadein(hwnd: HWND) {
    unsafe {
        let mut a = CURRENT_ALPHA.lock().unwrap();
        let new_a = (*a as u16 + FADE_STEP as u16).min(245) as u8;
        *a = new_a;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), new_a, LWA_ALPHA);
        if new_a >= 245 {
            let _ = KillTimer(hwnd, TIMER_FADEIN);
        }
    }
}

// ============================================================
// Layout calculation
// ============================================================

struct Layout {
    total_h: i32,
    translated_rect: RECT,
    separator_y: i32,
    direction_rect: RECT,
    original_rect: RECT,
    copy_btn_rect: RECT,
    has_original: bool,
    is_loading: bool,
    show_copy: bool,
}

fn measure_text(hdc: HDC, font: HFONT, text: &str, max_width: i32) -> RECT {
    unsafe {
        let old = SelectObject(hdc, font);
        let mut wide = to_wide(text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut rc = RECT { left: 0, top: 0, right: max_width, bottom: 0 };
        DrawTextW(hdc, &mut wide, &mut rc,
            DRAW_TEXT_FORMAT(DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL));
        SelectObject(hdc, old);
        rc
    }
}

fn calc_layout(hdc: HDC) -> Layout {
    let guard = POPUP_TEXT.lock().unwrap();
    let text = guard.as_ref().unwrap();

    let has_original = !text.original.is_empty();
    let is_loading = text.loading;
    let show_copy = !is_loading && !text.translated.is_empty();

    let copy_margin = if show_copy { COPY_BTN_SIZE + 8 } else { 0 };
    let text_w = MAX_W - 2 * PAD_X - copy_margin;

    let font_main = hfont(&FONT_MAIN_RAW);
    let font_small = hfont(&FONT_SMALL_RAW);
    let font_dir = hfont(&FONT_DIR_RAW);

    let tr = measure_text(hdc, font_main, &text.translated, text_w);
    let translated_h = (tr.bottom - tr.top).max(22);

    let mut y = ACCENT_H + PAD_Y;

    let direction_rect = if has_original && !text.direction.is_empty() {
        let dr = measure_text(hdc, font_dir, &text.direction, text_w);
        let dir_h = (dr.bottom - dr.top).max(14);
        let r = RECT { left: PAD_X, top: y, right: PAD_X + text_w, bottom: y + dir_h };
        y += dir_h + 8;
        r
    } else {
        RECT::default()
    };

    let text_left = if is_loading { PAD_X + SPINNER_R * 2 + 12 } else { PAD_X };
    let translated_rect = RECT {
        left: text_left, top: y,
        right: PAD_X + text_w, bottom: y + translated_h,
    };

    let copy_btn_rect = if show_copy {
        let btn_y = (y + (translated_h - COPY_BTN_SIZE) / 2).max(ACCENT_H + 4);
        RECT {
            left: MAX_W - PAD_X - COPY_BTN_SIZE,
            top: btn_y,
            right: MAX_W - PAD_X,
            bottom: btn_y + COPY_BTN_SIZE,
        }
    } else {
        RECT::default()
    };

    y += translated_h;

    let (separator_y, original_rect) = if has_original {
        y += 12;
        let sep = y;
        y += 12;
        let or = measure_text(hdc, font_small, &text.original, MAX_W - 2 * PAD_X);
        let orig_h = (or.bottom - or.top).max(16);
        let r = RECT { left: PAD_X, top: y, right: MAX_W - PAD_X, bottom: y + orig_h };
        y += orig_h;
        (sep, r)
    } else {
        (0, RECT::default())
    };

    y += PAD_Y;

    Layout {
        total_h: y.min(MAX_POPUP_H),
        translated_rect, separator_y, direction_rect,
        original_rect, copy_btn_rect,
        has_original, is_loading, show_copy,
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
                let _ = InvalidateRect(hwnd, None, false);
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
        let class = w!("ScrTransPopup6");

        let wc = WNDCLASSW {
            style: CS_DROPSHADOW | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(popup_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        *FONT_MAIN_RAW.lock().unwrap() =
            CreateFontW(-18, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0 as isize;
        *FONT_SMALL_RAW.lock().unwrap() =
            CreateFontW(-13, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0 as isize;
        *FONT_DIR_RAW.lock().unwrap() =
            CreateFontW(-11, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0 as isize;

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            class, w!(""), WS_POPUP,
            cursor.x + 15, cursor.y + 15, MAX_W, 200,
            HWND::default(), HMENU::default(), hinstance, None,
        ).ok()?;

        let rgn = CreateRoundRectRgn(0, 0, MAX_W + 1, 201, CORNER_R, CORNER_R);
        SetWindowRgn(hwnd, rgn, true);
        reposition_and_repaint(hwnd);

        if is_loading() {
            set_alpha(hwnd, 245);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        } else {
            start_fadein(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }

        Some(hwnd)
    }
}

unsafe fn reposition_and_repaint(hwnd: HWND) {
    unsafe {
        let hdc = GetDC(hwnd);
        let layout = calc_layout(hdc);
        ReleaseDC(hwnd, hdc);
        let h = layout.total_h;

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let mut x = cursor.x + 15;
        let mut y = cursor.y + 15;
        if x + MAX_W > sw { x = cursor.x - MAX_W - 5; }
        if y + h > sh { y = cursor.y - h - 5; }
        x = x.max(5);
        y = y.max(5);

        let rgn = CreateRoundRectRgn(0, 0, MAX_W + 1, h + 1, CORNER_R, CORNER_R);
        SetWindowRgn(hwnd, rgn, true);
        let _ = MoveWindow(hwnd, x, y, MAX_W, h, true);
        let _ = InvalidateRect(hwnd, None, false);
    }
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn popup_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                match wp.0 {
                    TIMER_FADEIN => tick_fadein(hwnd),
                    TIMER_SPINNER => {
                        let mut angle = SPINNER_ANGLE.lock().unwrap();
                        *angle = (*angle + 25) % 360;
                        drop(angle);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    TIMER_COPY_RESET => {
                        let _ = KillTimer(hwnd, TIMER_COPY_RESET);
                        *COPY_DONE.lock().unwrap() = false;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                if !is_loading() {
                    let (x, y) = lparam_to_point(lp);
                    let hdc = GetDC(hwnd);
                    let layout = calc_layout(hdc);
                    ReleaseDC(hwnd, hdc);
                    if layout.show_copy && point_in_rect(x, y, &layout.copy_btn_rect) {
                        copy_translated(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if !is_loading() {
                    let (x, y) = lparam_to_point(lp);
                    let hdc = GetDC(hwnd);
                    let layout = calc_layout(hdc);
                    ReleaseDC(hwnd, hdc);
                    let hover = layout.show_copy && point_in_rect(x, y, &layout.copy_btn_rect);
                    let old = *COPY_HOVER.lock().unwrap();
                    if hover != old {
                        *COPY_HOVER.lock().unwrap() = hover;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint_buffered(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) == 0 && !is_loading() {
                    kill_all_timers(hwnd);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                kill_all_timers(hwnd);
                let _ = ShowWindow(hwnd, SW_HIDE);
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
        let _ = KillTimer(hwnd, TIMER_COPY_RESET);
    }
}

// ============================================================
// Painting (double-buffered)
// ============================================================

unsafe fn paint_buffered(hwnd: HWND, hdc: HDC) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);
        if w <= 0 || h <= 0 { return; }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        let bg = CreateSolidBrush(COLORREF(theme::CLR_BG));
        let _ = FillRect(mem_dc, &rc, bg);
        let _ = DeleteObject(bg);

        paint_content(mem_dc);
        let _ = BitBlt(hdc, 0, 0, w, h, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

unsafe fn paint_content(hdc: HDC) {
    unsafe {
        let layout = calc_layout(hdc);
        let text_guard = POPUP_TEXT.lock().unwrap();
        let Some(text) = text_guard.as_ref() else { return };

        // Accent bar.
        let accent_brush = CreateSolidBrush(COLORREF(theme::CLR_ACCENT));
        let accent_rc = RECT { left: 0, top: 0, right: MAX_W, bottom: ACCENT_H };
        let _ = FillRect(hdc, &accent_rc, accent_brush);
        let _ = DeleteObject(accent_brush);

        SetBkMode(hdc, TRANSPARENT);

        // Direction label.
        if layout.has_original && !text.direction.is_empty() {
            draw_text_block(hdc, &text.direction, &layout.direction_rect,
                hfont(&FONT_DIR_RAW), theme::CLR_ACCENT);
        }

        // Translated text.
        let translated_clr = if layout.is_loading { theme::CLR_HINT } else { theme::CLR_TEXT_BRIGHT };
        draw_text_block(hdc, &text.translated, &layout.translated_rect,
            hfont(&FONT_MAIN_RAW), translated_clr);

        // Spinner.
        if layout.is_loading {
            let cx = PAD_X + SPINNER_R + 2;
            let cy = (layout.translated_rect.top + layout.translated_rect.bottom) / 2;
            let angle = *SPINNER_ANGLE.lock().unwrap();
            draw_spinner(hdc, cx, cy, SPINNER_R, angle);
        }

        // Copy / checkmark button.
        if layout.show_copy {
            if *COPY_DONE.lock().unwrap() {
                draw_checkmark(hdc, &layout.copy_btn_rect);
            } else {
                draw_copy_icon(hdc, &layout.copy_btn_rect, *COPY_HOVER.lock().unwrap());
            }
        }

        // Separator + original text.
        if layout.has_original {
            let sep_pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_SEPARATOR));
            let old_pen = SelectObject(hdc, sep_pen);
            let _ = MoveToEx(hdc, PAD_X, layout.separator_y, None);
            let _ = LineTo(hdc, MAX_W - PAD_X, layout.separator_y);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(sep_pen);

            draw_text_block(hdc, &text.original, &layout.original_rect,
                hfont(&FONT_SMALL_RAW), theme::CLR_TEXT_DIM);
        }
    }
}

/// Draws word-wrapped text into a rect with a given font and colour.
unsafe fn draw_text_block(hdc: HDC, text: &str, rect: &RECT, font: HFONT, colour: u32) {
    unsafe {
        SetTextColor(hdc, COLORREF(colour));
        let old = SelectObject(hdc, font);
        let mut wide = to_wide(text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut rc = *rect;
        DrawTextW(hdc, &mut wide, &mut rc,
            DRAW_TEXT_FORMAT(DT_WORDBREAK | DT_NOPREFIX | DT_EDITCONTROL));
        SelectObject(hdc, old);
    }
}

// ============================================================
// Drawing helpers
// ============================================================

unsafe fn draw_spinner(hdc: HDC, cx: i32, cy: i32, r: i32, angle: i32) {
    unsafe {
        // Background ring.
        let bg_pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_SEPARATOR));
        let old_pen = SelectObject(hdc, bg_pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Ellipse(hdc, cx - r, cy - r, cx + r, cy + r);
        let _ = DeleteObject(bg_pen);

        // 270-degree foreground arc.
        let fg_pen = CreatePen(PS_SOLID, 3, COLORREF(theme::CLR_ACCENT));
        SelectObject(hdc, fg_pen);

        let pi = std::f64::consts::PI;
        let rf = r as f64;
        let start_rad = (angle as f64) * pi / 180.0;
        let end_rad = start_rad + 270.0 * pi / 180.0;

        let x_start = cx + (rf * end_rad.cos()) as i32;
        let y_start = cy - (rf * end_rad.sin()) as i32;
        let x_end = cx + (rf * start_rad.cos()) as i32;
        let y_end = cy - (rf * start_rad.sin()) as i32;

        let _ = Arc(hdc, cx - r, cy - r, cx + r, cy + r, x_start, y_start, x_end, y_end);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(fg_pen);
    }
}

unsafe fn draw_copy_icon(hdc: HDC, rect: &RECT, hover: bool) {
    unsafe {
        let clr = if hover { CLR_COPY_HOVER } else { CLR_COPY_ICON };
        let pen = CreatePen(PS_SOLID, 1, COLORREF(clr));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;

        // Back sheet.
        let _ = Rectangle(hdc, cx - 3, cy - 5, cx + 7, cy + 5);
        // Erase overlap for front sheet.
        let fill = CreateSolidBrush(COLORREF(theme::CLR_BG));
        let fr = RECT { left: cx - 7, top: cy - 6, right: cx + 4, bottom: cy + 4 };
        let _ = FillRect(hdc, &fr, fill);
        let _ = DeleteObject(fill);
        // Front sheet.
        let _ = Rectangle(hdc, cx - 7, cy - 6, cx + 3, cy + 4);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

unsafe fn draw_checkmark(hdc: HDC, rect: &RECT) {
    unsafe {
        let pen = CreatePen(PS_SOLID, 2, COLORREF(CLR_CHECK));
        let old_pen = SelectObject(hdc, pen);

        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;

        let _ = MoveToEx(hdc, cx - 5, cy, None);
        let _ = LineTo(hdc, cx - 1, cy + 4);
        let _ = LineTo(hdc, cx + 6, cy - 4);

        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);
    }
}
