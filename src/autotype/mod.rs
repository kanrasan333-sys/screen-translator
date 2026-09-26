//! Automatic layout correction, Punto Switcher style.
//!
//! Words typed on the wrong keyboard layout — "ghbdtn" for "привет", "руддщ"
//! for "hello", "привыт" for "привіт" — are retyped on the right one, and the
//! layout is switched so the rest comes out right. English, Russian and
//! Ukrainian, on whichever variants of those layouts are installed.
//!
//! * [`model`] — dictionaries and trigram models, one per language.
//! * [`keymap`] — what each key types on each installed layout.
//! * [`decide`] — whether a word was typed on the wrong layout, and which.
//! * [`engine`] — keystrokes in, corrections out; no Win32 in it.
//! * this file — the hook that feeds the engine and carries out its effects.
//!
//! The hooks live on a thread of their own. A low-level hook runs on the
//! thread that installed it, and every keystroke in the system waits for it:
//! installed on the main thread, which sleeps through clipboard round-trips
//! and blocks in scroll captures, it stalled all typing for the duration —
//! and past `LowLevelHooksTimeout` Windows removes such a hook without a word,
//! which is how correction used to stop working until a restart.

mod decide;
mod engine;
#[cfg(test)]
mod eval;
mod keymap;
mod model;

use crate::utils::{INJECTED_TAG, make_key_input_tagged, make_unicode_input_tagged};
use engine::{Action, Effect, Engine, Hkl, KeyDown, Layout};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD: Mutex<Option<(u32, JoinHandle<()>)>> = Mutex::new(None);
/// The layout-conversion hotkey, kept for an engine started later.
static HOTKEY: Mutex<Option<(u32, u16)>> = Mutex::new(None);

// ============================================================
// Public API
// ============================================================

pub fn start() {
    if ENABLED.load(Ordering::SeqCst) {
        return;
    }
    // A few dozen milliseconds of decompression: not on the first keystroke.
    thread::spawn(|| {
        model::load();
    });

    let (ready_tx, ready_rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("punto-hook".into())
        .spawn(move || hook_thread(ready_tx));
    let Ok(handle) = spawned else {
        println!("[punto] could not start the hook thread");
        return;
    };
    match ready_rx.recv() {
        Ok(Some(tid)) => {
            *HOOK_THREAD.lock().unwrap() = Some((tid, handle));
            ENABLED.store(true, Ordering::SeqCst);
            println!("[punto] on (Pause or the layout hotkey takes a correction back)");
        }
        _ => {
            let _ = handle.join();
            println!("[punto] failed to install the keyboard hook");
        }
    }
}

pub fn stop() {
    ENABLED.store(false, Ordering::SeqCst);
    if let Some((tid, handle)) = HOOK_THREAD.lock().unwrap().take() {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        let _ = handle.join();
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

pub fn toggle() -> bool {
    if is_enabled() {
        stop();
    } else {
        start();
    }
    is_enabled()
}

/// The layout hotkey (RegisterHotKey modifiers and virtual key). Pressing it
/// must not count as an edit, or it would wipe out the word it is about to
/// convert.
pub fn set_hotkey(modifiers: u32, vk: u32) {
    let hk = Some((modifiers, vk as u16));
    *HOTKEY.lock().unwrap() = hk;
    if let Some(e) = ENGINE.lock().unwrap().as_mut() {
        e.hotkey = hk;
    }
}

/// The layout hotkey, pressed: takes back the last correction, or converts
/// the word just typed. `false` if there is no such word — the caller falls
/// back to converting the selection.
///
/// Runs on the main thread while the hotkey's modifiers are still held, so
/// they are let go first: backspaces sent under a held Ctrl delete words.
pub fn convert_last_word() -> bool {
    let effects = {
        let mut guard = ENGINE.lock().unwrap();
        let Some(engine) = guard.as_mut() else {
            return false;
        };
        if !engine.hotkey_convert(model::loaded()) {
            return false;
        }
        persist_rejected(engine);
        std::mem::take(&mut engine.effects)
    };
    release_modifiers();
    execute(effects);
    true
}

/// Retypes `text`, a selection typed on the wrong layout, on the layout it
/// was meant for. Returns the new text and that layout.
pub fn convert_text(text: &str) -> Option<(String, isize)> {
    let models = model::load()?;
    // With the switcher running it knows which layout of each language the
    // user actually types on ("Russian (Ukraine)" or plain Russian) and which
    // Cyrillic they have been writing; without it, the first installed.
    let (targets, bias) = match ENGINE.lock().unwrap().as_ref() {
        Some(e) => (e.targets(), e.bias()),
        None => {
            let layouts = installed_layouts();
            let firsts = keymap::Lang::ALL
                .into_iter()
                .filter_map(|lang| layouts.iter().find(|l| l.map.lang == Some(lang)).cloned())
                .collect();
            (firsts, 0.0)
        }
    };
    let list: Vec<decide::Target> = targets
        .iter()
        .filter_map(|l| {
            Some(decide::Target {
                lang: l.map.lang?,
                map: &l.map,
            })
        })
        .collect();
    let never = |_: &str| false;
    let ctx = decide::Context {
        models,
        targets: &list,
        bias,
        continues: false,
        rejected: &never,
    };
    let (out, t) = decide::convert_text(&ctx, text)?;
    Some((out, targets[t].hkl))
}

/// Types `text` into the focused window, marked as ours.
pub fn type_text(text: &str) {
    execute(vec![Effect::Type(text.to_string())]);
}

/// Asks the focused window to switch to `hkl`.
pub fn switch_layout(hkl: isize) {
    request_layout(hkl);
}

// ============================================================
// The hook thread
// ============================================================

fn hook_thread(ready: mpsc::Sender<Option<u32>>) {
    let layouts = installed_layouts();
    let names: Vec<String> = layouts
        .iter()
        .map(|l| {
            let lang = l.map.lang.map_or("—", keymap::Lang::code);
            format!("{:08X} {lang}", l.hkl as u32)
        })
        .collect();
    println!("[punto] layouts: {}", names.join(", "));
    let mut engine = Engine::new(layouts);
    engine.hotkey = *HOTKEY.lock().unwrap();
    for w in load_rejected() {
        engine.reject(&w);
    }
    *ENGINE.lock().unwrap() = Some(engine);

    unsafe {
        let module = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default();
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
        let Ok(keyboard) = keyboard else {
            if let Ok(m) = mouse {
                let _ = UnhookWindowsHookEx(m);
            }
            *ENGINE.lock().unwrap() = None;
            let _ = ready.send(None);
            return;
        };
        let _ = ready.send(Some(GetCurrentThreadId()));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(keyboard);
        if let Ok(m) = mouse {
            let _ = UnhookWindowsHookEx(m);
        }
    }
    *ENGINE.lock().unwrap() = None;
}

unsafe extern "system" fn keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wp, lp);
        }
        let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        // Our own keystrokes carry a tag; they are the effect, not input.
        if info.dwExtraInfo == INJECTED_TAG {
            return CallNextHookEx(None, code, wp, lp);
        }
        let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let vk = info.vkCode as u16;

        let (action, effects) = {
            let mut guard = ENGINE.lock().unwrap();
            let Some(engine) = guard.as_mut() else {
                return CallNextHookEx(None, code, wp, lp);
            };
            let action = if down {
                let ev = key_event(info, engine);
                let action = engine.key_down(model::loaded(), &ev);
                persist_rejected(engine);
                action
            } else {
                engine.key_up(vk)
            };
            (action, std::mem::take(&mut engine.effects))
        };
        // Injected right here, inside the hook, so they line up ahead of any
        // keystroke typed after this one: SendInput only queues them, and the
        // next key cannot be processed until this call returns.
        if !effects.is_empty() {
            execute(effects);
        }
        match action {
            Action::Swallow => LRESULT(1),
            Action::Pass => CallNextHookEx(None, code, wp, lp),
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0
            && matches!(
                wp.0 as u32,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            )
            && let Some(engine) = ENGINE.lock().unwrap().as_mut()
        {
            engine.click();
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

thread_local! {
    /// Focused window last seen, and whether it is a password box.
    static FOCUS_CACHE: Cell<(isize, bool)> = const { Cell::new((0, false)) };
    /// Last layout the installed list was re-read for, so that one Windows
    /// does not list (an IME's) is not re-read on every keystroke.
    static REREAD_FOR: Cell<isize> = const { Cell::new(0) };
}

/// Everything about this keystroke the engine needs, looked up now.
fn key_event(info: &KBDLLHOOKSTRUCT, engine: &mut Engine) -> KeyDown {
    unsafe {
        let down = |vk: VIRTUAL_KEY| GetAsyncKeyState(vk.0 as i32) < 0;
        let (focus, thread) = focused();
        let mut hkl = GetKeyboardLayout(thread).0 as isize;

        // A layout we have not read yet: the user installed one.
        if hkl != 0 && !engine.knows(hkl) && REREAD_FOR.with(|r| r.replace(hkl)) != hkl {
            engine.set_layouts(installed_layouts());
        }
        // Password boxes are never touched; to the engine they are a layout
        // it cannot read.
        if is_password(focus) {
            hkl = 0;
        }

        let vk = info.vkCode as u16;
        let mut sc = info.scanCode as u8;
        if sc == 0 {
            // Synthesised input sometimes carries no scan code.
            sc = MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC, HKL(hkl as _)) as u8;
        }
        KeyDown {
            vk,
            sc,
            extended: info.flags.0 & LLKHF_EXTENDED.0 != 0,
            shift: down(VK_SHIFT),
            ctrl: down(VK_CONTROL),
            alt: down(VK_MENU),
            win: down(VK_LWIN) || down(VK_RWIN),
            caps: GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0,
            hkl,
            focus: focus.0 as isize,
            now: Instant::now(),
        }
    }
}

/// The focused window of the foreground thread, and the thread that owns
/// it — whose layout is the one keystrokes will be read through. The
/// foreground window alone gets this wrong for hosted content (a UWP app's
/// frame belongs to a different process than the page with the caret).
fn focused() -> (HWND, u32) {
    unsafe {
        let mut gti = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let hwnd = if GetGUIThreadInfo(0, &mut gti).is_ok() && !gti.hwndFocus.0.is_null() {
            gti.hwndFocus
        } else {
            GetForegroundWindow()
        };
        (hwnd, GetWindowThreadProcessId(hwnd, None))
    }
}

/// A classic edit control with ES_PASSWORD. Browsers and the like draw their
/// own password boxes and do not say so without accessibility round-trips far
/// too slow for a keyboard hook.
fn is_password(hwnd: HWND) -> bool {
    let key = hwnd.0 as isize;
    FOCUS_CACHE.with(|cache| {
        let (seen, answer) = cache.get();
        if seen == key {
            return answer;
        }
        let answer = unsafe {
            let mut class = [0u16; 32];
            let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
            let class = String::from_utf16_lossy(&class[..n]).to_ascii_lowercase();
            class.contains("edit")
                && GetWindowLongW(hwnd, GWL_STYLE) as u32 & ES_PASSWORD as u32 != 0
        };
        cache.set((key, answer));
        answer
    })
}

/// Every installed layout, with its key table read from Windows.
fn installed_layouts() -> Vec<Layout> {
    unsafe {
        let n = GetKeyboardLayoutList(None);
        if n <= 0 {
            return Vec::new();
        }
        let mut list = vec![HKL::default(); n as usize];
        let got = GetKeyboardLayoutList(Some(&mut list)).max(0) as usize;
        list.truncate(got);
        list.into_iter()
            .map(|hkl| Layout {
                hkl: hkl.0 as isize,
                map: Arc::new(keymap::from_hkl(hkl)),
            })
            .collect()
    }
}

// ============================================================
// Carrying out effects
// ============================================================

fn execute(effects: Vec<Effect>) {
    let mut inputs: Vec<INPUT> = Vec::new();
    let key = |inputs: &mut Vec<INPUT>, vk: VIRTUAL_KEY| {
        inputs.push(make_key_input_tagged(vk, false, INJECTED_TAG));
        inputs.push(make_key_input_tagged(vk, true, INJECTED_TAG));
    };
    let text = |inputs: &mut Vec<INPUT>, s: &str| {
        for u in s.encode_utf16() {
            inputs.push(make_unicode_input_tagged(u, false, INJECTED_TAG));
            inputs.push(make_unicode_input_tagged(u, true, INJECTED_TAG));
        }
    };
    for e in effects {
        match e {
            Effect::Replace { erase, text: t } => {
                for _ in 0..erase {
                    key(&mut inputs, VK_BACK);
                }
                text(&mut inputs, &t);
            }
            Effect::Type(t) => text(&mut inputs, &t),
            Effect::Key(vk) => key(&mut inputs, VIRTUAL_KEY(vk)),
            // Posted first: the application switches before it gets to the
            // keystrokes that follow ours.
            Effect::Layout(hkl) => request_layout(hkl),
        }
    }
    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, size_of::<INPUT>() as i32);
        }
    }
}

/// Asks the window with the caret to switch layout — the window Windows
/// itself sends the request to when the user presses Alt+Shift.
fn request_layout(hkl: Hkl) {
    let (hwnd, _) = focused();
    if !hwnd.0.is_null() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(hkl));
        }
    }
}

/// Lets go of every modifier still held, the hotkey's among them. A dummy
/// key goes first so that releasing Alt does not open the window's menu.
fn release_modifiers() {
    const MASK: VIRTUAL_KEY = VIRTUAL_KEY(0xE8); // unassigned
    let held: Vec<VIRTUAL_KEY> = [
        VK_LSHIFT, VK_RSHIFT, VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN,
    ]
    .into_iter()
    .filter(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0)
    .collect();
    if held.is_empty() {
        return;
    }
    let mut inputs = vec![
        make_key_input_tagged(MASK, false, INJECTED_TAG),
        make_key_input_tagged(MASK, true, INJECTED_TAG),
    ];
    inputs.extend(held.into_iter().map(|vk| make_key_input_tagged(vk, true, INJECTED_TAG)));
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

// ============================================================
// Words the user sent back
// ============================================================
//
// Kept across restarts: taking a correction back with the undo key says the
// word is right as typed, and it would be as wrong to correct it tomorrow.
// One word per line — the file can be edited by hand.

fn rejected_path() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").ok()?).join("screen-translator");
    Some(dir.join("punto_exceptions.txt"))
}

fn load_rejected() -> Vec<String> {
    rejected_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_lowercase())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn persist_rejected(engine: &mut Engine) {
    if engine.rejected_new.is_empty() {
        return;
    }
    let words = std::mem::take(&mut engine.rejected_new);
    let Some(path) = rejected_path() else {
        return;
    };
    // Off the hook thread: a slow disk must not hold up the keyboard.
    thread::spawn(move || {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            for w in words {
                let _ = writeln!(f, "{w}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    /// The layouts installed on this machine, as the switcher reads them.
    #[test]
    #[ignore]
    fn installed_layouts_read() {
        for l in super::installed_layouts() {
            let sample: String = [0x10u8, 0x1F, 0x1B, 0x28, 0x29, 0x33, 0x56]
                .iter()
                .map(|&sc| {
                    l.map
                        .render(super::keymap::Key {
                            sc,
                            shift: false,
                            caps: false,
                        })
                        .unwrap_or('·')
                })
                .collect();
            println!("{:08X} {:?} {sample}", l.hkl as u32, l.map.lang);
        }
    }
}
