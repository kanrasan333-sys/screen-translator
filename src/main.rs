#![windows_subsystem = "windows"]

mod autostart;
mod autotype;
mod capture;
mod layout;
mod ocr;
mod popup;
mod screenshot;
mod settings;
mod settings_ui;
mod taskbar_center;
mod theme;
mod translate;
mod tray;
mod utils;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use utils::make_key_input;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ============================================================
// Hotkey IDs
// ============================================================

const HOTKEY_TRANSLATE: i32 = 1;
const HOTKEY_OCR: i32 = 2;
const HOTKEY_SCREENSHOT: i32 = 3;
const HOTKEY_LAYOUT: i32 = 4;
const HOTKEY_SETTINGS: i32 = 5;
const HOTKEY_AUTOTYPE: i32 = 6;
const HOTKEY_TASKBAR_CENTER: i32 = 7;

const ALL_HOTKEY_IDS: [i32; 7] = [
    HOTKEY_TRANSLATE,
    HOTKEY_OCR,
    HOTKEY_SCREENSHOT,
    HOTKEY_LAYOUT,
    HOTKEY_SETTINGS,
    HOTKEY_AUTOTYPE,
    HOTKEY_TASKBAR_CENTER,
];

// ============================================================
// Global state
// ============================================================

static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static CURRENT_SETTINGS: Mutex<settings::Settings> = Mutex::new(settings::Settings::DEFAULT);

/// Result delivered from a background translation thread.
struct TranslationResult {
    original: String,
    translated: String,
    direction: String,
    is_error: bool,
}
static PENDING_RESULT: Mutex<Option<TranslationResult>> = Mutex::new(None);

fn get_settings() -> settings::Settings {
    CURRENT_SETTINGS.lock().unwrap().clone()
}

/// Wakes the main thread so `MsgWaitForMultipleObjectsEx` returns
/// and checks `PENDING_RESULT`.
fn wake_main_thread() {
    let tid = MAIN_THREAD_ID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

// ============================================================
// Entry point
// ============================================================

fn main() {
    // Redirect stdout/stderr to NUL so println! doesn't panic in GUI mode
    redirect_stdio_to_nul();

    unsafe {
        MAIN_THREAD_ID.store(GetCurrentThreadId(), Ordering::Relaxed);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let s = settings::load();
    *CURRENT_SETTINGS.lock().unwrap() = s.clone();

    popup::init();
    tray::create();
    register_hotkeys(&s);
    if s.punto_enabled {
        autotype::start();
    }
    if s.taskbar_center_enabled {
        taskbar_center::start();
    }

    run_main_loop();
    tray::destroy();
}


fn run_main_loop() {
    let mut msg = MSG::default();
    unsafe {
        loop {
            // Wait for a message or 100 ms timeout (to check PENDING_RESULT).
            MsgWaitForMultipleObjectsEx(None, 100, QS_ALLINPUT, MWMO_INPUTAVAILABLE);

            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                if msg.message == WM_HOTKEY {
                    dispatch_hotkey(msg.wParam.0 as i32);
                }
                if msg.message == tray::WM_TRAY_OPEN_SETTINGS {
                    handle_settings();
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Deliver background translation result to the popup.
            if let Some(r) = PENDING_RESULT.lock().unwrap().take() {
                let dir = if r.is_error { "error" } else { &r.direction };
                popup::show(&r.original, &r.translated, dir);
            }

            // Apply settings changes from the settings window.
            if let Some(new) = settings_ui::take_updated_settings() {
                println!("[*] Настройки обновлены");
                unregister_hotkeys();
                register_hotkeys(&new);

                // Punto Switcher
                if new.punto_enabled && !autotype::is_enabled() {
                    autotype::start();
                } else if !new.punto_enabled && autotype::is_enabled() {
                    autotype::toggle();
                }

                // Taskbar center
                if new.taskbar_center_enabled && !taskbar_center::is_enabled() {
                    taskbar_center::start();
                } else if !new.taskbar_center_enabled && taskbar_center::is_enabled() {
                    taskbar_center::stop();
                }

                *CURRENT_SETTINGS.lock().unwrap() = new;
            }
        }
    }
}

fn dispatch_hotkey(id: i32) {
    match id {
        HOTKEY_TRANSLATE => handle_text_translate(),
        HOTKEY_OCR => handle_screenshot_translate(),
        HOTKEY_SCREENSHOT => handle_screenshot_save(),
        HOTKEY_LAYOUT => handle_layout_switch(),
        HOTKEY_SETTINGS => handle_settings(),
        HOTKEY_AUTOTYPE => handle_autotype_toggle(),
        HOTKEY_TASKBAR_CENTER => handle_taskbar_center_toggle(),
        _ => {}
    }
}

// ============================================================
// Hotkey registration
// ============================================================

fn register_hotkeys(s: &settings::Settings) {
    unsafe {
        let reg = |id: i32, hk: &settings::HotkeyConfig| {
            let _ = RegisterHotKey(None, id, HOT_KEY_MODIFIERS(hk.modifiers), hk.vk);
        };
        reg(HOTKEY_TRANSLATE, &s.hk_translate);
        reg(HOTKEY_OCR, &s.hk_ocr);
        reg(HOTKEY_SCREENSHOT, &s.hk_screenshot);
        reg(HOTKEY_LAYOUT, &s.hk_layout);
        // Settings: always Ctrl+Alt+O
        let _ = RegisterHotKey(None, HOTKEY_SETTINGS, HOT_KEY_MODIFIERS(0x0003), 0x4F);
        // Autotype toggle: always Ctrl+Alt+A
        let _ = RegisterHotKey(None, HOTKEY_AUTOTYPE, HOT_KEY_MODIFIERS(0x0003), 0x41);
        // Taskbar center toggle: always Ctrl+Alt+T
        let _ = RegisterHotKey(None, HOTKEY_TASKBAR_CENTER, HOT_KEY_MODIFIERS(0x0003), 0x54);
    }
}

fn unregister_hotkeys() {
    unsafe {
        for id in ALL_HOTKEY_IDS {
            let _ = UnregisterHotKey(None, id);
        }
    }
}

// ============================================================
// Handlers
// ============================================================

fn handle_text_translate() {
    println!("[*] Копирование выделенного текста...");
    simulate_copy();
    thread::sleep(Duration::from_millis(250));

    let text = match clipboard_text() {
        Some(t) => t,
        None => return,
    };

    println!("[*] Текст: {}...", utils::truncate(&text, 80));
    popup::show_loading("Переводим...");

    thread::spawn(move || {
        init_com_in_thread();
        let result = match translate::translate(&text) {
            Ok((translated, direction)) => {
                println!("[+] Перевод ({}): {}...", direction, utils::truncate(&translated, 80));
                TranslationResult { original: text, translated, direction, is_error: false }
            }
            Err(e) => {
                println!("[!] Ошибка перевода: {e}");
                TranslationResult {
                    original: text,
                    translated: format!("Ошибка: {e}"),
                    direction: String::new(),
                    is_error: true,
                }
            }
        };
        *PENDING_RESULT.lock().unwrap() = Some(result);
        wake_main_thread();
    });
}

fn handle_screenshot_translate() {
    println!("[*] Выберите область экрана...");
    dispatch_capture_result(capture::select_and_capture());
}

fn handle_screenshot_save() {
    println!("[*] Выберите область для скриншота...");
    dispatch_capture_result(capture::select_and_capture());
}

fn dispatch_capture_result(result: Option<capture::CaptureAction>) {
    match result {
        Some(capture::CaptureAction::Translate(p, w, h)) => do_ocr_and_translate(&p, w, h),
        Some(capture::CaptureAction::Save(p, w, h)) => save_captured(&p, w, h),
        Some(capture::CaptureAction::Clipboard(p, w, h)) => copy_to_clipboard(&p, w, h),
        None => println!("[*] Отменено"),
    }
}

fn do_ocr_and_translate(pixels: &[u8], width: u32, height: u32) {
    println!("[*] Захвачена область {width}x{height}, распознавание...");
    popup::show_loading("Распознаём текст...");

    let pixels = pixels.to_vec();
    let screenshot_folder = get_settings().screenshot_folder.clone();

    thread::spawn(move || {
        init_com_in_thread();

        let text = match ocr::recognize(&pixels, width, height) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            Ok(_) => {
                println!("[!] Текст не распознан");
                deliver_result(TranslationResult {
                    original: String::new(),
                    translated: "Текст не распознан в выбранной области".into(),
                    direction: "info".into(),
                    is_error: false,
                });
                return;
            }
            Err(e) => {
                println!("[!] Ошибка OCR: {e}");
                deliver_result(TranslationResult {
                    original: String::new(),
                    translated: format!("Ошибка OCR: {e}"),
                    direction: String::new(),
                    is_error: true,
                });
                return;
            }
        };

        println!("[*] Распознано: {}...", utils::truncate(&text, 80));

        // Auto-save screenshot if folder is configured.
        if !screenshot_folder.is_empty() {
            match screenshot::save_png(&pixels, width, height, &screenshot_folder) {
                Ok(_) => println!("[+] Скрин автосохранён в {screenshot_folder}"),
                Err(e) => println!("[!] Автосохранение скрина: {e}"),
            }
        }

        let result = match translate::translate(&text) {
            Ok((translated, direction)) => {
                println!("[+] Перевод: {}...", utils::truncate(&translated, 80));
                TranslationResult { original: text, translated, direction, is_error: false }
            }
            Err(e) => {
                println!("[!] Ошибка перевода: {e}");
                TranslationResult {
                    original: text,
                    translated: format!("Ошибка: {e}"),
                    direction: String::new(),
                    is_error: true,
                }
            }
        };
        deliver_result(result);
    });
}

fn deliver_result(r: TranslationResult) {
    *PENDING_RESULT.lock().unwrap() = Some(r);
    wake_main_thread();
}

fn save_captured(pixels: &[u8], width: u32, height: u32) {
    match save_with_dialog(pixels, width, height) {
        Ok(Some(path)) => {
            println!("[+] Скриншот сохранён: {path}");
            popup::show("", &format!("Скриншот сохранён:\n{path}"), "info");
        }
        Ok(None) => println!("[*] Сохранение отменено"),
        Err(e) => {
            println!("[!] Ошибка сохранения: {e}");
            popup::show("", &format!("Ошибка: {e}"), "error");
        }
    }
}

fn save_with_dialog(pixels: &[u8], width: u32, height: u32) -> anyhow::Result<Option<String>> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileSaveDialog, IFileSaveDialog, SIGDN_FILESYSPATH};
    use windows::core::PCWSTR;

    let filter_name = utils::to_wide("PNG (*.png)");
    let filter_ext = utils::to_wide("*.png");

    unsafe {
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_ALL)
                .map_err(|e| anyhow::anyhow!("Dialog: {e}"))?;

        let filters = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(filter_name.as_ptr()),
            pszSpec: PCWSTR(filter_ext.as_ptr()),
        }];
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetDefaultExtension(windows::core::w!("png"));

        let now = chrono::Local::now();
        let default_name = utils::to_wide(&now.format("screenshot_%Y%m%d_%H%M%S.png").to_string());
        let _ = dialog.SetFileName(PCWSTR(default_name.as_ptr()));

        // Open in the configured screenshot folder.
        let s = get_settings();
        if !s.screenshot_folder.is_empty() {
            use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};
            let folder_wide = utils::to_wide(&s.screenshot_folder);
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(folder_wide.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }

        if dialog.Show(None).is_err() {
            return Ok(None);
        }

        let result = dialog.GetResult().map_err(|e| anyhow::anyhow!("GetResult: {e}"))?;
        let path = result
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|e| anyhow::anyhow!("GetPath: {e}"))?;
        let path_str = path.to_string().unwrap_or_default();
        CoTaskMemFree(Some(path.0 as *const _));

        screenshot::save_png_to_file(pixels, width, height, &path_str)?;
        Ok(Some(path_str))
    }
}

fn handle_layout_switch() {
    println!("[*] Смена раскладки выделенного текста...");
    simulate_copy();
    thread::sleep(Duration::from_millis(250));

    let text = match clipboard_text() {
        Some(t) => t,
        None => return,
    };

    let converted = layout::convert(&text);
    println!("[+] {} → {}", utils::truncate(&text, 40), utils::truncate(&converted, 40));

    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(&converted);
    }
    thread::sleep(Duration::from_millis(50));
    simulate_paste();
}

fn handle_settings() {
    settings_ui::open(&get_settings());
}

fn handle_autotype_toggle() {
    let enabled = autotype::toggle();
    let msg = if enabled {
        "Авто-смена раскладки включена"
    } else {
        "Авто-смена раскладки выключена"
    };
    println!("[*] {msg}");
    popup::show("", msg, "info");
}

fn handle_taskbar_center_toggle() {
    let enabled = taskbar_center::toggle();
    let msg = if enabled {
        "Центрирование иконок таскбара ВКЛ"
    } else {
        "Центрирование иконок таскбара ВЫКЛ"
    };
    println!("[*] {msg}");
    popup::show("", msg, "info");
}

fn copy_to_clipboard(pixels: &[u8], width: u32, height: u32) {
    // Convert BGRA → RGBA for arboard.
    let mut rgba = pixels.to_vec();
    for chunk in rgba.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    let Ok(mut cb) = arboard::Clipboard::new() else {
        println!("[!] Буфер обмена недоступен");
        return;
    };

    let img = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: std::borrow::Cow::Borrowed(&rgba),
    };
    match cb.set_image(img) {
        Ok(()) => {
            println!("[+] Скриншот скопирован в буфер обмена ({width}x{height})");
            popup::show("", &format!("Скриншот скопирован ({width}x{height})"), "info");
        }
        Err(e) => {
            println!("[!] Ошибка копирования: {e}");
            popup::show("", &format!("Ошибка: {e}"), "error");
        }
    }
}

// ============================================================
// stdio redirect (GUI mode)
// ============================================================

/// Without a console, `println!` panics on Windows.
/// Redirect stdout/stderr to NUL so writes silently succeed.
fn redirect_stdio_to_nul() {
    use std::os::windows::io::AsRawHandle;
    if let Ok(nul) = std::fs::File::create("NUL") {
        let handle = nul.as_raw_handle();
        unsafe {
            unsafe extern "system" {
                fn SetStdHandle(nStdHandle: u32, hHandle: *mut core::ffi::c_void) -> i32;
            }
            SetStdHandle(0xFFFF_FFF5, handle); // STD_OUTPUT_HANDLE
            SetStdHandle(0xFFFF_FFF4, handle); // STD_ERROR_HANDLE
        }
        std::mem::forget(nul); // keep handle alive for process lifetime
    }
}

// ============================================================
// Helpers
// ============================================================

/// Reads text from the clipboard, logging on failure.
fn clipboard_text() -> Option<String> {
    match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
        Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => {
            println!("[!] Буфер обмена пуст");
            None
        }
    }
}

/// Initialises COM in a worker thread.
fn init_com_in_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn simulate_copy() {
    send_key_combo(VK_CONTROL, VK_C);
}

fn simulate_paste() {
    send_key_combo(VK_CONTROL, VK_V);
}

/// Releases all modifiers, then sends `modifier + key`.
fn send_key_combo(modifier: VIRTUAL_KEY, key: VIRTUAL_KEY) {
    unsafe {
        // Release stale modifiers.
        let release = [make_key_input(VK_MENU, true), make_key_input(VK_CONTROL, true)];
        let _ = SendInput(&release, size_of::<INPUT>() as i32);
        thread::sleep(Duration::from_millis(50));

        let combo = [
            make_key_input(modifier, false),
            make_key_input(key, false),
            make_key_input(key, true),
            make_key_input(modifier, true),
        ];
        let _ = SendInput(&combo, size_of::<INPUT>() as i32);
    }
}
