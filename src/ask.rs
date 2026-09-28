//! "Ask the model" — a single input line, summoned where you are looking.
//!
//! The whole window is one field until there is something to show: type,
//! press Enter, and the answer unfolds underneath while the input stays
//! exactly where it was.  No caption, no buttons, no chrome — it is summoned
//! by a hotkey and dismissed with Escape or the same hotkey, so anything else
//! is furniture.
//!
//! Summoned over a selection it opens beside it rather than in the middle of
//! the screen, so the text the question is about stays where the eye left it.
//! With nothing selected there is nothing to sit beside and it falls back to
//! the centre.  Either way it can be dragged anywhere by the panel around the
//! fields — there is no caption to grab, so the padding is the caption.
//!
//! Clicking into another window leaves it alone.  It is topmost and it is a
//! place to keep an answer, and the useful thing to do with an answer on
//! screen is to work in the window underneath it.
//!
//! The height follows the answer: it is measured in wrapped lines and clamped,
//! so a one-word reply doesn't leave a half-empty panel hanging on screen and
//! a long one scrolls instead of running off the bottom.
//!
//! It has eyes, too: paste a picture and the question is asked about that.  The
//! picture goes to Gemini as an image, so the answer is about what is actually
//! *drawn*, not just the text on it.  The same model searches the web when a
//! question needs it, and any sources it leaned on are listed under the answer.
//!
//! The conversation lives only as long as the window does.  Reopening starts
//! clean, which is what you want from something bound to a hotkey.

use crate::gemini;
use crate::i18n;
use crate::paint;
use crate::settings;
use crate::theme;
use crate::utils::to_wide;
use crate::websearch;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{SetWindowTheme, ShowScrollBar};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL,
    VK_SHIFT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ============================================================
// Win32 constants the windows crate doesn't surface
// ============================================================

const ES_MULTILINE: WINDOW_STYLE = WINDOW_STYLE(0x0004);
const ES_AUTOVSCROLL: WINDOW_STYLE = WINDOW_STYLE(0x0040);
const ES_READONLY: WINDOW_STYLE = WINDOW_STYLE(0x0800);

const EM_SETMARGINS: u32 = 0x00D3;
const EM_LIMITTEXT: u32 = 0x00C5;
const EM_SETSEL: u32 = 0x00B1;
const EM_SCROLLCARET: u32 = 0x00B7;
const EM_GETLINECOUNT: u32 = 0x00BA;

/// `VK_A`, `VK_CONTROL` — the windows crate exposes these, but only the two
/// are needed here and importing them by name reads worse than the codes do
/// alongside the `0x0D` / `0x1B` already used below.
const VK_A: usize = 0x41;

/// Tallest rectangle still believable as a text caret.  Some controls leave a
/// stale one behind, and a "caret" the height of a pane is one of those.
const MAX_CARET_H: i32 = 80;

/// DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
const DT_LINE: u32 = 0x0824;

const IDC_ANSWER: i32 = 401;
const IDC_QUESTION: i32 = 402;

/// Posted by the worker thread once a reply (or an error) is waiting.
const WM_APP_REPLY: u32 = WM_APP + 11;

/// Posted when a copied selection turns up after the window was already shown.
const WM_APP_PREFILL: u32 = WM_APP + 12;

/// Posted by the file picker with the chosen paths (a boxed `Vec<String>`).
const WM_APP_ATTACH_FILE: u32 = WM_APP + 13;

/// Posted by the build worker once it has read and decoded those files (a boxed
/// `AttachResult`), ready to drop into the input.
const WM_APP_ATTACH_READY: u32 = WM_APP + 14;

/// `WM_MOUSELEAVE` — the windows crate's glob doesn't surface it, and matching a
/// bare unknown name would silently become a catch-all binding.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Drives the unfold.  `WM_TIMER` is floored by the system timer resolution —
/// asking for 8 ms measures out at about 15, so this runs near 60 fps whatever
/// is requested.  Raising the global timer resolution to close that gap costs
/// every other process on the machine battery for a 200 ms movement.
const ANIM_TIMER: usize = 1;
const ANIM_TICK_MS: u32 = 8;

/// Ticks the seconds counter beside "Thinking…".
///
/// A wait with no sign of progress is indistinguishable from a hang — which is
/// exactly how a slow network read of this window once looked.  A number that
/// climbs says the window is alive and lets the user judge whether to wait.
const THINK_TIMER: usize = 2;
const THINK_TICK_MS: u32 = 1000;

/// Share of the remaining distance covered each tick.  Exponential ease-out:
/// the panel leaves quickly and settles, which reads as movement rather than
/// as a window being resized.
const ANIM_EASE: f32 = 0.34;

// ============================================================
// Layout
// ============================================================

mod layout {
    /// Wide enough for a sentence of answer without the eye tracking back,
    /// narrow enough to still read as a prompt rather than a document — and
    /// to stand beside a selection instead of on top of the page it came from.
    pub const WIN_W: i32 = 560;
    pub const PAD_X: i32 = 16;

    /// The attached picture, and the air between it and the input line.  A
    /// thumbnail is the whole of the attachment UI: it says what is attached
    /// better than a filename would, and clicking it takes it back off.
    pub const THUMB: i32 = 34;
    pub const THUMB_GAP: i32 = 10;

    /// The attach-a-file button in the top-right of the header, and the air
    /// between it and the input line.  Always present, so the input reserves
    /// room for it whether or not anything is attached.
    pub const CLIP: i32 = 24;
    pub const CLIP_GAP: i32 = 8;

    /// The web-search toggle, sitting to the left of the attach button.  Same
    /// square as its neighbour so the two read as one pair of controls.
    pub const GLOBE: i32 = 24;
    pub const GLOBE_GAP: i32 = 6;

    /// The input line and the air around it — the whole window until an
    /// answer arrives.
    ///
    /// The control is cut to one line of its own font with a pixel to spare,
    /// and everything left over goes to the padding rather than inside the
    /// control: the field has no visible box, so the two look identical, but
    /// the padding is what there is to drag the window by.
    pub const INPUT_H: i32 = 24;
    pub const PAD_Y: i32 = 14;

    /// How tall the input is allowed to grow before it scrolls instead.  Past
    /// this the window is more text box than prompt, and the answer below it
    /// would be squeezed off the screen.
    pub const INPUT_MAX_H: i32 = 24 * 6;

    /// Height of everything above the hairline.  Both a chip and a question run
    /// to several lines are taller than one line of text, so the header takes
    /// whichever is bigger and shrinks again when it can.
    pub const fn head_h_of(shot: bool, input_h: i32) -> i32 {
        let content = if shot && THUMB > input_h {
            THUMB
        } else {
            input_h
        };
        content + PAD_Y * 2
    }

    /// The header as it currently stands — measured from the live input.
    pub fn head_h(shot: bool) -> i32 {
        head_h_of(shot, super::input_h())
    }

    /// The card between the input and the answer text: its inner padding, the
    /// height of each part, and the air under it.
    pub const CARD_PAD: i32 = 14;
    pub const CARD_TITLE_H: i32 = 16;
    pub const CARD_VALUE_H: i32 = 34;
    pub const CARD_SUB_H: i32 = 18;
    pub const CARD_ROW_H: i32 = 20;
    pub const CARD_GAP: i32 = 10;
    pub const CARD_MAX_ROWS: usize = 5;

    /// How tall the card is for what it holds — nothing at all when there is no
    /// card, so the pane closes up rather than leaving a gap.
    pub fn card_h(card: Option<(bool, bool, usize)>) -> i32 {
        let Some((has_title, has_sub, rows)) = card else {
            return 0;
        };
        let mut h = CARD_PAD * 2 + CARD_VALUE_H;
        if has_title {
            h += CARD_TITLE_H;
        }
        if has_sub {
            h += CARD_SUB_H;
        }
        if rows > 0 {
            // A hairline above the rows, then the rows themselves.
            h += 9 + rows as i32 * CARD_ROW_H;
        }
        h + CARD_GAP
    }

    /// Hairline under the input, then the card, then the answer.
    pub fn answer_top(shot: bool) -> i32 {
        head_h(shot) + 1 + 9 + super::live_card_h()
    }
    pub const ANSWER_MIN_H: i32 = 22;
    pub const ANSWER_MAX_H: i32 = 340;

    pub const HINT_GAP: i32 = 8;
    pub const HINT_H: i32 = 14;
    pub const BOTTOM: i32 = 11;

    /// Where the collapsed box sits vertically when there is no selection to
    /// sit beside, as a fraction of the work area.  A third of the way down
    /// rather than halfway: dead centre reads as low once the answer unfolds
    /// beneath it, and it is where every summoned launcher has sat since
    /// Spotlight.
    pub const ANCHOR_NUM: i32 = 1;
    pub const ANCHOR_DEN: i32 = 3;

    /// Air between the selection and the window that opens beside it, and the
    /// least the window will leave between itself and the edge of the screen.
    pub const ANCHOR_GAP: i32 = 10;
    pub const EDGE: i32 = 8;

    /// Total window height for an answer pane `answer_h` tall.
    pub fn expanded(shot: bool, answer_h: i32) -> i32 {
        answer_top(shot) + answer_h + HINT_GAP + HINT_H + BOTTOM
    }
}

/// Height the input line currently wants: one line per wrapped line of the
/// question, up to a ceiling past which it scrolls.
///
/// Measured from the control itself, which has already wrapped the text at its
/// real width — counting characters here would only ever be a guess.
static INPUT_H: Mutex<i32> = Mutex::new(layout::INPUT_H);

fn input_h() -> i32 {
    *INPUT_H.lock().unwrap()
}

/// Height the current card needs, measured from the card itself.
fn live_card_h() -> i32 {
    let g = CARD.lock().unwrap();
    layout::card_h(g.as_ref().map(|c| {
        (
            !c.title.is_empty(),
            !c.subtitle.is_empty(),
            c.rows.len(),
        )
    }))
}

/// Where the card sits, when there is one.
fn card_rect(shot: bool) -> RECT {
    use layout::*;
    let top = head_h(shot) + 1 + 9;
    RECT {
        left: PAD_X,
        top,
        right: WIN_W - PAD_X,
        bottom: top + (live_card_h() - CARD_GAP).max(0),
    }
}

/// Remeasures the input and returns whether its height changed.
unsafe fn measure_input(hwnd: HWND) -> bool {
    unsafe {
        let Ok(edit) = GetDlgItem(hwnd, IDC_QUESTION) else {
            return false;
        };
        let lines = SendMessageW(edit, EM_GETLINECOUNT, WPARAM(0), LPARAM(0)).0 as i32;
        let line_h = res().as_ref().map(|r| r.line_input_h).unwrap_or(20);
        let want = (lines.max(1) * line_h)
            .max(layout::INPUT_H)
            .min(layout::INPUT_MAX_H);

        let mut cur = INPUT_H.lock().unwrap();
        if *cur == want {
            return false;
        }
        *cur = want;
        true
    }
}

fn input_rect(count: usize) -> RECT {
    use layout::*;
    let h = input_h();
    // Centred on the header rather than pinned to the top of it, so a one-line
    // question sits level with the middle of any chips beside it.
    let top = (head_h_of(count > 0, h) - h) / 2;
    RECT {
        // Start after however many chips are lined up on the left.
        left: PAD_X + count as i32 * (THUMB + THUMB_GAP),
        top,
        // Both buttons sit to the right of the input, so the line stops short
        // of them rather than running underneath.
        right: WIN_W - PAD_X - CLIP - CLIP_GAP - GLOBE - GLOBE_GAP,
        bottom: top + h,
    }
}

/// The web-search toggle, immediately left of the attach button.
fn globe_rect(shot: bool) -> RECT {
    use layout::*;
    let clip = clip_rect(shot);
    let top = (head_h(shot) - GLOBE) / 2;
    RECT {
        left: clip.left - GLOBE_GAP - GLOBE,
        top,
        right: clip.left - GLOBE_GAP,
        bottom: top + GLOBE,
    }
}

/// The attach-a-file button: top-right of the header, vertically centred on
/// whatever the header currently holds.
fn clip_rect(shot: bool) -> RECT {
    use layout::*;
    let top = (head_h(shot) - CLIP) / 2;
    RECT {
        left: WIN_W - PAD_X - CLIP,
        top,
        right: WIN_W - PAD_X,
        bottom: top + CLIP,
    }
}

/// Where the thumbnail sits.  Meaningless unless something is attached.
/// The box for attachment chip `i`, laid out left to right across the header.
fn chip_rect(i: usize) -> RECT {
    use layout::*;
    let left = PAD_X + i as i32 * (THUMB + THUMB_GAP);
    RECT {
        left,
        top: PAD_Y,
        right: left + THUMB,
        bottom: PAD_Y + THUMB,
    }
}

/// Which attachment chip, if any, the point falls on.
fn chip_at(x: i32, y: i32) -> Option<usize> {
    (0..attach_count()).find(|&i| in_rect(x, y, chip_rect(i)))
}

fn answer_rect(shot: bool, answer_h: i32) -> RECT {
    use layout::*;
    RECT {
        left: PAD_X,
        top: answer_top(shot),
        right: WIN_W - PAD_X,
        bottom: answer_top(shot) + answer_h,
    }
}

// ============================================================
// Window state
// ============================================================

/// Who said a line of the transcript.
#[derive(Clone, Copy, PartialEq)]
enum Who {
    User,
    Model,
}

/// One turn of the conversation.
struct Msg {
    who: Who,
    /// What the transcript shows — typed, or answered.
    text: String,
    /// A marker shown before a user turn that carried a file — an i18n key, so
    /// once the chip is gone the transcript still says which question was about
    /// what.  `None` for a plain text turn.
    tag: Option<&'static str>,
    /// The attached files, prepared for sending.  Kept per turn because the API
    /// is stateless: every follow-up resends the whole conversation, files
    /// included, and dropping them after the first question would leave the model
    /// answering about something it can no longer see.
    attach: Vec<gemini::Attach>,
}

/// A picture waiting in the input, unsent.  Raw pixels rather than PNG: the
/// thumbnail is drawn from them, and the encode is worth doing once, on the
/// worker thread, rather than on every repaint.
#[derive(Clone)]
struct Shot {
    bgra: Vec<u8>,
    w: u32,
    h: u32,
}

/// Something waiting in the input to be sent with the next question.
#[derive(Clone)]
enum Pending {
    /// An image — kept as pixels so it can be shown as a thumbnail, and encoded
    /// to PNG only at send time.
    Image(Shot),
    /// Any other file — sent as-is with its own MIME type, and shown as a chip
    /// labelled with its extension rather than a preview.
    File {
        data: Arc<Vec<u8>>,
        mime: String,
        ext: String,
        /// The file's own name, for the Files API to label an upload with.
        name: String,
    },
}

/// What kind of thing is attached, for the bits of the UI that only need to
/// tell the two apart (placeholder text, the header chip, the transcript tag).
enum AttachKind {
    None,
    Image,
    File,
}

/// The running conversation.  The system turn is prepended at send time rather
/// than stored, so a language change between questions takes effect.
static HISTORY: Mutex<Vec<Msg>> = Mutex::new(Vec::new());

/// Files waiting in the input, in the order they were added — several can ride
/// one question, so this is a list rather than a single slot.
static PENDING_ATTACH: Mutex<Vec<Pending>> = Mutex::new(Vec::new());

/// Whether anything is waiting in the input.  The layout asks constantly — the
/// header is a different height with an attachment.
fn attached() -> bool {
    !PENDING_ATTACH.lock().unwrap().is_empty()
}

fn attach_count() -> usize {
    PENDING_ATTACH.lock().unwrap().len()
}

/// The gist of what's attached, for the bits that only need to tell images from
/// the rest: nothing, all images, or a set that includes a non-image file.
fn attach_kind() -> AttachKind {
    let g = PENDING_ATTACH.lock().unwrap();
    if g.is_empty() {
        AttachKind::None
    } else if g.iter().all(|p| matches!(p, Pending::Image(_))) {
        AttachKind::Image
    } else {
        AttachKind::File
    }
}

/// The biggest file this window will take on.  Anything over
/// `gemini::INLINE_LIMIT` goes up through the Files API instead of riding in the
/// request, so the ceiling here is about what can sit in memory comfortably
/// while it is read and sent, not what the request can carry.
const MAX_ATTACH: usize = 200 * 1024 * 1024;

/// The biggest image file worth opening.  Images are decoded to raw pixels —
/// four bytes each — so the file being small says little about what it costs in
/// memory, and this is the honest ceiling.
const MAX_IMAGE_FILE: usize = 50 * 1024 * 1024;

/// How many chips fit across the header before the input line gets too narrow.
const MAX_CHIPS: usize = 6;

/// How long to wait on the model before giving up.
///
/// This window is summoned by a hotkey and answered in seconds; two minutes of
/// "Thinking…" is indistinguishable from a hang, and by then the user has long
/// since given up on the answer anyway.  Better to fail early and say why.
const MODEL_TIMEOUT: u64 = 45;

/// Reply (or error text) handed over by the worker thread.
static PENDING: Mutex<Option<Result<String, String>>> = Mutex::new(None);

/// Set while a request is in flight, so a second Enter doesn't stack calls.
static BUSY: Mutex<bool> = Mutex::new(false);

/// When the question in flight was sent, for the counter beside "Thinking…".
static ASKED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// "Thinking…" plus however many seconds it has been, once it has been long
/// enough to be worth saying.  Under two seconds the number is just noise.
fn thinking_line() -> String {
    let base = i18n::t("ask.thinking");
    match *ASKED_AT.lock().unwrap() {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s >= 2 {
                format!("{base}  {s} s")
            } else {
                base.to_string()
            }
        }
        None => base.to_string(),
    }
}

/// A selection that finished copying only after the window was up.
static PENDING_PREFILL: Mutex<Option<String>> = Mutex::new(None);

/// Where the selection was when the hotkey was pressed.  Kept for exactly that
/// late copy: by the time it lands the foreground window is this one, and this
/// window's own caret is not what the question is about.
static ANCHOR: Mutex<RECT> = Mutex::new(RECT {
    left: 0,
    top: 0,
    right: 0,
    bottom: 0,
});

/// Height the window is currently travelling towards, while the unfold runs.
static ANIM_TARGET: Mutex<i32> = Mutex::new(0);

static ASK_HWND: Mutex<isize> = Mutex::new(0);

/// Whether the pointer is currently over the attach button, so it can light up
/// and read as clickable.
static CLIP_HOVER: Mutex<bool> = Mutex::new(false);

/// The same for the web-search toggle beside it.
static GLOBE_HOVER: Mutex<bool> = Mutex::new(false);

/// Whether web search is on, mirrored from the settings so the paint code
/// doesn't take a settings lock on every repaint.
fn web_on() -> bool {
    settings::current().web_search
}

struct Resources {
    panel_brush: isize,
    font_input: isize,
    font_body: isize,
    font_hint: isize,
    /// Height of one wrapped line of `font_body`, measured once.  The window
    /// grows in multiples of this.
    line_h: i32,
    /// The same for the larger input font, so the question line grows by whole
    /// lines of its own text.
    line_input_h: i32,
    /// The card's headline figure — the one thing on screen meant to be read
    /// from across the desk.
    font_value: isize,
}

impl Resources {
    fn new() -> Self {
        unsafe {
            let font_body = make_font(-13, 400);
            let font_input = make_font(-17, 400);
            Self {
                panel_brush: CreateSolidBrush(COLORREF(theme::CLR_CARD)).0 as isize,
                font_input: font_input.0 as isize,
                font_body: font_body.0 as isize,
                font_hint: make_font(-11, 400).0 as isize,
                line_h: line_height(font_body),
                line_input_h: line_height(font_input),
                font_value: make_font(-27, 600).0 as isize,
            }
        }
    }
    fn font_value(&self) -> HFONT {
        HFONT(self.font_value as *mut _)
    }
    fn panel_brush(&self) -> HBRUSH {
        HBRUSH(self.panel_brush as *mut _)
    }
    fn font_input(&self) -> HFONT {
        HFONT(self.font_input as *mut _)
    }
    fn font_body(&self) -> HFONT {
        HFONT(self.font_body as *mut _)
    }
    fn font_hint(&self) -> HFONT {
        HFONT(self.font_hint as *mut _)
    }
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1,
            0,
            0,
            5,
            0,
            w!("Segoe UI"),
        )
    }
}

unsafe fn line_height(font: HFONT) -> i32 {
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, font);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        (tm.tmHeight + tm.tmExternalLeading).max(1)
    }
}

static RES: Mutex<Option<Box<Resources>>> = Mutex::new(None);

fn res() -> std::sync::MutexGuard<'static, Option<Box<Resources>>> {
    RES.lock().unwrap()
}

// ============================================================
// Public API
// ============================================================

/// The live window, if there is one.
fn live() -> Option<HWND> {
    let v = *ASK_HWND.lock().unwrap();
    if v == 0 {
        return None;
    }
    let hwnd = HWND(v as *mut _);
    unsafe { IsWindow(hwnd).as_bool() }.then_some(hwnd)
}

pub fn is_open() -> bool {
    live().is_some()
}

pub fn close() {
    if let Some(hwnd) = live() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Hands the window a selection that finished copying after it was shown.
///
/// Dropped unless the input is still untouched — the point is to catch a slow
/// application, never to overwrite something the user has started typing.
pub fn prefill_late(text: String) {
    if text.trim().is_empty() {
        return;
    }
    let Some(hwnd) = live() else {
        return;
    };
    *PENDING_PREFILL.lock().unwrap() = Some(text);
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_PREFILL, WPARAM(0), LPARAM(0));
    }
}

/// Opens the window with `prefill` already in the input, unsent.
pub fn open(prefill: &str) {
    unsafe {
        if let Some(hwnd) = live() {
            let _ = SetForegroundWindow(hwnd);
            focus_input(hwnd);
            return;
        }

        {
            let mut g = RES.lock().unwrap();
            if g.is_none() {
                *g = Some(Box::new(Resources::new()));
            }
        }
        HISTORY.lock().unwrap().clear();
        PENDING_ATTACH.lock().unwrap().clear();
        *CARD.lock().unwrap() = None;
        // The height carries over from the last window otherwise: it is a static,
        // and a question left long at close would open the next one tall.
        *INPUT_H.lock().unwrap() = layout::INPUT_H;
        *PENDING.lock().unwrap() = None;
        *PENDING_PREFILL.lock().unwrap() = None;
        *BUSY.lock().unwrap() = false;

        let Some(hmodule) = GetModuleHandleW(None).ok() else {
            return;
        };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransAsk2");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(ask_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: res().as_ref().unwrap().panel_brush(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Beside the selection the question will be about, so the text being
        // asked about stays where it was read; centred on the work area when
        // there is no selection and so nothing to stand beside.  The top edge
        // then stays put when an answer arrives, so the input never jumps out
        // from under the cursor.
        let prefill = prefill.trim();
        let anchor = selection_anchor();
        *ANCHOR.lock().unwrap() = anchor;
        let head = layout::head_h(false);
        let (x, y) = if prefill.is_empty() {
            anchored_origin(layout::WIN_W, head)
        } else {
            selection_origin(anchor, layout::WIN_W, head)
        };
        let title = to_wide(i18n::t("ask.title"));

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            layout::WIN_W,
            head,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap_or_default();

        if hwnd.0.is_null() {
            return;
        }

        *ASK_HWND.lock().unwrap() = hwnd.0 as isize;
        create_controls(hwnd, hinstance);

        // Say up front when there is no key, rather than after a question has
        // been typed and thrown away.
        if settings::current().gemini_api_key.trim().is_empty() {
            set_answer(hwnd, i18n::t("ask.no_key"));
            fit_to_content(hwnd);
        }

        put_question(hwnd, prefill);
        // A prefilled selection can be several lines long, so the window opens
        // at the height that text actually needs.
        if measure_input(hwnd) {
            place_input(hwnd);
            fit_to_content(hwnd);
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        focus_input(hwnd);
    }
}

/// Top-left corner placing a `width` x `height` window horizontally centred and
/// vertically a third of the way down the work area of the monitor the pointer
/// is on.
unsafe fn anchored_origin(width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        match work_area(MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY)) {
            Some(wa) => (
                wa.left + (wa.right - wa.left - width) / 2,
                anchor_y(wa.top, wa.bottom - wa.top, height),
            ),
            None => {
                let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                ((sw - width) / 2, anchor_y(0, sh, height))
            }
        }
    }
}

/// Top-left corner placing a `width` x `height` window just under the text the
/// user has selected, with the input starting where that text does.
///
/// Flipped above the selection when there is no room below it, and nudged
/// inside the work area either way.
unsafe fn selection_origin(at: RECT, width: i32, height: i32) -> (i32, i32) {
    unsafe {
        use layout::{ANCHOR_GAP, EDGE, PAD_X};

        let Some(wa) = work_area(MonitorFromPoint(
            POINT {
                x: at.left,
                y: at.bottom,
            },
            MONITOR_DEFAULTTONEAREST,
        )) else {
            return anchored_origin(width, height);
        };

        let mut y = at.bottom + ANCHOR_GAP;
        if y + height > wa.bottom - EDGE {
            y = at.top - ANCHOR_GAP - height;
        }
        (
            clamp_span(at.left - PAD_X, width, wa.left, wa.right),
            clamp_span(y, height, wa.top, wa.bottom),
        )
    }
}

/// Where on screen the selected text is, as well as it can be told in the
/// microseconds this is allowed to take.
///
/// The caret is the accurate answer — it sits at the live end of a selection —
/// and costs one call, but only applications with a real Win32 caret have one,
/// which a page of text selected in a browser does not.  The pointer is the
/// fallback and is rarely far off: a selection dragged out with the mouse ends
/// underneath it, and the hotkey is pressed a moment later.
///
/// Nothing here asks UI Automation, which would answer for every application
/// and spend tens of milliseconds doing it.  The window is on screen in less.
unsafe fn selection_anchor() -> RECT {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let pointer = RECT {
            left: pt.x,
            top: pt.y,
            right: pt.x,
            bottom: pt.y,
        };

        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return pointer;
        }
        let mut gti = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(GetWindowThreadProcessId(fg, None), &mut gti).is_err()
            || gti.hwndCaret.0.is_null()
        {
            return pointer;
        }

        let caret_h = gti.rcCaret.bottom - gti.rcCaret.top;
        if caret_h <= 0 || caret_h > MAX_CARET_H {
            return pointer;
        }

        let mut tl = POINT {
            x: gti.rcCaret.left,
            y: gti.rcCaret.top,
        };
        let mut br = POINT {
            x: gti.rcCaret.right,
            y: gti.rcCaret.bottom,
        };
        if !ClientToScreen(gti.hwndCaret, &mut tl).as_bool()
            || !ClientToScreen(gti.hwndCaret, &mut br).as_bool()
        {
            return pointer;
        }

        // Scrolled out of view, or left behind in a field that is no longer on
        // screen: a real caret, but not where the text is.
        let mut fg_rc = RECT::default();
        if GetWindowRect(fg, &mut fg_rc).is_err()
            || tl.x < fg_rc.left
            || tl.x > fg_rc.right
            || tl.y < fg_rc.top
            || br.y > fg_rc.bottom
        {
            return pointer;
        }

        RECT {
            left: tl.x,
            top: tl.y,
            right: br.x,
            bottom: br.y,
        }
    }
}

/// Work area of `mon` — the screen minus the taskbar, which is not space this
/// window can use.
unsafe fn work_area(mon: HMONITOR) -> Option<RECT> {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(mon, &mut mi).as_bool().then_some(mi.rcWork)
    }
}

/// `start` moved as little as possible — and only ever towards `low` — for
/// `start .. start + len` to clear both edges of `low .. high`.
fn clamp_span(start: i32, len: i32, low: i32, high: i32) -> i32 {
    start.min(high - layout::EDGE - len).max(low + layout::EDGE)
}

// ============================================================
// Controls
// ============================================================

unsafe fn create_controls(parent: HWND, hinst: HINSTANCE) {
    unsafe {
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();

        // Both fields sit directly on the panel with no recessed box of their
        // own — at this size a border around the input would make it look like
        // a form rather than a prompt.
        let input = create_edit(
            parent,
            hinst,
            r.font_input(),
            &input_rect(0),
            IDC_QUESTION,
            ES_MULTILINE | ES_AUTOVSCROLL,
        );

        let answer = create_edit(
            parent,
            hinst,
            r.font_body(),
            &answer_rect(false, layout::ANSWER_MIN_H),
            IDC_ANSWER,
            ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | WS_VSCROLL,
        );
        // A native EDIT caps itself at 32 KB by default, which a long
        // conversation reaches; 0 means "as much as will fit".
        let _ = SendMessageW(answer, EM_LIMITTEXT, WPARAM(0), LPARAM(0));
        // The scroll bar is drawn by the theme engine, not by us, and it
        // arrives in the light palette.  This is how Explorer asks for dark.
        let _ = SetWindowTheme(answer, w!("DarkMode_Explorer"), None);
        let _ = ShowWindow(answer, SW_HIDE);

        // Enter belongs to the window, not to the edit control: unsubclassed,
        // a multiline EDIT would just insert a newline.  Through
        // `SetWindowSubclass` rather than a hand-rolled `GWLP_WNDPROC` swap,
        // which returns nothing usable for a system class like EDIT.
        for ctrl in [input, answer] {
            let _ = SetWindowSubclass(ctrl, Some(field_proc), 0, 0);
        }
    }
}

unsafe fn create_edit(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    rc: &RECT,
    id: i32,
    extra: WINDOW_STYLE,
) -> HWND {
    unsafe {
        let class = to_wide("EDIT");
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | extra,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let _ = SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        // The panel already provides the padding; another margin inside the
        // control would push the caret away from the left edge of the text.
        let _ = SendMessageW(edit, EM_SETMARGINS, WPARAM(3), LPARAM(0));
        edit
    }
}

// ============================================================
// Field subclass — Enter sends, Shift+Enter breaks, Esc closes
// ============================================================

unsafe extern "system" fn field_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _ref_data: usize,
) -> LRESULT {
    unsafe {
        let is_input = GetDlgCtrlID(hwnd) == IDC_QUESTION;
        // `GetKeyState`, not `GetAsyncKeyState`: inside a window procedure the
        // state that matters is the one that went with the message being
        // handled, not whatever the keyboard happens to be doing now.
        let down = |vk: i32| GetKeyState(vk) as u16 & 0x8000 != 0;
        let shift = || down(VK_SHIFT.0 as i32);
        let ctrl = || down(VK_CONTROL.0 as i32);

        match msg {
            // The stock EDIT control has never implemented Ctrl+A; it comes
            // from the dialog manager, which a bare `WS_POPUP` window doesn't
            // have.  Applies to the answer pane too, where select-all is what
            // makes Ctrl+C useful.
            WM_KEYDOWN if wp.0 == VK_A && ctrl() => {
                let _ = SendMessageW(hwnd, EM_SETSEL, WPARAM(0), LPARAM(-1));
                LRESULT(0)
            }
            // Ctrl+A also arrives as WM_CHAR 0x01, which the control would
            // answer with a beep.
            WM_CHAR if wp.0 == 0x01 => LRESULT(0),

            // Pasting a picture attaches it.  A stock EDIT ignores an image on
            // the clipboard entirely — nothing happens and nothing says why —
            // so this takes the paste when there is one to take and hands it
            // back to the control otherwise.
            //
            // At `WM_PASTE` rather than at the keystroke, because that is where
            // every route into pasting converges: Ctrl+V, Shift+Insert and the
            // context menu all end up here.  Catching the key instead means
            // catching each of them separately — and Ctrl+V does not even
            // arrive as a key the control acts on: an EDIT pastes off the
            // control character in `WM_CHAR`, so a handler on `WM_KEYDOWN`
            // that swallows the keystroke silently breaks pasting text.
            WM_PASTE if is_input => {
                let taken = GetParent(hwnd).is_ok_and(|p| attach_clipboard_image(p));
                if taken {
                    LRESULT(0)
                } else {
                    DefSubclassProc(hwnd, msg, wp, lp)
                }
            }

            WM_KEYDOWN if wp.0 == 0x1B => {
                // VK_ESCAPE — close from whichever field has focus.
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 == 0x0D && is_input && !shift() => {
                if let Ok(parent) = GetParent(hwnd) {
                    send_question(parent);
                }
                LRESULT(0)
            }
            // `TranslateMessage` turns the same key press into a WM_CHAR that
            // would otherwise leave a stray blank line behind.
            WM_CHAR if wp.0 == 0x0D && is_input && !shift() => LRESULT(0),

            // The placeholder is painted over the empty control: without a
            // v6 common-controls manifest there is no `EM_SETCUEBANNER`.
            WM_PAINT if is_input => {
                let r = DefSubclassProc(hwnd, msg, wp, lp);
                if GetWindowTextLengthW(hwnd) == 0 {
                    draw_placeholder(hwnd);
                }
                r
            }
            _ => DefSubclassProc(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn draw_placeholder(edit: HWND) {
    unsafe {
        let dc = GetDC(edit);
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old = SelectObject(dc, r.font_input());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(theme::CLR_HINT));

        let mut rc = RECT::default();
        let _ = GetClientRect(edit, &mut rc);
        rc.left += 2;
        let key = match attach_kind() {
            AttachKind::Image => "ask.placeholder_image",
            AttachKind::File => "ask.placeholder_file",
            AttachKind::None => "ask.placeholder",
        };
        let mut text = to_wide(i18n::t(key));
        if text.last() == Some(&0) {
            text.pop();
        }
        DrawTextW(dc, &mut text, &mut rc, DRAW_TEXT_FORMAT(DT_LINE));

        SelectObject(dc, old);
        ReleaseDC(edit, dc);
    }
}

// ============================================================
// Sending
// ============================================================

unsafe fn send_question(hwnd: HWND) {
    unsafe {
        if hwnd.0.is_null() || *BUSY.lock().unwrap() {
            return;
        }

        let typed = read_text(hwnd, IDC_QUESTION).trim().to_string();
        let kind = attach_kind();
        // A file on its own is a question in itself.  Rather than sitting there
        // doing nothing on Enter, it asks the obvious one.
        let question = if typed.is_empty() {
            match kind {
                AttachKind::Image => i18n::t("ask.describe_image").to_string(),
                AttachKind::File => i18n::t("ask.describe_file").to_string(),
                AttachKind::None => typed,
            }
        } else {
            typed
        };
        if question.is_empty() {
            return;
        }

        let cfg = settings::current();
        let gm_key = cfg.gemini_api_key.trim().to_string();
        let search = Search {
            key: cfg.search_api_key.trim().to_string(),
            enabled: cfg.web_search,
        };
        if gm_key.is_empty() {
            println!("[ask] no API key configured");
            set_answer(hwnd, i18n::t("ask.no_key"));
            fit_to_content(hwnd);
            return;
        }
        let tag = match kind {
            AttachKind::Image => Some("ask.image_tag"),
            AttachKind::File => Some("ask.file_tag"),
            AttachKind::None => None,
        };
        println!(
            "[ask] asking ({} chars{}) via {}",
            question.chars().count(),
            if tag.is_some() { " + file" } else { "" },
            gemini::MODEL,
        );

        set_text(hwnd, IDC_QUESTION, "");
        // The line collapses back to one now the question has left it.
        if measure_input(hwnd) {
            place_input(hwnd);
        }
        HISTORY.lock().unwrap().push(Msg {
            who: Who::User,
            text: question,
            tag,
            attach: Vec::new(),
        });
        *BUSY.lock().unwrap() = true;
        // The previous answer's card goes with the question it belonged to;
        // leaving it up while the next one is being thought about would read as
        // an answer to the new question.
        *CARD.lock().unwrap() = None;
        *ASKED_AT.lock().unwrap() = Some(std::time::Instant::now());
        SetTimer(hwnd, THINK_TIMER, THINK_TICK_MS, None);
        render_transcript(hwnd, Some(&thinking_line()));
        let _ = InvalidateRect(hwnd, None, true);

        // Answer in the UI language: the window is opened from a hotkey with
        // no chance to say "reply in Ukrainian" every time.
        let system = format!(
            "You are a concise, knowledgeable assistant. Answer in {}. \
             Be direct and specific; skip pleasantries and restating the question. \
             Use plain text \u{2014} no Markdown syntax, since the answer is shown in a \
             plain text box. When the answer centres on figures \u{2014} weather, a rate, a \
             price, a specification \u{2014} also fill in the card: it is drawn above your \
             text, so keep the text itself short and do not simply repeat the numbers \
             already in the card.",
            i18n::current().native_name()
        );
        let sources_label = i18n::t("ask.sources").to_string();

        // Cloned rather than taken: the file stays in the input until an answer
        // actually arrives, so it is still there to retry with if the network
        // drops, and so the user can see what they asked about.
        let pending = PENDING_ATTACH.lock().unwrap().clone();

        let target = hwnd.0 as isize;
        std::thread::spawn(move || {
            let result = run_turn(&system, &gm_key, &search, pending, &sources_label)
                .map_err(|e| e.to_string());
            match &result {
                Ok(_) => println!("[ask] reply received"),
                Err(e) => println!("[!] [ask] {e}"),
            }
            *PENDING.lock().unwrap() = Some(result);
            let hwnd = HWND(target as *mut _);
            if IsWindow(hwnd).as_bool() {
                let _ = PostMessageW(hwnd, WM_APP_REPLY, WPARAM(0), LPARAM(0));
            }
        });
    }
}

/// One whole exchange, off the window thread: prepare the file, then ask.
///
/// Encoding an image is slow enough to matter — a full-screen grab takes real
/// time to shrink and compress — which is why it doesn't happen where a repaint
/// would be waiting on it.  Gemini answers text, files and web searches all the
/// same way, so there is only ever the one path.
/// A structured summary the model may return alongside its answer.
///
/// Deliberately one shape rather than one per topic: a heading, the number that
/// matters, a line of context and a few labelled rows covers weather, an
/// exchange rate, a population, a spec sheet — anything whose answer is really a
/// figure plus details.  A card per subject would be a new painting routine
/// every time the user asked about something new.
#[derive(Default, Clone)]
struct Card {
    title: String,
    value: String,
    subtitle: String,
    rows: Vec<(String, String)>,
    /// Colours the value: a rate that rose reads green, one that fell red.
    accent: Accent,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Accent {
    #[default]
    None,
    Up,
    Down,
}

impl Card {
    /// A card with nothing in it is not worth the space it would take.
    fn is_empty(&self) -> bool {
        self.value.trim().is_empty() && self.rows.is_empty()
    }
}

/// The card from the newest answer, if it had one.  Only the latest is shown:
/// the pane below already keeps the whole conversation as text.
static CARD: Mutex<Option<Card>> = Mutex::new(None);

/// What the model is asked to fill in.  `card` is nullable on purpose — most
/// questions have no figure at their heart, and a card invented for one of them
/// would be worse than none.
fn card_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "answer": {
                "type": "string",
                "description": "The full answer in plain text, as it would be written without any card."
            },
            "card": {
                "type": "object",
                "nullable": true,
                "description":
                    "Fill this in ONLY when the answer centres on a figure or a set of \
                     readings — weather, an exchange or crypto rate, a price, a score, a \
                     specification. Leave it null for explanations, opinions, code, \
                     translations and anything conversational. Never invent numbers to \
                     fill it.",
                "properties": {
                    "title": { "type": "string", "description": "What the figure is about, e.g. a city or a currency pair." },
                    "value": { "type": "string", "description": "The headline figure with its unit, e.g. '-3 °C' or '$64,120'." },
                    "subtitle": { "type": "string", "description": "One short line of context, e.g. 'feels like -8 °C' or '+2.4% today'." },
                    "rows": {
                        "type": "array",
                        "description": "Up to five supporting readings.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["label", "value"]
                        }
                    },
                    "accent": {
                        "type": "string",
                        "enum": ["none", "up", "down"],
                        "description": "'up' or 'down' when the figure has risen or fallen; otherwise 'none'."
                    }
                },
                "required": ["title", "value"]
            }
        },
        "required": ["answer"]
    })
}

/// Pulls the answer and any card out of a structured reply.  A reply that isn't
/// the JSON we asked for is shown as it came: the answer matters more than the
/// decoration, and refusing to display it would be the worse failure.
fn parse_structured(raw: &str) -> (String, Option<Card>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (raw.to_string(), None);
    };
    let answer = v
        .get("answer")
        .and_then(|a| a.as_str())
        .unwrap_or(raw)
        .trim()
        .to_string();

    let card = v.get("card").and_then(|c| c.as_object()).map(|c| {
        let s = |k: &str| {
            c.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        Card {
            title: s("title"),
            value: s("value"),
            subtitle: s("subtitle"),
            rows: c
                .get("rows")
                .and_then(|r| r.as_array())
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| {
                            let o = r.as_object()?;
                            let l = o.get("label")?.as_str()?.trim().to_string();
                            let v = o.get("value")?.as_str()?.trim().to_string();
                            (!l.is_empty() || !v.is_empty()).then_some((l, v))
                        })
                        .take(layout::CARD_MAX_ROWS)
                        .collect()
                })
                .unwrap_or_default(),
            accent: match s("accent").as_str() {
                "up" => Accent::Up,
                "down" => Accent::Down,
                _ => Accent::None,
            },
        }
    });

    (answer, card.filter(|c| !c.is_empty()))
}

/// The web-search key, plus whether the user wants it used at all.
#[derive(Clone)]
struct Search {
    key: String,
    enabled: bool,
}

impl Search {
    fn ready(&self) -> bool {
        self.enabled && !self.key.is_empty()
    }
}

fn run_turn(
    system: &str,
    gm_key: &str,
    search: &Search,
    pending: Vec<Pending>,
    sources_label: &str,
) -> anyhow::Result<String> {
    if !pending.is_empty() {
        let mut prepared = Vec::with_capacity(pending.len());
        for p in pending {
            let a = match p {
                Pending::Image(shot) => {
                    // Images are shrunk to the model's working resolution first,
                    // which puts them far under the inline ceiling every time.
                    let png = gemini::prepare(&shot.bgra, shot.w, shot.h)?;
                    println!("[ask] image {}x{} -> {} KB PNG", shot.w, shot.h, png.len() / 1024);
                    gemini::Attach {
                        data: gemini::Data::Inline(Arc::new(png)),
                        mime: "image/png".to_string(),
                    }
                }
                Pending::File {
                    data,
                    mime,
                    ext,
                    name,
                } => {
                    if data.len() > gemini::INLINE_LIMIT {
                        // Too big to carry in the request — hand it to the Files
                        // API and send a reference instead.
                        println!(
                            "[ask] uploading {name} ({} KB, {mime})\u{2026}",
                            data.len() / 1024
                        );
                        let uri = gemini::upload(gm_key, &data, &mime, &name)?;
                        println!("[ask] uploaded: {uri}");
                        gemini::Attach {
                            data: gemini::Data::Uploaded(uri),
                            mime,
                        }
                    } else {
                        println!("[ask] file .{ext} {} KB ({mime})", data.len() / 1024);
                        gemini::Attach {
                            data: gemini::Data::Inline(data),
                            mime,
                        }
                    }
                }
            };
            prepared.push(a);
        }
        set_attaches(prepared);
    }

    let mut turns: Vec<gemini::Turn> = HISTORY
        .lock()
        .unwrap()
        .iter()
        .map(|m| gemini::Turn {
            role: match m.who {
                Who::User => "user",
                Who::Model => "model",
            },
            text: m.text.clone(),
            files: m.attach.clone(),
        })
        .collect();

    // Search the web ourselves when there are keys for it: Gemini's own
    // grounding is refused on free-tier accounts, so this is what actually gets
    // current information into the answer.  A failed search is not a failed
    // question — it is logged and the model answers from what it knows.
    let mut hits = Vec::new();
    if search.ready() {
        // The latest question is what to search for; earlier turns are context
        // the model already has.
        let query = turns
            .iter()
            .rev()
            .find(|t| t.role == "user")
            .map(|t| t.text.clone())
            .unwrap_or_default();
        if !query.trim().is_empty() {
            let t = std::time::Instant::now();
            match websearch::search(&search.key, &query) {
                Ok(h) if !h.is_empty() => {
                    println!(
                        "[ask] web search: {} results in {} ms for {query:?}",
                        h.len(),
                        t.elapsed().as_millis()
                    );
                    // Prepended as its own turn rather than folded into the
                    // system prompt, so the results sit next to the question
                    // they belong to and don't leak into later ones.
                    turns.insert(
                        turns.len() - 1,
                        gemini::Turn {
                            role: "user",
                            text: websearch::as_context(&h),
                            files: Vec::new(),
                        },
                    );
                    hits = h;
                }
                Ok(_) => println!("[ask] web search: nothing found"),
                Err(e) => println!("[!] [ask] {e}"),
            }
        }
    }

    // Gemini's built-in grounding is only worth attempting when we aren't doing
    // the searching ourselves.  It is refused on free keys, so a quota refusal
    // drops to a plain answer rather than failing the question; other errors
    // (bad request, network) are real and propagate.
    // With the toggle off nothing goes looking for the web at all — not our own
    // search, and not the model's built-in one either.
    let use_grounding = search.enabled && !search.ready();
    // Structured output can't travel with a tool, so the card is only asked for
    // on the path that doesn't use one.  On the grounding path the reply is
    // plain text and simply has no card.
    let schema = (!use_grounding).then(card_schema);
    let t = std::time::Instant::now();
    let answer = match gemini::ask(gm_key, system, &turns, use_grounding, schema, 0.7, MODEL_TIMEOUT) {
        Ok(a) => a,
        Err(e) if use_grounding && is_quota(&e) => {
            println!("[ask] search refused on quota ({e}); retrying without search");
            gemini::ask(gm_key, system, &turns, false, Some(card_schema()), 0.7, MODEL_TIMEOUT)?
        }
        Err(e) => return Err(e),
    };

    println!("[ask] model answered in {} ms", t.elapsed().as_millis());

    // The card is lifted out of the reply and kept aside to be painted; only the
    // prose goes into the transcript.
    let (text, card) = parse_structured(&answer.text);
    *CARD.lock().unwrap() = card;

    // The sources ride along under the answer, in the UI language.  They live in
    // the transcript with it, so a follow-up resends them as context — cheap,
    // and it keeps the model honest about what it already cited.
    let mut out = text;
    let sources: Vec<(String, String)> = if hits.is_empty() {
        answer
            .sources
            .into_iter()
            .map(|s| (s.title, s.uri))
            .collect()
    } else {
        hits.into_iter().map(|h| (h.title, h.link)).collect()
    };
    if !sources.is_empty() {
        out.push_str("\n\n");
        out.push_str(sources_label);
        for (title, uri) in sources {
            out.push_str("\n\u{2022} ");
            out.push_str(&title);
            out.push_str("\n  ");
            out.push_str(&uri);
        }
    }
    Ok(out)
}

/// Whether a failure was the server refusing on quota — the one error worth
/// retrying without search, since grounding has a separate allowance the plain
/// endpoint doesn't.  Matched on the message because the transport only hands
/// back a formatted string by this point.
fn is_quota(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("quota") || s.contains("exceeded") || s.contains("429")
}

/// Hangs the prepared files on the turn they arrived with, so follow-ups still
/// carry them.
fn set_attaches(files: Vec<gemini::Attach>) {
    let mut history = HISTORY.lock().unwrap();
    if let Some(m) = history.iter_mut().rev().find(|m| m.who == Who::User) {
        m.attach = files;
    }
}

unsafe fn take_reply(hwnd: HWND) {
    unsafe {
        let Some(result) = PENDING.lock().unwrap().take() else {
            return;
        };
        *BUSY.lock().unwrap() = false;
        *ASKED_AT.lock().unwrap() = None;
        let _ = KillTimer(hwnd, THINK_TIMER);

        match result {
            Ok(reply) => {
                HISTORY.lock().unwrap().push(Msg {
                    who: Who::Model,
                    text: reply,
                    tag: None,
                    attach: Vec::new(),
                });
                // The files have been asked about and now live on their turn in
                // the history, so the input goes back to being a plain line until
                // another one is attached.
                let had = {
                    let mut g = PENDING_ATTACH.lock().unwrap();
                    let had = !g.is_empty();
                    g.clear();
                    had
                };
                if had {
                    place_input(hwnd);
                }
                render_transcript(hwnd, None);
                // The card is painted by the window itself, so the whole panel
                // repaints whether or not the height changed.
                let _ = InvalidateRect(hwnd, None, true);
                let _ = had;
            }
            Err(e) => {
                // Drop the unanswered question from the history — it must not
                // go out as context next time — but hand it back to the input
                // so a network blip doesn't cost the user their typing.  The
                // picture is still attached, untouched, for the same reason.
                let failed = HISTORY.lock().unwrap().pop();
                if let Some(m) = failed
                    && m.who == Who::User
                {
                    set_text(hwnd, IDC_QUESTION, &m.text);
                }
                render_transcript(hwnd, Some(&format!("{}{e}", i18n::t("popup.error_prefix"))));
            }
        }
        focus_input(hwnd);
    }
}

/// Rebuilds the answer pane from the conversation, plus an optional trailing
/// status line ("Thinking…", an error).  Cheaper than incremental appends and
/// impossible to get out of step with `HISTORY`.
unsafe fn render_transcript(hwnd: HWND, trailing: Option<&str>) {
    unsafe {
        let mut out = String::new();
        for m in HISTORY.lock().unwrap().iter() {
            if !out.is_empty() {
                out.push_str("\r\n\r\n");
            }
            if m.who == Who::User {
                // Quoted, so a question is never mistaken for an answer.
                out.push_str("> ");
                // Once the chip is gone this tag is all that says which question
                // was the one about a file.
                if let Some(k) = m.tag {
                    out.push_str(i18n::t(k));
                    out.push(' ');
                }
                out.push_str(&m.text.replace('\n', "\n> "));
            } else {
                out.push_str(&m.text);
            }
        }
        if let Some(t) = trailing {
            if !out.is_empty() {
                out.push_str("\r\n\r\n");
            }
            out.push_str(t);
        }
        set_answer(hwnd, &out);
        fit_to_content(hwnd);
    }
}

/// Replaces the answer pane's text and pins the view to the bottom, where the
/// newest turn is.
unsafe fn set_answer(hwnd: HWND, text: &str) {
    unsafe {
        // EDIT wants CRLF; a bare LF renders as a box.
        let normalised = text.replace("\r\n", "\n").replace('\n', "\r\n");
        set_text(hwnd, IDC_ANSWER, &normalised);

        if let Ok(ctrl) = GetDlgItem(hwnd, IDC_ANSWER) {
            let end = normalised.encode_utf16().count();
            let _ = SendMessageW(ctrl, EM_SETSEL, WPARAM(end), LPARAM(end as isize));
            let _ = SendMessageW(ctrl, EM_SCROLLCARET, WPARAM(0), LPARAM(0));
        }
    }
}

// ============================================================
// Growing and shrinking
// ============================================================

/// Sizes the window to whatever the answer pane currently holds: collapsed to
/// the bare input when empty, otherwise tall enough for the wrapped text up to
/// a ceiling, past which the pane scrolls.
///
/// The top edge stays put unless the bottom of the screen forces it up.
/// Re-centring on every reply would drag the input out from under the cursor
/// mid-conversation.
unsafe fn fit_to_content(hwnd: HWND) {
    unsafe {
        let Ok(answer) = GetDlgItem(hwnd, IDC_ANSWER) else {
            return;
        };

        let shot = attached();
        let height = if GetWindowTextLengthW(answer) == 0 {
            let _ = ShowWindow(answer, SW_HIDE);
            layout::head_h(shot)
        } else {
            // Wrapped lines, not newlines — the control has already done the
            // wrapping at its real width.
            let lines = SendMessageW(answer, EM_GETLINECOUNT, WPARAM(0), LPARAM(0)).0 as i32;
            let line_h = res().as_ref().unwrap().line_h;
            let natural = lines.max(1) * line_h + 4;
            let answer_h = natural.clamp(layout::ANSWER_MIN_H, layout::ANSWER_MAX_H);

            // `WS_VSCROLL` alone leaves the bar parked there whether or not
            // there is anything to scroll, and since the window now grows to
            // fit, that is almost always.  Show it only once the answer is
            // taller than the ceiling.
            let _ = ShowScrollBar(answer, SB_VERT, natural > layout::ANSWER_MAX_H);

            let rc = answer_rect(shot, answer_h);
            let _ = MoveWindow(
                answer,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                true,
            );
            let _ = ShowWindow(answer, SW_SHOW);
            layout::expanded(shot, answer_h)
        };

        let mut cur = RECT::default();
        let _ = GetWindowRect(hwnd, &mut cur);
        if cur.bottom - cur.top == height {
            return;
        }

        // Before it is on screen there is nothing to animate — and the
        // no-key notice takes this path, which should simply be the size it
        // opens at.
        if !IsWindowVisible(hwnd).as_bool() {
            set_height(hwnd, height);
            return;
        }

        *ANIM_TARGET.lock().unwrap() = height;
        SetTimer(hwnd, ANIM_TIMER, ANIM_TICK_MS, None);
    }
}

/// Moves the window beside the selection without resizing it, once a copy the
/// window opened without has finally arrived.
unsafe fn move_to_selection(hwnd: HWND) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let anchor = *ANCHOR.lock().unwrap();
        let (x, y) = selection_origin(anchor, layout::WIN_W, rc.bottom - rc.top);
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Puts the window at `height` immediately, region and all.
///
/// It grows downwards from wherever it is — opened beside a selection near the
/// foot of the screen, or dragged there, that runs out of screen before it runs
/// out of answer.  So the last stretch of growth is taken off the top edge
/// instead, which is the only way the rest of it is readable at all.
unsafe fn set_height(hwnd: HWND, height: i32) {
    unsafe {
        let mut cur = RECT::default();
        let _ = GetWindowRect(hwnd, &mut cur);
        let y = match work_area(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) {
            Some(wa) => clamp_span(cur.top, height, wa.top, wa.bottom),
            None => cur.top,
        };

        let _ = SetWindowPos(
            hwnd,
            None,
            cur.left,
            y,
            layout::WIN_W,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = InvalidateRect(hwnd, None, true);
    }
}

/// One frame of the unfold: cover a share of what is left, and stop once the
/// remainder is smaller than a pixel of travel.
unsafe fn step_unfold(hwnd: HWND) {
    unsafe {
        let target = *ANIM_TARGET.lock().unwrap();
        let mut cur = RECT::default();
        let _ = GetWindowRect(hwnd, &mut cur);
        let now = cur.bottom - cur.top;

        let remaining = target - now;
        if remaining == 0 {
            let _ = KillTimer(hwnd, ANIM_TIMER);
            return;
        }

        // At least a pixel, so a slow tail still converges.
        let step = ((remaining as f32 * ANIM_EASE) as i32).clamp(-remaining.abs(), remaining.abs());
        let step = if step == 0 { remaining.signum() } else { step };
        let next = now + step;

        if (target - next).abs() <= 1 {
            let _ = KillTimer(hwnd, ANIM_TIMER);
            set_height(hwnd, target);
        } else {
            set_height(hwnd, next);
        }
    }
}

/// Vertical origin that centres a `height`-tall box on the anchor line, kept
/// on screen if the area is too short for it.
fn anchor_y(top: i32, area_h: i32, height: i32) -> i32 {
    let centre = top + area_h * layout::ANCHOR_NUM / layout::ANCHOR_DEN;
    (centre - height / 2).max(top + 4)
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn ask_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // A read-only EDIT asks for its colours through
            // `WM_CTLCOLORSTATIC`, not `WM_CTLCOLOREDIT` — miss that and the
            // answer pane comes up white on a dark panel.
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkColor(hdc, COLORREF(theme::CLR_CARD));
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));
                LRESULT(res().as_ref().unwrap().panel_brush().0 as isize)
            }

            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }

            // The question line grows with what is typed into it.  EN_CHANGE is
            // the only notification that catches every route in — typing, paste,
            // undo — and remeasuring is cheap enough to do on each of them.
            WM_COMMAND => {
                let code = ((wp.0 >> 16) & 0xFFFF) as u16;
                let id = (wp.0 & 0xFFFF) as i32;
                const EN_CHANGE: u16 = 0x0300;
                if code == EN_CHANGE && id == IDC_QUESTION && measure_input(hwnd) {
                    relayout(hwnd);
                }
                LRESULT(0)
            }

            WM_TIMER if wp.0 == ANIM_TIMER => {
                step_unfold(hwnd);
                LRESULT(0)
            }

            WM_TIMER if wp.0 == THINK_TIMER => {
                if *BUSY.lock().unwrap() {
                    render_transcript(hwnd, Some(&thinking_line()));
                } else {
                    let _ = KillTimer(hwnd, THINK_TIMER);
                }
                LRESULT(0)
            }

            m if m == WM_APP_REPLY => {
                take_reply(hwnd);
                LRESULT(0)
            }

            m if m == WM_APP_PREFILL => {
                // Checked here rather than at the call site so the test and the
                // write happen on the thread that owns the control, with no gap
                // for a keystroke to land in between.
                let text = PENDING_PREFILL.lock().unwrap().take();
                if let Some(text) = text {
                    let untouched = read_text(hwnd, IDC_QUESTION).is_empty()
                        && HISTORY.lock().unwrap().is_empty()
                        && !*BUSY.lock().unwrap();
                    if untouched {
                        put_question(hwnd, &text);
                        // The window went up centred because there was no
                        // selection to place it against yet.  There is now.
                        move_to_selection(hwnd);
                    }
                }
                LRESULT(0)
            }

            m if m == WM_APP_ATTACH_FILE => {
                // The picker hands back the chosen paths.  Reading and decoding
                // them goes to a worker, never here: a big file read whole on the
                // window thread freezes it long enough for Windows to kill the
                // app — which a large JSON did.
                let ptr = lp.0 as *mut Vec<String>;
                if !ptr.is_null() {
                    let paths = *Box::from_raw(ptr);
                    build_and_post(hwnd, paths);
                }
                LRESULT(0)
            }

            m if m == WM_APP_ATTACH_READY => {
                // The worker has done the slow part; dropping the results into the
                // input is cheap and stays on the window thread.
                let ptr = lp.0 as *mut AttachResult;
                if !ptr.is_null() {
                    let res = *Box::from_raw(ptr);
                    let mut added = 0;
                    {
                        let mut g = PENDING_ATTACH.lock().unwrap();
                        for p in res.items {
                            if g.len() >= MAX_CHIPS {
                                break;
                            }
                            g.push(p);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        relayout(hwnd);
                        focus_input(hwnd);
                    }
                    if res.too_big {
                        set_answer(hwnd, i18n::t("ask.file_too_large"));
                        fit_to_content(hwnd);
                    }
                }
                LRESULT(0)
            }

            // There is no caption to drag the window by, so the panel is the
            // caption.  This only ever fires on the parent's own pixels — a
            // click over either field is delivered to that control, which
            // needs it for the caret and for selecting text — so the padding
            // around them, and the strip under the answer, are the handle.
            //
            // `SendMessageW` here does not return until the move is over: the
            // caption drag is a modal loop inside `DefWindowProcW`.
            WM_SETCURSOR => {
                // A hand over the clickable bits — the attach button, and the
                // chip that removes an attachment — so they read as clickable.
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let over = in_rect(pt.x, pt.y, clip_rect(attached()))
                    || in_rect(pt.x, pt.y, globe_rect(attached()))
                    || chip_at(pt.x, pt.y).is_some();
                if over {
                    SetCursor(LoadCursorW(None, IDC_HAND).unwrap_or_default());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }

            WM_MOUSEMOVE => {
                let (x, y) = unpack_point(lp);
                for (rc, state) in [
                    (clip_rect(attached()), &CLIP_HOVER),
                    (globe_rect(attached()), &GLOBE_HOVER),
                ] {
                    let over = in_rect(x, y, rc);
                    let changed = {
                        let mut h = state.lock().unwrap();
                        let c = *h != over;
                        *h = over;
                        c
                    };
                    if changed {
                        let _ = InvalidateRect(hwnd, Some(&rc), false);
                    }
                }
                // Ask to be told when the pointer leaves, so the highlight
                // clears instead of sticking on.
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                LRESULT(0)
            }

            m if m == WM_MOUSELEAVE => {
                for (rc, state) in [
                    (clip_rect(attached()), &CLIP_HOVER),
                    (globe_rect(attached()), &GLOBE_HOVER),
                ] {
                    let was = {
                        let mut h = state.lock().unwrap();
                        let was = *h;
                        *h = false;
                        was
                    };
                    if was {
                        let _ = InvalidateRect(hwnd, Some(&rc), false);
                    }
                }
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                // The attach button, top-right — opens a file picker.  Checked
                // before the drag, so a click on it never starts moving the
                // window instead.
                {
                    let (x, y) = unpack_point(lp);
                    if in_rect(x, y, globe_rect(attached())) {
                        toggle_web(hwnd);
                        return LRESULT(0);
                    }
                    if in_rect(x, y, clip_rect(attached())) {
                        attach_file(hwnd);
                        return LRESULT(0);
                    }
                }

                // Clicking an attachment chip takes that one file back off — the
                // × badge marks each chip as its own remove target.
                {
                    let (x, y) = unpack_point(lp);
                    if let Some(i) = chip_at(x, y) {
                        PENDING_ATTACH.lock().unwrap().remove(i);
                        relayout(hwnd);
                        focus_input(hwnd);
                        return LRESULT(0);
                    }
                }

                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ReleaseCapture();
                let _ = SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    WPARAM(HTCAPTION as usize),
                    LPARAM(pack_point(pt)),
                );
                // A click on the panel that went nowhere should still leave the
                // window ready to be typed into.
                focus_input(hwnd);
                LRESULT(0)
            }

            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }

            WM_DESTROY => {
                let _ = KillTimer(hwnd, ANIM_TIMER);
                let _ = KillTimer(hwnd, THINK_TIMER);
                *ASK_HWND.lock().unwrap() = 0;
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let _ = FillRect(hdc, &rc, r.panel_brush());

        let head = layout::head_h(attached());
        // `r` is passed down rather than looked up again: this function holds the
        // resources lock, and a second `res()` on the same thread would deadlock
        // it — which is exactly what a non-image chip used to do.
        paint_chips(hdc, r);
        paint_clip(hdc);
        paint_globe(hdc);
        paint_card(hdc, r);

        // Everything below only exists once there is an answer to separate.
        if rc.bottom > head {
            let sep = RECT {
                left: 0,
                top: head,
                right: rc.right,
                bottom: head + 1,
            };
            let brush = CreateSolidBrush(COLORREF(theme::CLR_SEPARATOR));
            let _ = FillRect(hdc, &sep, brush);
            let _ = DeleteObject(brush);

            let old_font = SelectObject(hdc, r.font_hint());
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(theme::CLR_HINT));
            let mut hint = to_wide(i18n::t("ask.hint"));
            if hint.last() == Some(&0) {
                hint.pop();
            }
            let mut hint_rc = RECT {
                left: layout::PAD_X,
                top: rc.bottom - layout::BOTTOM - layout::HINT_H,
                right: rc.right - layout::PAD_X,
                bottom: rc.bottom - layout::BOTTOM,
            };
            DrawTextW(hdc, &mut hint, &mut hint_rc, DRAW_TEXT_FORMAT(DT_LINE));
            SelectObject(hdc, old_font);
        }

        // A hairline border stands in for the drop shadow the window no longer
        // casts: it defines the edge against whatever is behind it, without the
        // ragged corners the rounded region used to leave.
        let border = CreateSolidBrush(COLORREF(theme::CLR_SEPARATOR));
        let _ = FrameRect(hdc, &rc, border);
        let _ = DeleteObject(border);

        let _ = EndPaint(hwnd, &ps);
    }
}

// ============================================================
// Helpers
// ============================================================

/// Puts `text` in the input with the caret at its end — a starting point to add
/// a question to, not something to overtype.
unsafe fn put_question(hwnd: HWND, text: &str) {
    unsafe {
        if text.is_empty() {
            return;
        }
        set_text(hwnd, IDC_QUESTION, text);
        if let Ok(ctrl) = GetDlgItem(hwnd, IDC_QUESTION) {
            let end = text.encode_utf16().count();
            let _ = SendMessageW(ctrl, EM_SETSEL, WPARAM(end), LPARAM(end as isize));
        }
    }
}

/// A screen point in the form the non-client mouse messages carry it: two
/// signed 16-bit halves, y above x.
fn pack_point(pt: POINT) -> isize {
    (((pt.y as i16) as u16 as isize) << 16) | ((pt.x as i16) as u16 as isize)
}

/// Draws every attachment chip, left to right: images as thumbnails, other
/// files as extension-labelled squares, each with a remove badge.
unsafe fn paint_chips(hdc: HDC, r: &Resources) {
    unsafe {
        let guard = PENDING_ATTACH.lock().unwrap();
        for (i, p) in guard.iter().enumerate() {
            let box_rc = chip_rect(i);
            match p {
                Pending::Image(shot) => draw_image_chip(hdc, box_rc, shot),
                Pending::File { ext, .. } => draw_file_chip(hdc, box_rc, ext, r),
            }
            draw_remove_badge(hdc, box_rc);
        }
    }
}

/// Draws one image thumbnail into `box_rc`, letterboxed so a wide screenshot
/// isn't squashed into a portrait crop of itself.
unsafe fn draw_image_chip(hdc: HDC, box_rc: RECT, shot: &Shot) {
    unsafe {
        let (bw, bh) = (box_rc.right - box_rc.left, box_rc.bottom - box_rc.top);

        // The picture almost never has the square's proportions, so the
        // remainder is filled first — otherwise the panel shows through the
        // bars and the thumbnail reads as two disconnected strips.
        let back = CreateSolidBrush(COLORREF(theme::CLR_SEPARATOR));
        let _ = FillRect(hdc, &box_rc, back);
        let _ = DeleteObject(back);

        let scale = (bw as f64 / shot.w as f64).min(bh as f64 / shot.h as f64);
        let dw = ((shot.w as f64 * scale).round() as i32).clamp(1, bw);
        let dh = ((shot.h as f64 * scale).round() as i32).clamp(1, bh);
        let dx = box_rc.left + (bw - dw) / 2;
        let dy = box_rc.top + (bh - dh) / 2;

        // Negative height: the pixels are top-down, the way both the clipboard
        // and a screen grab hand them over, not bottom-up like a stored DIB.
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: shot.w as i32,
                biHeight: -(shot.h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // Averaging on the way down rather than dropping pixels: at this size
        // the alternative is a thumbnail made of whichever pixels happened to
        // land on the grid.
        let old_mode = SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
        StretchDIBits(
            hdc,
            dx,
            dy,
            dw,
            dh,
            0,
            0,
            shot.w as i32,
            shot.h as i32,
            Some(shot.bgra.as_ptr() as *const _),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        SetStretchBltMode(hdc, STRETCH_BLT_MODE(old_mode));
    }
}

/// Draws one non-image file chip into `box_rc`: a recessed square labelled with
/// the file's extension, since there is nothing to preview.
unsafe fn draw_file_chip(hdc: HDC, box_rc: RECT, ext: &str, r: &Resources) {
    unsafe {
        let style = paint::Style::flat(6, theme::CLR_FIELD).border(theme::CLR_SEPARATOR);
        paint::round_rect(hdc, &box_rc, &style);

        let old_font = SelectObject(hdc, r.font_hint());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));
        let mut label = to_wide(ext);
        if label.last() == Some(&0) {
            label.pop();
        }
        let mut trc = box_rc;
        // DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
        DrawTextW(hdc, &mut label, &mut trc, DRAW_TEXT_FORMAT(0x0825));
        SelectObject(hdc, old_font);
    }
}

/// A small × in the top-right corner of an attachment chip, so it is obvious
/// the whole chip is a click-to-remove target.
unsafe fn draw_remove_badge(hdc: HDC, box_rc: RECT) {
    unsafe {
        const BD: i32 = 14;
        let badge = RECT {
            left: box_rc.right - BD,
            top: box_rc.top,
            right: box_rc.right,
            bottom: box_rc.top + BD,
        };
        // A dark disc so the cross reads over whatever is behind it.
        paint::round_rect(hdc, &badge, &paint::Style::flat(BD / 2, theme::CLR_BG));

        let pad = 4;
        let pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_TEXT_BRIGHT));
        let old = SelectObject(hdc, pen);
        let _ = MoveToEx(hdc, badge.left + pad, badge.top + pad, None);
        let _ = LineTo(hdc, badge.right - pad, badge.bottom - pad);
        let _ = MoveToEx(hdc, badge.right - pad, badge.top + pad, None);
        let _ = LineTo(hdc, badge.left + pad, badge.bottom - pad);
        SelectObject(hdc, old);
        let _ = DeleteObject(pen);
    }
}

/// Green and red for a figure that rose or fell — the system colours, so they
/// read the same way as everywhere else on the machine.
const CLR_UP: u32 = 0x0058_D130;
const CLR_DOWN: u32 = 0x003A_45FF;

/// Draws the card above the answer: heading, the figure itself, a line of
/// context, then the supporting readings under a hairline.
unsafe fn paint_card(hdc: HDC, r: &Resources) {
    unsafe {
        // Cloned and the lock let go before anything else: `card_rect` measures
        // the card and would take the same non-reentrant lock again.
        let Some(card) = CARD.lock().unwrap().clone() else {
            return;
        };
        let rc = card_rect(attached());
        if rc.bottom <= rc.top {
            return;
        }

        paint::round_rect(
            hdc,
            &rc,
            &paint::Style::flat(8, theme::CLR_FIELD).border(theme::CLR_SEPARATOR),
        );

        let left = rc.left + layout::CARD_PAD;
        let right = rc.right - layout::CARD_PAD;
        let mut y = rc.top + layout::CARD_PAD;
        SetBkMode(hdc, TRANSPARENT);

        // A line of text in the card, left-aligned, in the given font/colour.
        let line = |text: &str, font: HFONT, color: u32, h: i32, y: &mut i32| {
            if text.is_empty() {
                return;
            }
            let old = SelectObject(hdc, font);
            SetTextColor(hdc, COLORREF(color));
            let mut wide = to_wide(text);
            if wide.last() == Some(&0) {
                wide.pop();
            }
            let mut trc = RECT {
                left,
                top: *y,
                right,
                bottom: *y + h,
            };
            DrawTextW(hdc, &mut wide, &mut trc, DRAW_TEXT_FORMAT(DT_LINE));
            SelectObject(hdc, old);
            *y += h;
        };

        line(
            &card.title,
            r.font_hint(),
            theme::CLR_HINT,
            layout::CARD_TITLE_H,
            &mut y,
        );
        let value_color = match card.accent {
            Accent::Up => CLR_UP,
            Accent::Down => CLR_DOWN,
            Accent::None => theme::CLR_TEXT_BRIGHT,
        };
        line(
            &card.value,
            r.font_value(),
            value_color,
            layout::CARD_VALUE_H,
            &mut y,
        );
        line(
            &card.subtitle,
            r.font_body(),
            theme::CLR_HINT,
            layout::CARD_SUB_H,
            &mut y,
        );

        if card.rows.is_empty() {
            return;
        }

        // The readings sit under a hairline, label left and value right, so the
        // figures line up in a column instead of trailing their labels.
        y += 4;
        let sep = RECT {
            left,
            top: y,
            right,
            bottom: y + 1,
        };
        let brush = CreateSolidBrush(COLORREF(theme::CLR_SEPARATOR));
        let _ = FillRect(hdc, &sep, brush);
        let _ = DeleteObject(brush);
        y += 5;

        let old = SelectObject(hdc, r.font_body());
        for (label, value) in &card.rows {
            let mut trc = RECT {
                left,
                top: y,
                right,
                bottom: y + layout::CARD_ROW_H,
            };

            SetTextColor(hdc, COLORREF(theme::CLR_HINT));
            let mut l = to_wide(label);
            if l.last() == Some(&0) {
                l.pop();
            }
            DrawTextW(hdc, &mut l, &mut trc, DRAW_TEXT_FORMAT(DT_LINE));

            SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));
            let mut v = to_wide(value);
            if v.last() == Some(&0) {
                v.pop();
            }
            // DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
            DrawTextW(hdc, &mut v, &mut trc, DRAW_TEXT_FORMAT(0x0822));

            y += layout::CARD_ROW_H;
        }
        SelectObject(hdc, old);
    }
}

/// Draws the web-search toggle: a globe, struck through when search is off.
///
/// Drawn rather than set in an icon font, for the same reason the plus is: a
/// glyph that a given Windows build happens not to have would leave a blank
/// square where a control should be.
unsafe fn paint_globe(hdc: HDC) {
    unsafe {
        let on = web_on();
        let hover = *GLOBE_HOVER.lock().unwrap();
        let rc = globe_rect(attached());

        let fill = if hover {
            theme::lighten(theme::CLR_FIELD, 12)
        } else {
            theme::CLR_FIELD
        };
        let border = if hover {
            theme::CLR_ACCENT
        } else {
            theme::CLR_SEPARATOR
        };
        paint::round_rect(hdc, &rc, &paint::Style::flat(6, fill).border(border));

        // On, the globe takes the accent colour — the one thing in the header
        // that is ever coloured, so its state is readable at a glance.
        let ink = if !on {
            theme::CLR_HINT
        } else if hover {
            theme::lighten(theme::CLR_ACCENT, 20)
        } else {
            theme::CLR_ACCENT
        };

        let cx = (rc.left + rc.right) / 2;
        let cy = (rc.top + rc.bottom) / 2;
        let r = 7;

        let pen = CreatePen(PS_SOLID, 1, COLORREF(ink));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        // The sphere, its equator, and one meridian: the least that still reads
        // as a globe at 24 pixels.
        let _ = Ellipse(hdc, cx - r, cy - r, cx + r + 1, cy + r + 1);
        let _ = MoveToEx(hdc, cx - r, cy, None);
        let _ = LineTo(hdc, cx + r + 1, cy);
        let _ = Ellipse(hdc, cx - r / 2 - 1, cy - r, cx + r / 2 + 2, cy + r + 1);

        // Off: struck through, the way a muted speaker or a disabled camera is.
        if !on {
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(pen);
            let slash = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_HINT));
            let old = SelectObject(hdc, slash);
            let _ = MoveToEx(hdc, cx - r - 1, cy + r + 1, None);
            let _ = LineTo(hdc, cx + r + 2, cy - r - 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(slash);
        } else {
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(pen);
        }
        SelectObject(hdc, old_brush);
    }
}

/// Draws the attach-a-file button: a recessed rounded square with a plus in it,
/// brightening on hover so it reads as clickable.
unsafe fn paint_clip(hdc: HDC) {
    unsafe {
        let hover = *CLIP_HOVER.lock().unwrap();
        let rc = clip_rect(attached());
        let fill = if hover {
            theme::lighten(theme::CLR_FIELD, 12)
        } else {
            theme::CLR_FIELD
        };
        let border = if hover {
            theme::CLR_ACCENT
        } else {
            theme::CLR_SEPARATOR
        };
        let style = paint::Style::flat(6, fill).border(border);
        paint::round_rect(hdc, &rc, &style);

        // A plus — "add" — drawn as two bars so it needs no icon font that a
        // given Windows build might be missing.
        let cx = (rc.left + rc.right) / 2;
        let cy = (rc.top + rc.bottom) / 2;
        let arm = 5;
        let plus = if hover {
            theme::CLR_TEXT_BRIGHT
        } else {
            theme::CLR_HINT
        };
        let brush = CreateSolidBrush(COLORREF(plus));
        let h_bar = RECT {
            left: cx - arm,
            top: cy - 1,
            right: cx + arm + 1,
            bottom: cy + 1,
        };
        let v_bar = RECT {
            left: cx - 1,
            top: cy - arm,
            right: cx + 1,
            bottom: cy + arm + 1,
        };
        let _ = FillRect(hdc, &h_bar, brush);
        let _ = FillRect(hdc, &v_bar, brush);
        let _ = DeleteObject(brush);
    }
}

/// Takes a picture off the clipboard and hangs it on the input.  `false` when
/// there wasn't one, so the paste can fall through to the control.
unsafe fn attach_clipboard_image(parent: HWND) -> bool {
    unsafe {
        let Some(shot) = clipboard_shot() else {
            return false;
        };
        if attach_count() >= MAX_CHIPS {
            println!("[ask] attachment limit reached, paste ignored");
            return true;
        }
        println!("[ask] image attached ({}x{})", shot.w, shot.h);
        PENDING_ATTACH.lock().unwrap().push(Pending::Image(shot));
        relayout(parent);
        true
    }
}

/// Reads a picture off the clipboard, if what is on it is one.
fn clipboard_shot() -> Option<Shot> {
    let image = arboard::Clipboard::new()
        .and_then(|mut cb| cb.get_image())
        .ok()?;
    let (w, h) = (image.width as u32, image.height as u32);
    let mut bgra = image.bytes.into_owned();
    if w == 0 || h == 0 || bgra.len() < w as usize * h as usize * 4 {
        return None;
    }

    // The clipboard hands over RGBA; GDI and the PNG encoder both want BGRA.
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    // A screenshot copied as a DIB usually arrives with every alpha byte zero,
    // which the encoder would take at its word and produce a fully transparent
    // picture from — an image of nothing, sent to be looked at.  Nothing on a
    // screenshot is meant to be see-through, so an all-zero alpha channel is a
    // missing one, not an empty one.
    if bgra.chunks_exact(4).all(|px| px[3] == 0) {
        for px in bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }

    Some(Shot { bgra, w, h })
}

/// Flips web search on or off and remembers it.
///
/// Saved to disk rather than kept for the session: the setting is a preference
/// about how answers should look, and having it revert every time the window
/// closes would mean setting it again all day.
unsafe fn toggle_web(hwnd: HWND) {
    unsafe {
        let mut cfg = settings::current();
        cfg.web_search = !cfg.web_search;
        println!("[ask] web search {}", if cfg.web_search { "on" } else { "off" });
        settings::save(&cfg);
        settings::set_current(cfg);
        let rc = globe_rect(attached());
        let _ = InvalidateRect(hwnd, Some(&rc), false);
    }
}

/// Opens a native file picker on an STA worker thread and posts the chosen path
/// back to the window to decode and attach.
///
/// The picker must run in a single-threaded apartment, and the app's main thread
/// is MTA — so, like the settings folder browser, it runs on its own thread and
/// hands the result back through a window message.
fn attach_file(hwnd: HWND) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{
            CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        };
        use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
        use windows::Win32::UI::Shell::{
            FOS_ALLOWMULTISELECT, FileOpenDialog, IFileOpenDialog, IShellItemArray,
            SIGDN_FILESYSPATH,
        };

        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            let picked: Option<Vec<String>> = (|| {
                let dialog: IFileOpenDialog =
                    CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
                let name = to_wide("All files");
                let spec = to_wide("*.*");
                let filters = [COMDLG_FILTERSPEC {
                    pszName: PCWSTR(name.as_ptr()),
                    pszSpec: PCWSTR(spec.as_ptr()),
                }];
                let _ = dialog.SetFileTypes(&filters);
                // Let the user pick several files in one go.
                if let Ok(opts) = dialog.GetOptions() {
                    let _ = dialog.SetOptions(opts | FOS_ALLOWMULTISELECT);
                }
                // Owned by the ask window, so the dialog sits above it even
                // though that window is topmost.
                dialog.Show(HWND(target as *mut _)).ok()?;

                let items: IShellItemArray = dialog.GetResults().ok()?;
                let count = items.GetCount().ok()?;
                let mut paths = Vec::new();
                for i in 0..count {
                    let Ok(item) = items.GetItemAt(i) else {
                        continue;
                    };
                    let Ok(pwstr) = item.GetDisplayName(SIGDN_FILESYSPATH) else {
                        continue;
                    };
                    if let Ok(s) = pwstr.to_string() {
                        paths.push(s);
                    }
                    CoTaskMemFree(Some(pwstr.0 as *const _));
                }
                (!paths.is_empty()).then_some(paths)
            })();

            if let Some(paths) = picked {
                let boxed: *mut Vec<String> = Box::into_raw(Box::new(paths));
                let owner = HWND(target as *mut _);
                if !IsWindow(owner).as_bool()
                    || PostMessageW(owner, WM_APP_ATTACH_FILE, WPARAM(0), LPARAM(boxed as isize))
                        .is_err()
                {
                    drop(Box::from_raw(boxed));
                }
            }

            CoUninitialize();
        }
    });
}

/// Why a path couldn't become an attachment.
enum BuildErr {
    TooBig,
    Failed,
}

/// Reads `path` into an attachment: an image is decoded to pixels so it shows a
/// thumbnail; anything else rides along as raw bytes with a MIME type.  No UI —
/// the caller batches several of these and repaints once.
fn build_pending(path: &str) -> Result<Pending, BuildErr> {
    let ext = extension_of(path);

    // Check the size on disk *before* reading a byte.  A huge file read whole
    // into memory would freeze the caller's thread long enough for Windows to
    // kill the app as unresponsive — which is exactly what a big JSON did.
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize;
    let cap = if is_image_ext(&ext) {
        MAX_IMAGE_FILE
    } else {
        MAX_ATTACH
    };
    if size > cap {
        println!("[!] [ask] file too large: {size} bytes ({path})");
        return Err(BuildErr::TooBig);
    }

    if is_image_ext(&ext) {
        match crate::screenshot::decode_to_bgra(path) {
            Ok((bgra, w, h)) => {
                println!("[ask] image file attached: {path} ({w}x{h})");
                Ok(Pending::Image(Shot { bgra, w, h }))
            }
            Err(e) => {
                println!("[!] [ask] decode {path}: {e}");
                Err(BuildErr::Failed)
            }
        }
    } else {
        let data = std::fs::read(path).map_err(|e| {
            println!("[!] [ask] read {path}: {e}");
            BuildErr::Failed
        })?;
        let mime = mime_of(&ext);
        println!(
            "[ask] file attached: {path} ({} KB, {mime})",
            data.len() / 1024
        );
        Ok(Pending::File {
            data: Arc::new(data),
            mime,
            ext: if ext.is_empty() {
                "FILE".to_string()
            } else {
                ext.to_uppercase()
            },
            name: std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string(),
        })
    }
}

/// The outcome of reading a batch of files, handed from the build worker back
/// to the window thread.
struct AttachResult {
    items: Vec<Pending>,
    too_big: bool,
}

/// Reads and decodes the chosen files off the window thread, then posts the
/// finished attachments back for the window to drop in — so nothing heavy ever
/// runs where a repaint is waiting on it.
fn build_and_post(hwnd: HWND, paths: Vec<String>) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        // The image decoder is WinRT and blocks on `.get()`; the multithreaded
        // apartment is where that is safe.
        unsafe {
            use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let mut items = Vec::new();
        let mut too_big = false;
        for path in paths {
            if items.len() >= MAX_CHIPS {
                break;
            }
            match build_pending(&path) {
                Ok(p) => items.push(p),
                Err(BuildErr::TooBig) => too_big = true,
                Err(BuildErr::Failed) => {}
            }
        }

        let boxed: *mut AttachResult = Box::into_raw(Box::new(AttachResult { items, too_big }));
        let owner = HWND(target as *mut _);
        unsafe {
            if !IsWindow(owner).as_bool()
                || PostMessageW(owner, WM_APP_ATTACH_READY, WPARAM(0), LPARAM(boxed as isize))
                    .is_err()
            {
                drop(Box::from_raw(boxed));
            }
        }
    });
}

/// The lowercase extension of `path`, without the dot.
fn extension_of(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp")
}

/// A best-effort MIME type from the extension.  Anything unrecognised goes as
/// octet-stream — Gemini will reject a type it can't read, and that error is
/// shown as-is, which beats silently pretending it worked.
fn mime_of(ext: &str) -> String {
    let m = match ext {
        "pdf" => "application/pdf",
        "json" => "application/json",
        "xml" => "text/xml",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "txt" | "log" | "md" | "markdown" | "rtf" | "rs" | "py" | "js" | "ts" | "c" | "cc"
        | "cpp" | "h" | "hpp" | "java" | "go" | "rb" | "php" | "cs" | "sh" | "bat" | "ps1"
        | "ini" | "cfg" | "conf" | "toml" | "yaml" | "yml" | "sql" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    };
    m.to_string()
}

/// Puts the input line where the current attachment state wants it.
unsafe fn place_input(hwnd: HWND) {
    unsafe {
        if let Ok(input) = GetDlgItem(hwnd, IDC_QUESTION) {
            let rc = input_rect(attach_count());
            let _ = MoveWindow(
                input,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                true,
            );
        }
    }
}

/// Reacts to a picture arriving or leaving: the input shifts across, the
/// window grows or shrinks by the height of a thumbnail, the panel repaints.
unsafe fn relayout(hwnd: HWND) {
    unsafe {
        place_input(hwnd);
        fit_to_content(hwnd);
        let _ = InvalidateRect(hwnd, None, true);
    }
}

/// The client point a mouse message carries: two signed 16-bit halves, y above x.
fn unpack_point(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, (lp.0 >> 16) as i16 as i32)
}

fn in_rect(x: i32, y: i32, rc: RECT) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

unsafe fn focus_input(hwnd: HWND) {
    unsafe {
        if let Ok(ctrl) = GetDlgItem(hwnd, IDC_QUESTION) {
            let _ = SetFocus(ctrl);
        }
    }
}

unsafe fn read_text(parent: HWND, id: i32) -> String {
    unsafe {
        let Ok(ctrl) = GetDlgItem(parent, id) else {
            return String::new();
        };
        let len = GetWindowTextLengthW(ctrl) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 2];
        let got = GetWindowTextW(ctrl, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

unsafe fn set_text(parent: HWND, id: i32, text: &str) {
    unsafe {
        let Ok(ctrl) = GetDlgItem(parent, id) else {
            return;
        };
        let wide = to_wide(text);
        let _ = SetWindowTextW(ctrl, PCWSTR(wide.as_ptr()));
    }
}
