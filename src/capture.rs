use crate::i18n;
use crate::theme;
use crate::utils::{lparam_to_point, to_wide};
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

// ============================================================
// Public types
// ============================================================

pub enum CaptureAction {
    Translate(Vec<u8>, u32, u32),
    Save(Vec<u8>, u32, u32),
    Clipboard(Vec<u8>, u32, u32),
}

// ============================================================
// Internal state
// ============================================================

#[derive(PartialEq, Clone, Copy)]
enum Phase { Selecting, Toolbar }

#[derive(PartialEq, Clone, Copy)]
enum Action { None, Translate, Save, Clipboard, Cancel }

/// Eight resize handles around the selection rect.  Compass directions
/// (NW = top-left, SE = bottom-right, etc.) map cleanly to what the
/// cursor and drag behaviour should do.
#[derive(PartialEq, Clone, Copy)]
enum Handle { NW, N, NE, E, SE, S, SW, W }

impl Handle {
    fn all() -> &'static [Handle] {
        &[Handle::NW, Handle::N, Handle::NE, Handle::E,
          Handle::SE, Handle::S, Handle::SW, Handle::W]
    }
    fn cursor(self) -> windows::core::PCWSTR {
        match self {
            Handle::NW | Handle::SE => IDC_SIZENWSE,
            Handle::NE | Handle::SW => IDC_SIZENESW,
            Handle::N  | Handle::S  => IDC_SIZENS,
            Handle::E  | Handle::W  => IDC_SIZEWE,
        }
    }
}

/// Size of each resize-handle square in pixels.
const HANDLE_SIZE: i32 = 10;

struct OverlayState {
    phase: Phase,
    dragging: bool,
    /// Set while the user is dragging one of the eight handles.
    resizing: Option<Handle>,
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
    action: Action,
}

impl OverlayState {
    const fn new() -> Self {
        Self {
            phase: Phase::Selecting, dragging: false, resizing: None,
            start_x: 0, start_y: 0, end_x: 0, end_y: 0,
            action: Action::None,
        }
    }
    fn reset(&mut self) { *self = Self::new(); }
    fn rect(&self) -> (i32, i32, i32, i32) {
        let x = self.start_x.min(self.end_x);
        let y = self.start_y.min(self.end_y);
        let w = (self.start_x - self.end_x).abs();
        let h = (self.start_y - self.end_y).abs();
        (x, y, w, h)
    }
    /// Re-assign start/end so start is the top-left, end is the bottom-right.
    /// Called on resize start so the per-handle update rules below have a
    /// stable frame to work in.
    fn normalize(&mut self) {
        let (rx, ry, rw, rh) = self.rect();
        self.start_x = rx;
        self.start_y = ry;
        self.end_x   = rx + rw;
        self.end_y   = ry + rh;
    }
}

/// Rect of a single resize handle centred on the appropriate edge/corner.
fn handle_rect(rx: i32, ry: i32, rw: i32, rh: i32, h: Handle) -> RECT {
    let half = HANDLE_SIZE / 2;
    let (cx, cy) = match h {
        Handle::NW => (rx,          ry),
        Handle::N  => (rx + rw / 2, ry),
        Handle::NE => (rx + rw,     ry),
        Handle::E  => (rx + rw,     ry + rh / 2),
        Handle::SE => (rx + rw,     ry + rh),
        Handle::S  => (rx + rw / 2, ry + rh),
        Handle::SW => (rx,          ry + rh),
        Handle::W  => (rx,          ry + rh / 2),
    };
    RECT {
        left: cx - half, top: cy - half,
        right: cx - half + HANDLE_SIZE, bottom: cy - half + HANDLE_SIZE,
    }
}

/// Which handle (if any) the point falls inside.
fn handle_at(x: i32, y: i32, rx: i32, ry: i32, rw: i32, rh: i32) -> Option<Handle> {
    for h in Handle::all() {
        let r = handle_rect(rx, ry, rw, rh, *h);
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return Some(*h);
        }
    }
    None
}

static STATE: Mutex<OverlayState> = Mutex::new(OverlayState::new());

// ============================================================
// Pre-captured screen (original + dimmed)
// ============================================================

struct ScreenCapture {
    x: i32,
    y: i32,
    orig_dc: isize,
    orig_bmp: isize,
    orig_old: isize,
    dim_dc: isize,
    dim_bmp: isize,
    dim_old: isize,
    /// Back buffer — paint the whole scene here first, then BitBlt in one
    /// pass to the window.  Kills the flicker that came from painting
    /// dim → selection → border directly to the visible HDC.
    back_dc: isize,
    back_bmp: isize,
    back_old: isize,
    width: i32,
    height: i32,
}
static SCREEN_CAP: Mutex<Option<ScreenCapture>> = Mutex::new(None);

fn init_screen_capture() {
    unsafe {
        let screen_dc = GetDC(None);
        let sx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let sy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let sw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let sh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        // Original screenshot.
        let orig_dc = CreateCompatibleDC(screen_dc);
        let orig_bmp = CreateCompatibleBitmap(screen_dc, sw, sh);
        let orig_old = SelectObject(orig_dc, orig_bmp);
        let _ = BitBlt(orig_dc, 0, 0, sw, sh, screen_dc, sx, sy, SRCCOPY);

        // Dimmed copy.
        let dim_dc = CreateCompatibleDC(screen_dc);
        let dim_bmp = CreateCompatibleBitmap(screen_dc, sw, sh);
        let dim_old = SelectObject(dim_dc, dim_bmp);
        let _ = BitBlt(dim_dc, 0, 0, sw, sh, orig_dc, 0, 0, SRCCOPY);

        // AlphaBlend a black overlay to dim the screen.
        let black_dc = CreateCompatibleDC(screen_dc);
        let black_bmp = CreateCompatibleBitmap(screen_dc, 1, 1);
        let black_old = SelectObject(black_dc, black_bmp);
        let _ = PatBlt(black_dc, 0, 0, 1, 1, BLACKNESS);

        let _ = AlphaBlend(dim_dc, 0, 0, sw, sh, black_dc, 0, 0, 1, 1, BLENDFUNCTION {
            BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 140, AlphaFormat: 0,
        });

        SelectObject(black_dc, black_old);
        let _ = DeleteObject(black_bmp);
        let _ = DeleteDC(black_dc);

        // Back buffer — same size as the overlay.  Allocated once per
        // capture session so every WM_PAINT reuses it rather than churning
        // GDI objects on each mouse move.
        let back_dc = CreateCompatibleDC(screen_dc);
        let back_bmp = CreateCompatibleBitmap(screen_dc, sw, sh);
        let back_old = SelectObject(back_dc, back_bmp);

        ReleaseDC(None, screen_dc);

        *SCREEN_CAP.lock().unwrap() = Some(ScreenCapture {
            x: sx,
            y: sy,
            orig_dc: orig_dc.0 as isize, orig_bmp: orig_bmp.0 as isize,
            orig_old: orig_old.0 as isize,
            dim_dc: dim_dc.0 as isize, dim_bmp: dim_bmp.0 as isize,
            dim_old: dim_old.0 as isize,
            back_dc: back_dc.0 as isize, back_bmp: back_bmp.0 as isize,
            back_old: back_old.0 as isize,
            width: sw, height: sh,
        });
    }
}

fn cleanup_screen_capture() {
    if let Some(c) = SCREEN_CAP.lock().unwrap().take() {
        unsafe {
            let orig_dc = HDC(c.orig_dc as *mut _);
            SelectObject(orig_dc, HGDIOBJ(c.orig_old as *mut _));
            let _ = DeleteObject(HGDIOBJ(c.orig_bmp as *mut _));
            let _ = DeleteDC(orig_dc);

            let dim_dc = HDC(c.dim_dc as *mut _);
            SelectObject(dim_dc, HGDIOBJ(c.dim_old as *mut _));
            let _ = DeleteObject(HGDIOBJ(c.dim_bmp as *mut _));
            let _ = DeleteDC(dim_dc);

            let back_dc = HDC(c.back_dc as *mut _);
            SelectObject(back_dc, HGDIOBJ(c.back_old as *mut _));
            let _ = DeleteObject(HGDIOBJ(c.back_bmp as *mut _));
            let _ = DeleteDC(back_dc);
        }
    }
}

fn screen_bounds() -> (i32, i32, i32, i32) {
    SCREEN_CAP.lock().unwrap().as_ref()
        .map(|c| (c.x, c.y, c.width, c.height))
        .unwrap_or_else(|| unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        })
}

fn extract_pixels(x: i32, y: i32, w: i32, h: i32) -> Option<(Vec<u8>, u32, u32)> {
    if w <= 0 || h <= 0 { return None; }

    let orig_dc_raw = SCREEN_CAP.lock().unwrap().as_ref().map(|c| c.orig_dc)?;

    unsafe {
        let src_dc = HDC(orig_dc_raw as *mut _);
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);
        let _ = BitBlt(mem_dc, 0, 0, w, h, src_dc, x, y, SRCCOPY);
        SelectObject(mem_dc, old);

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w, biHeight: -h, biPlanes: 1,
                biBitCount: 32, biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pixels = vec![0u8; (w * h * 4) as usize];
        GetDIBits(screen_dc, bmp, 0, h as u32,
            Some(pixels.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);

        // Ensure alpha = 255.
        for chunk in pixels.chunks_exact_mut(4) { chunk[3] = 255; }

        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        Some((pixels, w as u32, h as u32))
    }
}

// ============================================================
// Toolbar buttons
// ============================================================

const BTN_W: i32 = 110;
const BTN_H: i32 = 34;
const BTN_GAP: i32 = 10;
const BTN_MARGIN: i32 = 12;

struct BtnRect { x: i32, y: i32, w: i32, h: i32 }

fn toolbar_buttons(sx: i32, sy: i32, sw: i32, sh_sel: i32, screen_h: i32) -> (BtnRect, BtnRect) {
    let total_w = BTN_W * 2 + BTN_GAP;
    let bx = sx + sw - total_w;

    let below = sy + sh_sel + BTN_MARGIN;
    let by = if below + BTN_H + 4 > screen_h { sy - BTN_H - BTN_MARGIN } else { below };

    (
        BtnRect { x: bx.max(4), y: by.max(4), w: BTN_W, h: BTN_H },
        BtnRect { x: (bx + BTN_W + BTN_GAP).max(4), y: by.max(4), w: BTN_W, h: BTN_H },
    )
}

fn point_in_btn(px: i32, py: i32, btn: &BtnRect) -> bool {
    px >= btn.x && px <= btn.x + btn.w && py >= btn.y && py <= btn.y + btn.h
}

// ============================================================
// Cached overlay fonts (created once, never leaked)
// ============================================================

struct OverlayFonts {
    badge: isize,   // -12, weight 600
    button: isize,  // -13, weight 600
    hint: isize,    // -11, weight 400
    initial: isize, // -16, weight 400
}

static OVERLAY_FONTS: Mutex<Option<OverlayFonts>> = Mutex::new(None);

fn ensure_fonts() {
    let mut guard = OVERLAY_FONTS.lock().unwrap();
    if guard.is_some() { return; }
    unsafe {
        *guard = Some(OverlayFonts {
            badge:   CreateFontW(-12,0,0,0,600,0,0,0,1,0,0,5,0,w!("Segoe UI")).0 as isize,
            button:  CreateFontW(-13,0,0,0,600,0,0,0,1,0,0,5,0,w!("Segoe UI")).0 as isize,
            hint:    CreateFontW(-11,0,0,0,400,0,0,0,1,0,0,5,0,w!("Segoe UI")).0 as isize,
            initial: CreateFontW(-16,0,0,0,400,0,0,0,1,0,0,5,0,w!("Segoe UI")).0 as isize,
        });
    }
}

fn font(f: impl Fn(&OverlayFonts) -> isize) -> HFONT {
    let guard = OVERLAY_FONTS.lock().unwrap();
    HFONT(f(guard.as_ref().unwrap()) as *mut _)
}

// ============================================================
// Public API
// ============================================================

static OVERLAY_HWND: Mutex<isize> = Mutex::new(0);

fn store_overlay(hwnd: HWND) { *OVERLAY_HWND.lock().unwrap() = hwnd.0 as isize; }
fn load_overlay() -> Option<HWND> {
    let v = *OVERLAY_HWND.lock().unwrap();
    if v == 0 { None } else { Some(HWND(v as *mut _)) }
}

pub fn select_and_capture() -> Option<CaptureAction> {
    STATE.lock().unwrap().reset();
    init_screen_capture();
    ensure_fonts();

    let hwnd = create_overlay()?;

    let mut msg = MSG::default();
    unsafe {
        loop {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() { break; }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if STATE.lock().unwrap().action != Action::None { break; }
        }
    }

    let (action, x, y, w, h) = {
        let s = STATE.lock().unwrap();
        let (x, y, w, h) = s.rect();
        (s.action, x, y, w, h)
    };

    unsafe { let _ = DestroyWindow(hwnd); }

    if action == Action::Cancel || w < 5 || h < 5 {
        cleanup_screen_capture();
        return None;
    }

    let result = extract_pixels(x, y, w, h);
    cleanup_screen_capture();

    result.map(|(pix, pw, ph)| match action {
        Action::Translate => CaptureAction::Translate(pix, pw, ph),
        Action::Save => CaptureAction::Save(pix, pw, ph),
        Action::Clipboard => CaptureAction::Clipboard(pix, pw, ph),
        _ => CaptureAction::Translate(pix, pw, ph),
    })
}

// ============================================================
// Overlay window
// ============================================================

fn create_overlay() -> Option<HWND> {
    unsafe {
        let hmodule = GetModuleHandleW(None).ok()?;
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransOverlay3");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let (sx, sy, sw, sh) = screen_bounds();

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class, w!(""), WS_POPUP | WS_VISIBLE,
            sx, sy, sw, sh,
            HWND::default(), HMENU::default(), hinstance, None,
        ).ok()?;

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        store_overlay(hwnd);
        Some(hwnd)
    }
}

fn signal_close() {
    if let Some(hwnd) = load_overlay() {
        unsafe { let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)); }
    }
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn overlay_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_to_point(lp);
                let mut s = STATE.lock().unwrap();

                match s.phase {
                    Phase::Selecting => {
                        s.dragging = true;
                        s.start_x = x; s.start_y = y;
                        s.end_x = x;   s.end_y = y;
                        drop(s);
                        SetCapture(hwnd);
                    }
                    Phase::Toolbar => {
                        let (rx, ry, rw, rh) = s.rect();
                        // Handle-first: grab a handle over hitting toolbar /
                        // cancelling.  Matches expected WYSIWYG behaviour.
                        if let Some(h) = handle_at(x, y, rx, ry, rw, rh) {
                            s.normalize();
                            s.resizing = Some(h);
                            drop(s);
                            SetCapture(hwnd);
                        } else {
                            let (_, _, _, screen_h) = screen_bounds();
                            drop(s);
                            let (btn_tr, btn_save) =
                                toolbar_buttons(rx, ry, rw, rh, screen_h);

                            if point_in_btn(x, y, &btn_tr) {
                                STATE.lock().unwrap().action = Action::Translate;
                            } else if point_in_btn(x, y, &btn_save) {
                                STATE.lock().unwrap().action = Action::Save;
                            } else {
                                STATE.lock().unwrap().action = Action::Cancel;
                            }
                            signal_close();
                        }
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let mut s = STATE.lock().unwrap();
                let (x, y) = lparam_to_point(lp);
                if s.dragging && s.phase == Phase::Selecting {
                    s.end_x = x; s.end_y = y;
                    drop(s);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if let Some(h) = s.resizing {
                    // start_* = top-left, end_* = bottom-right (we normalised
                    // on mouse-down).  Update only the edges the grabbed
                    // handle owns.
                    match h {
                        Handle::NW => { s.start_x = x; s.start_y = y; }
                        Handle::N  => { s.start_y = y; }
                        Handle::NE => { s.start_y = y; s.end_x = x; }
                        Handle::E  => { s.end_x = x; }
                        Handle::SE => { s.end_x = x; s.end_y = y; }
                        Handle::S  => { s.end_y = y; }
                        Handle::SW => { s.start_x = x; s.end_y = y; }
                        Handle::W  => { s.start_x = x; }
                    }
                    drop(s);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Show a resize cursor while hovering a handle (or resizing).
                let s = STATE.lock().unwrap();
                let phase = s.phase;
                let resizing = s.resizing;
                let (rx, ry, rw, rh) = s.rect();
                drop(s);

                if let Some(h) = resizing {
                    let _ = SetCursor(LoadCursorW(None, h.cursor()).unwrap_or_default());
                    return LRESULT(1);
                }
                if phase == Phase::Toolbar {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let _ = ScreenToClient(hwnd, &mut pt);
                        if let Some(h) = handle_at(pt.x, pt.y, rx, ry, rw, rh) {
                            let _ = SetCursor(
                                LoadCursorW(None, h.cursor()).unwrap_or_default()
                            );
                            return LRESULT(1);
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_LBUTTONUP => {
                let mut s = STATE.lock().unwrap();
                if s.dragging && s.phase == Phase::Selecting {
                    drop(s);
                    let _ = ReleaseCapture();
                    let (x, y) = lparam_to_point(lp);
                    let mut s = STATE.lock().unwrap();
                    s.end_x = x; s.end_y = y;
                    s.dragging = false;

                    let (_, _, w, h) = s.rect();
                    if w < 5 || h < 5 {
                        s.action = Action::Cancel;
                        drop(s);
                        signal_close();
                    } else {
                        s.phase = Phase::Toolbar;
                        drop(s);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if s.resizing.is_some() {
                    s.resizing = None;
                    // Rebuild (start=NW, end=SE) in case the handle was dragged
                    // past the opposite edge (flip).
                    s.normalize();
                    drop(s);
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_RBUTTONDOWN => {
                STATE.lock().unwrap().action = Action::Cancel;
                let _ = ReleaseCapture();
                signal_close();
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wp.0 == VK_ESCAPE.0 as usize {
                    STATE.lock().unwrap().action = Action::Cancel;
                    let _ = ReleaseCapture();
                    signal_close();
                } else if wp.0 == VK_C.0 as usize {
                    let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
                    if ctrl && STATE.lock().unwrap().phase == Phase::Toolbar {
                        STATE.lock().unwrap().action = Action::Clipboard;
                        signal_close();
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => { let _ = DestroyWindow(hwnd); LRESULT(0) }
            WM_DESTROY => { *OVERLAY_HWND.lock().unwrap() = 0; LRESULT(0) }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint_overlay(hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ============================================================
// Painting
// ============================================================

unsafe fn paint_overlay(hdc: HDC) {
    unsafe {
        let (orig_dc, dim_dc, back_dc, sw, sh) = {
            let cap = SCREEN_CAP.lock().unwrap();
            let Some(c) = cap.as_ref() else { return };
            (
                HDC(c.orig_dc as *mut _),
                HDC(c.dim_dc as *mut _),
                HDC(c.back_dc as *mut _),
                c.width, c.height,
            )
        };

        let s = STATE.lock().unwrap();
        let (rx, ry, rw, rh) = s.rect();
        let phase = s.phase;
        let dragging = s.dragging;
        drop(s);

        let has_selection = (dragging || phase == Phase::Toolbar) && rw > 2 && rh > 2;

        // Compose the whole scene into the back buffer first, then blit it
        // out as a single op.  Drawing straight to `hdc` in multiple passes
        // (dim → selection → border → badge → buttons) used to flash
        // during rapid mouse moves.
        if has_selection {
            let _ = BitBlt(back_dc, 0, 0, sw, sh, dim_dc, 0, 0, SRCCOPY);
            let _ = BitBlt(back_dc, rx, ry, rw, rh, orig_dc, rx, ry, SRCCOPY);

            let pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_ACCENT));
            let old_pen = SelectObject(back_dc, pen);
            let old_brush = SelectObject(back_dc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(back_dc, rx, ry, rx + rw, ry + rh);
            SelectObject(back_dc, old_pen);
            SelectObject(back_dc, old_brush);
            let _ = DeleteObject(pen);

            draw_size_badge(back_dc, rx, ry, rw, rh);

            if phase == Phase::Toolbar {
                draw_resize_handles(back_dc, rx, ry, rw, rh);
                let (btn_tr, btn_save) = toolbar_buttons(rx, ry, rw, rh, sh);
                draw_button(back_dc, &btn_tr, i18n::t("capture.btn.translate"), true);
                draw_button(back_dc, &btn_save, i18n::t("capture.btn.save"), false);
                draw_hint(back_dc, rx, ry, rw, rh, sh);
            }
        } else {
            let _ = BitBlt(back_dc, 0, 0, sw, sh, dim_dc, 0, 0, SRCCOPY);
            draw_initial_hint(back_dc, sw, sh);
        }

        // Single blit to the visible surface — the one frame the user
        // actually sees.
        let _ = BitBlt(hdc, 0, 0, sw, sh, back_dc, 0, 0, SRCCOPY);
    }
}

unsafe fn draw_resize_handles(hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32) {
    unsafe {
        let fill = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let pen  = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_ACCENT));
        let op = SelectObject(hdc, pen);
        let ob = SelectObject(hdc, fill);
        for h in Handle::all() {
            let r = handle_rect(rx, ry, rw, rh, *h);
            let _ = Rectangle(hdc, r.left, r.top, r.right, r.bottom);
        }
        SelectObject(hdc, op);
        SelectObject(hdc, ob);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);
    }
}

unsafe fn draw_size_badge(hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32) {
    unsafe {
        let size_text = format!("{rw}x{rh}");
        let f = font(|f| f.badge);
        let old_font = SelectObject(hdc, f);

        let mut wide = to_wide(&size_text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut measure_rc = RECT { left: 0, top: 0, right: 200, bottom: 0 };
        DrawTextW(hdc, &mut wide, &mut measure_rc, DRAW_TEXT_FORMAT(0x0C00));
        let tw = measure_rc.right - measure_rc.left;
        let th = measure_rc.bottom - measure_rc.top;

        let badge_w = tw + 16;
        let badge_h = th + 8;
        let badge_x = rx + rw - badge_w - 4;
        let badge_y = if ry - badge_h - 6 < 2 { ry + 4 } else { ry - badge_h - 6 };

        let badge_brush = CreateSolidBrush(COLORREF(theme::CLR_BG));
        let badge_pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_ACCENT));
        let old_p = SelectObject(hdc, badge_pen);
        let old_b = SelectObject(hdc, badge_brush);
        let _ = RoundRect(hdc, badge_x, badge_y, badge_x + badge_w, badge_y + badge_h, 6, 6);
        SelectObject(hdc, old_p);
        SelectObject(hdc, old_b);
        let _ = DeleteObject(badge_brush);
        let _ = DeleteObject(badge_pen);

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00FF_FFFF));
        let mut badge_rc = RECT {
            left: badge_x, top: badge_y,
            right: badge_x + badge_w, bottom: badge_y + badge_h,
        };
        DrawTextW(hdc, &mut wide, &mut badge_rc, DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX));
        SelectObject(hdc, old_font);
    }
}

const DT_CENTER_VCENTER_SINGLE_NOPREFIX: u32 = 0x0825;
const CLR_BTN_BG: u32 = 0x0036_3636;

unsafe fn draw_button(hdc: HDC, btn: &BtnRect, text: &str, primary: bool) {
    unsafe {
        let (bg_clr, border_clr, text_clr) = if primary {
            (theme::CLR_ACCENT, theme::CLR_ACCENT, 0x00FF_FFFF_u32)
        } else {
            (CLR_BTN_BG, 0x0060_6060_u32, theme::CLR_TEXT_BRIGHT)
        };

        let bg = CreateSolidBrush(COLORREF(bg_clr));
        let border_pen = CreatePen(PS_SOLID, 1, COLORREF(border_clr));
        let old_pen = SelectObject(hdc, border_pen);
        let old_brush = SelectObject(hdc, bg);
        let _ = RoundRect(hdc, btn.x, btn.y, btn.x + btn.w, btn.y + btn.h, 8, 8);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(bg);
        let _ = DeleteObject(border_pen);

        let f = font(|f| f.button);
        let old_font = SelectObject(hdc, f);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(text_clr));
        let mut wide = to_wide(text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut trc = RECT { left: btn.x, top: btn.y, right: btn.x + btn.w, bottom: btn.y + btn.h };
        DrawTextW(hdc, &mut wide, &mut trc, DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX));
        SelectObject(hdc, old_font);
    }
}

unsafe fn draw_hint(hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32, sh: i32) {
    unsafe {
        let (btn_tr, _) = toolbar_buttons(rx, ry, rw, rh, sh);
        let hint = i18n::t("capture.hint.copy");
        let f = font(|f| f.hint);
        let old_font = SelectObject(hdc, f);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(theme::CLR_HINT));
        let mut wide = to_wide(hint);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut rc = RECT {
            left: btn_tr.x, top: btn_tr.y + BTN_H + 6,
            right: btn_tr.x + BTN_W * 2 + BTN_GAP, bottom: btn_tr.y + BTN_H + 24,
        };
        DrawTextW(hdc, &mut wide, &mut rc, DRAW_TEXT_FORMAT(0x0802)); // DT_RIGHT | DT_NOPREFIX
        SelectObject(hdc, old_font);
    }
}

unsafe fn draw_initial_hint(hdc: HDC, sw: i32, sh: i32) {
    unsafe {
        let hint = i18n::t("capture.hint.initial");
        let f = font(|f| f.initial);
        let old_font = SelectObject(hdc, f);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00C0_C0C0));
        let mut wide = to_wide(hint);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut rc = RECT { left: 0, top: sh / 2 - 20, right: sw, bottom: sh / 2 + 20 };
        DrawTextW(hdc, &mut wide, &mut rc, DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX));
        SelectObject(hdc, old_font);
    }
}
