use crate::autostart;
use crate::i18n::{self, Language};
use crate::settings::{self, HotkeyConfig, Settings};
use crate::theme;
use crate::utils::to_wide;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_HOTKEY_CLASS};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ============================================================
// Control IDs
// ============================================================

const IDC_HK_TRANSLATE: i32 = 101;
const IDC_HK_OCR: i32 = 102;
const IDC_HK_SCREENSHOT: i32 = 103;
const IDC_HK_LAYOUT: i32 = 104;
const IDC_EDIT_FOLDER: i32 = 106;
const IDC_BTN_BROWSE: i32 = 107;
const IDC_BTN_SAVE: i32 = 108;
const IDC_BTN_CANCEL: i32 = 109;
const IDC_CHK_PUNTO: i32 = 110;
const IDC_CHK_TASKBAR: i32 = 111;
const IDC_CHK_AUTOSTART: i32 = 112;
const IDC_COMBO_LANG: i32 = 114;

// Section header IDs (for accent coloring in WM_CTLCOLORSTATIC)
const IDC_SEC_GENERAL: i32 = 200;
const IDC_SEC_HOTKEYS: i32 = 201;
const IDC_SEC_FOLDER: i32 = 202;
const IDC_SEC_FUNC: i32 = 203;

const HKM_SETHOTKEY: u32 = 0x0401;
const HKM_GETHOTKEY: u32 = 0x0402;
const EM_SETMARGINS: u32 = 0x00D3;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;

// ============================================================
// Layout
// ============================================================

const WIN_W: i32 = 460;
const WIN_H: i32 = 600;

const MARGIN: i32 = 24;
const LABEL_W: i32 = 186;
const INPUT_X: i32 = 218;
const INPUT_W: i32 = 206;
const INPUT_H: i32 = 26;

const Y_TITLE: i32 = 14;
const Y_ACCENT: i32 = 40;

// General (language picker)
const Y_SEC_GEN: i32 = 52;
const Y_LANG: i32 = 76;

const Y_SEC_HK: i32 = 130;
const Y_HK_START: i32 = 154;
const HK_STEP: i32 = 32;

const Y_SEP1: i32 = 282;
const Y_SEC_FOLDER: i32 = 292;
const Y_FOLDER: i32 = 316;
const FOLDER_W: i32 = 330;

const Y_SEP2: i32 = 354;
const Y_SEC_FUNC: i32 = 364;
const Y_CHK_START: i32 = 388;
const CHK_STEP: i32 = 28;

const Y_BUTTONS: i32 = 496;
const BTN_W: i32 = 140;
const BTN_H: i32 = 38;
const BTN_GAP: i32 = 16;

// Colors
const CLR_BTN_BG: u32 = 0x0040_4040;
const CLR_FIELD_BG: u32 = 0x0038_3838;
const CLR_FIELD_BORDER: u32 = 0x0055_5555;

// ============================================================
// State
// ============================================================

static SETTINGS_HWND: Mutex<isize> = Mutex::new(0);
static UPDATED_SETTINGS: Mutex<Option<Box<Settings>>> = Mutex::new(None);
static BG_BRUSH_RAW: Mutex<isize> = Mutex::new(0);
static FIELD_BRUSH_RAW: Mutex<isize> = Mutex::new(0);

fn bg_brush() -> HBRUSH {
    let mut guard = BG_BRUSH_RAW.lock().unwrap();
    if *guard == 0 {
        let b = unsafe { CreateSolidBrush(COLORREF(theme::CLR_BG)) };
        *guard = b.0 as isize;
    }
    HBRUSH(*guard as *mut _)
}

fn field_brush() -> HBRUSH {
    let mut guard = FIELD_BRUSH_RAW.lock().unwrap();
    if *guard == 0 {
        let b = unsafe { CreateSolidBrush(COLORREF(CLR_FIELD_BG)) };
        *guard = b.0 as isize;
    }
    HBRUSH(*guard as *mut _)
}

// ============================================================
// Public API
// ============================================================

pub fn open(current: &Settings) {
    unsafe {
        let v = *SETTINGS_HWND.lock().unwrap();
        if v != 0 {
            let hwnd = HWND(v as *mut _);
            if IsWindow(hwnd).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }

        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_HOTKEY_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let Some(hmodule) = GetModuleHandleW(None).ok() else { return };
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("ScrTransSettings4");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(settings_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: bg_brush(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);

        let title = to_wide(i18n::t("settings.title"));
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class, PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            (sw - WIN_W) / 2, (sh - WIN_H) / 2, WIN_W, WIN_H,
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
        let font = CreateFontW(-14,0,0,0,400,0,0,0,1,0,0,5,0,w!("Segoe UI"));
        let font_title = CreateFontW(-20,0,0,0,700,0,0,0,1,0,0,5,0,w!("Segoe UI"));
        let font_section = CreateFontW(-12,0,0,0,700,0,0,0,1,0,0,5,0,w!("Segoe UI"));

        // Title
        create_label(parent, hinst, font_title, MARGIN, Y_TITLE, 300, 24, "Screen Translator", 0);

        // ── GENERAL / Language ──
        create_label(parent, hinst, font_section, MARGIN, Y_SEC_GEN, 200, 16,
            i18n::t("settings.section.general"), IDC_SEC_GENERAL);
        create_label(parent, hinst, font, MARGIN, Y_LANG + 3, LABEL_W, 20,
            i18n::t("settings.label.language"), 0);
        create_lang_combo(parent, hinst, font, INPUT_X, Y_LANG, INPUT_W, &s.language);

        // ── HOTKEYS ──
        create_label(parent, hinst, font_section, MARGIN, Y_SEC_HK, 200, 16,
            i18n::t("settings.section.hotkeys"), IDC_SEC_HOTKEYS);

        let hk_class = to_wide("msctls_hotkey32");
        let hotkeys: &[(&str, i32, &HotkeyConfig)] = &[
            (i18n::t("settings.hotkey.translate"),  IDC_HK_TRANSLATE,  &s.hk_translate),
            (i18n::t("settings.hotkey.ocr"),        IDC_HK_OCR,        &s.hk_ocr),
            (i18n::t("settings.hotkey.screenshot"), IDC_HK_SCREENSHOT, &s.hk_screenshot),
            (i18n::t("settings.hotkey.layout"),     IDC_HK_LAYOUT,     &s.hk_layout),
        ];

        for (i, &(label, id, hk)) in hotkeys.iter().enumerate() {
            let y = Y_HK_START + (i as i32) * HK_STEP;
            create_label(parent, hinst, font, MARGIN, y + 3, LABEL_W, 20, label, 0);

            // No WS_BORDER — custom border drawn in WM_PAINT
            let ctrl = CreateWindowExW(
                WINDOW_EX_STYLE(0), PCWSTR(hk_class.as_ptr()), PCWSTR(std::ptr::null()),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                INPUT_X, y, INPUT_W, INPUT_H,
                parent, HMENU(id as *mut _), hinst, None,
            ).unwrap_or_default();

            let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            let _ = SendMessageW(ctrl, HKM_SETHOTKEY, WPARAM(hk.to_hotkey_ctrl_param()), LPARAM(0));
        }

        // ── SCREENSHOT FOLDER ──
        create_label(parent, hinst, font_section, MARGIN, Y_SEC_FOLDER, 200, 16,
            i18n::t("settings.section.folder"), IDC_SEC_FOLDER);

        let edit_class = to_wide("EDIT");
        let folder_wide = to_wide(&s.screenshot_folder);
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(edit_class.as_ptr()), PCWSTR(folder_wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0080), // ES_AUTOHSCROLL
            MARGIN, Y_FOLDER, FOLDER_W, INPUT_H,
            parent, HMENU(IDC_EDIT_FOLDER as *mut _), hinst, None,
        ).unwrap_or_default();
        let _ = SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        // Internal padding so text doesn't touch edges
        let _ = SendMessageW(edit, EM_SETMARGINS, WPARAM(3), LPARAM(6 | (6 << 16)));

        create_od_button(parent, hinst, MARGIN + FOLDER_W + 10, Y_FOLDER, 60, INPUT_H, "...", IDC_BTN_BROWSE);

        // ── FEATURES ──
        create_label(parent, hinst, font_section, MARGIN, Y_SEC_FUNC, 200, 16,
            i18n::t("settings.section.functions"), IDC_SEC_FUNC);

        create_checkbox(parent, hinst, font, MARGIN, Y_CHK_START, 400, 22,
            i18n::t("settings.checkbox.punto"), IDC_CHK_PUNTO, s.punto_enabled);
        create_checkbox(parent, hinst, font, MARGIN, Y_CHK_START + CHK_STEP, 400, 22,
            i18n::t("settings.checkbox.taskbar"), IDC_CHK_TASKBAR, s.taskbar_center_enabled);
        create_checkbox(parent, hinst, font, MARGIN, Y_CHK_START + CHK_STEP * 2, 400, 22,
            i18n::t("settings.checkbox.autostart"), IDC_CHK_AUTOSTART, autostart::is_enabled());

        // ── Buttons ──
        let bx = (WIN_W - BTN_W * 2 - BTN_GAP) / 2;
        create_od_button(parent, hinst, bx, Y_BUTTONS, BTN_W, BTN_H,
            i18n::t("settings.btn.save"), IDC_BTN_SAVE);
        create_od_button(parent, hinst, bx + BTN_W + BTN_GAP, Y_BUTTONS, BTN_W, BTN_H,
            i18n::t("settings.btn.cancel"), IDC_BTN_CANCEL);
    }
}

unsafe fn create_lang_combo(
    parent: HWND, hinst: HINSTANCE, font: HFONT,
    x: i32, y: i32, w: i32, current_code: &str,
) {
    unsafe {
        let class = to_wide("COMBOBOX");
        // CBS_DROPDOWNLIST = 0x0003, WS_VSCROLL = 0x00200000
        let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP
            | WINDOW_STYLE(0x0003) | WINDOW_STYLE(0x00200000);
        // Height parameter includes the dropdown; give it 260 for ~10 items.
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(std::ptr::null()),
            style, x, y, w, 260,
            parent, HMENU(IDC_COMBO_LANG as *mut _), hinst, None,
        ).unwrap_or_default();

        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

        let current = Language::from_code(current_code);
        let mut selected_index: usize = 0;
        for (i, lang) in Language::all().iter().enumerate() {
            let name = to_wide(lang.native_name());
            let _ = SendMessageW(ctrl, CB_ADDSTRING, WPARAM(0),
                LPARAM(name.as_ptr() as isize));
            if *lang == current {
                selected_index = i;
            }
        }
        let _ = SendMessageW(ctrl, CB_SETCURSEL, WPARAM(selected_index), LPARAM(0));
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

unsafe fn create_od_button(
    parent: HWND, hinst: HINSTANCE,
    x: i32, y: i32, w: i32, h: i32, text: &str, id: i32,
) {
    unsafe {
        let class = to_wide("BUTTON");
        let wide = to_wide(text);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x000B), // BS_OWNERDRAW
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        let font = CreateFontW(-14,0,0,0,600,0,0,0,1,0,0,5,0,w!("Segoe UI"));
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

unsafe fn create_checkbox(
    parent: HWND, hinst: HINSTANCE, font: HFONT,
    x: i32, y: i32, w: i32, h: i32, text: &str, id: i32, checked: bool,
) {
    unsafe {
        let class = to_wide("BUTTON");
        let wide = to_wide(text);
        let ctrl = CreateWindowExW(
            WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(wide.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(0x0003), // BS_AUTOCHECKBOX
            x, y, w, h,
            parent, HMENU(id as *mut _), hinst, None,
        ).unwrap_or_default();
        let _ = SendMessageW(ctrl, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        if checked {
            let _ = SendMessageW(ctrl, BM_SETCHECK, WPARAM(1), LPARAM(0));
        }
    }
}

// ============================================================
// Read back control values
// ============================================================

unsafe fn read_hotkey(parent: HWND, id: i32) -> HotkeyConfig {
    unsafe {
        let ctrl = GetDlgItem(parent, id).unwrap_or_default();
        let r = SendMessageW(ctrl, HKM_GETHOTKEY, WPARAM(0), LPARAM(0));
        let vk = (r.0 & 0xFF) as u32;
        let mods = ((r.0 >> 8) & 0xFF) as u32;
        HotkeyConfig::from_hotkey_ctrl(vk, mods)
    }
}

unsafe fn read_folder(parent: HWND) -> String {
    unsafe {
        let ctrl = GetDlgItem(parent, IDC_EDIT_FOLDER).unwrap_or_default();
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
        SendMessageW(ctrl, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0
    }
}

unsafe fn read_selected_language(parent: HWND) -> String {
    unsafe {
        let ctrl = GetDlgItem(parent, IDC_COMBO_LANG).unwrap_or_default();
        let idx = SendMessageW(ctrl, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        if idx < 0 { return "en".to_string(); }
        Language::all()
            .get(idx as usize)
            .map(|l| l.code().to_string())
            .unwrap_or_else(|| "en".to_string())
    }
}

unsafe fn browse_folder(parent: HWND) {
    unsafe {
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
        use windows::Win32::UI::Shell::{IFileOpenDialog, FileOpenDialog, FOS_PICKFOLDERS};

        let Ok(dialog) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_ALL) else { return };
        if let Ok(opts) = dialog.GetOptions() {
            let _ = dialog.SetOptions(opts | FOS_PICKFOLDERS);
        }
        if dialog.Show(parent).is_err() { return; }
        let Ok(result) = dialog.GetResult() else { return };
        let Ok(path) = result.GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH) else { return };
        let path_str = path.to_string().unwrap_or_default();

        let ctrl = GetDlgItem(parent, IDC_EDIT_FOLDER).unwrap_or_default();
        let wide = to_wide(&path_str);
        let _ = SetWindowTextW(ctrl, PCWSTR(wide.as_ptr()));

        windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const _));
    }
}

// ============================================================
// Owner-drawn button painting
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

        let is_save = dis.ctl_id as i32 == IDC_BTN_SAVE;
        let is_pressed = dis.item_state & 0x0001 != 0; // ODS_SELECTED

        let (mut bg_clr, text_clr, border_clr) = if is_save {
            (theme::CLR_ACCENT, 0x00FF_FFFF_u32, theme::CLR_ACCENT)
        } else {
            (CLR_BTN_BG, theme::CLR_TEXT_BRIGHT, 0x0060_6060_u32)
        };

        // Darken on press
        if is_pressed {
            bg_clr = darken(bg_clr, 30);
        }

        let bg = CreateSolidBrush(COLORREF(bg_clr));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(border_clr));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, bg);
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, 8);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(bg);
        let _ = DeleteObject(pen);

        let font = CreateFontW(-14,0,0,0,600,0,0,0,1,0,0,5,0,w!("Segoe UI"));
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(text_clr));

        let mut buf = vec![0u16; 64];
        let len = GetWindowTextW(dis.hwnd_item, &mut buf) as usize;
        let mut text: Vec<u16> = buf[..len].to_vec();
        let mut trc = rc;
        // DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX
        DrawTextW(hdc, &mut text, &mut trc, DRAW_TEXT_FORMAT(0x0825));
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

/// Subtract `amount` from each RGB channel (COLORREF = 0x00BBGGRR).
fn darken(c: u32, amount: u32) -> u32 {
    let r = (c & 0xFF).saturating_sub(amount);
    let g = ((c >> 8) & 0xFF).saturating_sub(amount);
    let b = ((c >> 16) & 0xFF).saturating_sub(amount);
    r | (g << 8) | (b << 16)
}

// ============================================================
// WM_PAINT — custom borders & decorations
// ============================================================

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        // ── Accent line under header ──
        let accent_brush = CreateSolidBrush(COLORREF(theme::CLR_ACCENT));
        let line_rc = RECT { left: MARGIN, top: Y_ACCENT, right: WIN_W - MARGIN, bottom: Y_ACCENT + 2 };
        let _ = FillRect(hdc, &line_rc, accent_brush);
        let _ = DeleteObject(accent_brush);

        // ── Section separators ──
        let sep_pen = CreatePen(PS_SOLID, 1, COLORREF(theme::CLR_SEPARATOR));
        let old_pen = SelectObject(hdc, sep_pen);
        let _ = MoveToEx(hdc, MARGIN, Y_SEP1, None);
        let _ = LineTo(hdc, WIN_W - MARGIN, Y_SEP1);
        let _ = MoveToEx(hdc, MARGIN, Y_SEP2, None);
        let _ = LineTo(hdc, WIN_W - MARGIN, Y_SEP2);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(sep_pen);

        // ── Custom dark borders around input fields ──
        let border_pen = CreatePen(PS_SOLID, 1, COLORREF(CLR_FIELD_BORDER));
        let fill_brush = CreateSolidBrush(COLORREF(CLR_FIELD_BG));
        let old_pen = SelectObject(hdc, border_pen);
        let old_brush = SelectObject(hdc, fill_brush);

        // Hotkey field borders
        for i in 0..4 {
            let y = Y_HK_START + i * HK_STEP;
            let _ = RoundRect(hdc,
                INPUT_X - 2, y - 2,
                INPUT_X + INPUT_W + 2, y + INPUT_H + 2,
                6, 6);
        }

        // Folder edit border
        let _ = RoundRect(hdc,
            MARGIN - 2, Y_FOLDER - 2,
            MARGIN + FOLDER_W + 2, Y_FOLDER + INPUT_H + 2,
            6, 6);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(border_pen);
        let _ = DeleteObject(fill_brush);

        let _ = EndPaint(hwnd, &ps);
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
            WM_DRAWITEM => { draw_owner_button(lp); LRESULT(1) }

            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkMode(hdc, TRANSPARENT);

                // Section headers get accent color
                let ctrl = HWND(lp.0 as *mut _);
                let ctrl_id = GetDlgCtrlID(ctrl);
                let text_clr = if ctrl_id >= 200 && ctrl_id <= 203 {
                    theme::CLR_ACCENT
                } else {
                    theme::CLR_TEXT
                };
                SetTextColor(hdc, COLORREF(text_clr));
                LRESULT(bg_brush().0 as isize)
            }

            WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as *mut _);
                SetBkColor(hdc, COLORREF(CLR_FIELD_BG));
                SetTextColor(hdc, COLORREF(theme::CLR_TEXT));
                LRESULT(field_brush().0 as isize)
            }

            WM_PAINT => { paint(hwnd); LRESULT(0) }

            WM_COMMAND => {
                match (wp.0 & 0xFFFF) as i32 {
                    IDC_BTN_SAVE => {
                        let new_settings = Settings {
                            hk_translate: read_hotkey(hwnd, IDC_HK_TRANSLATE),
                            hk_ocr: read_hotkey(hwnd, IDC_HK_OCR),
                            hk_screenshot: read_hotkey(hwnd, IDC_HK_SCREENSHOT),
                            hk_layout: read_hotkey(hwnd, IDC_HK_LAYOUT),
                            screenshot_folder: read_folder(hwnd),
                            punto_enabled: read_checkbox(hwnd, IDC_CHK_PUNTO),
                            taskbar_center_enabled: read_checkbox(hwnd, IDC_CHK_TASKBAR),
                            language: read_selected_language(hwnd),
                        };

                        // Autostart → registry
                        autostart::set_enabled(read_checkbox(hwnd, IDC_CHK_AUTOSTART));

                        settings::save(&new_settings);
                        *UPDATED_SETTINGS.lock().unwrap() = Some(Box::new(new_settings));
                        let _ = DestroyWindow(hwnd);
                    }
                    IDC_BTN_CANCEL => { let _ = DestroyWindow(hwnd); }
                    IDC_BTN_BROWSE => { browse_folder(hwnd); }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => { let _ = DestroyWindow(hwnd); LRESULT(0) }
            WM_DESTROY => { *SETTINGS_HWND.lock().unwrap() = 0; LRESULT(0) }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
