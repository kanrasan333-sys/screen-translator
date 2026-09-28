//! Antialiased shapes.  Mostly rounded rectangles — the one primitive the
//! whole UI is built from: cards, buttons, switches, text fields, popups —
//! plus `supersampled`, which lends the same treatment to anything else that
//! has to sit on top of them, such as the capture overlay's tool glyphs.
//!
//! GDI does no antialiasing whatsoever, and a stair-stepped corner is the
//! single thing that most gives away a hand-drawn control.  So everything here
//! is painted at 4× into an off-screen buffer laid over a copy of the real
//! background, then box-filtered back down.  It costs a few hundred kilobytes
//! and well under a millisecond per shape.

use windows::Win32::Foundation::{COLORREF, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;

/// Oversampling factor.
const SS: i32 = 4;

/// How a rounded rectangle is drawn.
#[derive(Clone, Copy)]
pub struct Style {
    pub radius: i32,
    pub fill: u32,
    pub border: Option<u32>,
    /// Border thickness in logical pixels.
    pub border_width: i32,
}

impl Style {
    /// A flat fill with no border.
    pub fn flat(radius: i32, fill: u32) -> Self {
        Self {
            radius,
            fill,
            border: None,
            border_width: 1,
        }
    }

    pub fn border(mut self, color: u32) -> Self {
        self.border = Some(color);
        self
    }

    pub fn border_width(mut self, px: i32) -> Self {
        self.border_width = px;
        self
    }
}

/// Paints an arbitrary shape into `rc`, antialiased.
///
/// The closure is handed a DC holding an oversampled copy of whatever is
/// already behind `rc`, plus the factor it was scaled by, and draws in those
/// oversampled coordinates relative to the rect's top-left.  Whatever it
/// leaves behind is box-filtered back down onto `hdc`.
pub unsafe fn supersampled(hdc: HDC, rc: &RECT, draw: impl FnOnce(HDC, i32)) {
    unsafe {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        if w < 2 || h < 2 {
            return;
        }
        let (sw, sh) = (w * SS, h * SS);
        let Some(canvas) = Dib::new(sw, sh) else {
            return;
        };

        // The soft edge has to fade into whatever is actually behind the
        // shape, so start from a copy of it rather than a flat fill.
        SetStretchBltMode(canvas.dc, COLORONCOLOR);
        let _ = StretchBlt(
            canvas.dc, 0, 0, sw, sh, hdc, rc.left, rc.top, w, h, SRCCOPY,
        );

        draw(canvas.dc, SS);

        let (pixels, info) = downsample(&canvas, w, h);
        SetDIBitsToDevice(
            hdc,
            rc.left,
            rc.top,
            w as u32,
            h as u32,
            0,
            0,
            0,
            h as u32,
            pixels.as_ptr() as *const _,
            &info,
            DIB_RGB_COLORS,
        );
    }
}

/// Paints a rounded rectangle into `rc`.
///
/// Only the corners are curved, so on anything bigger than a button only the
/// corners are oversampled; the straight runs are plain fills.  A settings
/// card is a few hundred pixels across, and supersampling all of it on every
/// repaint was the bulk of the window's paint time.
pub unsafe fn round_rect(hdc: HDC, rc: &RECT, style: &Style) {
    unsafe {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let bw = if style.border.is_some() {
            style.border_width
        } else {
            0
        };
        // Corner square: the arc plus the border inside it plus a pixel of
        // slack for the antialiased edge.
        let k = style.radius + bw + 1;
        if w * h < 64 * 64 || w < 2 * k + 2 || h < 2 * k + 2 {
            supersampled(hdc, rc, |dc, ss| draw_shape(dc, rc, rc, style, ss));
            return;
        }

        for (x, y) in [
            (rc.left, rc.top),
            (rc.right - k, rc.top),
            (rc.left, rc.bottom - k),
            (rc.right - k, rc.bottom - k),
        ] {
            let corner = RECT {
                left: x,
                top: y,
                right: x + k,
                bottom: y + k,
            };
            supersampled(hdc, &corner, |dc, ss| draw_shape(dc, &corner, rc, style, ss));
        }

        let fill = CreateSolidBrush(COLORREF(style.fill));
        for band in [
            // Full-height middle column, then the two side columns between
            // the corners.
            RECT {
                left: rc.left + k,
                top: rc.top,
                right: rc.right - k,
                bottom: rc.bottom,
            },
            RECT {
                left: rc.left,
                top: rc.top + k,
                right: rc.left + k,
                bottom: rc.bottom - k,
            },
            RECT {
                left: rc.right - k,
                top: rc.top + k,
                right: rc.right,
                bottom: rc.bottom - k,
            },
        ] {
            let _ = FillRect(hdc, &band, fill);
        }
        let _ = DeleteObject(fill);

        if let Some(border) = style.border {
            let brush = CreateSolidBrush(COLORREF(border));
            for edge in [
                RECT {
                    left: rc.left + k,
                    top: rc.top,
                    right: rc.right - k,
                    bottom: rc.top + bw,
                },
                RECT {
                    left: rc.left + k,
                    top: rc.bottom - bw,
                    right: rc.right - k,
                    bottom: rc.bottom,
                },
                RECT {
                    left: rc.left,
                    top: rc.top + k,
                    right: rc.left + bw,
                    bottom: rc.bottom - k,
                },
                RECT {
                    left: rc.right - bw,
                    top: rc.top + k,
                    right: rc.right,
                    bottom: rc.bottom - k,
                },
            ] {
                let _ = FillRect(hdc, &edge, brush);
            }
            let _ = DeleteObject(brush);
        }
    }
}

/// Draws the whole of `shape` (fill, then border) into an oversampled canvas
/// that covers `canvas` — which may be all of the shape or just one corner.
unsafe fn draw_shape(dc: HDC, canvas: &RECT, shape: &RECT, style: &Style, ss: i32) {
    unsafe {
        let body = RECT {
            left: (shape.left - canvas.left) * ss,
            top: (shape.top - canvas.top) * ss,
            right: (shape.right - canvas.left) * ss,
            bottom: (shape.bottom - canvas.top) * ss,
        };
        let r = style.radius * ss;
        fill_round(dc, &body, r, style.fill);
        if let Some(border) = style.border {
            stroke(dc, &body, r, border, style.border_width * ss);
        }
    }
}

/// A one-pixel hairline, antialiased the same way.  Used for the separators
/// between rows of a grouped list.
pub unsafe fn hairline(hdc: HDC, x1: i32, x2: i32, y: i32, color: u32) {
    unsafe {
        let rc = RECT {
            left: x1,
            top: y,
            right: x2,
            bottom: y + 1,
        };
        let brush = CreateSolidBrush(COLORREF(color));
        let _ = FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush);
    }
}


/// An antialiased polyline — round caps and joins — through `pts`, which are
/// relative to `rc`'s top-left.  For glyph-like strokes: chevrons, ticks,
/// slashes, the close cross.
pub unsafe fn polyline(hdc: HDC, rc: &RECT, pts: &[(f32, f32)], width: f32, color: u32) {
    unsafe {
        supersampled(hdc, rc, |dc, ss| {
            let brush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: COLORREF(color),
                lbHatch: 0,
            };
            let pen = ExtCreatePen(
                PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
                (width * ss as f32).round() as u32,
                &brush,
                None,
            );
            let op = SelectObject(dc, pen);
            let scaled: Vec<POINT> = pts
                .iter()
                .map(|&(x, y)| POINT {
                    x: (x * ss as f32).round() as i32,
                    y: (y * ss as f32).round() as i32,
                })
                .collect();
            let _ = Polyline(dc, &scaled);
            SelectObject(dc, op);
            let _ = DeleteObject(pen);
        });
    }
}

/// A filled, antialiased circle.
pub unsafe fn circle(hdc: HDC, cx: i32, cy: i32, r: i32, fill: u32) {
    unsafe {
        let rc = RECT {
            left: cx - r,
            top: cy - r,
            right: cx + r,
            bottom: cy + r,
        };
        round_rect(hdc, &rc, &Style::flat(r, fill));
    }
}

/// Renders `draw` twice, over black and over white, and recovers per-pixel
/// alpha from the difference.  Returns premultiplied BGRA, top-down.
///
/// GDI writes no alpha of its own, so this is how a shape drawn with ordinary
/// GDI calls becomes something with transparent, antialiased edges — an icon.
pub unsafe fn render_argb(w: i32, h: i32, draw: impl Fn(HDC, i32)) -> Option<Vec<u8>> {
    unsafe {
        let canvas = Dib::new(w * SS, h * SS)?;
        let full = RECT {
            left: 0,
            top: 0,
            right: w * SS,
            bottom: h * SS,
        };
        let mut passes = [Vec::new(), Vec::new()];
        for (i, bg) in [0x0000_0000u32, 0x00FF_FFFF].into_iter().enumerate() {
            let brush = CreateSolidBrush(COLORREF(bg));
            let _ = FillRect(canvas.dc, &full, brush);
            let _ = DeleteObject(brush);
            draw(canvas.dc, SS);
            let _ = GdiFlush();
            passes[i] = downsample_raw(&canvas, w, h);
        }
        let [mut black, white] = passes;
        for (b, wt) in black.chunks_exact_mut(4).zip(white.chunks_exact(4)) {
            // Over black the result is colour × alpha; over white it's that
            // plus (1 − alpha) × 255.  Green is the least ClearType-tinted.
            let a = 255 - (wt[1] as i32 - b[1] as i32).clamp(0, 255);
            b[3] = a as u8;
            for c in &mut b[..3] {
                *c = (*c as i32).min(a) as u8;
            }
        }
        Some(black)
    }
}

/// Premultiplied → straight alpha, in place.
pub fn unpremultiply(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a == 0 {
            p[0] = 0;
            p[1] = 0;
            p[2] = 0;
        } else {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// A 32-bit top-down DIB section holding `px` (BGRA).
pub unsafe fn argb_bitmap(w: i32, h: i32, px: &[u8]) -> Option<HBITMAP> {
    unsafe {
        let info = dib_info(w, h);
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(bmp);
            return None;
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        Some(bmp)
    }
}

// ============================================================
// Floating cards — per-pixel-alpha layered windows
// ============================================================

/// Room reserved on every side of a floating card for its shadow.
pub const CARD_MARGIN: i32 = 20;
/// The shadow falls a little below the card, the way light from above would.
const SHADOW_DY: f32 = 5.0;
const SHADOW_OPACITY: f32 = 0.55;

/// What a floating card looks like.  Content is drawn by the caller.
pub struct Card {
    pub w: i32,
    pub h: i32,
    pub radius: i32,
    pub fill: u32,
    pub border: u32,
}

/// Signed distance from `(x, y)` to a `w`×`h` rounded rect at the origin;
/// negative inside.
fn sdf_round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (y - h / 2.0).abs() - (h / 2.0 - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Paints a floating card and hands it to `UpdateLayeredWindow`.
///
/// A window region can only clip on whole pixels, so its corners staircase,
/// and `CS_DROPSHADOW` is a hard grey smear on one side.  Per-pixel alpha
/// gets both right: the edge is antialiased against whatever is on screen
/// behind it, and the shadow is a soft falloff all the way round.
///
/// `x`/`y` are where the card itself goes; the window is `CARD_MARGIN` larger
/// on every side.  `draw` gets a DC in card coordinates, already filled.
/// `alpha` fades the whole thing, for fade-in.
pub unsafe fn present_card(
    hwnd: windows::Win32::Foundation::HWND,
    x: i32,
    y: i32,
    card: &Card,
    alpha: u8,
    draw: impl FnOnce(HDC),
) {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
    unsafe {
        let (cw, ch) = (card.w.max(1), card.h.max(1));
        let Some(content) = Dib::new(cw, ch) else {
            return;
        };
        let brush = CreateSolidBrush(COLORREF(card.fill));
        let _ = FillRect(
            content.dc,
            &RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            },
            brush,
        );
        let _ = DeleteObject(brush);
        draw(content.dc);
        let _ = GdiFlush();

        let m = CARD_MARGIN;
        let (w, h) = (cw + 2 * m, ch + 2 * m);
        let Some(out) = Dib::new(w, h) else {
            return;
        };
        let src = std::slice::from_raw_parts(content.bits, (cw * ch * 4) as usize);
        let dst = std::slice::from_raw_parts_mut(out.bits as *mut u8, (w * h * 4) as usize);

        let (fw, fh, r) = (cw as f32, ch as f32, card.radius as f32);
        let blur = (m - 4) as f32;
        // BGR order, matching the DIB's bytes.
        let border = [
            ((card.border >> 16) & 0xFF) as f32,
            ((card.border >> 8) & 0xFF) as f32,
            (card.border & 0xFF) as f32,
        ];
        for py in 0..h {
            for px in 0..w {
                let cx = (px - m) as f32 + 0.5;
                let cy = (py - m) as f32 + 0.5;
                let d = sdf_round_rect(cx, cy, fw, fh, r);
                let cov = (0.5 - d).clamp(0.0, 1.0);

                let ds = sdf_round_rect(cx, cy - SHADOW_DY, fw, fh, r);
                let t = ((ds + 6.0) / (blur + 6.0)).clamp(0.0, 1.0);
                let shadow = SHADOW_OPACITY * (1.0 - t) * (1.0 - t);

                let a = cov + shadow * (1.0 - cov);
                let o = ((py * w + px) * 4) as usize;
                if cov > 0.0 {
                    let sx = (px - m).clamp(0, cw - 1);
                    let sy = (py - m).clamp(0, ch - 1);
                    let s = ((sy * cw + sx) * 4) as usize;
                    // The outermost pixel ring is the border colour.
                    let wb = (d + 1.5).clamp(0.0, 1.0);
                    for c in 0..3 {
                        let v = src[s + c] as f32 * (1.0 - wb) + border[c] * wb;
                        dst[o + c] = (v * cov) as u8;
                    }
                } else {
                    dst[o] = 0;
                    dst[o + 1] = 0;
                    dst[o + 2] = 0;
                }
                dst[o + 3] = (a * 255.0) as u8;
            }
        }

        let screen = GetDC(None);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            screen,
            Some(&POINT { x: x - m, y: y - m }),
            Some(&SIZE { cx: w, cy: h }),
            out.dc,
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen);
    }
}

// ============================================================
// Internals
// ============================================================

/// Outlines a rounded rect with a pen `width` wide.
unsafe fn stroke(hdc: HDC, rc: &RECT, radius: i32, color: u32, width: i32) {
    unsafe {
        let pen = CreatePen(PS_SOLID, width, COLORREF(color));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        // A pen straddles the path, so inset by half its width to keep the
        // stroke inside the shape instead of bleeding past the edge.
        let half = width / 2;
        let _ = RoundRect(
            hdc,
            rc.left + half,
            rc.top + half,
            rc.right - half,
            rc.bottom - half,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

/// A flat fill clipped to the rounded outline.
///
/// The clip region is scoped with `SaveDC`/`RestoreDC` rather than cleared
/// afterwards, so a caller that had its own clip set keeps it.
unsafe fn fill_round(hdc: HDC, rc: &RECT, radius: i32, color: u32) {
    unsafe {
        let saved = SaveDC(hdc);
        let rgn = CreateRoundRectRgn(rc.left, rc.top, rc.right + 1, rc.bottom + 1, radius * 2, radius * 2);
        SelectClipRgn(hdc, rgn);
        let brush = CreateSolidBrush(COLORREF(color));
        let _ = FillRect(hdc, rc, brush);
        let _ = DeleteObject(brush);
        let _ = RestoreDC(hdc, saved);
        let _ = DeleteObject(rgn);
    }
}

/// Box-filters the oversampled canvas down to its final size.
unsafe fn downsample(canvas: &Dib, w: i32, h: i32) -> (Vec<u8>, BITMAPINFO) {
    (unsafe { downsample_raw(canvas, w, h) }, dib_info(w, h))
}

unsafe fn downsample_raw(canvas: &Dib, w: i32, h: i32) -> Vec<u8> {
    let src_stride = (canvas.w * 4) as usize;
    let src = unsafe { std::slice::from_raw_parts(canvas.bits, src_stride * canvas.h as usize) };

    let mut out = vec![0u8; (w * h * 4) as usize];
    let samples = (SS * SS) as u32;

    for y in 0..h as usize {
        for x in 0..w as usize {
            let mut acc = [0u32; 3];
            for sy in 0..SS as usize {
                let row = (y * SS as usize + sy) * src_stride;
                for sx in 0..SS as usize {
                    let p = row + (x * SS as usize + sx) * 4;
                    acc[0] += src[p] as u32;
                    acc[1] += src[p + 1] as u32;
                    acc[2] += src[p + 2] as u32;
                }
            }
            let d = (y * w as usize + x) * 4;
            out[d] = (acc[0] / samples) as u8;
            out[d + 1] = (acc[1] / samples) as u8;
            out[d + 2] = (acc[2] / samples) as u8;
            out[d + 3] = 255;
        }
    }
    out
}

fn dib_info(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A top-down 32-bit DIB section with a DC selected into it, released on drop.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *const u8,
    w: i32,
    h: i32,
}

impl Dib {
    unsafe fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let info = dib_info(w, h);
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if bits.is_null() {
                let _ = DeleteObject(bmp);
                return None;
            }
            let dc = CreateCompatibleDC(None);
            let old = SelectObject(dc, bmp);
            Some(Self {
                dc,
                bmp,
                old,
                bits: bits as *const u8,
                w,
                h,
            })
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}
