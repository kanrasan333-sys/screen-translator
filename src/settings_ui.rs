//! Settings window — Win32 UI with a custom dark title bar, hand-drawn
//! input borders, and hover/focus aware buttons.
//!
//! Architecture:
//! * `open()` registers a window class and creates a `WS_POPUP` (no native
//!   chrome) — all chrome is painted by us in `WM_PAINT` / WM_NCHITTEST.
//! * Geometry lives in the `layout::*` constants near the top of the file;
//!   to reshape the window, tweak those and nothing else should break.
//! * `Resources` lazily caches brushes/fonts that the window needs for
//!   repeated paints — avoids leaking GDI objects that the old code
//!   created on every `WM_DRAWITEM`.
//! * The window proc dispatches to small, single-purpose handlers
//!   (`paint`, `draw_owner_button`, `hit_test`, `on_command`).

use crate::autostart;
use crate::button;
use crate::i18n::{self, Language};
use crate::paint;
use crate::settings::{self, HotkeyConfig, Settings};
use crate::theme::{self, darken, lighten};
use crate::utils::to_wide;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ============================================================
// Named Win32 style constants (was scattered magic hex)
// ============================================================

const BS_OWNERDRAW: WINDOW_STYLE = WINDOW_STYLE(0x000B);
const ES_AUTOHSCROLL: WINDOW_STYLE = WINDOW_STYLE(0x0080);
/// Vertically centres a single line of static text in its rect.
const SS_CENTERIMAGE: WINDOW_STYLE = WINDOW_STYLE(0x0200);
/// Truncates with "…" instead of clipping mid-glyph.
const SS_ENDELLIPSIS: WINDOW_STYLE = WINDOW_STYLE(0x4000);

const EM_SETMARGINS: u32 = 0x00D3;

// Custom messages for our hand-rolled hotkey / language controls.
// WPARAM/LRESULT both encode (vk | mods << 16) for hotkeys, and a raw index
// into Language::all() for the language combo.
const HK_MSG_GET: u32 = WM_USER + 100;
const HK_MSG_SET: u32 = WM_USER + 101;
const LANG_MSG_GET: u32 = WM_USER + 200;
const LANG_MSG_SET: u32 = WM_USER + 201;

// ============================================================
// Control IDs
// ============================================================

const IDC_HK_TRANSLATE: i32 = 101;
const IDC_HK_OCR: i32 = 102;
const IDC_HK_SCREENSHOT: i32 = 103;
const IDC_HK_LAYOUT: i32 = 104;
const IDC_HK_EXPLORER_CMD: i32 = 105;
const IDC_EDIT_FOLDER: i32 = 106;
const IDC_BTN_BROWSE: i32 = 107;
const IDC_BTN_SAVE: i32 = 108;
const IDC_BTN_CANCEL: i32 = 109;
const IDC_CHK_PUNTO: i32 = 110;
const IDC_CHK_TASKBAR: i32 = 111;
const IDC_CHK_AUTOSTART: i32 = 112;
const IDC_COMBO_LANG: i32 = 114;
const IDC_CHK_EXPLORER_CMD: i32 = 115;
const IDC_EDIT_DEEPSEEK: i32 = 116;
const IDC_HK_ASK: i32 = 117;

// Static-text ids come in ranges, because `WM_CTLCOLORSTATIC` has nothing but
// the id to decide what a label is: a group title on the window background, a
// footnote under a card, or a row label sitting on the card itself.
const IDC_GROUP_TITLE: i32 = 200; // 200..205, one per group
const IDC_FOOTNOTE: i32 = 250;
const IDC_ROW_LABEL: i32 = 300; // 300.., one per row label

/// What we currently know about the configured DeepSeek key.  Checking runs
/// on a worker thread; the window is told to repaint when it lands.
#[derive(Clone, PartialEq, Eq)]
enum KeyStatus {
    Unset,
    Checking,
    Valid,
    Rejected,
    Unreachable,
}

static KEY_STATUS: Mutex<KeyStatus> = Mutex::new(KeyStatus::Unset);
/// The key the current status refers to, so re-checking only happens when the
/// text actually changed.
static KEY_CHECKED: Mutex<String> = Mutex::new(String::new());

/// Posted by the key-checking thread when it has an answer.
const WM_APP_KEY_STATUS: u32 = WM_APP + 2;

// Private window message: the worker thread running the folder-picker
// posts the chosen path back to the settings window via this message.
// LPARAM carries a `Box::into_raw(Box<String>)` pointer, the handler
// converts it back via Box::from_raw and frees it.
const WM_APP_BROWSE_RESULT: u32 = WM_APP + 1;

// ============================================================
// Layout — all geometry in one place.  Dimensions in px.
// ============================================================

mod layout {
    pub const WIN_W: i32 = 520;
    pub const MARGIN: i32 = 20;
    pub const CARD_W: i32 = WIN_W - MARGIN * 2;
    pub const CARD_R: i32 = 10;

    /// One row of a grouped list.
    pub const ROW_H: i32 = 42;
    /// Breathing room inside a row — and how far the hairline between two rows
    /// stops short of the left edge, the way grouped lists indent theirs.
    pub const ROW_PAD: i32 = 14;

    /// Secondary-colour title sitting above each card.
    pub const TITLE_H: i32 = 18;
    pub const TITLE_GAP: i32 = 6;
    /// Between one card and the next group's title.
    pub const GROUP_GAP: i32 = 16;
    /// Explanatory line under a card.
    pub const FOOTNOTE_GAP: i32 = 7;
    pub const FOOTNOTE_H: i32 = 32;

    pub const TOP: i32 = 16;
    pub const BOTTOM: i32 = 18;

    /// Height of a control sitting in a row, and the width of the value column
    /// they all line up in.
    pub const CTRL_H: i32 = 28;
    pub const VALUE_W: i32 = 188;

    pub const SWITCH_W: i32 = 40;
    pub const SWITCH_H: i32 = 24;

    pub const BTN_W: i32 = 96;
    pub const BTN_H: i32 = 30;
    pub const BTN_GAP: i32 = 10;
    pub const BROWSE_W: i32 = 96;
}

/// Group indices, so call sites read as English rather than magic numbers.
const G_GENERAL: usize = 0;
const G_TRANSLATION: usize = 1;
const G_SHORTCUTS: usize = 2;
const G_SCREENSHOTS: usize = 3;
const G_FEATURES: usize = 4;

/// Rows per group, and whether a footnote follows the card.  Translation has
/// two: the key, and whether that key actually works.
const GROUP_SPEC: [(usize, bool); 5] = [(1, false), (2, true), (6, false), (1, false), (4, false)];

/// One group: a title, a rounded card, and the rows inside it.
struct Group {
    title_y: i32,
    card: RECT,
    rows: usize,
    footnote_y: Option<i32>,
}

impl Group {
    fn row_top(&self, i: usize) -> i32 {
        self.card.top + i as i32 * layout::ROW_H
    }

    /// Vertically centred slot of height `h`, right-aligned inside row `i`.
    fn trailing(&self, i: usize, w: i32, h: i32) -> RECT {
        let top = self.row_top(i) + (layout::ROW_H - h) / 2;
        RECT {
            left: self.card.right - layout::ROW_PAD - w,
            top,
            right: self.card.right - layout::ROW_PAD,
            bottom: top + h,
        }
    }

    /// Label slot in row `i`, running from the left padding up to `right`.
    fn leading(&self, i: usize, right: i32) -> RECT {
        RECT {
            left: self.card.left + layout::ROW_PAD,
            top: self.row_top(i),
            right,
            bottom: self.row_top(i) + layout::ROW_H,
        }
    }
}

/// Whole-window geometry, computed in one place so control creation, painting
/// and hit-testing can't drift apart as groups gain or lose rows.
struct Page {
    groups: Vec<Group>,
    buttons_y: i32,
    height: i32,
}

fn page() -> Page {
    use layout::*;

    let mut y = TOP;
    let mut groups = Vec::with_capacity(GROUP_SPEC.len());

    for (rows, has_footnote) in GROUP_SPEC {
        let title_y = y;
        y += TITLE_H + TITLE_GAP;

        let card = RECT {
            left: MARGIN,
            top: y,
            right: MARGIN + CARD_W,
            bottom: y + ROW_H * rows as i32,
        };
        y = card.bottom;

        let mut footnote_y = None;
        if has_footnote {
            let fy = y + FOOTNOTE_GAP;
            footnote_y = Some(fy);
            y = fy + FOOTNOTE_H;
        }
        y += GROUP_GAP;

        groups.push(Group {
            title_y,
            card,
            rows,
            footnote_y,
        });
    }

    let buttons_y = y;
    Page {
        groups,
        buttons_y,
        height: buttons_y + BTN_H + BOTTOM,
    }
}

// ============================================================
// Local palette (everything else comes from theme::*)
// ============================================================

/// The current choice in the open language list; the hovered row takes the
/// accent instead.
const CLR_ROW_SELECTED: u32 = 0x0032_3232;

/// Corner radius of a text field, a hotkey field or a popup list.
const FIELD_R: i32 = 6;

// Status colours, matching macOS dark-appearance system green/red/orange.
const CLR_GREEN: u32 = 0x0058_D130;
const CLR_RED: u32 = 0x003A_45FF;
const CLR_ORANGE: u32 = 0x000A_9FFF;

// ============================================================
// Resources — GDI objects cached for the lifetime of the window.
// ============================================================

struct Resources {
    bg_brush: isize,
    card_brush: isize,
    field_brush: isize,
    font_body: isize,
    font_group: isize,
    font_meta: isize,
    font_button: isize,
}

impl Resources {
    fn new() -> Self {
        unsafe {
            Self {
                bg_brush: CreateSolidBrush(COLORREF(theme::CLR_BG)).0 as isize,
                card_brush: CreateSolidBrush(COLORREF(theme::CLR_CARD)).0 as isize,
                field_brush: CreateSolidBrush(COLORREF(theme::CLR_FIELD)).0 as isize,
                font_body: make_font(-14, 400).0 as isize,
                // Group titles carry structure by colour and position, not by
                // shouting — no caps, no bold.
                font_group: make_font(-13, 500).0 as isize,
                font_meta: make_font(-12, 400).0 as isize,
                // AppKit button labels sit near regular weight.
                font_button: make_font(-13, 500).0 as isize,
            }
        }
    }
    fn bg_brush(&self) -> HBRUSH {
        HBRUSH(self.bg_brush as *mut _)
    }
    fn card_brush(&self) -> HBRUSH {
        HBRUSH(self.card_brush as *mut _)
    }
    fn field_brush(&self) -> HBRUSH {
        HBRUSH(self.field_brush as *mut _)
    }
    fn font_body(&self) -> HFONT {
        HFONT(self.font_body as *mut _)
    }
    fn font_group(&self) -> HFONT {
        HFONT(self.font_group as *mut _)
    }
    fn font_meta(&self) -> HFONT {
        HFONT(self.font_meta as *mut _)
    }
    fn font_button(&self) -> HFONT {
        HFONT(self.font_button as *mut _)
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

// ============================================================
// Window-level state
// ============================================================

static SETTINGS_HWND: Mutex<isize> = Mutex::new(0);
static UPDATED_SETTINGS: Mutex<Option<Box<Settings>>> = Mutex::new(None);
static RES: Mutex<Option<Box<Resources>>> = Mutex::new(None);

fn res() -> std::sync::MutexGuard<'static, Option<Box<Resources>>> {
    RES.lock().unwrap()
}

fn ensure_res() {
    let mut g = RES.lock().unwrap();
    if g.is_none() {
        *g = Some(Box::new(Resources::new()));
    }
}

// ============================================================
// Public API
// ============================================================

pub fn open(current: &Settings) {
    unsafe {
        // Reuse existing window if still alive.
        let v = *SETTINGS_HWND.lock().unwrap();
        if v != 0 {
            let hwnd = HWND(v as *mut _);
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }

        ensure_res();

        let Some(hmodule) = GetModuleHandleW(None).ok() else {
            return;
        };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransSettings7");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(settings_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: res().as_ref().unwrap().bg_brush(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let title = to_wide(i18n::t("settings.title"));
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
        let ex_style = WS_EX_TOOLWINDOW | WS_EX_TOPMOST;

        // AdjustWindowRectEx expands the client rect we want into the
        // total window rect (client + caption + borders).  That way the
        // content inside the window is exactly WIN_H_CLIENT tall.
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: layout::WIN_W,
            bottom: page().height,
        };
        let _ = AdjustWindowRectEx(&mut rc, style, false, ex_style);
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;

        let hwnd = CreateWindowExW(
            ex_style,
            class,
            PCWSTR(title.as_ptr()),
            style,
            (sw - win_w) / 2,
            (sh - win_h) / 2,
            win_w,
            win_h,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap_or_default();

        if hwnd.0.is_null() {
            return;
        }

        *SETTINGS_HWND.lock().unwrap() = hwnd.0 as isize;
        theme::dark_titlebar(hwnd);
        create_controls(hwnd, hinstance, current);
        start_key_check(hwnd, &current.deepseek_api_key);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn take_updated_settings() -> Option<Settings> {
    UPDATED_SETTINGS.lock().unwrap().take().map(|b| *b)
}

// ============================================================
// Controls
// ============================================================

unsafe fn create_controls(parent: HWND, hinst: HINSTANCE, s: &Settings) {
    unsafe {
        use layout::*;
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let page = page();
        let mut label_id = IDC_ROW_LABEL;

        // Group titles.
        for (i, key) in [
            "settings.section.general",
            "settings.section.translation",
            "settings.section.hotkeys",
            "settings.section.folder",
            "settings.section.functions",
        ]
        .into_iter()
        .enumerate()
        {
            create_label(
                parent,
                hinst,
                r.font_group(),
                MARGIN + 2,
                page.groups[i].title_y,
                CARD_W,
                TITLE_H,
                i18n::t(key),
                IDC_GROUP_TITLE + i as i32,
                LabelKind::GroupTitle,
            );
        }

        // ── General: interface language ──
        let g = &page.groups[G_GENERAL];
        let combo = g.trailing(0, VALUE_W, CTRL_H);
        row_label(
            parent,
            hinst,
            r,
            g,
            0,
            combo.left - 12,
            i18n::t("settings.label.language"),
            &mut label_id,
        );
        create_lang_combo(
            parent,
            hinst,
            r.font_body(),
            combo.left,
            combo.top,
            VALUE_W,
            &s.language,
        );

        // ── Translation: DeepSeek key ──
        let g = &page.groups[G_TRANSLATION];
        let field = g.trailing(0, VALUE_W, CTRL_H);
        row_label(
            parent,
            hinst,
            r,
            g,
            0,
            field.left - 12,
            i18n::t("settings.label.deepseek_key"),
            &mut label_id,
        );
        create_edit_field(
            parent,
            hinst,
            r.font_body(),
            field.left,
            field.top,
            VALUE_W,
            CTRL_H,
            &s.deepseek_api_key,
            IDC_EDIT_DEEPSEEK,
        );
        row_label(
            parent,
            hinst,
            r,
            g,
            1,
            g.card.right - ROW_PAD - VALUE_W,
            i18n::t("settings.label.key_status"),
            &mut label_id,
        );
        create_label(
            parent,
            hinst,
            r.font_meta(),
            MARGIN + 2,
            g.footnote_y.unwrap_or(0),
            CARD_W - 4,
            FOOTNOTE_H,
            i18n::t("settings.hint.deepseek"),
            IDC_FOOTNOTE,
            LabelKind::Footnote,
        );

        // ── Shortcuts ──
        let g = &page.groups[G_SHORTCUTS];
        let hotkeys: &[(&str, i32, &HotkeyConfig)] = &[
            (
                i18n::t("settings.hotkey.translate"),
                IDC_HK_TRANSLATE,
                &s.hk_translate,
            ),
            (i18n::t("settings.hotkey.ocr"), IDC_HK_OCR, &s.hk_ocr),
            (
                i18n::t("settings.hotkey.screenshot"),
                IDC_HK_SCREENSHOT,
                &s.hk_screenshot,
            ),
            (
                i18n::t("settings.hotkey.layout"),
                IDC_HK_LAYOUT,
                &s.hk_layout,
            ),
            (
                i18n::t("settings.hotkey.explorer_cmd"),
                IDC_HK_EXPLORER_CMD,
                &s.hk_explorer_cmd,
            ),
            (i18n::t("settings.hotkey.ask"), IDC_HK_ASK, &s.hk_ask),
        ];
        for (i, &(label, id, hk)) in hotkeys.iter().enumerate() {
            let field = g.trailing(i, VALUE_W, CTRL_H);
            row_label(
                parent,
                hinst,
                r,
                g,
                i,
                field.left - 12,
                label,
                &mut label_id,
            );
            create_hotkey_field(
                parent,
                hinst,
                r.font_body(),
                field.left,
                field.top,
                VALUE_W,
                CTRL_H,
                id,
                hk,
            );
        }

        // ── Screenshots: folder path + Choose.  The group title already says
        // what the row is for, so it carries no label of its own. ──
        let g = &page.groups[G_SCREENSHOTS];
        let browse = g.trailing(0, BROWSE_W, CTRL_H);
        let folder_left = g.card.left + ROW_PAD;
        create_edit_field(
            parent,
            hinst,
            r.font_body(),
            folder_left,
            browse.top,
            browse.left - 10 - folder_left,
            CTRL_H,
            &s.screenshot_folder,
            IDC_EDIT_FOLDER,
        );
        create_od_button(
            parent,
            hinst,
            r.font_button(),
            browse.left,
            browse.top,
            BROWSE_W,
            CTRL_H,
            i18n::t("settings.btn.browse"),
            IDC_BTN_BROWSE,
        );

        // ── Features: one switch per row ──
        let g = &page.groups[G_FEATURES];
        let features: &[(&str, i32, bool)] = &[
            (
                i18n::t("settings.checkbox.punto"),
                IDC_CHK_PUNTO,
                s.punto_enabled,
            ),
            (
                i18n::t("settings.checkbox.taskbar"),
                IDC_CHK_TASKBAR,
                s.taskbar_center_enabled,
            ),
            (
                i18n::t("settings.checkbox.autostart"),
                IDC_CHK_AUTOSTART,
                autostart::is_enabled(),
            ),
            (
                i18n::t("settings.checkbox.explorer_cmd"),
                IDC_CHK_EXPLORER_CMD,
                crate::explorer_cmd::is_menu_enabled(),
            ),
        ];
        for (i, &(label, id, checked)) in features.iter().enumerate() {
            let sw = g.trailing(i, SWITCH_W, SWITCH_H);
            row_label(
                parent,
                hinst,
                r,
                g,
                i,
                sw.left - 12,
                label,
                &mut label_id,
            );
            create_switch(parent, hinst, sw.left, sw.top, id, checked);
        }

        // ── Cancel / Save, bottom right.  The default button goes last, the
        // way every macOS sheet puts it. ──
        let save_x = WIN_W - MARGIN - BTN_W;
        create_od_button(
            parent,
            hinst,
            r.font_button(),
            save_x - BTN_GAP - BTN_W,
            page.buttons_y,
            BTN_W,
            BTN_H,
            i18n::t("settings.btn.cancel"),
            IDC_BTN_CANCEL,
        );
        create_od_button(
            parent,
            hinst,
            r.font_button(),
            save_x,
            page.buttons_y,
            BTN_W,
            BTN_H,
            i18n::t("settings.btn.save"),
            IDC_BTN_SAVE,
        );
    }
}

/// A row's left-hand label.  Grouped-list labels carry no trailing colon — the
/// alignment already says the control on the right belongs to them.
#[allow(clippy::too_many_arguments)]
unsafe fn row_label(
    parent: HWND,
    hinst: HINSTANCE,
    r: &Resources,
    g: &Group,
    row: usize,
    right: i32,
    text: &str,
    next_id: &mut i32,
) {
    unsafe {
        let rc = g.leading(row, right);
        create_label(
            parent,
            hinst,
            r.font_body(),
            rc.left,
            rc.top,
            (rc.right - rc.left).max(0),
            rc.bottom - rc.top,
            strip_colon(text),
            *next_id,
            LabelKind::Row,
        );
        *next_id += 1;
    }
}

fn strip_colon(s: &str) -> &str {
    s.trim_end().trim_end_matches([':', '：']).trim_end()
}

// ============================================================
// DeepSeek key status
// ============================================================

/// Kicks off a key check unless the same key was already checked.  Runs on a
/// worker thread — the models endpoint costs no tokens but does cost a
/// round-trip, and the settings window must not freeze on it.
unsafe fn start_key_check(hwnd: HWND, key: &str) {
    let key = key.trim().to_string();

    if key.is_empty() {
        *KEY_STATUS.lock().unwrap() = KeyStatus::Unset;
        KEY_CHECKED.lock().unwrap().clear();
        unsafe { invalidate_key_status(hwnd) };
        return;
    }
    if *KEY_CHECKED.lock().unwrap() == key {
        return;
    }

    *KEY_CHECKED.lock().unwrap() = key.clone();
    *KEY_STATUS.lock().unwrap() = KeyStatus::Checking;
    unsafe { invalidate_key_status(hwnd) };

    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let status = match crate::deepseek::check_key(&key) {
            crate::deepseek::KeyCheck::Valid => KeyStatus::Valid,
            crate::deepseek::KeyCheck::Rejected => KeyStatus::Rejected,
            crate::deepseek::KeyCheck::Unreachable(e) => {
                println!("[!] DeepSeek key check: {e}");
                KeyStatus::Unreachable
            }
        };
        // A key typed after this check started owns the answer, not us.
        if *KEY_CHECKED.lock().unwrap() != key {
            return;
        }
        *KEY_STATUS.lock().unwrap() = status;
        unsafe {
            let hwnd = HWND(target as *mut _);
            if IsWindow(hwnd).as_bool() {
                let _ = PostMessageW(hwnd, WM_APP_KEY_STATUS, WPARAM(0), LPARAM(0));
            }
        }
    });
}

/// The status value is parent-painted (a coloured dot plus a word), so it has
/// no control of its own to invalidate.
unsafe fn invalidate_key_status(hwnd: HWND) {
    unsafe {
        let g = &page().groups[G_TRANSLATION];
        let rc = RECT {
            left: g.card.left + layout::ROW_PAD,
            top: g.row_top(1),
            right: g.card.right,
            bottom: g.row_top(1) + layout::ROW_H,
        };
        let _ = InvalidateRect(hwnd, Some(&rc), false);
    }
}

fn key_status_text() -> (&'static str, u32) {
    match *KEY_STATUS.lock().unwrap() {
        KeyStatus::Unset => ("settings.key.unset", theme::CLR_TEXT_DIM),
        KeyStatus::Checking => ("settings.key.checking", theme::CLR_TEXT_DIM),
        KeyStatus::Valid => ("settings.key.valid", CLR_GREEN),
        KeyStatus::Rejected => ("settings.key.rejected", CLR_RED),
        KeyStatus::Unreachable => ("settings.key.offline", CLR_ORANGE),
    }
}

/// Draws the status row's value: a dot in the state's colour, then the word.
/// Right-aligned to the same edge every other control in the card lines up on.
unsafe fn draw_key_status(hdc: HDC) {
    unsafe {
        use layout::*;
        let (key, color) = key_status_text();
        let text = i18n::t(key);

        let g = &page().groups[G_TRANSLATION];
        let cy = g.row_top(1) + ROW_H / 2;
        let right = g.card.right - ROW_PAD;

        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(hdc, r.font_body());

        let mut wide = to_wide(text);
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(color));
        let mut trc = RECT {
            left: right - size.cx,
            top: cy - size.cy / 2,
            right,
            bottom: cy + size.cy / 2 + 1,
        };
        DrawTextW(hdc, &mut wide, &mut trc, DRAW_TEXT_FORMAT(0x0820));
        SelectObject(hdc, old_font);
        drop(r_guard);

        const DOT: i32 = 8;
        let dot_right = trc.left - 8;
        paint::round_rect(
            hdc,
            &RECT {
                left: dot_right - DOT,
                top: cy - DOT / 2,
                right: dot_right,
                bottom: cy - DOT / 2 + DOT,
            },
            &paint::Style::flat(DOT / 2, color),
        );
    }
}


// ── Reusable control-creation helpers ──

/// What a piece of static text is, which decides its colour and what it sits
/// on — the only thing `WM_CTLCOLORSTATIC` gets to work with is the control id.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    /// Sits on a card, next to its control.
    Row,
    /// Secondary-colour title above a card.
    GroupTitle,
    /// Explanatory line under a card, on the window background.
    Footnote,
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_label(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    text: &str,
    id: i32,
    kind: LabelKind,
) {
    unsafe {
        let class = to_wide("STATIC");
        let wide = to_wide(text);
        // Row labels are single lines centred against a taller row; footnotes
        // wrap and start at the top.
        let style = if kind == LabelKind::Footnote {
            WS_CHILD | WS_VISIBLE
        } else {
            WS_CHILD | WS_VISIBLE | SS_CENTERIMAGE | SS_ENDELLIPSIS
        };
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(wide.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

/// A switch is a plain owner-drawn button sized to the track, so it never
/// overlaps the card's rounded corners the way a full-row control would.
unsafe fn create_switch(parent: HWND, hinst: HINSTANCE, x: i32, y: i32, id: i32, on: bool) {
    unsafe {
        let class = to_wide("BUTTON");
        // BS_OWNERDRAW → parent paints in WM_DRAWITEM.  The auto-toggle that
        // BS_AUTOCHECKBOX provides is gone with it, so the state lives in
        // GWLP_USERDATA and we flip it ourselves on BN_CLICKED.
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW,
            x,
            y,
            layout::SWITCH_W,
            layout::SWITCH_H,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, if on { 1 } else { 0 });
    }
}

unsafe fn create_edit_field(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    initial: &str,
    id: i32,
) {
    unsafe {
        let class = to_wide("EDIT");
        let initial_wide = to_wide(initial);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(initial_wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let _ = SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        // Internal padding so text doesn't touch edges.
        let _ = SendMessageW(edit, EM_SETMARGINS, WPARAM(3), LPARAM(8 | (8 << 16)));
    }
}

unsafe fn create_hotkey_field(
    parent: HWND,
    hinst: HINSTANCE,
    _font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    current: &HotkeyConfig,
) {
    unsafe {
        register_hotkey_class(hinst);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ScrTransHotkey"),
            PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let state = Box::into_raw(Box::new(HotkeyState {
            mods: current.modifiers,
            vk: current.vk,
            focused: false,
        }));
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, state as isize);
    }
}

unsafe fn create_od_button(
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    text: &str,
    id: i32,
) {
    unsafe {
        let class = to_wide("BUTTON");
        let wide = to_wide(text);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW,
            x,
            y,
            w,
            h,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

fn is_checkbox_id(id: i32) -> bool {
    matches!(
        id,
        IDC_CHK_PUNTO
            | IDC_CHK_TASKBAR
            | IDC_CHK_AUTOSTART
            | IDC_CHK_EXPLORER_CMD
    )
}

unsafe fn create_lang_combo(
    parent: HWND,
    hinst: HINSTANCE,
    _font: HFONT,
    x: i32,
    y: i32,
    w: i32,
    current_code: &str,
) {
    unsafe {
        register_lang_class(hinst);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ScrTransLangCombo"),
            PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x,
            y,
            w,
            layout::CTRL_H,
            parent,
            HMENU(IDC_COMBO_LANG as *mut _),
            hinst,
            None,
        )
        .unwrap_or_default();
        let current = Language::from_code(current_code);
        let selected = Language::all()
            .iter()
            .position(|l| *l == current)
            .unwrap_or(0);
        let state = Box::into_raw(Box::new(LangState {
            selected,
            focused: false,
            popup: 0,
        }));
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, state as isize);
    }
}

// ============================================================
// Reading control values back into a Settings struct
// ============================================================

unsafe fn read_hotkey(parent: HWND, id: i32) -> HotkeyConfig {
    unsafe {
        let ctrl = GetDlgItem(parent, id).unwrap_or_default();
        let r = SendMessageW(ctrl, HK_MSG_GET, WPARAM(0), LPARAM(0));
        let packed = r.0 as u32;
        HotkeyConfig {
            vk: packed & 0xFFFF,
            modifiers: packed >> 16,
        }
    }
}

unsafe fn read_folder(parent: HWND) -> String {
    unsafe { read_edit_text(parent, IDC_EDIT_FOLDER) }
}

unsafe fn read_deepseek_key(parent: HWND) -> String {
    unsafe { read_edit_text(parent, IDC_EDIT_DEEPSEEK) }
}

unsafe fn read_edit_text(parent: HWND, id: i32) -> String {
    unsafe {
        let ctrl = GetDlgItem(parent, id).unwrap_or_default();
        let len = GetWindowTextLengthW(ctrl) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 2];
        let got = GetWindowTextW(ctrl, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

unsafe fn read_checkbox(parent: HWND, id: i32) -> bool {
    unsafe {
        let ctrl = GetDlgItem(parent, id).unwrap_or_default();
        GetWindowLongPtrW(ctrl, GWLP_USERDATA) != 0
    }
}

unsafe fn read_selected_language(parent: HWND) -> String {
    unsafe {
        let ctrl = GetDlgItem(parent, IDC_COMBO_LANG).unwrap_or_default();
        let idx = SendMessageW(ctrl, LANG_MSG_GET, WPARAM(0), LPARAM(0)).0 as usize;
        Language::all()
            .get(idx)
            .map(|l| l.code().to_string())
            .unwrap_or_else(|| "en".to_string())
    }
}

/// Shows the native pick-folder dialog on a dedicated STA worker thread.
///
/// Why the indirection: `IFileOpenDialog` is a UI component that requires
/// its thread to be an STA (single-threaded apartment).  Our main thread
/// runs as MTA (see `CoInitializeEx(COINIT_MULTITHREADED)` in main.rs),
/// so calling `dialog.Show()` from the UI thread deadlocks Windows.
///
/// We spawn an STA thread, let it run the dialog (which internally pumps
/// its own message loop), and post the chosen path back via a custom
/// `WM_APP_BROWSE_RESULT` message.  The main-thread handler updates the
/// edit field.  The Settings window stays responsive the whole time
/// because Windows auto-disables the parent while a modal child exists.
unsafe fn browse_folder(parent: HWND) {
    let parent_isize = parent.0 as isize;

    std::thread::spawn(move || {
        use windows::Win32::System::Com::{
            CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
        };
        use windows::Win32::UI::Shell::{
            FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        };

        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            let picked: Option<String> = (|| -> Option<String> {
                let dialog: IFileOpenDialog =
                    CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
                if let Ok(opts) = dialog.GetOptions() {
                    let _ = dialog.SetOptions(opts | FOS_PICKFOLDERS);
                }
                let parent_hwnd = HWND(parent_isize as *mut _);
                dialog.Show(parent_hwnd).ok()?;
                let item = dialog.GetResult().ok()?;
                let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
                let s = pwstr.to_string().ok()?;
                windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
                Some(s)
            })();

            if let Some(path) = picked {
                // Hand the string back via a heap-allocated Box.  Handler
                // on the main thread takes ownership and frees it.
                let boxed: *mut String = Box::into_raw(Box::new(path));
                let parent_hwnd = HWND(parent_isize as *mut _);
                if PostMessageW(
                    parent_hwnd,
                    WM_APP_BROWSE_RESULT,
                    WPARAM(0),
                    LPARAM(boxed as isize),
                )
                .is_err()
                {
                    drop(Box::from_raw(boxed));
                }
            }

            CoUninitialize();
        }
    });
}

/// Handler for `WM_APP_BROWSE_RESULT`.  Runs on the main (UI) thread.
unsafe fn apply_browse_result(hwnd: HWND, lp: LPARAM) {
    unsafe {
        let ptr = lp.0 as *mut String;
        if ptr.is_null() {
            return;
        }
        let path = *Box::from_raw(ptr);
        let ctrl = GetDlgItem(hwnd, IDC_EDIT_FOLDER).unwrap_or_default();
        if ctrl.0.is_null() {
            return;
        }
        let wide = to_wide(&path);
        let _ = SetWindowTextW(ctrl, PCWSTR(wide.as_ptr()));
    }
}

// ============================================================
// Owner-drawn button painting (hover-aware)
// ============================================================

#[repr(C)]
struct DrawItemStruct {
    ctl_type: u32,
    ctl_id: u32,
    item_id: u32,
    item_action: u32,
    item_state: u32,
    hwnd_item: HWND,
    hdc: HDC,
    rc_item: RECT,
    item_data: usize,
}

unsafe fn draw_owner_button(lp: LPARAM) {
    unsafe {
        let dis = &*(lp.0 as *const DrawItemStruct);
        let hdc = dis.hdc;
        let rc = dis.rc_item;
        let id = dis.ctl_id as i32;

        // Item-state flags set by the BS_OWNERDRAW protocol.
        //   ODS_SELECTED (0x0001) = pressed
        //   ODS_FOCUS    (0x0010) = keyboard focus
        //   ODS_HOTLIGHT (0x0040) = mouse hover (best-effort from the theme)
        let is_pressed = dis.item_state & 0x0001 != 0;
        let is_hover = dis.item_state & 0x0040 != 0;
        let state = if is_pressed {
            button::State::Pressed
        } else if is_hover {
            button::State::Hover
        } else {
            button::State::Normal
        };

        // Save is the default button — the one Return activates.
        let variant = if id == IDC_BTN_SAVE {
            button::Variant::Primary
        } else {
            button::Variant::Secondary
        };

        // The control's own background shows through the rounded corners, so
        // clear it before the body goes down.
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let _ = FillRect(hdc, &rc, r.bg_brush());

        button::draw(hdc, &rc, theme::CLR_ACCENT, variant, state);

        // Text.
        let old_font = SelectObject(hdc, r.font_button());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(button::text_color(variant, state)));

        let mut buf = vec![0u16; 64];
        let len = GetWindowTextW(dis.hwnd_item, &mut buf) as usize;
        let mut text: Vec<u16> = buf[..len].to_vec();
        let mut trc = rc;
        // DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
        DrawTextW(hdc, &mut text, &mut trc, DRAW_TEXT_FORMAT(0x0825));
        SelectObject(hdc, old_font);
    }
}

/// Paints a macOS-style switch: a pill track with a white knob that sits at
/// one end or the other.  The state lives in `GWLP_USERDATA` because
/// `BS_OWNERDRAW` disables the auto-toggle a real checkbox would have.
unsafe fn draw_owner_switch(dis: &DrawItemStruct) {
    unsafe {
        use layout::*;
        let hdc = dis.hdc;
        let rc = dis.rc_item;
        let on = GetWindowLongPtrW(dis.hwnd_item, GWLP_USERDATA) != 0;
        let hover = dis.item_state & 0x0040 != 0;
        let pressed = dis.item_state & 0x0001 != 0;

        // The switch sits on a card, so that is what shows through its
        // rounded ends.
        let r_guard = res();
        let _ = FillRect(hdc, &rc, r_guard.as_ref().unwrap().card_brush());
        drop(r_guard);

        let accent = theme::CLR_ACCENT;
        let (top, bottom, border) = if on {
            (lighten(accent, 14), accent, darken(accent, 28))
        } else {
            (
                theme::CLR_CTRL_TOP,
                theme::CLR_CTRL_BOTTOM,
                theme::CLR_CTRL_BORDER,
            )
        };
        let shift: i32 = if pressed {
            -14
        } else if hover {
            10
        } else {
            0
        };
        let adjust = |c: u32| {
            if shift < 0 {
                darken(c, shift.unsigned_abs())
            } else {
                lighten(c, shift as u32)
            }
        };

        let track = paint::Style::flat(SWITCH_H / 2, top)
            .gradient(adjust(top), adjust(bottom))
            .border(adjust(border));
        paint::round_rect(hdc, &rc, &track);

        // Knob: a circle inset from the track, plus one row for its shadow.
        let d = SWITCH_H - 6;
        let x = if on { rc.right - 3 - d } else { rc.left + 3 };
        let knob_rc = RECT {
            left: x,
            top: rc.top + 3,
            right: x + d,
            bottom: rc.top + 3 + d + 1,
        };
        let knob = paint::Style::flat(d / 2, 0x00FF_FFFF)
            .gradient(0x00FF_FFFF, 0x00F0_F0F0)
            .shadow(theme::CLR_SHADOW);
        paint::round_rect(hdc, &knob_rc, &knob);
    }
}

// ============================================================
// WM_PAINT — grouped cards, row hairlines, field frames
// ============================================================

/// Native EDIT controls the parent draws a rounded frame around.  The hotkey
/// fields and the language combo are custom classes that paint their own.
fn input_rects() -> Vec<(i32, RECT)> {
    use layout::*;
    let page = page();
    let deepseek = page.groups[G_TRANSLATION].trailing(0, VALUE_W, CTRL_H);
    let browse = page.groups[G_SCREENSHOTS].trailing(0, BROWSE_W, CTRL_H);
    let folder_left = page.groups[G_SCREENSHOTS].card.left + ROW_PAD;
    vec![
        (IDC_EDIT_DEEPSEEK, deepseek),
        (
            IDC_EDIT_FOLDER,
            RECT {
                left: folder_left,
                top: browse.top,
                right: browse.left - 10,
                bottom: browse.bottom,
            },
        ),
    ]
}

/// A field's frame is drawn just outside the control, so the child's own
/// rectangular fill never shows a square corner.
fn field_frame(rc: &RECT) -> RECT {
    RECT {
        left: rc.left - 3,
        top: rc.top - 3,
        right: rc.right + 3,
        bottom: rc.bottom + 3,
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        use layout::*;

        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let page = page();

        // Grouped cards, the way macOS lays out a settings pane: content on a
        // slightly raised surface, hairlines between rows indented past the
        // left padding so the group reads as one block rather than a table.
        let card_style = paint::Style::flat(CARD_R, theme::CLR_CARD).border(theme::CLR_SEPARATOR);
        for g in &page.groups {
            paint::round_rect(hdc, &g.card, &card_style);
            for i in 1..g.rows {
                paint::hairline(
                    hdc,
                    g.card.left + ROW_PAD,
                    g.card.right,
                    g.row_top(i),
                    theme::CLR_SEPARATOR,
                );
            }
        }

        // Field frames — accent when the field has focus, the closest GDI gets
        // to a focus ring.
        let focused = GetFocus();
        let focused_id = if focused.0.is_null() {
            0
        } else {
            GetDlgCtrlID(focused)
        };
        for (id, rc) in input_rects() {
            draw_field_frame(hdc, &field_frame(&rc), id == focused_id);
        }

        draw_key_status(hdc);

        let _ = EndPaint(hwnd, &ps);
    }
}

/// Recessed fill and a border that turns accent on focus — the closest GDI
/// gets to a macOS focus ring.  Shared by the native EDITs (framed by the
/// parent) and the custom hotkey / language controls (which paint their own).
fn field_style(focused: bool) -> paint::Style {
    paint::Style::flat(FIELD_R, theme::CLR_FIELD)
        .border(if focused {
            theme::CLR_ACCENT
        } else {
            theme::CLR_FIELD_BORDER
        })
        .border_width(if focused { 2 } else { 1 })
}

unsafe fn draw_field_frame(hdc: HDC, rc: &RECT, focused: bool) {
    unsafe { paint::round_rect(hdc, rc, &field_style(focused)) }
}

// ============================================================
// Helpers
// ============================================================

/// Invalidate just the frame around every input field, so focus changes
/// repaint the ring without touching the rest of the window.
unsafe fn invalidate_input_borders(hwnd: HWND) {
    unsafe {
        for (_, rc) in input_rects() {
            let _ = InvalidateRect(hwnd, Some(&field_frame(&rc)), false);
        }
    }
}

// ============================================================
// Command dispatch & save
// ============================================================

unsafe fn do_save(hwnd: HWND) {
    unsafe {
        let new_settings = Settings {
            hk_translate: read_hotkey(hwnd, IDC_HK_TRANSLATE),
            hk_ocr: read_hotkey(hwnd, IDC_HK_OCR),
            hk_screenshot: read_hotkey(hwnd, IDC_HK_SCREENSHOT),
            hk_layout: read_hotkey(hwnd, IDC_HK_LAYOUT),
            hk_explorer_cmd: read_hotkey(hwnd, IDC_HK_EXPLORER_CMD),
            hk_ask: read_hotkey(hwnd, IDC_HK_ASK),
            screenshot_folder: read_folder(hwnd),
            punto_enabled: read_checkbox(hwnd, IDC_CHK_PUNTO),
            taskbar_center_enabled: read_checkbox(hwnd, IDC_CHK_TASKBAR),
            language: read_selected_language(hwnd),
            deepseek_api_key: read_deepseek_key(hwnd).trim().to_string(),
        };

        // Side-channel toggles (registry / context menu).
        autostart::set_enabled(read_checkbox(hwnd, IDC_CHK_AUTOSTART));
        crate::explorer_cmd::set_menu_enabled(read_checkbox(hwnd, IDC_CHK_EXPLORER_CMD));

        settings::save(&new_settings);
        *UPDATED_SETTINGS.lock().unwrap() = Some(Box::new(new_settings));
        let _ = DestroyWindow(hwnd);
    }
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DRAWITEM => {
                let dis = &*(lp.0 as *const DrawItemStruct);
                if is_checkbox_id(dis.ctl_id as i32) {
                    draw_owner_switch(dis);
                } else {
                    draw_owner_button(lp);
                }
                LRESULT(1)
            }

            // The id is all there is to go on, so it decides both the label's
            // colour and which surface it is standing on: row labels sit on a
            // card, titles and footnotes on the window background.
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkMode(hdc, TRANSPARENT);
                let id = GetDlgCtrlID(HWND(lp.0 as *mut _));
                let r_guard = res();
                let r = r_guard.as_ref().unwrap();

                let (text, brush) = if id >= IDC_ROW_LABEL {
                    (theme::CLR_TEXT_BRIGHT, r.card_brush())
                } else if id == IDC_FOOTNOTE {
                    (theme::CLR_HINT, r.bg_brush())
                } else if (IDC_GROUP_TITLE..IDC_FOOTNOTE).contains(&id) {
                    (theme::CLR_TEXT_DIM, r.bg_brush())
                } else if is_checkbox_id(id) {
                    (theme::CLR_TEXT, r.card_brush())
                } else {
                    (theme::CLR_TEXT, r.bg_brush())
                };
                SetTextColor(hdc, COLORREF(text));
                LRESULT(brush.0 as isize)
            }

            WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkColor(hdc, COLORREF(theme::CLR_FIELD));
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));
                LRESULT(res().as_ref().unwrap().field_brush().0 as isize)
            }

            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }

            WM_COMMAND => {
                let code = ((wp.0 >> 16) & 0xFFFF) as u16;
                let id = (wp.0 & 0xFFFF) as i32;
                // EN_SETFOCUS = 0x0100, EN_KILLFOCUS = 0x0200 (native EDIT).
                if code == 0x0100 || code == 0x0200 {
                    invalidate_input_borders(hwnd);
                }
                // Re-check the key once the user is done typing it.
                if code == 0x0200 && id == IDC_EDIT_DEEPSEEK {
                    start_key_check(hwnd, &read_deepseek_key(hwnd));
                }
                // BN_CLICKED on an owner-drawn checkbox (notify == 0) — flip
                // the stashed state and repaint the control.
                if code == 0 && is_checkbox_id(id) {
                    let ctrl = HWND(lp.0 as *mut _);
                    let cur = GetWindowLongPtrW(ctrl, GWLP_USERDATA);
                    SetWindowLongPtrW(ctrl, GWLP_USERDATA, if cur == 0 { 1 } else { 0 });
                    let _ = InvalidateRect(ctrl, None, false);
                }
                match id {
                    IDC_BTN_SAVE => do_save(hwnd),
                    IDC_BTN_CANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_BTN_BROWSE => browse_folder(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }

            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                *SETTINGS_HWND.lock().unwrap() = 0;
                LRESULT(0)
            }

            m if m == WM_APP_BROWSE_RESULT => {
                apply_browse_result(hwnd, lp);
                LRESULT(0)
            }

            m if m == WM_APP_KEY_STATUS => {
                invalidate_key_status(hwnd);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ============================================================
// Custom HOTKEY control — owner-drawn, same visual language as the
// rest of the settings panel.  Captures key combos directly and
// exposes get/set via HK_MSG_{GET,SET} custom messages.
// ============================================================

struct HotkeyState {
    mods: u32,
    vk: u32,
    focused: bool,
}

static HOTKEY_CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);

unsafe fn register_hotkey_class(hinst: HINSTANCE) {
    unsafe {
        let mut g = HOTKEY_CLASS_REGISTERED.lock().unwrap();
        if *g {
            return;
        }
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(hotkey_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: w!("ScrTransHotkey"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        *g = true;
    }
}

unsafe fn hotkey_state(hwnd: HWND) -> Option<&'static mut HotkeyState> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut HotkeyState;
        if p.is_null() { None } else { Some(&mut *p) }
    }
}

unsafe extern "system" fn hotkey_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_GETDLGCODE => LRESULT(0x000E), // WANTARROWS | WANTTAB | WANTALLKEYS
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                let _ = SetFocus(hwnd);
                LRESULT(0)
            }
            WM_SETFOCUS => {
                if let Some(s) = hotkey_state(hwnd) {
                    s.focused = true;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if let Some(s) = hotkey_state(hwnd) {
                    s.focused = false;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wp.0 as u32;
                // Ignore standalone modifier keys — wait for the "real" key.
                if matches!(
                    vk,
                    0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
                ) {
                    return LRESULT(0);
                }
                // Tab and Escape are reserved for UI navigation / dismiss —
                // capturing them as hotkeys would break basic keyboard use.
                if vk == 0x09 || vk == 0x1B {
                    return LRESULT(0);
                }
                // Backspace or Delete clears the hotkey.
                if vk == 0x08 || vk == 0x2E {
                    if let Some(s) = hotkey_state(hwnd) {
                        s.mods = 0;
                        s.vk = 0;
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let ctrl_down = (GetKeyState(0x11) as u16 & 0x8000) != 0;
                let shift_down = (GetKeyState(0x10) as u16 & 0x8000) != 0;
                let alt_down = (GetKeyState(0x12) as u16 & 0x8000) != 0;
                let mut m = 0u32;
                if ctrl_down {
                    m |= 0x0002;
                }
                if alt_down {
                    m |= 0x0001;
                }
                if shift_down {
                    m |= 0x0004;
                }
                if let Some(s) = hotkey_state(hwnd) {
                    s.mods = m;
                    s.vk = vk;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let (mods, vk, focused) = hotkey_state(hwnd)
                    .map(|s| (s.mods, s.vk, s.focused))
                    .unwrap_or((0, 0, false));
                paint_hotkey(hwnd, hdc, mods, vk, focused);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            m if m == HK_MSG_GET => hotkey_state(hwnd)
                .map(|s| LRESULT((s.vk | (s.mods << 16)) as isize))
                .unwrap_or(LRESULT(0)),
            m if m == HK_MSG_SET => {
                let v = wp.0 as u32;
                if let Some(s) = hotkey_state(hwnd) {
                    s.vk = v & 0xFFFF;
                    s.mods = v >> 16;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut HotkeyState;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn paint_hotkey(hwnd: HWND, hdc: HDC, mods: u32, vk: u32, focused: bool) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        // Double-buffer so the rounded-rect fill + border + text land on
        // screen as one frame.
        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem_dc, mem_bmp);

        // The card shows through the rounded corners — this control sits on
        // one, not on the window background.
        let card = CreateSolidBrush(COLORREF(theme::CLR_CARD));
        FillRect(mem_dc, &rc, card);
        let _ = DeleteObject(card);
        paint::round_rect(mem_dc, &rc, &field_style(focused));

        let text = hotkey_display(mods, vk, focused);
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);
        let text_color = if vk == 0 {
            theme::CLR_TEXT_DIM
        } else {
            theme::CLR_TEXT_BRIGHT
        };
        SetTextColor(mem_dc, COLORREF(text_color));
        let mut wide = to_wide(&text);
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut trc = RECT {
            left: 12,
            top: 0,
            right: rc.right - 12,
            bottom: rc.bottom,
        };
        DrawTextW(mem_dc, &mut wide, &mut trc, DRAW_TEXT_FORMAT(0x0824));
        SelectObject(mem_dc, old_font);
        drop(r_guard);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem_dc, 0, 0, SRCCOPY);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn hotkey_display(mods: u32, vk: u32, focused: bool) -> String {
    if vk == 0 {
        if focused {
            "…".to_string()
        } else {
            "—".to_string()
        }
    } else {
        HotkeyConfig {
            modifiers: mods,
            vk,
        }
        .display()
    }
}

// ============================================================
// Custom LANGUAGE combo — button + popup list.  Popup is a
// separate WS_POPUP window that takes mouse capture so clicks
// outside close it.
// ============================================================

struct LangState {
    selected: usize,
    focused: bool,
    /// HWND of the open popup, or 0 when closed.
    popup: isize,
}

struct LangPopupState {
    /// HWND of the owning combo — we poke its state from item clicks.
    owner: isize,
    hover: usize, // usize::MAX = none
    item_h: i32,
    pad: i32,
}

static LANG_CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);
static LANG_POPUP_CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);

unsafe fn register_lang_class(hinst: HINSTANCE) {
    unsafe {
        let mut g = LANG_CLASS_REGISTERED.lock().unwrap();
        if *g {
            return;
        }
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(lang_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: w!("ScrTransLangCombo"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        *g = true;
    }
}

unsafe fn register_lang_popup_class(hinst: HINSTANCE) {
    unsafe {
        let mut g = LANG_POPUP_CLASS_REGISTERED.lock().unwrap();
        if *g {
            return;
        }
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(lang_popup_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: w!("ScrTransLangPopup"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        *g = true;
    }
}

unsafe fn lang_state(hwnd: HWND) -> Option<&'static mut LangState> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LangState;
        if p.is_null() { None } else { Some(&mut *p) }
    }
}

unsafe fn lang_popup_state(hwnd: HWND) -> Option<&'static mut LangPopupState> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LangPopupState;
        if p.is_null() { None } else { Some(&mut *p) }
    }
}

unsafe extern "system" fn lang_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_GETDLGCODE => LRESULT(0x0001 | 0x0002),
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                let _ = SetFocus(hwnd);
                let already_open = lang_state(hwnd).map(|s| s.popup != 0).unwrap_or(false);
                if already_open {
                    // Click on combo while popup is open — capture routes the
                    // click to the popup instead, so this path is unused in
                    // practice, but guard anyway.
                    let _ = ReleaseCapture();
                } else {
                    open_lang_popup(hwnd);
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                if let Some(s) = lang_state(hwnd) {
                    s.focused = true;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if let Some(s) = lang_state(hwnd) {
                    s.focused = false;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u32;
                let popup_open = lang_state(hwnd).map(|s| s.popup != 0).unwrap_or(false);
                match vk {
                    0x1B if popup_open => {
                        let _ = ReleaseCapture();
                    } // Esc closes popup
                    0x0D | 0x20 | 0x28 => {
                        open_lang_popup(hwnd);
                    } // Enter / Space / Down
                    0x26 => {
                        // Up — cycle selection
                        if let Some(s) = lang_state(hwnd) {
                            if s.selected > 0 {
                                s.selected -= 1;
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let (selected, focused) = lang_state(hwnd)
                    .map(|s| (s.selected, s.focused))
                    .unwrap_or((0, false));
                paint_lang(hwnd, hdc, selected, focused);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            m if m == LANG_MSG_GET => lang_state(hwnd)
                .map(|s| LRESULT(s.selected as isize))
                .unwrap_or(LRESULT(0)),
            m if m == LANG_MSG_SET => {
                if let Some(s) = lang_state(hwnd) {
                    s.selected = wp.0 as usize;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LangState;
                if !p.is_null() {
                    let popup_hwnd = (*p).popup;
                    if popup_hwnd != 0 {
                        let _ = DestroyWindow(HWND(popup_hwnd as *mut _));
                    }
                    drop(Box::from_raw(p));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn paint_lang(hwnd: HWND, hdc: HDC, selected: usize, focused: bool) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem_dc, mem_bmp);

        let card = CreateSolidBrush(COLORREF(theme::CLR_CARD));
        FillRect(mem_dc, &rc, card);
        let _ = DeleteObject(card);
        paint::round_rect(mem_dc, &rc, &field_style(focused));

        let langs = Language::all();
        let name = langs.get(selected).map(|l| l.native_name()).unwrap_or("");
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(theme::CLR_TEXT_BRIGHT));
        let mut wide = to_wide(name);
        if wide.last() == Some(&0) {
            wide.pop();
        }
        let mut trc = RECT {
            left: 14,
            top: 0,
            right: rc.right - 32,
            bottom: rc.bottom,
        };
        DrawTextW(mem_dc, &mut wide, &mut trc, DRAW_TEXT_FORMAT(0x0824));
        SelectObject(mem_dc, old_font);
        drop(r_guard);

        // Chevron ▼
        let cx = rc.right - 16;
        let cy = rc.bottom / 2 + 1;
        let chev_pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_TEXT));
        let op2 = SelectObject(mem_dc, chev_pen);
        let _ = MoveToEx(mem_dc, cx - 5, cy - 3, None);
        let _ = LineTo(mem_dc, cx, cy + 3);
        let _ = MoveToEx(mem_dc, cx + 1, cy + 2, None);
        let _ = LineTo(mem_dc, cx + 6, cy - 3);
        SelectObject(mem_dc, op2);
        let _ = DeleteObject(chev_pen);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem_dc, 0, 0, SRCCOPY);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

unsafe fn open_lang_popup(owner: HWND) {
    unsafe {
        if let Some(s) = lang_state(owner) {
            if s.popup != 0 {
                return;
            }
        }
        let Ok(hmodule) = GetModuleHandleW(None) else {
            return;
        };
        let hinst = HINSTANCE(hmodule.0);
        register_lang_popup_class(hinst);

        let mut rc = RECT::default();
        let _ = GetWindowRect(owner, &mut rc);
        let item_h = 32i32;
        let pad = 6i32;
        let n = Language::all().len() as i32;
        let popup_w = rc.right - rc.left;
        let popup_h = item_h * n + pad * 2;

        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("ScrTransLangPopup"),
            w!(""),
            WS_POPUP,
            rc.left,
            rc.bottom + 4,
            popup_w,
            popup_h,
            owner,
            HMENU::default(),
            hinst,
            None,
        )
        .unwrap_or_default();
        if popup.0.is_null() {
            return;
        }

        let state = Box::into_raw(Box::new(LangPopupState {
            owner: owner.0 as isize,
            hover: lang_state(owner).map(|s| s.selected).unwrap_or(0),
            item_h,
            pad,
        }));
        SetWindowLongPtrW(popup, GWLP_USERDATA, state as isize);

        if let Some(s) = lang_state(owner) {
            s.popup = popup.0 as isize;
        }

        let _ = ShowWindow(popup, SW_SHOWNA);
        // Capture so clicks outside the popup close it.  WM_CAPTURECHANGED
        // is the canonical "close yourself" signal.
        SetCapture(popup);
    }
}

unsafe extern "system" fn lang_popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE => {
                let x = (lp.0 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                if let Some(s) = lang_popup_state(hwnd) {
                    let inside = x >= 0 && y >= 0 && x < client.right && y < client.bottom;
                    let new_hover = if inside {
                        let idx = (y - s.pad) / s.item_h;
                        if idx >= 0 && (idx as usize) < Language::all().len() {
                            idx as usize
                        } else {
                            usize::MAX
                        }
                    } else {
                        usize::MAX
                    };
                    if new_hover != s.hover {
                        s.hover = new_hover;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (lp.0 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let (owner_raw, pad, item_h) = lang_popup_state(hwnd)
                    .map(|s| (s.owner, s.pad, s.item_h))
                    .unwrap_or((0, 0, 1));
                let owner = HWND(owner_raw as *mut _);
                let inside = x >= 0 && y >= 0 && x < client.right && y < client.bottom;
                if inside {
                    let idx = (y - pad) / item_h;
                    if idx >= 0 && (idx as usize) < Language::all().len() {
                        if let Some(os) = lang_state(owner) {
                            os.selected = idx as usize;
                        }
                    }
                }
                // Closing routes through ReleaseCapture → WM_CAPTURECHANGED.
                let _ = ReleaseCapture();
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                let owner_raw = lang_popup_state(hwnd).map(|s| s.owner).unwrap_or(0);
                if owner_raw != 0 {
                    let owner = HWND(owner_raw as *mut _);
                    if let Some(os) = lang_state(owner) {
                        os.popup = 0;
                    }
                    let _ = InvalidateRect(owner, None, false);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint_lang_popup(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut LangPopupState;
                if !p.is_null() {
                    drop(Box::from_raw(p));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn paint_lang_popup(hwnd: HWND, hdc: HDC) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem_dc, mem_bmp);

        // Floating list panel, the shape of a macOS menu.
        let bg = CreateSolidBrush(COLORREF(theme::CLR_BG));
        FillRect(mem_dc, &rc, bg);
        let _ = DeleteObject(bg);
        paint::round_rect(
            mem_dc,
            &rc,
            &paint::Style::flat(10, theme::CLR_FIELD).border(theme::CLR_FIELD_BORDER),
        );

        let Some(s) = lang_popup_state(hwnd) else {
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem_dc, 0, 0, SRCCOPY);
            SelectObject(mem_dc, old);
            let _ = DeleteObject(mem_bmp);
            let _ = DeleteDC(mem_dc);
            return;
        };
        let owner = HWND(s.owner as *mut _);
        let selected = lang_state(owner)
            .map(|os| os.selected)
            .unwrap_or(usize::MAX);

        let langs = Language::all();
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);

        for (i, lang) in langs.iter().enumerate() {
            let y = s.pad + (i as i32) * s.item_h;
            let item_rc = RECT {
                left: s.pad,
                top: y,
                right: rc.right - s.pad,
                bottom: y + s.item_h,
            };
            let is_hover = i == s.hover;
            let is_selected = i == selected;

            // Hovered row gets an accent plate, the way an open macOS menu
            // tracks the pointer; the current choice is marked with a tick
            // rather than a coloured bar.
            if is_hover {
                paint::round_rect(
                    mem_dc,
                    &item_rc,
                    &paint::Style::flat(FIELD_R, theme::CLR_ACCENT),
                );
            } else if is_selected {
                paint::round_rect(
                    mem_dc,
                    &item_rc,
                    &paint::Style::flat(FIELD_R, CLR_ROW_SELECTED),
                );
            }
            if is_selected {
                let tick = CreatePen(
                    PS_SOLID,
                    2,
                    COLORREF(if is_hover {
                        0x00FF_FFFF
                    } else {
                        theme::CLR_ACCENT
                    }),
                );
                let op = SelectObject(mem_dc, tick);
                let cx = item_rc.left + 10;
                let cy = (item_rc.top + item_rc.bottom) / 2;
                let _ = MoveToEx(mem_dc, cx - 4, cy, None);
                let _ = LineTo(mem_dc, cx - 1, cy + 4);
                let _ = LineTo(mem_dc, cx + 5, cy - 4);
                SelectObject(mem_dc, op);
                let _ = DeleteObject(tick);
            }

            SetTextColor(mem_dc, COLORREF(theme::CLR_TEXT_BRIGHT));
            let mut wide = to_wide(lang.native_name());
            if wide.last() == Some(&0) {
                wide.pop();
            }
            let mut text_rc = RECT {
                left: item_rc.left + 16,
                ..item_rc
            };
            DrawTextW(mem_dc, &mut wide, &mut text_rc, DRAW_TEXT_FORMAT(0x0824));
        }
        SelectObject(mem_dc, old_font);
        drop(r_guard);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem_dc, 0, 0, SRCCOPY);
        SelectObject(mem_dc, old);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}
