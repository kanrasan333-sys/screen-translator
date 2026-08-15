//! "Full page" capture — the part of a document that lives below the fold.
//!
//! Windows has no API for "give me what that window would show if it were
//! taller": the content simply doesn't exist until the app paints it.  So the
//! app has to be driven — scroll, grab the region, scroll again — and the
//! frames stitched into one tall strip.
//!
//! Two things have to be worked out at every step: how to move the document,
//! and how far it actually moved.  The first is [`Driver`], which prefers UI
//! Automation and falls back to the mouse wheel.  The second is always
//! [`find_shift`]: even UI Automation reports a percentage rather than a pixel
//! offset, so the frames themselves remain the authority on where the seam
//! goes — the driver only supplies a strong hint and an honest end-of-document
//! signal.

use crate::i18n;
use crate::uia_scroll;
use anyhow::{Result, anyhow};
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetCursorPos, PostMessageW, SetCursorPos, WM_MOUSEWHEEL, WindowFromPoint,
};

// ============================================================
// Tuning
// ============================================================

/// A region smaller than this can't be matched reliably.  Height is the
/// binding constraint: the matcher only resolves shifts up to `h - min_overlap`,
/// and a single wheel notch already moves ~100 px in a browser, so a short
/// region overshoots its own detection range on the very first step.
const MIN_W: i32 = 64;
const MIN_H: i32 = 200;

/// Pause after each wheel step so smooth-scroll animations land before the
/// frame is grabbed.  Everything below ~200 ms starts catching browsers
/// mid-animation.  The UI Automation path doesn't guess: it polls the scroll
/// position until it stops changing.
const SETTLE_MS: u64 = 260;

/// Safety valves.  An infinite feed never stops producing new content, so the
/// capture has to end on its own terms.
const MAX_STEPS: usize = 120;
const MAX_TOTAL_H: i32 = 20_000;

/// Every row is reduced to this many horizontal buckets before frames are
/// compared.  This has to be fine enough to tell one line of body text from
/// the next: at 24 buckets each one averaged some 40 px of a row, which blurred
/// unrelated lines into the same fingerprint — and a page could then explain
/// itself as "never moved".  At 48 a bucket is a character or two wide.
const ROW_BUCKETS: usize = 48;

/// Per-bucket brightness tolerance (0-255) for calling two rows the same.
/// Loose enough to absorb sub-pixel antialiasing, tight enough that different
/// text doesn't slip through.
const BUCKET_TOL: i32 = 5;

/// Buckets allowed to disagree anyway.  A scrollbar, a hover highlight or a
/// blinking caret occupies one or two of them and must not veto the row.
const MAX_BAD_BUCKETS: usize = 5;

/// Share of the overlapping rows that must line up before a shift is
/// believed.  Sticky headers and footers never move, so even a correct match
/// falls short of 100 %.
const MIN_MATCH_RATIO: f32 = 0.6;

/// Share of rows that must match *exactly* for the exact pass to be trusted
/// on its own.  Most apps scroll by blitting, so their rows come back
/// byte-identical; half of them agreeing is already conclusive.
const EXACT_MIN_RATIO: f32 = 0.5;

/// How alike two consecutive frames must be to count as "nothing happened".
/// Measured on exact row equality, not on fingerprints: "did the screen
/// change" is a question about pixels, and a fingerprint loose enough to
/// survive antialiasing is also loose enough to call two different lines of
/// text the same row.
const STILL_RATIO: f32 = 0.98;

/// One wheel notch, as Windows counts them.
const WHEEL_NOTCH: i32 = 120;

// ============================================================
// Public API
// ============================================================

/// Scroll-captures the screen rectangle `(x, y, w, h)` (virtual-screen
/// coordinates) and returns the stitched strip as top-down BGRA.
pub fn capture(x: i32, y: i32, w: i32, h: i32) -> Result<(Vec<u8>, u32, u32)> {
    if w < MIN_W || h < MIN_H {
        return Err(anyhow!("{}", i18n::t("capture.error.too_small")));
    }

    // Try UI Automation first, then the wheel.  The choice can't be final:
    // plenty of elements report themselves scrollable and then ignore
    // `SetScrollPercent` — panes that merely proxy a child scroller,
    // virtualised lists, custom controls with a half-implemented provider.
    // The only honest test is whether the pixels moved.
    let mut last_err = None;
    for wheel_only in [false, true] {
        let mut driver = Driver::at(x + w / 2, y + h / 2, target_shift(h), wheel_only);
        let was_uia = !driver.needs_pointer();

        // The wheel goes wherever the pointer is, so the pointer has to be
        // moved into the region — which drags hover highlights and tooltips
        // along with it.  UI Automation addresses the element directly and
        // leaves the pointer, and the page, alone.
        let restore = driver.needs_pointer().then(cursor_pos).flatten();
        if restore.is_some() {
            let _ = set_cursor(x + w / 2, y + h / 2);
            thread::sleep(Duration::from_millis(150));
        }

        let result = scroll_and_stitch(x, y, w, h, &mut driver);

        driver.restore();
        if let Some(p) = restore {
            let _ = set_cursor(p.x, p.y);
        }

        match result {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if !was_uia {
                    break; // the wheel was the fallback; there is nothing after it
                }
                println!("[!] UI Automation moved nothing — retrying with the wheel");
                // `restore()` put the document back, so the wheel run starts
                // from where the user left it.
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("{}", i18n::t("capture.error.no_scroll"))))
}

/// How far each step should move the content: 70 % of the largest shift the
/// matcher can resolve, leaving headroom for a page that overshoots.
fn target_shift(h: i32) -> i32 {
    ((h - min_overlap(h)) * 7) / 10
}

// ============================================================
// Scroll drivers
// ============================================================

/// Whatever can move the document one step.
enum Driver {
    /// UI Automation: knows where it is and when it has reached the bottom.
    Uia(uia_scroll::Scroller),
    /// The mouse wheel: works anywhere, knows nothing.
    Wheel(Wheel),
}

/// Wheel state.  Apps disagree wildly about how far one notch scrolls — Chrome
/// moves ~100 px, others three lines of text — so the first step is a single
/// notch spent purely on finding out, and the rest are sized from the answer.
struct Wheel {
    notches: i32,
    calibrated: bool,
    expected: Option<i32>,
    /// Notches sent so far, so the view can be put back.
    spent: i32,
    target: i32,
    /// Where the pointer sits, in screen coordinates.
    at: POINT,
    /// The window that actually owns those pixels.
    target_hwnd: HWND,
}

impl Driver {
    fn at(x: i32, y: i32, target: i32, wheel_only: bool) -> Self {
        match (!wheel_only)
            .then(|| uia_scroll::Scroller::at_point(x, y, target))
            .flatten()
        {
            Some(s) => {
                println!("[*] Scroll driver: UI Automation ({} px/step)", s.step_px());
                Self::Uia(s)
            }
            None => {
                let at = POINT { x, y };
                let target_hwnd = unsafe { WindowFromPoint(at) };
                println!(
                    "[*] Scroll driver: mouse wheel (no scroll pattern here); \
                     window at ({x},{y}) is {:?}",
                    window_class(target_hwnd)
                );
                Self::Wheel(Wheel {
                    notches: 1,
                    calibrated: false,
                    expected: None,
                    spent: 0,
                    target,
                    at,
                    target_hwnd,
                })
            }
        }
    }

    fn needs_pointer(&self) -> bool {
        matches!(self, Self::Wheel(_))
    }

    /// Advances one step.  `false` means the driver knows the document is
    /// already at the bottom; the wheel never knows, and says so by always
    /// returning `true` and letting the frame matcher call it.
    fn step(&mut self) -> bool {
        match self {
            Self::Uia(s) => s.step(),
            Self::Wheel(w) => {
                wheel(-w.notches, w);
                w.spent += w.notches;
                thread::sleep(Duration::from_millis(SETTLE_MS));
                true
            }
        }
    }

    /// Pixels the last step was expected to move the content, as a tie-break
    /// hint for the matcher.
    fn expected(&self) -> Option<i32> {
        match self {
            Self::Uia(s) => Some(s.step_px()),
            Self::Wheel(w) => w.expected,
        }
    }

    /// Told what the matcher actually measured, so the wheel can size its
    /// remaining steps from a real number instead of a guess.
    fn observed(&mut self, dy: i32) {
        if let Self::Wheel(w) = self {
            if w.calibrated {
                w.expected = Some(dy);
            } else {
                w.notches = (w.target / dy.max(1)).clamp(1, 12);
                w.calibrated = true;
                w.expected = Some(dy * w.notches);
            }
        }
    }

    fn restore(&self) {
        match self {
            Self::Uia(s) => s.restore(),
            Self::Wheel(w) => {
                if w.spent > 0 {
                    wheel(w.spent, w);
                }
            }
        }
    }
}

// ============================================================
// Capture loop
// ============================================================

fn scroll_and_stitch(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    driver: &mut Driver,
) -> Result<(Vec<u8>, u32, u32)> {
    let stride = (w * 4) as usize;

    let first = grab(x, y, w, h)?;
    debug_dump(&first, w, h, "frame0");
    let mut prev = Signature::build(&first, w, h);
    let mut out = first;
    let mut out_h = h;
    let mut unmatched = false;

    for step in 0..MAX_STEPS {
        if !driver.step() {
            println!("[*] step {step}: driver reports the end of the document");
            break;
        }

        let frame = grab(x, y, w, h)?;
        debug_dump(&frame, w, h, &format!("frame{}", step + 1));
        let sig = Signature::build(&frame, w, h);

        let shift = find_shift(&prev, &sig, h, driver.expected());
        println!("[*] step {step}: {shift}");

        // No confident match means the app moved in a way we can't account for
        // (a lightbox opened, the page jumped).  Stop with what we have rather
        // than splice content into the wrong place.
        let Some(dy) = shift.dy else {
            unmatched = true;
            break;
        };
        if dy == 0 {
            break; // nothing moved — end of the document
        }

        out.extend_from_slice(&frame[((h - dy) as usize) * stride..]);
        out_h += dy;
        prev = sig;
        driver.observed(dy);

        if out_h >= MAX_TOTAL_H {
            break;
        }
    }

    // Nothing was stitched.  Which of the two reasons it was decides what the
    // user can do about it: a static region is their mistake, an untrackable
    // one usually means the region is too short for this app's scroll step.
    if out_h == h {
        return Err(anyhow!(
            "{}",
            i18n::t(if unmatched {
                "capture.error.no_match"
            } else {
                "capture.error.no_scroll"
            })
        ));
    }
    Ok((out, w as u32, out_h as u32))
}

/// Writes every grabbed frame out when `SCROLL_DEBUG_DIR` is set.
///
/// Kept because it earned its place: when a capture insisted a page had not
/// scrolled, the log said the frames were identical and the dumped frames said
/// they were clean, unscrolled content — which is what pinned the fault on the
/// row fingerprint rather than on the grab or the scroll driver.
fn debug_dump(px: &[u8], w: i32, h: i32, name: &str) {
    if let Ok(dir) = std::env::var("SCROLL_DEBUG_DIR") {
        let path = format!("{dir}\\{name}.png");
        match crate::screenshot::save_png_to_file(px, w as u32, h as u32, &path) {
            Ok(()) => println!("[dbg] wrote {path}"),
            Err(e) => println!("[dbg] {path}: {e}"),
        }
    }
}

/// Rows of overlap the matcher insists on keeping.  Below this a shift is
/// being judged on too little evidence.
fn min_overlap(h: i32) -> i32 {
    (h / 3).max(16)
}

// ============================================================
// Frame matching
// ============================================================

/// Two fingerprints of the same frame, one per row.
///
/// `hashes` is exact — most apps scroll by blitting, so a row that moved comes
/// back byte-identical, and exact equality can tell one line of body text from
/// the next where a tolerant fingerprint cannot.  `rows` is the tolerant one,
/// kept for apps that re-render instead of blitting: sub-pixel text
/// positioning changes every byte of a row that is visually the same.
struct Signature {
    /// `h` rows of [`ROW_BUCKETS`] bytes each.
    rows: Vec<u8>,
    hashes: Vec<u64>,
}

impl Signature {
    fn build(bgra: &[u8], w: i32, h: i32) -> Self {
        let stride = (w * 4) as usize;
        let mut rows = vec![0u8; h as usize * ROW_BUCKETS];
        let mut hashes = Vec::with_capacity(h as usize);

        for y in 0..h as usize {
            let row = &bgra[y * stride..y * stride + stride];
            for b in 0..ROW_BUCKETS {
                let from = b * w as usize / ROW_BUCKETS;
                let to = (((b + 1) * w as usize / ROW_BUCKETS).max(from + 1)).min(w as usize);
                let mut sum = 0u32;
                for px in from..to {
                    let p = px * 4;
                    sum += row[p] as u32 + row[p + 1] as u32 + row[p + 2] as u32;
                }
                rows[y * ROW_BUCKETS + b] = (sum / ((to - from) as u32 * 3)) as u8;
            }
            hashes.push(fnv1a(&bgra[y * stride..(y + 1) * stride]));
        }
        Self { rows, hashes }
    }

    fn row(&self, y: i32) -> &[u8] {
        &self.rows[y as usize * ROW_BUCKETS..][..ROW_BUCKETS]
    }

    fn row_matches(&self, y: i32, other: &Signature, oy: i32) -> bool {
        let a = self.row(y);
        let b = other.row(oy);
        let mut bad = 0;
        for i in 0..ROW_BUCKETS {
            if (a[i] as i32 - b[i] as i32).abs() > BUCKET_TOL {
                bad += 1;
                if bad > MAX_BAD_BUCKETS {
                    return false;
                }
            }
        }
        true
    }
}

/// How far the content moved up between the two frames, in pixels.
///
/// `Some(0)` means the frames look identical — the document is already at the
/// bottom.  `None` means no shift explained the new frame well enough.
///
/// `expected` is the previous step's shift, used only to break ties: a list of
/// visually identical rows matches equally well at every multiple of its row
/// height, and the wheel moves by roughly the same amount every step.
/// What the matcher made of a pair of frames.  The numbers come back with the
/// answer so the caller can log them: when a capture fails on someone else's
/// machine, "no confident match, best 0.41 at 220px" is the difference between
/// a diagnosis and a guess.
struct Shift {
    dy: Option<i32>,
    still: f32,
    best: f32,
    best_dy: i32,
}

impl std::fmt::Display for Shift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "still={:.3} best={:.3}@{}px -> {:?}",
            self.still, self.best, self.best_dy, self.dy
        )
    }
}

fn find_shift(prev: &Signature, curr: &Signature, h: i32, expected: Option<i32>) -> Shift {
    // Two separate questions, and conflating them was the bug.  First: did
    // anything happen at all?  A document already at the bottom hands back a
    // frame whose rows are byte-for-byte what they were.
    let still = ratio_at(prev, curr, h, 0, Match::Exact);
    if still >= STILL_RATIO {
        return Shift {
            dy: Some(0),
            still,
            best: still,
            best_dy: 0,
        };
    }

    // Something did happen — so zero is no longer a candidate.  It used to be,
    // and on a page of evenly-set body text it would win: every row resembled
    // every other row closely enough that "unchanged" scored as well as the
    // true shift, and the tie-break favoured the smaller number.  The capture
    // then stopped on its first step insisting the page had not scrolled.
    let exact = search(prev, curr, h, expected, Match::Exact);
    if exact.best >= EXACT_MIN_RATIO {
        return Shift { still, ..exact };
    }

    // The app re-renders rather than blits.  Fall back to the fingerprint that
    // forgives a pixel here and there.
    let fuzzy = search(prev, curr, h, expected, Match::Fuzzy);
    Shift { still, ..fuzzy }
}

/// How rows are compared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Match {
    Exact,
    Fuzzy,
}

/// Scores every candidate shift and picks one.
fn search(prev: &Signature, curr: &Signature, h: i32, expected: Option<i32>, how: Match) -> Shift {
    let max_dy = h - min_overlap(h);
    let mut ratios = Vec::with_capacity(max_dy as usize);
    for dy in 1..=max_dy {
        ratios.push(ratio_at(prev, curr, h, dy, how));
    }

    let best = ratios.iter().copied().fold(0.0f32, f32::max);
    let best_dy = ratios
        .iter()
        .position(|r| *r == best)
        .map_or(0, |i| i as i32 + 1);

    let threshold = if how == Match::Exact {
        EXACT_MIN_RATIO
    } else {
        MIN_MATCH_RATIO
    };
    if best < threshold {
        return Shift {
            dy: None,
            still: 0.0,
            best,
            best_dy,
        };
    }

    // Anything within a whisker of the best score is as good an explanation.
    // Pick the one closest to what the last step did; with nothing to go on,
    // pick the smallest shift — under-shooting stops the capture early, while
    // over-shooting silently drops a band of content.
    let target = expected.unwrap_or(1);
    let dy = ratios
        .iter()
        .enumerate()
        .filter(|(_, r)| **r >= best - 0.005)
        .map(|(i, _)| i as i32 + 1)
        .min_by_key(|dy| (dy - target).abs());

    Shift {
        dy,
        still: 0.0,
        best,
        best_dy,
    }
}

/// Share of the overlapping rows that line up when `curr` is read as `prev`
/// shifted up by `dy` pixels.
fn ratio_at(prev: &Signature, curr: &Signature, h: i32, dy: i32, how: Match) -> f32 {
    let overlap = h - dy;
    let mut hits = 0;
    for r in 0..overlap {
        let same = match how {
            Match::Exact => curr.hashes[r as usize] == prev.hashes[(r + dy) as usize],
            Match::Fuzzy => curr.row_matches(r, prev, r + dy),
        };
        if same {
            hits += 1;
        }
    }
    hits as f32 / overlap as f32
}

/// FNV-1a over a row's raw bytes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

// ============================================================
// Win32 plumbing
// ============================================================

/// Grabs a screen rectangle as top-down BGRA with alpha forced opaque.
fn grab(x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>> {
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(anyhow!("GetDC failed"));
        }

        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);
        let blitted = BitBlt(mem_dc, 0, 0, w, h, screen_dc, x, y, SRCCOPY).is_ok();
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
        let lines = GetDIBits(
            screen_dc,
            bmp,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        if !blitted || lines == 0 {
            return Err(anyhow!("Screen grab failed"));
        }

        for chunk in pixels.chunks_exact_mut(4) {
            chunk[3] = 255;
        }
        Ok(pixels)
    }
}

/// Rolls the wheel.  Positive scrolls up.
///
/// Two ways, because either one alone has a hole.  Injected input is what a
/// real wheel produces, but Windows only routes it to the window under the
/// pointer when "scroll inactive windows when I hover over them" is on —
/// switch that off and it goes to whatever has focus, which after the capture
/// overlay closes may be nothing at all.  A message posted straight at the
/// window that owns those pixels doesn't care about the setting or about
/// focus.  Sending both is harmless: the frame matcher measures whatever
/// actually happened, and the wheel calibrates its step from that.
fn wheel(notches: i32, w: &Wheel) {
    // `WM_MOUSEWHEEL` carries its delta in a signed 16-bit field, so putting
    // the view back after a long capture has to go out in pieces.
    const MAX_PER_MSG: i32 = 200; // 200 notches = 24000, comfortably inside i16
    let mut left = notches;
    while left != 0 {
        let chunk = left.clamp(-MAX_PER_MSG, MAX_PER_MSG);
        send_wheel(chunk, w);
        left -= chunk;
    }
}

fn send_wheel(notches: i32, w: &Wheel) {
    unsafe {
        let delta = notches * WHEEL_NOTCH;

        let mut input = INPUT {
            r#type: INPUT_MOUSE,
            ..Default::default()
        };
        input.Anonymous.mi = MOUSEINPUT {
            mouseData: delta as u32,
            dwFlags: MOUSEEVENTF_WHEEL,
            ..Default::default()
        };
        let _ = SendInput(&[input], size_of::<INPUT>() as i32);

        if w.target_hwnd.0.is_null() {
            return;
        }
        // Delta in the high word of wParam; screen coordinates in lParam, one
        // 16-bit field each — masked, because a monitor left of or above the
        // primary one makes them negative.
        let wp = WPARAM(((delta as i16 as u16) as usize) << 16);
        let lp = LPARAM(((w.at.y & 0xFFFF) << 16 | (w.at.x & 0xFFFF)) as isize);
        if PostMessageW(w.target_hwnd, WM_MOUSEWHEEL, wp, lp).is_err() {
            // ERROR_ACCESS_DENIED here means UIPI blocked us: the window
            // belongs to a process at a higher integrity level, and nothing
            // this process sends will ever reach it.
            println!(
                "[!] WM_MOUSEWHEEL to {:?} rejected: {}",
                window_class(w.target_hwnd),
                windows::core::Error::from_win32()
            );
        }
    }
}

fn window_class(hwnd: HWND) -> String {
    if hwnd.0.is_null() {
        return "<none>".into();
    }
    let mut buf = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn cursor_pos() -> Option<POINT> {
    unsafe {
        let mut p = POINT::default();
        GetCursorPos(&mut p).ok()?;
        Some(p)
    }
}

fn set_cursor(x: i32, y: i32) -> Result<()> {
    unsafe { SetCursorPos(x, y).map_err(|e| anyhow!("SetCursorPos: {e}")) }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 480;
    const LINE_H: i32 = 20;

    /// A page of pseudo text: every line carries the same ink density and the
    /// same rhythm of word-shaped runs, differing only in where the runs fall.
    /// That is the case that used to defeat the matcher — lines that look
    /// alike at a glance but are not the same line.
    fn text_page(rows: i32) -> Vec<u8> {
        let mut buf = vec![250u8; (W * rows * 4) as usize];
        for y in 0..rows {
            let line = y / LINE_H;
            if !(4..16).contains(&(y % LINE_H)) {
                continue;
            }
            let mut seed = (line as u32).wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
            let mut x = 2;
            while x < W - 10 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let run = 4 + (seed >> 13) as i32 % 6;
                let gap = 3 + (seed >> 19) as i32 % 3;
                for px in x..(x + run).min(W) {
                    let p = ((y * W + px) * 4) as usize;
                    buf[p] = 40;
                    buf[p + 1] = 40;
                    buf[p + 2] = 40;
                }
                x += run + gap;
            }
        }
        buf
    }

    fn window(page: &[u8], top: i32, h: i32) -> Vec<u8> {
        let stride = (W * 4) as usize;
        page[top as usize * stride..(top + h) as usize * stride].to_vec()
    }

    fn shift_between(a: &[u8], b: &[u8], h: i32, expected: Option<i32>) -> Option<i32> {
        let prev = Signature::build(a, W, h);
        let curr = Signature::build(b, W, h);
        find_shift(&prev, &curr, h, expected).dy
    }

    #[test]
    fn identical_frames_mean_the_document_stopped() {
        let page = text_page(600);
        let frame = window(&page, 0, 400);
        assert_eq!(shift_between(&frame, &frame, 400, None), Some(0));
    }

    #[test]
    fn finds_the_shift_without_a_hint() {
        let page = text_page(900);
        let (h, s) = (400, 137);
        let a = window(&page, 0, h);
        let b = window(&page, s, h);
        assert_eq!(shift_between(&a, &b, h, None), Some(s));
    }

    /// The regression: with the frames plainly different, zero is not an
    /// answer.  It used to win on a page of evenly-set text, and the capture
    /// gave up on its first step claiming nothing had scrolled.
    fn assert_moved(a: &[u8], b: &[u8], h: i32, hint: Option<i32>) -> i32 {
        let dy = shift_between(a, b, h, hint).expect("frames differ, so a shift must be found");
        assert_ne!(dy, 0, "a moved page must never report zero movement");
        dy
    }

    #[test]
    fn never_reports_zero_when_the_page_moved() {
        let page = text_page(900);
        let h = 400;
        for s in [23, 60, 137, 200, 260] {
            let a = window(&page, 0, h);
            let b = window(&page, s, h);
            assert_eq!(assert_moved(&a, &b, h, None), s, "shift of {s}px");
        }
    }

    /// A page whose lines are word-for-word identical apart from a line number
    /// a few pixels wide — a log, a table, a numbered listing.  This is the
    /// case the tolerant fingerprint could not see: the number occupies three
    /// buckets of forty-eight, well inside the budget for buckets allowed to
    /// disagree, so every row read as every other row and the capture stopped
    /// on its first step insisting the page had never moved.
    fn numbered_page(rows: i32) -> Vec<u8> {
        let mut buf = vec![250u8; (W * rows * 4) as usize];
        for y in 0..rows {
            let line = y / LINE_H;
            if !(4..16).contains(&(y % LINE_H)) {
                continue;
            }
            let ink = |buf: &mut Vec<u8>, x: i32| {
                let p = ((y * W + x) * 4) as usize;
                buf[p] = 40;
                buf[p + 1] = 40;
                buf[p + 2] = 40;
            };
            // The line number: four digits' worth of pixels in the left margin,
            // one bit each, so consecutive lines are always marked differently.
            for d in 0..4 {
                if (line >> d) & 1 == 1 {
                    for x in (4 + d * 7)..(9 + d * 7) {
                        ink(&mut buf, x);
                    }
                }
            }
            // Body text: identical on every line at the same height, but
            // different from one row to the next within a line — the top of a
            // letter is not its middle, and a generator that ignores that
            // makes any small shift look like a perfect match.
            let mut x = 40 + (y % LINE_H) % 11;
            while x < W - 12 {
                for px in x..x + 7 {
                    ink(&mut buf, px);
                }
                x += 11;
            }
        }
        buf
    }

    #[test]
    fn tells_apart_rows_that_differ_only_in_a_line_number() {
        let page = numbered_page(900);
        let h = 400;
        // Shifts stay inside what the matcher can resolve — `target_shift`
        // sizes the driver's step to 70 % of exactly that.
        for s in [40, 137, 186, target_shift(h)] {
            let a = window(&page, 0, h);
            let b = window(&page, s, h);
            assert_eq!(assert_moved(&a, &b, h, None), s, "shift of {s}px");
        }
    }

    #[test]
    fn a_sticky_header_does_not_derail_the_match() {
        let page = text_page(900);
        let (h, s, header) = (400, 150, 56);
        let a = window(&page, 0, h);
        let mut b = window(&page, s, h);
        // Freeze the top rows the way a pinned toolbar would.
        let stride = (W * 4) as usize;
        b[..header as usize * stride].copy_from_slice(&a[..header as usize * stride]);
        assert_eq!(assert_moved(&a, &b, h, None), s);
    }
}
