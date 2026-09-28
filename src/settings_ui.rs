//! Settings window — frameless, with a navigation column on the left and one
//! page of settings at a time on the right.
//!
//! Architecture:
//! * `open()` creates a captioned popup and then takes the caption away in
//!   `WM_NCCALCSIZE`, so the whole window is client area.  DWM still draws the
//!   shadow because the frame is extended one pixel into the client.  Dragging
//!   comes from `WM_NCHITTEST` answering `HTCAPTION` over the heading strip.
//! * What each page holds is declared once, in `PAGES`.  `compute_geo` turns
//!   that into rectangles, and control creation, painting and hit-testing all
//!   read the same rectangles, so they can't drift apart.
//! * Every control exists from the start; switching pages only shows and hides
//!   them.  Saving reads every control, on every page — a key on a page that
//!   was never opened is still read back exactly as it was loaded.
//! * All text, cards and field frames are painted by the window itself into
//!   one off-screen buffer.  Child windows are only the things that take
//!   input: edits, switches, hotkey fields, the language picker and buttons.

use crate::autostart;
use crate::button;
use crate::i18n::{self, Language};
use crate::paint;
use crate::settings::{self, HotkeyConfig, Settings};
use crate::theme::{self, lighten};
use crate::utils::to_wide;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT,
    TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ============================================================
// Win32 constants the windows crate doesn't name
// ============================================================

const BS_OWNERDRAW: WINDOW_STYLE = WINDOW_STYLE(0x000B);
const ES_AUTOHSCROLL: WINDOW_STYLE = WINDOW_STYLE(0x0080);
const EM_SETPASSWORDCHAR: u32 = 0x00CC;
const WM_MOUSELEAVE: u32 = 0x02A3;
const EN_SETFOCUS: u16 = 0x0100;
const EN_KILLFOCUS: u16 = 0x0200;
const EN_CHANGE: u16 = 0x0300;
const DLGC_WANTARROWS: isize = 0x0001;
const DLGC_WANTALLKEYS: isize = 0x0004;
const DLGC_WANTCHARS: isize = 0x0080;
const ODS_SELECTED: u32 = 0x0001;
const ODS_FOCUS: u32 = 0x0010;
const ODS_NOFOCUSRECT: u32 = 0x0200;
/// What `IsDialogMessage` sends for Enter and Escape.
const IDOK_CMD: i32 = 1;
const IDCANCEL_CMD: i32 = 2;
/// The bullet a masked key is drawn with.
const MASK_CHAR: usize = 0x2022;

// Custom messages for our hand-rolled hotkey / language controls.
// WPARAM/LRESULT both encode (vk | mods << 16) for hotkeys, and a raw index
// into Language::all() for the language combo.
const HK_MSG_GET: u32 = WM_USER + 100;
const HK_MSG_SET: u32 = WM_USER + 101;
const LANG_MSG_GET: u32 = WM_USER + 200;
const LANG_MSG_SET: u32 = WM_USER + 201;

// Private window messages.
/// The folder picker's worker posts the chosen path back through this.
/// LPARAM carries a `Box::into_raw(Box<String>)`; the handler takes it back.
const WM_APP_BROWSE_RESULT: u32 = WM_APP + 1;
/// A key check finished.  WPARAM is the `Service` index.
const WM_APP_KEY_STATUS: u32 = WM_APP + 2;

/// Debounce timers for re-checking a key while it's being typed, one per
/// service: `TIMER_KEY + index`.
const TIMER_KEY: usize = 500;
const KEY_DEBOUNCE_MS: u32 = 800;

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
const IDC_EDIT_GEMINI: i32 = 118;
const IDC_EDIT_SEARCH_KEY: i32 = 119;
const IDC_CHK_ASK: i32 = 120;
/// Show/hide toggle inside each key field: `IDC_EYE + Service index`.
const IDC_EYE: i32 = 130;

fn is_switch_id(id: i32) -> bool {
    matches!(
        id,
        IDC_CHK_PUNTO | IDC_CHK_TASKBAR | IDC_CHK_AUTOSTART | IDC_CHK_EXPLORER_CMD | IDC_CHK_ASK
    )
}

fn is_eye_id(id: i32) -> bool {
    (IDC_EYE..IDC_EYE + Service::ALL.len() as i32).contains(&id)
}

// ============================================================
// API keys and their status
// ============================================================

/// Every service the app takes a key for.  Each key field shows whether the
/// key works — a key that silently does nothing is the worst kind of setting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Service {
    DeepSeek,
    Gemini,
    Tavily,
}

impl Service {
    const ALL: [Service; 3] = [Service::DeepSeek, Service::Gemini, Service::Tavily];

    fn index(self) -> usize {
        self as usize
    }

    fn edit_id(self) -> i32 {
        match self {
            Service::DeepSeek => IDC_EDIT_DEEPSEEK,
            Service::Gemini => IDC_EDIT_GEMINI,
            Service::Tavily => IDC_EDIT_SEARCH_KEY,
        }
    }

    fn from_edit_id(id: i32) -> Option<Service> {
        Service::ALL.into_iter().find(|s| s.edit_id() == id)
    }

    fn label(self) -> &'static str {
        match self {
            Service::DeepSeek => "settings.label.deepseek_key",
            Service::Gemini => "settings.label.gemini_key",
            Service::Tavily => "settings.label.search_key",
        }
    }

    /// "Service unreachable" names the service, so it's per key.
    fn offline(self) -> &'static str {
        match self {
            Service::DeepSeek => "settings.key.offline",
            Service::Gemini => "settings.key.offline_gemini",
            Service::Tavily => "settings.key.offline_search",
        }
    }

    fn stored(self, s: &Settings) -> &str {
        match self {
            Service::DeepSeek => &s.deepseek_api_key,
            Service::Gemini => &s.gemini_api_key,
            Service::Tavily => &s.search_api_key,
        }
    }

    /// Asks the service whether it accepts `key`, using whichever endpoint
    /// costs no quota: the model list for DeepSeek and Gemini, the usage
    /// endpoint for Tavily.  Blocking — call from a worker thread.
    fn check(self, key: &str) -> KeyStatus {
        let (status, err) = match self {
            Service::DeepSeek => match crate::deepseek::check_key(key) {
                crate::deepseek::KeyCheck::Valid => (KeyStatus::Valid, None),
                crate::deepseek::KeyCheck::Rejected => (KeyStatus::Rejected, None),
                crate::deepseek::KeyCheck::Unreachable(e) => (KeyStatus::Unreachable, Some(e)),
            },
            Service::Gemini => match crate::gemini::check_key(key) {
                crate::gemini::KeyCheck::Valid => (KeyStatus::Valid, None),
                crate::gemini::KeyCheck::Rejected => (KeyStatus::Rejected, None),
                crate::gemini::KeyCheck::Unreachable(e) => (KeyStatus::Unreachable, Some(e)),
            },
            Service::Tavily => match crate::websearch::check_key(key) {
                crate::websearch::KeyCheck::Valid => (KeyStatus::Valid, None),
                crate::websearch::KeyCheck::Rejected => (KeyStatus::Rejected, None),
                crate::websearch::KeyCheck::Unreachable(e) => (KeyStatus::Unreachable, Some(e)),
            },
        };
        if let Some(e) = err {
            println!("[!] Key check ({}): {e}", self.index());
        }
        status
    }
}

/// What we currently know about a configured key.  "Valid" means the service
/// accepted the key — not that quota is left, which only a real request can
/// tell and which surfaces at use time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyStatus {
    Unset,
    Checking,
    Valid,
    Rejected,
    Unreachable,
}

/// One per service: its status, and the key that status refers to, so a
/// re-check only happens when the text actually changed.
struct KeySlot {
    status: Mutex<KeyStatus>,
    checked: Mutex<String>,
}

static KEY_SLOTS: [KeySlot; 3] = [
    KeySlot {
        status: Mutex::new(KeyStatus::Unset),
        checked: Mutex::new(String::new()),
    },
    KeySlot {
        status: Mutex::new(KeyStatus::Unset),
        checked: Mutex::new(String::new()),
    },
    KeySlot {
        status: Mutex::new(KeyStatus::Unset),
        checked: Mutex::new(String::new()),
    },
];

// ============================================================
// What goes where
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Language,
    Switch(i32, &'static str),
    Hotkey(i32, &'static str),
    Folder,
    Key(Service),
}

struct GroupSpec {
    title: Option<&'static str>,
    rows: &'static [Row],
    footnote: Option<&'static str>,
}

struct PageSpec {
    title: &'static str,
    icon: char,
    groups: &'static [GroupSpec],
}

static PAGES: [PageSpec; 4] = [
    PageSpec {
        title: "settings.section.general",
        icon: theme::ICON_SETTINGS,
        groups: &[
            GroupSpec {
                title: None,
                rows: &[Row::Language],
                footnote: None,
            },
            GroupSpec {
                title: Some("settings.section.functions"),
                rows: &[
                    Row::Switch(IDC_CHK_PUNTO, "settings.checkbox.punto"),
                    Row::Switch(IDC_CHK_TASKBAR, "settings.checkbox.taskbar"),
                    Row::Switch(IDC_CHK_AUTOSTART, "settings.checkbox.autostart"),
                    Row::Switch(IDC_CHK_EXPLORER_CMD, "settings.checkbox.explorer_cmd"),
                ],
                footnote: None,
            },
            GroupSpec {
                title: Some("settings.section.folder"),
                rows: &[Row::Folder],
                footnote: Some("settings.hint.folder"),
            },
        ],
    },
    PageSpec {
        title: "settings.section.hotkeys",
        icon: theme::ICON_KEYBOARD,
        groups: &[GroupSpec {
            title: None,
            rows: &[
                Row::Hotkey(IDC_HK_TRANSLATE, "settings.hotkey.translate"),
                Row::Hotkey(IDC_HK_OCR, "settings.hotkey.ocr"),
                Row::Hotkey(IDC_HK_SCREENSHOT, "settings.hotkey.screenshot"),
                Row::Hotkey(IDC_HK_LAYOUT, "settings.hotkey.layout"),
                Row::Hotkey(IDC_HK_EXPLORER_CMD, "settings.hotkey.explorer_cmd"),
            ],
            footnote: Some("settings.hint.hotkeys"),
        }],
    },
    PageSpec {
        title: "settings.section.translation",
        icon: theme::ICON_TRANSLATE,
        groups: &[GroupSpec {
            title: None,
            rows: &[Row::Key(Service::DeepSeek)],
            footnote: Some("settings.hint.deepseek"),
        }],
    },
    PageSpec {
        title: "settings.hotkey.ask",
        icon: theme::ICON_CHAT,
        groups: &[
            GroupSpec {
                title: None,
                rows: &[
                    Row::Switch(IDC_CHK_ASK, "settings.checkbox.ask"),
                    Row::Hotkey(IDC_HK_ASK, "settings.label.shortcut"),
                ],
                footnote: Some("settings.hint.ask"),
            },
            GroupSpec {
                title: Some("settings.section.keys"),
                rows: &[Row::Key(Service::Gemini), Row::Key(Service::Tavily)],
                footnote: Some("settings.hint.search"),
            },
        ],
    },
];

// ============================================================
// Geometry — all in px
// ============================================================

mod layout {
    pub const WIN_W: i32 = 720;
    pub const SIDEBAR_W: i32 = 208;

    /// Content column, between the sidebar and the right edge.
    pub const PAD: i32 = 28;
    pub const CX0: i32 = SIDEBAR_W + PAD;
    pub const CX1: i32 = WIN_W - PAD;

    /// The strip across the top that drags the window.
    pub const DRAG_H: i32 = 60;
    /// Page heading, and where the first card starts below it.
    pub const HEADING_Y: i32 = 22;
    pub const HEADING_H: i32 = 30;
    pub const CONTENT_TOP: i32 = 70;

    pub const CARD_R: i32 = 8;
    pub const ROW_H: i32 = 44;
    /// A key row: label and status on one line, the field full-width below.
    pub const KEY_ROW_H: i32 = 82;
    pub const ROW_PAD: i32 = 14;

    pub const TITLE_H: i32 = 18;
    pub const TITLE_GAP: i32 = 8;
    pub const GROUP_GAP: i32 = 22;
    pub const FOOTNOTE_GAP: i32 = 8;

    /// Controls sitting in a row, and the value column they line up in.
    pub const CTRL_H: i32 = 30;
    pub const VALUE_W: i32 = 212;
    pub const KEY_FIELD_H: i32 = 32;
    /// A native edit is only as tall as its line, centred in its frame.
    pub const EDIT_H: i32 = 20;
    pub const EYE_W: i32 = 30;

    pub const SWITCH_W: i32 = 40;
    pub const SWITCH_H: i32 = 22;

    pub const BTN_W: i32 = 112;
    pub const BTN_H: i32 = 32;
    pub const BTN_GAP: i32 = 8;
    pub const BROWSE_W: i32 = 104;
    pub const FOOTER_GAP: i32 = 24;
    pub const FOOTER_PAD: i32 = 22;

    pub const NAV_TOP: i32 = 70;
    pub const NAV_H: i32 = 38;
    pub const NAV_GAP: i32 = 2;
    pub const NAV_INSET: i32 = 10;

    pub const CLOSE_W: i32 = 46;
    pub const CLOSE_H: i32 = 34;
}

// Type sizes.
const FONT_BODY: i32 = 14;
const FONT_META: i32 = 12;
const FONT_TITLE: i32 = 13;
const FONT_HEADING: i32 = 22;

fn row_height(r: &Row) -> i32 {
    match r {
        Row::Key(_) => layout::KEY_ROW_H,
        _ => layout::ROW_H,
    }
}

#[derive(Clone, Copy)]
struct RowGeo {
    row: Row,
    top: i32,
    h: i32,
}

struct GroupGeo {
    title: Option<(&'static str, i32)>,
    card: RECT,
    rows: Vec<RowGeo>,
    footnote: Option<(&'static str, RECT)>,
}

struct Geo {
    pages: Vec<Vec<GroupGeo>>,
    height: i32,
}

static GEO: Mutex<Option<Geo>> = Mutex::new(None);

fn with_geo<R>(f: impl FnOnce(&Geo) -> R) -> R {
    f(GEO.lock().unwrap().as_ref().expect("settings geometry"))
}

/// Lays every page out once.  Needs a DC because footnotes wrap, and how many
/// lines they take depends on the language.
unsafe fn compute_geo(hdc: HDC) -> Geo {
    use layout::*;
    let foot_font = theme::ui_font(FONT_META, 400);
    let mut pages = Vec::new();
    let mut bottom = 0;
    for page in PAGES.iter() {
        let mut y = CONTENT_TOP;
        let mut groups = Vec::new();
        for (gi, g) in page.groups.iter().enumerate() {
            if gi > 0 {
                y += GROUP_GAP;
            }
            let title = g.title.map(|k| {
                let t = (k, y);
                y += TITLE_H + TITLE_GAP;
                t
            });
            let top = y;
            let mut rows = Vec::new();
            for r in g.rows {
                let h = row_height(r);
                rows.push(RowGeo { row: *r, top: y, h });
                y += h;
            }
            let card = RECT {
                left: CX0,
                top,
                right: CX1,
                bottom: y,
            };
            let footnote = g.footnote.map(|k| {
                let w = CX1 - CX0 - 8;
                let fh = unsafe { theme::measure_wrapped(hdc, i18n::t(k), foot_font, w) };
                let rc = RECT {
                    left: CX0 + 4,
                    top: y + FOOTNOTE_GAP,
                    right: CX0 + 4 + w,
                    bottom: y + FOOTNOTE_GAP + fh,
                };
                y = rc.bottom;
                (k, rc)
            });
            groups.push(GroupGeo {
                title,
                card,
                rows,
                footnote,
            });
        }
        bottom = bottom.max(y);
        pages.push(groups);
    }
    let nav_bottom = NAV_TOP + PAGES.len() as i32 * (NAV_H + NAV_GAP);
    Geo {
        pages,
        height: bottom.max(nav_bottom) + FOOTER_GAP + BTN_H + FOOTER_PAD,
    }
}

/// Vertically centred slot of `w`×`h`, right-aligned in the row.
fn trailing(card: &RECT, r: &RowGeo, w: i32, h: i32) -> RECT {
    let top = r.top + (r.h - h) / 2;
    RECT {
        left: card.right - layout::ROW_PAD - w,
        top,
        right: card.right - layout::ROW_PAD,
        bottom: top + h,
    }
}

/// The frame of a key row's field: full width, below the label line.
fn key_field(card: &RECT, r: &RowGeo) -> RECT {
    let top = r.top + 36;
    RECT {
        left: card.left + layout::ROW_PAD,
        top,
        right: card.right - layout::ROW_PAD,
        bottom: top + layout::KEY_FIELD_H,
    }
}

/// A key row's label line.
fn key_label_line(card: &RECT, r: &RowGeo) -> RECT {
    RECT {
        left: card.left + layout::ROW_PAD,
        top: r.top + 8,
        right: card.right - layout::ROW_PAD,
        bottom: r.top + 34,
    }
}

fn folder_field(card: &RECT, r: &RowGeo) -> RECT {
    let browse = trailing(card, r, layout::BROWSE_W, layout::CTRL_H);
    RECT {
        left: card.left + layout::ROW_PAD,
        top: browse.top,
        right: browse.left - 8,
        bottom: browse.bottom,
    }
}

/// Where the native edit sits inside a field frame: one line tall, centred,
/// leaving `right_gap` free at the end for the show/hide toggle.
fn edit_in(frame: &RECT, right_gap: i32) -> RECT {
    let top = frame.top + (frame.bottom - frame.top - layout::EDIT_H) / 2;
    RECT {
        left: frame.left + 10,
        top,
        right: frame.right - 10 - right_gap,
        bottom: top + layout::EDIT_H,
    }
}

fn eye_rect(frame: &RECT) -> RECT {
    let h = frame.bottom - frame.top - 6;
    RECT {
        left: frame.right - 3 - layout::EYE_W,
        top: frame.top + 3,
        right: frame.right - 3,
        bottom: frame.top + 3 + h,
    }
}

/// Every native edit frame on page `page`, with the edit's control id.
fn field_frames(page: usize) -> Vec<(i32, RECT)> {
    with_geo(|geo| {
        let mut out = Vec::new();
        for g in &geo.pages[page] {
            for r in &g.rows {
                match r.row {
                    Row::Folder => out.push((IDC_EDIT_FOLDER, folder_field(&g.card, r))),
                    Row::Key(s) => out.push((s.edit_id(), key_field(&g.card, r))),
                    _ => {}
                }
            }
        }
        out
    })
}

/// The page a key row lives on, and its label line.
fn key_status_rect(svc: Service) -> Option<(usize, RECT)> {
    with_geo(|geo| {
        for (p, groups) in geo.pages.iter().enumerate() {
            for g in groups {
                for r in &g.rows {
                    if r.row == Row::Key(svc) {
                        return Some((p, key_label_line(&g.card, r)));
                    }
                }
            }
        }
        None
    })
}

fn nav_rect(i: usize) -> RECT {
    use layout::*;
    let top = NAV_TOP + i as i32 * (NAV_H + NAV_GAP);
    RECT {
        left: NAV_INSET,
        top,
        right: SIDEBAR_W - NAV_INSET,
        bottom: top + NAV_H,
    }
}

fn close_rect() -> RECT {
    use layout::*;
    RECT {
        left: WIN_W - CLOSE_W,
        top: 0,
        right: WIN_W,
        bottom: CLOSE_H,
    }
}

fn contains(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

fn intersects(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

// ============================================================
// Window-level state
// ============================================================

struct Resources {
    bg_brush: isize,
    card_brush: isize,
    field_brush: isize,
}

impl Resources {
    fn new() -> Self {
        unsafe {
            Self {
                bg_brush: CreateSolidBrush(COLORREF(theme::CLR_BG)).0 as isize,
                card_brush: CreateSolidBrush(COLORREF(theme::CLR_CARD)).0 as isize,
                field_brush: CreateSolidBrush(COLORREF(theme::CLR_FIELD)).0 as isize,
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
}

static RES: Mutex<Option<Box<Resources>>> = Mutex::new(None);

fn res() -> std::sync::MutexGuard<'static, Option<Box<Resources>>> {
    let mut g = RES.lock().unwrap();
    if g.is_none() {
        *g = Some(Box::new(Resources::new()));
    }
    g
}

static SETTINGS_HWND: Mutex<isize> = Mutex::new(0);
static UPDATED_SETTINGS: Mutex<Option<Box<Settings>>> = Mutex::new(None);

/// The page on show.
static PAGE: Mutex<usize> = Mutex::new(0);

/// Every child control, with the page it belongs to (`usize::MAX` = all
/// pages, for the footer buttons).
static CONTROLS: Mutex<Vec<(isize, usize)>> = Mutex::new(Vec::new());
const ALL_PAGES: usize = usize::MAX;

/// What the pointer is over among the things the window paints itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    Nav(usize),
    Close,
}
static HOVER: Mutex<Hover> = Mutex::new(Hover::None);
static TRACKING: Mutex<bool> = Mutex::new(false);

/// The owner-drawn button under the pointer.  Only one can be, so one slot
/// covers them all.
static HOT_BUTTON: Mutex<isize> = Mutex::new(0);
/// The BUTTON class's own window procedure, which the hover subclass wraps.
static BUTTON_PROC: Mutex<isize> = Mutex::new(0);

fn settings_hwnd() -> Option<HWND> {
    let v = *SETTINGS_HWND.lock().unwrap();
    (v != 0).then_some(HWND(v as *mut _))
}

fn current_page() -> usize {
    *PAGE.lock().unwrap()
}

// ============================================================
// Public API
// ============================================================

pub fn open(current: &Settings) {
    unsafe {
        // Reuse existing window if still alive.
        if let Some(hwnd) = settings_hwnd() {
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }

        let Some(hmodule) = GetModuleHandleW(None).ok() else {
            return;
        };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransSettings8");

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

        let screen = GetDC(None);
        *GEO.lock().unwrap() = Some(compute_geo(screen));
        ReleaseDC(None, screen);
        let win_h = with_geo(|g| g.height);
        let win_w = layout::WIN_W;

        // Centre on the monitor under the pointer — on a multi-monitor setup
        // that's the one being looked at.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
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
        let x = work.left + (work.right - work.left - win_w) / 2;
        let y = work.top + ((work.bottom - work.top - win_h) / 2).max(0);

        *PAGE.lock().unwrap() = 0;
        *HOVER.lock().unwrap() = Hover::None;

        let title = to_wide(i18n::t("settings.title"));
        // The caption is there for DWM — shadow, snap, a taskbar-less window
        // that still behaves like one — and taken away in WM_NCCALCSIZE.
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
        let ex_style = WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
        let hwnd = CreateWindowExW(
            ex_style,
            class,
            PCWSTR(title.as_ptr()),
            style,
            x,
            y,
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
        theme::round_corners(hwnd);
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 1,
            cyBottomHeight: 0,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        create_controls(hwnd, hinstance, current);
        // Focus rings stay hidden until the keyboard is used, the way a
        // dialog starts out — otherwise every mouse click leaves one behind.
        // WM_CHANGEUISTATE, MAKEWPARAM(UIS_SET, UISF_HIDEFOCUS | UISF_HIDEACCEL).
        SendMessageW(hwnd, 0x0127, WPARAM(1 | (3 << 16)), LPARAM(0));
        show_page(hwnd, 0);
        for svc in Service::ALL {
            start_key_check(svc, svc.stored(current));
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn take_updated_settings() -> Option<Settings> {
    UPDATED_SETTINGS.lock().unwrap().take().map(|b| *b)
}

/// Keyboard handling a dialog gets for free: Tab and Shift+Tab between
/// fields, Enter to save, Escape to close — plus Ctrl+Tab between pages.
/// Called from the main message loop; true means the message was handled.
pub fn is_dialog_message(msg: &MSG) -> bool {
    unsafe {
        let Some(hwnd) = settings_hwnd() else {
            return false;
        };
        if msg.hwnd != hwnd && !IsChild(hwnd, msg.hwnd).as_bool() {
            return false;
        }
        if msg.message == WM_KEYDOWN {
            let ctrl = GetKeyState(0x11) < 0;
            let shift = GetKeyState(0x10) < 0;
            let step = match msg.wParam.0 {
                0x09 if ctrl => Some(if shift { PAGES.len() - 1 } else { 1 }),
                0x22 if ctrl => Some(1),               // Ctrl+PgDn
                0x21 if ctrl => Some(PAGES.len() - 1), // Ctrl+PgUp
                _ => None,
            };
            if let Some(step) = step {
                show_page(hwnd, (current_page() + step) % PAGES.len());
                return true;
            }
        }
        IsDialogMessageW(hwnd, msg).as_bool()
    }
}

// ============================================================
// Controls
// ============================================================

unsafe fn create_controls(parent: HWND, hinst: HINSTANCE, s: &Settings) {
    unsafe {
        use layout::*;
        CONTROLS.lock().unwrap().clear();
        let body = theme::ui_font(FONT_BODY, 400);
        let button_font = theme::ui_font(FONT_BODY, 500);

        let rows: Vec<(usize, RECT, RowGeo)> = with_geo(|geo| {
            geo.pages
                .iter()
                .enumerate()
                .flat_map(|(p, groups)| {
                    groups
                        .iter()
                        .flat_map(move |g| g.rows.iter().map(move |r| (p, g.card, *r)))
                })
                .collect()
        });

        for (page, card, r) in rows {
            match r.row {
                Row::Language => {
                    let rc = trailing(&card, &r, VALUE_W, CTRL_H);
                    let h = create_lang_combo(parent, hinst, &rc, &s.language);
                    register(h, page);
                }
                Row::Switch(id, _) => {
                    let rc = trailing(&card, &r, SWITCH_W, SWITCH_H);
                    let on = match id {
                        IDC_CHK_PUNTO => s.punto_enabled,
                        IDC_CHK_TASKBAR => s.taskbar_center_enabled,
                        IDC_CHK_AUTOSTART => autostart::is_enabled(),
                        IDC_CHK_EXPLORER_CMD => crate::explorer_cmd::is_menu_enabled(),
                        IDC_CHK_ASK => s.ask_enabled,
                        _ => false,
                    };
                    let h = create_owner_button(parent, hinst, &rc, "", id, body);
                    SetWindowLongPtrW(h, GWLP_USERDATA, on as isize);
                    register(h, page);
                }
                Row::Hotkey(id, _) => {
                    let rc = trailing(&card, &r, VALUE_W, CTRL_H);
                    let hk = match id {
                        IDC_HK_TRANSLATE => &s.hk_translate,
                        IDC_HK_OCR => &s.hk_ocr,
                        IDC_HK_SCREENSHOT => &s.hk_screenshot,
                        IDC_HK_LAYOUT => &s.hk_layout,
                        IDC_HK_EXPLORER_CMD => &s.hk_explorer_cmd,
                        _ => &s.hk_ask,
                    };
                    let h = create_hotkey_field(parent, hinst, &rc, id, hk);
                    register(h, page);
                }
                Row::Folder => {
                    let frame = folder_field(&card, &r);
                    let h = create_edit(
                        parent,
                        hinst,
                        &edit_in(&frame, 0),
                        &s.screenshot_folder,
                        IDC_EDIT_FOLDER,
                        body,
                    );
                    register(h, page);
                    let browse = trailing(&card, &r, BROWSE_W, CTRL_H);
                    let h = create_owner_button(
                        parent,
                        hinst,
                        &browse,
                        i18n::t("settings.btn.browse"),
                        IDC_BTN_BROWSE,
                        button_font,
                    );
                    register(h, page);
                }
                Row::Key(svc) => {
                    let frame = key_field(&card, &r);
                    let edit = create_edit(
                        parent,
                        hinst,
                        &edit_in(&frame, EYE_W),
                        svc.stored(s),
                        svc.edit_id(),
                        body,
                    );
                    // Masked until asked: a settings window ends up in
                    // screenshots and screen shares far more often than
                    // anyone means it to.
                    SendMessageW(edit, EM_SETPASSWORDCHAR, WPARAM(MASK_CHAR), LPARAM(0));
                    register(edit, page);
                    let eye = create_owner_button(
                        parent,
                        hinst,
                        &eye_rect(&frame),
                        "",
                        IDC_EYE + svc.index() as i32,
                        body,
                    );
                    register(eye, page);
                }
            }
        }

        // Cancel / Save, bottom right.  The default button goes last.
        let h = with_geo(|g| g.height);
        let y = h - FOOTER_PAD - BTN_H;
        let save = RECT {
            left: CX1 - BTN_W,
            top: y,
            right: CX1,
            bottom: y + BTN_H,
        };
        let cancel = RECT {
            left: save.left - BTN_GAP - BTN_W,
            right: save.left - BTN_GAP,
            ..save
        };
        let b = create_owner_button(
            parent,
            hinst,
            &cancel,
            i18n::t("settings.btn.cancel"),
            IDC_BTN_CANCEL,
            button_font,
        );
        register(b, ALL_PAGES);
        let b = create_owner_button(
            parent,
            hinst,
            &save,
            i18n::t("settings.btn.save"),
            IDC_BTN_SAVE,
            button_font,
        );
        register(b, ALL_PAGES);
    }
}

fn register(h: HWND, page: usize) {
    CONTROLS.lock().unwrap().push((h.0 as isize, page));
}

unsafe fn show_page(hwnd: HWND, page: usize) {
    unsafe {
        *PAGE.lock().unwrap() = page;
        // Focus can't stay on a control that's about to disappear.
        let focus = GetFocus();
        let controls = CONTROLS.lock().unwrap().clone();
        for (h, p) in &controls {
            let h = HWND(*h as *mut _);
            let visible = *p == page || *p == ALL_PAGES;
            if !visible && h == focus {
                let _ = SetFocus(hwnd);
            }
            let _ = ShowWindow(h, if visible { SW_SHOWNA } else { SW_HIDE });
        }
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe fn create_edit(
    parent: HWND,
    hinst: HINSTANCE,
    rc: &RECT,
    initial: &str,
    id: i32,
    font: HFONT,
) -> HWND {
    unsafe {
        let initial_wide = to_wide(initial);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR(initial_wide.as_ptr()),
            WS_CHILD | WS_TABSTOP | ES_AUTOHSCROLL,
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
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        edit
    }
}

/// An owner-drawn button: push buttons, switches and the show/hide toggles
/// are all this, and `WM_DRAWITEM` tells them apart by id.  Subclassed so it
/// knows when the pointer is over it — plain `BS_OWNERDRAW` never reports
/// hover.
unsafe fn create_owner_button(
    parent: HWND,
    hinst: HINSTANCE,
    rc: &RECT,
    text: &str,
    id: i32,
    font: HFONT,
) -> HWND {
    unsafe {
        let wide = to_wide(text);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_TABSTOP | BS_OWNERDRAW,
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
        SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        let prev = SetWindowLongPtrW(ctrl, GWLP_WNDPROC, button_hover_proc as *const () as isize);
        let mut orig = BUTTON_PROC.lock().unwrap();
        if *orig == 0 {
            *orig = prev;
        }
        ctrl
    }
}

unsafe extern "system" fn button_hover_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEMOVE => {
                let prev = std::mem::replace(&mut *HOT_BUTTON.lock().unwrap(), hwnd.0 as isize);
                if prev != hwnd.0 as isize {
                    if prev != 0 {
                        let _ = InvalidateRect(HWND(prev as *mut _), None, false);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
            }
            WM_MOUSELEAVE => {
                let mut hot = HOT_BUTTON.lock().unwrap();
                if *hot == hwnd.0 as isize {
                    *hot = 0;
                    drop(hot);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            // The owner draw covers every pixel; letting the class erase
            // first is what makes owner-drawn buttons flicker.
            WM_ERASEBKGND => return LRESULT(1),
            _ => {}
        }
        let orig = *BUTTON_PROC.lock().unwrap();
        let orig: WNDPROC = std::mem::transmute(orig);
        CallWindowProcW(orig, hwnd, msg, wp, lp)
    }
}

fn is_hot(hwnd: HWND) -> bool {
    *HOT_BUTTON.lock().unwrap() == hwnd.0 as isize
}

unsafe fn create_hotkey_field(
    parent: HWND,
    hinst: HINSTANCE,
    rc: &RECT,
    id: i32,
    current: &HotkeyConfig,
) -> HWND {
    unsafe {
        register_class(hinst, w!("ScrTransHotkey"), hotkey_proc, IDC_IBEAM);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ScrTransHotkey"),
            PCWSTR::null(),
            WS_CHILD | WS_TABSTOP,
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
        let state = Box::into_raw(Box::new(HotkeyState {
            mods: current.modifiers,
            vk: current.vk,
            focused: false,
        }));
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, state as isize);
        ctrl
    }
}

unsafe fn create_lang_combo(parent: HWND, hinst: HINSTANCE, rc: &RECT, current_code: &str) -> HWND {
    unsafe {
        register_class(hinst, w!("ScrTransLangCombo"), lang_proc, IDC_HAND);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ScrTransLangCombo"),
            PCWSTR::null(),
            WS_CHILD | WS_TABSTOP,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
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
        ctrl
    }
}

/// Registers a window class once per process; later calls are no-ops that
/// fail harmlessly.
unsafe fn register_class(
    hinst: HINSTANCE,
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    cursor: PCWSTR,
) {
    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// `WM_GETDLGCODE` for the custom controls: every key but Tab (and Escape,
/// unless `keep_escape`), so `IsDialogMessage` still moves focus on Tab and
/// closes the window on Escape.
unsafe fn dlg_code(lp: LPARAM, keep_escape: bool) -> LRESULT {
    unsafe {
        let m = lp.0 as *const MSG;
        if !m.is_null() {
            let m = &*m;
            if m.message == WM_KEYDOWN
                && (m.wParam.0 == 0x09 || (m.wParam.0 == 0x1B && !keep_escape))
            {
                return LRESULT(0);
            }
        }
        LRESULT(DLGC_WANTARROWS | DLGC_WANTALLKEYS | DLGC_WANTCHARS)
    }
}

// ============================================================
// Reading control values back into a Settings struct
// ============================================================

unsafe fn read_hotkey(parent: HWND, id: i32) -> HotkeyConfig {
    unsafe {
        let ctrl = GetDlgItem(parent, id).unwrap_or_default();
        let packed = SendMessageW(ctrl, HK_MSG_GET, WPARAM(0), LPARAM(0)).0 as u32;
        HotkeyConfig {
            vk: packed & 0xFFFF,
            modifiers: packed >> 16,
        }
    }
}

/// The text of an edit — masked or not, `GetWindowText` returns what was
/// typed, since the control belongs to this process.
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

unsafe fn read_switch(parent: HWND, id: i32) -> bool {
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

unsafe fn do_save(hwnd: HWND) {
    unsafe {
        let key = |svc: Service| read_edit_text(hwnd, svc.edit_id()).trim().to_string();
        let current = settings::current();
        let new_settings = Settings {
            hk_translate: read_hotkey(hwnd, IDC_HK_TRANSLATE),
            hk_ocr: read_hotkey(hwnd, IDC_HK_OCR),
            hk_screenshot: read_hotkey(hwnd, IDC_HK_SCREENSHOT),
            hk_layout: read_hotkey(hwnd, IDC_HK_LAYOUT),
            hk_explorer_cmd: read_hotkey(hwnd, IDC_HK_EXPLORER_CMD),
            hk_ask: read_hotkey(hwnd, IDC_HK_ASK),
            screenshot_folder: read_edit_text(hwnd, IDC_EDIT_FOLDER),
            punto_enabled: read_switch(hwnd, IDC_CHK_PUNTO),
            taskbar_center_enabled: read_switch(hwnd, IDC_CHK_TASKBAR),
            language: read_selected_language(hwnd),
            deepseek_api_key: key(Service::DeepSeek),
            gemini_api_key: key(Service::Gemini),
            search_api_key: key(Service::Tavily),
            // Carried through untouched: this one is toggled from the ask
            // window, and saving here must not quietly reset it.
            web_search: current.web_search,
            ask_enabled: read_switch(hwnd, IDC_CHK_ASK),
        };

        // Side-channel toggles (registry / context menu).
        autostart::set_enabled(read_switch(hwnd, IDC_CHK_AUTOSTART));
        crate::explorer_cmd::set_menu_enabled(read_switch(hwnd, IDC_CHK_EXPLORER_CMD));

        settings::save(&new_settings);
        *UPDATED_SETTINGS.lock().unwrap() = Some(Box::new(new_settings));
        let _ = DestroyWindow(hwnd);
    }
}

// ============================================================
// Key checks
// ============================================================

/// Kicks off a check of `key` unless it's the one already checked.  Runs on a
/// worker thread — no endpoint used costs quota, but each costs a round-trip,
/// and the window must not freeze on it.
fn start_key_check(svc: Service, key: &str) {
    let key = key.trim().to_string();
    let slot = &KEY_SLOTS[svc.index()];

    if key.is_empty() {
        *slot.status.lock().unwrap() = KeyStatus::Unset;
        slot.checked.lock().unwrap().clear();
        post_key_status(svc);
        return;
    }
    {
        let mut checked = slot.checked.lock().unwrap();
        if *checked == key && *slot.status.lock().unwrap() != KeyStatus::Unset {
            return;
        }
        *checked = key.clone();
    }
    *slot.status.lock().unwrap() = KeyStatus::Checking;
    post_key_status(svc);

    std::thread::spawn(move || {
        let status = svc.check(&key);
        // A key typed after this check started owns the answer, not us.
        if *slot.checked.lock().unwrap() != key {
            return;
        }
        *slot.status.lock().unwrap() = status;
        post_key_status(svc);
    });
}

/// Tells whichever settings window is open *now* to repaint a status — the
/// check may outlive the window that started it, and a reopened window must
/// still hear the answer.
fn post_key_status(svc: Service) {
    if let Some(hwnd) = settings_hwnd() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_KEY_STATUS, WPARAM(svc.index()), LPARAM(0));
        }
    }
}

fn status_look(svc: Service) -> (&'static str, u32) {
    match *KEY_SLOTS[svc.index()].status.lock().unwrap() {
        KeyStatus::Unset => (i18n::t("settings.key.unset"), theme::CLR_HINT),
        KeyStatus::Checking => (i18n::t("settings.key.checking"), theme::CLR_TEXT_DIM),
        KeyStatus::Valid => (i18n::t("settings.key.valid"), theme::CLR_GREEN),
        KeyStatus::Rejected => (i18n::t("settings.key.rejected"), theme::CLR_RED),
        KeyStatus::Unreachable => (i18n::t(svc.offline()), theme::CLR_ORANGE),
    }
}

// ============================================================
// Folder picker
// ============================================================

/// Shows the native pick-folder dialog on a dedicated STA worker thread.
///
/// Why the indirection: `IFileOpenDialog` is a UI component that requires
/// its thread to be an STA (single-threaded apartment).  Our main thread
/// runs as MTA (see `CoInitializeEx(COINIT_MULTITHREADED)` in main.rs),
/// so calling `dialog.Show()` from the UI thread deadlocks Windows.
///
/// We spawn an STA thread, let it run the dialog (which internally pumps
/// its own message loop), and post the chosen path back via a custom
/// `WM_APP_BROWSE_RESULT` message.
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
// Owner-drawn buttons: push buttons, switches, show/hide toggles
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

/// A focus ring is only drawn once the keyboard has been used — Windows
/// sets `ODS_NOFOCUSRECT` until then, so a mouse click doesn't leave one.
fn keyboard_focus(dis: &DrawItemStruct) -> bool {
    dis.item_state & ODS_FOCUS != 0 && dis.item_state & ODS_NOFOCUSRECT == 0
}

/// Everything an owner-drawn control paints goes through one off-screen
/// buffer, so hover changes never flash.
unsafe fn buffered(dis: &DrawItemStruct, draw: impl FnOnce(HDC, &RECT)) {
    unsafe {
        let rc = dis.rc_item;
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mem = CreateCompatibleDC(dis.hdc);
        let bmp = CreateCompatibleBitmap(dis.hdc, w, h);
        let old = SelectObject(mem, bmp);
        let local = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        draw(mem, &local);
        let _ = BitBlt(dis.hdc, rc.left, rc.top, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
    }
}

unsafe fn draw_item(lp: LPARAM) {
    unsafe {
        let dis = &*(lp.0 as *const DrawItemStruct);
        let id = dis.ctl_id as i32;
        if is_switch_id(id) {
            draw_switch(dis);
        } else if is_eye_id(id) {
            draw_eye(dis);
        } else {
            draw_push_button(dis, id);
        }
    }
}

unsafe fn draw_push_button(dis: &DrawItemStruct, id: i32) {
    unsafe {
        let state = if dis.item_state & ODS_SELECTED != 0 {
            button::State::Pressed
        } else if is_hot(dis.hwnd_item) {
            button::State::Hover
        } else {
            button::State::Normal
        };
        let variant = if id == IDC_BTN_SAVE {
            button::Variant::Primary
        } else {
            button::Variant::Secondary
        };
        // What shows through the rounded corners: the browse button sits on
        // a card, the footer buttons on the window.
        let behind = if id == IDC_BTN_BROWSE {
            theme::CLR_CARD
        } else {
            theme::CLR_BG
        };
        let focus = keyboard_focus(dis);

        let mut buf = [0u16; 64];
        let len = GetWindowTextW(dis.hwnd_item, &mut buf) as usize;
        let label = String::from_utf16_lossy(&buf[..len]);

        buffered(dis, |hdc, rc| {
            fill(hdc, rc, behind);
            let fill_c = button::fill(theme::CLR_ACCENT, variant, state);
            let mut style =
                paint::Style::flat(button::radius(rc.bottom - rc.top), fill_c);
            if focus {
                style = style
                    .border(lighten(theme::CLR_ACCENT, 60))
                    .border_width(2);
            }
            paint::round_rect(hdc, rc, &style);
            theme::text(
                hdc,
                &label,
                rc,
                theme::ui_font(FONT_BODY, 500),
                button::text_color(variant, state),
                theme::DT_CENTER_VCENTER,
            );
        });
    }
}

/// A pill track with a white knob at one end or the other.  The state lives
/// in `GWLP_USERDATA`, because `BS_OWNERDRAW` has no check state of its own.
unsafe fn draw_switch(dis: &DrawItemStruct) {
    unsafe {
        let on = GetWindowLongPtrW(dis.hwnd_item, GWLP_USERDATA) != 0;
        let hot = is_hot(dis.hwnd_item);
        let pressed = dis.item_state & ODS_SELECTED != 0;
        let focus = keyboard_focus(dis);

        buffered(dis, |hdc, rc| {
            fill(hdc, rc, theme::CLR_CARD);
            let base = if on { theme::CLR_ACCENT } else { theme::CLR_CTRL };
            let track = if pressed {
                theme::darken(base, 14)
            } else if hot {
                lighten(base, 14)
            } else {
                base
            };
            let h = rc.bottom - rc.top;
            let mut style = paint::Style::flat(h / 2, track);
            if focus {
                style = style.border(lighten(theme::CLR_ACCENT, 60)).border_width(2);
            }
            paint::round_rect(hdc, rc, &style);

            let r = h / 2 - 3;
            let cx = if on { rc.right - 3 - r } else { rc.left + 3 + r };
            paint::circle(hdc, cx, rc.top + h / 2, r, 0x00FF_FFFF);
        });
    }
}

/// The show/hide toggle inside a key field: an eye, struck through while
/// the key is masked.
unsafe fn draw_eye(dis: &DrawItemStruct) {
    unsafe {
        let shown = GetWindowLongPtrW(dis.hwnd_item, GWLP_USERDATA) != 0;
        let hot = is_hot(dis.hwnd_item);
        let focus = keyboard_focus(dis);
        buffered(dis, |hdc, rc| {
            fill(hdc, rc, theme::CLR_FIELD);
            if hot || focus {
                let mut style = paint::Style::flat(5, lighten(theme::CLR_FIELD, 14));
                if focus {
                    style = style.border(theme::CLR_ACCENT);
                }
                paint::round_rect(hdc, rc, &style);
            }
            let color = if shown {
                theme::CLR_ACCENT
            } else if hot {
                theme::CLR_TEXT_BRIGHT
            } else {
                theme::CLR_TEXT_DIM
            };
            theme::glyph(hdc, theme::ICON_EYE, rc, 15, color);
            if !shown {
                let cx = (rc.right - rc.left) as f32 / 2.0;
                let cy = (rc.bottom - rc.top) as f32 / 2.0;
                // The strike needs a gap cut either side of it to read as a
                // slash rather than part of the eye.
                let bg = if hot || focus {
                    lighten(theme::CLR_FIELD, 14)
                } else {
                    theme::CLR_FIELD
                };
                let pts = [(cx - 7.0, cy - 7.0), (cx + 7.0, cy + 7.0)];
                paint::polyline(hdc, rc, &pts, 3.6, bg);
                paint::polyline(hdc, rc, &pts, 1.4, color);
            }
        });
    }
}

unsafe fn fill(hdc: HDC, rc: &RECT, color: u32) {
    unsafe {
        let b = CreateSolidBrush(COLORREF(color));
        let _ = FillRect(hdc, rc, b);
        let _ = DeleteObject(b);
    }
}

unsafe fn toggle_eye(hwnd: HWND, eye: HWND, id: i32) {
    unsafe {
        let shown = GetWindowLongPtrW(eye, GWLP_USERDATA) == 0;
        SetWindowLongPtrW(eye, GWLP_USERDATA, shown as isize);
        let svc = Service::ALL[(id - IDC_EYE) as usize];
        let edit = GetDlgItem(hwnd, svc.edit_id()).unwrap_or_default();
        let ch = if shown { 0 } else { MASK_CHAR };
        SendMessageW(edit, EM_SETPASSWORDCHAR, WPARAM(ch), LPARAM(0));
        let _ = InvalidateRect(edit, None, true);
        let _ = InvalidateRect(eye, None, false);
    }
}

// ============================================================
// WM_PAINT — sidebar, heading, cards, labels, field frames
// ============================================================

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let (w, h) = (client.right, client.bottom);

        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp);

        paint_scene(mem, &client, &ps.rcPaint);

        let dirty = ps.rcPaint;
        let _ = BitBlt(
            hdc,
            dirty.left,
            dirty.top,
            dirty.right - dirty.left,
            dirty.bottom - dirty.top,
            mem,
            dirty.left,
            dirty.top,
            SRCCOPY,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn paint_scene(hdc: HDC, client: &RECT, dirty: &RECT) {
    unsafe {
        use layout::*;
        let page = current_page();
        let hover = *HOVER.lock().unwrap();

        // ── Background: navigation column and content ──
        fill(hdc, client, theme::CLR_BG);
        let sidebar = RECT {
            right: SIDEBAR_W,
            ..*client
        };
        fill(hdc, &sidebar, theme::CLR_SIDEBAR);
        paint::hairline(
            hdc,
            SIDEBAR_W - 1,
            SIDEBAR_W,
            0,
            theme::mix(theme::CLR_SIDEBAR, theme::CLR_SEPARATOR, 160),
        );
        let edge = RECT {
            left: SIDEBAR_W - 1,
            top: 0,
            right: SIDEBAR_W,
            bottom: client.bottom,
        };
        fill(hdc, &edge, theme::mix(theme::CLR_SIDEBAR, theme::CLR_SEPARATOR, 160));

        // ── App mark and name ──
        let mark = RECT {
            left: 20,
            top: 20,
            right: 46,
            bottom: 46,
        };
        paint::round_rect(hdc, &mark, &paint::Style::flat(7, theme::CLR_ACCENT));
        theme::glyph(hdc, theme::ICON_TRANSLATE, &mark, 15, 0x00FF_FFFF);
        theme::text(
            hdc,
            "Screen Translator",
            &RECT {
                left: 56,
                top: 18,
                right: SIDEBAR_W - 12,
                bottom: 48,
            },
            theme::ui_font(FONT_BODY, 600),
            theme::CLR_TEXT_BRIGHT,
            theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
        );

        // ── Navigation ──
        for (i, p) in PAGES.iter().enumerate() {
            let rc = nav_rect(i);
            let selected = i == page;
            if selected || hover == Hover::Nav(i) {
                let plate = if selected {
                    theme::CLR_NAV_SELECTED
                } else {
                    theme::CLR_NAV_HOVER
                };
                paint::round_rect(hdc, &rc, &paint::Style::flat(6, plate));
            }
            if selected {
                // The accent pill on the leading edge, the way Windows 11
                // marks the current page.
                let pill = RECT {
                    left: rc.left,
                    top: rc.top + 10,
                    right: rc.left + 3,
                    bottom: rc.bottom - 10,
                };
                paint::round_rect(hdc, &pill, &paint::Style::flat(1, theme::CLR_ACCENT));
            }
            let icon = RECT {
                left: rc.left + 12,
                right: rc.left + 36,
                ..rc
            };
            let fg = if selected {
                theme::CLR_TEXT_BRIGHT
            } else {
                theme::CLR_TEXT
            };
            theme::glyph(
                hdc,
                p.icon,
                &icon,
                16,
                if selected { theme::CLR_ACCENT } else { theme::CLR_TEXT_DIM },
            );
            theme::text(
                hdc,
                strip_colon(i18n::t(p.title)),
                &RECT {
                    left: rc.left + 44,
                    right: rc.right - 8,
                    ..rc
                },
                theme::ui_font(FONT_BODY, if selected { 600 } else { 400 }),
                fg,
                theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
            );
        }

        // ── Close button, top right ──
        let close = close_rect();
        if hover == Hover::Close {
            fill(hdc, &close, theme::CLR_CLOSE_HOVER);
        }
        theme::glyph(
            hdc,
            theme::ICON_CLOSE,
            &close,
            10,
            if hover == Hover::Close {
                0x00FF_FFFF
            } else {
                theme::CLR_TEXT_DIM
            },
        );

        // ── Page heading ──
        theme::text(
            hdc,
            strip_colon(i18n::t(PAGES[page].title)),
            &RECT {
                left: CX0,
                top: HEADING_Y,
                right: CX1 - CLOSE_W,
                bottom: HEADING_Y + HEADING_H,
            },
            theme::ui_font(FONT_HEADING, 600),
            theme::CLR_TEXT_BRIGHT,
            theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
        );

        // ── Groups ──
        let focused = GetFocus();
        let focused_id = if focused.0.is_null() {
            0
        } else {
            GetDlgCtrlID(focused)
        };
        with_geo(|geo| {
            for g in &geo.pages[page] {
                paint_group(hdc, g, dirty, focused_id);
            }
        });
    }
}

unsafe fn paint_group(hdc: HDC, g: &GroupGeo, dirty: &RECT, focused_id: i32) {
    unsafe {
        use layout::*;
        let body = theme::ui_font(FONT_BODY, 400);

        if let Some((key, y)) = g.title {
            theme::text(
                hdc,
                i18n::t(key),
                &RECT {
                    left: g.card.left + 4,
                    top: y,
                    right: g.card.right,
                    bottom: y + TITLE_H,
                },
                theme::ui_font(FONT_TITLE, 600),
                theme::CLR_TEXT_DIM,
                theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
            );
        }

        if intersects(&g.card, dirty) {
            paint::round_rect(
                hdc,
                &g.card,
                &paint::Style::flat(CARD_R, theme::CLR_CARD).border(theme::CLR_SEPARATOR),
            );
            for r in g.rows.iter().skip(1) {
                paint::hairline(
                    hdc,
                    g.card.left + ROW_PAD,
                    g.card.right - ROW_PAD,
                    r.top,
                    theme::CLR_SEPARATOR,
                );
            }
        }

        for r in &g.rows {
            let label_until = |w: i32| g.card.right - ROW_PAD - w - 16;
            let row_label = |text: &str, right: i32| {
                theme::text(
                    hdc,
                    strip_colon(text),
                    &RECT {
                        left: g.card.left + ROW_PAD,
                        top: r.top,
                        right,
                        bottom: r.top + r.h,
                    },
                    body,
                    theme::CLR_TEXT_BRIGHT,
                    theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
                )
            };
            match r.row {
                Row::Language => {
                    row_label(i18n::t("settings.label.language"), label_until(VALUE_W))
                }
                Row::Hotkey(_, key) => row_label(i18n::t(key), label_until(VALUE_W)),
                Row::Switch(_, key) => row_label(i18n::t(key), label_until(SWITCH_W)),
                Row::Folder => {
                    let frame = folder_field(&g.card, r);
                    draw_field_frame(hdc, &frame, focused_id == IDC_EDIT_FOLDER);
                }
                Row::Key(svc) => {
                    let line = key_label_line(&g.card, r);
                    let right = draw_key_status(hdc, svc, &line);
                    theme::text(
                        hdc,
                        strip_colon(i18n::t(svc.label())),
                        &RECT {
                            right: right - 12,
                            ..line
                        },
                        body,
                        theme::CLR_TEXT_BRIGHT,
                        theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
                    );
                    let frame = key_field(&g.card, r);
                    draw_field_frame(hdc, &frame, focused_id == svc.edit_id());
                }
            }
        }

        if let Some((key, rc)) = g.footnote {
            theme::text(
                hdc,
                i18n::t(key),
                &rc,
                theme::ui_font(FONT_META, 400),
                theme::CLR_HINT,
                theme::DT_WRAP,
            );
        }
    }
}

/// A key's status, right-aligned on its label line: a dot in the state's
/// colour and a word.  Returns where it starts, for the label to stop short.
unsafe fn draw_key_status(hdc: HDC, svc: Service, line: &RECT) -> i32 {
    unsafe {
        let (text, color) = status_look(svc);
        let font = theme::ui_font(FONT_META, 500);
        let (tw, _) = theme::measure(hdc, text, font);
        let text_rc = RECT {
            left: line.right - tw,
            ..*line
        };
        theme::text(hdc, text, &text_rc, font, color, theme::DT_RIGHT_VCENTER);
        let cy = (line.top + line.bottom) / 2;
        let dot_cx = text_rc.left - 9;
        paint::circle(hdc, dot_cx, cy, 4, color);
        dot_cx - 4
    }
}

/// Recessed fill and a border that turns accent on focus — shared by the
/// native edits (framed here) and the custom hotkey / language controls
/// (which paint their own).
fn field_style(focused: bool) -> paint::Style {
    paint::Style::flat(6, theme::CLR_FIELD)
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

fn strip_colon(s: &str) -> &str {
    s.trim_end().trim_end_matches([':', '：']).trim_end()
}

/// Repaints just the field frames on the current page, so focus changes move
/// the ring without touching anything else.
unsafe fn invalidate_frames(hwnd: HWND) {
    unsafe {
        for (_, rc) in field_frames(current_page()) {
            let _ = InvalidateRect(hwnd, Some(&rc), false);
        }
    }
}

unsafe fn set_hover(hwnd: HWND, new: Hover) {
    unsafe {
        let old = std::mem::replace(&mut *HOVER.lock().unwrap(), new);
        if old == new {
            return;
        }
        for h in [old, new] {
            let rc = match h {
                Hover::Nav(i) => nav_rect(i),
                Hover::Close => close_rect(),
                Hover::None => continue,
            };
            let _ = InvalidateRect(hwnd, Some(&rc), false);
        }
    }
}

fn hover_at(x: i32, y: i32) -> Hover {
    if contains(&close_rect(), x, y) {
        return Hover::Close;
    }
    (0..PAGES.len())
        .find(|&i| contains(&nav_rect(i), x, y))
        .map_or(Hover::None, Hover::Nav)
}

// ============================================================
// Window procedure
// ============================================================

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // The whole window is client area — see `open`.
            WM_NCCALCSIZE if wp.0 != 0 => LRESULT(0),
            // Without a caption to repaint, the default would flash one in.
            WM_NCACTIVATE => DefWindowProcW(hwnd, msg, wp, LPARAM(-1)),
            WM_NCHITTEST => {
                let mut pt = POINT {
                    x: (lp.0 & 0xFFFF) as i16 as i32,
                    y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut pt);
                let over_nav = (0..PAGES.len()).any(|i| contains(&nav_rect(i), pt.x, pt.y));
                if pt.y < layout::DRAG_H && !contains(&close_rect(), pt.x, pt.y) && !over_nav {
                    LRESULT(HTCAPTION as isize)
                } else {
                    LRESULT(HTCLIENT as isize)
                }
            }

            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, (lp.0 >> 16) as i16 as i32);
                set_hover(hwnd, hover_at(x, y));
                let mut tracking = TRACKING.lock().unwrap();
                if !*tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    *tracking = TrackMouseEvent(&mut tme).is_ok();
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                *TRACKING.lock().unwrap() = false;
                set_hover(hwnd, Hover::None);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, (lp.0 >> 16) as i16 as i32);
                match hover_at(x, y) {
                    Hover::Close => {
                        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    Hover::Nav(i) => show_page(hwnd, i),
                    Hover::None => {
                        // A click on a field's padding still lands in it.
                        let hit = field_frames(current_page())
                            .into_iter()
                            .find(|(_, rc)| contains(rc, x, y));
                        match hit {
                            Some((id, _)) => {
                                let _ = SetFocus(GetDlgItem(hwnd, id).unwrap_or_default());
                            }
                            None => {
                                let _ = SetFocus(hwnd);
                            }
                        }
                    }
                }
                LRESULT(0)
            }

            WM_DRAWITEM => {
                draw_item(lp);
                LRESULT(1)
            }

            WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkColor(hdc, COLORREF(theme::CLR_FIELD));
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));
                LRESULT(res().as_ref().unwrap().field_brush().0 as isize)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT));
                LRESULT(res().as_ref().unwrap().card_brush().0 as isize)
            }

            WM_COMMAND => {
                let code = ((wp.0 >> 16) & 0xFFFF) as u16;
                let id = (wp.0 & 0xFFFF) as i32;
                if code == EN_SETFOCUS || code == EN_KILLFOCUS {
                    invalidate_frames(hwnd);
                }
                if let Some(svc) = Service::from_edit_id(id) {
                    match code {
                        // Re-check once the user is done typing — on leaving
                        // the field, or after a pause.
                        EN_KILLFOCUS => {
                            let _ = KillTimer(hwnd, TIMER_KEY + svc.index());
                            start_key_check(svc, &read_edit_text(hwnd, id));
                        }
                        EN_CHANGE => {
                            let _ = SetTimer(hwnd, TIMER_KEY + svc.index(), KEY_DEBOUNCE_MS, None);
                        }
                        _ => {}
                    }
                }
                // BN_CLICKED is 0.
                if code == 0 && is_switch_id(id) {
                    let ctrl = HWND(lp.0 as *mut _);
                    let cur = GetWindowLongPtrW(ctrl, GWLP_USERDATA);
                    SetWindowLongPtrW(ctrl, GWLP_USERDATA, (cur == 0) as isize);
                    let _ = InvalidateRect(ctrl, None, false);
                }
                if code == 0 && is_eye_id(id) {
                    toggle_eye(hwnd, HWND(lp.0 as *mut _), id);
                }
                match id {
                    IDC_BTN_SAVE | IDOK_CMD => do_save(hwnd),
                    IDC_BTN_CANCEL | IDCANCEL_CMD => {
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_BTN_BROWSE => browse_folder(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }

            WM_TIMER => {
                let idx = wp.0.wrapping_sub(TIMER_KEY);
                if let Some(&svc) = Service::ALL.get(idx) {
                    let _ = KillTimer(hwnd, wp.0);
                    start_key_check(svc, &read_edit_text(hwnd, svc.edit_id()));
                }
                LRESULT(0)
            }

            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                *SETTINGS_HWND.lock().unwrap() = 0;
                CONTROLS.lock().unwrap().clear();
                *HOT_BUTTON.lock().unwrap() = 0;
                *TRACKING.lock().unwrap() = false;
                LRESULT(0)
            }

            m if m == WM_APP_BROWSE_RESULT => {
                apply_browse_result(hwnd, lp);
                LRESULT(0)
            }
            m if m == WM_APP_KEY_STATUS => {
                if let Some(&svc) = Service::ALL.get(wp.0) {
                    if let Some((page, rc)) = key_status_rect(svc) {
                        if page == current_page() {
                            let _ = InvalidateRect(hwnd, Some(&rc), false);
                        }
                    }
                }
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ============================================================
// Hotkey field — captures a key combination and shows it as keycaps.
// Get/set via HK_MSG_{GET,SET}.
// ============================================================

struct HotkeyState {
    mods: u32,
    vk: u32,
    focused: bool,
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
            WM_GETDLGCODE => dlg_code(lp, false),
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                let _ = SetFocus(hwnd);
                LRESULT(0)
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                if let Some(s) = hotkey_state(hwnd) {
                    s.focused = msg == WM_SETFOCUS;
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
                // Tab and Escape are reserved for UI navigation / dismiss.
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
                let mut m = 0u32;
                if GetKeyState(0x11) < 0 {
                    m |= 0x0002;
                }
                if GetKeyState(0x12) < 0 {
                    m |= 0x0001;
                }
                if GetKeyState(0x10) < 0 {
                    m |= 0x0004;
                }
                if let Some(s) = hotkey_state(hwnd) {
                    s.mods = m;
                    s.vk = vk;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            // Alt+key arrives as a system character too; swallowing it keeps
            // the default handler from beeping.
            WM_SYSCHAR | WM_CHAR => LRESULT(0),
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

/// Paints a custom control double-buffered: `draw` gets the client rect in
/// an off-screen DC already filled with the card colour the control sits on.
unsafe fn paint_control(hwnd: HWND, hdc: HDC, draw: impl FnOnce(HDC, &RECT)) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old = SelectObject(mem, bmp);
        fill(mem, &rc, theme::CLR_CARD);
        draw(mem, &rc);
        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
    }
}

unsafe fn paint_hotkey(hwnd: HWND, hdc: HDC, mods: u32, vk: u32, focused: bool) {
    unsafe {
        paint_control(hwnd, hdc, |dc, rc| {
            paint::round_rect(dc, rc, &field_style(focused));

            if vk == 0 {
                let hint = if focused {
                    i18n::t("settings.hotkey.press")
                } else {
                    i18n::t("settings.hotkey.none")
                };
                theme::text(
                    dc,
                    hint,
                    &RECT {
                        left: rc.left + 12,
                        right: rc.right - 10,
                        ..*rc
                    },
                    theme::ui_font(FONT_BODY - 1, 400),
                    theme::CLR_HINT,
                    theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
                );
                return;
            }

            // One keycap per key of the combination.
            let combo = HotkeyConfig {
                modifiers: mods,
                vk,
            }
            .display();
            let font = theme::ui_font(FONT_META, 600);
            let cap_h = rc.bottom - rc.top - 10;
            let top = rc.top + 5;
            let mut x = rc.left + 6;
            for key in combo.split('+').filter(|k| !k.is_empty()) {
                let (tw, _) = theme::measure(dc, key, font);
                let cap = RECT {
                    left: x,
                    top,
                    right: (x + tw + 14).min(rc.right - 6),
                    bottom: top + cap_h,
                };
                if cap.right <= cap.left {
                    break;
                }
                paint::round_rect(
                    dc,
                    &cap,
                    &paint::Style::flat(4, theme::CLR_CTRL)
                        .border(lighten(theme::CLR_CTRL, 10)),
                );
                theme::text(
                    dc,
                    key,
                    &cap,
                    font,
                    theme::CLR_TEXT_BRIGHT,
                    theme::DT_CENTER_VCENTER,
                );
                x = cap.right + 4;
            }
        });
    }
}

// ============================================================
// Language picker — a field that opens a floating list.  The list is a
// separate layered popup that takes mouse capture so clicks outside close it.
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
    /// The list card's top-left on screen, and its size.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

const LANG_ITEM_H: i32 = 30;
const LANG_PAD: i32 = 6;

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
            WM_GETDLGCODE => {
                let open = lang_state(hwnd).is_some_and(|s| s.popup != 0);
                dlg_code(lp, open)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => {
                let _ = SetFocus(hwnd);
                if lang_state(hwnd).is_some_and(|s| s.popup != 0) {
                    let _ = ReleaseCapture();
                } else {
                    open_lang_popup(hwnd);
                }
                LRESULT(0)
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                if let Some(s) = lang_state(hwnd) {
                    s.focused = msg == WM_SETFOCUS;
                }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u32;
                let popup_open = lang_state(hwnd).is_some_and(|s| s.popup != 0);
                let n = Language::all().len();
                match vk {
                    0x1B if popup_open => {
                        let _ = ReleaseCapture();
                    }
                    0x0D | 0x20 if popup_open => {
                        let _ = ReleaseCapture();
                    }
                    0x0D | 0x20 => open_lang_popup(hwnd),
                    // Up / Down step through the list, open or not.
                    0x26 | 0x28 => {
                        if let Some(s) = lang_state(hwnd) {
                            s.selected = if vk == 0x26 {
                                s.selected.saturating_sub(1)
                            } else {
                                (s.selected + 1).min(n - 1)
                            };
                            if s.popup != 0 {
                                let popup = HWND(s.popup as *mut _);
                                if let Some(ps) = lang_popup_state(popup) {
                                    ps.hover = s.selected;
                                }
                                present_lang_popup(popup);
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
                let (selected, focused, open) = lang_state(hwnd)
                    .map(|s| (s.selected, s.focused, s.popup != 0))
                    .unwrap_or((0, false, false));
                paint_lang(hwnd, hdc, selected, focused || open);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            m if m == LANG_MSG_GET => lang_state(hwnd)
                .map(|s| LRESULT(s.selected as isize))
                .unwrap_or(LRESULT(0)),
            m if m == LANG_MSG_SET => {
                if let Some(s) = lang_state(hwnd) {
                    s.selected = wp.0;
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
        paint_control(hwnd, hdc, |dc, rc| {
            paint::round_rect(dc, rc, &field_style(focused));
            let name = Language::all()
                .get(selected)
                .map(|l| l.native_name())
                .unwrap_or("");
            theme::text(
                dc,
                name,
                &RECT {
                    left: rc.left + 12,
                    right: rc.right - 32,
                    ..*rc
                },
                theme::ui_font(FONT_BODY, 400),
                theme::CLR_TEXT_BRIGHT,
                theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
            );
            theme::glyph(
                dc,
                theme::ICON_CHEVRON_DOWN,
                &RECT {
                    left: rc.right - 30,
                    right: rc.right - 8,
                    ..*rc
                },
                10,
                theme::CLR_TEXT_DIM,
            );
        });
    }
}

unsafe fn open_lang_popup(owner: HWND) {
    unsafe {
        if lang_state(owner).is_some_and(|s| s.popup != 0) {
            return;
        }
        let Ok(hmodule) = GetModuleHandleW(None) else {
            return;
        };
        let hinst = HINSTANCE(hmodule.0);
        register_class(hinst, w!("ScrTransLangPopup2"), lang_popup_proc, IDC_ARROW);

        let mut rc = RECT::default();
        let _ = GetWindowRect(owner, &mut rc);
        let n = Language::all().len() as i32;
        let w = rc.right - rc.left;
        let h = LANG_ITEM_H * n + LANG_PAD * 2;

        // Below the field, or above it when the screen runs out.
        let mon = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let screen_bottom = if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork.bottom
        } else {
            GetSystemMetrics(SM_CYSCREEN)
        };
        let y = if rc.bottom + 6 + h > screen_bottom {
            rc.top - 6 - h
        } else {
            rc.bottom + 6
        };

        let m = paint::CARD_MARGIN;
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
            w!("ScrTransLangPopup2"),
            w!(""),
            WS_POPUP,
            rc.left - m,
            y - m,
            w + 2 * m,
            h + 2 * m,
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
            x: rc.left,
            y,
            w,
            h,
        }));
        SetWindowLongPtrW(popup, GWLP_USERDATA, state as isize);

        if let Some(s) = lang_state(owner) {
            s.popup = popup.0 as isize;
        }
        let _ = InvalidateRect(owner, None, false);

        present_lang_popup(popup);
        let _ = ShowWindow(popup, SW_SHOWNA);
        // Capture so clicks outside the popup close it.  WM_CAPTURECHANGED
        // is the canonical "close yourself" signal.
        SetCapture(popup);
    }
}

/// The list item under a point in popup client coordinates, if any.
unsafe fn lang_item_at(hwnd: HWND, lp: LPARAM) -> Option<usize> {
    unsafe {
        let s = lang_popup_state(hwnd)?;
        let x = (lp.0 & 0xFFFF) as i16 as i32 - paint::CARD_MARGIN;
        let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32 - paint::CARD_MARGIN;
        if x < 0 || x >= s.w || y < LANG_PAD || y >= s.h - LANG_PAD {
            return None;
        }
        let idx = ((y - LANG_PAD) / LANG_ITEM_H) as usize;
        (idx < Language::all().len()).then_some(idx)
    }
}

unsafe extern "system" fn lang_popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE => {
                let hover = lang_item_at(hwnd, lp).unwrap_or(usize::MAX);
                let changed = lang_popup_state(hwnd).is_some_and(|s| {
                    let c = s.hover != hover;
                    s.hover = hover;
                    c
                });
                if changed {
                    present_lang_popup(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                if let Some(idx) = lang_item_at(hwnd, lp) {
                    let owner = lang_popup_state(hwnd).map(|s| s.owner).unwrap_or(0);
                    if let Some(os) = lang_state(HWND(owner as *mut _)) {
                        os.selected = idx;
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
                // Layered: the content goes up through UpdateLayeredWindow,
                // so there's nothing to paint here but the validation.
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
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

unsafe fn present_lang_popup(hwnd: HWND) {
    unsafe {
        let Some(s) = lang_popup_state(hwnd) else {
            return;
        };
        let owner = HWND(s.owner as *mut _);
        let selected = lang_state(owner).map(|os| os.selected).unwrap_or(usize::MAX);
        let (hover, w) = (s.hover, s.w);
        let card = paint::Card {
            w: s.w,
            h: s.h,
            radius: 10,
            fill: theme::CLR_ELEVATED,
            border: theme::CLR_SEPARATOR,
        };
        paint::present_card(hwnd, s.x, s.y, &card, 255, |dc| {
            for (i, lang) in Language::all().iter().enumerate() {
                let top = LANG_PAD + i as i32 * LANG_ITEM_H;
                let item = RECT {
                    left: LANG_PAD,
                    top,
                    right: w - LANG_PAD,
                    bottom: top + LANG_ITEM_H,
                };
                let is_hover = i == hover;
                let is_selected = i == selected;
                if is_hover {
                    paint::round_rect(dc, &item, &paint::Style::flat(6, theme::CLR_ACCENT));
                }
                if is_selected {
                    let tick = RECT {
                        left: item.left + 4,
                        right: item.left + 26,
                        ..item
                    };
                    let color = if is_hover {
                        0x00FF_FFFF
                    } else {
                        theme::CLR_ACCENT
                    };
                    theme::glyph(dc, theme::ICON_CHECK, &tick, 12, color);
                }
                theme::text(
                    dc,
                    lang.native_name(),
                    &RECT {
                        left: item.left + 30,
                        right: item.right - 8,
                        ..item
                    },
                    theme::ui_font(FONT_BODY, if is_selected { 600 } else { 400 }),
                    if is_hover {
                        0x00FF_FFFF
                    } else {
                        theme::CLR_TEXT_BRIGHT
                    },
                    theme::DT_LEFT_VCENTER | theme::DT_ELLIPSIS,
                );
            }
        });
    }
}
