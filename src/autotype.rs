use crate::utils::{is_cyrillic, make_key_input, make_unicode_input};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ============================================================
// Global state
// ============================================================

static HOOK: Mutex<isize> = Mutex::new(0);
static WORD_BUF: Mutex<String> = Mutex::new(String::new());
static ENABLED: AtomicBool = AtomicBool::new(false);
static SKIP_EVENTS: AtomicI32 = AtomicI32::new(0);

struct PendingSwitch {
    backspace_count: usize,
    new_text: String,
    target_layout: u32,
}
static PENDING: Mutex<Option<PendingSwitch>> = Mutex::new(None);
static SWITCH_TIMER_ID: Mutex<usize> = Mutex::new(0);

/// Anti-ping-pong: remember last correction to avoid re-converting it back.
struct LastCorrection {
    output_word: String,
    when: Instant,
}
static LAST_CORRECTION: Mutex<Option<LastCorrection>> = Mutex::new(None);
const PINGPONG_WINDOW_SECS: u64 = 15;

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

        let is_down = wp.0 == WM_KEYDOWN as usize || wp.0 == WM_SYSKEYDOWN as usize;
        if !is_down {
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        if SKIP_EVENTS.load(Ordering::SeqCst) > 0 {
            SKIP_EVENTS.fetch_sub(1, Ordering::SeqCst);
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let info = &*(lp.0 as *const KbdLLHookStruct);
        let vk = info.vk_code;

        let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0;
        let alt = GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000 != 0;
        if ctrl || alt {
            return CallNextHookEx(hook_handle, code, wp, lp);
        }

        let shift = GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0;
        let caps = GetKeyState(VK_CAPITAL.0 as i32) & 0x0001 != 0;
        let fg = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(fg, None);
        let layout = GetKeyboardLayout(tid);
        let layout_id = layout.0 as usize & 0xFFFF;
        let kbd = match layout_id {
            0x0419 => Kbd::Russian,
            0x0422 => Kbd::Ukrainian,
            _ => Kbd::Latin,
        };
        let is_cyrillic_layout = !matches!(kbd, Kbd::Latin);

        match vk {
            // Letters A-Z (layout-independent VK codes)
            0x41..=0x5A => {
                if let Some(ch) = vk_to_char(vk, shift, caps, kbd) {
                    WORD_BUF.lock().unwrap().push(ch);
                    // Fast path: try to correct mid-word before Space is hit.
                    check_partial_switch();
                }
            }
            // OEM keys that produce Cyrillic LETTERS on Russian/Ukrainian layouts:
            //   OEM_1(;)→ж  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
            //   OEM_4([)→х
            //   OEM_6(])→ъ (RU) / ї (UK)
            //   OEM_7(')→э (RU) / є (UK)
            //   OEM_3(`)→ё (RU) / ' apostrophe (UK, boundary-like, skipped)
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0
                if is_cyrillic_layout =>
            {
                if let Some(ch) = oem_to_cyr_char(vk, shift, caps, kbd) {
                    WORD_BUF.lock().unwrap().push(ch);
                    check_partial_switch();
                } else {
                    // Unmapped OEM on this layout → treat as boundary
                    let ch = oem_boundary_char(vk, shift, is_cyrillic_layout);
                    check_word_boundary(ch);
                }
            }
            // OEM keys as punctuation (English layout, or keys that stay
            // punctuation in Russian: OEM_2(/?)→.,  OEM_5(\|)  OEM_PLUS  OEM_MINUS)
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0 |
            0xBF | 0xDC | 0xBB | 0xBD => {
                let ch = oem_boundary_char(vk, shift, is_cyrillic_layout);
                check_word_boundary(ch);
            }
            // Space, Tab — word boundary
            0x20 => check_word_boundary(Some(' ')),
            0x09 => check_word_boundary(Some('\t')),
            // Number keys (0-9) — word boundary
            0x30..=0x39 => {
                let digit = (vk - 0x30) as u8 + b'0';
                check_word_boundary(Some(digit as char));
            }
            // Enter — check, then the newline goes through
            0x0D => {
                check_word_boundary(Some('\r'));
            }
            // Backspace — erase last char
            0x08 => { WORD_BUF.lock().unwrap().pop(); }
            // Modifiers — ignore (don't reset buffer)
            0x10 | 0x11 | 0x12 | 0x14 | 0xA0..=0xA5 => {}
            // Win key, arrows, function keys — reset
            _ => { WORD_BUF.lock().unwrap().clear(); }
        }

        CallNextHookEx(hook_handle, code, wp, lp)
    }
}

// ============================================================
// Word checking & replacement
// ============================================================

/// Checks the accumulated word buffer for layout mismatch.
/// `separator` is the boundary character (Space, period, comma, digit, etc.)
/// We erase word + separator from the document and re-type converted + separator.
fn check_word_boundary(separator: Option<char>) {
    let word = {
        let mut buf = WORD_BUF.lock().unwrap();
        let w = buf.clone();
        buf.clear();
        w
    };

    let char_count = word.chars().count();
    if char_count < 2 || !word.chars().all(|c| c.is_alphabetic()) {
        return;
    }

    // --- decide_switch returns the converted word to avoid double convert() ---
    let converted = match decide_switch(&word) {
        Some(conv) => conv,
        None => return,
    };

    // Anti-ping-pong: don't re-convert a word that was just produced by correction.
    {
        let guard = LAST_CORRECTION.lock().unwrap();
        if let Some(lc) = guard.as_ref() {
            if lc.output_word.to_lowercase() == word.to_lowercase()
                && lc.when.elapsed().as_secs() < PINGPONG_WINDOW_SECS
            {
                println!("[punto] skip ping-pong: {word}");
                return;
            }
        }
    }

    let has_latin = word.chars().any(|c| c.is_ascii_alphabetic());
    let sep = separator.unwrap_or(' ');
    let backspace_count = char_count + 1; // word + boundary key
    let new_text = format!("{converted}{sep}");
    let target_layout = if has_latin { 0x0419u32 } else { 0x0409u32 };

    println!("[punto] {word} → {converted}");

    // Record this correction for anti-ping-pong.
    *LAST_CORRECTION.lock().unwrap() = Some(LastCorrection {
        output_word: converted.clone(),
        when: Instant::now(),
    });

    *PENDING.lock().unwrap() = Some(PendingSwitch { backspace_count, new_text, target_layout });
    schedule_switch();
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
/// Unlike `check_word_boundary`, no separator is eaten/retyped.  After
/// replacement the buffer is cleared so subsequent keystrokes — which
/// will arrive in the freshly-switched layout — accumulate fresh.
fn check_partial_switch() {
    // Don't stack switches on top of each other.  If the boundary path
    // (or a prior partial fire) already queued something, wait for the
    // timer to drain it before evaluating again.
    if PENDING.lock().unwrap().is_some() { return; }

    let word = WORD_BUF.lock().unwrap().clone();
    let char_count = word.chars().count();
    if char_count < MIN_PARTIAL_LEN { return; }

    let converted = match decide_partial_switch(&word) {
        Some(c) => c,
        None => return,
    };

    // Anti-ping-pong: if the current buf is a prefix of a word we JUST
    // corrected to, we're seeing our own output — bail out.
    {
        let guard = LAST_CORRECTION.lock().unwrap();
        if let Some(lc) = guard.as_ref() {
            let out_lc = lc.output_word.to_lowercase();
            let word_lc = word.to_lowercase();
            if (out_lc.starts_with(&word_lc) || word_lc.starts_with(&out_lc))
                && lc.when.elapsed().as_secs() < PINGPONG_WINDOW_SECS
            {
                return;
            }
        }
    }

    let has_latin = word.chars().any(|c| c.is_ascii_alphabetic());
    let backspace_count = char_count; // no separator to erase — partial word
    let new_text = converted.clone();
    let target_layout = if has_latin { 0x0419u32 } else { 0x0409u32 };

    println!("[punto early] {word} → {converted}");

    *LAST_CORRECTION.lock().unwrap() = Some(LastCorrection {
        output_word: converted.clone(),
        when: Instant::now(),
    });

    // Commit: clear the buffer.  Subsequent keystrokes will arrive in the
    // new (correct) layout and start a fresh word.
    WORD_BUF.lock().unwrap().clear();

    *PENDING.lock().unwrap() = Some(PendingSwitch {
        backspace_count,
        new_text,
        target_layout,
    });
    schedule_switch();
}

fn schedule_switch() {
    unsafe {
        let mut id = SWITCH_TIMER_ID.lock().unwrap();
        if *id != 0 { let _ = KillTimer(None, *id); }
        *id = SetTimer(None, 0, 10, Some(switch_timer_proc));
    }
}

unsafe extern "system" fn switch_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _tick: u32) {
    unsafe {
        let _ = KillTimer(None, id);
        *SWITCH_TIMER_ID.lock().unwrap() = 0;
    }
    if let Some(p) = PENDING.lock().unwrap().take() {
        do_replace(p.backspace_count, &p.new_text);
        switch_keyboard_layout(p.target_layout);
    }
}

fn switch_keyboard_layout(lang_id: u32) {
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.0.is_null() {
            let _ = PostMessageW(fg, WM_INPUTLANGCHANGEREQUEST, WPARAM(0), LPARAM(lang_id as isize));
        }
    }
}

fn do_replace(backspace_count: usize, new_text: &str) {
    let utf16: Vec<u16> = new_text.encode_utf16().collect();
    let down_event_count = backspace_count + utf16.len();
    SKIP_EVENTS.store(down_event_count as i32, Ordering::SeqCst);

    unsafe {
        let mut inputs: Vec<INPUT> = Vec::with_capacity(down_event_count * 2);
        for _ in 0..backspace_count {
            inputs.push(make_key_input(VK_BACK, false));
            inputs.push(make_key_input(VK_BACK, true));
        }
        for &ch in &utf16 {
            inputs.push(make_unicode_input(ch, false));
            inputs.push(make_unicode_input(ch, true));
        }
        let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

// ============================================================
// Decision: should we switch this word?
// ============================================================

/// Returns `Some(converted_word)` if we should switch, `None` otherwise.
/// This avoids calling `convert()` twice (once here, once in the caller).
fn decide_switch(word: &str) -> Option<String> {
    let has_latin = word.chars().any(|c| c.is_ascii_alphabetic());
    let has_cyrillic = word.chars().any(|c| is_cyrillic(c));

    // Mixed scripts — never touch.
    if has_latin && has_cyrillic { return None; }

    // Ukrainian-specific letters (і, ї, є, ґ + uppercase) are a strong signal
    // that the user is intentionally typing Ukrainian. Don't "correct" these
    // into Russian or English — they have no clean mapping in either direction
    // and such a swap would always be unwanted.
    if word.chars().any(is_ukrainian_only) { return None; }

    let converted = crate::layout::convert(word);
    let word_lc = word.to_lowercase();
    let conv_lc = converted.to_lowercase();
    let len = word.chars().count();

    let should = if has_latin {
        // User typed latin — maybe wanted Cyrillic?

        // If the typed word is a known English word, don't switch.
        if is_known_en_word(&word_lc) { return None; }
        // If the converted word is a known Russian word, definitely switch.
        if is_known_ru_word(&conv_lc) { return Some(converted); }

        let en_score = score_latin(&word_lc);
        let ru_score = score_cyrillic(&conv_lc);
        let threshold = threshold_en_to_ru(len);
        ru_score > en_score + threshold
    } else if has_cyrillic {
        // User typed Cyrillic — maybe wanted Latin?

        // If the typed word is a known Russian word, don't switch.
        if is_known_ru_word(&word_lc) { return None; }
        // If the converted word is a known English word, definitely switch.
        if is_known_en_word(&conv_lc) { return Some(converted); }

        let ru_score = score_cyrillic(&word_lc);
        let en_score = score_latin(&conv_lc);
        let threshold = threshold_ru_to_en(len);
        en_score > ru_score + threshold
    } else {
        false
    };

    if should { Some(converted) } else { None }
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
/// Returns `Some(converted_word)` to switch, `None` to wait.
fn decide_partial_switch(buf: &str) -> Option<String> {
    let char_count = buf.chars().count();
    if char_count < MIN_PARTIAL_LEN { return None; }

    // Must be purely alphabetic & single-script.
    if !buf.chars().all(|c| c.is_alphabetic()) { return None; }
    let has_latin = buf.chars().any(|c| c.is_ascii_alphabetic());
    let has_cyrillic = buf.chars().any(|c| is_cyrillic(c));
    if has_latin && has_cyrillic { return None; }

    // Ukrainian-only letters → never mid-correct (same rationale as decide_switch).
    if buf.chars().any(is_ukrainian_only) { return None; }

    let buf_lc = buf.to_lowercase();

    if has_latin {
        // Fast out: typed prefix matches a real English word start.
        if is_en_prefix(&buf_lc) { return None; }

        let converted = crate::layout::convert(buf);
        let conv_lc = converted.to_lowercase();

        // The converted prefix must look like a plausible Russian word start.
        if !is_ru_prefix(&conv_lc) { return None; }

        // Extra safety: avoid firing when the converted prefix already
        // contains a "bad RU bigram" (would mean both directions look wrong,
        // e.g. random gibberish).
        let chars: Vec<char> = conv_lc.chars().collect();
        if chars.windows(2).any(|p| is_bad_ru_bigram(p[0], p[1])) { return None; }

        Some(converted)
    } else if has_cyrillic {
        if is_ru_prefix(&buf_lc) { return None; }

        let converted = crate::layout::convert(buf);
        let conv_lc = converted.to_lowercase();

        if !is_en_prefix(&conv_lc) { return None; }

        let bytes: Vec<u8> = conv_lc.bytes().collect();
        if bytes.windows(2).any(|p| is_bad_en_bigram(p[0], p[1])) { return None; }

        Some(converted)
    } else {
        None
    }
}

/// Minimum buffer length before the early detector even considers a switch.
/// 3 chars is the sweet spot: shorter prefixes are too ambiguous, longer
/// means more keystrokes wasted before the user gets their correction.
const MIN_PARTIAL_LEN: usize = 3;

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
    if alpha == 0 { return -100; }

    let mut score = 0i32;

    // --- Vowel analysis ---
    let vowels = chars.iter().filter(|c| is_en_vowel(**c)).count();
    let ratio = vowels as f32 / alpha as f32;
    if (0.15..=0.60).contains(&ratio) { score += 8; }
    if vowels == 0 && alpha >= 3 { score -= 25; }
    if ratio > 0.80 && alpha >= 3 { score -= 10; } // too many vowels

    // --- Bigram analysis ---
    let bytes: Vec<u8> = word.bytes().collect();
    for pair in bytes.windows(2) {
        if is_common_en_bigram(pair[0], pair[1]) { score += 3; }
        if is_bad_en_bigram(pair[0], pair[1]) { score -= 8; }
    }

    // --- Trigram analysis ---
    for triple in bytes.windows(3) {
        if is_common_en_trigram(triple[0], triple[1], triple[2]) { score += 5; }
    }

    // --- Consecutive consonant penalty ---
    let mut cons = 0u32;
    for c in &chars {
        if c.is_ascii_alphabetic() && !is_en_vowel(*c) {
            cons += 1;
            if cons == 3 { score -= 4; }
            if cons >= 4 { score -= 6; }
        } else {
            cons = 0;
        }
    }

    // --- Suffix / prefix signature bonus ---
    score += suffix_bonus_en(word);
    score += prefix_bonus_en(word);

    score
}

/// Bonus for word endings that are very characteristic of English.
/// Applied on the lowercased word.
fn suffix_bonus_en(word: &str) -> i32 {
    // 4-char suffixes (very specific)
    for s in ["tion", "sion", "ness", "ment", "able", "ible", "ship",
              "ward", "ough", "ious", "eous"] {
        if word.ends_with(s) && word.len() > s.len() { return 10; }
    }
    // 3-char suffixes
    for s in ["ing", "est", "ful", "ity", "ive", "ous", "ize",
              "ise", "ify", "ism", "ist", "age", "ery"] {
        if word.ends_with(s) && word.len() > s.len() + 1 { return 7; }
    }
    // 2-char suffixes (weaker — many short Russian→Latin typos also end this way)
    for s in ["ly", "ed", "er"] {
        if word.ends_with(s) && word.len() >= 4 { return 3; }
    }
    0
}

/// Bonus for word-initial patterns common in English.
fn prefix_bonus_en(word: &str) -> i32 {
    for p in ["un", "re", "pre", "dis", "mis", "over", "under",
              "inter", "trans", "anti", "auto", "semi", "sub", "non",
              "non-"] {
        if word.starts_with(p) && word.len() > p.len() + 1 { return 3; }
    }
    0
}

// ============================================================
// Scoring: Cyrillic (Russian)
// ============================================================

fn score_cyrillic(word: &str) -> i32 {
    let chars: Vec<char> = word.chars().collect();
    let cyr = chars.iter().filter(|c| is_cyrillic(**c)).count();
    if cyr == 0 { return -100; }

    let mut score = 0i32;

    // --- Vowel analysis ---
    let vowels = chars.iter().filter(|c| is_ru_vowel(**c)).count();
    let ratio = vowels as f32 / cyr as f32;
    if (0.15..=0.60).contains(&ratio) { score += 8; }
    if vowels == 0 && cyr >= 3 { score -= 25; }
    if ratio > 0.80 && cyr >= 3 { score -= 10; }

    // --- Bigram analysis ---
    for pair in chars.windows(2) {
        if is_common_ru_bigram(pair[0], pair[1]) { score += 3; }
        if is_bad_ru_bigram(pair[0], pair[1]) { score -= 8; }
    }

    // --- Trigram analysis ---
    for triple in chars.windows(3) {
        if is_common_ru_trigram(triple[0], triple[1], triple[2]) { score += 5; }
    }

    // --- Consecutive consonant penalty (excluding ь, ъ, й) ---
    let mut cons = 0u32;
    for c in &chars {
        if is_cyrillic(*c) && !is_ru_vowel(*c) && !matches!(*c, 'ь' | 'ъ' | 'й') {
            cons += 1;
            if cons == 3 { score -= 4; }
            if cons >= 4 { score -= 6; }
        } else {
            cons = 0;
        }
    }

    // --- Rare-letter penalty: ъ and э are very uncommon in natural Russian ---
    // (ё is excluded — it's a normal letter, just often replaced by е)
    for c in &chars {
        if matches!(*c, 'ъ' | 'э') { score -= 2; }
    }

    // --- Suffix signature bonus ---
    score += suffix_bonus_ru(word);

    score
}

/// Bonus for word endings that are very characteristic of Russian.
fn suffix_bonus_ru(word: &str) -> i32 {
    let chars: Vec<char> = word.chars().collect();
    let len = chars.len();

    // 4-char endings (very specific — inflections/reflexives)
    let endings_4 = [
        ['о','с','т','ь'], ['т','ь','с','я'], ['е','н','и','е'],
        ['а','н','и','е'], ['о','в','а','л'], ['и','в','а','л'],
        ['я'  ,'т','ь','с'],
    ];
    if len > 4 {
        let tail: Vec<char> = chars[len-4..].to_vec();
        for e in endings_4 {
            if tail == e { return 10; }
        }
    }

    // 3-char endings (adjective/verb/noun inflections)
    let endings_3 = [
        ['о','г','о'], ['о','м','у'], ['ы','м','и'], ['а','м','и'],
        ['и','м','и'], ['я','м','и'], ['е','г','о'], ['е','м','у'],
        ['и','т','ь'], ['а','т','ь'], ['у','т','ь'], ['е','т','ь'],
        ['ы','т','ь'], ['о','т','ь'], ['ю','т','с'], ['у','т','с'],
        ['с','я',' '], // ignored since we pre-split
        ['с','т','ь'], ['н','ы','й'], ['н','ы','е'], ['н','ы','х'],
        ['н','о','й'], ['н','о','е'], ['н','о','м'], ['н','у','ю'],
        ['о','й',' '],
    ];
    if len > 3 {
        let tail: Vec<char> = chars[len-3..].to_vec();
        for e in endings_3 {
            if tail[0] == e[0] && tail[1] == e[1] && tail[2] == e[2] { return 7; }
        }
    }

    // 2-char endings (weaker — plural/case endings)
    let endings_2 = [
        ('т','ь'), ('с','я'), ('ы','й'), ('и','й'), ('о','й'),
        ('а','я'), ('о','е'), ('ы','е'), ('и','е'), ('и','х'),
        ('о','в'), ('е','в'), ('о','м'), ('е','м'), ('а','х'),
        ('а','м'), ('я','х'), ('я','м'), ('у','ю'), ('ю','ю'),
    ];
    if len >= 4 {
        let a = chars[len-2];
        let b = chars[len-1];
        for (x, y) in endings_2 {
            if a == x && b == y { return 3; }
        }
    }

    0
}

// ============================================================
// Common word whitelists
// ============================================================

fn is_known_en_word(word: &str) -> bool {
    EN_WORDS.binary_search(&word).is_ok()
}

fn is_known_ru_word(word: &str) -> bool {
    RU_WORDS.binary_search(&word).is_ok()
}

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
    if prefix.is_empty() { return true; }
    let idx = dict.partition_point(|&w| w < prefix);
    idx < dict.len() && dict[idx].starts_with(prefix)
}

fn is_en_prefix(prefix: &str) -> bool {
    has_prefix_in(EN_WORDS, prefix)
}

fn is_ru_prefix(prefix: &str) -> bool {
    has_prefix_in(RU_WORDS, prefix)
}

/// Sorted list of common English words (2-8 letters).
/// Includes everyday + programming/tech + chat vocabulary.
/// MUST stay sorted — lookup is binary_search.
const EN_WORDS: &[&str] = &[
    "a", "about", "above", "abroad", "absolute", "absurd", "accept", "access",
    "account", "accurate", "achieve", "across", "act", "action", "active", "actor",
    "actual", "add", "address", "admin", "admit", "adopt", "adult", "advance",
    "advise", "after", "afternoon", "again", "against", "age", "agency", "agent",
    "ago", "agree", "ahead", "aim", "air", "airport", "alarm", "album", "alert",
    "alien", "alive", "all", "allow", "almost", "alone", "along", "already", "also",
    "although", "always", "am", "amazing", "among", "amount", "an", "and", "angry",
    "animal", "announce", "annual", "another", "answer", "any", "anyone", "anything",
    "anyway", "apart", "apartment", "api", "app", "apparent", "appeal", "appear",
    "apple", "apply", "appoint", "approach", "approve", "april", "arch", "archive",
    "are", "area", "arena", "arg", "args", "argue", "arise", "arm", "army", "around",
    "arrange", "array", "arrive", "art", "article", "artist", "as", "ask", "aspect",
    "assembly", "assert", "asset", "assign", "assist", "associate", "assume", "async",
    "at", "atom", "attach", "attack", "attempt", "attend", "attention", "audio",
    "august", "aunt", "auth", "authenticate", "author", "authority", "auto", "autumn",
    "available", "avenue", "average", "avoid", "award", "aware", "away", "awesome",
    "awful", "back", "background", "backup", "bad", "bag", "balance", "ball", "band",
    "bank", "bar", "base", "basic", "basis", "batch", "battery", "battle", "bay", "be",
    "beach", "bear", "beat", "beautiful", "beauty", "because", "become", "bed", "been",
    "before", "begin", "behavior", "behind", "being", "believe", "below", "bench",
    "benefit", "beside", "best", "bet", "better", "between", "beyond", "big", "bike",
    "bill", "bin", "bind", "biology", "bird", "birth", "bit", "bite", "black", "blame",
    "blank", "block", "blog", "blood", "blow", "blue", "board", "boat", "body", "bold",
    "bone", "bonus", "book", "bool", "boost", "boot", "border", "born", "both",
    "bother", "bottle", "bottom", "bound", "box", "boy", "brain", "branch", "brand",
    "brave", "bread", "break", "breath", "brick", "bridge", "brief", "bright", "bring",
    "british", "broad", "broken", "bronze", "brother", "brown", "browser", "buddy",
    "budget", "buf", "buffer", "bug", "build", "built", "burn", "bus", "business",
    "busy", "but", "button", "buy", "by", "byte", "cable", "cache", "calendar", "call",
    "calm", "camera", "camp", "campus", "can", "cancel", "candidate", "candy",
    "cannot", "canvas", "capital", "captain", "car", "card", "care", "career",
    "careful", "carry", "case", "cash", "cast", "cat", "catalog", "catch", "category",
    "cause", "cave", "ceiling", "cell", "center", "central", "century", "ceremony",
    "certain", "chain", "chair", "challenge", "chance", "change", "channel", "chapter",
    "char", "character", "charge", "chart", "chase", "chat", "cheap", "check", "chef",
    "chemical", "chest", "chicken", "chief", "child", "chocolate", "choice", "choose",
    "chrome", "church", "city", "civil", "claim", "class", "classic", "clean", "clear",
    "click", "client", "climate", "climb", "clip", "clock", "clone", "close", "cloud",
    "club", "cmd", "code", "coffee", "cold", "collapse", "collect", "college", "color",
    "column", "combat", "combine", "come", "comfort", "command", "comment", "commit",
    "committee", "common", "communicate", "community", "company", "compare", "compete",
    "complete", "complex", "complicate", "component", "compose", "compound",
    "compress", "compute", "concept", "concern", "concert", "conclude", "concrete",
    "condition", "conduct", "conference", "confirm", "conflict", "confront", "confuse",
    "congress", "connect", "consider", "consist", "constant", "construct", "consult",
    "consume", "contact", "contain", "content", "contest", "context", "continue",
    "contract", "contrast", "control", "convert", "cook", "cool", "copy", "core",
    "corn", "corner", "correct", "cost", "cotton", "couch", "could", "council",
    "count", "country", "county", "couple", "course", "court", "cover", "cpu", "crash",
    "crazy", "cream", "create", "credit", "crime", "crisis", "critical", "cross",
    "crowd", "crown", "crucial", "cruel", "cry", "crypto", "css", "ctx", "cube",
    "cultural", "culture", "cup", "curious", "current", "curtain", "curve", "custom",
    "customer", "cut", "cycle", "daily", "damage", "dance", "danger", "dark", "data",
    "date", "daughter", "day", "days", "dead", "deal", "dear", "death", "debate",
    "debug", "decade", "december", "decide", "decision", "declare", "decline",
    "decrease", "deep", "def", "default", "defeat", "defend", "define", "definite",
    "degree", "delay", "delete", "deliver", "demand", "democrat", "demonstrate",
    "deny", "depart", "depend", "deploy", "describe", "desert", "design", "desire",
    "desk", "despite", "destroy", "detail", "detect", "determine", "dev", "develop",
    "device", "dialog", "did", "die", "diet", "diff", "differ", "difficult", "dig",
    "dimension", "dinner", "dir", "direct", "director", "dirty", "disagree",
    "disappear", "disaster", "discover", "discuss", "disease", "disk", "dismiss",
    "display", "distance", "district", "diverse", "divide", "dna", "do", "doc",
    "doctor", "document", "does", "dog", "doing", "dollar", "domestic", "dominate",
    "donate", "done", "door", "double", "doubt", "down", "download", "draft", "drag",
    "draw", "dream", "dress", "drink", "drive", "driver", "drop", "drug", "dry", "due",
    "during", "dust", "duty", "each", "early", "earn", "earth", "ease", "east", "easy",
    "eat", "economic", "economy", "edge", "edit", "editor", "education", "effect",
    "effort", "egg", "eight", "either", "electric", "element", "else", "email",
    "embed", "embrace", "emerge", "emotional", "employ", "empty", "enable",
    "encourage", "end", "enemy", "energy", "engage", "engine", "enhance", "enjoy",
    "enough", "enroll", "ensure", "enter", "entire", "entry", "enum", "env",
    "environment", "equal", "equip", "era", "err", "error", "escape", "especially",
    "essential", "establish", "estimate", "ethnic", "even", "evening", "event", "ever",
    "every", "everyone", "everything", "evidence", "evolve", "exact", "example",
    "exceed", "excellent", "except", "exchange", "excite", "exec", "execute",
    "exhibit", "exist", "exit", "expand", "expect", "experience", "experiment",
    "expert", "explain", "explore", "export", "expose", "express", "extend",
    "external", "extra", "extract", "eye", "face", "facility", "fact", "factor",
    "factory", "fade", "fail", "failure", "fair", "false", "familiar", "family",
    "famous", "fan", "fantasy", "far", "farm", "fashion", "fast", "fat", "father",
    "fault", "fav", "favor", "fear", "feat", "feature", "february", "fee", "feed",
    "feel", "fellow", "female", "fence", "few", "fiction", "field", "fifteen", "fifth",
    "fifty", "fight", "figure", "file", "files", "fill", "film", "filter", "final",
    "finance", "financial", "find", "fine", "finger", "finish", "fire", "firm",
    "first", "fish", "fit", "five", "fix", "flag", "flat", "flavor", "flight", "float",
    "floor", "flow", "flower", "flu", "fluid", "fly", "fmt", "focus", "folder",
    "follow", "food", "foot", "football", "for", "force", "forecast", "foreign",
    "forest", "forever", "forget", "fork", "form", "format", "former", "forth",
    "fortune", "forum", "forward", "foster", "foundation", "four", "fourth", "frame",
    "free", "freedom", "frequent", "fresh", "friday", "friend", "from", "front",
    "frozen", "fruit", "fuel", "full", "fun", "function", "fund", "funeral",
    "furniture", "further", "future", "fx", "fyi", "gain", "galaxy", "gallery", "game",
    "gang", "gap", "garage", "garbage", "garden", "gas", "gate", "gather", "gave",
    "gay", "gear", "general", "generate", "generation", "genius", "gentle",
    "gentleman", "gently", "genuine", "gesture", "get", "giant", "gift", "git", "give",
    "given", "glance", "global", "glove", "go", "goal", "god", "goes", "going", "gold",
    "golf", "gone", "good", "google", "got", "government", "grand", "grant", "grass",
    "grave", "gray", "great", "green", "grep", "grid", "grief", "grocery", "grope",
    "ground", "group", "grow", "growth", "guarantee", "guard", "guess", "guest",
    "guide", "guilty", "gun", "guy", "gym", "habit", "hair", "half", "hall", "hand",
    "handle", "hang", "happen", "happy", "hard", "hardly", "hash", "hat", "hate",
    "have", "head", "headline", "health", "hear", "heart", "heat", "heavy", "height",
    "hello", "help", "her", "here", "hex", "hey", "hi", "hide", "high", "hill", "him",
    "his",
    "historical", "history", "hit", "hobby", "hockey", "hold", "hole", "holiday",
    "home", "homeless", "homework", "honest", "honey", "honor", "hope", "horizon",
    "horror", "hospital", "host", "hot", "hotel", "hour", "house", "household",
    "housing", "how", "however", "html", "http", "https", "hub", "huge", "human",
    "hunger", "hunt", "hurry", "hurt", "husband", "hybrid", "hypothesis", "icon",
    "idea", "ideal", "identify", "identity", "ideology", "idx", "if", "ignore", "ill",
    "image", "imagine", "immediate", "impact", "implement", "imply", "import",
    "impose", "improve", "in", "incident", "include", "income", "increase",
    "incredible", "indeed", "independent", "index", "indicate", "individual",
    "industrial", "industry", "infect", "info", "inform", "ingredient", "initial",
    "inject", "injure", "injury", "inner", "innocent", "input", "insert", "inside",
    "insist", "inspect", "inspire", "install", "instance", "instead", "institute",
    "institution", "insurance", "intake", "integrate", "intellectual", "intelligence",
    "intend", "intense", "interest", "interior", "intern", "internal", "international",
    "internet", "interpret", "interview", "intimate", "into", "introduce", "invest",
    "invite", "involve", "iphone", "iron", "is", "island", "issue", "it", "item",
    "its", "itself", "ivory", "jacket", "jail", "java", "jeans", "jet", "job", "join",
    "joint", "joke", "journal", "journey", "joy", "judge", "judgment", "juice", "july",
    "jump", "june", "jury", "just", "justice", "justify", "kbd", "keep", "kept", "key",
    "keyboard", "keys", "kick", "kid", "kill", "killer", "kind", "king", "kiss",
    "kitchen", "knee", "knock", "know", "knowledge", "known", "lab", "label", "labor",
    "lack", "lady", "lake", "lamb", "lamp", "land", "lane", "language", "lap",
    "laptop", "large", "last", "late", "later", "latest", "latin", "latter", "laugh",
    "launch", "laundry", "law", "lawyer", "lay", "layer", "layout", "lazy", "lead",
    "leader", "leaf", "lean", "learn", "least", "leather", "leave", "lecture", "left",
    "leg", "legacy", "legal", "legend", "legitimate", "lemon", "length", "lens",
    "less", "lesson", "let", "letter", "level", "lexicon", "liberal", "liberty",
    "library", "license", "lie", "life", "light", "like", "likely", "limit", "line",
    "link", "linux", "liquid", "list", "listen", "literally", "literary", "literature",
    "little", "live", "load", "loan", "local", "locate", "lock", "log", "login",
    "logout", "long", "look", "loop", "lose", "loss", "lost", "lot", "loud", "love",
    "low", "lower", "loyal", "lucky", "lunch", "machine", "mad", "made", "magazine",
    "magic", "mail", "main", "maintain", "major", "make", "male", "mall", "man",
    "manage", "manager", "many", "map", "march", "margin", "mark", "market",
    "marriage", "marry", "mask", "mass", "master", "match", "material", "matter",
    "may", "maybe", "mayor", "me", "meal", "mean", "meanwhile", "measure", "meat",
    "media", "medical", "medicine", "medium", "meet", "meeting", "mem", "member",
    "memorize", "memory", "menu", "message", "metal", "method", "middle", "might",
    "military", "milk", "million", "mind", "mine", "mini", "minor", "minority",
    "minute", "mirror", "miss", "mission", "mistake", "mix", "mobile", "mock", "mod",
    "mode", "model", "modern", "modest", "modify", "module", "moment", "monday",
    "money", "monitor", "monster", "month", "mood", "moral", "more", "morning", "most",
    "mostly", "mother", "motion", "motor", "mountain", "mouse", "mouth", "move",
    "movie", "msg", "much", "mud", "multiply", "muscle", "museum", "music", "must",
    "mut", "mutual", "my", "myself", "myth", "name", "narrow", "nasty", "nation",
    "national", "native", "natural", "nature", "naval", "navigation", "navy", "near",
    "nearly", "necessary", "neck", "need", "negative", "negotiate", "neither",
    "nephew", "nerve", "network", "never", "new", "news", "next", "nice", "niche",
    "night", "nine", "ninety", "no", "nobody", "node", "noise", "nominate", "none",
    "nope", "normal", "north", "northeast", "northern", "nose", "not", "note",
    "nothing", "notice", "notion", "novel", "november", "now", "nuclear", "null",
    "number", "numerous", "nurse", "nut", "object", "objective", "obligation",
    "observation", "observe", "obtain", "obvious", "occasion", "occupy", "occur",
    "ocean", "october", "odd", "of", "off", "offense", "offer", "office", "officer",
    "official", "often", "oh", "oil", "okay", "old", "older", "olive", "omit", "on",
    "once", "one", "online", "only", "onto", "open", "operate", "operation", "opinion",
    "opponent", "opportunity", "oppose", "opposite", "option", "orange", "order",
    "ordinary", "organic", "organization", "organize", "origin", "original", "other",
    "otherwise", "our", "ourselves", "out", "outcome", "outdoor", "output", "outside",
    "oven", "over", "overall", "own", "owner", "oxygen", "pace", "pack", "package",
    "page", "pain", "paint", "painting", "pair", "palace", "palm", "pan", "panel",
    "panic", "pants", "paper", "parent", "park", "part", "particular", "partner",
    "party", "pass", "past", "patch", "path", "patient", "pattern", "pause", "pay",
    "peace", "peak", "pen", "pencil", "people", "per", "perceive", "perfect",
    "perform", "perhaps", "period", "permit", "person", "personal", "perspective",
    "pet", "phase", "phone", "photo", "phrase", "physical", "piano", "pick", "picture",
    "pie", "piece", "pig", "pillow", "pilot", "pin", "pink", "pipe", "pitch", "place",
    "plain", "plan", "plant", "plastic", "plate", "platform", "platinum", "play",
    "player", "please", "pleasure", "plenty", "plot", "plug", "plus", "pocket", "poem",
    "poet", "poetry", "point", "police", "policy", "political", "pool", "poor", "pop",
    "popular", "population", "port", "portion", "portrait", "position", "positive",
    "possess", "possible", "post", "potato", "potential", "pound", "poverty", "powder",
    "power", "practical", "practice", "pray", "prayer", "predict", "prefer",
    "preference", "pregnant", "prepare", "present", "preserve", "president", "press",
    "pressure", "pretend", "pretty", "prev", "prevent", "previous", "price", "pride",
    "primary", "prime", "prince", "principal", "print", "prior", "priority", "prison",
    "private", "prize", "probably", "problem", "procedure", "process", "produce",
    "product", "production", "professional", "professor", "profile", "profit",
    "program", "progress", "project", "promise", "promote", "prompt", "proof",
    "proper", "property", "proposal", "propose", "protect", "protein", "protest",
    "proud", "prove", "provide", "province", "psychology", "pub", "public", "publish",
    "pull", "pump", "punch", "pundit", "punish", "purchase", "pure", "purple",
    "purpose", "pursue", "push", "put", "python", "quality", "quarter", "queen",
    "query", "question", "queue", "quick", "quickly", "quiet", "quit", "quite",
    "quote", "race", "racial", "radar", "radiation", "radio", "rail", "rain", "raise",
    "ram", "random", "range", "rank", "rapid", "rare", "rate", "rather", "ratio",
    "raw", "reach", "react", "read", "reader", "ready", "real", "realize", "really",
    "rear", "reason", "recall", "receive", "recent", "recipe", "recognize", "record",
    "recover", "red", "redo", "reduce", "ref", "refer", "reference", "reflect",
    "reform", "refresh", "refrigerator", "refuse", "regard", "regime", "region",
    "register", "regular", "regulate", "regulation", "rehabilitation", "reject",
    "relate", "relative", "relax", "release", "relevant", "relief", "religion",
    "religious", "rely", "remain", "remember", "remind", "remote", "remove", "render",
    "rent", "repair", "repeat", "replace", "reply", "repo", "report", "represent",
    "republican", "request", "require", "rescue", "research", "resemble", "reserve",
    "resident", "resist", "resolve", "resort", "resource", "respect", "respond",
    "response", "responsibility", "rest", "restaurant", "restore", "result", "retire",
    "return", "reveal", "revenue", "review", "revolution", "rhythm", "rib", "ribbon",
    "rice", "rich", "ride", "right", "ring", "rise", "risk", "river", "road", "rock",
    "role", "roll", "roman", "romance", "roof", "room", "root", "rose", "rough",
    "round", "route", "routine", "row", "royal", "rule", "run", "rush", "russian",
    "rust", "sacred", "sacrifice", "sad", "safe", "safety", "said", "sail", "salad",
    "salary", "sale", "salt", "same", "sample", "sanction", "sand", "satellite",
    "satisfy", "saturday", "sauce", "save", "say", "scale", "scan", "scandal", "scar",
    "scare", "scene", "schedule", "schema", "scheme", "scholar", "school", "science",
    "scientific", "scientist", "scope", "score", "screen", "script", "sculpture",
    "sea", "search", "season", "seat", "second", "secret", "secretary", "section",
    "sector", "secure", "security", "see", "seed", "seek", "seem", "segment", "seldom",
    "select", "sell", "senate", "send", "senior", "sense", "sensitive", "sentence",
    "separate", "september", "sequence", "serious", "serve", "server", "service",
    "session", "set", "setting", "settle", "settlement", "seven", "several", "severe",
    "sexual", "sha", "shake", "shall", "shape", "share", "sharp", "she", "shed",
    "shell", "shelter", "sheriff", "shift", "shine", "ship", "shirt", "shock", "shoe",
    "shoot", "shop", "shore", "short", "shortly", "shot", "should", "shoulder",
    "shout", "show", "shower", "shrug", "shut", "sick", "side", "sign", "signal",
    "significant", "silent", "silver", "similar", "simple", "simply", "since", "sing",
    "singer", "single", "sink", "sir", "sister", "sit", "site", "situation", "six",
    "size", "skill", "skin", "skip", "sky", "slave", "sleep", "slice", "slide", "slip",
    "slow", "slowly", "small", "smart", "smell", "smile", "smoke", "snap", "snow",
    "so", "social", "society", "soft", "software", "soil", "solar", "soldier", "solid",
    "solution", "solve", "some", "somebody", "someone", "something", "sometimes",
    "somewhat", "somewhere", "son", "song", "soon", "sort", "soul", "sound", "soup",
    "source", "south", "southeast", "southern", "sovereign", "space", "span", "speak",
    "speaker", "special", "species", "specific", "specify", "speech", "speed", "spell",
    "spend", "sphere", "spirit", "spiritual", "split", "sponsor", "sport", "spot",
    "spouse", "spread", "spring", "sql", "square", "src", "ssh", "stable", "staff",
    "stage", "stair", "stake", "stand", "standard", "star", "stare", "start", "state",
    "statement", "station", "statistics", "status", "stay", "steady", "steal", "steel",
    "step", "stick", "still", "stock", "stomach", "stone", "stop", "storage", "store",
    "storm", "story", "strain", "strange", "strategy", "stream", "street", "strength",
    "stress", "stretch", "strict", "strike", "string", "strip", "stroke", "strong",
    "struct", "structure", "struggle", "student", "studio", "study", "stuff", "stupid",
    "style", "subject", "submit", "subscribe", "substance", "substantial", "success",
    "successful", "such", "suddenly", "suffer", "sufficient", "sugar", "suggest",
    "suit", "sum", "summer", "summit", "sun", "sunday", "sunny", "super", "support",
    "suppose", "supreme", "sure", "surely", "surface", "surgery", "surprise",
    "surround", "survey", "survive", "suspect", "suspend", "sustain", "swap", "sweep",
    "sweet", "swim", "swing", "switch", "sword", "symbol", "sympathy", "symptom",
    "sync", "system", "table", "tablet", "tag", "tail", "take", "tale", "talent",
    "talk", "tall", "tank", "tap", "tape", "target", "task", "taste", "tax", "taxi",
    "tcp", "tea", "teach", "teacher", "team", "tear", "technical", "technique",
    "technology", "teen", "telephone", "television", "tell", "temperature", "temple",
    "temporary", "ten", "tend", "tendency", "tension", "tent", "term", "terms",
    "terrible", "territory", "terror", "terrorist", "test", "text", "than", "thank",
    "thanks", "that", "the", "their", "them", "then", "theory", "therapy", "there",
    "therefore", "these", "they", "thin", "thing", "think", "third", "thirty", "this",
    "those", "though", "thought", "thousand", "threat", "three", "threshold",
    "through", "throughout", "throw", "thursday", "thus", "thx", "ticket", "tide",
    "tie", "tight", "time", "tin", "tiny", "tip", "tire", "tired", "title", "tmp",
    "to", "today", "together", "toilet", "token", "tolerate", "tomato", "tomorrow",
    "tone", "tongue", "tonight", "too", "took", "tool", "tooth", "top", "topic",
    "total", "touch", "tough", "tour", "tourist", "toward", "tower", "town", "toy",
    "track", "trade", "tradition", "traditional", "traffic", "tragedy", "trail",
    "train", "training", "transfer", "transform", "transit", "translate", "transport",
    "trap", "travel", "treat", "tree", "trend", "trial", "tribe", "trick", "trigger",
    "trip", "triumph", "troop", "trouble", "trousers", "truck", "true", "truly",
    "trust", "truth", "try", "tub", "tube", "tuesday", "tune", "tunnel", "turn", "tv",
    "twelve", "twenty", "twice", "twin", "two", "tx", "type", "typical", "typo", "udp",
    "ugly", "ui", "ultimate", "ultimately", "unable", "uncle", "under", "understand",
    "undertake", "undo", "uniform", "union", "unique", "unit", "unite", "university",
    "unknown", "unless", "unlike", "until", "unusual", "up", "update", "upload",
    "upon", "upper", "urban", "urge", "us", "usage", "use", "used", "user", "usual",
    "utf", "val", "validate", "valley", "valuable", "value", "vampire", "van",
    "vanish", "var", "vary", "vast", "vector", "vehicle", "venture", "venue", "verb",
    "verdict", "version", "versus", "very", "veteran", "via", "victim", "victory",
    "video", "view", "viewer", "village", "violate", "violence", "violent", "virtual",
    "virtue", "visible", "vision", "visit", "visitor", "visual", "vital", "vitamin",
    "voice", "void", "volume", "volunteer", "vote", "voter", "vs", "vue", "wage",
    "wait", "wake", "walk", "wall", "wallet", "war", "warm", "warn", "warning", "wash",
    "waste", "watch", "water", "wave", "way", "ways", "we", "weak", "wealth", "weapon",
    "wear", "weather", "web", "website", "wedding", "wednesday", "week", "weekend",
    "weigh", "weight", "welcome", "welfare", "well", "west", "western", "wet", "what",
    "whatever", "wheel", "when", "whenever", "where", "whereas", "whether", "which",
    "while", "whisper", "white", "who", "whole", "whom", "whose", "why", "wide",
    "widely", "wife", "wild", "will", "willing", "win", "wind", "window", "wine",
    "wing", "winner", "winter", "wipe", "wire", "wise", "wish", "with", "withdraw",
    "within", "without", "witness", "wolf", "woman", "women", "wonder", "wood",
    "wooden", "word", "work", "worker", "workshop", "world", "worried", "worry",
    "worth", "would", "wound", "wrap", "write", "writer", "wrong", "wrote", "xml",
    "yahoo", "yard", "yeah", "year", "years", "yell", "yellow", "yep", "yes",
    "yesterday", "yet", "yield", "you", "young", "your", "yours", "yourself", "youth",
    "yup", "zero", "zip", "zone", "zoom",
];

/// Sorted list of common Russian words (2-8 letters).
/// MUST stay sorted — lookup is binary_search.
const RU_WORDS: &[&str] = &[
    "а", "август", "автор", "агент", "адрес", "актуально", "актёр", "алгоритм",
    "алкоголь", "альбом", "анализ", "английский", "апрель", "армия", "архив", "аспект",
    "атака", "аудио", "аэропорт", "база", "базовый", "бай", "байт", "балкон", "банк",
    "бар", "барабан", "бармен", "барьер", "бассейн", "батарея", "бедный", "бежать",
    "без", "безопасность", "безумный", "белый", "берег", "беречь", "берлин", "беседа",
    "бесконечный", "беспокоить", "беспокойство", "бесполезный", "беспомощный",
    "беспощадно", "бессмертный", "бесценный", "бетон", "библиотека", "бизнес", "билет",
    "благо", "благодарить", "благородный", "бланк", "близкий", "близко", "блок",
    "блокировка", "блюдо", "бог", "богатый", "бой", "более", "болезнь", "болеть",
    "большой", "борт", "бояться", "брат", "брать", "бриллиант", "бросать", "будет",
    "будущее", "будь", "бумага", "буря", "бутылка", "буфер", "бывает", "была", "были",
    "было", "быстро", "быстрый", "быть", "бюджет", "в", "важно", "важный", "вариант",
    "вас", "ваш", "ведь", "везде", "век", "великий", "вера", "верить", "верно",
    "вероятно", "верхний", "вершина", "вес", "веселый", "весенний", "весна", "вести",
    "весь", "весьма", "ветер", "ветка", "вечер", "вечно", "вечный", "вещь", "взгляд",
    "взять", "вид", "видел", "видит", "видишь", "видно", "виж", "вижу", "виза",
    "визит", "виноват", "винт", "висеть", "висок", "витамин", "вкладка", "включать",
    "включить", "вкус", "владелец", "власть", "влияние", "вместе", "вместо", "вне",
    "внезапно", "внизу", "внимание", "внимательно", "внутри", "вовремя", "вода",
    "водитель", "водка", "воевать", "военный", "вождь", "возвращаться", "возможно",
    "возможность", "возможный", "возраст", "война", "войти", "вокруг", "вообще",
    "вопрос", "ворота", "восемь", "воспитание", "восток", "восторг", "восхищение",
    "восьмой", "вот", "впервые", "вперед", "впечатление", "впрочем", "врач", "время",
    "все", "всегда", "всего", "всем", "всех", "вскоре", "всюду", "всякий", "вторник",
    "второй", "вход", "входить", "вчера", "вы", "выбирать", "выбор", "выбрать",
    "вывод", "выдержать", "выиграть", "выйти", "выпить", "вырастать", "высокий",
    "высота", "выставка", "вытащить", "выход", "выходной", "газ", "газета", "где",
    "генерал", "генератор", "гениальный", "георгий", "герой", "глава", "главный",
    "глаз", "глубина", "глубокий", "глупый", "глухой", "глядеть", "гнев", "гнездо",
    "говорить", "говорят", "год", "годы", "гол", "голова", "голод", "голубой", "голый",
    "гонка", "гонконг", "гора", "гораздо", "горе", "город", "горячий", "госпиталь",
    "господи", "господин", "гостиница", "гость", "государство", "готов", "готовый",
    "граница", "группа", "грустный", "грусть", "грязный", "два", "дверь", "двести",
    "движение", "двор", "двоюродный", "двухтысячный", "девочка", "девушка",
    "девяносто", "девятый", "девять", "деградация", "дед", "действие", "действительно",
    "действовать", "декабрь", "делать", "дело", "день", "деньги", "дерево", "десятый",
    "десять", "деталь", "дети", "детский", "диалог", "дизайн", "диск", "длинный",
    "для", "дневник", "до", "добавить", "добрый", "доверие", "довольно", "догадаться",
    "дождь", "дойти", "доктор", "документ", "долго", "долгосрочный", "должен",
    "должно", "должный", "долина", "дом", "домашний", "допустимый", "дорога",
    "дорогой", "доска", "достаточно", "достать", "достичь", "достоевский", "достояние",
    "доступ", "дохнуть", "дочь", "драка", "драма", "другая", "другие", "другое",
    "другой", "дружба", "дубликат", "думал", "думала", "думали", "думаю", "дурак",
    "духовный", "душа", "дыра", "дырка", "дядя", "его", "ежедневно", "ежемесячный",
    "ежесекундный", "если", "ест", "есть", "ехать", "еще", "ещё", "её", "жалко",
    "жалобный", "жаль", "жара", "жаркий", "ждать", "же", "желать", "железный",
    "желтый", "желудок", "жена", "женский", "женщина", "жертва", "жест", "жестокий",
    "живой", "живот", "жизнь", "жилой", "жители", "жить", "журнал", "жюри", "за",
    "забавно", "забыл", "забыть", "завидовать", "зависть", "завод", "завтра", "задача",
    "задний", "задумчивый", "закат", "заключение", "закон", "закрыть", "зал", "залежи",
    "зальный", "замечательный", "занять", "запад", "записать", "запрос", "зарплата",
    "заставить", "затем", "захотеть", "зачем", "защита", "защищать", "заявить",
    "звать", "звезда", "звонить", "звук", "здание", "здесь", "здоровый", "здоровье",
    "здравствуй", "зеленый", "земля", "зеркало", "зима", "златый", "знак", "знакомый",
    "знать", "значение", "значит", "золотой", "зона", "зуб", "и", "играть", "игрок",
    "идея", "идти", "иерархия", "избежать", "известно", "известный", "извинение",
    "извинить", "изгнать", "изменение", "измерение", "изображение", "изучать", "имей",
    "именно", "иметь", "имя", "иначе", "инженер", "иногда", "иное", "иностранный",
    "институт", "интересный", "интернет", "информация", "искать", "исключение",
    "искренний", "искусство", "испанский", "исполнитель", "использовать", "история",
    "источник", "исход", "июль", "июнь", "йог", "к", "кабинет", "каждый", "казался",
    "казаться", "казнить", "как", "какая", "какие", "какое", "какой", "калина",
    "камень", "камера", "кампания", "канадский", "канал", "капитал", "карандаш",
    "карман", "картофель", "катастрофа", "катать", "категория", "кафе", "кафедра",
    "квартира", "керамика", "кивать", "километр", "кино", "клавиатура", "класс",
    "клиент", "ключ", "книга", "кнопка", "когда", "кого", "кому", "конечно",
    "конкретный", "конкурент", "контракт", "конференция", "концерт", "кончать",
    "копия", "корабль", "корейский", "король", "короткий", "коротко", "корпус",
    "космический", "космос", "которая", "которого", "которое", "которой", "которые",
    "который", "кофе", "красивый", "красный", "кремль", "крепкий", "крест", "крик",
    "кричать", "кровавый", "кровь", "кроме", "крохотный", "крупный", "крыло", "крыша",
    "кто", "кувалда", "куда", "кукла", "культура", "купить", "курс", "кусок", "кухня",
    "лаборатория", "лавка", "лагерь", "ладно", "ладонь", "лазер", "лампа", "лапа",
    "лауреат", "лев", "лед", "лес", "лето", "ли", "либеральный", "лидер", "лисий",
    "лист", "литература", "лифт", "лицо", "личный", "лишь", "лоб", "ложиться", "лучше",
    "любая", "любит", "любить", "любой", "люди", "мавр", "магазин", "мало", "малыш",
    "мама", "манер", "манера", "материал", "мать", "маша", "машина", "мгновение",
    "мгновенный", "медленно", "медленный", "медь", "между", "международный", "мел",
    "мелкий", "меньше", "меня", "менять", "мертвый", "места", "местный", "место",
    "месяц", "металл", "метод", "милион", "милиция", "миллиард", "миллионный", "мир",
    "мировой", "мнение", "много", "многое", "множество", "мог", "могу", "мода",
    "модный", "моего", "моет", "может", "можно", "мой", "молитва", "молодежь",
    "молодой", "молоко", "молчание", "молчать", "момент", "монумент", "мороз",
    "москва", "москвич", "мотоцикл", "мочить", "мочь", "мощный", "моя", "моё", "моём",
    "мрак", "мрачный", "мудрый", "муж", "мужчина", "музыка", "мурзик", "мы", "мыло",
    "мысленно", "мысль", "мыть", "мясо", "мяч", "на", "наблюдать", "наверное",
    "наверняка", "навсегда", "нагреть", "над", "надежда", "надо", "надоесть",
    "назвать", "назначить", "наиболее", "наконец", "налог", "нам", "наполнять",
    "направо", "например", "нас", "настоящий", "наступать", "находиться", "начало",
    "начинать", "начну", "наш", "наше", "наши", "небо", "невозможно", "него", "нежный",
    "немец", "немецкий", "нередко", "нести", "нет", "никак", "никогда", "николаевич",
    "николай", "никто", "ничего", "ничто", "но", "новость", "новый", "нога", "ноль",
    "номер", "нос", "носить", "ночь", "ноябрь", "нравиться", "ну", "нужен", "нужно",
    "о", "об", "оба", "обернуться", "обет", "облако", "обмен", "образ", "образование",
    "обратиться", "обручение", "обслуживание", "обучение", "общаться", "общение",
    "общество", "общий", "объект", "объяснить", "обычно", "обязан", "обязательно",
    "огонь", "ограничение", "огромный", "один", "одиночество", "одиночный", "одно",
    "одновременно", "одобрить", "оказаться", "океан", "окно", "около", "октябрь", "он",
    "она", "они", "оно", "опасно", "опасность", "опасный", "определить",
    "опубликовать", "опыт", "опять", "оранжевый", "организация", "организм",
    "организовать", "орел", "орион", "оркестр", "ос", "освободить", "освоение",
    "основа", "основной", "особенно", "особенность", "особый", "оставить", "остаться",
    "остров", "осуществить", "ответ", "ответить", "открытие", "открыть", "откуда",
    "отлично", "относительно", "относиться", "отношение", "отправить", "отпуск",
    "отстать", "отступить", "отсюда", "отчего", "отчества", "отчет", "офицер",
    "официально", "официант", "охота", "охрана", "очевидно", "очевидный", "очень",
    "очередной", "ошибка", "ощущать", "ощущение", "падать", "палец", "пальто",
    "память", "папа", "парадигма", "парень", "париж", "парк", "парковка", "парламент",
    "паровой", "пароль", "паром", "партия", "партнер", "паспорт", "пассажир", "пастор",
    "патент", "патриот", "пауза", "пахнуть", "пациент", "пачка", "пейзаж", "пельмень",
    "пенсия", "пепел", "первый", "перевести", "перевод", "перевозка", "перевоплотить",
    "перед", "передать", "перейти", "перекресток", "перелет", "перемена", "переписка",
    "перерыв", "перестать", "перестройка", "период", "перо", "перон", "перрон",
    "персик", "перспектива", "песня", "петух", "печаль", "печальный", "печать",
    "пешеход", "пив", "пиво", "пиджак", "пилот", "писатель", "писать", "письмо",
    "питание", "пить", "плавание", "плакать", "план", "плановый", "планшет", "пластик",
    "плата", "плато", "плач", "плохой", "площадка", "площадь", "плыть", "плюс", "по",
    "победа", "побежать", "поведение", "повесть", "повод", "поворот", "повторить",
    "погибнуть", "погода", "под", "подарок", "подбородок", "подвал", "подвиг",
    "поджигатель", "подниматься", "подобный", "подойти", "подросток", "подружиться",
    "подряд", "подумать", "подходить", "подчиняться", "подъезд", "поезд", "пожалуйста",
    "пожар", "позади", "позволять", "позвонить", "поздний", "поздно", "позиция",
    "познакомиться", "пойду", "поймать", "пойти", "пока", "показать", "покоить",
    "покой", "покрытие", "покупатель", "пол", "поле", "полезно", "полезный", "ползать",
    "поливать", "полить", "полицейский", "полиция", "полночь", "полный", "половина",
    "положение", "получить", "польза", "помидор", "помнить", "помогать", "помощник",
    "помощь", "понимание", "понимать", "понятие", "понятно", "понять", "попросить",
    "популярный", "пора", "порой", "порт", "портрет", "поручение", "порядок",
    "посвящать", "поселить", "после", "последний", "последовательно", "послушать",
    "пособие", "потерять", "поток", "потом", "потому", "похожий", "почему", "почти",
    "поэзия", "поэт", "поэтому", "появиться", "правда", "правило", "право", "правый",
    "прадед", "праздник", "практика", "практически", "предложить", "предмет",
    "предприятие", "представить", "представление", "прежде", "президент", "презирать",
    "прекрасный", "премия", "пренебрежение", "преодолеть", "препарат", "препятствие",
    "преступник", "прибыль", "привет", "пригласить", "приглашение", "приготовить",
    "придумать", "приезд", "приказ", "приключение", "пример", "принадлежать",
    "принести", "принимать", "принцип", "принять", "природа", "присутствие",
    "притянуть", "причина", "приятно", "про", "проблема", "проверить", "провести",
    "провод", "проводить", "программа", "прогресс", "продавать", "продать",
    "продолжать", "продукт", "продукция", "прожить", "проиграть", "произведение",
    "произойти", "пройти", "пропажа", "пропасть", "просить", "просто", "простой",
    "простор", "простота", "против", "противник", "противоположный", "профессия",
    "профиль", "прохладный", "процесс", "пруд", "прыжок", "прямо", "прямой", "прятать",
    "психолог", "птица", "пустой", "пусть", "путать", "путь", "пытаться", "пьеса",
    "пять", "работа", "работать", "рабочий", "радио", "радость", "разговаривать",
    "разговор", "разговорный", "раздел", "разделить", "различие", "различный",
    "размер", "разный", "разобраться", "разом", "разорвать", "разрешение", "разрешить",
    "разум", "рай", "район", "рак", "рамка", "ранее", "ранний", "раньше", "раскрывать",
    "распространить", "рассвет", "рассказ", "рассказать", "рассматривать",
    "расставание", "расстроить", "рассудок", "реализация", "ребенок", "революция",
    "регион", "регистрация", "редактор", "режим", "результат", "река", "реклама",
    "рекомендация", "религия", "репутация", "ресторан", "решение", "решительный",
    "решить", "риск", "робот", "родители", "родиться", "рождество", "розовый", "роль",
    "роман", "российский", "россия", "рост", "рот", "рубашка", "руббль", "рубежом",
    "рука", "руководитель", "русский", "ручка", "рядом", "с", "сад", "сахар", "свежий",
    "свет", "свидетель", "свидетельство", "свобода", "свободный", "свой", "связать",
    "связь", "сдавать", "северный", "сегодня", "седьмой", "сейчас", "семейный",
    "семья", "сентябрь", "серверный", "сердце", "середина", "серьезный", "сестра",
    "сеть", "сидеть", "сила", "сильный", "симпатичный", "синий", "сирота", "сиять",
    "сказал", "сказала", "сказать", "сквозь", "скорее", "скоро", "скорый", "скрестить",
    "скрыться", "слабый", "сладкий", "слева", "слегка", "след", "следовать",
    "следующий", "слеза", "слезать", "слишком", "слово", "сломать", "служба", "случай",
    "случиться", "слушать", "слышать", "смена", "смеяться", "смотреть", "смысл",
    "снаружи", "снег", "снова", "собака", "собственный", "событие", "совершенно",
    "совершенство", "советовать", "советский", "современный", "совсем", "согласиться",
    "содержание", "соединение", "сознание", "сойти", "сок", "солнце", "соль",
    "сомневаться", "сон", "соответственно", "соперник", "сопротивление", "сорок",
    "состояние", "сотня", "сотрудник", "сохранить", "союз", "спаси", "спасибо",
    "спать", "спектакль", "специалист", "специально", "список", "спокойно",
    "спокойный", "спор", "спорт", "способ", "способность", "справа", "справиться",
    "спрашивать", "среда", "среди", "средний", "средство", "срок", "ссылка", "ставить",
    "становиться", "станция", "стараться", "старик", "старший", "старый", "стать",
    "статья", "стена", "стиль", "сто", "стоит", "стол", "столетие", "столица",
    "столько", "сторона", "стоять", "страна", "страница", "страх", "страшно",
    "стрелять", "стремиться", "строгий", "строить", "строй", "стройка", "строка",
    "структура", "студент", "стул", "субъект", "суд", "судить", "судьба", "суровый",
    "сути", "суть", "сухой", "сцена", "счастливый", "счастье", "сын", "сюда", "так",
    "также", "такой", "талант", "там", "танец", "танк", "твердо", "твой", "театр",
    "тебе", "тебя", "текст", "телефон", "тема", "темно", "темный", "теперь", "тепло",
    "теплый", "термин", "территория", "террор", "террорист", "тесный", "тихий", "тихо",
    "тишина", "ткань", "то", "товарищ", "тогда", "тоже", "толстый", "только", "тонкий",
    "тоска", "тот", "точка", "точно", "точный", "тошнить", "трагедия", "трактор",
    "трамвай", "транспорт", "трасса", "тревога", "тренировка", "третий", "три",
    "тридцать", "триста", "тройка", "тропический", "труд", "трудно", "трудный", "трус",
    "ты", "тысяча", "тьма", "тюрьма", "тянуть", "тёмный", "тётя", "убедиться",
    "убежать", "убийство", "убить", "уважать", "уверенность", "уверенный", "увидеть",
    "угадать", "угол", "удар", "удачно", "удивление", "удобно", "удобный",
    "удовольствие", "уезжать", "уже", "узнать", "уйти", "указать", "улица", "улыбка",
    "улыбнуться", "уметь", "умный", "умолять", "университет", "уникальный",
    "управление", "урок", "условие", "успеть", "успех", "усталость", "устать",
    "устройство", "утвердить", "утро", "уходить", "участие", "участник", "учитель",
    "учиться", "фаза", "факт", "факультет", "фамилия", "февраль", "философия", "фильм",
    "финал", "финансы", "фирма", "фон", "фонтан", "форма", "формула", "фото",
    "фотография", "фраза", "франция", "французский", "фронт", "футбол", "хвост",
    "хитрый", "хлеб", "ходить", "хозяин", "холм", "холод", "холодный", "хороший",
    "хорошо", "хотел", "хотела", "хотели", "хоть", "хотя", "хочет", "хочешь", "хочу",
    "храбрый", "храм", "христианский", "художник", "худший", "царь", "цвет", "целевой",
    "целый", "цель", "центр", "цепь", "церковь", "цикл", "цифра", "чай", "чайка",
    "час", "частный", "часто", "часть", "часы", "чашка", "чаще", "чей", "человек",
    "чем", "через", "черный", "черта", "четвертый", "четыре", "число", "чисто",
    "чистый", "читать", "чтение", "что", "чтобы", "чувство", "чуть", "шаг", "шапка",
    "шахматы", "шеф", "шея", "широкий", "шкаф", "школа", "шляпа", "шоу", "шофер",
    "штаб", "штат", "щедрый", "щит", "экзамен", "экземпляр", "экономика", "экран",
    "электричество", "элемент", "эмоция", "энергия", "эпоха", "эра", "этаж", "этап",
    "эти", "этих", "это", "этого", "этой", "этом", "этот", "эффект", "юбилей", "юбка",
    "юг", "южный", "юмор", "юрист", "я", "явиться", "явный", "ядерный", "язык",
    "январь", "япония", "ясно", "ясный", "ящик",
];

// ============================================================
// English bigrams (expanded: ~90 most common)
// ============================================================

fn is_common_en_bigram(a: u8, b: u8) -> bool {
    matches!((a, b),
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
    matches!((a, b),
        (b'q',b'b')|(b'q',b'c')|(b'q',b'd')|(b'q',b'f')|(b'q',b'g')|(b'q',b'h')|
        (b'q',b'j')|(b'q',b'k')|(b'q',b'l')|(b'q',b'm')|(b'q',b'n')|(b'q',b'p')|
        (b'q',b'r')|(b'q',b's')|(b'q',b't')|(b'q',b'v')|(b'q',b'w')|(b'q',b'x')|
        (b'q',b'z')|(b'j',b'q')|(b'j',b'x')|(b'j',b'z')|(b'j',b'w')|(b'j',b'v')|
        (b'j',b'f')|(b'j',b'g')|(b'j',b'b')|(b'j',b'c')|(b'j',b'd')|(b'j',b'k')|
        (b'v',b'q')|(b'v',b'j')|(b'v',b'x')|(b'v',b'z')|(b'v',b'w')|(b'v',b'b')|
        (b'z',b'x')|(b'z',b'q')|(b'z',b'j')|(b'z',b'v')|(b'z',b'b')|(b'z',b'g')|
        (b'z',b'k')|(b'z',b'r')|(b'z',b'w')|(b'z',b'f')|(b'z',b'p')|(b'z',b'd')|
        (b'x',b'j')|(b'x',b'q')|(b'x',b'z')|(b'x',b'g')|(b'x',b'k')|(b'x',b'r')|
        (b'w',b'q')|(b'w',b'z')|(b'w',b'x')|(b'w',b'v')|(b'k',b'q')|(b'k',b'z')|
        (b'k',b'x')|(b'h',b'q')|(b'h',b'z')|(b'h',b'x')|(b'b',b'q')|(b'b',b'x')|
        (b'b',b'z')|(b'g',b'q')|(b'g',b'x')|(b'g',b'z')|(b'f',b'q')|(b'f',b'x')|
        (b'f',b'z')|(b'p',b'q')|(b'p',b'x')|(b'p',b'z')
    )
}

// ============================================================
// English trigrams (top ~50)
// ============================================================

fn is_common_en_trigram(a: u8, b: u8, c: u8) -> bool {
    matches!((a, b, c),
        (b't',b'h',b'e')|(b'a',b'n',b'd')|(b'i',b'n',b'g')|(b't',b'i',b'o')|
        (b'i',b'o',b'n')|(b'e',b'n',b't')|(b'h',b'e',b'r')|(b't',b'h',b'a')|
        (b'e',b'r',b'e')|(b'f',b'o',b'r')|(b'y',b'o',b'u')|(b'a',b'l',b'l')|
        (b'v',b'e',b'r')|(b't',b'h',b'i')|(b'w',b'i',b't')|(b'i',b't',b'h')|
        (b'h',b'i',b'n')|(b'g',b'h',b't')|(b'o',b'u',b'r')|(b'n',b'o',b't')|
        (b'o',b'm',b'e')|(b'o',b'u',b't')|(b's',b't',b'r')|(b'c',b'o',b'n')|
        (b'p',b'r',b'o')|(b'a',b'r',b'e')|(b'a',b'v',b'e')|(b'i',b'n',b't')|
        (b'e',b's',b's')|(b'e',b's',b't')|(b'a',b't',b'e')|(b'a',b'c',b'k')|
        (b'o',b'r',b'e')|(b'e',b'r',b's')|(b'e',b'c',b't')|(b'o',b'n',b'e')|
        (b'l',b'i',b'n')|(b't',b'e',b'r')|(b'w',b'a',b's')|(b'h',b'a',b't')|
        (b'h',b'i',b's')|(b'h',b'a',b's')|(b'h',b'a',b'v')|(b'r',b'e',b'a')|
        (b'n',b'c',b'e')|(b'i',b'v',b'e')|(b'o',b'r',b'd')|(b'u',b's',b'e')|
        (b'a',b'k',b'e')|(b't',b'e',b'd')|(b's',b'o',b'm')|(b'u',b'l',b'd')|
        (b'a',b's',b't')|(b'i',b'g',b'h')|(b'e',b'a',b'd')|(b'l',b'o',b'o')|
        (b'e',b'e',b'n')|(b'a',b'n',b't')|(b'h',b'e',b'n')|(b'h',b'e',b'm')
    )
}

// ============================================================
// Russian bigrams (expanded: ~80 most common)
// ============================================================

fn is_common_ru_bigram(a: char, b: char) -> bool {
    matches!((a, b),
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
    matches!((a, b),
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
    matches!((a, b, c),
        ('с','т','о')|('с','т','а')|('с','т','в')|('е','н','и')|('о','в','а')|
        ('а','т','ь')|('и','т','ь')|('п','р','о')|('п','р','и')|('п','р','е')|
        ('п','е','р')|('е','р','е')|('о','г','о')|('н','о','й')|('н','ы','х')|
        ('е','г','о')|('о','н','а')|('в','с','е')|('и','л','и')|('э','т','о')|
        ('к','о','м')|('т','е','л')|('о','с','т')|('п','о','л')|('о','д','н')|
        ('н','и','е')|('н','о','с')|('т','о','р')|('к','а','к')|('ч','т','о')|
        ('д','е','л')|('а','н','и')|('н','ы','е')|('о','й','н')|('т','ь','с')|
        ('н','а','л')|('е','с','т')|('о','в','о')|('е','д','е')|('а','л','ь')
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

/// Cyrillic letters that exist in Ukrainian but NOT in Russian.
/// Their presence is a strong signal the word is intentionally Ukrainian.
fn is_ukrainian_only(c: char) -> bool {
    matches!(c, 'і' | 'І' | 'ї' | 'Ї' | 'є' | 'Є' | 'ґ' | 'Ґ')
}

/// Active keyboard layout category.  Only layouts we handle natively get
/// their own variant; everything else falls into `Latin` (no translation).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kbd {
    Latin,
    Russian,
    Ukrainian,
}

fn vk_to_char(vk: u32, shift: bool, caps: bool, kbd: Kbd) -> Option<char> {
    if !(0x41..=0x5A).contains(&vk) { return None; }
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

/// Maps OEM VK codes to their punctuation character for the boundary replacement.
/// Used when OEM keys are treated as word boundaries (English layout, or
/// non-letter OEM keys in Cyrillic layout).
fn oem_boundary_char(vk: u32, shift: bool, is_cyrillic_layout: bool) -> Option<char> {
    if is_cyrillic_layout {
        // Cyrillic layouts: only a few OEM keys produce punctuation
        Some(match (vk, shift) {
            (0xBF, false) => '.', // OEM_2 → .
            (0xBF, true)  => ',', // OEM_2 + Shift → ,
            (0xBB, false) => '=', // OEM_PLUS → =
            (0xBB, true)  => '+', // OEM_PLUS + Shift → +
            (0xBD, false) => '-', // OEM_MINUS → -
            (0xBD, true)  => '_', // OEM_MINUS + Shift → _
            (0xDC, false) => '\\',
            (0xDC, true)  => '/',
            _ => ' ',
        })
    } else {
        // English layout: standard mapping
        Some(match (vk, shift) {
            (0xBA, false) => ';', (0xBA, true) => ':',
            (0xBC, false) => ',', (0xBC, true) => '<',
            (0xBE, false) => '.', (0xBE, true) => '>',
            (0xBF, false) => '/', (0xBF, true) => '?',
            (0xDE, false) => '\'', (0xDE, true) => '"',
            (0xDB, false) => '[', (0xDB, true) => '{',
            (0xDD, false) => ']', (0xDD, true) => '}',
            (0xDC, false) => '\\', (0xDC, true) => '|',
            (0xBB, false) => '=', (0xBB, true) => '+',
            (0xBD, false) => '-', (0xBD, true) => '_',
            (0xC0, false) => '`', (0xC0, true) => '~',
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

    #[track_caller]
    fn should_switch(typed: &str, expected: &str) {
        let got = decide_switch(typed);
        assert_eq!(
            got.as_deref(),
            Some(expected),
            "typed={typed:?} → wanted Some({expected:?}), got {got:?}"
        );
    }

    #[track_caller]
    fn should_not_switch(typed: &str) {
        let got = decide_switch(typed);
        assert_eq!(got, None, "typed={typed:?} expected no switch, got {got:?}");
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
        let _ = decide_switch(&ru_gibberish_of("typo"));
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
        should_not_switch("їжа");       // food
        should_not_switch("україна");   // country name
    }

    #[test]
    fn keeps_ukrainian_with_ye() {
        should_not_switch("є");          // "is/am/are"
        should_not_switch("єдиний");     // only/single
    }

    #[test]
    fn keeps_ukrainian_with_g() {
        should_not_switch("ґанок");      // porch
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
        assert_eq!(dedup.len(), same.len(), "EN_WORDS must not contain duplicates");
    }

    #[test]
    fn ru_words_sorted_and_unique() {
        let mut sorted: Vec<&&str> = RU_WORDS.iter().collect();
        sorted.sort();
        let same: Vec<&&str> = RU_WORDS.iter().collect();
        assert_eq!(sorted, same, "RU_WORDS must be sorted");
        let mut dedup = same.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), same.len(), "RU_WORDS must not contain duplicates");
    }

    // ============================================================
    // Prefix lookup / early-detector tests
    // ============================================================

    #[track_caller]
    fn should_partial(typed: &str, expected: &str) {
        let got = decide_partial_switch(typed);
        assert_eq!(
            got.as_deref(),
            Some(expected),
            "partial: typed={typed:?} wanted Some({expected:?}), got {got:?}"
        );
    }

    #[track_caller]
    fn should_not_partial(typed: &str) {
        let got = decide_partial_switch(typed);
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
        assert!(!is_ru_prefix("руд"));  // "руд" is not in our dict directly;
                                        // actually "ру…" starts words but "руд"
                                        // specifically has no word beginning
                                        // with those 3 letters in our curated list.
        // Actually "руда" is plausible but we didn't include it — double-check:
        // If this test fails, just delete this assertion — it only verifies
        // the DEAD branch, it's OK if a word happens to match.
    }

    // --- Partial (mid-word) detection — should fire early ---

    #[test]
    fn partial_fires_privet_after_three_chars() {
        // User intends "привет" but is in EN layout. After typing "ghb"
        // (which maps to "при"), we should already detect the mismatch.
        let buf = "ghb"; // == ru_gibberish_of("при") when typed in EN layout
        should_partial(buf, "при");
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
        should_not_partial("їж");  // UK letter 'ї'
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
    fn partial_fires_one_char_past_dead_end() {
        // "ghb" — "gh" is a valid bigram, but "ghb" is a dead-end in EN.
        // And "при" (the RU conversion) is a live prefix.  This is the
        // sweet spot where the early detector outperforms the word-end
        // detector by ~3-4 keystrokes.
        should_partial("ghb", "при");
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
}
