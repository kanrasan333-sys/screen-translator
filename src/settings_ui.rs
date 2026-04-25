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
use crate::i18n::{self, Language};
use crate::settings::{self, HotkeyConfig, Settings};
use crate::theme;
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

const BS_OWNERDRAW:   WINDOW_STYLE = WINDOW_STYLE(0x000B);
const ES_AUTOHSCROLL: WINDOW_STYLE = WINDOW_STYLE(0x0080);

const EM_SETMARGINS: u32 = 0x00D3;

// Custom messages for our hand-rolled hotkey / language controls.
// WPARAM/LRESULT both encode (vk | mods << 16) for hotkeys, and a raw index
// into Language::all() for the language combo.
const HK_MSG_GET:    u32 = WM_USER + 100;
const HK_MSG_SET:    u32 = WM_USER + 101;
const LANG_MSG_GET:  u32 = WM_USER + 200;
const LANG_MSG_SET:  u32 = WM_USER + 201;

// ============================================================
// Control IDs
// ============================================================

const IDC_HK_TRANSLATE:    i32 = 101;
const IDC_HK_OCR:          i32 = 102;
const IDC_HK_SCREENSHOT:   i32 = 103;
const IDC_HK_LAYOUT:       i32 = 104;
const IDC_HK_EXPLORER_CMD: i32 = 105;
const IDC_EDIT_FOLDER:     i32 = 106;
const IDC_BTN_BROWSE:      i32 = 107;
const IDC_BTN_SAVE:        i32 = 108;
const IDC_BTN_CANCEL:      i32 = 109;
const IDC_CHK_PUNTO:       i32 = 110;
const IDC_CHK_TASKBAR:     i32 = 111;
const IDC_CHK_AUTOSTART:   i32 = 112;
const IDC_COMBO_LANG:      i32 = 114;
const IDC_CHK_EXPLORER_CMD: i32 = 115;
const IDC_EDIT_DEEPSEEK:   i32 = 116;

// Section headers (`WM_CTLCOLORSTATIC` gives these the accent colour).
const IDC_SEC_GENERAL: i32 = 200;
const IDC_SEC_HOTKEYS: i32 = 201;
const IDC_SEC_FOLDER:  i32 = 202;
const IDC_SEC_FUNC:    i32 = 203;
const IDC_SEC_TRANS:   i32 = 204;

// Private window message: the worker thread running the folder-picker
// posts the chosen path back to the settings window via this message.
// LPARAM carries a `Box::into_raw(Box<String>)` pointer, the handler
// converts it back via Box::from_raw and frees it.
const WM_APP_BROWSE_RESULT: u32 = WM_APP + 1;

// ============================================================
// Layout — all geometry in one place.  Dimensions in px.
// ============================================================

mod layout {
    pub const WIN_W:      i32 = 560;
    pub const Y_HERO:     i32 = 18;
    pub const HERO_H:     i32 = 82;

    pub const MARGIN:  i32 = 28;
    pub const LABEL_W: i32 = 210;
    pub const INPUT_X: i32 = MARGIN + LABEL_W + 12;
    pub const INPUT_W: i32 = WIN_W - INPUT_X - MARGIN;
    pub const INPUT_H: i32 = 32;

    // Section rhythm: header + gap, then row content, then gap.
    pub const SEC_HEADER_H: i32 = 22;
    pub const SEC_PAD_TOP:  i32 = 10;
    pub const SEC_PAD_BOT:  i32 = 18;

    pub const HK_STEP:  i32 = 42;
    pub const CHK_STEP: i32 = 32;

    pub const FOLDER_W: i32 = WIN_W - MARGIN * 2 - 96; // edit + 8 gap + browse(88)
    pub const BROWSE_W: i32 = 88;

    pub const BTN_W:   i32 = 148;
    pub const BTN_H:   i32 = 40;
    pub const BTN_GAP: i32 = 14;

    pub const Y_SEC_GEN:    i32 = Y_HERO + HERO_H + 16;
    pub const Y_LANG:       i32 = Y_SEC_GEN + SEC_HEADER_H + SEC_PAD_TOP;

    pub const Y_SEP1:       i32 = Y_LANG + INPUT_H + SEC_PAD_BOT;
    pub const Y_SEC_TRANS:  i32 = Y_SEP1 + 12;
    pub const Y_DEEPSEEK:   i32 = Y_SEC_TRANS + SEC_HEADER_H + SEC_PAD_TOP;
    pub const HINT_H:       i32 = 18;
    pub const Y_DEEPSEEK_HINT: i32 = Y_DEEPSEEK + INPUT_H + 6;

    pub const Y_SEP2:       i32 = Y_DEEPSEEK_HINT + HINT_H + SEC_PAD_BOT;
    pub const Y_SEC_HK:     i32 = Y_SEP2 + 12;
    pub const Y_HK_START:   i32 = Y_SEC_HK + SEC_HEADER_H + SEC_PAD_TOP;
    pub const Y_HK_END:     i32 = Y_HK_START + HK_STEP * 5;

    pub const Y_SEP3:       i32 = Y_HK_END + SEC_PAD_BOT;
    pub const Y_SEC_FOLDER: i32 = Y_SEP3 + 12;
    pub const Y_FOLDER:     i32 = Y_SEC_FOLDER + SEC_HEADER_H + SEC_PAD_TOP;

    pub const Y_SEP4:       i32 = Y_FOLDER + INPUT_H + SEC_PAD_BOT;
    pub const Y_SEC_FUNC:   i32 = Y_SEP4 + 12;
    pub const Y_CHK_START:  i32 = Y_SEC_FUNC + SEC_HEADER_H + SEC_PAD_TOP;
    pub const Y_CHK_END:    i32 = Y_CHK_START + CHK_STEP * 4;

    pub const Y_BUTTONS:    i32 = Y_CHK_END + 22;

    // Client-area height; system adds the caption on top when we use
    // WS_CAPTION (see AdjustWindowRectEx call in open()).
    pub const WIN_H_CLIENT: i32 = Y_BUTTONS + BTN_H + 22;
}

// ============================================================
// Colour palette (beyond theme::*)
// ============================================================

const CLR_FIELD_BG:       u32 = 0x0035_3535;
const CLR_FIELD_BORDER:   u32 = 0x0060_6060;
const CLR_FIELD_BORDER_FOCUS: u32 = theme::CLR_ACCENT;
const CLR_BTN_BG:         u32 = 0x003C_3C3C;
const CLR_BTN_BG_HOVER:   u32 = 0x004A_4A4A;
const CLR_BTN_BG_PRESS:   u32 = 0x002C_2C2C;
const CLR_BTN_BORDER:     u32 = 0x0070_7070;
const CLR_HERO_BG:        u32 = 0x002F_2D2A;
const CLR_HERO_BORDER:    u32 = 0x0049_433A;
const CLR_HERO_SUBTITLE:  u32 = 0x00B8_B1A8;
const CLR_HERO_DECOR:     u32 = 0x0041_3830;

// ============================================================
// Resources — GDI objects cached for the lifetime of the window.
// ============================================================

struct Resources {
    bg_brush:     isize,
    field_brush:  isize,
    font_body:    isize,
    font_section: isize,
    font_title:   isize,
    font_meta:    isize,
    font_badge:   isize,
    font_button:  isize,
}

impl Resources {
    fn new() -> Self {
        unsafe {
            Self {
                bg_brush:     CreateSolidBrush(COLORREF(theme::CLR_BG)).0 as isize,
                field_brush:  CreateSolidBrush(COLORREF(CLR_FIELD_BG)).0 as isize,
                font_body:    make_font(-14, 400).0 as isize,
                font_section: make_font(-13, 700).0 as isize,
                font_title:   make_font(-24, 700).0 as isize,
                font_meta:    make_font(-12, 500).0 as isize,
                font_badge:   make_font(-16, 700).0 as isize,
                font_button:  make_font(-14, 600).0 as isize,
            }
        }
    }
    fn bg_brush(&self)     -> HBRUSH { HBRUSH(self.bg_brush as *mut _) }
    fn field_brush(&self)  -> HBRUSH { HBRUSH(self.field_brush as *mut _) }
    fn font_body(&self)    -> HFONT  { HFONT(self.font_body as *mut _) }
    fn font_section(&self) -> HFONT  { HFONT(self.font_section as *mut _) }
    fn font_title(&self)   -> HFONT  { HFONT(self.font_title as *mut _) }
    fn font_meta(&self)    -> HFONT  { HFONT(self.font_meta as *mut _) }
    fn font_badge(&self)   -> HFONT  { HFONT(self.font_badge as *mut _) }
    fn font_button(&self)  -> HFONT  { HFONT(self.font_button as *mut _) }
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI"))
    }
}

// ============================================================
// Window-level state
// ============================================================

static SETTINGS_HWND:     Mutex<isize>            = Mutex::new(0);
static UPDATED_SETTINGS:  Mutex<Option<Box<Settings>>> = Mutex::new(None);
static RES:               Mutex<Option<Box<Resources>>> = Mutex::new(None);

fn res() -> std::sync::MutexGuard<'static, Option<Box<Resources>>> {
    RES.lock().unwrap()
}

fn ensure_res() {
    let mut g = RES.lock().unwrap();
    if g.is_none() { *g = Some(Box::new(Resources::new())); }
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

        let Some(hmodule) = GetModuleHandleW(None).ok() else { return };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransSettings6");

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
        let mut rc = RECT { left: 0, top: 0, right: layout::WIN_W, bottom: layout::WIN_H_CLIENT };
        let _ = AdjustWindowRectEx(&mut rc, style, false, ex_style);
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;

        let hwnd = CreateWindowExW(
            ex_style,
            class, PCWSTR(title.as_ptr()),
            style,
            (sw - win_w) / 2, (sh - win_h) / 2, win_w, win_h,
            HWND::default(), HMENU::default(), hinstance, None,
        ).unwrap_or_default();

        if hwnd.0.is_null() { return; }

        *SETTINGS_HWND.lock().unwrap() = hwnd.0 as isize;
        create_controls(hwnd, hinstance, current);
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

        // ── GENERAL section ──
        section_header(parent, hinst, r, MARGIN, Y_SEC_GEN,
            i18n::t("settings.section.general"), IDC_SEC_GENERAL);
        create_label(parent, hinst, r.font_body(),
            MARGIN, Y_LANG + 5, LABEL_W, 20,
            i18n::t("settings.label.language"), 0);
        create_lang_combo(parent, hinst, r.font_body(),
            INPUT_X, Y_LANG, INPUT_W, &s.language);

        // ── TRANSLATION section ──
        section_header(parent, hinst, r, MARGIN, Y_SEC_TRANS,
            i18n::t("settings.section.translation"), IDC_SEC_TRANS);
        create_label(parent, hinst, r.font_body(),
            MARGIN, Y_DEEPSEEK + 5, LABEL_W, 20,
            i18n::t("settings.label.deepseek_key"), 0);
        create_edit_field(parent, hinst, r.font_body(),
            INPUT_X, Y_DEEPSEEK, INPUT_W, INPUT_H,
            &s.deepseek_api_key, IDC_EDIT_DEEPSEEK);
        create_label(parent, hinst, r.font_meta(),
            MARGIN, Y_DEEPSEEK_HINT, WIN_W - MARGIN * 2, HINT_H,
            i18n::t("settings.hint.deepseek"), 0);

        // ── HOTKEYS ──
        section_header(parent, hinst, r, MARGIN, Y_SEC_HK,
            i18n::t("settings.section.hotkeys"), IDC_SEC_HOTKEYS);

        let hotkeys: &[(&str, i32, &HotkeyConfig)] = &[
            (i18n::t("settings.hotkey.translate"),    IDC_HK_TRANSLATE,    &s.hk_translate),
            (i18n::t("settings.hotkey.ocr"),          IDC_HK_OCR,          &s.hk_ocr),
            (i18n::t("settings.hotkey.screenshot"),   IDC_HK_SCREENSHOT,   &s.hk_screenshot),
            (i18n::t("settings.hotkey.layout"),       IDC_HK_LAYOUT,       &s.hk_layout),
            (i18n::t("settings.hotkey.explorer_cmd"), IDC_HK_EXPLORER_CMD, &s.hk_explorer_cmd),
        ];

        for (i, &(label, id, hk)) in hotkeys.iter().enumerate() {
            let y = Y_HK_START + (i as i32) * HK_STEP;
            create_label(parent, hinst, r.font_body(), MARGIN, y + 5, LABEL_W, 20, label, 0);
            create_hotkey_field(parent, hinst, r.font_body(), INPUT_X, y, INPUT_W, INPUT_H, id, hk);
        }

        // ── SCREENSHOT FOLDER ──
        section_header(parent, hinst, r, MARGIN, Y_SEC_FOLDER,
            i18n::t("settings.section.folder"), IDC_SEC_FOLDER);

        create_edit_field(parent, hinst, r.font_body(),
            MARGIN, Y_FOLDER, FOLDER_W, INPUT_H,
            &s.screenshot_folder, IDC_EDIT_FOLDER);
        create_od_button(parent, hinst, r.font_button(),
            MARGIN + FOLDER_W + 10, Y_FOLDER, BROWSE_W, INPUT_H,
            i18n::t("settings.btn.browse"), IDC_BTN_BROWSE);

        // ── FEATURES ──
        section_header(parent, hinst, r, MARGIN, Y_SEC_FUNC,
            i18n::t("settings.section.functions"), IDC_SEC_FUNC);

        let features: &[(&str, i32, bool)] = &[
            (i18n::t("settings.checkbox.punto"),        IDC_CHK_PUNTO,        s.punto_enabled),
            (i18n::t("settings.checkbox.taskbar"),      IDC_CHK_TASKBAR,      s.taskbar_center_enabled),
            (i18n::t("settings.checkbox.autostart"),    IDC_CHK_AUTOSTART,    autostart::is_enabled()),
            (i18n::t("settings.checkbox.explorer_cmd"), IDC_CHK_EXPLORER_CMD, crate::explorer_cmd::is_menu_enabled()),
        ];
        for (i, &(label, id, checked)) in features.iter().enumerate() {
            create_checkbox(parent, hinst, r.font_body(),
                MARGIN, Y_CHK_START + (i as i32) * CHK_STEP,
                WIN_W - MARGIN * 2, 24, label, id, checked);
        }

        // ── Save / Cancel ──
        let bx = (WIN_W - BTN_W * 2 - BTN_GAP) / 2;
        create_od_button(parent, hinst, r.font_button(),
            bx, Y_BUTTONS, BTN_W, BTN_H,
            i18n::t("settings.btn.save"), IDC_BTN_SAVE);
        create_od_button(parent, hinst, r.font_button(),
            bx + BTN_W + BTN_GAP, Y_BUTTONS, BTN_W, BTN_H,
            i18n::t("settings.btn.cancel"), IDC_BTN_CANCEL);
    }
}

// ── Reusable control-creation helpers ──

unsafe fn section_header(
    parent: HWND, hinst: HINSTANCE, r: &Resources,
    x: i32, y: i32, text: &str, id: i32,
) {
    unsafe {
        create_label(parent, hinst, r.font_section(), x, y, 260, layout::SEC_HEADER_H, text, id);
    }
}

unsafe fn create_label(
    parent: HWND, hinst: HINSTANCE, font: HFONT,
    x: i32, y: i32, w: i32, h: i32, text: &str, id: i32,
) {
    unsafe {
        let class = to_wide("STATIC");
        let wide = to_wide(text);
        let hmenu = if id != 0 { HMENU(id as *mut _) } else { HMENU::default() };
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            x, y, w, h,
            parent, hmenu, hinst, None,
        ).unwrap_or_default();
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

unsafe fn create_edit_field(
    parent: HWND, hinst: HINSTANCE, font: HFONT,
    x: i32, y: i32, w: i32, h: i32, initial: &str, id: i32,
) {
    unsafe {
        let class = to_wide("EDIT");
        let initial_wide = to_wide(initial);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(initial_wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL,
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        let _ = SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        // Internal padding so text doesn't touch edges.
        let _ = SendMessageW(edit, EM_SETMARGINS, WPARAM(3), LPARAM(8 | (8 << 16)));
    }
}

unsafe fn create_hotkey_field(
    parent: HWND, hinst: HINSTANCE, _font: HFONT,
    x: i32, y: i32, w: i32, h: i32, id: i32, current: &HotkeyConfig,
) {
    unsafe {
        register_hotkey_class(hinst);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("ScrTransHotkey"), PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        let state = Box::into_raw(Box::new(HotkeyState {
            mods: current.modifiers, vk: current.vk, focused: false,
        }));
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, state as isize);
    }
}

unsafe fn create_od_button(
    parent: HWND, hinst: HINSTANCE, font: HFONT,
    x: i32, y: i32, w: i32, h: i32, text: &str, id: i32,
) {
    unsafe {
        let class = to_wide("BUTTON");
        let wide = to_wide(text);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW,
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

unsafe fn create_checkbox(
    parent: HWND, hinst: HINSTANCE, _font: HFONT,
    x: i32, y: i32, w: i32, h: i32, text: &str, id: i32, checked: bool,
) {
    unsafe {
        let class = to_wide("BUTTON");
        let wide = to_wide(text);
        // BS_OWNERDRAW → parent paints in WM_DRAWITEM.  The auto-toggle
        // behaviour that BS_AUTOCHECKBOX provides is gone, so we stash
        // the check state in GWLP_USERDATA and flip it ourselves on
        // BN_CLICKED (see the WM_COMMAND handler).
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW,
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        SetWindowLongPtrW(ctrl, GWLP_USERDATA, if checked { 1 } else { 0 });
    }
}

fn is_checkbox_id(id: i32) -> bool {
    matches!(id, IDC_CHK_PUNTO | IDC_CHK_TASKBAR | IDC_CHK_AUTOSTART | IDC_CHK_EXPLORER_CMD)
}

unsafe fn create_lang_combo(
    parent: HWND, hinst: HINSTANCE, _font: HFONT,
    x: i32, y: i32, w: i32, current_code: &str,
) {
    unsafe {
        register_lang_class(hinst);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("ScrTransLangCombo"), PCWSTR(std::ptr::null()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            x, y, w, layout::INPUT_H,
            parent, HMENU(IDC_COMBO_LANG as *mut _), hinst, None,
        ).unwrap_or_default();
        let current = Language::from_code(current_code);
        let selected = Language::all().iter().position(|l| *l == current).unwrap_or(0);
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
        HotkeyConfig { vk: packed & 0xFFFF, modifiers: packed >> 16 }
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
        if len == 0 { return String::new(); }
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
            CoCreateInstance, CoInitializeEx, CoUninitialize,
            CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{
            FileOpenDialog, IFileOpenDialog, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
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
        if ptr.is_null() { return; }
        let path = *Box::from_raw(ptr);
        let ctrl = GetDlgItem(hwnd, IDC_EDIT_FOLDER).unwrap_or_default();
        if ctrl.0.is_null() { return; }
        let wide = to_wide(&path);
        let _ = SetWindowTextW(ctrl, PCWSTR(wide.as_ptr()));
    }
}

// ============================================================
// Owner-drawn button painting (hover-aware)
// ============================================================

#[repr(C)]
struct DrawItemStruct {
    ctl_type: u32, ctl_id: u32, item_id: u32, item_action: u32, item_state: u32,
    hwnd_item: HWND, hdc: HDC, rc_item: RECT, item_data: usize,
}

/// Button drawing states.  Same enum for primary/secondary variants.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnState { Normal, Hover, Pressed }

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
        let is_hover   = dis.item_state & 0x0040 != 0;
        let state = if is_pressed { BtnState::Pressed }
                    else if is_hover { BtnState::Hover }
                    else { BtnState::Normal };

        let is_primary = id == IDC_BTN_SAVE;

        let (bg, fg, border) = button_colors(is_primary, state);

        // Fill rounded background.
        let bg_brush = CreateSolidBrush(COLORREF(bg));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, bg_brush);
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, 10, 10);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(bg_brush);
        let _ = DeleteObject(pen);

        // Text.
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(hdc, r.font_button());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(fg));

        let mut buf = vec![0u16; 64];
        let len = GetWindowTextW(dis.hwnd_item, &mut buf) as usize;
        let mut text: Vec<u16> = buf[..len].to_vec();
        let mut trc = rc;
        // DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
        DrawTextW(hdc, &mut text, &mut trc, DRAW_TEXT_FORMAT(0x0825));
        SelectObject(hdc, old_font);
    }
}

/// Paint an owner-drawn checkbox: rounded box on the left, text on the
/// right.  Checked state is read from the control's GWLP_USERDATA (we
/// maintain it ourselves because BS_OWNERDRAW disables BS_AUTOCHECKBOX).
unsafe fn draw_owner_checkbox(dis: &DrawItemStruct) {
    unsafe {
        let hdc = dis.hdc;
        let rc = dis.rc_item;
        let is_hover = dis.item_state & 0x0040 != 0;

        // Parent bg fills the rect first so rounded corners don't leak.
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let bg_brush = r.bg_brush();
        let _ = FillRect(hdc, &rc, bg_brush);

        // Box geometry.
        let box_size = 20i32;
        let box_y = rc.top + ((rc.bottom - rc.top) - box_size) / 2;
        let box_rc = RECT {
            left: rc.left, top: box_y,
            right: rc.left + box_size, bottom: box_y + box_size,
        };

        let checked = GetWindowLongPtrW(dis.hwnd_item, GWLP_USERDATA) != 0;

        // Checked → filled with accent.  Unchecked → field-fill with grey border.
        let (fill_color, border_color, border_w) = if checked {
            (theme::CLR_ACCENT, theme::CLR_ACCENT, 1)
        } else if is_hover {
            (CLR_FIELD_BG, theme::CLR_ACCENT, 1)
        } else {
            (CLR_FIELD_BG, CLR_FIELD_BORDER, 1)
        };

        let fill = CreateSolidBrush(COLORREF(fill_color));
        let pen  = CreatePen(PS_SOLID, border_w, COLORREF(border_color));
        let op = SelectObject(hdc, pen);
        let ob = SelectObject(hdc, fill);
        let _ = RoundRect(hdc, box_rc.left, box_rc.top, box_rc.right, box_rc.bottom, 5, 5);
        SelectObject(hdc, op);
        SelectObject(hdc, ob);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);

        // Checkmark when checked.
        if checked {
            let check_pen = CreatePen(PS_SOLID, 2, COLORREF(0x00FF_FFFF));
            let op2 = SelectObject(hdc, check_pen);
            let cx = box_rc.left + box_size / 2;
            let cy = box_rc.top  + box_size / 2;
            let _ = MoveToEx(hdc, cx - 5, cy,     None);
            let _ = LineTo  (hdc, cx - 1, cy + 4);
            let _ = MoveToEx(hdc, cx - 1, cy + 4, None);
            let _ = LineTo  (hdc, cx + 6, cy - 4);
            SelectObject(hdc, op2);
            let _ = DeleteObject(check_pen);
        }

        // Label text.
        let old_font = SelectObject(hdc, r.font_body());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(theme::CLR_TEXT_BRIGHT));

        let mut buf = vec![0u16; 128];
        let len = GetWindowTextW(dis.hwnd_item, &mut buf) as usize;
        let mut text: Vec<u16> = buf[..len].to_vec();
        let mut trc = RECT {
            left: box_rc.right + 12, top: rc.top,
            right: rc.right, bottom: rc.bottom,
        };
        // DT_VCENTER | DT_SINGLELINE | DT_LEFT | DT_NOPREFIX
        DrawTextW(hdc, &mut text, &mut trc, DRAW_TEXT_FORMAT(0x0824));
        SelectObject(hdc, old_font);
        drop(r_guard);
    }
}

fn button_colors(primary: bool, state: BtnState) -> (u32, u32, u32) {
    if primary {
        let bg = match state {
            BtnState::Normal  => theme::CLR_ACCENT,
            BtnState::Hover   => lighten(theme::CLR_ACCENT, 15),
            BtnState::Pressed => darken(theme::CLR_ACCENT, 25),
        };
        (bg, 0x00FF_FFFF, theme::CLR_ACCENT)
    } else {
        let bg = match state {
            BtnState::Normal  => CLR_BTN_BG,
            BtnState::Hover   => CLR_BTN_BG_HOVER,
            BtnState::Pressed => CLR_BTN_BG_PRESS,
        };
        (bg, theme::CLR_TEXT_BRIGHT, CLR_BTN_BORDER)
    }
}

fn darken(c: u32, amount: u32) -> u32 {
    let r = (c & 0xFF).saturating_sub(amount);
    let g = ((c >> 8) & 0xFF).saturating_sub(amount);
    let b = ((c >> 16) & 0xFF).saturating_sub(amount);
    r | (g << 8) | (b << 16)
}

fn lighten(c: u32, amount: u32) -> u32 {
    let r = ((c & 0xFF) + amount).min(0xFF);
    let g = (((c >> 8) & 0xFF) + amount).min(0xFF);
    let b = (((c >> 16) & 0xFF) + amount).min(0xFF);
    r | (g << 8) | (b << 16)
}

// ============================================================
// WM_PAINT — title bar, separators, input-field borders
// ============================================================

unsafe fn draw_text_into(hdc: HDC, text: &str, font: HFONT, color: u32, rc: &RECT, format: u32) {
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(color));
        let mut wide = to_wide(text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut area = *rc;
        DrawTextW(hdc, &mut wide, &mut area, DRAW_TEXT_FORMAT(format));
        SelectObject(hdc, old_font);
    }
}

unsafe fn draw_round_panel(hdc: HDC, rc: &RECT, fill: u32, border: u32, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(fill));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, brush);
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);
    }
}

unsafe fn paint_hero(hdc: HDC) {
    unsafe {
        use layout::*;

        let hero = RECT {
            left: MARGIN,
            top: Y_HERO,
            right: WIN_W - MARGIN,
            bottom: Y_HERO + HERO_H,
        };
        draw_round_panel(hdc, &hero, CLR_HERO_BG, CLR_HERO_BORDER, 18);

        let badge = RECT {
            left: hero.left + 18,
            top: hero.top + 18,
            right: hero.left + 66,
            bottom: hero.top + 66,
        };
        draw_round_panel(hdc, &badge, theme::CLR_ACCENT, lighten(theme::CLR_ACCENT, 8), 16);

        let r_guard = res();
        let r = r_guard.as_ref().unwrap();

        draw_text_into(
            hdc,
            "ST",
            r.font_badge(),
            0x00FF_FFFF,
            &badge,
            0x0825,
        );

        let title_rc = RECT {
            left: badge.right + 16,
            top: hero.top + 14,
            right: hero.right - 108,
            bottom: hero.top + 46,
        };
        draw_text_into(
            hdc,
            i18n::t("settings.title"),
            r.font_title(),
            theme::CLR_TEXT_BRIGHT,
            &title_rc,
            0x0800,
        );

        let subtitle = format!(
            "{} / {} / {} / {}",
            i18n::t("settings.section.general"),
            i18n::t("settings.section.hotkeys"),
            i18n::t("settings.section.folder"),
            i18n::t("settings.section.functions"),
        );
        let subtitle_rc = RECT {
            left: badge.right + 16,
            top: hero.top + 50,
            right: hero.right - 108,
            bottom: hero.bottom - 14,
        };
        draw_text_into(
            hdc,
            &subtitle,
            r.font_meta(),
            CLR_HERO_SUBTITLE,
            &subtitle_rc,
            0x0810,
        );
        drop(r_guard);

        let decor_left = hero.right - 84;
        for (offset, width, color) in [
            (22, 46, CLR_HERO_DECOR),
            (38, 62, darken(CLR_HERO_DECOR, 6)),
            (54, 36, lighten(CLR_HERO_DECOR, 8)),
        ] {
            let decor = RECT {
                left: decor_left,
                top: hero.top + offset,
                right: decor_left + width,
                bottom: hero.top + offset + 8,
            };
            draw_round_panel(hdc, &decor, color, color, 8);
        }
    }
}

/// Parent-painted input-field rectangles.  The hotkey fields and the
/// language combo now paint themselves (custom classes), so only the
/// native EDIT for the folder path is listed here.
fn input_rects() -> Vec<(i32, RECT)> {
    use layout::*;
    vec![
        (IDC_EDIT_FOLDER, RECT {
            left: MARGIN, top: Y_FOLDER,
            right: MARGIN + FOLDER_W, bottom: Y_FOLDER + INPUT_H,
        }),
        (IDC_EDIT_DEEPSEEK, RECT {
            left: INPUT_X, top: Y_DEEPSEEK,
            right: INPUT_X + INPUT_W, bottom: Y_DEEPSEEK + INPUT_H,
        }),
    ]
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        use layout::*;

        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        paint_hero(hdc);

        // ── Section separators ──
        let sep_pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_SEPARATOR));
        let old_sep = SelectObject(hdc, sep_pen);
        for y in [Y_SEP1, Y_SEP2, Y_SEP3, Y_SEP4] {
            let _ = MoveToEx(hdc, MARGIN, y, None);
            let _ = LineTo(hdc, WIN_W - MARGIN, y);
        }
        SelectObject(hdc, old_sep);
        let _ = DeleteObject(sep_pen);

        let accent_pen = CreatePen(PS_SOLID, 2, COLORREF(theme::CLR_ACCENT));
        let old_accent = SelectObject(hdc, accent_pen);
        for y in [Y_SEP1, Y_SEP2, Y_SEP3, Y_SEP4] {
            let _ = MoveToEx(hdc, MARGIN, y, None);
            let _ = LineTo(hdc, MARGIN + 54, y);
        }
        SelectObject(hdc, old_accent);
        let _ = DeleteObject(accent_pen);

        // ── Input-field borders (always on; accent when focused) ──
        let focused = GetFocus();
        let focused_id = if focused.0.is_null() { 0 } else { GetDlgCtrlID(focused) };

        for (id, rc) in input_rects() {
            let is_focused = id == focused_id;
            let border = if is_focused { CLR_FIELD_BORDER_FOCUS } else { CLR_FIELD_BORDER };
            draw_field_border(hdc, &rc, border, is_focused);
        }

        let _ = EndPaint(hwnd, &ps);
    }
}

/// Paints one input-field rounded-rectangle border with matching fill.
unsafe fn draw_field_border(hdc: HDC, rc: &RECT, border_color: u32, focused: bool) {
    unsafe {
        let thickness = if focused { 2 } else { 1 };
        let pen = CreatePen(PS_SOLID, thickness, COLORREF(border_color));
        let brush = CreateSolidBrush(COLORREF(CLR_FIELD_BG));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, brush);
        let _ = RoundRect(hdc, rc.left - 2, rc.top - 2,
            rc.right + 2, rc.bottom + 2, 8, 8);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
        let _ = DeleteObject(brush);
    }
}

// ============================================================
// Helpers
// ============================================================

/// Invalidate just the 2-px margin around every input field, so the paint
/// code can redraw focus borders without touching the rest of the window.
unsafe fn invalidate_input_borders(hwnd: HWND) {
    unsafe {
        for (_, rc) in input_rects() {
            let outer = RECT {
                left: rc.left - 3, top: rc.top - 3,
                right: rc.right + 3, bottom: rc.bottom + 3,
            };
            let _ = InvalidateRect(hwnd, Some(&outer), false);
        }
    }
}

// ============================================================
// Command dispatch & save
// ============================================================

unsafe fn do_save(hwnd: HWND) {
    unsafe {
        let new_settings = Settings {
            hk_translate:    read_hotkey(hwnd, IDC_HK_TRANSLATE),
            hk_ocr:          read_hotkey(hwnd, IDC_HK_OCR),
            hk_screenshot:   read_hotkey(hwnd, IDC_HK_SCREENSHOT),
            hk_layout:       read_hotkey(hwnd, IDC_HK_LAYOUT),
            hk_explorer_cmd: read_hotkey(hwnd, IDC_HK_EXPLORER_CMD),
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

unsafe extern "system" fn settings_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_DRAWITEM => {
                let dis = &*(lp.0 as *const DrawItemStruct);
                if is_checkbox_id(dis.ctl_id as i32) {
                    draw_owner_checkbox(dis);
                } else {
                    draw_owner_button(lp);
                }
                LRESULT(1)
            }

            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkMode(hdc, TRANSPARENT);
                let ctrl = HWND(lp.0 as *mut _);
                let ctrl_id = GetDlgCtrlID(ctrl);
                let text_clr = if (IDC_SEC_GENERAL..=IDC_SEC_TRANS).contains(&ctrl_id) {
                    theme::CLR_ACCENT
                } else {
                    theme::CLR_TEXT
                };
                SetTextColor(hdc, COLORREF(text_clr));
                LRESULT(res().as_ref().unwrap().bg_brush().0 as isize)
            }

            WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkColor(hdc, COLORREF(CLR_FIELD_BG));
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT));
                LRESULT(res().as_ref().unwrap().field_brush().0 as isize)
            }

            WM_PAINT => { paint(hwnd); LRESULT(0) }

            WM_COMMAND => {
                let code = ((wp.0 >> 16) & 0xFFFF) as u16;
                let id   = (wp.0 & 0xFFFF) as i32;
                // EN_SETFOCUS = 0x0100, EN_KILLFOCUS = 0x0200 (native EDIT).
                if code == 0x0100 || code == 0x0200 {
                    invalidate_input_borders(hwnd);
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
                    IDC_BTN_SAVE   => do_save(hwnd),
                    IDC_BTN_CANCEL => { let _ = DestroyWindow(hwnd); }
                    IDC_BTN_BROWSE => browse_folder(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }

            WM_CLOSE   => { let _ = DestroyWindow(hwnd); LRESULT(0) }
            WM_DESTROY => { *SETTINGS_HWND.lock().unwrap() = 0; LRESULT(0) }

            m if m == WM_APP_BROWSE_RESULT => {
                apply_browse_result(hwnd, lp);
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

struct HotkeyState { mods: u32, vk: u32, focused: bool }

static HOTKEY_CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);

unsafe fn register_hotkey_class(hinst: HINSTANCE) {
    unsafe {
        let mut g = HOTKEY_CLASS_REGISTERED.lock().unwrap();
        if *g { return; }
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

unsafe extern "system" fn hotkey_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_GETDLGCODE => LRESULT(0x000E),   // WANTARROWS | WANTTAB | WANTALLKEYS
            WM_ERASEBKGND => LRESULT(1),
            WM_LBUTTONDOWN => { let _ = SetFocus(hwnd); LRESULT(0) }
            WM_SETFOCUS => {
                if let Some(s) = hotkey_state(hwnd) { s.focused = true; }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if let Some(s) = hotkey_state(hwnd) { s.focused = false; }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wp.0 as u32;
                // Ignore standalone modifier keys — wait for the "real" key.
                if matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5) {
                    return LRESULT(0);
                }
                // Tab and Escape are reserved for UI navigation / dismiss —
                // capturing them as hotkeys would break basic keyboard use.
                if vk == 0x09 || vk == 0x1B {
                    return LRESULT(0);
                }
                // Backspace or Delete clears the hotkey.
                if vk == 0x08 || vk == 0x2E {
                    if let Some(s) = hotkey_state(hwnd) { s.mods = 0; s.vk = 0; }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let ctrl_down  = (GetKeyState(0x11) as u16 & 0x8000) != 0;
                let shift_down = (GetKeyState(0x10) as u16 & 0x8000) != 0;
                let alt_down   = (GetKeyState(0x12) as u16 & 0x8000) != 0;
                let mut m = 0u32;
                if ctrl_down  { m |= 0x0002; }
                if alt_down   { m |= 0x0001; }
                if shift_down { m |= 0x0004; }
                if let Some(s) = hotkey_state(hwnd) { s.mods = m; s.vk = vk; }
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
            m if m == HK_MSG_GET => {
                hotkey_state(hwnd)
                    .map(|s| LRESULT((s.vk | (s.mods << 16)) as isize))
                    .unwrap_or(LRESULT(0))
            }
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

        // Parent background behind the rounded rect — hides the rectangular
        // corners that'd otherwise peek through.
        let bg = CreateSolidBrush(COLORREF(theme::CLR_BG));
        FillRect(mem_dc, &rc, bg);
        let _ = DeleteObject(bg);

        let border = if focused { CLR_FIELD_BORDER_FOCUS } else { CLR_FIELD_BORDER };
        let pen = CreatePen(PS_SOLID, if focused { 2 } else { 1 }, COLORREF(border));
        let fill = CreateSolidBrush(COLORREF(CLR_FIELD_BG));
        let op = SelectObject(mem_dc, pen);
        let ob = SelectObject(mem_dc, fill);
        let _ = RoundRect(mem_dc, 0, 0, rc.right, rc.bottom, 8, 8);
        SelectObject(mem_dc, op);
        SelectObject(mem_dc, ob);
        let _ = DeleteObject(pen);
        let _ = DeleteObject(fill);

        let text = hotkey_display(mods, vk, focused);
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);
        let text_color = if vk == 0 { theme::CLR_TEXT_DIM } else { theme::CLR_TEXT_BRIGHT };
        SetTextColor(mem_dc, COLORREF(text_color));
        let mut wide = to_wide(&text);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut trc = RECT { left: 12, top: 0, right: rc.right - 12, bottom: rc.bottom };
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
        if focused { "…".to_string() } else { "—".to_string() }
    } else {
        HotkeyConfig { modifiers: mods, vk }.display()
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

static LANG_CLASS_REGISTERED:       Mutex<bool> = Mutex::new(false);
static LANG_POPUP_CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);

unsafe fn register_lang_class(hinst: HINSTANCE) {
    unsafe {
        let mut g = LANG_CLASS_REGISTERED.lock().unwrap();
        if *g { return; }
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
        if *g { return; }
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

unsafe extern "system" fn lang_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
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
                if let Some(s) = lang_state(hwnd) { s.focused = true; }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if let Some(s) = lang_state(hwnd) { s.focused = false; }
                let _ = InvalidateRect(hwnd, None, false);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u32;
                let popup_open = lang_state(hwnd).map(|s| s.popup != 0).unwrap_or(false);
                match vk {
                    0x1B if popup_open => { let _ = ReleaseCapture(); } // Esc closes popup
                    0x0D | 0x20 | 0x28 => { open_lang_popup(hwnd); }    // Enter / Space / Down
                    0x26 => {                                            // Up — cycle selection
                        if let Some(s) = lang_state(hwnd) {
                            if s.selected > 0 { s.selected -= 1; }
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
                    .map(|s| (s.selected, s.focused)).unwrap_or((0, false));
                paint_lang(hwnd, hdc, selected, focused);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            m if m == LANG_MSG_GET => {
                lang_state(hwnd).map(|s| LRESULT(s.selected as isize))
                    .unwrap_or(LRESULT(0))
            }
            m if m == LANG_MSG_SET => {
                if let Some(s) = lang_state(hwnd) { s.selected = wp.0 as usize; }
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

        let bg = CreateSolidBrush(COLORREF(theme::CLR_BG));
        FillRect(mem_dc, &rc, bg);
        let _ = DeleteObject(bg);

        let border = if focused { CLR_FIELD_BORDER_FOCUS } else { CLR_FIELD_BORDER };
        let pen = CreatePen(PS_SOLID, if focused { 2 } else { 1 }, COLORREF(border));
        let fill = CreateSolidBrush(COLORREF(CLR_FIELD_BG));
        let op = SelectObject(mem_dc, pen);
        let ob = SelectObject(mem_dc, fill);
        let _ = RoundRect(mem_dc, 0, 0, rc.right, rc.bottom, 8, 8);
        SelectObject(mem_dc, op);
        SelectObject(mem_dc, ob);
        let _ = DeleteObject(pen);
        let _ = DeleteObject(fill);

        let langs = Language::all();
        let name = langs.get(selected).map(|l| l.native_name()).unwrap_or("");
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(theme::CLR_TEXT_BRIGHT));
        let mut wide = to_wide(name);
        if wide.last() == Some(&0) { wide.pop(); }
        let mut trc = RECT { left: 14, top: 0, right: rc.right - 32, bottom: rc.bottom };
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
            if s.popup != 0 { return; }
        }
        let Ok(hmodule) = GetModuleHandleW(None) else { return };
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
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("ScrTransLangPopup"), w!(""),
            WS_POPUP,
            rc.left, rc.bottom + 4, popup_w, popup_h,
            owner, HMENU::default(), hinst, None,
        ).unwrap_or_default();
        if popup.0.is_null() { return; }

        let state = Box::into_raw(Box::new(LangPopupState {
            owner: owner.0 as isize,
            hover: lang_state(owner).map(|s| s.selected).unwrap_or(0),
            item_h, pad,
        }));
        SetWindowLongPtrW(popup, GWLP_USERDATA, state as isize);

        if let Some(s) = lang_state(owner) { s.popup = popup.0 as isize; }

        let _ = ShowWindow(popup, SW_SHOWNA);
        // Capture so clicks outside the popup close it.  WM_CAPTURECHANGED
        // is the canonical "close yourself" signal.
        SetCapture(popup);
    }
}

unsafe extern "system" fn lang_popup_proc(
    hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM,
) -> LRESULT {
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
                        } else { usize::MAX }
                    } else { usize::MAX };
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
                    if let Some(os) = lang_state(owner) { os.popup = 0; }
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

        // Dark rounded panel with 1-px accent-adjacent border.
        let fill = CreateSolidBrush(COLORREF(CLR_FIELD_BG));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(CLR_FIELD_BORDER));
        let op = SelectObject(mem_dc, pen);
        let ob = SelectObject(mem_dc, fill);
        let _ = RoundRect(mem_dc, 0, 0, rc.right, rc.bottom, 10, 10);
        SelectObject(mem_dc, op);
        SelectObject(mem_dc, ob);
        let _ = DeleteObject(pen);
        let _ = DeleteObject(fill);

        let Some(s) = lang_popup_state(hwnd) else {
            let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem_dc, 0, 0, SRCCOPY);
            SelectObject(mem_dc, old);
            let _ = DeleteObject(mem_bmp);
            let _ = DeleteDC(mem_dc);
            return;
        };
        let owner = HWND(s.owner as *mut _);
        let selected = lang_state(owner).map(|os| os.selected).unwrap_or(usize::MAX);

        let langs = Language::all();
        let r_guard = res();
        let r = r_guard.as_ref().unwrap();
        let old_font = SelectObject(mem_dc, r.font_body());
        SetBkMode(mem_dc, TRANSPARENT);

        for (i, lang) in langs.iter().enumerate() {
            let y = s.pad + (i as i32) * s.item_h;
            let item_rc = RECT {
                left: s.pad, top: y,
                right: rc.right - s.pad, bottom: y + s.item_h,
            };
            let is_hover    = i == s.hover;
            let is_selected = i == selected;

            if is_hover {
                let hover_brush = CreateSolidBrush(COLORREF(CLR_BTN_BG_HOVER));
                FillRect(mem_dc, &item_rc, hover_brush);
                let _ = DeleteObject(hover_brush);
            } else if is_selected {
                let sel_brush = CreateSolidBrush(COLORREF(CLR_BTN_BG));
                FillRect(mem_dc, &item_rc, sel_brush);
                let _ = DeleteObject(sel_brush);
            }
            if is_selected {
                let accent = CreateSolidBrush(COLORREF(theme::CLR_ACCENT));
                let bar = RECT {
                    left: item_rc.left + 4, top: item_rc.top + 8,
                    right: item_rc.left + 7, bottom: item_rc.bottom - 8,
                };
                FillRect(mem_dc, &bar, accent);
                let _ = DeleteObject(accent);
            }

            SetTextColor(mem_dc, COLORREF(theme::CLR_TEXT_BRIGHT));
            let mut wide = to_wide(lang.native_name());
            if wide.last() == Some(&0) { wide.pop(); }
            let mut text_rc = RECT { left: item_rc.left + 16, ..item_rc };
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
