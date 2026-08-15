#![windows_subsystem = "windows"]

mod autostart;
mod ask;
mod autotype;
mod button;
mod capture;
mod deepseek;
mod explorer_cmd;
mod i18n;
mod layout;
mod ocr;
mod paint;
mod popup;
mod screenshot;
mod scroll_capture;
mod settings;
mod settings_ui;
mod taskbar_center;
mod theme;
mod translate;
mod tray;
mod uia_scroll;
mod utils;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
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
const HOTKEY_EXPLORER_CMD: i32 = 8;
const HOTKEY_ASK: i32 = 9;

const ALL_HOTKEY_IDS: [i32; 9] = [
    HOTKEY_TRANSLATE,
    HOTKEY_OCR,
    HOTKEY_SCREENSHOT,
    HOTKEY_LAYOUT,
    HOTKEY_SETTINGS,
    HOTKEY_AUTOTYPE,
    HOTKEY_TASKBAR_CENTER,
    HOTKEY_EXPLORER_CMD,
    HOTKEY_ASK,
];

// ============================================================
// Global state
// ============================================================

static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Result delivered from a background translation thread.
struct TranslationResult {
    original: String,
    translated: String,
    direction: String,
    is_error: bool,
}
static PENDING_RESULT: Mutex<Option<TranslationResult>> = Mutex::new(None);

struct HotkeyRegistration {
    id: i32,
    label: String,
    config: settings::HotkeyConfig,
    /// User-configured hotkeys are "required" — failures pop a visible error.
    /// Hard-coded internal toggles (settings window, punto, taskbar centering)
    /// are best-effort; if another app already owns the combo we just log it
    /// and move on.  Those features are still reachable from the tray / UI.
    required: bool,
}

struct StaComGuard;

impl Drop for StaComGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn get_settings() -> settings::Settings {
    settings::current()
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
    // Redirect stdout/stderr to a log file so println! doesn't panic in GUI mode
    redirect_stdio_to_log();

    unsafe {
        MAIN_THREAD_ID.store(GetCurrentThreadId(), Ordering::Relaxed);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let s = settings::load();
    i18n::set_from_code(&s.language);
    settings::set_current(s.clone());

    popup::init();
    tray::create();
    let hotkey_failures = register_hotkeys(&s);
    if s.punto_enabled {
        autotype::start();
    }
    if s.taskbar_center_enabled {
        taskbar_center::start();
    }
    show_hotkey_registration_failures(&hotkey_failures);

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
                println!("[*] Settings updated");
                i18n::set_from_code(&new.language);
                unregister_hotkeys();
                let hotkey_failures = register_hotkeys(&new);

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

                settings::set_current(new);
                show_hotkey_registration_failures(&hotkey_failures);
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
        HOTKEY_EXPLORER_CMD => handle_explorer_cmd(),
        HOTKEY_ASK => ask::open(),
        _ => {}
    }
}

// ============================================================
// Hotkey registration
// ============================================================

fn register_hotkeys(s: &settings::Settings) -> Vec<String> {
    let registrations = vec![
        HotkeyRegistration {
            id: HOTKEY_TRANSLATE,
            label: i18n::t("settings.hotkey.translate").to_string(),
            config: s.hk_translate.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_OCR,
            label: i18n::t("settings.hotkey.ocr").to_string(),
            config: s.hk_ocr.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_SCREENSHOT,
            label: i18n::t("settings.hotkey.screenshot").to_string(),
            config: s.hk_screenshot.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_LAYOUT,
            label: i18n::t("settings.hotkey.layout").to_string(),
            config: s.hk_layout.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_EXPLORER_CMD,
            label: i18n::t("settings.hotkey.explorer_cmd").to_string(),
            config: s.hk_explorer_cmd.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_ASK,
            label: i18n::t("settings.hotkey.ask").to_string(),
            config: s.hk_ask.clone(),
            required: true,
        },
        HotkeyRegistration {
            id: HOTKEY_SETTINGS,
            label: "Settings window".to_string(),
            config: settings::HotkeyConfig {
                modifiers: 0x0003,
                vk: 0x4F,
            },
            required: false,
        },
        HotkeyRegistration {
            id: HOTKEY_AUTOTYPE,
            label: "Toggle auto layout correction".to_string(),
            config: settings::HotkeyConfig {
                modifiers: 0x0003,
                vk: 0x41,
            },
            required: false,
        },
        HotkeyRegistration {
            id: HOTKEY_TASKBAR_CENTER,
            label: "Toggle taskbar centering".to_string(),
            config: settings::HotkeyConfig {
                modifiers: 0x0003,
                vk: 0x4D,
            },
            required: false,
        },
    ];

    let mut failures = Vec::new();
    unsafe {
        for reg in registrations {
            if RegisterHotKey(
                None,
                reg.id,
                HOT_KEY_MODIFIERS(reg.config.modifiers),
                reg.config.vk,
            )
            .is_err()
            {
                if reg.required {
                    failures.push(format!("{} ({})", reg.label, reg.config.display()));
                } else {
                    println!(
                        "[!] Optional hotkey skipped: {} ({})",
                        reg.label,
                        reg.config.display()
                    );
                }
            }
        }
    }
    failures
}

fn unregister_hotkeys() {
    unsafe {
        for id in ALL_HOTKEY_IDS {
            let _ = UnregisterHotKey(None, id);
        }
    }
}

fn show_hotkey_registration_failures(failures: &[String]) {
    if failures.is_empty() {
        return;
    }

    let details = failures.join("\n");
    let message = format!(
        "{}Hotkey unavailable:\n{details}",
        i18n::t("popup.error_prefix")
    );
    println!("[!] {message}");
    popup::show("", &message, "error");
}

// ============================================================
// Handlers
// ============================================================

fn handle_text_translate() {
    println!("[*] Copying selected text...");
    simulate_copy();
    thread::sleep(Duration::from_millis(250));

    let text = match clipboard_text() {
        Some(t) => t,
        None => return,
    };

    println!("[*] Text: {}...", utils::truncate(&text, 80));
    popup::show_loading(i18n::t("popup.translating"));

    thread::spawn(move || {
        init_com_in_thread();
        let result = match translate::translate(&text) {
            Ok((translated, direction)) => {
                println!(
                    "[+] Translated ({}): {}...",
                    direction,
                    utils::truncate(&translated, 80)
                );
                TranslationResult {
                    original: text,
                    translated,
                    direction,
                    is_error: false,
                }
            }
            Err(e) => {
                println!("[!] Translation error: {e}");
                TranslationResult {
                    original: text,
                    translated: format!("{}{e}", i18n::t("popup.error_prefix")),
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
    println!("[*] Select a screen area...");
    dispatch_capture_result(capture::select_and_capture());
}

fn handle_screenshot_save() {
    println!("[*] Select a screen area for screenshot...");
    dispatch_capture_result(capture::select_and_capture());
}

fn dispatch_capture_result(result: Option<capture::CaptureAction>) {
    match result {
        Some(capture::CaptureAction::Translate(p, w, h)) => do_ocr_and_translate(&p, w, h),
        Some(capture::CaptureAction::Save(p, w, h)) => save_captured(&p, w, h),
        Some(capture::CaptureAction::Clipboard(p, w, h)) => copy_to_clipboard(&p, w, h),
        Some(capture::CaptureAction::FullPage { x, y, w, h }) => capture_full_page(x, y, w, h),
        None => println!("[*] Cancelled"),
    }
}

/// Scroll-captures the selected region and hands the stitched strip to the
/// regular save flow.  Runs on the main thread: it drives the mouse wheel and
/// grabs the live screen, so nothing else may paint over the region meanwhile.
fn capture_full_page(x: i32, y: i32, w: i32, h: i32) {
    println!("[*] Scrolling capture of {w}x{h} at {x},{y}...");
    match scroll_capture::capture(x, y, w, h) {
        Ok((pixels, pw, ph)) => {
            println!("[+] Stitched {pw}x{ph}");
            save_captured(&pixels, pw, ph);
        }
        Err(e) => {
            println!("[!] Full-page capture error: {e}");
            popup::show(
                "",
                &format!("{}{e}", i18n::t("popup.error_prefix")),
                "error",
            );
        }
    }
}

fn do_ocr_and_translate(pixels: &[u8], width: u32, height: u32) {
    println!("[*] Captured {width}x{height}, running OCR...");
    popup::show_loading(i18n::t("popup.recognizing"));

    let pixels = pixels.to_vec();
    let screenshot_folder = get_settings().screenshot_folder.clone();

    thread::spawn(move || {
        init_com_in_thread();

        let text = match ocr::recognize(&pixels, width, height) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            Ok(_) => {
                println!("[!] No text recognized");
                deliver_result(TranslationResult {
                    original: String::new(),
                    translated: i18n::t("popup.no_text").to_string(),
                    direction: "info".into(),
                    is_error: false,
                });
                return;
            }
            Err(e) => {
                println!("[!] OCR error: {e}");
                deliver_result(TranslationResult {
                    original: String::new(),
                    translated: format!("{}{e}", i18n::t("popup.ocr_error_prefix")),
                    direction: String::new(),
                    is_error: true,
                });
                return;
            }
        };

        println!("[*] Recognized: {}...", utils::truncate(&text, 80));

        // Auto-save screenshot if folder is configured.
        if !screenshot_folder.is_empty() {
            match screenshot::save_png(&pixels, width, height, &screenshot_folder) {
                Ok(_) => println!("[+] Auto-saved screenshot to {screenshot_folder}"),
                Err(e) => println!("[!] Auto-save error: {e}"),
            }
        }

        let result = match translate::translate(&text) {
            Ok((translated, direction)) => {
                println!("[+] Translated: {}...", utils::truncate(&translated, 80));
                TranslationResult {
                    original: text,
                    translated,
                    direction,
                    is_error: false,
                }
            }
            Err(e) => {
                println!("[!] Translation error: {e}");
                TranslationResult {
                    original: text,
                    translated: format!("{}{e}", i18n::t("popup.error_prefix")),
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
            println!("[+] Screenshot saved: {path}");
            popup::show(
                "",
                &format!("{}:\n{}", i18n::t("popup.screenshot_saved"), path),
                "info",
            );
        }
        Ok(None) => println!("[*] Save cancelled"),
        Err(e) => {
            println!("[!] Save error: {e}");
            popup::show(
                "",
                &format!("{}{e}", i18n::t("popup.error_prefix")),
                "error",
            );
        }
    }
}

fn save_with_dialog(pixels: &[u8], width: u32, height: u32) -> anyhow::Result<Option<String>> {
    let screenshot_folder = {
        let settings = get_settings();
        if settings.screenshot_folder.is_empty() {
            None
        } else {
            Some(settings.screenshot_folder)
        }
    };

    let Some(path) = pick_png_save_path(screenshot_folder)? else {
        return Ok(None);
    };

    screenshot::save_png_to_file(pixels, width, height, &path)?;
    Ok(Some(path))
}

fn pick_png_save_path(initial_folder: Option<String>) -> anyhow::Result<Option<String>> {
    thread::spawn(move || -> anyhow::Result<Option<String>> {
        use windows::Win32::System::Com::{
            CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoTaskMemFree,
        };
        use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
        use windows::Win32::UI::Shell::{
            FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName,
            SIGDN_FILESYSPATH,
        };
        use windows::core::PCWSTR;

        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|e| anyhow::anyhow!("CoInitializeEx: {e}"))?;
            let _guard = StaComGuard;

            let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_ALL)
                .map_err(|e| anyhow::anyhow!("Dialog: {e}"))?;

            let filter_name = utils::to_wide("PNG (*.png)");
            let filter_ext = utils::to_wide("*.png");
            let filters = [COMDLG_FILTERSPEC {
                pszName: PCWSTR(filter_name.as_ptr()),
                pszSpec: PCWSTR(filter_ext.as_ptr()),
            }];
            let _ = dialog.SetFileTypes(&filters);
            let _ = dialog.SetDefaultExtension(windows::core::w!("png"));

            let now = chrono::Local::now();
            let default_name =
                utils::to_wide(&now.format("screenshot_%Y%m%d_%H%M%S.png").to_string());
            let _ = dialog.SetFileName(PCWSTR(default_name.as_ptr()));

            if let Some(folder) = initial_folder.as_ref() {
                let folder_wide = utils::to_wide(folder);
                if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                    PCWSTR(folder_wide.as_ptr()),
                    None,
                ) {
                    let _ = dialog.SetFolder(&item);
                }
            }

            if dialog.Show(None).is_err() {
                return Ok(None);
            }

            let result = dialog
                .GetResult()
                .map_err(|e| anyhow::anyhow!("GetResult: {e}"))?;
            let path = result
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| anyhow::anyhow!("GetPath: {e}"))?;
            let path_str = path.to_string().unwrap_or_default();
            CoTaskMemFree(Some(path.0 as *const _));
            Ok(Some(path_str))
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Save dialog thread panicked"))?
}

fn handle_layout_switch() {
    println!("[*] Switching layout of selected text...");
    simulate_copy();
    thread::sleep(Duration::from_millis(250));

    let text = match clipboard_text() {
        Some(t) => t,
        None => return,
    };

    let converted = layout::convert(&text);
    println!(
        "[+] {} -> {}",
        utils::truncate(&text, 40),
        utils::truncate(&converted, 40)
    );

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
    let msg = i18n::t(if enabled {
        "popup.autotype_on"
    } else {
        "popup.autotype_off"
    });
    println!("[*] {msg}");
    popup::show("", msg, "info");
}

fn handle_taskbar_center_toggle() {
    let enabled = taskbar_center::toggle();
    let msg = i18n::t(if enabled {
        "popup.taskbar_on"
    } else {
        "popup.taskbar_off"
    });
    println!("[*] {msg}");
    popup::show("", msg, "info");
}

fn handle_explorer_cmd() {
    println!("[*] Opening CMD in active Explorer folder...");
    explorer_cmd::open_in_active_folder();
}

fn copy_to_clipboard(pixels: &[u8], width: u32, height: u32) {
    // Convert BGRA → RGBA for arboard.
    let mut rgba = pixels.to_vec();
    for chunk in rgba.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    let Ok(mut cb) = arboard::Clipboard::new() else {
        println!("[!] Clipboard unavailable");
        return;
    };

    let img = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: std::borrow::Cow::Borrowed(&rgba),
    };
    match cb.set_image(img) {
        Ok(()) => {
            println!("[+] Screenshot copied to clipboard ({width}x{height})");
            popup::show(
                "",
                &format!("{} ({width}x{height})", i18n::t("popup.screenshot_copied")),
                "info",
            );
        }
        Err(e) => {
            println!("[!] Copy error: {e}");
            popup::show(
                "",
                &format!("{}{e}", i18n::t("popup.error_prefix")),
                "error",
            );
        }
    }
}

// ============================================================
// stdio redirect (GUI mode)
// ============================================================

/// Without a console, `println!` panics on Windows, so stdout and stderr have
/// to go somewhere.  They go to a log file next to the settings rather than to
/// NUL: when a capture or a hotkey misbehaves on someone else's machine, the
/// alternative is guessing.  Truncated at every start, so it stays small.
fn redirect_stdio_to_log() {
    use std::os::windows::io::AsRawHandle;

    let path = std::env::var("APPDATA")
        .map(|dir| std::path::PathBuf::from(dir).join("screen-translator"))
        .map(|dir| {
            let _ = std::fs::create_dir_all(&dir);
            dir.join("log.txt")
        });

    let file = path
        .ok()
        .and_then(|p| std::fs::File::create(p).ok())
        .or_else(|| std::fs::File::create("NUL").ok());

    if let Some(file) = file {
        let handle = file.as_raw_handle();
        unsafe {
            unsafe extern "system" {
                fn SetStdHandle(nStdHandle: u32, hHandle: *mut core::ffi::c_void) -> i32;
            }
            SetStdHandle(0xFFFF_FFF5, handle); // STD_OUTPUT_HANDLE
            SetStdHandle(0xFFFF_FFF4, handle); // STD_ERROR_HANDLE
        }
        std::mem::forget(file); // keep the handle alive for the process lifetime
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
            println!("[!] Clipboard is empty");
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
        let release = [
            make_key_input(VK_SHIFT, true),
            make_key_input(VK_MENU, true),
            make_key_input(VK_CONTROL, true),
        ];
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
