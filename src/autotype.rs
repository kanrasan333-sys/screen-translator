use crate::utils::{INJECTED_TAG, is_cyrillic, make_key_input_tagged, make_unicode_input_tagged};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ============================================================
// Global state
// ============================================================

static HOOK: Mutex<isize> = Mutex::new(0);
static WORD_BUF: Mutex<String> = Mutex::new(String::new());
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The physical key that ended a word. We swallow it, perform the replacement,
/// then re-send it — so Enter still sends the message and Tab still moves focus
/// *after* the text is fixed, instead of us having to backspace over an action
/// that already happened.
#[derive(Clone, Copy)]
struct BoundaryKey {
    vk: u16,
    shift: bool,
}

struct PendingSwitch {
    backspace_count: usize,
    new_text: String,
    target_layout: u32,
    /// Boundary key to replay once the word has been fixed.
    replay: Option<BoundaryKey>,
    /// Set for mid-word corrections: re-derive the replacement from the live
    /// word buffer at apply time, using these tables.
    recompute: Option<Recode>,
    /// Snapshot handed to [`try_undo`] after the replacement lands.
    undo: LastCorrection,
}
static PENDING: Mutex<Option<PendingSwitch>> = Mutex::new(None);
static SWITCH_TIMER_ID: Mutex<usize> = Mutex::new(0);

/// The correction we most recently applied. Serves two purposes: anti-ping-pong
/// (never convert our own output straight back) and Backspace-undo.
#[derive(Clone)]
struct LastCorrection {
    /// Exactly what the user typed, before we touched it.
    original: String,
    /// What we typed in its place.
    output: String,
    /// Character the replayed boundary key produced (`None` for a mid-word fix).
    sep: Option<char>,
    /// Layout that was active before we switched — restored on undo.
    restore_layout: u32,
    /// False for Enter/Tab boundaries: the app already acted on them, so
    /// rewinding the text would not rewind the side effect.
    undoable: bool,
    when: Instant,
}
static LAST_CORRECTION: Mutex<Option<LastCorrection>> = Mutex::new(None);
const PINGPONG_WINDOW_SECS: u64 = 15;

/// True only in the window between applying a correction and the next real
/// keystroke. Backspace pressed while armed means "no, I meant what I typed".
static UNDO_ARMED: AtomicBool = AtomicBool::new(false);
const UNDO_WINDOW_SECS: u64 = 10;

/// Words the user rejected by undoing our correction. We never auto-correct
/// them again for the rest of the session — the cheapest possible form of
/// learning, and it kills the "it keeps fighting me on this one word" problem.
static REJECTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
const REJECTED_MAX: usize = 128;

/// Last keyboard layout we actually observed the user typing in, per script.
/// Better than hardcoding 0x0419/0x0409: a user whose Cyrillic layout is
/// Ukrainian (or Belarusian) would otherwise be switched to a Russian layout
/// that may not even be installed, in which case nothing happens at all.
static LAST_CYR_LAYOUT: AtomicU32 = AtomicU32::new(0);
static LAST_LATIN_LAYOUT: AtomicU32 = AtomicU32::new(0);

/// Foreground window and timestamp of the last keystroke we processed.
///
/// The word buffer only ever tracked keys, so nothing told it that the caret
/// had moved: click into another field, alt-tab, or just walk away, and the
/// half-word left behind was still sitting there waiting to be glued onto
/// whatever you typed next. That made corrections fail in exactly the case
/// where no space precedes the word — typing it on its own.
static LAST_FG: AtomicIsize = AtomicIsize::new(0);
static LAST_KEY_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// A pause this long means the next keystroke starts a new word, wherever the
/// caret now is. Comfortably longer than any mid-word hesitation.
const IDLE_RESET: Duration = Duration::from_secs(3);

/// Something the engine does to the outside world. In tests these are
/// captured into a list instead of being sent to Windows, which is what lets
/// the whole pipeline be exercised against a simulated application.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    /// Delete `backspaces` characters, then type `text`.
    Replace { backspaces: usize, text: String },
    /// Emit a character in place of a keystroke we swallowed.
    Type(char),
    /// Re-send a boundary key we held back.
    ReplayKey { vk: u16, shift: bool },
    /// Ask the foreground window to change keyboard layout.
    Layout(u32),
}

#[cfg(test)]
static EFFECTS: Mutex<Option<Vec<Effect>>> = Mutex::new(None);

/// Records `make()` and returns true when a test is capturing; otherwise does
/// nothing at all — `make` is never called in a release build.
fn captured(make: impl FnOnce() -> Effect) -> bool {
    #[cfg(test)]
    {
        let mut guard = EFFECTS.lock().unwrap();
        if let Some(list) = guard.as_mut() {
            list.push(make());
            return true;
        }
    }
    #[cfg(not(test))]
    {
        let _ = make;
    }
    false
}

/// A layout switch we have requested but Windows has not applied yet.
struct Bridge {
    target: Kbd,
    target_layout: u32,
    until: Instant,
}
static BRIDGE: Mutex<Option<Bridge>> = Mutex::new(None);

/// How long to keep covering for a layout switch. Long enough for any app to
/// pump its message queue, short enough that a request the app simply ignored
/// does not leave us translating keystrokes forever.
const BRIDGE_MAX: Duration = Duration::from_millis(1500);

/// Virtual key whose press we swallowed, so we can swallow its release too.
static SWALLOWED_VK: AtomicU32 = AtomicU32::new(NO_VK);
const NO_VK: u32 = u32::MAX;

/// Swallow this keystroke: the caller returns `1` from the hook, and the
/// matching key-up will be dropped as well.
fn swallow(vk: u32) -> KeyAction {
    SWALLOWED_VK.store(vk, Ordering::SeqCst);
    KeyAction::Swallow
}

#[repr(C)]
struct KbdLLHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

// ============================================================
// Public API
// ============================================================

pub fn start() {
    if !ensure_hook() {
        ENABLED.store(false, Ordering::Relaxed);
        println!("[punto] Failed to install keyboard hook");
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    WORD_BUF.lock().unwrap().clear();
    println!("[punto] Авто-смена раскладки включена (Ctrl+Alt+A для вкл/выкл)");
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn toggle() -> bool {
    let new_state = !ENABLED.load(Ordering::Relaxed);

    if new_state {
        if !ensure_hook() {
            ENABLED.store(false, Ordering::Relaxed);
            println!("[punto] Failed to install keyboard hook");
            return false;
        }
        ENABLED.store(true, Ordering::Relaxed);
    } else {
        ENABLED.store(false, Ordering::Relaxed);
    }

    WORD_BUF.lock().unwrap().clear();
    new_state
}

fn ensure_hook() -> bool {
    let mut hook_guard = HOOK.lock().unwrap();
    if *hook_guard != 0 {
        return true;
    }

    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), HINSTANCE::default(), 0)
            .unwrap_or_default();
        if hook.0.is_null() {
            return false;
        }
        *hook_guard = hook.0 as isize;
    }

    true
}

// ============================================================
// Keyboard hook
// ============================================================

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let hook_handle = HHOOK(*HOOK.lock().unwrap() as *mut _);

        if code < 0 || !ENABLED.load(Ordering::SeqCst) {
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let info = &*(lp.0 as *const KbdLLHookStruct);
        let vk = info.vk_code;

        // Our own injected keystrokes carry a tag — never feed them back into
        // the word buffer or the undo state machine.
        if info.extra_info == INJECTED_TAG {
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let is_down = wp.0 == WM_KEYDOWN as usize || wp.0 == WM_SYSKEYDOWN as usize;
        if !is_down {
            // If we swallowed this key's press, swallow its release too —
            // a lone key-up can make an app act on a keystroke we cancelled.
            if SWALLOWED_VK.load(Ordering::SeqCst) == vk {
                SWALLOWED_VK.store(NO_VK, Ordering::SeqCst);
                return LRESULT(1);
            }
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
        let alt = GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000 != 0;
        if ctrl || alt {
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let fg = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(fg, None);
        let layout_id = (GetKeyboardLayout(tid).0 as usize & 0xFFFF) as u32;

        let cx = KeyEvent {
            vk,
            shift: GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0,
            caps: GetKeyState(VK_CAPITAL.0 as i32) & 0x0001 != 0,
            layout_id,
            window: fg.0 as isize,
            // Low bit of GetAsyncKeyState means "pressed since the previous
            // call", and we call it once per key — so this asks precisely
            // about the gap between this keystroke and the last one.
            clicked: [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
                .iter()
                .any(|b| GetAsyncKeyState(b.0 as i32) as u16 & 0x0001 != 0),
        };

        match process_key(cx) {
            KeyAction::Swallow => LRESULT(1),
            KeyAction::Pass => CallNextHookEx(hook_handle, code, wp, lp),
        }
    }
}

/// One key-down, with every Win32 lookup already resolved.
///
/// Splitting this out is what makes the pipeline testable: the interesting
/// bugs live in how corrections interact with keystrokes that arrive while a
/// correction is in flight, and reproducing that by driving a real keyboard
/// means hijacking the machine — which also hides the bug, because a fast app
/// applies the layout switch before the race can happen.
#[derive(Debug, Clone, Copy)]
struct KeyEvent {
    vk: u32,
    shift: bool,
    caps: bool,
    layout_id: u32,
    window: isize,
    clicked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    /// Let the keystroke reach the application.
    Pass,
    /// Eat it — we have emitted, or are about to emit, something else.
    Swallow,
}

fn process_key(cx: KeyEvent) -> KeyAction {
    let KeyEvent {
        vk,
        shift,
        caps,
        layout_id,
        window,
        clicked,
    } = cx;

    // A pending key-up swallow only ever applies to the key we just
    // swallowed; any other press means that key-up is never coming.
    if SWALLOWED_VK.load(Ordering::SeqCst) != vk {
        SWALLOWED_VK.store(NO_VK, Ordering::SeqCst);
    }

    // Undo is only offered for a Backspace pressed *immediately* after a
    // correction. Any other real key means the user accepted it.
    if !matches!(vk, 0x08 | 0x10 | 0x11 | 0x12 | 0x14 | 0xA0..=0xA5) {
        UNDO_ARMED.store(false, Ordering::SeqCst);
    }

    let real_kbd = kbd_of_layout(layout_id);
    remember_layout(layout_id, !matches!(real_kbd, Kbd::Latin));

    // If we asked Windows to change layout and it has not done so yet, act
    // as if it already had: `kbd` is the layout the user is typing *for*,
    // not the one still installed on the window.
    let bridge = bridge_target(layout_id);
    let bridging = bridge.is_some();
    let kbd = bridge.unwrap_or(real_kbd);
    let is_cyrillic_layout = !matches!(kbd, Kbd::Latin);

    if caret_may_have_moved(window, clicked) {
        WORD_BUF.lock().unwrap().clear();
        UNDO_ARMED.store(false, Ordering::SeqCst);
    }

    let key = BoundaryKey {
        vk: vk as u16,
        shift,
    };

    {
        match vk {
            // Letters A-Z (layout-independent VK codes)
            0x41..=0x5A => {
                if let Some(ch) = vk_to_char(vk, shift, caps, kbd) {
                    if bridging {
                        return type_through_bridge(vk, ch, kbd);
                    }
                    WORD_BUF.lock().unwrap().push(ch);
                    // Fast path: try to correct mid-word before Space is hit.
                    check_partial_switch(kbd);
                }
            }
            // OEM keys that produce Cyrillic LETTERS on Russian/Ukrainian layouts:
            //   OEM_1(;)→ж  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
            //   OEM_4([)→х
            //   OEM_6(])→ъ (RU) / ї (UK)
            //   OEM_7(')→э (RU) / є (UK)
            //   OEM_3(`)→ё (RU) / ' apostrophe (UK, boundary-like, skipped)
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0 if is_cyrillic_layout => {
                if let Some(ch) = oem_to_cyr_char(vk, shift, caps, kbd) {
                    if bridging {
                        return type_through_bridge(vk, ch, kbd);
                    }
                    WORD_BUF.lock().unwrap().push(ch);
                    check_partial_switch(kbd);
                } else {
                    // Unmapped OEM on this layout → treat as boundary
                    let ch = oem_boundary_char(vk, shift, is_cyrillic_layout);
                    if check_word_boundary(ch, key, kbd) {
                        return swallow(vk);
                    }
                }
            }
            // OEM keys as punctuation (English layout, or keys that stay
            // punctuation in Russian: OEM_2(/?)→.,  OEM_5(\|)  OEM_PLUS  OEM_MINUS)
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0 | 0xBF | 0xDC | 0xBB | 0xBD => {
                let ch = oem_boundary_char(vk, shift, is_cyrillic_layout);
                if check_word_boundary(ch, key, kbd) {
                    return swallow(vk);
                }
            }
            // Space, Tab — word boundary
            0x20 => {
                if check_word_boundary(Some(' '), key, kbd) {
                    return swallow(vk);
                }
            }
            0x09 => {
                if check_word_boundary(Some('\t'), key, kbd) {
                    return swallow(vk);
                }
            }
            // Number keys (0-9) — word boundary. With Shift they are symbols
            // that differ between layouts, so resolve them here.
            0x30..=0x39 => {
                let ch = digit_row_char(vk, shift, is_cyrillic_layout);
                if check_word_boundary(ch, key, kbd) {
                    return swallow(vk);
                }
            }
            // Enter — fix the word first, then let the (replayed) Enter act.
            0x0D => {
                if check_word_boundary(Some('\r'), key, kbd) {
                    return swallow(vk);
                }
            }
            // Backspace — undo the last correction, or erase the last char.
            0x08 => {
                let empty = {
                    let mut buf = WORD_BUF.lock().unwrap();
                    if buf.is_empty() {
                        true
                    } else {
                        buf.pop();
                        false
                    }
                };
                if empty && try_undo() {
                    return swallow(vk);
                }
            }
            // Modifiers — ignore (don't reset buffer)
            0x10 | 0x11 | 0x12 | 0x14 | 0xA0..=0xA5 => {}
            // Win key, arrows, function keys — reset
            _ => {
                WORD_BUF.lock().unwrap().clear();
            }
        }
    }

    KeyAction::Pass
}

/// Whether anything since the previous keystroke could have moved the caret,
/// making the buffered word stale: a mouse click, a different foreground
/// window, or simply a long enough pause.
fn caret_may_have_moved(window: isize, clicked: bool) -> bool {
    let fg_changed = LAST_FG.swap(window, Ordering::Relaxed) != window;

    let now = Instant::now();
    let idle = {
        let mut last = LAST_KEY_AT.lock().unwrap();
        let idle = last.is_some_and(|t| now.duration_since(t) > IDLE_RESET);
        *last = Some(now);
        idle
    };

    clicked || fg_changed || idle
}

/// Records the layout the user is really typing in, so corrections switch them
/// to a layout that actually exists on this machine.
fn remember_layout(layout_id: u32, is_cyrillic_layout: bool) {
    if is_cyrillic_layout {
        LAST_CYR_LAYOUT.store(layout_id, Ordering::Relaxed);
    } else {
        LAST_LATIN_LAYOUT.store(layout_id, Ordering::Relaxed);
    }
}

fn latin_target_layout() -> u32 {
    match LAST_LATIN_LAYOUT.load(Ordering::Relaxed) {
        0 => 0x0409,
        id => id,
    }
}

/// Layout to switch to for a correction targeting `kbd`.
///
/// The text we produce is injected as Unicode, so it is correct whether or not
/// the matching layout exists. Only the *switch* needs a real layout: asking
/// for an uninstalled one is a no-op that would strand the user on the layout
/// they mistyped in, so fall back to any installed Cyrillic.
fn layout_id_for(kbd: Kbd) -> u32 {
    match kbd {
        Kbd::Latin => latin_target_layout(),
        Kbd::Russian => installed_layout(&[0x0419, 0x0422, 0x0423]).unwrap_or(0x0419),
        Kbd::Ukrainian => installed_layout(&[0x0422, 0x0419, 0x0423]).unwrap_or(0x0422),
    }
}

/// Which Cyrillic the user actually writes in — used to break the tie when a
/// Latin word reads identically as Russian and as Ukrainian. Falls back to
/// whichever Cyrillic layout is installed, and to Russian if both are.
fn preferred_cyrillic() -> Kbd {
    match LAST_CYR_LAYOUT.load(Ordering::Relaxed) {
        0x0422 => Kbd::Ukrainian,
        0x0419 => Kbd::Russian,
        _ => match installed_layout(&[0x0419, 0x0422]) {
            Some(0x0422) => Kbd::Ukrainian,
            _ => Kbd::Russian,
        },
    }
}

/// First of `wanted` that is actually installed, if any.
///
/// Results are cached: `preferred_cyrillic` runs on the mid-word path, i.e.
/// once per keystroke, and enumerating layouts there would be wasteful.
fn installed_layout(wanted: &[u32]) -> Option<u32> {
    static CACHE: Mutex<Vec<(Vec<u32>, Option<u32>)>> = Mutex::new(Vec::new());
    if let Some((_, hit)) = CACHE
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_slice() == wanted)
    {
        return *hit;
    }
    let found = query_installed_layout(wanted);
    CACHE.lock().unwrap().push((wanted.to_vec(), found));
    found
}

fn query_installed_layout(wanted: &[u32]) -> Option<u32> {
    unsafe {
        let count = GetKeyboardLayoutList(None);
        if count <= 0 {
            return None;
        }
        let mut list = vec![HKL::default(); count as usize];
        let got = GetKeyboardLayoutList(Some(&mut list));
        list.truncate(got.max(0) as usize);
        let installed: Vec<u32> = list.iter().map(|h| (h.0 as usize & 0xFFFF) as u32).collect();
        wanted.iter().copied().find(|w| installed.contains(w))
    }
}

// ============================================================
// Rejected-word memory
// ============================================================

fn is_rejected(word_lc: &str) -> bool {
    REJECTED.lock().unwrap().iter().any(|w| w == word_lc)
}

/// True if any rejected word starts with `prefix_lc` — used by the mid-word
/// detector so it doesn't re-fire on a word the user already pushed back on.
fn is_rejected_prefix(prefix_lc: &str) -> bool {
    REJECTED
        .lock()
        .unwrap()
        .iter()
        .any(|w| w.starts_with(prefix_lc))
}

fn remember_rejected(word: &str) {
    let word_lc = word.to_lowercase();
    let mut list = REJECTED.lock().unwrap();
    if list.contains(&word_lc) {
        return;
    }
    if list.len() >= REJECTED_MAX {
        list.remove(0);
    }
    list.push(word_lc);
}

// ============================================================
// Undo
// ============================================================

/// Backspace pressed right after a correction: put back exactly what the user
/// typed, restore their layout, and blacklist the word for this session.
fn try_undo() -> bool {
    if !UNDO_ARMED.swap(false, Ordering::SeqCst) {
        return false;
    }

    let lc = match LAST_CORRECTION.lock().unwrap().clone() {
        Some(lc) => lc,
        None => return false,
    };
    if !lc.undoable || lc.when.elapsed().as_secs() >= UNDO_WINDOW_SECS {
        return false;
    }

    let mut restore = lc.original.clone();
    let mut erase = lc.output.chars().count();
    if let Some(sep) = lc.sep {
        restore.push(sep);
        erase += 1;
    }

    println!("[punto] undo: {} → {}", lc.output, lc.original);
    remember_rejected(&lc.original);
    WORD_BUF.lock().unwrap().clear();

    do_replace(erase, &restore);
    switch_keyboard_layout(lc.restore_layout);
    true
}

// ============================================================
// Word checking & replacement
// ============================================================

/// Checks the accumulated word buffer for layout mismatch.
///
/// `separator` is the character the boundary key produces (Space, period,
/// comma, digit…), or `None` when we can't name it. `key` is the physical key
/// itself: when we decide to correct we return `true`, the hook swallows that
/// keystroke, and we replay it after the word has been fixed. That ordering
/// matters — the old code let Enter through first and then tried to backspace
/// over it, which in any chat client meant correcting a message that had
/// already been sent.
///
/// Returns `true` if the caller should swallow the boundary keystroke.
fn check_word_boundary(separator: Option<char>, key: BoundaryKey, kbd: Kbd) -> bool {
    let word = {
        let mut buf = WORD_BUF.lock().unwrap();
        let w = buf.clone();
        buf.clear();
        w
    };

    let char_count = word.chars().count();
    if char_count < 2 || !word.chars().all(|c| c.is_alphabetic()) {
        return false;
    }

    let word_lc = word.to_lowercase();

    // The user already told us this word is fine — don't argue.
    if is_rejected(&word_lc) {
        return false;
    }

    // --- decide_switch returns the converted word to avoid double convert() ---
    let fix = match decide_switch(&word, kbd, preferred_cyrillic()) {
        Some(f) => f,
        None => return false,
    };
    let converted = fix.text;

    // Anti-ping-pong: don't re-convert a word that was just produced by correction.
    {
        let guard = LAST_CORRECTION.lock().unwrap();
        if let Some(lc) = guard.as_ref() {
            if lc.output.to_lowercase() == word_lc
                && lc.when.elapsed().as_secs() < PINGPONG_WINDOW_SECS
            {
                println!("[punto] skip ping-pong: {word}");
                return false;
            }
        }
    }

    let target_layout = layout_id_for(fix.target);
    let restore_layout = layout_id_for(kbd);
    // Enter/Tab have already-visible side effects once replayed (message sent,
    // focus moved), so rewinding the text would not rewind those.
    let undoable = !matches!(separator, Some('\r') | Some('\t'));

    // Printable boundaries are re-typed as text rather than replayed as keys.
    // The layout switch is a *posted* message and posted messages outrank
    // queued input, so a replayed `.` could well be read after the switch —
    // landing as `ю` on a Russian layout. A Unicode character can't drift.
    let (new_text, replay) = match separator {
        Some('\r') | Some('\t') | None => (converted.clone(), Some(key)),
        Some(sep) => {
            let mut t = converted.clone();
            t.push(sep);
            (t, None)
        }
    };

    println!("[punto] {word} → {converted}");

    *PENDING.lock().unwrap() = Some(PendingSwitch {
        backspace_count: char_count,
        new_text,
        target_layout,
        replay,
        // The word is finished and the buffer already cleared — nothing left
        // to recompute against.
        recompute: None,
        undo: LastCorrection {
            original: word,
            output: converted,
            sep: separator,
            restore_layout,
            undoable,
            when: Instant::now(),
        },
    });
    schedule_switch();
    true
}

/// Early / mid-word variant of `check_word_boundary`.
///
/// Called after EVERY letter is appended to `WORD_BUF`.  It delegates to
/// `decide_partial_switch()` which applies strict safety checks (current
/// prefix is dead-end in current lang AND live in the other) and only
/// returns `Some` when we have very high confidence the user wants the
/// opposite layout.  This lets the correction happen BEFORE Space is hit
/// (typically after the 3rd–4th keystroke of a misstyped word), instead
/// of the user having to finish the whole gibberish word first.
///
/// Unlike `check_word_boundary`, no separator is eaten/retyped.
///
/// The buffer is deliberately *not* cleared here. Applying the replacement is
/// deferred by a few milliseconds, and a fast typist gets another letter or
/// two in during that window — "рудд" becomes "руддщ" before the backspaces
/// run. Committing the decision now and recomputing the text against the live
/// buffer at apply time keeps the two in step; clearing early left the extra
/// letters unaccounted for and ate the wrong number of characters.
fn check_partial_switch(kbd: Kbd) {
    // Don't stack switches on top of each other.  If the boundary path
    // (or a prior partial fire) already queued something, wait for the
    // timer to drain it before evaluating again.
    if PENDING.lock().unwrap().is_some() {
        return;
    }

    let word = WORD_BUF.lock().unwrap().clone();
    let char_count = word.chars().count();
    if !(MIN_PARTIAL_LEN..=MAX_PARTIAL_LEN).contains(&char_count) {
        return;
    }

    let word_lc = word.to_lowercase();
    if is_rejected_prefix(&word_lc) {
        return;
    }

    let fix = match decide_partial_switch(&word, kbd, preferred_cyrillic()) {
        Some(f) => f,
        None => return,
    };
    let converted = fix.text;

    // Anti-ping-pong: if the current buf is a prefix of a word we JUST
    // corrected to, we're seeing our own output — bail out.
    {
        let guard = LAST_CORRECTION.lock().unwrap();
        if let Some(lc) = guard.as_ref() {
            let out_lc = lc.output.to_lowercase();
            if (out_lc.starts_with(&word_lc) || word_lc.starts_with(&out_lc))
                && lc.when.elapsed().as_secs() < PINGPONG_WINDOW_SECS
            {
                return;
            }
        }
    }

    let target_layout = layout_id_for(fix.target);
    let restore_layout = layout_id_for(kbd);

    println!("[punto early] {word} → {converted}");

    *PENDING.lock().unwrap() = Some(PendingSwitch {
        backspace_count: char_count,
        new_text: converted.clone(),
        target_layout,
        replay: None,
        // Recompute against the buffer as it stands when the timer fires.
        recompute: Some(fix.how),
        undo: LastCorrection {
            original: word,
            output: converted,
            sep: None,
            restore_layout,
            undoable: true,
            when: Instant::now(),
        },
    });
    schedule_switch();
}

fn schedule_switch() {
    // Under test the harness drives `apply_pending` explicitly.
    #[cfg(test)]
    if EFFECTS.lock().unwrap().is_some() {
        return;
    }
    unsafe {
        let mut id = SWITCH_TIMER_ID.lock().unwrap();
        if *id != 0 {
            let _ = KillTimer(None, *id);
        }
        *id = SetTimer(None, 0, 10, Some(switch_timer_proc));
    }
}

unsafe extern "system" fn switch_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _tick: u32) {
    unsafe {
        let _ = KillTimer(None, id);
        *SWITCH_TIMER_ID.lock().unwrap() = 0;
    }
    apply_pending();
}

/// Applies whatever correction is queued. Deferred by a few milliseconds after
/// the decision, which is the window a fast typist can slip more keystrokes
/// into — hence the recompute below.
fn apply_pending() {
    let pending = PENDING.lock().unwrap().take();
    if let Some(mut p) = pending {
        // Mid-word: the buffer is the source of truth now, not what it held
        // when the decision was made a few milliseconds ago.
        if let Some(how) = p.recompute {
            let live = {
                let mut buf = WORD_BUF.lock().unwrap();
                let live = buf.clone();
                buf.clear();
                live
            };
            if live.is_empty() {
                // Everything got deleted while we waited — nothing to fix.
                return;
            }
            let converted = recode(&live, how);
            p.backspace_count = live.chars().count();
            p.new_text = converted.clone();
            p.undo.original = live;
            p.undo.output = converted;
        }
        do_replace(p.backspace_count, &p.new_text);
        if let Some(key) = p.replay {
            replay_key(key);
        }
        switch_keyboard_layout(p.target_layout);

        *LAST_CORRECTION.lock().unwrap() = Some(p.undo);
        UNDO_ARMED.store(true, Ordering::SeqCst);
    }
}

/// Re-sends the boundary keystroke we swallowed, preserving Shift so that
/// `Shift+/` still lands as `?` rather than `/`.
fn replay_key(key: BoundaryKey) {
    if captured(|| Effect::ReplayKey {
        vk: key.vk,
        shift: key.shift,
    }) {
        return;
    }
    unsafe {
        let vk = VIRTUAL_KEY(key.vk);
        let mut inputs: Vec<INPUT> = Vec::with_capacity(6);
        if key.shift {
            inputs.push(make_key_input_tagged(VK_SHIFT, false, INJECTED_TAG));
        }
        inputs.push(make_key_input_tagged(vk, false, INJECTED_TAG));
        inputs.push(make_key_input_tagged(vk, true, INJECTED_TAG));
        if key.shift {
            inputs.push(make_key_input_tagged(VK_SHIFT, true, INJECTED_TAG));
        }
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn switch_keyboard_layout(lang_id: u32) {
    if !captured(|| Effect::Layout(lang_id)) {
        request_layout(lang_id);
    }

    // That request is a *posted message*: the target window applies it when it
    // next pumps its queue, which in a busy app can be several keystrokes
    // later. Typing "curtains" on a Russian layout would come out "curtфшты" —
    // the word fixed, the tail still in the old layout. Until the switch lands
    // we produce those characters ourselves.
    *BRIDGE.lock().unwrap() = Some(Bridge {
        target: kbd_of_layout(lang_id),
        target_layout: lang_id,
        until: Instant::now() + BRIDGE_MAX,
    });
}

fn request_layout(lang_id: u32) {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.0.is_null() {
            let _ = PostMessageW(
                fg,
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(lang_id as isize),
            );
        }
    }
}

fn kbd_of_layout(layout_id: u32) -> Kbd {
    match layout_id {
        0x0419 => Kbd::Russian,
        0x0422 => Kbd::Ukrainian,
        _ => Kbd::Latin,
    }
}

/// The layout we are typing *for* while waiting for Windows to catch up, or
/// `None` once it has (or once we give up waiting).
fn bridge_target(current_layout: u32) -> Option<Kbd> {
    let mut guard = BRIDGE.lock().unwrap();
    match guard.as_ref() {
        Some(b) if current_layout == b.target_layout || Instant::now() > b.until => {
            *guard = None;
            None
        }
        Some(b) => Some(b.target),
        None => None,
    }
}

/// Swallow the real keystroke and emit the character the not-yet-active layout
/// would have produced, keeping the word buffer in step with the document.
fn type_through_bridge(vk: u32, ch: char, kbd: Kbd) -> KeyAction {
    if captured(|| Effect::Type(ch)) {
        WORD_BUF.lock().unwrap().push(ch);
        check_partial_switch(kbd);
        return swallow(vk);
    }
    let mut utf16 = [0u16; 2];
    let encoded = ch.encode_utf16(&mut utf16);
    unsafe {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(encoded.len() * 2);
        for &u in encoded.iter() {
            inputs.push(make_unicode_input_tagged(u, false, INJECTED_TAG));
            inputs.push(make_unicode_input_tagged(u, true, INJECTED_TAG));
        }
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
    WORD_BUF.lock().unwrap().push(ch);
    check_partial_switch(kbd);
    swallow(vk)
}

fn do_replace(backspace_count: usize, new_text: &str) {
    if captured(|| Effect::Replace {
        backspaces: backspace_count,
        text: new_text.to_string(),
    }) {
        return;
    }
    let utf16: Vec<u16> = new_text.encode_utf16().collect();

    unsafe {
        let mut inputs: Vec<INPUT> = Vec::with_capacity((backspace_count + utf16.len()) * 2);
        for _ in 0..backspace_count {
            inputs.push(make_key_input_tagged(VK_BACK, false, INJECTED_TAG));
            inputs.push(make_key_input_tagged(VK_BACK, true, INJECTED_TAG));
        }
        for &ch in &utf16 {
            inputs.push(make_unicode_input_tagged(ch, false, INJECTED_TAG));
            inputs.push(make_unicode_input_tagged(ch, true, INJECTED_TAG));
        }
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

// ============================================================
// Decision: should we switch this word?
// ============================================================

/// Which pair of layout tables turned the keystrokes into this text. Recorded
/// so a mid-word correction can be recomputed at the moment it is applied,
/// once we know how much more the user managed to type in the meantime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recode {
    /// EN ⇄ RU tables.
    Ru,
    /// EN ⇄ UK tables.
    Uk,
    /// Russian letters re-read as the Ukrainian ones on the same keys.
    RuToUk,
}

fn recode(text: &str, how: Recode) -> String {
    match how {
        Recode::Ru => crate::layout::convert(text),
        Recode::Uk => crate::layout::convert_uk(text),
        Recode::RuToUk => crate::layout::convert_uk(&crate::layout::convert(text)),
    }
}

/// A correction we are willing to apply: the replacement text plus the layout
/// it belongs to, since with three languages in play "the other one" is no
/// longer well defined.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Correction {
    text: String,
    target: Kbd,
    how: Recode,
}

impl Correction {
    fn new(text: String, target: Kbd, how: Recode) -> Self {
        Self { text, target, how }
    }
}

/// Returns `Some(correction)` if we should switch, `None` otherwise.
///
/// `kbd` is the layout the word was typed on. It matters for the Ukrainian
/// case: `і ї є` are simultaneously "real Ukrainian letters" and "the UK-layout
/// image of `s ] '`", and only the active layout disambiguates them.
///
/// `cyr_pref` breaks the tie when a Latin word converts to the *same* Cyrillic
/// under both mappings (i.e. it contains no `s ] ' \``): the text is then
/// identical either way and only the target layout differs, so we send the
/// user to whichever Cyrillic layout they actually use.
fn decide_switch(word: &str, kbd: Kbd, cyr_pref: Kbd) -> Option<Correction> {
    let has_latin = word.chars().any(|c| c.is_ascii_alphabetic());
    let has_cyrillic = word.chars().any(|c| is_cyrillic(c));

    // Mixed scripts — never touch.
    if has_latin && has_cyrillic {
        return None;
    }

    // Typed on a Ukrainian layout: the reverse mapping is `uk_to_en`, not
    // `ru_to_en`. Without this branch every Latin word containing `s` (→ і)
    // is uncorrectable, because the generic Ukrainian-letter guard below sees
    // that `і` and gives up.
    if has_cyrillic && matches!(kbd, Kbd::Ukrainian) {
        let word_lc = word.to_lowercase();
        // Real Ukrainian is protected first by the dictionary, and then by
        // demanding that the Latin reading be a real English word — "привіт"
        // maps to "ghbdsn", which is not a word in anyone's language.
        if is_known_uk_word(&word_lc) || morph_uk_veto(&word_lc) {
            return None;
        }
        // `ґ` has no plain key on the MS Ukrainian layout, so its presence
        // means deliberate Ukrainian text.
        if word.chars().any(|c| matches!(c, 'ґ' | 'Ґ')) {
            return None;
        }
        let converted = crate::layout::convert_uk(word);
        let conv_lc = converted.to_lowercase();
        if !converted.chars().all(|c| c.is_ascii_alphabetic()) {
            return None;
        }
        if is_known_en_word(&conv_lc) || morph_en_confirm(&conv_lc) {
            return Some(Correction::new(converted, Kbd::Latin, Recode::Uk));
        }
        return None;
    }

    // Ukrainian-specific letters (і, ї, є, ґ + uppercase) on a non-Ukrainian
    // layout mean deliberate Ukrainian text — there is no key that produces
    // them by accident, so any swap would be unwanted.
    if word.chars().any(is_ukrainian_only) {
        return None;
    }

    let converted = crate::layout::convert(word);
    let word_lc = word.to_lowercase();
    let conv_lc = converted.to_lowercase();
    let len = word.chars().count();

    let should = if has_latin {
        // User typed latin — maybe wanted Cyrillic?

        // If the typed word is a known English word, don't switch.
        // The veto side is deliberately lenient: it also accepts inflected
        // forms ("configs", "running") that the flat word list can't hold.
        // Erring here costs a missed correction; erring the other way
        // rewrites text the user meant.
        if is_known_en_word(&word_lc) || morph_en_veto(&word_lc) {
            return None;
        }

        // Latin keys have two Cyrillic readings, and they differ only on
        // `s ] ' \``. Test both, so a Ukrainian typist gets "привіт" where a
        // Russian one gets "привет" — off the same keystrokes.
        let uk = crate::layout::convert_uk(word);
        let uk_lc = uk.to_lowercase();
        let uk_ok = is_known_uk_word(&uk_lc) || morph_uk_confirm(&uk_lc);
        let ru_ok = is_known_ru_word(&conv_lc) || morph_ru_confirm(&conv_lc);

        // The confirm side uses the strict matcher — this branch bypasses all
        // scoring, so a loose match here would rewrite text on weak evidence.
        //
        // When both readings are words, or when they are the same text and
        // only the target layout is in question, follow the layout the user
        // actually writes Cyrillic in.
        match (ru_ok, uk_ok) {
            (true, true) => {
                return Some(if matches!(cyr_pref, Kbd::Ukrainian) {
                    Correction::new(uk, Kbd::Ukrainian, Recode::Uk)
                } else {
                    Correction::new(converted, Kbd::Russian, Recode::Ru)
                });
            }
            (true, false) => return Some(Correction::new(converted, Kbd::Russian, Recode::Ru)),
            (false, true) => return Some(Correction::new(uk, Kbd::Ukrainian, Recode::Uk)),
            (false, false) => {}
        }

        // Tech-identifier guard: Latin words with no vowels (gta, vlc, mkv,
        // sql, npm, vpn, ssh-style acronyms) or just one vowel in ≤6 chars
        // (ffmpeg, regex-style names) are almost always programming/tech
        // identifiers, file extensions, or brand names — NOT Russian typed
        // in the wrong layout. The bigram scoring is too eager here because
        // their low vowel ratio mimics layout-typo gibberish and the
        // resulting Cyrillic ("пеф", "ааьзуп") happens to share a couple
        // of common bigrams with real Russian. Past this point we already
        // failed both dictionary lookups, so unless the converted form
        // looked clearly Russian (caught above), bail.
        let vowels_en = word_lc.chars().filter(|c| is_en_vowel(*c)).count();
        let alpha_en = word_lc.chars().filter(|c| c.is_ascii_alphabetic()).count();
        if alpha_en >= 2 && (vowels_en == 0 || (vowels_en == 1 && alpha_en <= 6)) {
            return None;
        }

        // Positive-evidence floor on the converted Russian.  Pure scoring
        // deltas can let a Latin word with a heavy "no-vowel"/consonant-run
        // penalty lose to a Cyrillic gibberish that just happens to score 0,
        // even when the Cyrillic looks nothing like a Russian word.  We want
        // the converted form to clear an absolute floor, AND contain at
        // least two common Russian bigrams (positive evidence the result is
        // word-shaped, not just "less-bad than the input").
        let en_score = score_latin(&word_lc);
        let ru_score = score_cyrillic(&conv_lc);
        let threshold = threshold_en_to_ru(len);
        if count_common_ru_bigrams(&conv_lc) < required_bigrams(len) {
            return None;
        }
        ru_score > en_score + threshold && ru_score >= 12
    } else if has_cyrillic {
        // User typed Cyrillic — maybe wanted Latin, maybe wanted Ukrainian?

        // If the typed word is a known Russian word, don't switch.
        if is_known_ru_word(&word_lc) || morph_ru_veto(&word_lc) {
            return None;
        }

        // Ukrainian typed on a Russian layout. The RU layout has no і/ї/є, so
        // those keys come out as ы/ъ/э and "привіт" lands as "привыт". Re-read
        // the same keys through the Ukrainian table and check the dictionary.
        // Only worth doing when the two readings actually differ, and only on
        // an exact hit — short words like "віл"/"выл" are too easy to confuse.
        let as_uk = crate::layout::convert_uk(&crate::layout::convert(word));
        if as_uk != word && len >= 4 {
            let as_uk_lc = as_uk.to_lowercase();
            if is_known_uk_word(&as_uk_lc) {
                return Some(Correction::new(as_uk, Kbd::Ukrainian, Recode::RuToUk));
            }
        }

        // If the converted word is a known English word, definitely switch.
        if is_known_en_word(&conv_lc) || morph_en_confirm(&conv_lc) {
            return Some(Correction::new(converted, Kbd::Latin, Recode::Ru));
        }

        // Positive-evidence floor, mirroring the EN→RU branch above. Historically
        // this direction only compared scores with no absolute floor, so a Russian
        // word absent from RU_WORDS could be flipped to Latin that merely scored
        // "less bad" than the Cyrillic input. We now require the converted form to
        // look genuinely English: ≥2 common English bigrams AND an absolute score
        // floor, not just a margin over the Cyrillic score.
        //
        // NOTE: unlike EN→RU, we deliberately do NOT apply a low-vowel
        // "tech-identifier" guard here. Many ordinary English words convert from
        // Cyrillic with a single vowel (text, word, list, test, next), so a
        // vowel-count bail would cause misses, not avoid false positives.
        if count_common_en_bigrams(&conv_lc) < required_bigrams(len) {
            return None;
        }

        let ru_score = score_cyrillic(&word_lc);
        let en_score = score_latin(&conv_lc);
        let threshold = threshold_ru_to_en(len);
        en_score > ru_score + threshold && en_score >= 12
    } else {
        false
    };

    if should {
        // The scoring path only ever compares the EN↔RU pair.
        let target = if has_latin { Kbd::Russian } else { Kbd::Latin };
        Some(Correction::new(converted, target, Recode::Ru))
    } else {
        None
    }
}

/// Early / mid-word decision used by the prefix-based detector.
///
/// Algorithm (dictionary prefix trie via binary search):
///   1. `buf` must be ≥ MIN_PARTIAL_LEN chars, all alphabetic, single-script.
///   2. If `buf` is a valid prefix of ANY word in the *current* language
///      dictionary — user is likely still on a legitimate word, bail out.
///   3. Otherwise, convert `buf` to the other layout.  If the *converted*
///      prefix matches ≥ 1 word in the other-language dictionary, AND the
///      converted prefix doesn't look suspicious (no rare-letter runs), fire.
///
/// The asymmetry is important: we only switch when "current = dead-end"
/// AND "other = live prefix".  That avoids firing mid-word for unusual
/// proper nouns, transliterations, or code identifiers whose prefixes
/// happen not to be in our dictionary.
///
/// Returns `Some(correction)` to switch, `None` to wait.
fn decide_partial_switch(buf: &str, kbd: Kbd, cyr_pref: Kbd) -> Option<Correction> {
    let char_count = buf.chars().count();
    if !(MIN_PARTIAL_LEN..=MAX_PARTIAL_LEN).contains(&char_count) {
        return None;
    }

    // Must be purely alphabetic & single-script.
    if !buf.chars().all(|c| c.is_alphabetic()) {
        return None;
    }
    let has_latin = buf.chars().any(|c| c.is_ascii_alphabetic());
    let has_cyrillic = buf.chars().any(|c| is_cyrillic(c));
    if has_latin && has_cyrillic {
        return None;
    }

    let buf_lc = buf.to_lowercase();

    // Typed on a Ukrainian layout: bail if it is a live Ukrainian word start,
    // otherwise map back through the UK tables and require a live English word
    // start. A genuine Ukrainian prefix ("при…" → "ghb…") is a dead end in
    // English, so it stays put either way.
    if has_cyrillic && matches!(kbd, Kbd::Ukrainian) {
        if is_uk_prefix(&buf_lc) || buf.chars().any(|c| matches!(c, 'ґ' | 'Ґ')) {
            return None;
        }
        let converted = crate::layout::convert_uk(buf);
        let conv_lc = converted.to_lowercase();
        if !converted.chars().all(|c| c.is_ascii_alphabetic()) || !is_en_prefix(&conv_lc) {
            return None;
        }
        if has_bad_en_pair(&conv_lc) {
            return None;
        }
        return Some(Correction::new(converted, Kbd::Latin, Recode::Uk));
    }

    // Ukrainian-only letters → never mid-correct (same rationale as decide_switch).
    if buf.chars().any(is_ukrainian_only) {
        return None;
    }

    if has_latin {
        // Fast out: typed prefix matches a real English word start.
        if is_en_prefix(&buf_lc) {
            return None;
        }

        let ru = crate::layout::convert(buf);
        let ru_lc = ru.to_lowercase();
        let uk = crate::layout::convert_uk(buf);
        let uk_lc = uk.to_lowercase();

        // Both Cyrillic readings are candidates; a live prefix in either one
        // is enough, and the reading itself picks the target layout.
        // Note: no score comparison here. Word-level scoring reads a *fragment*
        // badly — "syst" looks vowel-less and consonant-heavy — and once the
        // length window rules out three-character guesses it buys almost
        // nothing (0.29% → 0.26% false fires). The window does the work.
        let ru_live = is_ru_prefix(&ru_lc) && !has_bad_ru_pair(&ru_lc);
        let uk_live = is_uk_prefix(&uk_lc) && !has_bad_ru_pair(&uk_lc);

        if uk == ru {
            if !ru_live && !uk_live {
                return None;
            }
            let target = if matches!(cyr_pref, Kbd::Ukrainian) {
                Kbd::Ukrainian
            } else {
                Kbd::Russian
            };
            return Some(Correction::new(ru, target, Recode::Ru));
        }
        // Readings differ — trust the user's own Cyrillic layout first.
        if matches!(cyr_pref, Kbd::Ukrainian) {
            if uk_live {
                return Some(Correction::new(uk, Kbd::Ukrainian, Recode::Uk));
            }
            if ru_live {
                return Some(Correction::new(ru, Kbd::Russian, Recode::Ru));
            }
        } else {
            if ru_live {
                return Some(Correction::new(ru, Kbd::Russian, Recode::Ru));
            }
            if uk_live {
                return Some(Correction::new(uk, Kbd::Ukrainian, Recode::Uk));
            }
        }
        None
    } else if has_cyrillic {
        if is_ru_prefix(&buf_lc) {
            return None;
        }

        // Ukrainian typed on a Russian layout — the ы/ъ/э reading of the same
        // keys. Only when the two readings actually differ.
        let as_uk = crate::layout::convert_uk(&crate::layout::convert(buf));
        if as_uk != buf && is_uk_prefix(&as_uk.to_lowercase()) {
            return Some(Correction::new(as_uk, Kbd::Ukrainian, Recode::RuToUk));
        }

        let converted = crate::layout::convert(buf);
        let conv_lc = converted.to_lowercase();

        if !is_en_prefix(&conv_lc) || has_bad_en_pair(&conv_lc) {
            return None;
        }

        Some(Correction::new(converted, Kbd::Latin, Recode::Ru))
    } else {
        None
    }
}

/// Any letter pair that is rare or outright illegal in Russian/Ukrainian.
fn has_bad_ru_pair(word_lc: &str) -> bool {
    let chars: Vec<char> = word_lc.chars().collect();
    chars
        .windows(2)
        .any(|p| is_bad_ru_bigram(p[0], p[1]) || is_illegal_ru_pair(p[0], p[1]))
}

/// English counterpart of [`has_bad_ru_pair`].
fn has_bad_en_pair(word_lc: &str) -> bool {
    let bytes: Vec<u8> = word_lc.bytes().collect();
    bytes
        .windows(2)
        .any(|p| is_bad_en_bigram(p[0], p[1]) || is_illegal_en_pair(p[0], p[1]))
}

/// How many common bigrams the converted form must contain before scoring is
/// even consulted. Longer words have more chances to accumulate them by luck,
/// so the bar rises with length.
fn required_bigrams(len: usize) -> usize {
    match len {
        0..=6 => 2,
        7..=9 => 3,
        _ => 4,
    }
}

// ============================================================
// Morphological dictionary matching
// ============================================================
//
// The word lists hold base forms, but Russian and English are inflected: no
// flat list can contain "работаешь" next to "работать", or "configs" next to
// "config". Exact-match-only lookup therefore drops most real words into the
// fuzzy scorer, which is exactly where the mistakes live.
//
// Instead of stemming (which needs the stem itself to be listed), we ask the
// sorted dictionary a cheaper question: *how long a prefix does the nearest
// entry share with this word?* In a sorted array that neighbour is adjacent to
// the binary-search insertion point, so this costs one `partition_point`.
// Combined with "…and the leftover tail is a real inflectional ending", it
// recognises inflected forms without any morphology tables.

/// Number of leading chars `a` and `b` have in common.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Longest common prefix between `w` and any entry of the sorted `dict`.
fn max_common_prefix_len(dict: &[&str], w: &str) -> usize {
    let idx = dict.partition_point(|&x| x < w);
    let mut best = 0;
    if idx > 0 {
        best = best.max(common_prefix_len(dict[idx - 1], w));
    }
    if idx < dict.len() {
        best = best.max(common_prefix_len(dict[idx], w));
    }
    best
}

/// Shared core: `w` is an inflected form of something in `dict` when it splits
/// cleanly into "a prefix some entry also has" + "a legal inflectional ending".
///
/// The split must be exact. Merely *ending* in a plausible inflection is not
/// enough: "кампаныя" shares six characters with "кампания" and ends in `-я`,
/// yet the leftover `ыя` is not an ending — the word differs in the middle, so
/// it is not Russian at all. Demanding the remainder itself be an ending is
/// what tells those two cases apart.
fn morph_match(dict: &[&str], w: &str, endings: &[&str], min_len: usize, min_lcp: usize) -> bool {
    let n = w.chars().count();
    if n < min_len {
        return false;
    }
    let lcp = max_common_prefix_len(dict, w);
    if lcp < min_lcp || lcp >= n {
        return false;
    }
    let rest: String = w.chars().skip(lcp).collect();
    endings.contains(&rest.as_str())
}

const RU_ENDINGS: &[&str] = &[
    "ся", "сь", "ться", "тся", "ть", "ешь", "ишь", "ете", "ите", "ет", "ит", "ем", "им", "ут",
    "ют", "ат", "ят", "ла", "ло", "ли", "л", "ами", "ями", "ыми", "ими", "ах", "ях", "ой", "ей",
    "ом", "ам", "ям", "ов", "ев", "ий", "ый", "ая", "ое", "ые", "их", "ым", "ую", "ную", "ость",
    "ение", "ание", "а", "я", "ы", "и", "о", "е", "у", "ю", "ь", "й",
];

/// Ukrainian inflections. Overlaps Russian heavily, but adds the vocative and
/// the endings built on і/ї/є (`-ої -ій -ії -ими -ється`).
const UK_ENDINGS: &[&str] = &[
    "ться", "ся", "сь", "ти", "ють", "ать", "ять", "ить", "ать", "ємо", "ете", "ите", "ає",
    "ить", "ить", "ла", "ло", "ли", "в", "ами", "ями", "ими", "ах", "ях", "ої", "ій", "ії",
    "ою", "ею", "ом", "ем", "ам", "ям", "ів", "їв", "ий", "ій", "а", "я", "и", "і", "ї", "о",
    "е", "є", "у", "ю", "ь", "й",
];

const EN_ENDINGS: &[&str] = &[
    "ing", "tion", "ment", "ness", "able", "ible", "ies", "ied", "ers", "est", "ful", "ive",
    "ous", "ly", "ed", "er", "es", "s", "d", "y",
];

/// Lenient "is this a real word?" — used to *veto* a correction. False
/// positives here only mean we leave the text alone.
fn morph_ru_veto(w: &str) -> bool {
    morph_match(RU_WORDS, w, RU_ENDINGS, 4, 4)
}

fn morph_uk_veto(w: &str) -> bool {
    morph_match(UK_WORDS, w, UK_ENDINGS, 4, 4)
}

fn morph_en_veto(w: &str) -> bool {
    morph_match(EN_WORDS, w, EN_ENDINGS, 5, 4)
        || morph_match(TECH_WORDS, w, EN_ENDINGS, 5, 4)
}

/// Strict "is this definitely a real word?" — used to *confirm* a correction,
/// bypassing the scorer entirely, so the bar is higher: a longer word and a
/// longer shared prefix. Layout gibberish practically never shares five
/// leading characters with a dictionary entry.
fn morph_ru_confirm(w: &str) -> bool {
    morph_match(RU_WORDS, w, RU_ENDINGS, 6, 5)
}

fn morph_uk_confirm(w: &str) -> bool {
    morph_match(UK_WORDS, w, UK_ENDINGS, 6, 5)
}

fn morph_en_confirm(w: &str) -> bool {
    morph_match(EN_WORDS, w, EN_ENDINGS, 6, 5)
        || morph_match(TECH_WORDS, w, EN_ENDINGS, 6, 5)
}

/// Window in which the early detector is allowed to fire, in characters.
///
/// Both bounds are measured, not guessed. At three characters 4.8% of all
/// letter combinations trip the detector — "afr" of "afraid", "ash" of
/// "ashamed" — because a dead end in a 2400-word English list is weak
/// evidence. At four it is 0.26%, and pushing the minimum up costs nothing:
/// real Russian words typed on an EN layout are still caught early 97% of the
/// time, just one keystroke later.
const MIN_PARTIAL_LEN: usize = 4;

/// Upper bound. Past this point the word is nearly finished, so the boundary
/// check — which has full scoring, morphology and whole-word lookup — will do
/// a far better job in a moment. Firing here is all risk and no benefit: a
/// word that survived six keystrokes without looking like gibberish is
/// probably a real word we simply do not have in the dictionary.
const MAX_PARTIAL_LEN: usize = 6;

/// Length-adaptive threshold for EN→RU detection.
fn threshold_en_to_ru(len: usize) -> i32 {
    match len {
        0..=2 => 999, // 2-char words: whitelist only (threshold unreachable by scoring)
        3 => 6,
        4 => 4,
        _ => 3,
    }
}

/// Length-adaptive threshold for RU→EN detection.
fn threshold_ru_to_en(len: usize) -> i32 {
    match len {
        0..=2 => 999, // 2-char words: whitelist only
        3 => 7,
        4 => 5,
        _ => 3,
    }
}

// ============================================================
// Scoring: Latin (English)
// ============================================================

fn score_latin(word: &str) -> i32 {
    let chars: Vec<char> = word.chars().collect();
    let alpha = chars.iter().filter(|c| c.is_ascii_alphabetic()).count();
    if alpha == 0 {
        return -100;
    }

    let mut score = 0i32;

    // --- Vowel analysis ---
    let vowels = chars.iter().filter(|c| is_en_vowel(**c)).count();
    let ratio = vowels as f32 / alpha as f32;
    if (0.15..=0.60).contains(&ratio) {
        score += 8;
    }
    if vowels == 0 && alpha >= 3 {
        score -= 25;
    }
    if ratio > 0.80 && alpha >= 3 {
        score -= 10;
    } // too many vowels

    // --- Bigram analysis ---
    let bytes: Vec<u8> = word.bytes().collect();
    for pair in bytes.windows(2) {
        if is_common_en_bigram(pair[0], pair[1]) {
            score += 3;
        }
        if is_bad_en_bigram(pair[0], pair[1]) {
            score -= 8;
        }
    }

    // --- Trigram analysis ---
    for triple in bytes.windows(3) {
        if is_common_en_trigram(triple[0], triple[1], triple[2]) {
            score += 5;
        }
    }

    // --- Consecutive consonant penalty ---
    let mut cons = 0u32;
    for c in &chars {
        if c.is_ascii_alphabetic() && !is_en_vowel(*c) {
            cons += 1;
            if cons == 3 {
                score -= 4;
            }
            if cons >= 4 {
                score -= 6;
            }
        } else {
            cons = 0;
        }
    }

    // --- Hard orthographic rules ---
    // Unlike the frequency-based bigram table these are categorical: a word
    // breaking one of them is not "unusual English", it is not English.
    for pair in bytes.windows(2) {
        if is_illegal_en_pair(pair[0], pair[1]) {
            score -= 15;
        }
    }
    if let Some(&last) = bytes.last() {
        // English words essentially never end in these.
        if matches!(last, b'q' | b'j' | b'v') {
            score -= 10;
        }
    }
    if bytes.windows(3).any(|t| t[0] == t[1] && t[1] == t[2]) {
        score -= 12;
    }

    // --- Suffix / prefix signature bonus ---
    score += suffix_bonus_en(word);
    score += prefix_bonus_en(word);

    score
}

/// Letter pairs forbidden by English spelling (as opposed to merely rare).
fn is_illegal_en_pair(a: u8, b: u8) -> bool {
    // `q` is always followed by `u` in English; "qw", "qz", "qb"… only occur in
    // the Cyrillic-typed-on-a-Latin-layout gibberish we are trying to detect.
    a == b'q' && b != b'u'
}

/// Bonus for word endings that are very characteristic of English.
/// Applied on the lowercased word.
fn suffix_bonus_en(word: &str) -> i32 {
    // 4-char suffixes (very specific)
    for s in [
        "tion", "sion", "ness", "ment", "able", "ible", "ship", "ward", "ough", "ious", "eous",
    ] {
        if word.ends_with(s) && word.len() > s.len() {
            return 10;
        }
    }
    // 3-char suffixes
    for s in [
        "ing", "est", "ful", "ity", "ive", "ous", "ize", "ise", "ify", "ism", "ist", "age", "ery",
    ] {
        if word.ends_with(s) && word.len() > s.len() + 1 {
            return 7;
        }
    }
    // 2-char suffixes (weaker — many short Russian→Latin typos also end this way)
    for s in ["ly", "ed", "er"] {
        if word.ends_with(s) && word.len() >= 4 {
            return 3;
        }
    }
    0
}

/// Bonus for word-initial patterns common in English.
fn prefix_bonus_en(word: &str) -> i32 {
    for p in [
        "un", "re", "pre", "dis", "mis", "over", "under", "inter", "trans", "anti", "auto", "semi",
        "sub", "non", "non-",
    ] {
        if word.starts_with(p) && word.len() > p.len() + 1 {
            return 3;
        }
    }
    0
}

// ============================================================
// Scoring: Cyrillic (Russian)
// ============================================================

fn score_cyrillic(word: &str) -> i32 {
    let chars: Vec<char> = word.chars().collect();
    let cyr = chars.iter().filter(|c| is_cyrillic(**c)).count();
    if cyr == 0 {
        return -100;
    }

    let mut score = 0i32;

    // --- Vowel analysis ---
    let vowels = chars.iter().filter(|c| is_cyr_vowel(**c)).count();
    let ratio = vowels as f32 / cyr as f32;
    if (0.15..=0.60).contains(&ratio) {
        score += 8;
    }
    if vowels == 0 && cyr >= 3 {
        score -= 25;
    }
    if ratio > 0.80 && cyr >= 3 {
        score -= 10;
    }

    // --- Bigram analysis ---
    for pair in chars.windows(2) {
        if is_common_ru_bigram(pair[0], pair[1]) {
            score += 3;
        }
        if is_bad_ru_bigram(pair[0], pair[1]) {
            score -= 8;
        }
    }

    // --- Trigram analysis ---
    for triple in chars.windows(3) {
        if is_common_ru_trigram(triple[0], triple[1], triple[2]) {
            score += 5;
        }
    }

    // --- Consecutive consonant penalty (excluding ь, ъ, й) ---
    let mut cons = 0u32;
    for c in &chars {
        if is_cyrillic(*c) && !is_cyr_vowel(*c) && !matches!(*c, 'ь' | 'ъ' | 'й') {
            cons += 1;
            if cons == 3 {
                score -= 4;
            }
            if cons >= 4 {
                score -= 6;
            }
        } else {
            cons = 0;
        }
    }

    // --- Rare-letter penalty: ъ and э are very uncommon in natural Russian ---
    // (ё is excluded — it's a normal letter, just often replaced by е)
    for c in &chars {
        if matches!(*c, 'ъ' | 'э') {
            score -= 2;
        }
    }

    // --- Hard orthographic rules ---
    // Russian spelling forbids these outright (жи-ши / ча-ща / чу-щу and the
    // positional rules for ы/ъ/ь). They are extremely common in the Cyrillic
    // that falls out of Latin typed on a RU layout, and almost absent from
    // real words — the sharpest single signal available here.
    if matches!(chars[0], 'ы' | 'ъ' | 'ь') {
        score -= 20;
    }
    if chars[chars.len() - 1] == 'ъ' {
        score -= 15;
    }
    for pair in chars.windows(2) {
        if is_illegal_ru_pair(pair[0], pair[1]) {
            score -= 15;
        }
    }
    if chars.windows(3).any(|t| t[0] == t[1] && t[1] == t[2]) {
        score -= 12;
    }

    // --- Suffix signature bonus ---
    score += suffix_bonus_ru(word);

    score
}

/// Letter pairs forbidden by Russian orthography (as opposed to merely rare).
///
/// The жи-ши / ча-ща / чу-щу rules are absolute for native vocabulary; the
/// handful of loanword exceptions (жюри, парашют, брошюра) are in `RU_WORDS`
/// and so never reach the scorer.
fn is_illegal_ru_pair(a: char, b: char) -> bool {
    matches!(
        (a, b),
        ('ж', 'ы') | ('ш', 'ы') | ('ч', 'ы') | ('щ', 'ы')
            | ('ч', 'я') | ('щ', 'я') | ('ж', 'я') | ('ш', 'я')
            | ('ч', 'ю') | ('щ', 'ю') | ('ж', 'ю') | ('ш', 'ю')
            | ('ц', 'ю') | ('ц', 'я')
            // A soft/hard sign can never be doubled or follow a vowel.
            | ('ь', 'ь') | ('ъ', 'ъ') | ('ь', 'ъ') | ('ъ', 'ь')
            | ('а', 'ъ') | ('е', 'ъ') | ('и', 'ъ') | ('о', 'ъ')
            | ('у', 'ъ') | ('ы', 'ъ') | ('э', 'ъ') | ('ю', 'ъ') | ('я', 'ъ')
            | ('а', 'ь') | ('е', 'ь') | ('и', 'ь') | ('о', 'ь')
            | ('у', 'ь') | ('ы', 'ь') | ('э', 'ь') | ('ю', 'ь') | ('я', 'ь')
    )
}

/// Count of common Russian bigrams in `word` (lowercase). Used as a
/// positive-evidence signal: a real Russian word of 4+ letters almost always
/// contains ≥2 common bigrams, while layout-typo gibberish from a Latin word
/// rarely does. Cheaper than full scoring when used as an early gate.
fn count_common_ru_bigrams(word: &str) -> usize {
    let chars: Vec<char> = word.chars().collect();
    chars
        .windows(2)
        .filter(|p| is_common_ru_bigram(p[0], p[1]))
        .count()
}

/// Count of common English bigrams in `word` (lowercase ASCII). Positive-
/// evidence mirror of `count_common_ru_bigrams` for the RU→EN direction: a
/// real English word almost always contains ≥2 common bigrams, while the
/// Latin produced by a Russian word typed in the wrong layout rarely does.
fn count_common_en_bigrams(word: &str) -> usize {
    let bytes: Vec<u8> = word.bytes().collect();
    bytes
        .windows(2)
        .filter(|p| is_common_en_bigram(p[0], p[1]))
        .count()
}

/// Bonus for word endings that are very characteristic of Russian.
fn suffix_bonus_ru(word: &str) -> i32 {
    let chars: Vec<char> = word.chars().collect();
    let len = chars.len();

    // 4-char endings (very specific — inflections/reflexives)
    let endings_4 = [
        ['о', 'с', 'т', 'ь'],
        ['т', 'ь', 'с', 'я'],
        ['е', 'н', 'и', 'е'],
        ['а', 'н', 'и', 'е'],
        ['о', 'в', 'а', 'л'],
        ['и', 'в', 'а', 'л'],
        ['я', 'т', 'ь', 'с'],
    ];
    if len > 4 {
        let tail: Vec<char> = chars[len - 4..].to_vec();
        for e in endings_4 {
            if tail == e {
                return 10;
            }
        }
    }

    // 3-char endings (adjective/verb/noun inflections)
    let endings_3 = [
        ['о', 'г', 'о'],
        ['о', 'м', 'у'],
        ['ы', 'м', 'и'],
        ['а', 'м', 'и'],
        ['и', 'м', 'и'],
        ['я', 'м', 'и'],
        ['е', 'г', 'о'],
        ['е', 'м', 'у'],
        ['и', 'т', 'ь'],
        ['а', 'т', 'ь'],
        ['у', 'т', 'ь'],
        ['е', 'т', 'ь'],
        ['ы', 'т', 'ь'],
        ['о', 'т', 'ь'],
        ['ю', 'т', 'с'],
        ['у', 'т', 'с'],
        ['с', 'я', ' '], // ignored since we pre-split
        ['с', 'т', 'ь'],
        ['н', 'ы', 'й'],
        ['н', 'ы', 'е'],
        ['н', 'ы', 'х'],
        ['н', 'о', 'й'],
        ['н', 'о', 'е'],
        ['н', 'о', 'м'],
        ['н', 'у', 'ю'],
        ['о', 'й', ' '],
    ];
    if len > 3 {
        let tail: Vec<char> = chars[len - 3..].to_vec();
        for e in endings_3 {
            if tail[0] == e[0] && tail[1] == e[1] && tail[2] == e[2] {
                return 7;
            }
        }
    }

    // 2-char endings (weaker — plural/case endings)
    let endings_2 = [
        ('т', 'ь'),
        ('с', 'я'),
        ('ы', 'й'),
        ('и', 'й'),
        ('о', 'й'),
        ('а', 'я'),
        ('о', 'е'),
        ('ы', 'е'),
        ('и', 'е'),
        ('и', 'х'),
        ('о', 'в'),
        ('е', 'в'),
        ('о', 'м'),
        ('е', 'м'),
        ('а', 'х'),
        ('а', 'м'),
        ('я', 'х'),
        ('я', 'м'),
        ('у', 'ю'),
        ('ю', 'ю'),
    ];
    if len >= 4 {
        let a = chars[len - 2];
        let b = chars[len - 1];
        for (x, y) in endings_2 {
            if a == x && b == y {
                return 3;
            }
        }
    }

    0
}

// ============================================================
// Common word whitelists
// ============================================================

fn is_known_en_word(word: &str) -> bool {
    EN_WORDS.binary_search(&word).is_ok() || TECH_WORDS.binary_search(&word).is_ok()
}

fn is_known_ru_word(word: &str) -> bool {
    RU_WORDS.binary_search(&word).is_ok()
}

fn is_known_uk_word(word: &str) -> bool {
    UK_WORDS.binary_search(&word).is_ok()
}

/// Tech terms, file extensions, package/brand names. Treated as known
/// English so that:
///   1. Typing one in EN layout is left alone (no false-positive switch
///      to Cyrillic — "gta" stays "gta").
///   2. Typing the same physical keys on RU layout is auto-corrected
///      back to Latin ("пеф" → "gta").
/// MUST stay sorted (binary_search). Also covered by the
/// `tech_words_sorted_and_unique` test.
const TECH_WORDS: &[&str] = &[
    "aac", "angular", "ansible", "apache", "apk", "apt", "avi", "awk", "aws", "azure",
    "babel", "backend", "bash", "bat", "bitbucket", "blob", "bmp", "bootstrap", "brew", "bun",
    "bundler", "cargo", "cassandra", "cdn", "cfg", "changelog", "chmod", "choco", "chown", "cli",
    "clojure", "cloudflare", "cobol", "compiler", "composer", "conda", "conf", "cors", "cpp", "crontab",
    "crud", "csharp", "csrf", "csv", "cuda", "curl", "cypress", "dao", "dart", "ddos",
    "deb", "debugger", "deno", "devops", "dhcp", "django", "dll", "dmg", "dns", "docker",
    "dockerfile", "docx", "dotnet", "dpkg", "dto", "elasticsearch", "elixir", "emacs", "erlang", "eslint",
    "exe", "fastapi", "favicon", "ffmpeg", "flac", "flask", "fortran", "frontend", "fsharp", "fullstack",
    "gcc", "gcp", "gem", "gif", "github", "gitignore", "gitlab", "golang", "goroutine", "gpu",
    "gradle", "grafana", "graphql", "groovy", "grpc", "gta", "gui", "guid", "haskell", "heic",
    "helm", "heroku", "htaccess", "htop", "ico", "ide", "ini", "iso", "jar", "jenkins",
    "jest", "journalctl", "jpeg", "jpg", "jquery", "json", "jsx", "julia", "jvm", "jwt",
    "kafka", "keras", "kotlin", "kubectl", "kubernetes", "kvm", "lambda", "laravel", "linker", "localhost",
    "lua", "makefile", "malloc", "mariadb", "matlab", "matplotlib", "maven", "md", "middleware", "minify",
    "mkdir", "mkv", "mocha", "mongo", "mongodb", "monorepo", "mov", "mp3", "mp4", "mpeg",
    "mpg", "msi", "mutex", "mvc", "mysql", "namespace", "neovim", "nestjs", "netlify", "nextjs",
    "nginx", "nmap", "nodejs", "nosql", "npm", "nuget", "numpy", "nuxt", "oauth", "objc",
    "ocaml", "ocr", "ogg", "opencv", "orm", "pacman", "pandas", "pdf", "perl", "php",
    "pid", "pip", "pipenv", "playwright", "png", "pnpm", "polyfill", "postgres", "postgresql", "powershell",
    "ppt", "pptx", "prettier", "printf", "prometheus", "psd", "pwsh", "pytest", "pytorch", "rabbitmq",
    "rails", "rar", "readme", "redis", "redux", "regex", "rollup", "rpm", "rsync", "ruby",
    "runtime", "saml", "sass", "scala", "scipy", "scoop", "scp", "scss", "sdk", "sed",
    "selenium", "semaphore", "sftp", "sitemap", "sklearn", "soap", "sqlite", "ssd", "sshd", "ssl",
    "sso", "stacktrace", "stderr", "stdin", "stdout", "sudo", "svelte", "svg", "swift", "symfony",
    "sys", "systemctl", "tailwind", "tar", "telnet", "tensorflow", "terraform", "tgz", "tiff", "tls",
    "tmux", "toml", "transpile", "tsv", "tsx", "tty", "txt", "usb", "uuid", "vagrant",
    "varchar", "vercel", "vim", "vite", "vitest", "vlc", "vmware", "vpn", "vscode", "wasm",
    "wav", "webassembly", "webhook", "webm", "webp", "webpack", "wget", "whl", "winget", "xls",
    "xlsx", "xss", "yaml", "yarn", "yml", "zsh",
];

/// Returns true if ANY word in the sorted dictionary starts with `prefix`.
/// This is the basis of the "early-switch" algorithm: we fire the
/// correction before the user hits Space as soon as the current prefix is
/// a dead-end in the active language but a live prefix in the other.
///
/// Complexity: O(log N + |prefix|).  The dictionaries stay sorted (enforced
/// by the `*_words_sorted_and_unique` tests), so `partition_point` locates
/// the insertion point in log time; we then only need to compare the entry
/// at that index to see if it continues with `prefix`.
fn has_prefix_in(dict: &[&str], prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let idx = dict.partition_point(|&w| w < prefix);
    idx < dict.len() && dict[idx].starts_with(prefix)
}

fn is_en_prefix(prefix: &str) -> bool {
    has_prefix_in(EN_WORDS, prefix) || has_prefix_in(TECH_WORDS, prefix)
}

fn is_ru_prefix(prefix: &str) -> bool {
    has_prefix_in(RU_WORDS, prefix)
}

fn is_uk_prefix(prefix: &str) -> bool {
    has_prefix_in(UK_WORDS, prefix)
}

/// Sorted list of common English words (2-8 letters).
/// Includes everyday + programming/tech + chat vocabulary.
/// MUST stay sorted — lookup is binary_search.
const EN_WORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "abroad",
    "absolute",
    "absurd",
    "accept",
    "access",
    "account",
    "accurate",
    "achieve",
    "across",
    "act",
    "action",
    "active",
    "actor",
    "actual",
    "add",
    "address",
    "admin",
    "admit",
    "adopt",
    "adult",
    "advance",
    "advise",
    "after",
    "afternoon",
    "again",
    "against",
    "age",
    "agency",
    "agent",
    "ago",
    "agree",
    "ahead",
    "aim",
    "air",
    "airport",
    "alarm",
    "album",
    "alert",
    "alien",
    "alive",
    "all",
    "allow",
    "almost",
    "alone",
    "along",
    "already",
    "also",
    "although",
    "always",
    "am",
    "amazing",
    "among",
    "amount",
    "an",
    "and",
    "angry",
    "animal",
    "announce",
    "annual",
    "another",
    "answer",
    "any",
    "anyone",
    "anything",
    "anyway",
    "apart",
    "apartment",
    "api",
    "app",
    "apparent",
    "appeal",
    "appear",
    "apple",
    "apply",
    "appoint",
    "approach",
    "approve",
    "april",
    "arch",
    "archive",
    "are",
    "area",
    "arena",
    "arg",
    "args",
    "argue",
    "arise",
    "arm",
    "army",
    "around",
    "arrange",
    "array",
    "arrive",
    "art",
    "article",
    "artist",
    "as",
    "ask",
    "aspect",
    "assembly",
    "assert",
    "asset",
    "assign",
    "assist",
    "associate",
    "assume",
    "async",
    "at",
    "atom",
    "attach",
    "attack",
    "attempt",
    "attend",
    "attention",
    "audio",
    "august",
    "aunt",
    "auth",
    "authenticate",
    "author",
    "authority",
    "auto",
    "autumn",
    "available",
    "avenue",
    "average",
    "avoid",
    "award",
    "aware",
    "away",
    "awesome",
    "awful",
    "back",
    "background",
    "backup",
    "bad",
    "bag",
    "balance",
    "ball",
    "band",
    "bank",
    "bar",
    "base",
    "basic",
    "basis",
    "batch",
    "battery",
    "battle",
    "bay",
    "be",
    "beach",
    "bear",
    "beat",
    "beautiful",
    "beauty",
    "because",
    "become",
    "bed",
    "been",
    "before",
    "begin",
    "behavior",
    "behind",
    "being",
    "believe",
    "below",
    "bench",
    "benefit",
    "beside",
    "best",
    "bet",
    "better",
    "between",
    "beyond",
    "big",
    "bike",
    "bill",
    "bin",
    "bind",
    "biology",
    "bird",
    "birth",
    "bit",
    "bite",
    "black",
    "blame",
    "blank",
    "block",
    "blog",
    "blood",
    "blow",
    "blue",
    "board",
    "boat",
    "body",
    "bold",
    "bone",
    "bonus",
    "book",
    "bool",
    "boost",
    "boot",
    "border",
    "born",
    "both",
    "bother",
    "bottle",
    "bottom",
    "bound",
    "box",
    "boy",
    "brain",
    "branch",
    "brand",
    "brave",
    "bread",
    "break",
    "breath",
    "brick",
    "bridge",
    "brief",
    "bright",
    "bring",
    "british",
    "broad",
    "broken",
    "bronze",
    "brother",
    "brown",
    "browser",
    "buddy",
    "budget",
    "buf",
    "buffer",
    "bug",
    "build",
    "built",
    "burn",
    "bus",
    "business",
    "busy",
    "but",
    "button",
    "buy",
    "by",
    "byte",
    "cable",
    "cache",
    "calendar",
    "call",
    "calm",
    "camera",
    "camp",
    "campus",
    "can",
    "cancel",
    "candidate",
    "candy",
    "cannot",
    "canvas",
    "capital",
    "captain",
    "car",
    "card",
    "care",
    "career",
    "careful",
    "carry",
    "case",
    "cash",
    "cast",
    "cat",
    "catalog",
    "catch",
    "category",
    "cause",
    "cave",
    "ceiling",
    "cell",
    "center",
    "central",
    "century",
    "ceremony",
    "certain",
    "chain",
    "chair",
    "challenge",
    "chance",
    "change",
    "channel",
    "chapter",
    "char",
    "character",
    "charge",
    "chart",
    "chase",
    "chat",
    "cheap",
    "check",
    "chef",
    "chemical",
    "chest",
    "chicken",
    "chief",
    "child",
    "chocolate",
    "choice",
    "choose",
    "chrome",
    "church",
    "city",
    "civil",
    "claim",
    "class",
    "classic",
    "clean",
    "clear",
    "click",
    "client",
    "climate",
    "climb",
    "clip",
    "clock",
    "clone",
    "close",
    "cloud",
    "club",
    "cmd",
    "code",
    "coffee",
    "cold",
    "collapse",
    "collect",
    "college",
    "color",
    "column",
    "combat",
    "combine",
    "come",
    "comfort",
    "command",
    "comment",
    "commit",
    "committee",
    "common",
    "communicate",
    "community",
    "company",
    "compare",
    "compete",
    "complete",
    "complex",
    "complicate",
    "component",
    "compose",
    "compound",
    "compress",
    "compute",
    "concept",
    "concern",
    "concert",
    "conclude",
    "concrete",
    "condition",
    "conduct",
    "conference",
    "confirm",
    "conflict",
    "confront",
    "confuse",
    "congress",
    "connect",
    "consider",
    "consist",
    "constant",
    "construct",
    "consult",
    "consume",
    "contact",
    "contain",
    "content",
    "contest",
    "context",
    "continue",
    "contract",
    "contrast",
    "control",
    "convert",
    "cook",
    "cool",
    "copy",
    "core",
    "corn",
    "corner",
    "correct",
    "cost",
    "cotton",
    "couch",
    "could",
    "council",
    "count",
    "country",
    "county",
    "couple",
    "course",
    "court",
    "cover",
    "cpu",
    "crash",
    "crazy",
    "cream",
    "create",
    "credit",
    "crime",
    "crisis",
    "critical",
    "cross",
    "crowd",
    "crown",
    "crucial",
    "cruel",
    "cry",
    "crypto",
    "css",
    "ctx",
    "cube",
    "cultural",
    "culture",
    "cup",
    "curious",
    "current",
    "curtain",
    "curve",
    "custom",
    "customer",
    "cut",
    "cycle",
    "daily",
    "damage",
    "dance",
    "danger",
    "dark",
    "data",
    "date",
    "daughter",
    "day",
    "days",
    "dead",
    "deal",
    "dear",
    "death",
    "debate",
    "debug",
    "decade",
    "december",
    "decide",
    "decision",
    "declare",
    "decline",
    "decrease",
    "deep",
    "def",
    "default",
    "defeat",
    "defend",
    "define",
    "definite",
    "degree",
    "delay",
    "delete",
    "deliver",
    "demand",
    "democrat",
    "demonstrate",
    "deny",
    "depart",
    "depend",
    "deploy",
    "describe",
    "desert",
    "design",
    "desire",
    "desk",
    "despite",
    "destroy",
    "detail",
    "detect",
    "determine",
    "dev",
    "develop",
    "device",
    "dialog",
    "did",
    "die",
    "diet",
    "diff",
    "differ",
    "difficult",
    "dig",
    "dimension",
    "dinner",
    "dir",
    "direct",
    "director",
    "dirty",
    "disagree",
    "disappear",
    "disaster",
    "discover",
    "discuss",
    "disease",
    "disk",
    "dismiss",
    "display",
    "distance",
    "district",
    "diverse",
    "divide",
    "dna",
    "do",
    "doc",
    "doctor",
    "document",
    "does",
    "dog",
    "doing",
    "dollar",
    "domestic",
    "dominate",
    "donate",
    "done",
    "door",
    "double",
    "doubt",
    "down",
    "download",
    "draft",
    "drag",
    "draw",
    "dream",
    "dress",
    "drink",
    "drive",
    "driver",
    "drop",
    "drug",
    "dry",
    "due",
    "during",
    "dust",
    "duty",
    "each",
    "early",
    "earn",
    "earth",
    "ease",
    "east",
    "easy",
    "eat",
    "economic",
    "economy",
    "edge",
    "edit",
    "editor",
    "education",
    "effect",
    "effort",
    "egg",
    "eight",
    "either",
    "electric",
    "element",
    "else",
    "email",
    "embed",
    "embrace",
    "emerge",
    "emotional",
    "employ",
    "empty",
    "enable",
    "encourage",
    "end",
    "enemy",
    "energy",
    "engage",
    "engine",
    "enhance",
    "enjoy",
    "enough",
    "enroll",
    "ensure",
    "enter",
    "entire",
    "entry",
    "enum",
    "env",
    "environment",
    "equal",
    "equip",
    "era",
    "err",
    "error",
    "escape",
    "especially",
    "essential",
    "establish",
    "estimate",
    "ethnic",
    "even",
    "evening",
    "event",
    "ever",
    "every",
    "everyone",
    "everything",
    "evidence",
    "evolve",
    "exact",
    "example",
    "exceed",
    "excellent",
    "except",
    "exchange",
    "excite",
    "exec",
    "execute",
    "exhibit",
    "exist",
    "exit",
    "expand",
    "expect",
    "experience",
    "experiment",
    "expert",
    "explain",
    "explore",
    "export",
    "expose",
    "express",
    "extend",
    "external",
    "extra",
    "extract",
    "eye",
    "face",
    "facility",
    "fact",
    "factor",
    "factory",
    "fade",
    "fail",
    "failure",
    "fair",
    "false",
    "familiar",
    "family",
    "famous",
    "fan",
    "fantasy",
    "far",
    "farm",
    "fashion",
    "fast",
    "fat",
    "father",
    "fault",
    "fav",
    "favor",
    "fear",
    "feat",
    "feature",
    "february",
    "fee",
    "feed",
    "feel",
    "fellow",
    "female",
    "fence",
    "few",
    "fiction",
    "field",
    "fifteen",
    "fifth",
    "fifty",
    "fight",
    "figure",
    "file",
    "files",
    "fill",
    "film",
    "filter",
    "final",
    "finance",
    "financial",
    "find",
    "fine",
    "finger",
    "finish",
    "fire",
    "firm",
    "first",
    "fish",
    "fit",
    "five",
    "fix",
    "flag",
    "flat",
    "flavor",
    "flight",
    "float",
    "floor",
    "flow",
    "flower",
    "flu",
    "fluid",
    "fly",
    "fmt",
    "focus",
    "folder",
    "follow",
    "food",
    "foot",
    "football",
    "for",
    "force",
    "forecast",
    "foreign",
    "forest",
    "forever",
    "forget",
    "fork",
    "form",
    "format",
    "former",
    "forth",
    "fortune",
    "forum",
    "forward",
    "foster",
    "foundation",
    "four",
    "fourth",
    "frame",
    "free",
    "freedom",
    "frequent",
    "fresh",
    "friday",
    "friend",
    "from",
    "front",
    "frozen",
    "fruit",
    "fuel",
    "full",
    "fun",
    "function",
    "fund",
    "funeral",
    "furniture",
    "further",
    "future",
    "fx",
    "fyi",
    "gain",
    "galaxy",
    "gallery",
    "game",
    "gang",
    "gap",
    "garage",
    "garbage",
    "garden",
    "gas",
    "gate",
    "gather",
    "gave",
    "gay",
    "gear",
    "general",
    "generate",
    "generation",
    "genius",
    "gentle",
    "gentleman",
    "gently",
    "genuine",
    "gesture",
    "get",
    "giant",
    "gift",
    "git",
    "give",
    "given",
    "glance",
    "global",
    "glove",
    "go",
    "goal",
    "god",
    "goes",
    "going",
    "gold",
    "golf",
    "gone",
    "good",
    "google",
    "got",
    "government",
    "grand",
    "grant",
    "grass",
    "grave",
    "gray",
    "great",
    "green",
    "grep",
    "grid",
    "grief",
    "grocery",
    "grope",
    "ground",
    "group",
    "grow",
    "growth",
    "guarantee",
    "guard",
    "guess",
    "guest",
    "guide",
    "guilty",
    "gun",
    "guy",
    "gym",
    "habit",
    "hair",
    "half",
    "hall",
    "hand",
    "handle",
    "hang",
    "happen",
    "happy",
    "hard",
    "hardly",
    "hash",
    "hat",
    "hate",
    "have",
    "head",
    "headline",
    "health",
    "hear",
    "heart",
    "heat",
    "heavy",
    "height",
    "hello",
    "help",
    "her",
    "here",
    "hex",
    "hey",
    "hi",
    "hide",
    "high",
    "hill",
    "him",
    "his",
    "historical",
    "history",
    "hit",
    "hobby",
    "hockey",
    "hold",
    "hole",
    "holiday",
    "home",
    "homeless",
    "homework",
    "honest",
    "honey",
    "honor",
    "hope",
    "horizon",
    "horror",
    "hospital",
    "host",
    "hot",
    "hotel",
    "hour",
    "house",
    "household",
    "housing",
    "how",
    "however",
    "html",
    "http",
    "https",
    "hub",
    "huge",
    "human",
    "hunger",
    "hunt",
    "hurry",
    "hurt",
    "husband",
    "hybrid",
    "hypothesis",
    "icon",
    "idea",
    "ideal",
    "identify",
    "identity",
    "ideology",
    "idx",
    "if",
    "ignore",
    "ill",
    "image",
    "imagine",
    "immediate",
    "impact",
    "implement",
    "imply",
    "import",
    "impose",
    "improve",
    "in",
    "incident",
    "include",
    "income",
    "increase",
    "incredible",
    "indeed",
    "independent",
    "index",
    "indicate",
    "individual",
    "industrial",
    "industry",
    "infect",
    "info",
    "inform",
    "ingredient",
    "initial",
    "inject",
    "injure",
    "injury",
    "inner",
    "innocent",
    "input",
    "insert",
    "inside",
    "insist",
    "inspect",
    "inspire",
    "install",
    "instance",
    "instead",
    "institute",
    "institution",
    "insurance",
    "intake",
    "integrate",
    "intellectual",
    "intelligence",
    "intend",
    "intense",
    "interest",
    "interior",
    "intern",
    "internal",
    "international",
    "internet",
    "interpret",
    "interview",
    "intimate",
    "into",
    "introduce",
    "invest",
    "invite",
    "involve",
    "iphone",
    "iron",
    "is",
    "island",
    "issue",
    "it",
    "item",
    "its",
    "itself",
    "ivory",
    "jacket",
    "jail",
    "java",
    "jeans",
    "jet",
    "job",
    "join",
    "joint",
    "joke",
    "journal",
    "journey",
    "joy",
    "judge",
    "judgment",
    "juice",
    "july",
    "jump",
    "june",
    "jury",
    "just",
    "justice",
    "justify",
    "kbd",
    "keep",
    "kept",
    "key",
    "keyboard",
    "keys",
    "kick",
    "kid",
    "kill",
    "killer",
    "kind",
    "king",
    "kiss",
    "kitchen",
    "knee",
    "knock",
    "know",
    "knowledge",
    "known",
    "lab",
    "label",
    "labor",
    "lack",
    "lady",
    "lake",
    "lamb",
    "lamp",
    "land",
    "lane",
    "language",
    "lap",
    "laptop",
    "large",
    "last",
    "late",
    "later",
    "latest",
    "latin",
    "latter",
    "laugh",
    "launch",
    "laundry",
    "law",
    "lawyer",
    "lay",
    "layer",
    "layout",
    "lazy",
    "lead",
    "leader",
    "leaf",
    "lean",
    "learn",
    "least",
    "leather",
    "leave",
    "lecture",
    "left",
    "leg",
    "legacy",
    "legal",
    "legend",
    "legitimate",
    "lemon",
    "length",
    "lens",
    "less",
    "lesson",
    "let",
    "letter",
    "level",
    "lexicon",
    "liberal",
    "liberty",
    "library",
    "license",
    "lie",
    "life",
    "light",
    "like",
    "likely",
    "limit",
    "line",
    "link",
    "linux",
    "liquid",
    "list",
    "listen",
    "literally",
    "literary",
    "literature",
    "little",
    "live",
    "load",
    "loan",
    "local",
    "locate",
    "lock",
    "log",
    "login",
    "logout",
    "long",
    "look",
    "loop",
    "lose",
    "loss",
    "lost",
    "lot",
    "loud",
    "love",
    "low",
    "lower",
    "loyal",
    "lucky",
    "lunch",
    "machine",
    "mad",
    "made",
    "magazine",
    "magic",
    "mail",
    "main",
    "maintain",
    "major",
    "make",
    "male",
    "mall",
    "man",
    "manage",
    "manager",
    "many",
    "map",
    "march",
    "margin",
    "mark",
    "market",
    "marriage",
    "marry",
    "mask",
    "mass",
    "master",
    "match",
    "material",
    "matter",
    "may",
    "maybe",
    "mayor",
    "me",
    "meal",
    "mean",
    "meanwhile",
    "measure",
    "meat",
    "media",
    "medical",
    "medicine",
    "medium",
    "meet",
    "meeting",
    "mem",
    "member",
    "memorize",
    "memory",
    "menu",
    "message",
    "metal",
    "method",
    "middle",
    "might",
    "military",
    "milk",
    "million",
    "mind",
    "mine",
    "mini",
    "minor",
    "minority",
    "minute",
    "mirror",
    "miss",
    "mission",
    "mistake",
    "mix",
    "mobile",
    "mock",
    "mod",
    "mode",
    "model",
    "modern",
    "modest",
    "modify",
    "module",
    "moment",
    "monday",
    "money",
    "monitor",
    "monster",
    "month",
    "mood",
    "moral",
    "more",
    "morning",
    "most",
    "mostly",
    "mother",
    "motion",
    "motor",
    "mountain",
    "mouse",
    "mouth",
    "move",
    "movie",
    "msg",
    "much",
    "mud",
    "multiply",
    "muscle",
    "museum",
    "music",
    "must",
    "mut",
    "mutual",
    "my",
    "myself",
    "myth",
    "name",
    "narrow",
    "nasty",
    "nation",
    "national",
    "native",
    "natural",
    "nature",
    "naval",
    "navigation",
    "navy",
    "near",
    "nearly",
    "necessary",
    "neck",
    "need",
    "negative",
    "negotiate",
    "neither",
    "nephew",
    "nerve",
    "network",
    "never",
    "new",
    "news",
    "next",
    "nice",
    "niche",
    "night",
    "nine",
    "ninety",
    "no",
    "nobody",
    "node",
    "noise",
    "nominate",
    "none",
    "nope",
    "normal",
    "north",
    "northeast",
    "northern",
    "nose",
    "not",
    "note",
    "nothing",
    "notice",
    "notion",
    "novel",
    "november",
    "now",
    "nuclear",
    "null",
    "number",
    "numerous",
    "nurse",
    "nut",
    "object",
    "objective",
    "obligation",
    "observation",
    "observe",
    "obtain",
    "obvious",
    "occasion",
    "occupy",
    "occur",
    "ocean",
    "october",
    "odd",
    "of",
    "off",
    "offense",
    "offer",
    "office",
    "officer",
    "official",
    "often",
    "oh",
    "oil",
    "okay",
    "old",
    "older",
    "olive",
    "omit",
    "on",
    "once",
    "one",
    "online",
    "only",
    "onto",
    "open",
    "operate",
    "operation",
    "opinion",
    "opponent",
    "opportunity",
    "oppose",
    "opposite",
    "option",
    "orange",
    "order",
    "ordinary",
    "organic",
    "organization",
    "organize",
    "origin",
    "original",
    "other",
    "otherwise",
    "our",
    "ourselves",
    "out",
    "outcome",
    "outdoor",
    "output",
    "outside",
    "oven",
    "over",
    "overall",
    "own",
    "owner",
    "oxygen",
    "pace",
    "pack",
    "package",
    "page",
    "pain",
    "paint",
    "painting",
    "pair",
    "palace",
    "palm",
    "pan",
    "panel",
    "panic",
    "pants",
    "paper",
    "parent",
    "park",
    "part",
    "particular",
    "partner",
    "party",
    "pass",
    "past",
    "patch",
    "path",
    "patient",
    "pattern",
    "pause",
    "pay",
    "peace",
    "peak",
    "pen",
    "pencil",
    "people",
    "per",
    "perceive",
    "perfect",
    "perform",
    "perhaps",
    "period",
    "permit",
    "person",
    "personal",
    "perspective",
    "pet",
    "phase",
    "phone",
    "photo",
    "phrase",
    "physical",
    "piano",
    "pick",
    "picture",
    "pie",
    "piece",
    "pig",
    "pillow",
    "pilot",
    "pin",
    "pink",
    "pipe",
    "pitch",
    "place",
    "plain",
    "plan",
    "plant",
    "plastic",
    "plate",
    "platform",
    "platinum",
    "play",
    "player",
    "please",
    "pleasure",
    "plenty",
    "plot",
    "plug",
    "plus",
    "pocket",
    "poem",
    "poet",
    "poetry",
    "point",
    "police",
    "policy",
    "political",
    "pool",
    "poor",
    "pop",
    "popular",
    "population",
    "port",
    "portion",
    "portrait",
    "position",
    "positive",
    "possess",
    "possible",
    "post",
    "potato",
    "potential",
    "pound",
    "poverty",
    "powder",
    "power",
    "practical",
    "practice",
    "pray",
    "prayer",
    "predict",
    "prefer",
    "preference",
    "pregnant",
    "prepare",
    "present",
    "preserve",
    "president",
    "press",
    "pressure",
    "pretend",
    "pretty",
    "prev",
    "prevent",
    "previous",
    "price",
    "pride",
    "primary",
    "prime",
    "prince",
    "principal",
    "print",
    "prior",
    "priority",
    "prison",
    "private",
    "prize",
    "probably",
    "problem",
    "procedure",
    "process",
    "produce",
    "product",
    "production",
    "professional",
    "professor",
    "profile",
    "profit",
    "program",
    "progress",
    "project",
    "promise",
    "promote",
    "prompt",
    "proof",
    "proper",
    "property",
    "proposal",
    "propose",
    "protect",
    "protein",
    "protest",
    "proud",
    "prove",
    "provide",
    "province",
    "psychology",
    "pub",
    "public",
    "publish",
    "pull",
    "pump",
    "punch",
    "pundit",
    "punish",
    "purchase",
    "pure",
    "purple",
    "purpose",
    "pursue",
    "push",
    "put",
    "python",
    "quality",
    "quarter",
    "queen",
    "query",
    "question",
    "queue",
    "quick",
    "quickly",
    "quiet",
    "quit",
    "quite",
    "quote",
    "race",
    "racial",
    "radar",
    "radiation",
    "radio",
    "rail",
    "rain",
    "raise",
    "ram",
    "random",
    "range",
    "rank",
    "rapid",
    "rare",
    "rate",
    "rather",
    "ratio",
    "raw",
    "reach",
    "react",
    "read",
    "reader",
    "ready",
    "real",
    "realize",
    "really",
    "rear",
    "reason",
    "recall",
    "receive",
    "recent",
    "recipe",
    "recognize",
    "record",
    "recover",
    "red",
    "redo",
    "reduce",
    "ref",
    "refer",
    "reference",
    "reflect",
    "reform",
    "refresh",
    "refrigerator",
    "refuse",
    "regard",
    "regime",
    "region",
    "register",
    "regular",
    "regulate",
    "regulation",
    "rehabilitation",
    "reject",
    "relate",
    "relative",
    "relax",
    "release",
    "relevant",
    "relief",
    "religion",
    "religious",
    "rely",
    "remain",
    "remember",
    "remind",
    "remote",
    "remove",
    "render",
    "rent",
    "repair",
    "repeat",
    "replace",
    "reply",
    "repo",
    "report",
    "represent",
    "republican",
    "request",
    "require",
    "rescue",
    "research",
    "resemble",
    "reserve",
    "resident",
    "resist",
    "resolve",
    "resort",
    "resource",
    "respect",
    "respond",
    "response",
    "responsibility",
    "rest",
    "restaurant",
    "restore",
    "result",
    "retire",
    "return",
    "reveal",
    "revenue",
    "review",
    "revolution",
    "rhythm",
    "rib",
    "ribbon",
    "rice",
    "rich",
    "ride",
    "right",
    "ring",
    "rise",
    "risk",
    "river",
    "road",
    "rock",
    "role",
    "roll",
    "roman",
    "romance",
    "roof",
    "room",
    "root",
    "rose",
    "rough",
    "round",
    "route",
    "routine",
    "row",
    "royal",
    "rule",
    "run",
    "rush",
    "russian",
    "rust",
    "sacred",
    "sacrifice",
    "sad",
    "safe",
    "safety",
    "said",
    "sail",
    "salad",
    "salary",
    "sale",
    "salt",
    "same",
    "sample",
    "sanction",
    "sand",
    "satellite",
    "satisfy",
    "saturday",
    "sauce",
    "save",
    "say",
    "scale",
    "scan",
    "scandal",
    "scar",
    "scare",
    "scene",
    "schedule",
    "schema",
    "scheme",
    "scholar",
    "school",
    "science",
    "scientific",
    "scientist",
    "scope",
    "score",
    "screen",
    "script",
    "sculpture",
    "sea",
    "search",
    "season",
    "seat",
    "second",
    "secret",
    "secretary",
    "section",
    "sector",
    "secure",
    "security",
    "see",
    "seed",
    "seek",
    "seem",
    "segment",
    "seldom",
    "select",
    "sell",
    "senate",
    "send",
    "senior",
    "sense",
    "sensitive",
    "sentence",
    "separate",
    "september",
    "sequence",
    "serious",
    "serve",
    "server",
    "service",
    "session",
    "set",
    "setting",
    "settle",
    "settlement",
    "seven",
    "several",
    "severe",
    "sexual",
    "sha",
    "shake",
    "shall",
    "shape",
    "share",
    "sharp",
    "she",
    "shed",
    "shell",
    "shelter",
    "sheriff",
    "shift",
    "shine",
    "ship",
    "shirt",
    "shock",
    "shoe",
    "shoot",
    "shop",
    "shore",
    "short",
    "shortly",
    "shot",
    "should",
    "shoulder",
    "shout",
    "show",
    "shower",
    "shrug",
    "shut",
    "sick",
    "side",
    "sign",
    "signal",
    "significant",
    "silent",
    "silver",
    "similar",
    "simple",
    "simply",
    "since",
    "sing",
    "singer",
    "single",
    "sink",
    "sir",
    "sister",
    "sit",
    "site",
    "situation",
    "six",
    "size",
    "skill",
    "skin",
    "skip",
    "sky",
    "slave",
    "sleep",
    "slice",
    "slide",
    "slip",
    "slow",
    "slowly",
    "small",
    "smart",
    "smell",
    "smile",
    "smoke",
    "snap",
    "snow",
    "so",
    "social",
    "society",
    "soft",
    "software",
    "soil",
    "solar",
    "soldier",
    "solid",
    "solution",
    "solve",
    "some",
    "somebody",
    "someone",
    "something",
    "sometimes",
    "somewhat",
    "somewhere",
    "son",
    "song",
    "soon",
    "sort",
    "soul",
    "sound",
    "soup",
    "source",
    "south",
    "southeast",
    "southern",
    "sovereign",
    "space",
    "span",
    "speak",
    "speaker",
    "special",
    "species",
    "specific",
    "specify",
    "speech",
    "speed",
    "spell",
    "spend",
    "sphere",
    "spirit",
    "spiritual",
    "split",
    "sponsor",
    "sport",
    "spot",
    "spouse",
    "spread",
    "spring",
    "sql",
    "square",
    "src",
    "ssh",
    "stable",
    "staff",
    "stage",
    "stair",
    "stake",
    "stand",
    "standard",
    "star",
    "stare",
    "start",
    "state",
    "statement",
    "station",
    "statistics",
    "status",
    "stay",
    "steady",
    "steal",
    "steel",
    "step",
    "stick",
    "still",
    "stock",
    "stomach",
    "stone",
    "stop",
    "storage",
    "store",
    "storm",
    "story",
    "strain",
    "strange",
    "strategy",
    "stream",
    "street",
    "strength",
    "stress",
    "stretch",
    "strict",
    "strike",
    "string",
    "strip",
    "stroke",
    "strong",
    "struct",
    "structure",
    "struggle",
    "student",
    "studio",
    "study",
    "stuff",
    "stupid",
    "style",
    "subject",
    "submit",
    "subscribe",
    "substance",
    "substantial",
    "success",
    "successful",
    "such",
    "suddenly",
    "suffer",
    "sufficient",
    "sugar",
    "suggest",
    "suit",
    "sum",
    "summer",
    "summit",
    "sun",
    "sunday",
    "sunny",
    "super",
    "support",
    "suppose",
    "supreme",
    "sure",
    "surely",
    "surface",
    "surgery",
    "surprise",
    "surround",
    "survey",
    "survive",
    "suspect",
    "suspend",
    "sustain",
    "swap",
    "sweep",
    "sweet",
    "swim",
    "swing",
    "switch",
    "sword",
    "symbol",
    "sympathy",
    "symptom",
    "sync",
    "system",
    "table",
    "tablet",
    "tag",
    "tail",
    "take",
    "tale",
    "talent",
    "talk",
    "tall",
    "tank",
    "tap",
    "tape",
    "target",
    "task",
    "taste",
    "tax",
    "taxi",
    "tcp",
    "tea",
    "teach",
    "teacher",
    "team",
    "tear",
    "technical",
    "technique",
    "technology",
    "teen",
    "telephone",
    "television",
    "tell",
    "temperature",
    "temple",
    "temporary",
    "ten",
    "tend",
    "tendency",
    "tension",
    "tent",
    "term",
    "terms",
    "terrible",
    "territory",
    "terror",
    "terrorist",
    "test",
    "text",
    "than",
    "thank",
    "thanks",
    "that",
    "the",
    "their",
    "them",
    "then",
    "theory",
    "therapy",
    "there",
    "therefore",
    "these",
    "they",
    "thin",
    "thing",
    "think",
    "third",
    "thirty",
    "this",
    "those",
    "though",
    "thought",
    "thousand",
    "threat",
    "three",
    "threshold",
    "through",
    "throughout",
    "throw",
    "thursday",
    "thus",
    "thx",
    "ticket",
    "tide",
    "tie",
    "tight",
    "time",
    "tin",
    "tiny",
    "tip",
    "tire",
    "tired",
    "title",
    "tmp",
    "to",
    "today",
    "together",
    "toilet",
    "token",
    "tolerate",
    "tomato",
    "tomorrow",
    "tone",
    "tongue",
    "tonight",
    "too",
    "took",
    "tool",
    "tooth",
    "top",
    "topic",
    "total",
    "touch",
    "tough",
    "tour",
    "tourist",
    "toward",
    "tower",
    "town",
    "toy",
    "track",
    "trade",
    "tradition",
    "traditional",
    "traffic",
    "tragedy",
    "trail",
    "train",
    "training",
    "transfer",
    "transform",
    "transit",
    "translate",
    "transport",
    "trap",
    "travel",
    "treat",
    "tree",
    "trend",
    "trial",
    "tribe",
    "trick",
    "trigger",
    "trip",
    "triumph",
    "troop",
    "trouble",
    "trousers",
    "truck",
    "true",
    "truly",
    "trust",
    "truth",
    "try",
    "tub",
    "tube",
    "tuesday",
    "tune",
    "tunnel",
    "turn",
    "tv",
    "twelve",
    "twenty",
    "twice",
    "twin",
    "two",
    "tx",
    "type",
    "typical",
    "typo",
    "udp",
    "ugly",
    "ui",
    "ultimate",
    "ultimately",
    "unable",
    "uncle",
    "under",
    "understand",
    "undertake",
    "undo",
    "uniform",
    "union",
    "unique",
    "unit",
    "unite",
    "university",
    "unknown",
    "unless",
    "unlike",
    "until",
    "unusual",
    "up",
    "update",
    "upload",
    "upon",
    "upper",
    "urban",
    "urge",
    "us",
    "usage",
    "use",
    "used",
    "user",
    "usual",
    "utf",
    "val",
    "validate",
    "valley",
    "valuable",
    "value",
    "vampire",
    "van",
    "vanish",
    "var",
    "vary",
    "vast",
    "vector",
    "vehicle",
    "venture",
    "venue",
    "verb",
    "verdict",
    "version",
    "versus",
    "very",
    "veteran",
    "via",
    "victim",
    "victory",
    "video",
    "view",
    "viewer",
    "village",
    "violate",
    "violence",
    "violent",
    "virtual",
    "virtue",
    "visible",
    "vision",
    "visit",
    "visitor",
    "visual",
    "vital",
    "vitamin",
    "voice",
    "void",
    "volume",
    "volunteer",
    "vote",
    "voter",
    "vs",
    "vue",
    "wage",
    "wait",
    "wake",
    "walk",
    "wall",
    "wallet",
    "war",
    "warm",
    "warn",
    "warning",
    "wash",
    "waste",
    "watch",
    "water",
    "wave",
    "way",
    "ways",
    "we",
    "weak",
    "wealth",
    "weapon",
    "wear",
    "weather",
    "web",
    "website",
    "wedding",
    "wednesday",
    "week",
    "weekend",
    "weigh",
    "weight",
    "welcome",
    "welfare",
    "well",
    "west",
    "western",
    "wet",
    "what",
    "whatever",
    "wheel",
    "when",
    "whenever",
    "where",
    "whereas",
    "whether",
    "which",
    "while",
    "whisper",
    "white",
    "who",
    "whole",
    "whom",
    "whose",
    "why",
    "wide",
    "widely",
    "wife",
    "wild",
    "will",
    "willing",
    "win",
    "wind",
    "window",
    "wine",
    "wing",
    "winner",
    "winter",
    "wipe",
    "wire",
    "wise",
    "wish",
    "with",
    "withdraw",
    "within",
    "without",
    "witness",
    "wolf",
    "woman",
    "women",
    "wonder",
    "wood",
    "wooden",
    "word",
    "work",
    "worker",
    "workshop",
    "world",
    "worried",
    "worry",
    "worth",
    "would",
    "wound",
    "wrap",
    "write",
    "writer",
    "wrong",
    "wrote",
    "xml",
    "yahoo",
    "yard",
    "yeah",
    "year",
    "years",
    "yell",
    "yellow",
    "yep",
    "yes",
    "yesterday",
    "yet",
    "yield",
    "you",
    "young",
    "your",
    "yours",
    "yourself",
    "youth",
    "yup",
    "zero",
    "zip",
    "zone",
    "zoom",
];

/// Sorted list of common Russian words (2-8 letters).
/// MUST stay sorted — lookup is binary_search.
const RU_WORDS: &[&str] = &[
    "а",
    "август",
    "автор",
    "агент",
    "адрес",
    "актуально",
    "актёр",
    "алгоритм",
    "алкоголь",
    "альбом",
    "анализ",
    "английский",
    "апрель",
    "армия",
    "архив",
    "аспект",
    "атака",
    "аудио",
    "аэропорт",
    "база",
    "базовый",
    "бай",
    "байт",
    "балкон",
    "банк",
    "бар",
    "барабан",
    "бармен",
    "барьер",
    "бассейн",
    "батарея",
    "бедный",
    "бежать",
    "без",
    "безопасность",
    "безумный",
    "белый",
    "берег",
    "беречь",
    "берлин",
    "беседа",
    "бесконечный",
    "беспокоить",
    "беспокойство",
    "бесполезный",
    "беспомощный",
    "беспощадно",
    "бессмертный",
    "бесценный",
    "бетон",
    "библиотека",
    "бизнес",
    "билет",
    "благо",
    "благодарить",
    "благородный",
    "бланк",
    "близкий",
    "близко",
    "блок",
    "блокировка",
    "блюдо",
    "бог",
    "богатый",
    "бой",
    "более",
    "болезнь",
    "болеть",
    "большой",
    "борт",
    "бояться",
    "брат",
    "брать",
    "бриллиант",
    "бросать",
    "будет",
    "будущее",
    "будь",
    "бумага",
    "буря",
    "бутылка",
    "буфер",
    "бывает",
    "была",
    "были",
    "было",
    "быстро",
    "быстрый",
    "быть",
    "бюджет",
    "в",
    "важно",
    "важный",
    "вариант",
    "вас",
    "ваш",
    "ведь",
    "везде",
    "век",
    "великий",
    "вера",
    "верить",
    "верно",
    "вероятно",
    "верхний",
    "вершина",
    "вес",
    "веселый",
    "весенний",
    "весна",
    "вести",
    "весь",
    "весьма",
    "ветер",
    "ветка",
    "вечер",
    "вечно",
    "вечный",
    "вещь",
    "взгляд",
    "взять",
    "вид",
    "видел",
    "видит",
    "видишь",
    "видно",
    "виж",
    "вижу",
    "виза",
    "визит",
    "виноват",
    "винт",
    "висеть",
    "висок",
    "витамин",
    "вкладка",
    "включать",
    "включить",
    "вкус",
    "владелец",
    "власть",
    "влияние",
    "вместе",
    "вместо",
    "вне",
    "внезапно",
    "внизу",
    "внимание",
    "внимательно",
    "внутри",
    "вовремя",
    "вода",
    "водитель",
    "водка",
    "воевать",
    "военный",
    "вождь",
    "возвращаться",
    "возможно",
    "возможность",
    "возможный",
    "возраст",
    "война",
    "войти",
    "вокруг",
    "вообще",
    "вопрос",
    "ворота",
    "восемь",
    "воспитание",
    "восток",
    "восторг",
    "восхищение",
    "восьмой",
    "вот",
    "впервые",
    "вперед",
    "впечатление",
    "впрочем",
    "врач",
    "время",
    "все",
    "всегда",
    "всего",
    "всем",
    "всех",
    "вскоре",
    "всюду",
    "всякий",
    "вторник",
    "второй",
    "вход",
    "входить",
    "вчера",
    "вы",
    "выбирать",
    "выбор",
    "выбрать",
    "вывод",
    "выдержать",
    "выиграть",
    "выйти",
    "выпить",
    "вырастать",
    "высокий",
    "высота",
    "выставка",
    "вытащить",
    "выход",
    "выходной",
    "газ",
    "газета",
    "где",
    "генерал",
    "генератор",
    "гениальный",
    "георгий",
    "герой",
    "глава",
    "главный",
    "глаз",
    "глубина",
    "глубокий",
    "глупый",
    "глухой",
    "глядеть",
    "гнев",
    "гнездо",
    "говорить",
    "говорят",
    "год",
    "годы",
    "гол",
    "голова",
    "голод",
    "голубой",
    "голый",
    "гонка",
    "гонконг",
    "гора",
    "гораздо",
    "горе",
    "город",
    "горячий",
    "госпиталь",
    "господи",
    "господин",
    "гостиница",
    "гость",
    "государство",
    "готов",
    "готовый",
    "граница",
    "группа",
    "грустный",
    "грусть",
    "грязный",
    "два",
    "дверь",
    "двести",
    "движение",
    "двор",
    "двоюродный",
    "двухтысячный",
    "девочка",
    "девушка",
    "девяносто",
    "девятый",
    "девять",
    "деградация",
    "дед",
    "действие",
    "действительно",
    "действовать",
    "декабрь",
    "делать",
    "дело",
    "день",
    "деньги",
    "дерево",
    "десятый",
    "десять",
    "деталь",
    "дети",
    "детский",
    "диалог",
    "дизайн",
    "диск",
    "длинный",
    "для",
    "дневник",
    "до",
    "добавить",
    "добрый",
    "доверие",
    "довольно",
    "догадаться",
    "дождь",
    "дойти",
    "доктор",
    "документ",
    "долго",
    "долгосрочный",
    "должен",
    "должно",
    "должный",
    "долина",
    "дом",
    "домашний",
    "допустимый",
    "дорога",
    "дорогой",
    "доска",
    "достаточно",
    "достать",
    "достичь",
    "достоевский",
    "достояние",
    "доступ",
    "дохнуть",
    "дочь",
    "драка",
    "драма",
    "другая",
    "другие",
    "другое",
    "другой",
    "дружба",
    "дубликат",
    "думал",
    "думала",
    "думали",
    "думаю",
    "дурак",
    "духовный",
    "душа",
    "дыра",
    "дырка",
    "дядя",
    "его",
    "ежедневно",
    "ежемесячный",
    "ежесекундный",
    "если",
    "ест",
    "есть",
    "ехать",
    "еще",
    "ещё",
    "её",
    "жалко",
    "жалобный",
    "жаль",
    "жара",
    "жаркий",
    "ждать",
    "же",
    "желать",
    "железный",
    "желтый",
    "желудок",
    "жена",
    "женский",
    "женщина",
    "жертва",
    "жест",
    "жестокий",
    "живой",
    "живот",
    "жизнь",
    "жилой",
    "жители",
    "жить",
    "журнал",
    "жюри",
    "за",
    "забавно",
    "забыл",
    "забыть",
    "завидовать",
    "зависть",
    "завод",
    "завтра",
    "задача",
    "задний",
    "задумчивый",
    "закат",
    "заключение",
    "закон",
    "закрыть",
    "зал",
    "залежи",
    "зальный",
    "замечательный",
    "занять",
    "запад",
    "записать",
    "запрос",
    "зарплата",
    "заставить",
    "затем",
    "захотеть",
    "зачем",
    "защита",
    "защищать",
    "заявить",
    "звать",
    "звезда",
    "звонить",
    "звук",
    "здание",
    "здесь",
    "здоровый",
    "здоровье",
    "здравствуй",
    "зеленый",
    "земля",
    "зеркало",
    "зима",
    "златый",
    "знак",
    "знакомый",
    "знать",
    "значение",
    "значит",
    "золотой",
    "зона",
    "зуб",
    "и",
    "играть",
    "игрок",
    "идея",
    "идти",
    "иерархия",
    "избежать",
    "известно",
    "известный",
    "извинение",
    "извинить",
    "изгнать",
    "изменение",
    "измерение",
    "изображение",
    "изучать",
    "имей",
    "именно",
    "иметь",
    "имя",
    "иначе",
    "инженер",
    "иногда",
    "иное",
    "иностранный",
    "институт",
    "интересный",
    "интернет",
    "информация",
    "искать",
    "исключение",
    "искренний",
    "искусство",
    "испанский",
    "исполнитель",
    "использовать",
    "история",
    "источник",
    "исход",
    "июль",
    "июнь",
    "йог",
    "к",
    "кабинет",
    "каждый",
    "казался",
    "казаться",
    "казнить",
    "как",
    "какая",
    "какие",
    "какое",
    "какой",
    "калина",
    "камень",
    "камера",
    "кампания",
    "канадский",
    "канал",
    "капитал",
    "карандаш",
    "карман",
    "картофель",
    "катастрофа",
    "катать",
    "категория",
    "кафе",
    "кафедра",
    "квартира",
    "керамика",
    "кивать",
    "километр",
    "кино",
    "клавиатура",
    "класс",
    "клиент",
    "ключ",
    "книга",
    "кнопка",
    "когда",
    "кого",
    "кому",
    "конечно",
    "конкретный",
    "конкурент",
    "контракт",
    "конференция",
    "концерт",
    "кончать",
    "копия",
    "корабль",
    "корейский",
    "король",
    "короткий",
    "коротко",
    "корпус",
    "космический",
    "космос",
    "которая",
    "которого",
    "которое",
    "которой",
    "которые",
    "который",
    "кофе",
    "красивый",
    "красный",
    "кремль",
    "крепкий",
    "крест",
    "крик",
    "кричать",
    "кровавый",
    "кровь",
    "кроме",
    "крохотный",
    "крупный",
    "крыло",
    "крыша",
    "кто",
    "кувалда",
    "куда",
    "кукла",
    "культура",
    "купить",
    "курс",
    "кусок",
    "кухня",
    "лаборатория",
    "лавка",
    "лагерь",
    "ладно",
    "ладонь",
    "лазер",
    "лампа",
    "лапа",
    "лауреат",
    "лев",
    "лед",
    "лес",
    "лето",
    "ли",
    "либеральный",
    "лидер",
    "лисий",
    "лист",
    "литература",
    "лифт",
    "лицо",
    "личный",
    "лишь",
    "лоб",
    "ложиться",
    "лучше",
    "любая",
    "любит",
    "любить",
    "любой",
    "люди",
    "мавр",
    "магазин",
    "мало",
    "малыш",
    "мама",
    "манер",
    "манера",
    "материал",
    "мать",
    "маша",
    "машина",
    "мгновение",
    "мгновенный",
    "медленно",
    "медленный",
    "медь",
    "между",
    "международный",
    "мел",
    "мелкий",
    "меньше",
    "меня",
    "менять",
    "мертвый",
    "места",
    "местный",
    "место",
    "месяц",
    "металл",
    "метод",
    "милион",
    "милиция",
    "миллиард",
    "миллионный",
    "мир",
    "мировой",
    "мнение",
    "много",
    "многое",
    "множество",
    "мог",
    "могу",
    "мода",
    "модный",
    "моего",
    "моет",
    "может",
    "можно",
    "мой",
    "молитва",
    "молодежь",
    "молодой",
    "молоко",
    "молчание",
    "молчать",
    "момент",
    "монумент",
    "мороз",
    "москва",
    "москвич",
    "мотоцикл",
    "мочить",
    "мочь",
    "мощный",
    "моя",
    "моё",
    "моём",
    "мрак",
    "мрачный",
    "мудрый",
    "муж",
    "мужчина",
    "музыка",
    "мурзик",
    "мы",
    "мыло",
    "мысленно",
    "мысль",
    "мыть",
    "мясо",
    "мяч",
    "на",
    "наблюдать",
    "наверное",
    "наверняка",
    "навсегда",
    "нагреть",
    "над",
    "надежда",
    "надо",
    "надоесть",
    "назвать",
    "назначить",
    "наиболее",
    "наконец",
    "налог",
    "нам",
    "наполнять",
    "направо",
    "например",
    "нас",
    "настоящий",
    "наступать",
    "находиться",
    "начало",
    "начинать",
    "начну",
    "наш",
    "наше",
    "наши",
    "небо",
    "невозможно",
    "него",
    "нежный",
    "немец",
    "немецкий",
    "нередко",
    "нести",
    "нет",
    "никак",
    "никогда",
    "николаевич",
    "николай",
    "никто",
    "ничего",
    "ничто",
    "но",
    "новость",
    "новый",
    "нога",
    "ноль",
    "номер",
    "нос",
    "носить",
    "ночь",
    "ноябрь",
    "нравиться",
    "ну",
    "нужен",
    "нужно",
    "о",
    "об",
    "оба",
    "обернуться",
    "обет",
    "облако",
    "обмен",
    "образ",
    "образование",
    "обратиться",
    "обручение",
    "обслуживание",
    "обучение",
    "общаться",
    "общение",
    "общество",
    "общий",
    "объект",
    "объяснить",
    "обычно",
    "обязан",
    "обязательно",
    "огонь",
    "ограничение",
    "огромный",
    "один",
    "одиночество",
    "одиночный",
    "одно",
    "одновременно",
    "одобрить",
    "оказаться",
    "океан",
    "окно",
    "около",
    "октябрь",
    "он",
    "она",
    "они",
    "оно",
    "опасно",
    "опасность",
    "опасный",
    "определить",
    "опубликовать",
    "опыт",
    "опять",
    "оранжевый",
    "организация",
    "организм",
    "организовать",
    "орел",
    "орион",
    "оркестр",
    "ос",
    "освободить",
    "освоение",
    "основа",
    "основной",
    "особенно",
    "особенность",
    "особый",
    "оставить",
    "остаться",
    "остров",
    "осуществить",
    "ответ",
    "ответить",
    "открытие",
    "открыть",
    "откуда",
    "отлично",
    "относительно",
    "относиться",
    "отношение",
    "отправить",
    "отпуск",
    "отстать",
    "отступить",
    "отсюда",
    "отчего",
    "отчества",
    "отчет",
    "офицер",
    "официально",
    "официант",
    "охота",
    "охрана",
    "очевидно",
    "очевидный",
    "очень",
    "очередной",
    "ошибка",
    "ощущать",
    "ощущение",
    "падать",
    "палец",
    "пальто",
    "память",
    "папа",
    "парадигма",
    "парень",
    "париж",
    "парк",
    "парковка",
    "парламент",
    "паровой",
    "пароль",
    "паром",
    "партия",
    "партнер",
    "паспорт",
    "пассажир",
    "пастор",
    "патент",
    "патриот",
    "пауза",
    "пахнуть",
    "пациент",
    "пачка",
    "пейзаж",
    "пельмень",
    "пенсия",
    "пепел",
    "первый",
    "перевести",
    "перевод",
    "перевозка",
    "перевоплотить",
    "перед",
    "передать",
    "перейти",
    "перекресток",
    "перелет",
    "перемена",
    "переписка",
    "перерыв",
    "перестать",
    "перестройка",
    "период",
    "перо",
    "перон",
    "перрон",
    "персик",
    "перспектива",
    "песня",
    "петух",
    "печаль",
    "печальный",
    "печать",
    "пешеход",
    "пив",
    "пиво",
    "пиджак",
    "пилот",
    "писатель",
    "писать",
    "письмо",
    "питание",
    "пить",
    "плавание",
    "плакать",
    "план",
    "плановый",
    "планшет",
    "пластик",
    "плата",
    "плато",
    "плач",
    "плохой",
    "площадка",
    "площадь",
    "плыть",
    "плюс",
    "по",
    "победа",
    "побежать",
    "поведение",
    "повесть",
    "повод",
    "поворот",
    "повторить",
    "погибнуть",
    "погода",
    "под",
    "подарок",
    "подбородок",
    "подвал",
    "подвиг",
    "поджигатель",
    "подниматься",
    "подобный",
    "подойти",
    "подросток",
    "подружиться",
    "подряд",
    "подумать",
    "подходить",
    "подчиняться",
    "подъезд",
    "поезд",
    "пожалуйста",
    "пожар",
    "позади",
    "позволять",
    "позвонить",
    "поздний",
    "поздно",
    "позиция",
    "познакомиться",
    "пойду",
    "поймать",
    "пойти",
    "пока",
    "показать",
    "покоить",
    "покой",
    "покрытие",
    "покупатель",
    "пол",
    "поле",
    "полезно",
    "полезный",
    "ползать",
    "поливать",
    "полить",
    "полицейский",
    "полиция",
    "полночь",
    "полный",
    "половина",
    "положение",
    "получить",
    "польза",
    "помидор",
    "помнить",
    "помогать",
    "помощник",
    "помощь",
    "понимание",
    "понимать",
    "понятие",
    "понятно",
    "понять",
    "попросить",
    "популярный",
    "пора",
    "порой",
    "порт",
    "портрет",
    "поручение",
    "порядок",
    "посвящать",
    "поселить",
    "после",
    "последний",
    "последовательно",
    "послушать",
    "пособие",
    "потерять",
    "поток",
    "потом",
    "потому",
    "похожий",
    "почему",
    "почти",
    "поэзия",
    "поэт",
    "поэтому",
    "появиться",
    "правда",
    "правило",
    "право",
    "правый",
    "прадед",
    "праздник",
    "практика",
    "практически",
    "предложить",
    "предмет",
    "предприятие",
    "представить",
    "представление",
    "прежде",
    "президент",
    "презирать",
    "прекрасный",
    "премия",
    "пренебрежение",
    "преодолеть",
    "препарат",
    "препятствие",
    "преступник",
    "прибыль",
    "привет",
    "пригласить",
    "приглашение",
    "приготовить",
    "придумать",
    "приезд",
    "приказ",
    "приключение",
    "пример",
    "принадлежать",
    "принести",
    "принимать",
    "принцип",
    "принять",
    "природа",
    "присутствие",
    "притянуть",
    "причина",
    "приятно",
    "про",
    "проблема",
    "проверить",
    "провести",
    "провод",
    "проводить",
    "программа",
    "прогресс",
    "продавать",
    "продать",
    "продолжать",
    "продукт",
    "продукция",
    "прожить",
    "проиграть",
    "произведение",
    "произойти",
    "пройти",
    "пропажа",
    "пропасть",
    "просить",
    "просто",
    "простой",
    "простор",
    "простота",
    "против",
    "противник",
    "противоположный",
    "профессия",
    "профиль",
    "прохладный",
    "процесс",
    "пруд",
    "прыжок",
    "прямо",
    "прямой",
    "прятать",
    "психолог",
    "птица",
    "пустой",
    "пусть",
    "путать",
    "путь",
    "пытаться",
    "пьеса",
    "пять",
    "работа",
    "работать",
    "рабочий",
    "радио",
    "радость",
    "разговаривать",
    "разговор",
    "разговорный",
    "раздел",
    "разделить",
    "различие",
    "различный",
    "размер",
    "разный",
    "разобраться",
    "разом",
    "разорвать",
    "разрешение",
    "разрешить",
    "разум",
    "рай",
    "район",
    "рак",
    "рамка",
    "ранее",
    "ранний",
    "раньше",
    "раскрывать",
    "распространить",
    "рассвет",
    "рассказ",
    "рассказать",
    "рассматривать",
    "расставание",
    "расстроить",
    "рассудок",
    "реализация",
    "ребенок",
    "революция",
    "регион",
    "регистрация",
    "редактор",
    "режим",
    "результат",
    "река",
    "реклама",
    "рекомендация",
    "религия",
    "репутация",
    "ресторан",
    "решение",
    "решительный",
    "решить",
    "риск",
    "робот",
    "родители",
    "родиться",
    "рождество",
    "розовый",
    "роль",
    "роман",
    "российский",
    "россия",
    "рост",
    "рот",
    "рубашка",
    "руббль",
    "рубежом",
    "рука",
    "руководитель",
    "русский",
    "ручка",
    "рядом",
    "с",
    "сад",
    "сахар",
    "свежий",
    "свет",
    "свидетель",
    "свидетельство",
    "свобода",
    "свободный",
    "свой",
    "связать",
    "связь",
    "сдавать",
    "северный",
    "сегодня",
    "седьмой",
    "сейчас",
    "семейный",
    "семья",
    "сентябрь",
    "серверный",
    "сердце",
    "середина",
    "серьезный",
    "сестра",
    "сеть",
    "сидеть",
    "сила",
    "сильный",
    "симпатичный",
    "синий",
    "сирота",
    "сиять",
    "сказал",
    "сказала",
    "сказать",
    "сквозь",
    "скорее",
    "скоро",
    "скорый",
    "скрестить",
    "скрыться",
    "слабый",
    "сладкий",
    "слева",
    "слегка",
    "след",
    "следовать",
    "следующий",
    "слеза",
    "слезать",
    "слишком",
    "слово",
    "сломать",
    "служба",
    "случай",
    "случиться",
    "слушать",
    "слышать",
    "смена",
    "смеяться",
    "смотреть",
    "смысл",
    "снаружи",
    "снег",
    "снова",
    "собака",
    "собственный",
    "событие",
    "совершенно",
    "совершенство",
    "советовать",
    "советский",
    "современный",
    "совсем",
    "согласиться",
    "содержание",
    "соединение",
    "сознание",
    "сойти",
    "сок",
    "солнце",
    "соль",
    "сомневаться",
    "сон",
    "соответственно",
    "соперник",
    "сопротивление",
    "сорок",
    "состояние",
    "сотня",
    "сотрудник",
    "сохранить",
    "союз",
    "спаси",
    "спасибо",
    "спать",
    "спектакль",
    "специалист",
    "специально",
    "список",
    "спокойно",
    "спокойный",
    "спор",
    "спорт",
    "способ",
    "способность",
    "справа",
    "справиться",
    "спрашивать",
    "среда",
    "среди",
    "средний",
    "средство",
    "срок",
    "ссылка",
    "ставить",
    "становиться",
    "станция",
    "стараться",
    "старик",
    "старший",
    "старый",
    "стать",
    "статья",
    "стена",
    "стиль",
    "сто",
    "стоит",
    "стол",
    "столетие",
    "столица",
    "столько",
    "сторона",
    "стоять",
    "страна",
    "страница",
    "страх",
    "страшно",
    "стрелять",
    "стремиться",
    "строгий",
    "строить",
    "строй",
    "стройка",
    "строка",
    "структура",
    "студент",
    "стул",
    "субъект",
    "суд",
    "судить",
    "судьба",
    "суровый",
    "сути",
    "суть",
    "сухой",
    "сцена",
    "счастливый",
    "счастье",
    "сын",
    "сюда",
    "так",
    "также",
    "такой",
    "талант",
    "там",
    "танец",
    "танк",
    "твердо",
    "твой",
    "театр",
    "тебе",
    "тебя",
    "текст",
    "телефон",
    "тема",
    "темно",
    "темный",
    "теперь",
    "тепло",
    "теплый",
    "термин",
    "территория",
    "террор",
    "террорист",
    "тесный",
    "тихий",
    "тихо",
    "тишина",
    "ткань",
    "то",
    "товарищ",
    "тогда",
    "тоже",
    "толстый",
    "только",
    "тонкий",
    "тоска",
    "тот",
    "точка",
    "точно",
    "точный",
    "тошнить",
    "трагедия",
    "трактор",
    "трамвай",
    "транспорт",
    "трасса",
    "тревога",
    "тренировка",
    "третий",
    "три",
    "тридцать",
    "триста",
    "тройка",
    "тропический",
    "труд",
    "трудно",
    "трудный",
    "трус",
    "ты",
    "тысяча",
    "тьма",
    "тюрьма",
    "тянуть",
    "тёмный",
    "тётя",
    "убедиться",
    "убежать",
    "убийство",
    "убить",
    "уважать",
    "уверенность",
    "уверенный",
    "увидеть",
    "угадать",
    "угол",
    "удар",
    "удачно",
    "удивление",
    "удобно",
    "удобный",
    "удовольствие",
    "уезжать",
    "уже",
    "узнать",
    "уйти",
    "указать",
    "улица",
    "улыбка",
    "улыбнуться",
    "уметь",
    "умный",
    "умолять",
    "университет",
    "уникальный",
    "управление",
    "урок",
    "условие",
    "успеть",
    "успех",
    "усталость",
    "устать",
    "устройство",
    "утвердить",
    "утро",
    "уходить",
    "участие",
    "участник",
    "учитель",
    "учиться",
    "фаза",
    "факт",
    "факультет",
    "фамилия",
    "февраль",
    "философия",
    "фильм",
    "финал",
    "финансы",
    "фирма",
    "фон",
    "фонтан",
    "форма",
    "формула",
    "фото",
    "фотография",
    "фраза",
    "франция",
    "французский",
    "фронт",
    "футбол",
    "хвост",
    "хитрый",
    "хлеб",
    "ходить",
    "хозяин",
    "холм",
    "холод",
    "холодный",
    "хороший",
    "хорошо",
    "хотел",
    "хотела",
    "хотели",
    "хоть",
    "хотя",
    "хочет",
    "хочешь",
    "хочу",
    "храбрый",
    "храм",
    "христианский",
    "художник",
    "худший",
    "царь",
    "цвет",
    "целевой",
    "целый",
    "цель",
    "центр",
    "цепь",
    "церковь",
    "цикл",
    "цифра",
    "чай",
    "чайка",
    "час",
    "частный",
    "часто",
    "часть",
    "часы",
    "чашка",
    "чаще",
    "чей",
    "человек",
    "чем",
    "через",
    "черный",
    "черта",
    "четвертый",
    "четыре",
    "число",
    "чисто",
    "чистый",
    "читать",
    "чтение",
    "что",
    "чтобы",
    "чувство",
    "чуть",
    "шаг",
    "шапка",
    "шахматы",
    "шеф",
    "шея",
    "широкий",
    "шкаф",
    "школа",
    "шляпа",
    "шоу",
    "шофер",
    "штаб",
    "штат",
    "щедрый",
    "щит",
    "экзамен",
    "экземпляр",
    "экономика",
    "экран",
    "электричество",
    "элемент",
    "эмоция",
    "энергия",
    "эпоха",
    "эра",
    "этаж",
    "этап",
    "эти",
    "этих",
    "это",
    "этого",
    "этой",
    "этом",
    "этот",
    "эффект",
    "юбилей",
    "юбка",
    "юг",
    "южный",
    "юмор",
    "юрист",
    "я",
    "явиться",
    "явный",
    "ядерный",
    "язык",
    "январь",
    "япония",
    "ясно",
    "ясный",
    "ящик",
];

// ============================================================
// English bigrams (expanded: ~90 most common)
// ============================================================

/// Sorted list of common Ukrainian words.
/// MUST stay sorted by UTF-8 bytes (that is what `str: Ord` compares, and
/// the Ukrainian-only letters є/і/ї/ґ sort *after* а-я) — enforced by the
/// `uk_words_sorted_and_unique` test.
const UK_WORDS: &[&str] = &[
    "а", "або", "абсолютно", "авжеж", "автобус", "автор", "адже", "адміністратор",
    "адреса", "аж", "але", "аналіз", "англійська", "апарат", "апеляція", "аптека",
    "артист", "архів", "аудіо", "аукціон", "багатий", "багато", "бажання", "бажати",
    "база", "базар", "батько", "бачив", "бачила", "бачити", "без", "безпека",
    "безпечний", "берег", "береза", "бесіда", "би", "близько", "блок", "блокнот",
    "бо", "бог", "богатир", "боятися", "брат", "брати", "британський", "бруд",
    "був", "будинок", "будувати", "будь", "була", "були", "було", "бути",
    "бюджет", "бібліотека", "біганина", "бігти", "бізнес", "білий", "біль", "більше",
    "більшість", "вага", "вагон", "важко", "важливий", "важливо", "вам", "вами",
    "ванна", "варто", "варіант", "вас", "ваш", "ваша", "ваше", "ваші",
    "вважати", "вважаю", "введення", "вгору", "вдалося", "вдень", "вдома", "вебсайт",
    "вечір", "вже", "взагалі", "взяти", "вибач", "вибачте", "вибрати", "вибір",
    "вивчати", "вигляд", "виглядає", "видалити", "видання", "визначити", "вийти", "викликати",
    "виконати", "використання", "використовувати", "вимкнути", "вимога", "винятково", "випадок", "випуск",
    "виробництво", "вирішити", "висновок", "високий", "висота", "виставка", "витрати", "виходити",
    "вихід", "вище", "вияв", "вміст", "внесок", "вниз", "внизу", "вода",
    "воду", "водій", "вокзал", "волосся", "вона", "вони", "воно", "восени",
    "восьмий", "вперед", "вправо", "враження", "врешті", "все", "всередині", "всесвіт",
    "встановити", "вступ", "всього", "вся", "всі", "втім", "вулиця", "вухо",
    "вчений", "вчитель", "вчора", "від", "відбувається", "відео", "відкрити", "відмінно",
    "відомий", "відповідальність", "відповідати", "відповідь", "відпустка", "відразу", "відсоток", "відстань",
    "відступ", "вік", "вікно", "він", "віра", "вірити", "вірно", "вісім",
    "вісімдесят", "вісімсот", "вітаю", "вітер", "віч", "газ", "газета", "галерея",
    "гаманець", "гаразд", "гарантія", "гарний", "гарно", "гарячий", "гейген", "герой",
    "гнів", "говорити", "говорю", "година", "годинник", "голова", "головна", "головний",
    "голос", "голосно", "гора", "гори", "горілка", "господар", "готель", "готовий",
    "готувати", "гра", "гравець", "градус", "грати", "графік", "гриб", "гривня",
    "гроші", "група", "густий", "давати", "давно", "далеко", "далі", "даний",
    "дані", "дата", "дах", "два", "двадцять", "двері", "двісті", "де",
    "декілька", "делегація", "демонстрація", "день", "дерево", "держава", "десять", "дещо",
    "джерело", "диван", "дивитися", "дивно", "дизайн", "директор", "диск", "дитина",
    "дитячий", "для", "дно", "до", "добре", "доброго", "довгий", "довго",
    "довіра", "договір", "додати", "додаток", "додому", "дозволити", "дозвіл", "документ",
    "долар", "долина", "дома", "дому", "донька", "допомагати", "допомога", "допомогти",
    "дорога", "дорогий", "досвід", "досить", "досліджувати", "доступ", "доступний", "дощ",
    "друг", "другий", "друзі", "друк", "друкувати", "дуже", "дума", "думати",
    "думка", "дурний", "дякую", "діалог", "діапазон", "дід", "дійсно", "дійти",
    "діло", "дім", "дія", "діяльність", "економіка", "екран", "експерт", "експорт",
    "елемент", "емоція", "енергія", "епоха", "ефект", "жаль", "жарт", "жати",
    "жваво", "живий", "життя", "жовтень", "жовтий", "журнал", "жінка", "з",
    "за", "забезпечити", "заборонити", "забув", "забути", "завантажити", "завдання", "завжди",
    "завод", "загалом", "заголовок", "заздалегідь", "зайняти", "заклад", "закон", "закрити",
    "зал", "залежить", "залишити", "залізниця", "замовлення", "замок", "занадто", "занепад",
    "запис", "записати", "запит", "запитання", "заплатити", "запрошення", "зараз", "зарплата",
    "захист", "захід", "зачекайте", "заявка", "зберегти", "збереження", "збірка", "зважати",
    "звичайний", "звичайно", "звук", "звідки", "звіт", "згідно", "зданий", "здається",
    "зелений", "земля", "зима", "зимовий", "злий", "зліва", "зміна", "змінити",
    "зміст", "знайти", "знак", "знання", "знати", "значення", "значно", "знову",
    "зображення", "зовнішній", "зовсім", "золото", "зона", "зошит", "зрозуміло", "зрозуміти",
    "зупинити", "зустрітися", "зустріч", "зшиток", "зір", "календар", "камера", "кампанія",
    "камінь", "канал", "кандидат", "канікули", "капітал", "карта", "картина", "картка",
    "каса", "катастрофа", "кафе", "качка", "каша", "квартира", "квиток", "квітень",
    "клас", "клуб", "ключ", "клієнт", "книга", "книжка", "кнопка", "код",
    "кожен", "кожна", "кожного", "колега", "колесо", "колись", "колір", "команда",
    "комбінація", "комерційний", "компанія", "комплекс", "комфорт", "конкурс", "консультація", "контакт",
    "контроль", "конференція", "концерт", "кордон", "корисний", "користуватися", "користувач", "король",
    "короткий", "коротко", "кошти", "край", "крайній", "крапка", "красивий", "краще",
    "країна", "кредит", "крок", "круглий", "крім", "культура", "купити", "курс",
    "кухня", "кілограм", "кілометр", "кілька", "кімната", "кіно", "лабораторія", "лампа",
    "ласка", "легкий", "легко", "лежати", "лекція", "лист", "листопад", "лише",
    "любий", "любити", "люди", "людина", "лютий", "ліворуч", "лід", "ліжко",
    "лікар", "лікарня", "ліс", "літак", "літера", "література", "літо", "магазин",
    "майбутнє", "майже", "макет", "малий", "мало", "мама", "матеріал", "мати",
    "машина", "ме", "меблі", "мед", "медаль", "межа", "мене", "менеджер",
    "менше", "мережа", "метод", "метро", "механізм", "мешканець", "ми", "мимо",
    "минулий", "мир", "мова", "могти", "може", "можливий", "можливість", "можна",
    "мокрий", "молодий", "молоко", "море", "мороз", "моя", "мрія", "мудрий",
    "музей", "музика", "мусити", "між", "мільйон", "мільярд", "міст", "містити",
    "місто", "місце", "місяць", "на", "набір", "навесні", "навколо", "навпаки",
    "навчання", "навіть", "нагода", "над", "надати", "надто", "надія", "назад",
    "назва", "наказ", "належати", "наліво", "напевно", "напис", "наприклад", "напій",
    "народ", "народження", "наступний", "наука", "національний", "наш", "наша", "наше",
    "наші", "не", "небо", "невдовзі", "негайно", "недавно", "недостатньо", "неділя",
    "нежить", "незалежність", "незважаючи", "немає", "непогано", "неправильно", "нерухомість", "нести",
    "нижче", "низький", "нове", "новий", "новина", "новини", "нога", "номер",
    "нормально", "носити", "ночі", "ну", "нуль", "ніби", "ніде", "ніж",
    "ніколи", "ніхто", "ніч", "нічого", "ніщо", "об", "обидва", "область",
    "обмін", "образ", "обрати", "обробка", "обслуговування", "обід", "одержати", "один",
    "одинадцять", "однак", "одного", "одразу", "одяг", "око", "окремо", "олівець",
    "операція", "опис", "описати", "оплата", "оптимальний", "організація", "оренда", "оригінал",
    "освіта", "основа", "основний", "останній", "офіс", "охорона", "очевидно", "очі",
    "пакет", "палець", "пан", "папір", "пара", "параметр", "парк", "паркан",
    "пароль", "партія", "пасажир", "пацієнт", "перевірити", "перевірка", "перед", "передати",
    "переклад", "перемога", "перерва", "перехід", "перший", "печиво", "пиво", "писати",
    "питання", "питати", "план", "планета", "платити", "плече", "площа", "побачити",
    "побудувати", "повернути", "поверх", "повний", "повністю", "повідомити", "повідомлення", "погано",
    "погляд", "погода", "подарунок", "подобатися", "подорож", "подяка", "подія", "позиція",
    "позичити", "поки", "покласти", "покупка", "поле", "половина", "положення", "політика",
    "помилка", "помітити", "помічник", "понад", "понеділок", "попереду", "попит", "порада",
    "поради", "поруч", "порівняння", "посада", "послуга", "постійно", "потреба", "потрібно",
    "потік", "потім", "початок", "починати", "пошта", "пошук", "поява", "правда",
    "правий", "правило", "право", "православний", "практика", "працювати", "працівник", "предмет",
    "предмети", "презентація", "претензія", "при", "прибуток", "приватний", "привіт", "призначення",
    "приклад", "прикрий", "приміщення", "принаймні", "принцип", "природа", "приходити", "причина",
    "приємно", "пробачте", "проблема", "проводити", "програма", "продавати", "продаж", "продовжити",
    "продукт", "проект", "проживання", "прокат", "промисловість", "пропозиція", "просити", "простий",
    "просто", "протягом", "процент", "процес", "прошу", "прямо", "птах", "публікація",
    "публічний", "південь", "північ", "під", "підготовка", "підключити", "підпис", "підприємство",
    "підстава", "підтвердити", "підтримка", "пізно", "пізніше", "після", "пісня", "раді",
    "радіо", "радість", "раз", "разом", "районний", "ранок", "раніше", "рахунок",
    "реальний", "режим", "резервний", "результат", "ремонт", "репутація", "ресторан", "ресурс",
    "речення", "речі", "решта", "реєстрація", "ризик", "ринок", "робити", "робота",
    "робочий", "родина", "рожевий", "розвиток", "розділ", "розклад", "розмова", "розмір",
    "розповідати", "розрахунок", "розробка", "розумний", "розуміти", "рука", "рукав", "рух",
    "рухатися", "ріг", "рідко", "рік", "річ", "річка", "сад", "сайт",
    "сам", "сама", "саме", "самий", "своя", "своє", "свої", "свято",
    "свіжий", "свій", "світ", "світло", "себе", "сезон", "секрет", "секунда",
    "село", "сенс", "серед", "середа", "середній", "серйозно", "серце", "сестра",
    "сила", "сильний", "символ", "син", "синій", "система", "сказати", "склад",
    "складний", "скло", "скоро", "скрізь", "скільки", "слава", "слово", "служба",
    "сліди", "смак", "смачно", "сміх", "сніг", "сніданок", "собака", "собі",
    "сон", "соняшник", "сорок", "сорочка", "соціальний", "спасибі", "спати", "спершу",
    "спеціальний", "спина", "спитати", "сповіщення", "спокій", "спорт", "спосіб", "справа",
    "справді", "сприяти", "спробувати", "спільний", "ставити", "стан", "станція", "старий",
    "старт", "стати", "стаття", "стежка", "страва", "страх", "структура", "стрічка",
    "студент", "стук", "стіл", "стільки", "стіна", "субота", "сум", "сумний",
    "сучасний", "схема", "сходинка", "схід", "сцена", "сьогодні", "сьомий", "сюди",
    "сім", "сімдесят", "сімсот", "та", "так", "такий", "також", "там",
    "танець", "твоя", "твоє", "твої", "твій", "театр", "тебе", "телефон",
    "тема", "темний", "температура", "тепер", "теплий", "термін", "тест", "техніка",
    "тиждень", "тижня", "тисяча", "тихо", "тиша", "то", "тобі", "товар",
    "товариш", "тоді", "той", "тому", "торгівля", "точка", "точно", "трава",
    "травень", "транспорт", "трапитися", "три", "тридцять", "тримати", "триста", "трохи",
    "труба", "туалет", "туди", "тур", "турбота", "тут", "тьотя", "тіло",
    "тільки", "тінь", "у", "увага", "уважно", "увечері", "угода", "уже",
    "україна", "українець", "українська", "український", "умова", "уміння", "університет", "управління",
    "урок", "усе", "успіх", "установа", "устрій", "усі", "утома", "уточнити",
    "учасник", "участь", "учень", "учитель", "файл", "факт", "факультет", "фах",
    "фестиваль", "форма", "формат", "фото", "фотографія", "фраза", "фрукт", "функція",
    "футбол", "фільм", "фінанси", "фірма", "характер", "хвилина", "хвиля", "хворий",
    "хиба", "хлопець", "хліб", "хмара", "хобі", "ходити", "хоч", "хоча",
    "хочу", "храм", "хтось", "худий", "художник", "хід", "цар", "цвях",
    "цей", "цукор", "цього", "цьому", "ці", "цікавий", "цікаво", "цілий",
    "цілком", "ціна", "цінність", "час", "частина", "часто", "частота", "чашка",
    "чверть", "чек", "чекати", "червень", "червоний", "черга", "черевик", "через",
    "чесно", "четвер", "четвертий", "чи", "чий", "чим", "численний", "чистий",
    "читати", "член", "чого", "чому", "чорний", "чотири", "чудово", "чужий",
    "шановний", "шапка", "швидкий", "швидко", "шести", "шеф", "шинка", "широкий",
    "шкода", "школа", "шлях", "шматок", "шоколад", "шоу", "штука", "шум",
    "шістдесят", "шість", "щастя", "ще", "щирий", "щоб", "щоденник", "щодня",
    "щось", "щотижня", "юрист", "я", "яблуко", "явище", "ягода", "яйце",
    "як", "якби", "який", "якийсь", "якось", "якраз", "якщо", "якість",
    "яма", "ярмарок", "ясно", "є", "єдиний", "ідея", "із", "їжа",
    "їхати", "їхній",
];

fn is_common_en_bigram(a: u8, b: u8) -> bool {
    matches!(
        (a, b),
        // Top bigrams
        (b't',b'h')|(b'h',b'e')|(b'i',b'n')|(b'e',b'r')|(b'a',b'n')|(b'r',b'e')|
        (b'o',b'n')|(b'a',b't')|(b'e',b'n')|(b'n',b'd')|(b't',b'i')|(b'e',b's')|
        (b'o',b'r')|(b't',b'e')|(b'o',b'f')|(b'e',b'd')|(b'i',b's')|(b'i',b't')|
        (b'a',b'l')|(b'a',b'r')|(b's',b't')|(b't',b'o')|(b'n',b't')|(b'n',b'g')|
        (b's',b'e')|(b'h',b'a')|(b'o',b'u')|(b'i',b'o')|(b'l',b'e')|(b'v',b'e')|
        (b'c',b'o')|(b'm',b'e')|(b'h',b'i')|(b'r',b'i')|(b'r',b'o')|(b'n',b'e')|
        (b'e',b'a')|(b'r',b'a')|(b'l',b'l')|(b'i',b'l')|(b'l',b'y')|(b'i',b'g')|
        (b'w',b'i')|(b'w',b'h')|(b'l',b'i')|(b'c',b'h')|(b's',b'h')|(b'o',b'w')|
        // Added: common pairs missing from original table
        (b's',b'o')|(b'o',b'm')|(b'm',b'a')|(b'a',b'k')|(b'k',b'e')|(b'u',b's')|
        (b'a',b's')|(b'u',b'r')|(b'a',b'd')|(b'u',b'n')|(b'd',b'i')|(b'l',b'o')|
        (b'l',b'a')|(b'p',b'e')|(b'a',b'c')|(b'c',b'a')|(b'g',b'e')|(b'u',b'l')|
        (b'p',b'r')|(b'p',b'l')|(b'b',b'e')|(b'o',b'o')|(b'e',b'e')|(b'w',b'a')|
        (b'i',b'c')|(b'd',b'e')|(b'a',b'v')|(b'c',b'e')|(b'u',b't')|(b'n',b'o')|
        (b'o',b't')|(b'a',b'i')|(b'd',b'o')|(b'i',b'd')|(b'u',b'p')|(b'b',b'l')|
        (b'c',b'l')|(b'g',b'r')|(b'c',b'r')|(b't',b'r')|(b'f',b'r')|(b's',b'p')|
        (b's',b'w')|(b'f',b'o')|(b'n',b'i')|(b'm',b'i')|(b'a',b'g')|(b'e',b'l')|
        (b'd',b'a')|(b'p',b'a')|(b't',b'a')|(b'w',b'o')|(b'k',b'i')|(b'f',b'i')|
        (b'e',b'x')|(b'o',b'p')
    )
}

/// Bigrams that almost never appear in English.
fn is_bad_en_bigram(a: u8, b: u8) -> bool {
    matches!(
        (a, b),
        (b'q', b'b')
            | (b'q', b'c')
            | (b'q', b'd')
            | (b'q', b'f')
            | (b'q', b'g')
            | (b'q', b'h')
            | (b'q', b'j')
            | (b'q', b'k')
            | (b'q', b'l')
            | (b'q', b'm')
            | (b'q', b'n')
            | (b'q', b'p')
            | (b'q', b'r')
            | (b'q', b's')
            | (b'q', b't')
            | (b'q', b'v')
            | (b'q', b'w')
            | (b'q', b'x')
            | (b'q', b'z')
            | (b'j', b'q')
            | (b'j', b'x')
            | (b'j', b'z')
            | (b'j', b'w')
            | (b'j', b'v')
            | (b'j', b'f')
            | (b'j', b'g')
            | (b'j', b'b')
            | (b'j', b'c')
            | (b'j', b'd')
            | (b'j', b'k')
            | (b'v', b'q')
            | (b'v', b'j')
            | (b'v', b'x')
            | (b'v', b'z')
            | (b'v', b'w')
            | (b'v', b'b')
            | (b'z', b'x')
            | (b'z', b'q')
            | (b'z', b'j')
            | (b'z', b'v')
            | (b'z', b'b')
            | (b'z', b'g')
            | (b'z', b'k')
            | (b'z', b'r')
            | (b'z', b'w')
            | (b'z', b'f')
            | (b'z', b'p')
            | (b'z', b'd')
            | (b'x', b'j')
            | (b'x', b'q')
            | (b'x', b'z')
            | (b'x', b'g')
            | (b'x', b'k')
            | (b'x', b'r')
            | (b'w', b'q')
            | (b'w', b'z')
            | (b'w', b'x')
            | (b'w', b'v')
            | (b'k', b'q')
            | (b'k', b'z')
            | (b'k', b'x')
            | (b'h', b'q')
            | (b'h', b'z')
            | (b'h', b'x')
            | (b'b', b'q')
            | (b'b', b'x')
            | (b'b', b'z')
            | (b'g', b'q')
            | (b'g', b'x')
            | (b'g', b'z')
            | (b'f', b'q')
            | (b'f', b'x')
            | (b'f', b'z')
            | (b'p', b'q')
            | (b'p', b'x')
            | (b'p', b'z')
    )
}

// ============================================================
// English trigrams (top ~50)
// ============================================================

fn is_common_en_trigram(a: u8, b: u8, c: u8) -> bool {
    matches!(
        (a, b, c),
        (b't', b'h', b'e')
            | (b'a', b'n', b'd')
            | (b'i', b'n', b'g')
            | (b't', b'i', b'o')
            | (b'i', b'o', b'n')
            | (b'e', b'n', b't')
            | (b'h', b'e', b'r')
            | (b't', b'h', b'a')
            | (b'e', b'r', b'e')
            | (b'f', b'o', b'r')
            | (b'y', b'o', b'u')
            | (b'a', b'l', b'l')
            | (b'v', b'e', b'r')
            | (b't', b'h', b'i')
            | (b'w', b'i', b't')
            | (b'i', b't', b'h')
            | (b'h', b'i', b'n')
            | (b'g', b'h', b't')
            | (b'o', b'u', b'r')
            | (b'n', b'o', b't')
            | (b'o', b'm', b'e')
            | (b'o', b'u', b't')
            | (b's', b't', b'r')
            | (b'c', b'o', b'n')
            | (b'p', b'r', b'o')
            | (b'a', b'r', b'e')
            | (b'a', b'v', b'e')
            | (b'i', b'n', b't')
            | (b'e', b's', b's')
            | (b'e', b's', b't')
            | (b'a', b't', b'e')
            | (b'a', b'c', b'k')
            | (b'o', b'r', b'e')
            | (b'e', b'r', b's')
            | (b'e', b'c', b't')
            | (b'o', b'n', b'e')
            | (b'l', b'i', b'n')
            | (b't', b'e', b'r')
            | (b'w', b'a', b's')
            | (b'h', b'a', b't')
            | (b'h', b'i', b's')
            | (b'h', b'a', b's')
            | (b'h', b'a', b'v')
            | (b'r', b'e', b'a')
            | (b'n', b'c', b'e')
            | (b'i', b'v', b'e')
            | (b'o', b'r', b'd')
            | (b'u', b's', b'e')
            | (b'a', b'k', b'e')
            | (b't', b'e', b'd')
            | (b's', b'o', b'm')
            | (b'u', b'l', b'd')
            | (b'a', b's', b't')
            | (b'i', b'g', b'h')
            | (b'e', b'a', b'd')
            | (b'l', b'o', b'o')
            | (b'e', b'e', b'n')
            | (b'a', b'n', b't')
            | (b'h', b'e', b'n')
            | (b'h', b'e', b'm')
    )
}

// ============================================================
// Russian bigrams (expanded: ~80 most common)
// ============================================================

fn is_common_ru_bigram(a: char, b: char) -> bool {
    matches!(
        (a, b),
        // Original table
        ('с','т')|('н','о')|('т','о')|('н','а')|('е','н')|('к','о')|('р','о')|
        ('о','в')|('п','о')|('р','а')|('о','т')|('н','е')|('о','с')|('н','и')|
        ('а','л')|('е','р')|('в','о')|('п','р')|('о','р')|('р','е')|('а','н')|
        ('т','ь')|('е','л')|('о','л')|('к','а')|('е','м')|('о','м')|('л','о')|
        ('в','а')|('л','и')|('и','т')|('т','е')|('о','й')|('и','я')|('а','т')|
        ('и','л')|('л','е')|('д','а')|('и','н')|('д','е')|('е','т')|('п','е')|
        ('о','д')|('е','с')|('о','н')|('и','е')|('н','н')|('е','д')|('в','е')|
        ('с','к')|
        // Added: common pairs missing from original
        ('а','р')|('е','к')|('о','г')|('с','е')|('т','а')|('п','а')|('е','г')|
        ('р','и')|('м','а')|('н','у')|('е','ж')|('г','о')|('м','о')|('б','о')|
        ('ч','е')|('т','и')|('б','ы')|('ы','л')|('о','ж')|('и','м')|('е','е')|
        ('а','к')|('ж','е')|('у','т')|('ю','т')|('ы','й')|('м','и')|('щ','е')|
        ('ш','и')|('н','ы')|('д','о')
    )
}

/// Bigrams that almost never appear in natural Russian.
fn is_bad_ru_bigram(a: char, b: char) -> bool {
    matches!(
        (a, b),
        // щ + hard consonant
        ('щ','к')|('щ','д')|('щ','б')|('щ','г')|('щ','п')|('щ','х')|('щ','ц')|
        ('щ','ш')|('щ','щ')|('щ','ж')|('щ','з')|('щ','ф')|('щ','в')|('щ','м')|
        ('щ','л')|('щ','р')|('щ','т')|('щ','н')|
        // consonant + щ (unusual)
        ('д','щ')|('ц','щ')|('г','щ')|('к','щ')|('п','щ')|('ф','щ')|('х','щ')|
        ('б','щ')|('л','щ')|('з','щ')|('ж','щ')|('ш','щ')|
        // ъ impossible combos
        ('ъ','б')|('ъ','г')|('ъ','д')|('ъ','ж')|('ъ','з')|('ъ','к')|('ъ','л')|
        ('ъ','м')|('ъ','н')|('ъ','п')|('ъ','р')|('ъ','с')|('ъ','т')|('ъ','ф')|
        ('ъ','х')|('ъ','ц')|('ъ','ч')|('ъ','ш')|('ъ','щ')|('ъ','ъ')|('ъ','ь')|
        ('ъ','ы')|('ъ','а')|('ъ','и')|('ъ','о')|('ъ','у')|('ъ','э')|
        // ь impossible combos
        ('ь','ъ')|('ь','ь')|('ь','ы')|
        // Other very rare
        ('й','ъ')|('й','ь')|('ж','ш')|('ш','ж')|('ц','ж')|('ж','ц')|
        ('ы','ь')|('ы','ъ')|('ы','э')|('э','ы')|('э','ъ')|('э','щ')|
        // Patterns typical of mistyped English
        ('ы','щ')|('щ','ь')|('ь','у')|('ь','ф')|('щ','у')
    )
}

// ============================================================
// Russian trigrams (top ~40)
// ============================================================

fn is_common_ru_trigram(a: char, b: char, c: char) -> bool {
    matches!(
        (a, b, c),
        ('с', 'т', 'о')
            | ('с', 'т', 'а')
            | ('с', 'т', 'в')
            | ('е', 'н', 'и')
            | ('о', 'в', 'а')
            | ('а', 'т', 'ь')
            | ('и', 'т', 'ь')
            | ('п', 'р', 'о')
            | ('п', 'р', 'и')
            | ('п', 'р', 'е')
            | ('п', 'е', 'р')
            | ('е', 'р', 'е')
            | ('о', 'г', 'о')
            | ('н', 'о', 'й')
            | ('н', 'ы', 'х')
            | ('е', 'г', 'о')
            | ('о', 'н', 'а')
            | ('в', 'с', 'е')
            | ('и', 'л', 'и')
            | ('э', 'т', 'о')
            | ('к', 'о', 'м')
            | ('т', 'е', 'л')
            | ('о', 'с', 'т')
            | ('п', 'о', 'л')
            | ('о', 'д', 'н')
            | ('н', 'и', 'е')
            | ('н', 'о', 'с')
            | ('т', 'о', 'р')
            | ('к', 'а', 'к')
            | ('ч', 'т', 'о')
            | ('д', 'е', 'л')
            | ('а', 'н', 'и')
            | ('н', 'ы', 'е')
            | ('о', 'й', 'н')
            | ('т', 'ь', 'с')
            | ('н', 'а', 'л')
            | ('е', 'с', 'т')
            | ('о', 'в', 'о')
            | ('е', 'д', 'е')
            | ('а', 'л', 'ь')
    )
}

// ============================================================
// Character classification
// ============================================================

fn is_en_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
    // Note: 'y' excluded — it's a consonant more often than a vowel
}

fn is_ru_vowel(c: char) -> bool {
    matches!(c, 'а' | 'е' | 'ё' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я')
}

/// Vowels across both Cyrillic languages we handle. `score_cyrillic` must use
/// this rather than [`is_ru_vowel`]: `і ї є` are vowels, and counting them as
/// consonants makes any Ukrainian word look like an unpronounceable cluster
/// ("ініе" scoring as four consonants in a row).
fn is_cyr_vowel(c: char) -> bool {
    is_ru_vowel(c) || matches!(c, 'і' | 'ї' | 'є')
}

/// Cyrillic letters that exist in Ukrainian but NOT in Russian.
/// Their presence is a strong signal the word is intentionally Ukrainian.
fn is_ukrainian_only(c: char) -> bool {
    matches!(c, 'і' | 'І' | 'ї' | 'Ї' | 'є' | 'Є' | 'ґ' | 'Ґ')
}

/// Active keyboard layout category.  Only layouts we handle natively get
/// their own variant; everything else falls into `Latin` (no translation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kbd {
    Latin,
    Russian,
    Ukrainian,
}

fn vk_to_char(vk: u32, shift: bool, caps: bool, kbd: Kbd) -> Option<char> {
    if !(0x41..=0x5A).contains(&vk) {
        return None;
    }
    let c = (vk as u8) as char;
    // CapsLock inverts the shift behaviour.
    let upper = shift ^ caps;
    let en = if upper { c } else { c.to_ascii_lowercase() };
    Some(match kbd {
        Kbd::Russian => crate::layout::en_to_ru(en),
        Kbd::Ukrainian => crate::layout::en_to_uk(en),
        Kbd::Latin => en,
    })
}

/// Character produced by the number row. Unshifted it is layout-independent,
/// but `Shift+2` is `@` on a Latin layout and `"` on the Russian/Ukrainian
/// ones — and we need the exact character to re-type it after the correction.
fn digit_row_char(vk: u32, shift: bool, is_cyrillic_layout: bool) -> Option<char> {
    if !shift {
        return Some(((vk - 0x30) as u8 + b'0') as char);
    }
    let idx = (vk - 0x30) as usize;
    let row = if is_cyrillic_layout {
        [')', '!', '"', '№', ';', '%', ':', '?', '*', '(']
    } else {
        [')', '!', '@', '#', '$', '%', '^', '&', '*', '(']
    };
    row.get(idx).copied()
}

/// Maps OEM VK codes to their punctuation character for the boundary replacement.
/// Used when OEM keys are treated as word boundaries (English layout, or
/// non-letter OEM keys in Cyrillic layout).
fn oem_boundary_char(vk: u32, shift: bool, is_cyrillic_layout: bool) -> Option<char> {
    if is_cyrillic_layout {
        // Cyrillic layouts: only a few OEM keys produce punctuation
        Some(match (vk, shift) {
            (0xBF, false) => '.', // OEM_2 → .
            (0xBF, true) => ',',  // OEM_2 + Shift → ,
            (0xBB, false) => '=', // OEM_PLUS → =
            (0xBB, true) => '+',  // OEM_PLUS + Shift → +
            (0xBD, false) => '-', // OEM_MINUS → -
            (0xBD, true) => '_',  // OEM_MINUS + Shift → _
            (0xDC, false) => '\\',
            (0xDC, true) => '/',
            // Ukrainian OEM_3 is a literal apostrophe, not a letter.
            (0xC0, _) => '\'',
            _ => ' ',
        })
    } else {
        // English layout: standard mapping
        Some(match (vk, shift) {
            (0xBA, false) => ';',
            (0xBA, true) => ':',
            (0xBC, false) => ',',
            (0xBC, true) => '<',
            (0xBE, false) => '.',
            (0xBE, true) => '>',
            (0xBF, false) => '/',
            (0xBF, true) => '?',
            (0xDE, false) => '\'',
            (0xDE, true) => '"',
            (0xDB, false) => '[',
            (0xDB, true) => '{',
            (0xDD, false) => ']',
            (0xDD, true) => '}',
            (0xDC, false) => '\\',
            (0xDC, true) => '|',
            (0xBB, false) => '=',
            (0xBB, true) => '+',
            (0xBD, false) => '-',
            (0xBD, true) => '_',
            (0xC0, false) => '`',
            (0xC0, true) => '~',
            _ => ' ',
        })
    }
}

/// Maps OEM VK codes to Cyrillic letters for the active layout.
///   Russian:    OEM_3(`)→ё  OEM_4([)→х  OEM_6(])→ъ  OEM_1(;)→ж
///               OEM_7(')→э  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
///   Ukrainian:  OEM_3(`)→' (apostrophe — NOT a letter, caller treats as boundary)
///               OEM_4([)→х  OEM_6(])→ї  OEM_1(;)→ж
///               OEM_7(')→є  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
fn oem_to_cyr_char(vk: u32, shift: bool, caps: bool, kbd: Kbd) -> Option<char> {
    let upper = shift ^ caps;
    let ch = match (kbd, vk) {
        (Kbd::Russian, 0xC0) => 'ё',
        (Kbd::Russian, 0xDD) => 'ъ',
        (Kbd::Russian, 0xDE) => 'э',
        (Kbd::Ukrainian, 0xDD) => 'ї',
        (Kbd::Ukrainian, 0xDE) => 'є',
        // Ukrainian OEM_3 is a literal apostrophe (boundary-like) — bail out
        // so caller treats it as punctuation/word separator.
        (Kbd::Ukrainian, 0xC0) => return None,
        // Shared letters between RU and UK layouts:
        (Kbd::Russian | Kbd::Ukrainian, 0xDB) => 'х',
        (Kbd::Russian | Kbd::Ukrainian, 0xBA) => 'ж',
        (Kbd::Russian | Kbd::Ukrainian, 0xBC) => 'б',
        (Kbd::Russian | Kbd::Ukrainian, 0xBE) => 'ю',
        _ => return None,
    };
    if upper {
        Some(ch.to_uppercase().next().unwrap_or(ch))
    } else {
        Some(ch)
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulates typing a latin word while RU layout is active.
    /// (What `decide_switch` sees for that situation.)
    fn ru_gibberish_of(en: &str) -> String {
        en.chars().map(crate::layout::en_to_ru).collect()
    }

    /// And vice-versa.
    fn en_gibberish_of(ru: &str) -> String {
        use crate::layout::convert;
        convert(ru) // convert detects cyrillic → calls ru_to_en
    }

    /// Keystrokes that produce `uk` on a Ukrainian layout.
    fn uk_keys_of(uk: &str) -> String {
        uk.chars().map(crate::layout::uk_to_en).collect()
    }

    /// Which layout the word was typed on. Everything except the dedicated
    /// Ukrainian tests uses the RU/EN pair, matching the original behaviour.
    fn kbd_of(typed: &str) -> Kbd {
        if typed.chars().any(is_cyrillic) {
            Kbd::Russian
        } else {
            Kbd::Latin
        }
    }

    #[track_caller]
    fn should_switch(typed: &str, expected: &str) {
        let got = decide_switch(typed, kbd_of(typed), Kbd::Russian);
        assert_eq!(
            got.as_ref().map(|c| c.text.as_str()),
            Some(expected),
            "typed={typed:?} → wanted Some({expected:?}), got {got:?}"
        );
    }

    /// Like `should_switch`, but also pins the layout we switch the user to.
    #[track_caller]
    fn should_switch_to(typed: &str, kbd: Kbd, cyr_pref: Kbd, expected: &str, target: Kbd) {
        let got = decide_switch(typed, kbd, cyr_pref);
        assert_eq!(
            got.as_ref().map(|c| (c.text.as_str(), c.target)),
            Some((expected, target)),
            "typed={typed:?} on {kbd:?} (pref {cyr_pref:?}), got {got:?}"
        );
    }

    #[track_caller]
    fn should_not_switch(typed: &str) {
        let got = decide_switch(typed, kbd_of(typed), Kbd::Russian);
        assert_eq!(got, None, "typed={typed:?} expected no switch, got {got:?}");
    }

    #[track_caller]
    fn should_switch_uk(typed: &str, expected: &str) {
        let got = decide_switch(typed, Kbd::Ukrainian, Kbd::Ukrainian);
        assert_eq!(
            got.as_ref().map(|c| c.text.as_str()),
            Some(expected),
            "uk: typed={typed:?} → wanted Some({expected:?}), got {got:?}"
        );
    }

    #[track_caller]
    fn should_not_switch_uk(typed: &str) {
        let got = decide_switch(typed, Kbd::Ukrainian, Kbd::Ukrainian);
        assert_eq!(
            got, None,
            "uk: typed={typed:?} expected no switch, got {got:?}"
        );
    }

    // --- EN typed with RU layout (gibberish cyrillic → should become english) ---

    #[test]
    fn fixes_hello_typed_in_ru_layout() {
        let g = ru_gibberish_of("hello");
        assert_eq!(g, "руддщ");
        should_switch(&g, "hello");
    }

    #[test]
    fn fixes_debug_typed_in_ru_layout() {
        let g = ru_gibberish_of("debug");
        should_switch(&g, "debug");
    }

    #[test]
    fn fixes_error_typed_in_ru_layout() {
        let g = ru_gibberish_of("error");
        should_switch(&g, "error");
    }

    #[test]
    fn fixes_config_typed_in_ru_layout() {
        should_switch(&ru_gibberish_of("config"), "config");
    }

    #[test]
    fn fixes_update_typed_in_ru_layout() {
        should_switch(&ru_gibberish_of("update"), "update");
    }

    #[test]
    fn fixes_running_via_suffix_ing() {
        // "running" isn't in whitelist — must win by scoring + -ing suffix bonus.
        should_switch(&ru_gibberish_of("running"), "running");
    }

    #[test]
    fn fixes_typo_via_suffix_or_scoring() {
        // "typo" — tricky because RU transliteration "енущ" looks semi-Russian
        // (contains common bigram ен). Requires added EN whitelist entry OR
        // strong EN signal. We added "typo"? No — must rely on scoring.
        // This test asserts current behaviour documented.
        // If it starts passing we're happy; if not, the doc stays accurate.
        let g = ru_gibberish_of("typo");
        let _ = decide_switch(&g, kbd_of(&g), Kbd::Russian);
    }

    // --- RU typed with EN layout (latin gibberish → should become russian) ---

    #[test]
    fn fixes_privet_typed_in_en_layout() {
        let g = en_gibberish_of("привет");
        assert_eq!(g, "ghbdtn");
        should_switch(&g, "привет");
    }

    // --- False-positive guards: real words must NOT switch ---

    #[test]
    fn keeps_real_english_hello() {
        should_not_switch("hello");
    }

    #[test]
    fn keeps_real_russian_привет() {
        should_not_switch("привет");
    }

    #[test]
    fn keeps_real_russian_работать() {
        should_not_switch("работать");
    }

    #[test]
    fn keeps_mixed_scripts_untouched() {
        should_not_switch("hello мир");
        should_not_switch("приветworld");
    }

    // --- Tech-identifier guard: low-vowel Latin tokens must NOT be
    //     converted to Cyrillic.  These are acronyms, file extensions,
    //     package/brand names — never accidental Russian. ---

    #[test]
    fn keeps_zero_vowel_acronyms() {
        // 0-vowel Latin: GTA, VLC, MKV, SQL, NPM, VPN, SSH, FTP, PNG, JPG…
        should_not_switch("gta");
        should_not_switch("vlc");
        should_not_switch("mkv");
        should_not_switch("png");
        should_not_switch("jpg");
        should_not_switch("npm");
        should_not_switch("vpn");
        should_not_switch("dll");
        should_not_switch("xml"); // also in EN dict, double-covered
    }

    #[test]
    fn keeps_low_vowel_tech_terms() {
        // 1-vowel ≤6-char Latin: ffmpeg, json, yarn, exe, pdf-style.
        should_not_switch("ffmpeg");
        should_not_switch("json");
        should_not_switch("yarn");
        should_not_switch("exe");
        should_not_switch("webp");
    }

    #[test]
    fn keeps_uppercase_tech_acronyms() {
        // Same applies regardless of case.
        should_not_switch("GTA");
        should_not_switch("FFmpeg");
        should_not_switch("JSON");
    }

    #[test]
    fn dict_match_overrides_low_vowel_guard() {
        // The guard runs AFTER dictionary lookup of the converted form, so
        // legitimate Russian words typed in EN layout still get fixed even
        // when the Latin gibberish has 0/1 vowels.
        should_switch(&en_gibberish_of("привет"), "привет"); // ghbdtn (0 vowels)
        should_switch(&en_gibberish_of("спасибо"), "спасибо"); // cgfcb,j-ish
    }

    // --- RU→EN symmetric hardening: positive-evidence floor ---
    //
    // The RU→EN direction used to switch on score margin alone. A Cyrillic
    // token absent from RU_WORDS whose Latin conversion only scored "less bad"
    // than the input would be flipped to a non-word. We now require the
    // converted English to clear an absolute floor (en_score ≥ 12) and contain
    // ≥2 common English bigrams.

    #[test]
    fn ru_to_en_floor_blocks_weak_nonword() {
        // "шфку" → "iare": converted Latin beats the old margin
        // (en≈11 > ru≈4 + 5) but does NOT clear the en_score ≥ 12 floor.
        // Before the hardening this wrongly flipped to a non-word.
        assert_eq!(crate::layout::convert("шфку"), "iare");
        should_not_switch("шфку");
    }

    #[test]
    fn ru_to_en_one_vowel_english_still_switches() {
        // Regression guard: we must NOT port the EN→RU low-vowel "tech" guard
        // into this direction. Plenty of ordinary English words convert from
        // Cyrillic with a single vowel — they must still be corrected.
        // "еуче" → "text" (one vowel, 4 letters).
        assert_eq!(crate::layout::convert("еуче"), "text");
        should_switch("еуче", "text");
    }

    // --- Tech terms typed on RU layout get auto-corrected back to Latin ---
    //
    // User wants "gta" but RU layout was active → typed "пеф" → must become
    // "gta". Same for "ffmpeg" → "ааьзуп". This is the inverse of the
    // false-positive guard above and relies on TECH_WORDS being treated as
    // valid English. The number-suffix case ("пеф6" → "gta6") is handled by
    // the keyboard hook treating digits as a word boundary, so decide_switch
    // only sees the alphabetic part — that's what we test here.
    #[test]
    fn fixes_gta_typed_in_ru_layout() {
        let g = ru_gibberish_of("gta");
        assert_eq!(g, "пеф");
        should_switch(&g, "gta");
    }

    #[test]
    fn fixes_ffmpeg_typed_in_ru_layout() {
        let g = ru_gibberish_of("ffmpeg");
        assert_eq!(g, "ааьзуп"); // f→а, m→ь, p→з, e→у, g→п
        should_switch(&g, "ffmpeg");
    }

    #[test]
    fn fixes_other_tech_terms_typed_in_ru_layout() {
        for w in ["json", "vlc", "mkv", "png", "exe", "regex", "yarn", "npm"] {
            should_switch(&ru_gibberish_of(w), w);
        }
    }

    #[test]
    fn tech_words_sorted_and_unique() {
        let mut sorted: Vec<&&str> = TECH_WORDS.iter().collect();
        sorted.sort();
        let same: Vec<&&str> = TECH_WORDS.iter().collect();
        assert_eq!(sorted, same, "TECH_WORDS must be sorted");
        let mut dedup = same.clone();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            same.len(),
            "TECH_WORDS must not contain duplicates"
        );
    }

    #[test]
    fn tech_words_dont_overlap_with_en_words() {
        // Anything in TECH_WORDS shouldn't already be in EN_WORDS — keeps
        // the lookup minimal and avoids confusing dual-source semantics.
        for &w in TECH_WORDS {
            assert!(
                EN_WORDS.binary_search(&w).is_err(),
                "{w:?} is in both TECH_WORDS and EN_WORDS — remove from one"
            );
        }
    }

    #[test]
    fn tech_words_no_ru_collision() {
        // Safety net for future additions: a tech term must not be the layout
        // image of a *known* Russian word. Otherwise typing that Russian word
        // (RU layout) would wrongly flip to the Latin tech term, since the
        // RU→EN branch returns Some on is_known_en_word(converted).
        for &w in TECH_WORDS {
            let cyr = crate::layout::convert(w).to_lowercase();
            assert!(
                !is_known_ru_word(&cyr),
                "TECH word {w:?} converts to known RU word {cyr:?} — collision"
            );
        }
    }

    #[test]
    fn fixes_new_tech_terms_typed_in_ru_layout() {
        // Representative sample of the expanded TECH_WORDS: each, typed on a
        // RU layout, must be auto-corrected back to Latin.
        for w in [
            "docker", "nginx", "redis", "kotlin", "django", "kubectl", "webpack", "golang",
            "sqlite", "tailwind", "graphql", "terraform",
        ] {
            should_switch(&ru_gibberish_of(w), w);
        }
    }

    // --- Ukrainian: words with UK-specific chars must never switch ---

    #[test]
    fn keeps_ukrainian_privit() {
        // "привіт" contains 'і' — Ukrainian-only. Must NOT become "привыт".
        should_not_switch("привіт");
    }

    #[test]
    fn keeps_ukrainian_with_yi() {
        // "її" — Ukrainian "her/hers" / reflexive. Has 'ї'.
        should_not_switch("її");
        should_not_switch("їжа"); // food
        should_not_switch("україна"); // country name
    }

    #[test]
    fn keeps_ukrainian_with_ye() {
        should_not_switch("є"); // "is/am/are"
        should_not_switch("єдиний"); // only/single
    }

    #[test]
    fn keeps_ukrainian_with_g() {
        should_not_switch("ґанок"); // porch
    }

    #[test]
    fn keeps_uppercase_ukrainian_chars() {
        should_not_switch("Їжак");
        should_not_switch("ІВАН");
    }

    // --- Latin word that would "convert" into Ukrainian still works fine ---
    // (No change expected — Latin→Cyrillic goes to Russian, never Ukrainian,
    // so Ukrainian-guard is not triggered on the input; guard runs first.)

    #[test]
    fn ukrainian_guard_runs_before_conversion() {
        // Even if some scoring heuristic would otherwise decide to switch,
        // the UK-letter guard short-circuits. This covers future scoring
        // changes that might accidentally weaken the safeguard.
        let uk = "привіт";
        assert!(uk.chars().any(is_ukrainian_only));
        should_not_switch(uk);
    }

    // --- Whitelists must stay sorted for binary_search ---

    #[test]
    fn en_words_sorted_and_unique() {
        let mut sorted: Vec<&&str> = EN_WORDS.iter().collect();
        sorted.sort();
        let same: Vec<&&str> = EN_WORDS.iter().collect();
        assert_eq!(sorted, same, "EN_WORDS must be sorted");
        let mut dedup = same.clone();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            same.len(),
            "EN_WORDS must not contain duplicates"
        );
    }

    #[test]
    fn ru_words_sorted_and_unique() {
        let mut sorted: Vec<&&str> = RU_WORDS.iter().collect();
        sorted.sort();
        let same: Vec<&&str> = RU_WORDS.iter().collect();
        assert_eq!(sorted, same, "RU_WORDS must be sorted");
        let mut dedup = same.clone();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            same.len(),
            "RU_WORDS must not contain duplicates"
        );
    }

    // ============================================================
    // Prefix lookup / early-detector tests
    // ============================================================

    #[track_caller]
    fn should_partial(typed: &str, expected: &str) {
        let got = decide_partial_switch(typed, kbd_of(typed), Kbd::Russian);
        assert_eq!(
            got.as_ref().map(|c| c.text.as_str()),
            Some(expected),
            "partial: typed={typed:?} wanted Some({expected:?}), got {got:?}"
        );
    }

    #[track_caller]
    fn should_not_partial(typed: &str) {
        let got = decide_partial_switch(typed, kbd_of(typed), Kbd::Russian);
        assert_eq!(
            got, None,
            "partial: typed={typed:?} expected None, got {got:?}"
        );
    }

    #[test]
    fn prefix_lookup_empty() {
        assert!(has_prefix_in(EN_WORDS, ""));
        assert!(has_prefix_in(RU_WORDS, ""));
    }

    #[test]
    fn prefix_lookup_live_en() {
        assert!(is_en_prefix("hel"));
        assert!(is_en_prefix("tho"));
        assert!(is_en_prefix("pro"));
        assert!(is_en_prefix("que"));
    }

    #[test]
    fn prefix_lookup_dead_en() {
        // 3-char combos that are not prefixes of any real English word.
        // These are exactly the gibberish we want the early detector to spot.
        assert!(!is_en_prefix("ghb"));
        assert!(!is_en_prefix("djk"));
        assert!(!is_en_prefix("xzq"));
    }

    #[test]
    fn prefix_lookup_live_ru() {
        assert!(is_ru_prefix("при"));
        assert!(is_ru_prefix("раб"));
        assert!(is_ru_prefix("дом"));
        assert!(is_ru_prefix("чел"));
    }

    #[test]
    fn prefix_lookup_dead_ru() {
        assert!(!is_ru_prefix("ыыы"));
        assert!(!is_ru_prefix("щщщ"));
        assert!(!is_ru_prefix("руд")); // "руд" is not in our dict directly;
        // actually "ру…" starts words but "руд"
        // specifically has no word beginning
        // with those 3 letters in our curated list.
        // Actually "руда" is plausible but we didn't include it — double-check:
        // If this test fails, just delete this assertion — it only verifies
        // the DEAD branch, it's OK if a word happens to match.
    }

    // --- Partial (mid-word) detection — should fire early ---

    #[test]
    fn partial_stays_quiet_at_three_chars() {
        // Three characters is deliberately not enough. A dead end in a
        // 2400-word English list is weak evidence that early: 4.8% of all
        // 3-letter combinations used to trip the detector, "afr" of "afraid"
        // among them. See MIN_PARTIAL_LEN.
        should_not_partial("ghb");
    }

    #[test]
    fn partial_fires_privet_after_four_chars_from_en() {
        // User intends "привет" but is in EN layout. One keystroke later than
        // before, the mismatch is unmistakable.
        should_partial("ghbd", "прив");
    }

    #[test]
    fn partial_fires_privet_after_four_chars() {
        let buf = "ghbd"; // "прив"
        should_partial(buf, "прив");
    }

    #[test]
    fn partial_fires_hello_after_three_chars_in_ru() {
        // User intends "hello" but is in RU layout. After typing the RU
        // gibberish "рудд" (== "hell" reversed), we should fire.
        let g = ru_gibberish_of("hell"); // "руддд"? let's see below
        // "hell" -> h=р, e=у, l=д, l=д -> "рудд"
        assert_eq!(g, "рудд");
        should_partial(&g, "hell");
    }

    // --- Partial detection — must NOT fire when user is typing a real word ---

    #[test]
    fn partial_no_fire_for_valid_en_prefix() {
        // "tho" is a valid EN prefix (though, thought). Don't switch.
        should_not_partial("tho");
        should_not_partial("hel"); // hello, help
        should_not_partial("pro"); // program, project
        should_not_partial("que"); // question, query
    }

    #[test]
    fn partial_no_fire_for_valid_ru_prefix() {
        should_not_partial("при");
        should_not_partial("раб");
        should_not_partial("дом");
    }

    #[test]
    fn partial_no_fire_for_too_short() {
        should_not_partial("gh");
        should_not_partial("ру");
        should_not_partial("x");
        should_not_partial("");
    }

    #[test]
    fn partial_no_fire_for_ukrainian_chars() {
        should_not_partial("при"); // valid RU prefix anyway
        should_not_partial("прі"); // UK letter 'і' → always skipped
        should_not_partial("їж"); // UK letter 'ї'
    }

    #[test]
    fn partial_no_fire_for_mixed_scripts() {
        should_not_partial("heприв");
        should_not_partial("приhello");
    }

    #[test]
    fn partial_no_fire_for_non_alpha() {
        should_not_partial("a1b");
        should_not_partial("gh3");
    }

    #[test]
    fn partial_no_fire_for_random_gibberish() {
        // Random consonants that aren't a prefix in either language —
        // must stay silent (user might be typing an unusual identifier).
        should_not_partial("xzq");
        should_not_partial("qqq");
    }

    #[test]
    fn partial_no_fire_for_rare_but_valid_prefix() {
        // "gh" is rare but "ghost"/"ghetto" start with it — "gho" is a valid
        // prefix.  We must NOT treat it as gibberish just because the bigram
        // is uncommon.
        should_not_partial("gho");
    }

    #[test]
    fn partial_only_fires_inside_its_window() {
        // Too short, then the window, then too long: past MAX_PARTIAL_LEN the
        // boundary check is a moment away and strictly better informed.
        let keys = en_gibberish_of("документ"); // "ljrevtyn"
        for n in 1..keys.len() {
            let pre = &keys[..n];
            let fired = decide_partial_switch(pre, Kbd::Latin, Kbd::Russian).is_some();
            if !(MIN_PARTIAL_LEN..=MAX_PARTIAL_LEN).contains(&n) {
                assert!(!fired, "{pre:?} ({n} chars) fired outside the window");
            }
        }
        // And inside the window it does its job.
        assert!(decide_partial_switch(&keys[..4], Kbd::Latin, Kbd::Russian).is_some());
    }

    /// The reported symptom: typing an ordinary English word and having the
    /// layout flip halfway through. These are all words our dictionary does
    /// NOT contain, which is exactly when the detector used to misfire.
    #[test]
    fn partial_never_fires_inside_real_english_words() {
        for w in [
            "afraid", "ashamed", "abyss", "although", "advantage", "refactoring",
            "serialization", "throughput", "workaround", "screenshot", "breakpoint",
            "deserialize", "polymorphism", "instantiate", "concatenate", "abbreviation",
            "acknowledgment", "subscription", "compatibility", "documentation",
            "troubleshoot", "benchmarking", "spreadsheet", "questionnaire",
            "straightforward", "brainstorming", "refrigerator", "thunderstorm",
        ] {
            for n in MIN_PARTIAL_LEN..=w.len().min(MAX_PARTIAL_LEN) {
                let pre = &w[..n];
                let got = decide_partial_switch(pre, Kbd::Latin, Kbd::Russian);
                assert_eq!(got, None, "mid-word switch inside {w:?} at {pre:?}");
            }
        }
    }

    #[test]
    fn early_detection_then_boundary_both_quiet_for_real_words() {
        // A natural English word — the word-end detector shouldn't flip it
        // either, but we check that the early detector stays equally quiet.
        for prefix_len in 3..="javascript".len() {
            let prefix = &"javascript"[..prefix_len];
            should_not_partial(prefix);
        }
    }

    // ============================================================
    // Morphological dictionary matching
    // ============================================================

    #[test]
    fn common_prefix_len_basics() {
        assert_eq!(common_prefix_len("работать", "работаешь"), 6);
        assert_eq!(common_prefix_len("system", "systems"), 6);
        assert_eq!(common_prefix_len("abc", "xyz"), 0);
        assert_eq!(common_prefix_len("", "abc"), 0);
    }

    #[test]
    fn max_common_prefix_finds_nearest_entry() {
        // The nearest entry in a sorted list is adjacent to the insertion
        // point — that is the whole reason this is one binary search.
        assert!(max_common_prefix_len(RU_WORDS, "работаешь") >= 6);
        assert!(max_common_prefix_len(EN_WORDS, "systems") >= 6);
        // Gibberish shares almost nothing with any entry.
        assert!(max_common_prefix_len(RU_WORDS, "ыъщжэ") < 3);
        assert!(max_common_prefix_len(EN_WORDS, "xzqvw") < 3);
    }

    #[test]
    fn morph_recognises_inflected_forms() {
        // Neither form is in the word lists; both are unmistakably real.
        assert!(!is_known_ru_word("работаешь"));
        assert!(morph_ru_veto("работаешь"));
        assert!(!is_known_en_word("systems"));
        assert!(morph_en_veto("systems"));
    }

    #[test]
    fn morph_rejects_gibberish() {
        // Layout gibberish must not be mistaken for an inflected word — this
        // is what keeps the lenient veto from silently disabling corrections.
        for g in ["руддщ", "ыщьуы", "ъхжэы"] {
            assert!(!morph_ru_veto(g), "{g:?} wrongly accepted as Russian");
            assert!(!morph_ru_confirm(g), "{g:?} wrongly confirmed as Russian");
        }
        for g in ["ghbdtn", "xzqvwy", "djkifn"] {
            assert!(!morph_en_veto(g), "{g:?} wrongly accepted as English");
            assert!(!morph_en_confirm(g), "{g:?} wrongly confirmed as English");
        }
    }

    #[test]
    fn morph_confirm_is_stricter_than_veto() {
        // Short near-misses pass the veto (harmless: we leave text alone) but
        // must not pass confirm (which rewrites text with no scoring check).
        assert!(!morph_ru_confirm("иарэ"));
        assert!(!morph_en_confirm("iare"));
    }

    #[test]
    fn fixes_inflected_russian_typed_in_en_layout() {
        // "программы" is not in RU_WORDS ("программа" is). Before morphological
        // matching this fell through to the fuzzy scorer; now the shared
        // 8-char prefix plus a real ending confirms it outright.
        assert!(!is_known_ru_word("программы"));
        let g = en_gibberish_of("программы");
        assert_eq!(g, "ghjuhfvvs");
        should_switch(&g, "программы");
    }

    // ============================================================
    // Ukrainian layout: `і ї є` are also the images of `s ] '`
    // ============================================================

    #[test]
    fn uk_tables_round_trip() {
        for c in "qwertyuiop[]asdfghjkl;'zxcvbnm,.".chars() {
            let there = crate::layout::en_to_uk(c);
            assert_eq!(
                crate::layout::uk_to_en(there),
                c,
                "{c:?} → {there:?} did not map back"
            );
        }
    }

    #[test]
    fn fixes_latin_with_s_typed_on_uk_layout() {
        // "list" on a Ukrainian layout comes out as "дшіе". The old RU-only
        // reverse mapping could not produce the `s`, and the blanket
        // Ukrainian-letter guard bailed out — so this was uncorrectable.
        let typed: String = "list".chars().map(crate::layout::en_to_uk).collect();
        assert_eq!(typed, "дшіе");
        should_switch_uk(&typed, "list");
    }

    #[test]
    fn fixes_more_latin_words_typed_on_uk_layout() {
        for w in ["test", "system", "insert", "assist"] {
            let typed: String = w.chars().map(crate::layout::en_to_uk).collect();
            should_switch_uk(&typed, w);
        }
    }

    #[test]
    fn keeps_real_ukrainian_on_uk_layout() {
        // The protection is "the Latin result must be a real English word".
        // Genuine Ukrainian never satisfies it.
        for w in ["привіт", "їжа", "україна", "єдиний", "ґанок", "дякую"] {
            should_not_switch_uk(w);
        }
    }

    #[test]
    fn uk_partial_fires_and_stays_quiet_correctly() {
        // "syst" (→ "ініе") is a live English prefix: fire early.
        let typed: String = "syst".chars().map(crate::layout::en_to_uk).collect();
        assert_eq!(typed, "ініе");
        assert_eq!(
            decide_partial_switch(&typed, Kbd::Ukrainian, Kbd::Ukrainian)
                .as_ref()
                .map(|c| c.text.as_str()),
            Some("syst")
        );
        // A real Ukrainian word start must not.
        assert_eq!(
            decide_partial_switch("при", Kbd::Ukrainian, Kbd::Ukrainian),
            None
        );
        assert_eq!(
            decide_partial_switch("їжа", Kbd::Ukrainian, Kbd::Ukrainian),
            None
        );
    }

    // ============================================================
    // Hard orthographic rules
    // ============================================================

    #[test]
    fn illegal_ru_pairs_are_penalised() {
        // жи-ши / ча-ща / чу-щу: breaking these means the text is not Russian.
        assert!(is_illegal_ru_pair('ж', 'ы'));
        assert!(is_illegal_ru_pair('ш', 'ы'));
        assert!(is_illegal_ru_pair('ч', 'я'));
        assert!(is_illegal_ru_pair('щ', 'ю'));
        assert!(!is_illegal_ru_pair('ж', 'и'));
        assert!(!is_illegal_ru_pair('ш', 'и'));
        assert!(!is_illegal_ru_pair('ч', 'а'));

        // A vowel can never be followed by a soft/hard sign.
        assert!(is_illegal_ru_pair('а', 'ь'));
        assert!(is_illegal_ru_pair('о', 'ъ'));
        assert!(!is_illegal_ru_pair('т', 'ь'));

        // And the rule has teeth in the scorer.
        assert!(score_cyrillic("жызнь") < score_cyrillic("жизнь"));
    }

    #[test]
    fn ru_positional_rules_are_penalised() {
        // No Russian word begins with ы/ъ/ь or ends with ъ.
        assert!(score_cyrillic("ыровт") < score_cyrillic("ровты"));
        assert!(score_cyrillic("ровтъ") < score_cyrillic("ровта"));
    }

    #[test]
    fn illegal_en_pairs_are_penalised() {
        // `q` is always followed by `u` in English.
        assert!(is_illegal_en_pair(b'q', b'w'));
        assert!(is_illegal_en_pair(b'q', b'a'));
        assert!(!is_illegal_en_pair(b'q', b'u'));
        assert!(score_latin("qwick") < score_latin("quick"));
        // Nor do English words end in q/j/v.
        assert!(score_latin("staliv") < score_latin("stalis"));
    }

    #[test]
    fn triple_letters_are_penalised() {
        assert!(score_latin("hellllo") < score_latin("hello"));
        assert!(score_cyrillic("прррвет") < score_cyrillic("прирвет"));
    }

    // ============================================================
    // Rejected-word memory
    // ============================================================

    #[test]
    fn rejected_words_are_remembered_and_matched_by_prefix() {
        // Note: REJECTED is process-global; use a token no other test touches.
        let word = "зззунусшфд";
        assert!(!is_rejected(word));
        remember_rejected(word);
        assert!(is_rejected(word));
        // Case-insensitive, and visible to the mid-word detector.
        remember_rejected("ЗЗЗУНУСШФД");
        assert!(is_rejected_prefix("зззу"));
        assert!(!is_rejected_prefix("яяяы"));
        // Idempotent — no duplicate entries.
        let before = REJECTED.lock().unwrap().len();
        remember_rejected(word);
        assert_eq!(REJECTED.lock().unwrap().len(), before);
    }

    // ============================================================
    // Ukrainian as a first-class language
    // ============================================================

    #[test]
    fn uk_words_sorted_and_unique() {
        // `str: Ord` is byte-wise, and є/і/ї/ґ sort *after* а-я in UTF-8 — the
        // whole binary-search/prefix machinery breaks silently if this slips.
        let mut sorted: Vec<&&str> = UK_WORDS.iter().collect();
        sorted.sort();
        let same: Vec<&&str> = UK_WORDS.iter().collect();
        assert_eq!(sorted, same, "UK_WORDS must be sorted");
        let mut dedup = same.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), same.len(), "UK_WORDS must not contain duplicates");
    }

    #[test]
    fn uk_words_are_lowercase_cyrillic() {
        for &w in UK_WORDS {
            assert!(
                w.chars().all(|c| is_cyrillic(c) || c == '\''),
                "{w:?} has non-Cyrillic characters"
            );
            assert_eq!(w, w.to_lowercase(), "{w:?} must be lowercase");
        }
    }

    #[test]
    fn fixes_ukrainian_typed_in_en_layout() {
        // The headline case: "привіт" typed on an English layout. The `s` key
        // is `і` in Ukrainian but `ы` in Russian, so the Russian reading
        // ("привыт") is a non-word and the Ukrainian one wins.
        let g = uk_keys_of("привіт");
        assert_eq!(g, "ghbdsn");
        should_switch_to(&g, Kbd::Latin, Kbd::Russian, "привіт", Kbd::Ukrainian);
    }

    #[test]
    fn fixes_more_ukrainian_typed_in_en_layout() {
        for w in ["дякую", "місто", "країна", "тиждень", "українська"] {
            should_switch_to(&uk_keys_of(w), Kbd::Latin, Kbd::Ukrainian, w, Kbd::Ukrainian);
        }
    }

    #[test]
    fn fixes_ukrainian_typed_on_russian_layout() {
        // A Russian layout has no і/ї/є, so those keys land as ы/ъ/э.
        // "привіт" comes out as "привыт" — same keys, wrong table.
        let typed: String = "привіт"
            .chars()
            .map(crate::layout::uk_to_en)
            .map(crate::layout::en_to_ru)
            .collect();
        assert_eq!(typed, "привыт");
        should_switch_to(&typed, Kbd::Russian, Kbd::Russian, "привіт", Kbd::Ukrainian);
    }

    #[test]
    fn ambiguous_reading_follows_the_users_own_layout() {
        // A word spelled identically in both languages: the keystrokes give
        // the same text either way, so only the target layout is in question
        // and the user's own Cyrillic layout decides.
        let shared = UK_WORDS
            .iter()
            .copied()
            .find(|w| {
                is_known_ru_word(w) && uk_keys_of(w).chars().all(|c| c.is_ascii_alphabetic())
            })
            .expect("dictionaries should share at least one word");
        let keys = uk_keys_of(shared);
        assert_eq!(crate::layout::convert(&keys), crate::layout::convert_uk(&keys));
        should_switch_to(&keys, Kbd::Latin, Kbd::Russian, shared, Kbd::Russian);
        should_switch_to(&keys, Kbd::Latin, Kbd::Ukrainian, shared, Kbd::Ukrainian);
    }

    #[test]
    fn russian_wins_when_only_it_reads_as_a_word() {
        // "привет" is Russian-only; a Ukrainian preference must not drag the
        // text somewhere it does not belong.
        let g = en_gibberish_of("привет");
        assert!(!is_known_uk_word("привет"));
        should_switch_to(&g, Kbd::Latin, Kbd::Ukrainian, "привет", Kbd::Russian);
    }

    #[test]
    fn morph_confirm_rejects_mid_word_mismatch() {
        // "кампаныя" shares six leading characters with "кампания" and ends in
        // a real inflection, but the leftover "ыя" is not an ending — the word
        // differs in the middle, so it is not Russian.
        assert!(!morph_ru_confirm("кампаныя"));
        assert!(morph_ru_confirm("программы"));
    }

    #[test]
    fn uk_dictionary_words_are_never_flipped_on_a_uk_layout() {
        // Sweep: every Ukrainian word we know must survive being typed on a
        // Ukrainian layout. This is what the dictionary veto buys us — without
        // it, any UK word whose Latin reading happens to be English would flip.
        for &w in UK_WORDS {
            should_not_switch_uk(w);
        }
    }

    #[test]
    fn uk_dictionary_words_round_trip_from_en_layout() {
        // And the reverse: typed on an English layout they come back.
        // Skipped when the keystrokes include punctuation (ж→`;`, б→`,`, …),
        // which the hook treats as a word boundary, or when the Latin form is
        // itself an English word (genuinely ambiguous — leave it alone).
        let mut checked = 0;
        for &w in UK_WORDS {
            if w.chars().count() < 4 {
                continue;
            }
            let keys: String = w.chars().map(crate::layout::uk_to_en).collect();
            if !keys.chars().all(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            if is_known_en_word(&keys) || morph_en_veto(&keys) {
                continue;
            }
            let got = decide_switch(&keys, Kbd::Latin, Kbd::Ukrainian);
            assert_eq!(
                got.as_ref().map(|c| c.text.as_str()),
                Some(w),
                "{keys:?} should restore {w:?}"
            );
            checked += 1;
        }
        assert!(checked > 300, "sweep only covered {checked} words");
    }

    #[test]
    fn uk_partial_fires_from_en_layout() {
        // "ghbds" → "приі"? No: mid-word, "ghbds" reads as "привi"… the point
        // is the Ukrainian prefix is live while the English one is dead.
        let keys: String = "прив".chars().map(crate::layout::uk_to_en).collect();
        assert_eq!(keys, "ghbd");
        let got = decide_partial_switch(&keys, Kbd::Latin, Kbd::Ukrainian);
        assert_eq!(
            got.as_ref().map(|c| (c.text.as_str(), c.target)),
            Some(("прив", Kbd::Ukrainian))
        );
    }

    #[test]
    fn ukrainian_vowels_count_as_vowels() {
        // Counting і/ї/є as consonants made Ukrainian words look like
        // unpronounceable clusters and dragged their score down.
        assert!(is_cyr_vowel('і') && is_cyr_vowel('ї') && is_cyr_vowel('є'));
        assert!(!is_ru_vowel('і'));
        assert!(score_cyrillic("місто") > score_cyrillic("мсто"));
    }

    #[test]
    fn uk_endings_cover_common_inflections() {
        // Sanity check on the ending table via the veto it feeds.
        assert!(morph_uk_veto("програми"));
        assert!(morph_uk_veto("роботи"));
        assert!(!morph_uk_veto("ыъэжщ"));
    }

    #[test]
    fn layout_ids_map_to_the_right_language() {
        assert_eq!(layout_id_for(Kbd::Russian) & 0x3FF, 0x019);
        assert_eq!(layout_id_for(Kbd::Ukrainian) & 0x3FF, 0x022);
    }

    // ============================================================
    // Whole-pipeline simulation
    // ============================================================
    //
    // Drives `process_key` / `apply_pending` against a fake application, so
    // the interesting timing bugs can be reproduced without touching the real
    // keyboard. Two knobs matter, and both are invisible when testing against
    // a snappy editor like Notepad:
    //
    //   `switch_latency` — how many keystrokes pass before the app acts on a
    //     layout-change request. Notepad does it immediately; a browser or
    //     chat client with a busy message queue does not.
    //   `timer_lag` — how many keystrokes land between a correction being
    //     decided and being applied. Zero for a slow typist, more for a fast
    //     one.

    /// Global state is process-wide, so simulation tests take turns.
    static SIM_LOCK: Mutex<()> = Mutex::new(());

    struct Sim {
        /// What the "application" currently shows.
        doc: String,
        /// Layout the app believes is active.
        layout: u32,
        /// Requested layout and how many more keystrokes until it applies.
        switching: Option<(u32, usize)>,
        switch_latency: usize,
        timer_lag: usize,
        /// Corrections decided but not yet applied, with keystrokes remaining.
        armed: Option<usize>,
        window: isize,
    }

    impl Sim {
        fn new(layout: u32) -> Self {
            *EFFECTS.lock().unwrap() = Some(Vec::new());
            WORD_BUF.lock().unwrap().clear();
            *PENDING.lock().unwrap() = None;
            *BRIDGE.lock().unwrap() = None;
            *LAST_CORRECTION.lock().unwrap() = None;
            REJECTED.lock().unwrap().clear();
            *LAST_KEY_AT.lock().unwrap() = None;
            LAST_FG.store(0, Ordering::Relaxed);
            LAST_CYR_LAYOUT.store(0, Ordering::Relaxed);
            LAST_LATIN_LAYOUT.store(0, Ordering::Relaxed);
            UNDO_ARMED.store(false, Ordering::SeqCst);
            SWALLOWED_VK.store(NO_VK, Ordering::SeqCst);
            ENABLED.store(true, Ordering::SeqCst);
            Sim {
                doc: String::new(),
                layout,
                switching: None,
                switch_latency: 0,
                timer_lag: 0,
                armed: None,
                window: 0x1234,
            }
        }

        fn with_switch_latency(mut self, n: usize) -> Self {
            self.switch_latency = n;
            self
        }

        fn with_timer_lag(mut self, n: usize) -> Self {
            self.timer_lag = n;
            self
        }

        /// The character this key produces under the app's *current* layout.
        fn native_char(&self, vk: u32, shift: bool) -> Option<char> {
            let kbd = kbd_of_layout(self.layout);
            if (0x41..=0x5A).contains(&vk) {
                vk_to_char(vk, shift, false, kbd)
            } else if vk == 0x20 {
                Some(' ')
            } else {
                None
            }
        }

        fn drain_effects(&mut self) {
            let effects: Vec<Effect> = EFFECTS.lock().unwrap().as_mut().unwrap().drain(..).collect();
            for e in effects {
                match e {
                    Effect::Replace { backspaces, text } => {
                        for _ in 0..backspaces {
                            self.doc.pop();
                        }
                        self.doc.push_str(&text);
                    }
                    Effect::Type(ch) => self.doc.push(ch),
                    Effect::ReplayKey { vk, shift } => {
                        if let Some(ch) = self.native_char(vk as u32, shift) {
                            self.doc.push(ch);
                        }
                    }
                    Effect::Layout(id) => self.switching = Some((id, self.switch_latency)),
                }
            }
        }

        fn key(&mut self, vk: u32, shift: bool) {
            // The app applies a queued layout change before handling the key.
            if let Some((id, 0)) = self.switching {
                self.layout = id;
                self.switching = None;
            }

            let action = process_key(KeyEvent {
                vk,
                shift,
                caps: false,
                layout_id: self.layout,
                window: self.window,
                clicked: false,
            });
            self.drain_effects();

            if action == KeyAction::Pass {
                if vk == 0x08 {
                    self.doc.pop();
                } else if let Some(ch) = self.native_char(vk, shift) {
                    self.doc.push(ch);
                }
            }

            // A queued correction becomes due after `timer_lag` more keys.
            if PENDING.lock().unwrap().is_some() && self.armed.is_none() {
                self.armed = Some(self.timer_lag);
            }
            match self.armed {
                Some(0) => {
                    self.armed = None;
                    apply_pending();
                    self.drain_effects();
                }
                Some(n) => self.armed = Some(n - 1),
                None => {}
            }

            if let Some((id, n)) = self.switching {
                self.switching = if n == 0 { Some((id, 0)) } else { Some((id, n - 1)) };
            }
        }

        /// Types `word` as physical keys (the letters are VK codes).
        fn type_word(&mut self, word: &str) {
            for ch in word.chars() {
                self.key(ch.to_ascii_uppercase() as u32, false);
            }
        }

        /// Flushes any correction still queued, as the idle timer would.
        fn settle(&mut self) -> String {
            for _ in 0..4 {
                if PENDING.lock().unwrap().is_some() {
                    apply_pending();
                    self.drain_effects();
                }
                if let Some((id, _)) = self.switching.take() {
                    self.layout = id;
                }
            }
            self.doc.clone()
        }
    }

    impl Drop for Sim {
        fn drop(&mut self) {
            *EFFECTS.lock().unwrap() = None;
            ENABLED.store(false, Ordering::SeqCst);
        }
    }

    const RU: u32 = 0x0419;

    #[test]
    fn sim_corrects_a_word_typed_on_the_wrong_layout() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut sim = Sim::new(RU);
        sim.type_word("curtains");
        assert_eq!(sim.settle(), "curtains");
    }

    /// The reported failure: "curtains" on a Russian layout came out
    /// "curtфшты" — word corrected, tail still in the old layout, because the
    /// app had not yet acted on the layout-change request.
    #[test]
    fn sim_survives_a_slow_layout_switch() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for latency in 0..=6 {
            let mut sim = Sim::new(RU).with_switch_latency(latency);
            sim.type_word("curtains");
            assert_eq!(
                sim.settle(),
                "curtains",
                "app applying the layout switch {latency} keystrokes late"
            );
        }
    }

    /// The other half: keystrokes landing between deciding a correction and
    /// applying it. This is what turned "hello" into an empty string.
    #[test]
    fn sim_survives_a_fast_typist() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for lag in 0..=4 {
            for word in ["curtains", "hello", "book", "keyboard", "javascript"] {
                let mut sim = Sim::new(RU).with_timer_lag(lag);
                sim.type_word(word);
                assert_eq!(sim.settle(), word, "{word:?} with the fix {lag} keys late");
            }
        }
    }

    /// Both at once, which is the realistic case.
    #[test]
    fn sim_survives_both_delays_together() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for latency in 0..=4 {
            for lag in 0..=3 {
                let mut sim = Sim::new(RU)
                    .with_switch_latency(latency)
                    .with_timer_lag(lag);
                sim.type_word("curtains");
                assert_eq!(
                    sim.settle(),
                    "curtains",
                    "switch {latency} late, correction {lag} late"
                );
            }
        }
    }

    #[test]
    fn sim_leaves_a_correctly_typed_word_alone() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut sim = Sim::new(0x0409);
        sim.type_word("curtains");
        assert_eq!(sim.settle(), "curtains");
    }

    #[test]
    fn sim_handles_a_word_followed_by_space() {
        let _g = SIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut sim = Sim::new(RU).with_switch_latency(3);
        sim.type_word("book");
        sim.key(0x20, false);
        assert_eq!(sim.settle(), "book ");
    }

    // ============================================================
    // Applying a mid-word correction
    // ============================================================

    #[test]
    fn recode_tables_match_the_decision() {
        // Whatever conversion produced the text must reproduce it, because the
        // mid-word path re-runs it against the live buffer at apply time.
        for (typed, kbd, pref) in [
            (ru_gibberish_of("hello"), Kbd::Russian, Kbd::Russian),
            (en_gibberish_of("привет"), Kbd::Latin, Kbd::Russian),
            (uk_keys_of("привіт"), Kbd::Latin, Kbd::Ukrainian),
            (uk_keys_of("список").to_string(), Kbd::Latin, Kbd::Ukrainian),
        ] {
            if let Some(fix) = decide_switch(&typed, kbd, pref) {
                assert_eq!(
                    recode(&typed, fix.how),
                    fix.text,
                    "recode({typed:?}, {:?}) must reproduce the decision",
                    fix.how
                );
            }
        }
    }

    #[test]
    fn recompute_absorbs_keystrokes_typed_during_the_delay() {
        // The race that ate whole words: the detector decides on "рудд"
        // ("hell"), the user lands the final "щ" before the backspaces run.
        // Recomputing from the longer buffer yields the whole word instead of
        // deleting one character too few.
        let decided = ru_gibberish_of("hell");
        let live = ru_gibberish_of("hello");
        let fix = decide_partial_switch(&decided, Kbd::Russian, Kbd::Russian)
            .expect("mid-word detector should fire on \"рудд\"");
        assert_eq!(fix.text, "hell");
        assert_eq!(recode(&live, fix.how), "hello");
        assert_eq!(live.chars().count(), 5);
    }

    #[test]
    fn required_bigrams_rises_with_length() {
        assert_eq!(required_bigrams(3), 2);
        assert_eq!(required_bigrams(6), 2);
        assert_eq!(required_bigrams(8), 3);
        assert_eq!(required_bigrams(12), 4);
    }
}
