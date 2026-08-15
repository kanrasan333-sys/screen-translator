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

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::*;

/// Oversampling factor.
const SS: i32 = 4;

/// How a rounded rectangle is drawn.
#[derive(Clone, Copy)]
pub struct Style {
    pub radius: i32,
    /// Vertical gradient endpoints.  Equal values give a flat fill.
    pub fill: (u32, u32),
    pub border: Option<u32>,
    /// Soft edge below, drawn into the bottom pixel row of the rect.
    pub shadow: Option<u32>,
    /// Border thickness in logical pixels.
    pub border_width: i32,
}

impl Style {
    /// A flat fill with no border, highlight or shadow.
    pub fn flat(radius: i32, fill: u32) -> Self {
        Self {
            radius,
            fill: (fill, fill),
            border: None,
            shadow: None,
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

    pub fn gradient(mut self, top: u32, bottom: u32) -> Self {
        self.fill = (top, bottom);
        self
    }

    pub fn shadow(mut self, color: u32) -> Self {
        self.shadow = Some(color);
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
pub unsafe fn round_rect(hdc: HDC, rc: &RECT, style: &Style) {
    unsafe {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        supersampled(hdc, rc, |dc, ss| {
            // When there's a shadow the body gives up its bottom pixel row to
            // it, so nothing is ever drawn outside the rect we were handed —
            // in `WM_DRAWITEM` the DC is clipped to the control and a shadow
            // past its edge would simply vanish.
            let (sw, sh) = (w * ss, h * ss);
            let body = RECT {
                left: 0,
                top: 0,
                right: sw,
                bottom: if style.shadow.is_some() { sh - ss } else { sh },
            };
            let r = style.radius * ss;

            if let Some(shadow) = style.shadow {
                stroke(dc, &body, r, shadow, ss, ss);
            }

            fill_gradient(dc, &body, r, style.fill.0, style.fill.1);

            if let Some(border) = style.border {
                stroke(dc, &body, r, border, style.border_width * ss, 0);
            }
        });
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

// ============================================================
// Internals
// ============================================================

/// Outlines a rounded rect with a pen `width` wide, shifted down by `dy`.
unsafe fn stroke(hdc: HDC, rc: &RECT, radius: i32, color: u32, width: i32, dy: i32) {
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
            rc.top + dy + half,
            rc.right - half,
            rc.bottom + dy - half,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

/// Vertical gradient clipped to the rounded outline.
///
/// The clip region is scoped with `SaveDC`/`RestoreDC` rather than cleared
/// afterwards, so a caller that had its own clip set keeps it.
unsafe fn fill_gradient(hdc: HDC, rc: &RECT, radius: i32, top: u32, bottom: u32) {
    unsafe {
        let saved = SaveDC(hdc);
        let rgn = CreateRoundRectRgn(rc.left, rc.top, rc.right, rc.bottom, radius * 2, radius * 2);
        SelectClipRgn(hdc, rgn);

        let verts = [
            vertex(rc.left, rc.top, top),
            vertex(rc.right, rc.bottom, bottom),
        ];
        let mesh = GRADIENT_RECT {
            UpperLeft: 0,
            LowerRight: 1,
        };
        let _ = GradientFill(
            hdc,
            &verts,
            &mesh as *const _ as *const core::ffi::c_void,
            1,
            GRADIENT_FILL_RECT_V,
        );

        let _ = RestoreDC(hdc, saved);
        let _ = DeleteObject(rgn);
    }
}

/// Box-filters the oversampled canvas down to its final size.
unsafe fn downsample(canvas: &Dib, w: i32, h: i32) -> (Vec<u8>, BITMAPINFO) {
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
    (out, dib_info(w, h))
}

/// COLORREF packs 8-bit channels; `TRIVERTEX` wants them 16-bit.
fn vertex(x: i32, y: i32, c: u32) -> TRIVERTEX {
    TRIVERTEX {
        x,
        y,
        Red: ((c & 0xFF) << 8) as u16,
        Green: (((c >> 8) & 0xFF) << 8) as u16,
        Blue: (((c >> 16) & 0xFF) << 8) as u16,
        Alpha: 0,
    }
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
