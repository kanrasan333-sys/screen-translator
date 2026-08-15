//! "Ask the model" — a single input line in the middle of the screen.
//!
//! The whole window is one field until there is something to show: type,
//! press Enter, and the answer unfolds underneath while the input stays
//! exactly where it was.  No caption, no buttons, no chrome — it is summoned
//! by a hotkey and dismissed with Escape, so anything else is furniture.
//!
//! The height follows the answer: it is measured in wrapped lines and clamped,
//! so a one-word reply doesn't leave a half-empty panel hanging on screen and
//! a long one scrolls instead of running off the bottom.
//!
//! The conversation lives only as long as the window does.  Reopening starts
//! clean, which is what you want from something bound to a hotkey.

use crate::deepseek;
use crate::i18n;
use crate::settings;
use crate::theme;
use crate::utils::to_wide;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{SetWindowTheme, ShowScrollBar};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL, VK_SHIFT};
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

/// A popup class asks for the system's drop shadow with this style.
const CS_DROPSHADOW: WNDCLASS_STYLES = WNDCLASS_STYLES(0x0002_0000);

/// DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
const DT_LINE: u32 = 0x0824;

const IDC_ANSWER: i32 = 401;
const IDC_QUESTION: i32 = 402;

/// Posted by the worker thread once a reply (or an error) is waiting.
const WM_APP_REPLY: u32 = WM_APP + 11;

/// Posted when a copied selection turns up after the window was already shown.
const WM_APP_PREFILL: u32 = WM_APP + 12;

/// Drives the unfold.  `WM_TIMER` is floored by the system timer resolution —
/// asking for 8 ms measures out at about 15, so this runs near 60 fps whatever
/// is requested.  Raising the global timer resolution to close that gap costs
/// every other process on the machine battery for a 200 ms movement.
const ANIM_TIMER: usize = 1;
const ANIM_TICK_MS: u32 = 8;

/// Share of the remaining distance covered each tick.  Exponential ease-out:
/// the panel leaves quickly and settles, which reads as movement rather than
/// as a window being resized.
const ANIM_EASE: f32 = 0.34;

// ============================================================
// Layout
// ============================================================

mod layout {
    /// Wide enough for a sentence of answer without the eye tracking back,
    /// narrow enough to still read as a prompt rather than a document.
    pub const WIN_W: i32 = 720;
    pub const PAD_X: i32 = 20;

    /// The input line and the air around it — the whole window until an
    /// answer arrives.
    pub const INPUT_H: i32 = 34;
    pub const PAD_Y: i32 = 15;
    pub const HEAD_H: i32 = INPUT_H + PAD_Y * 2;

    /// Hairline under the input, then the answer.
    pub const ANSWER_TOP: i32 = HEAD_H + 1 + 12;
    pub const ANSWER_MIN_H: i32 = 26;
    pub const ANSWER_MAX_H: i32 = 430;

    pub const HINT_GAP: i32 = 10;
    pub const HINT_H: i32 = 15;
    pub const BOTTOM: i32 = 14;

    pub const CORNER: i32 = 12;

    /// Where the collapsed box sits vertically, as a fraction of the work
    /// area.  A third of the way down rather than halfway: dead centre reads
    /// as low once the answer unfolds beneath it, and it is where every
    /// summoned launcher has sat since Spotlight.
    pub const ANCHOR_NUM: i32 = 1;
    pub const ANCHOR_DEN: i32 = 3;

    /// Total window height for an answer pane `answer_h` tall.
    pub const fn expanded(answer_h: i32) -> i32 {
        ANSWER_TOP + answer_h + HINT_GAP + HINT_H + BOTTOM
    }
}

fn input_rect() -> RECT {
    use layout::*;
    RECT {
        left: PAD_X,
        top: PAD_Y,
        right: WIN_W - PAD_X,
        bottom: PAD_Y + INPUT_H,
    }
}

fn answer_rect(answer_h: i32) -> RECT {
    use layout::*;
    RECT {
        left: PAD_X,
        top: ANSWER_TOP,
        right: WIN_W - PAD_X,
        bottom: ANSWER_TOP + answer_h,
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

/// The running conversation.  The system turn is prepended at send time rather
/// than stored, so a language change between questions takes effect.
static HISTORY: Mutex<Vec<(Who, String)>> = Mutex::new(Vec::new());

/// Reply (or error text) handed over by the worker thread.
static PENDING: Mutex<Option<Result<String, String>>> = Mutex::new(None);

/// Set while a request is in flight, so a second Enter doesn't stack calls.
static BUSY: Mutex<bool> = Mutex::new(false);

/// A selection that finished copying only after the window was up.
static PENDING_PREFILL: Mutex<Option<String>> = Mutex::new(None);

/// Height the window is currently travelling towards, while the unfold runs.
static ANIM_TARGET: Mutex<i32> = Mutex::new(0);

static ASK_HWND: Mutex<isize> = Mutex::new(0);

/// Whether losing focus should dismiss the window.  Off until it is fully up,
/// so the activation churn of showing it doesn't close it immediately.
static DISMISS_ON_BLUR: Mutex<bool> = Mutex::new(false);

struct Resources {
    panel_brush: isize,
    font_input: isize,
    font_body: isize,
    font_hint: isize,
    /// Height of one wrapped line of `font_body`, measured once.  The window
    /// grows in multiples of this.
    line_h: i32,
}

impl Resources {
    fn new() -> Self {
        unsafe {
            let font_body = make_font(-14, 400);
            Self {
                panel_brush: CreateSolidBrush(COLORREF(theme::CLR_CARD)).0 as isize,
                font_input: make_font(-20, 400).0 as isize,
                font_body: font_body.0 as isize,
                font_hint: make_font(-11, 400).0 as isize,
                line_h: line_height(font_body),
            }
        }
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
        *PENDING.lock().unwrap() = None;
        *PENDING_PREFILL.lock().unwrap() = None;
        *BUSY.lock().unwrap() = false;
        *DISMISS_ON_BLUR.lock().unwrap() = false;

        let Some(hmodule) = GetModuleHandleW(None).ok() else {
            return;
        };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransAsk2");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(ask_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: res().as_ref().unwrap().panel_brush(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Horizontally centred on the work area, vertically a third of the
        // way down it.  The taskbar is not space the window can use, so the
        // work area is what both are measured against.  The top edge then
        // stays put when an answer arrives, so the input never jumps out from
        // under the cursor.
        let (x, y) = anchored_origin(layout::WIN_W, layout::HEAD_H);
        let title = to_wide(i18n::t("ask.title"));

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            layout::WIN_W,
            layout::HEAD_H,
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
        round_corners(hwnd, layout::HEAD_H);
        create_controls(hwnd, hinstance);

        // Say up front when there is no key, rather than after a question has
        // been typed and thrown away.
        if settings::current().deepseek_api_key.trim().is_empty() {
            set_answer(hwnd, i18n::t("ask.no_key"));
            fit_to_content(hwnd);
        }

        put_question(hwnd, prefill.trim());

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        focus_input(hwnd);
        // Only now: `SetForegroundWindow` can bounce activation around while
        // the window is coming up, and a WM_ACTIVATE in the middle of that
        // would close it before it was ever seen.
        *DISMISS_ON_BLUR.lock().unwrap() = true;
    }
}

/// Top-left corner placing a `width` x `height` window horizontally centred and
/// vertically a third of the way down the work area of the monitor the pointer
/// is on.
unsafe fn anchored_origin(width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);

        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let wa = mi.rcWork;
            (
                wa.left + (wa.right - wa.left - width) / 2,
                anchor_y(wa.top, wa.bottom - wa.top, height),
            )
        } else {
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            ((sw - width) / 2, anchor_y(0, sh, height))
        }
    }
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
            &input_rect(),
            IDC_QUESTION,
            ES_MULTILINE | ES_AUTOVSCROLL,
        );

        let answer = create_edit(
            parent,
            hinst,
            r.font_body(),
            &answer_rect(layout::ANSWER_MIN_H),
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
        let mut text = to_wide(i18n::t("ask.placeholder"));
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

        let question = read_text(hwnd, IDC_QUESTION).trim().to_string();
        if question.is_empty() {
            return;
        }

        let key = settings::current().deepseek_api_key.trim().to_string();
        if key.is_empty() {
            println!("[ask] no API key configured");
            set_answer(hwnd, i18n::t("ask.no_key"));
            fit_to_content(hwnd);
            return;
        }
        println!("[ask] asking ({} chars)", question.chars().count());

        set_text(hwnd, IDC_QUESTION, "");
        HISTORY.lock().unwrap().push((Who::User, question));
        *BUSY.lock().unwrap() = true;
        render_transcript(hwnd, Some(i18n::t("ask.thinking")));

        // Answer in the UI language: the window is opened from a hotkey with
        // no chance to say "reply in Ukrainian" every time.
        let system = format!(
            "You are a concise, knowledgeable assistant. Answer in {}. \
             Be direct and specific; skip pleasantries and restating the question. \
             Use plain text — no Markdown syntax, since the answer is shown in a \
             plain text box.",
            i18n::current().native_name()
        );

        let mut turns = vec![deepseek::Turn::system(system)];
        for (who, text) in HISTORY.lock().unwrap().iter() {
            turns.push(match who {
                Who::User => deepseek::Turn::user(text.clone()),
                Who::Model => deepseek::Turn::assistant(text.clone()),
            });
        }

        let target = hwnd.0 as isize;
        std::thread::spawn(move || {
            println!("[ask] request sent, {} turns", turns.len());
            let result = deepseek::chat(&key, &turns, 0.7, 120).map_err(|e| e.to_string());
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

unsafe fn take_reply(hwnd: HWND) {
    unsafe {
        let Some(result) = PENDING.lock().unwrap().take() else {
            return;
        };
        *BUSY.lock().unwrap() = false;

        match result {
            Ok(reply) => {
                HISTORY.lock().unwrap().push((Who::Model, reply));
                render_transcript(hwnd, None);
            }
            Err(e) => {
                // Drop the unanswered question from the history — it must not
                // go out as context next time — but hand it back to the input
                // so a network blip doesn't cost the user their typing.
                let failed = HISTORY.lock().unwrap().pop();
                if let Some((Who::User, question)) = failed {
                    set_text(hwnd, IDC_QUESTION, &question);
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
        for (who, text) in HISTORY.lock().unwrap().iter() {
            if !out.is_empty() {
                out.push_str("\r\n\r\n");
            }
            if *who == Who::User {
                // Quoted, so a question is never mistaken for an answer.
                out.push_str("> ");
                out.push_str(&text.replace('\n', "\n> "));
            } else {
                out.push_str(text);
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
/// The top edge never moves.  Re-centring on every reply would drag the input
/// out from under the cursor mid-conversation.
unsafe fn fit_to_content(hwnd: HWND) {
    unsafe {
        let Ok(answer) = GetDlgItem(hwnd, IDC_ANSWER) else {
            return;
        };

        let height = if GetWindowTextLengthW(answer) == 0 {
            let _ = ShowWindow(answer, SW_HIDE);
            layout::HEAD_H
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

            let rc = answer_rect(answer_h);
            let _ = MoveWindow(
                answer,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                true,
            );
            let _ = ShowWindow(answer, SW_SHOW);
            layout::expanded(answer_h)
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

/// Puts the window at `height` immediately, region and all.
unsafe fn set_height(hwnd: HWND, height: i32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            layout::WIN_W,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        round_corners(hwnd, height);
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

/// Rounds the window itself with a region.  `DWMWA_WINDOW_CORNER_PREFERENCE`
/// would be smoother but is Windows 11 only, and a region is the one approach
/// that still lets the window host real child controls.
unsafe fn round_corners(hwnd: HWND, height: i32) {
    unsafe {
        let rgn = CreateRoundRectRgn(
            0,
            0,
            layout::WIN_W + 1,
            height + 1,
            layout::CORNER * 2,
            layout::CORNER * 2,
        );
        // The window owns the region after this call; it must not be deleted.
        let _ = SetWindowRgn(hwnd, rgn, true);
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

            WM_TIMER if wp.0 == ANIM_TIMER => {
                step_unfold(hwnd);
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
                    }
                }
                LRESULT(0)
            }

            // Clicking into any other window dismisses it, the way a
            // summoned panel should.  WA_INACTIVE is the low word being 0.
            WM_ACTIVATE => {
                if wp.0 & 0xFFFF == 0 && *DISMISS_ON_BLUR.lock().unwrap() {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }

            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }

            WM_DESTROY => {
                let _ = KillTimer(hwnd, ANIM_TIMER);
                *DISMISS_ON_BLUR.lock().unwrap() = false;
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

        // Everything below only exists once there is an answer to separate.
        if rc.bottom > layout::HEAD_H {
            let sep = RECT {
                left: 0,
                top: layout::HEAD_H,
                right: rc.right,
                bottom: layout::HEAD_H + 1,
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
