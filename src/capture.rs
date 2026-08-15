use crate::button;
use crate::i18n;
use crate::paint;
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
    /// Scrolling capture.  Carries the selected rectangle in virtual-screen
    /// coordinates rather than pixels: the content below the fold hasn't been
    /// painted yet, so it has to be grabbed live once the overlay is gone.
    FullPage { x: i32, y: i32, w: i32, h: i32 },
}

// ============================================================
// Internal state
// ============================================================

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Selecting,
    Toolbar,
}

#[derive(PartialEq, Clone, Copy)]
enum Action {
    None,
    Translate,
    Save,
    Clipboard,
    FullPage,
    Cancel,
}

/// Annotation tool armed in the side strip.  `None` is the resting state, in
/// which clicks inside the selection go to the resize handles as before.
#[derive(PartialEq, Clone, Copy)]
enum Tool {
    None,
    Pen,
    Rect,
}

/// One drawn annotation.  Held in overlay coordinates — the same frame the
/// selection rect lives in — so a shape stays put on screen when the
/// selection is dragged out from under it.
#[derive(Clone)]
enum Shape {
    /// Freehand stroke: every position the pointer visited while dragging.
    Pen(Vec<POINT>),
    /// Corner-to-corner drag; either corner may be the smaller one.
    Rect(RECT),
}

/// Eight resize handles around the selection rect.  Compass directions
/// (NW = top-left, SE = bottom-right, etc.) map cleanly to what the
/// cursor and drag behaviour should do.
#[derive(PartialEq, Clone, Copy)]
enum Handle {
    NW,
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
}

impl Handle {
    fn all() -> &'static [Handle] {
        &[
            Handle::NW,
            Handle::N,
            Handle::NE,
            Handle::E,
            Handle::SE,
            Handle::S,
            Handle::SW,
            Handle::W,
        ]
    }
    fn cursor(self) -> windows::core::PCWSTR {
        match self {
            Handle::NW | Handle::SE => IDC_SIZENWSE,
            Handle::NE | Handle::SW => IDC_SIZENESW,
            Handle::N | Handle::S => IDC_SIZENS,
            Handle::E | Handle::W => IDC_SIZEWE,
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
    /// Toolbar button the pointer is over, so it can light up like a real one.
    hover_btn: Option<usize>,
    /// Tool-strip button the pointer is over.
    hover_tool: Option<usize>,
    /// Armed annotation tool, everything drawn with one so far, and the shape
    /// currently under the pointer but not yet committed.
    tool: Tool,
    shapes: Vec<Shape>,
    drawing: Option<Shape>,
}

impl OverlayState {
    const fn new() -> Self {
        Self {
            phase: Phase::Selecting,
            dragging: false,
            resizing: None,
            start_x: 0,
            start_y: 0,
            end_x: 0,
            end_y: 0,
            action: Action::None,
            hover_btn: None,
            hover_tool: None,
            tool: Tool::None,
            shapes: Vec::new(),
            drawing: None,
        }
    }
    fn reset(&mut self) {
        *self = Self::new();
    }
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
        self.end_x = rx + rw;
        self.end_y = ry + rh;
    }
}

/// Rect of a single resize handle centred on the appropriate edge/corner.
fn handle_rect(rx: i32, ry: i32, rw: i32, rh: i32, h: Handle) -> RECT {
    let half = HANDLE_SIZE / 2;
    let (cx, cy) = match h {
        Handle::NW => (rx, ry),
        Handle::N => (rx + rw / 2, ry),
        Handle::NE => (rx + rw, ry),
        Handle::E => (rx + rw, ry + rh / 2),
        Handle::SE => (rx + rw, ry + rh),
        Handle::S => (rx + rw / 2, ry + rh),
        Handle::SW => (rx, ry + rh),
        Handle::W => (rx, ry + rh / 2),
    };
    RECT {
        left: cx - half,
        top: cy - half,
        right: cx - half + HANDLE_SIZE,
        bottom: cy - half + HANDLE_SIZE,
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

        let _ = AlphaBlend(
            dim_dc,
            0,
            0,
            sw,
            sh,
            black_dc,
            0,
            0,
            1,
            1,
            BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: 140,
                AlphaFormat: 0,
            },
        );

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
            orig_dc: orig_dc.0 as isize,
            orig_bmp: orig_bmp.0 as isize,
            orig_old: orig_old.0 as isize,
            dim_dc: dim_dc.0 as isize,
            dim_bmp: dim_bmp.0 as isize,
            dim_old: dim_old.0 as isize,
            back_dc: back_dc.0 as isize,
            back_bmp: back_bmp.0 as isize,
            back_old: back_old.0 as isize,
            width: sw,
            height: sh,
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
    SCREEN_CAP
        .lock()
        .unwrap()
        .as_ref()
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

fn extract_pixels(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    shapes: &[Shape],
) -> Option<(Vec<u8>, u32, u32)> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let orig_dc_raw = SCREEN_CAP.lock().unwrap().as_ref().map(|c| c.orig_dc)?;

    unsafe {
        let src_dc = HDC(orig_dc_raw as *mut _);
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);
        let _ = BitBlt(mem_dc, 0, 0, w, h, src_dc, x, y, SRCCOPY);
        // Annotations are in overlay coordinates; shift them into the cropped
        // image's own frame so what was drawn lands where it was drawn.
        draw_shapes(
            mem_dc,
            &RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
            -x,
            -y,
            shapes,
            None,
        );
        SelectObject(mem_dc, old);

        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pixels = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            screen_dc,
            bmp,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        // Ensure alpha = 255.
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[3] = 255;
        }

        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        Some((pixels, w as u32, h as u32))
    }
}

// ============================================================
// Toolbar buttons
// ============================================================

const BTN_W: i32 = 124;
const BTN_H: i32 = 34;
const BTN_GAP: i32 = 10;
const BTN_MARGIN: i32 = 12;
const BTN_COUNT: usize = 3;

/// Total width of the toolbar row, buttons plus the gaps between them.
const TOOLBAR_W: i32 = BTN_W * BTN_COUNT as i32 + BTN_GAP * (BTN_COUNT as i32 - 1);

struct BtnRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn toolbar_buttons(
    sx: i32,
    sy: i32,
    sw: i32,
    sh_sel: i32,
    screen_h: i32,
) -> [BtnRect; BTN_COUNT] {
    let bx = (sx + sw - TOOLBAR_W).max(4);

    let below = sy + sh_sel + BTN_MARGIN;
    let by = if below + BTN_H + 4 > screen_h {
        sy - BTN_H - BTN_MARGIN
    } else {
        below
    }
    .max(4);

    std::array::from_fn(|i| BtnRect {
        x: bx + i as i32 * (BTN_W + BTN_GAP),
        y: by,
        w: BTN_W,
        h: BTN_H,
    })
}

fn point_in_btn(px: i32, py: i32, btn: &BtnRect) -> bool {
    px >= btn.x && px <= btn.x + btn.w && py >= btn.y && py <= btn.y + btn.h
}

fn point_in_selection(px: i32, py: i32, rx: i32, ry: i32, rw: i32, rh: i32) -> bool {
    px >= rx && px < rx + rw && py >= ry && py < ry + rh
}

// ============================================================
// Annotation tool strip
// ============================================================

/// Square buttons stacked down the right-hand side of the selection.
const TOOL_SIZE: i32 = 34;
const TOOL_GAP: i32 = 8;
const TOOL_MARGIN: i32 = 12;

/// Which tool each button in the strip arms, top to bottom.
const TOOLS: [Tool; 2] = [Tool::Pen, Tool::Rect];

/// Height of the whole strip.
const TOOL_STRIP_H: i32 = TOOL_SIZE * TOOLS.len() as i32 + TOOL_GAP * (TOOLS.len() as i32 - 1);

/// Colour every annotation is drawn in, and how thick.  COLORREF is BGR, so
/// pure red is 0x0000_00FF.
const INK: u32 = 0x0000_00FF;
const INK_WIDTH: i32 = 3;

/// The strip sits just outside the right edge of the selection, aligned with
/// its top.  It flips to the left when the selection runs up against the right
/// edge of the screen, and tucks inside when neither side has room.
///
/// The action toolbar is right-aligned to the same edge and sits *below* the
/// selection, so the two never overlap however small the selection gets.
fn tool_buttons(rx: i32, ry: i32, rw: i32, screen_w: i32, screen_h: i32) -> [BtnRect; TOOLS.len()] {
    let outside_right = rx + rw + TOOL_MARGIN;
    let outside_left = rx - TOOL_MARGIN - TOOL_SIZE;
    let tx = if outside_right + TOOL_SIZE + 4 <= screen_w {
        outside_right
    } else if outside_left >= 4 {
        outside_left
    } else {
        (rx + rw - TOOL_SIZE - 4).max(4)
    };
    let ty = ry.min(screen_h - TOOL_STRIP_H - 4).max(4);

    std::array::from_fn(|i| BtnRect {
        x: tx,
        y: ty + i as i32 * (TOOL_SIZE + TOOL_GAP),
        w: TOOL_SIZE,
        h: TOOL_SIZE,
    })
}

// ============================================================
// Cached overlay fonts (created once, never leaked)
// ============================================================

struct OverlayFonts {
    badge: isize,   // -12, weight 600
    button: isize,  // -13, weight 500 — AppKit labels are near-regular
    hint: isize,    // -11, weight 400
    initial: isize, // -16, weight 400
}

static OVERLAY_FONTS: Mutex<Option<OverlayFonts>> = Mutex::new(None);

fn ensure_fonts() {
    let mut guard = OVERLAY_FONTS.lock().unwrap();
    if guard.is_some() {
        return;
    }
    unsafe {
        *guard = Some(OverlayFonts {
            badge: CreateFontW(-12, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0
                as isize,
            button: CreateFontW(-13, 0, 0, 0, 500, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0
                as isize,
            hint: CreateFontW(-11, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0 as isize,
            initial: CreateFontW(-16, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI")).0
                as isize,
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

fn store_overlay(hwnd: HWND) {
    *OVERLAY_HWND.lock().unwrap() = hwnd.0 as isize;
}
fn load_overlay() -> Option<HWND> {
    let v = *OVERLAY_HWND.lock().unwrap();
    if v == 0 {
        None
    } else {
        Some(HWND(v as *mut _))
    }
}

pub fn select_and_capture() -> Option<CaptureAction> {
    STATE.lock().unwrap().reset();
    init_screen_capture();
    ensure_fonts();

    let hwnd = create_overlay()?;

    let mut msg = MSG::default();
    unsafe {
        loop {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if STATE.lock().unwrap().action != Action::None {
                break;
            }
        }
    }

    let (action, x, y, w, h, shapes) = {
        let s = STATE.lock().unwrap();
        let (x, y, w, h) = s.rect();
        (s.action, x, y, w, h, s.shapes.clone())
    };
    // Overlay client coords are relative to the virtual screen origin, which
    // is negative on multi-monitor setups.
    let (origin_x, origin_y, _, _) = screen_bounds();

    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    if action == Action::Cancel || w < 5 || h < 5 {
        cleanup_screen_capture();
        return None;
    }

    if action == Action::FullPage {
        cleanup_screen_capture();
        // The overlay covered everything; give whatever was underneath a
        // moment to repaint before live frames start being grabbed.
        std::thread::sleep(std::time::Duration::from_millis(250));
        return Some(CaptureAction::FullPage {
            x: origin_x + x,
            y: origin_y + y,
            w,
            h,
        });
    }

    let result = extract_pixels(x, y, w, h, &shapes);
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
            class,
            w!(""),
            WS_POPUP | WS_VISIBLE,
            sx,
            sy,
            sw,
            sh,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .ok()?;

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        store_overlay(hwnd);
        Some(hwnd)
    }
}

fn signal_close() {
    if let Some(hwnd) = load_overlay() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_to_point(lp);
                let mut s = STATE.lock().unwrap();

                match s.phase {
                    Phase::Selecting => {
                        s.dragging = true;
                        s.start_x = x;
                        s.start_y = y;
                        s.end_x = x;
                        s.end_y = y;
                        drop(s);
                        SetCapture(hwnd);
                    }
                    Phase::Toolbar => {
                        // Precedence, outermost first: the tool strip, then
                        // ink inside the selection, then the resize handles,
                        // then the action toolbar — and anything else cancels.
                        let (rx, ry, rw, rh) = s.rect();
                        let tool = s.tool;
                        let (_, _, screen_w, screen_h) = screen_bounds();
                        let tools = tool_buttons(rx, ry, rw, screen_w, screen_h);

                        if let Some(i) = tools.iter().position(|b| point_in_btn(x, y, b)) {
                            // Clicking the armed tool disarms it, handing the
                            // resize handles back.
                            s.tool = if tool == TOOLS[i] {
                                Tool::None
                            } else {
                                TOOLS[i]
                            };
                            drop(s);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if tool != Tool::None && point_in_selection(x, y, rx, ry, rw, rh) {
                            s.drawing = Some(match tool {
                                Tool::Rect => Shape::Rect(RECT {
                                    left: x,
                                    top: y,
                                    right: x,
                                    bottom: y,
                                }),
                                _ => Shape::Pen(vec![POINT { x, y }]),
                            });
                            drop(s);
                            SetCapture(hwnd);
                        } else if let Some(h) =
                            handle_at(x, y, rx, ry, rw, rh).filter(|_| tool == Tool::None)
                        {
                            s.normalize();
                            s.resizing = Some(h);
                            drop(s);
                            SetCapture(hwnd);
                        } else {
                            drop(s);
                            let btns = toolbar_buttons(rx, ry, rw, rh, screen_h);

                            STATE.lock().unwrap().action = if point_in_btn(x, y, &btns[0]) {
                                Action::Translate
                            } else if point_in_btn(x, y, &btns[1]) {
                                Action::Save
                            } else if point_in_btn(x, y, &btns[2]) {
                                Action::FullPage
                            } else {
                                Action::Cancel
                            };
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
                    s.end_x = x;
                    s.end_y = y;
                    drop(s);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if let Some(shape) = s.drawing.as_mut() {
                    match shape {
                        Shape::Pen(pts) => {
                            // WM_MOUSEMOVE fires for a pointer that has not
                            // actually moved; every duplicate point is a
                            // wasted segment on every repaint from here on.
                            if pts.last().is_none_or(|p| p.x != x || p.y != y) {
                                pts.push(POINT { x, y });
                            }
                        }
                        Shape::Rect(r) => {
                            r.right = x;
                            r.bottom = y;
                        }
                    }
                    drop(s);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if let Some(h) = s.resizing {
                    // start_* = top-left, end_* = bottom-right (we normalised
                    // on mouse-down).  Update only the edges the grabbed
                    // handle owns.
                    match h {
                        Handle::NW => {
                            s.start_x = x;
                            s.start_y = y;
                        }
                        Handle::N => {
                            s.start_y = y;
                        }
                        Handle::NE => {
                            s.start_y = y;
                            s.end_x = x;
                        }
                        Handle::E => {
                            s.end_x = x;
                        }
                        Handle::SE => {
                            s.end_x = x;
                            s.end_y = y;
                        }
                        Handle::S => {
                            s.end_y = y;
                        }
                        Handle::SW => {
                            s.start_x = x;
                            s.end_y = y;
                        }
                        Handle::W => {
                            s.start_x = x;
                        }
                    }
                    drop(s);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if s.phase == Phase::Toolbar {
                    let (rx, ry, rw, rh) = s.rect();
                    let was = (s.hover_btn, s.hover_tool);
                    drop(s);

                    let (_, _, screen_w, screen_h) = screen_bounds();
                    let btns = toolbar_buttons(rx, ry, rw, rh, screen_h);
                    let tools = tool_buttons(rx, ry, rw, screen_w, screen_h);
                    let now = (
                        btns.iter().position(|b| point_in_btn(x, y, b)),
                        tools.iter().position(|b| point_in_btn(x, y, b)),
                    );
                    if now != was {
                        let mut s = STATE.lock().unwrap();
                        (s.hover_btn, s.hover_tool) = now;
                        drop(s);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Show a resize cursor while hovering a handle (or resizing).
                let s = STATE.lock().unwrap();
                let phase = s.phase;
                let resizing = s.resizing;
                let tool = s.tool;
                let (rx, ry, rw, rh) = s.rect();
                drop(s);

                if let Some(h) = resizing {
                    let _ = SetCursor(LoadCursorW(None, h.cursor()).unwrap_or_default());
                    return LRESULT(1);
                }
                // With a tool armed the handles are dead, so promising a
                // resize with the cursor would be a lie — a drag there draws.
                if phase == Phase::Toolbar && tool == Tool::None {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let _ = ScreenToClient(hwnd, &mut pt);
                        if let Some(h) = handle_at(pt.x, pt.y, rx, ry, rw, rh) {
                            let _ = SetCursor(LoadCursorW(None, h.cursor()).unwrap_or_default());
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
                    s.end_x = x;
                    s.end_y = y;
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
                } else if let Some(shape) = s.drawing.take() {
                    // A click with no drag leaves a single point or a
                    // zero-size rect; neither is worth keeping.
                    let worth_keeping = match &shape {
                        Shape::Pen(pts) => pts.len() > 1,
                        Shape::Rect(r) => {
                            (r.right - r.left).abs() > 2 && (r.bottom - r.top).abs() > 2
                        }
                    };
                    if worth_keeping {
                        s.shapes.push(shape);
                    }
                    drop(s);
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(hwnd, None, false);
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
                } else if wp.0 == VK_C.0 as usize || wp.0 == VK_S.0 as usize {
                    // Ctrl+C and Ctrl+S stand in for the two toolbar buttons
                    // that produce a file or a paste, so a capture can be
                    // finished without ever moving the mouse to the toolbar.
                    let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
                    if ctrl && STATE.lock().unwrap().phase == Phase::Toolbar {
                        STATE.lock().unwrap().action = if wp.0 == VK_C.0 as usize {
                            Action::Clipboard
                        } else {
                            Action::Save
                        };
                        signal_close();
                    }
                } else if wp.0 == VK_Z.0 as usize {
                    // Undo.  Without it one bad stroke means starting the
                    // whole capture over.
                    let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
                    if ctrl && STATE.lock().unwrap().shapes.pop().is_some() {
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                *OVERLAY_HWND.lock().unwrap() = 0;
                LRESULT(0)
            }
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
                c.width,
                c.height,
            )
        };

        let s = STATE.lock().unwrap();
        let (rx, ry, rw, rh) = s.rect();
        let phase = s.phase;
        let dragging = s.dragging;
        let hover_btn = s.hover_btn;
        let hover_tool = s.hover_tool;
        let tool = s.tool;
        // Cloned rather than held under the lock: a stroke is a few thousand
        // points at worst, which is nothing beside the full-screen blit this
        // frame is already doing.
        let shapes = s.shapes.clone();
        let drawing = s.drawing.clone();
        drop(s);

        let has_selection = (dragging || phase == Phase::Toolbar) && rw > 2 && rh > 2;

        // Compose the whole scene into the back buffer first, then blit it
        // out as a single op.  Drawing straight to `hdc` in multiple passes
        // (dim → selection → border → badge → buttons) used to flash
        // during rapid mouse moves.
        if has_selection {
            let _ = BitBlt(back_dc, 0, 0, sw, sh, dim_dc, 0, 0, SRCCOPY);
            let _ = BitBlt(back_dc, rx, ry, rw, rh, orig_dc, rx, ry, SRCCOPY);

            // Annotations go on before the selection border so the border
            // stays a clean unbroken line over the top of them.
            let sel = RECT {
                left: rx,
                top: ry,
                right: rx + rw,
                bottom: ry + rh,
            };
            draw_shapes(back_dc, &sel, 0, 0, &shapes, drawing.as_ref());

            let pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_ACCENT));
            let old_pen = SelectObject(back_dc, pen);
            let old_brush = SelectObject(back_dc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(back_dc, rx, ry, rx + rw, ry + rh);
            SelectObject(back_dc, old_pen);
            SelectObject(back_dc, old_brush);
            let _ = DeleteObject(pen);

            draw_size_badge(back_dc, rx, ry, rw, rh);

            if phase == Phase::Toolbar {
                // With a tool armed the handles are neither drawn nor
                // hit-tested — a drag inside the selection is ink, not a
                // resize.  Clicking the armed tool again brings them back.
                if tool == Tool::None {
                    draw_resize_handles(back_dc, rx, ry, rw, rh);
                }
                draw_tool_strip(back_dc, &tool_buttons(rx, ry, rw, sw, sh), tool, hover_tool);
                let btns = toolbar_buttons(rx, ry, rw, rh, sh);
                let labels = [
                    i18n::t("capture.btn.translate"),
                    i18n::t("capture.btn.save"),
                    i18n::t("capture.btn.fullpage"),
                ];
                for (i, (btn, label)) in btns.iter().zip(labels).enumerate() {
                    let variant = if i == 0 {
                        button::Variant::Primary
                    } else {
                        button::Variant::Secondary
                    };
                    let state = if hover_btn == Some(i) {
                        button::State::Hover
                    } else {
                        button::State::Normal
                    };
                    draw_button(back_dc, btn, label, variant, state);
                }
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

/// Replays the annotations onto `hdc`, clipped to `clip` so a stroke that ran
/// off the edge of the selection doesn't land on the dimmed backdrop.
///
/// `dx`/`dy` shift overlay coordinates into the target's own frame: zero when
/// painting the overlay, minus the selection origin when baking into the
/// cropped capture.
unsafe fn draw_shapes(
    hdc: HDC,
    clip: &RECT,
    dx: i32,
    dy: i32,
    shapes: &[Shape],
    pending: Option<&Shape>,
) {
    unsafe {
        if shapes.is_empty() && pending.is_none() {
            return;
        }

        let saved = SaveDC(hdc);
        let rgn = CreateRectRgn(clip.left, clip.top, clip.right, clip.bottom);
        SelectClipRgn(hdc, rgn);

        // A geometric pen is the only kind GDI rounds the ends and joins of.
        // A cosmetic one leaves a visible notch at every join, and a freehand
        // stroke is made of hundreds of them.
        let brush = LOGBRUSH {
            lbStyle: BS_SOLID,
            lbColor: COLORREF(INK),
            lbHatch: 0,
        };
        let mut pen = ExtCreatePen(
            PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
            INK_WIDTH as u32,
            &brush,
            None,
        );
        if pen.is_invalid() {
            pen = CreatePen(PS_SOLID, INK_WIDTH, COLORREF(INK));
        }
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        for shape in shapes.iter().chain(pending) {
            match shape {
                Shape::Pen(pts) => {
                    if pts.len() < 2 {
                        continue;
                    }
                    let moved: Vec<POINT> = pts
                        .iter()
                        .map(|p| POINT {
                            x: p.x + dx,
                            y: p.y + dy,
                        })
                        .collect();
                    let _ = Polyline(hdc, &moved);
                }
                Shape::Rect(r) => {
                    let _ = Rectangle(
                        hdc,
                        r.left.min(r.right) + dx,
                        r.top.min(r.bottom) + dy,
                        r.left.max(r.right) + dx,
                        r.top.max(r.bottom) + dy,
                    );
                }
            }
        }

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
        let _ = RestoreDC(hdc, saved);
        let _ = DeleteObject(rgn);
    }
}

unsafe fn draw_tool_strip(hdc: HDC, tools: &[BtnRect], armed: Tool, hover: Option<usize>) {
    unsafe {
        for (i, btn) in tools.iter().enumerate() {
            let rc = RECT {
                left: btn.x,
                top: btn.y,
                right: btn.x + btn.w,
                bottom: btn.y + btn.h,
            };
            let variant = if armed == TOOLS[i] {
                button::Variant::Primary
            } else {
                button::Variant::Secondary
            };
            let state = if hover == Some(i) {
                button::State::Hover
            } else {
                button::State::Normal
            };
            // The armed button is filled with the ink colour, so the strip
            // itself says what you are about to draw in.  Its glyph then has
            // to go white to stay legible.
            button::draw(hdc, &rc, INK, variant, state);
            let glyph = if variant == button::Variant::Primary {
                0x00FF_FFFF
            } else {
                INK
            };
            match TOOLS[i] {
                Tool::Rect => draw_rect_icon(hdc, &rc, glyph),
                _ => draw_pencil_icon(hdc, &rc, glyph),
            }
        }
    }
}

/// Side of the square the glyphs below are designed in, centred in a button.
const ICON: i32 = 16;

/// Maps a point in the glyph's design square to oversampled button coordinates.
fn glyph_pt(rc: &RECT, ss: i32, x: i32, y: i32) -> POINT {
    POINT {
        x: ((rc.right - rc.left - ICON) / 2 + x) * ss,
        y: ((rc.bottom - rc.top - ICON) / 2 + y) * ss,
    }
}

/// A pencil pointing down-left: filled silhouette, sharpened end at the tip.
/// Filled rather than outlined because at 16 px an outline reads as a smudge.
unsafe fn draw_pencil_icon(hdc: HDC, rc: &RECT, color: u32) {
    unsafe {
        paint::supersampled(hdc, rc, |dc, ss| {
            // Tip, the two shoulders the taper springs from, then the
            // angled cut at the eraser end.
            let pts = [(1, 15), (3, 10), (12, 1), (15, 4), (6, 13)]
                .map(|(x, y)| glyph_pt(rc, ss, x, y));
            let brush = CreateSolidBrush(COLORREF(color));
            let ob = SelectObject(dc, brush);
            let op = SelectObject(dc, GetStockObject(NULL_PEN));
            let _ = Polygon(dc, &pts);
            SelectObject(dc, ob);
            SelectObject(dc, op);
            let _ = DeleteObject(brush);
        });
    }
}

/// An outlined rectangle, drawn with the same two-pixel weight as the ink.
unsafe fn draw_rect_icon(hdc: HDC, rc: &RECT, color: u32) {
    unsafe {
        paint::supersampled(hdc, rc, |dc, ss| {
            let a = glyph_pt(rc, ss, 1, 3);
            let b = glyph_pt(rc, ss, 15, 13);
            let pen = CreatePen(PS_SOLID, 2 * ss, COLORREF(color));
            let op = SelectObject(dc, pen);
            let ob = SelectObject(dc, GetStockObject(NULL_BRUSH));
            // A pen straddles the path, so inset by half its width to keep
            // the stroke inside the glyph box.
            let _ = Rectangle(dc, a.x + ss, a.y + ss, b.x - ss, b.y - ss);
            SelectObject(dc, op);
            SelectObject(dc, ob);
            let _ = DeleteObject(pen);
        });
    }
}

unsafe fn draw_resize_handles(hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32) {
    unsafe {
        let fill = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_ACCENT));
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
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut measure_rc = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 0,
        };
        DrawTextW(hdc, &mut wide, &mut measure_rc, DRAW_TEXT_FORMAT(0x0C00));
        let tw = measure_rc.right - measure_rc.left;
        let th = measure_rc.bottom - measure_rc.top;

        let badge_w = tw + 16;
        let badge_h = th + 8;
        let badge_x = rx + rw - badge_w - 4;
        let badge_y = if ry - badge_h - 6 < 2 {
            ry + 4
        } else {
            ry - badge_h - 6
        };

        let badge_brush = CreateSolidBrush(COLORREF(theme::CLR_BG));
        let badge_pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_ACCENT));
        let old_p = SelectObject(hdc, badge_pen);
        let old_b = SelectObject(hdc, badge_brush);
        let _ = RoundRect(
            hdc,
            badge_x,
            badge_y,
            badge_x + badge_w,
            badge_y + badge_h,
            6,
            6,
        );
        SelectObject(hdc, old_p);
        SelectObject(hdc, old_b);
        let _ = DeleteObject(badge_brush);
        let _ = DeleteObject(badge_pen);

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00FF_FFFF));
        let mut badge_rc = RECT {
            left: badge_x,
            top: badge_y,
            right: badge_x + badge_w,
            bottom: badge_y + badge_h,
        };
        DrawTextW(
            hdc,
            &mut wide,
            &mut badge_rc,
            DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX),
        );
        SelectObject(hdc, old_font);
    }
}

const DT_CENTER_VCENTER_SINGLE_NOPREFIX: u32 = 0x0825;

unsafe fn draw_button(
    hdc: HDC,
    btn: &BtnRect,
    text: &str,
    variant: button::Variant,
    state: button::State,
) {
    unsafe {
        let rc = RECT {
            left: btn.x,
            top: btn.y,
            right: btn.x + btn.w,
            bottom: btn.y + btn.h,
        };
        button::draw(hdc, &rc, theme::CLR_ACCENT, variant, state);

        let f = font(|f| f.button);
        let old_font = SelectObject(hdc, f);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(button::text_color(variant, state)));
        let mut wide = to_wide(text);
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut trc = rc;
        DrawTextW(
            hdc,
            &mut wide,
            &mut trc,
            DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX),
        );
        SelectObject(hdc, old_font);
    }
}

unsafe fn draw_hint(hdc: HDC, rx: i32, ry: i32, rw: i32, rh: i32, sh: i32) {
    unsafe {
        let btns = toolbar_buttons(rx, ry, rw, rh, sh);
        let hint = i18n::t("capture.hint.copy");
        let f = font(|f| f.hint);
        let old_font = SelectObject(hdc, f);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(theme::CLR_HINT));
        let mut wide = to_wide(hint);
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut rc = RECT {
            left: btns[0].x,
            top: btns[0].y + BTN_H + 6,
            right: btns[0].x + TOOLBAR_W,
            bottom: btns[0].y + BTN_H + 24,
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
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut rc = RECT {
            left: 0,
            top: sh / 2 - 20,
            right: sw,
            bottom: sh / 2 + 20,
        };
        DrawTextW(
            hdc,
            &mut wide,
            &mut rc,
            DRAW_TEXT_FORMAT(DT_CENTER_VCENTER_SINGLE_NOPREFIX),
        );
        SelectObject(hdc, old_font);
    }
}
