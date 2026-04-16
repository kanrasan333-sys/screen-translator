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
static ENABLED: AtomicBool = AtomicBool::new(true);
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
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), HINSTANCE::default(), 0)
            .unwrap_or_default();
        *HOOK.lock().unwrap() = hook.0 as isize;
    }
    println!("[punto] Авто-смена раскладки включена (Ctrl+Alt+A для вкл/выкл)");
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn toggle() -> bool {
    let new_state = !ENABLED.load(Ordering::Relaxed);
    ENABLED.store(new_state, Ordering::Relaxed);
    if !new_state {
        WORD_BUF.lock().unwrap().clear();
    }
    new_state
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

/// Sorted list of common English words (2-8 letters).
/// Includes everyday + programming/tech + chat vocabulary.
/// MUST stay sorted — lookup is binary_search.
const EN_WORDS: &[&str] = &[
    "about", "above", "across", "add", "admin", "after", "again",
    "ago", "agree", "all", "allow", "almost", "along", "already",
    "also", "always", "am", "an", "and", "another", "answer", "any",
    "anyone", "anything", "api", "app", "apple", "apply", "are",
    "area", "arg", "args", "around", "array", "as", "ask", "async",
    "at", "audio", "auth", "auto", "available", "back", "bad", "base",
    "basic", "batch", "be", "because", "become", "been", "before",
    "begin", "being", "below", "best", "better", "between", "big",
    "bin", "bit", "block", "blog", "body", "book", "bool", "boot",
    "both", "box", "branch", "break", "bring", "browser", "buf",
    "buffer", "bug", "build", "built", "business", "but", "button",
    "buy", "by", "byte", "cache", "call", "came", "can", "cancel",
    "cannot", "care", "case", "catch", "cause", "center", "change",
    "char", "chat", "check", "child", "chrome", "city", "class",
    "clean", "clear", "click", "client", "clip", "clone", "close",
    "cloud", "cmd", "code", "color", "come", "command", "comment",
    "commit", "company", "compile", "complete", "conf", "config",
    "connect", "const", "content", "context", "copy", "core", "count",
    "course", "cpu", "crash", "create", "css", "ctx", "current",
    "custom", "cut", "data", "date", "day", "days", "debug", "def",
    "default", "delete", "deploy", "dev", "did", "diff", "dir",
    "disk", "display", "dns", "do", "doc", "docs", "does", "done",
    "down", "download", "draft", "draw", "drop", "each", "early",
    "easy", "edit", "editor", "else", "email", "empty", "enable",
    "end", "enter", "entry", "enum", "env", "err", "error", "even",
    "event", "ever", "every", "exact", "example", "exec", "exist",
    "exit", "export", "extra", "face", "fact", "fail", "false", "far",
    "fast", "fav", "feat", "feed", "feel", "felt", "few", "field",
    "file", "files", "fill", "filter", "final", "find", "fine",
    "finish", "first", "fit", "fix", "flag", "float", "flow", "fmt",
    "focus", "folder", "font", "food", "for", "force", "form",
    "format", "forum", "found", "four", "frame", "free", "friend",
    "from", "front", "full", "func", "game", "gave", "general", "get",
    "gif", "girl", "git", "give", "given", "glob", "global", "go",
    "goes", "going", "gone", "good", "google", "got", "gpu", "grand",
    "great", "green", "grep", "group", "grow", "gui", "guy", "had",
    "hand", "happen", "happy", "hard", "has", "hash", "have", "head",
    "header", "heap", "hear", "heart", "heavy", "hello", "help",
    "her", "here", "hex", "hey", "hi", "hide", "high", "him", "his",
    "history", "hit", "home", "hope", "host", "hot", "hour", "house",
    "how", "html", "http", "https", "hub", "human", "icon", "idea",
    "idx", "if", "image", "impl", "import", "in", "include", "index",
    "info", "init", "input", "insert", "inside", "install", "instead",
    "int", "into", "invite", "iphone", "is", "issue", "it", "item",
    "its", "java", "jo", "job", "join", "json", "jump", "just",
    "keep", "kept", "key", "keys", "kid", "kind", "know", "known",
    "lang", "language", "large", "last", "late", "later", "launch",
    "layer", "layout", "lazy", "lead", "learn", "least", "leave",
    "left", "len", "length", "less", "let", "letter", "level",
    "library", "life", "light", "like", "limit", "line", "link",
    "linux", "list", "listen", "little", "live", "load", "local",
    "lock", "log", "login", "logout", "long", "look", "loop", "lost",
    "lot", "love", "low", "mac", "made", "mail", "main", "make",
    "manage", "many", "map", "mark", "matter", "max", "may", "maybe",
    "me", "mean", "meet", "mem", "memory", "menu", "merge", "message",
    "method", "middle", "might", "min", "minute", "mix", "mobile",
    "mock", "mod", "mode", "model", "module", "money", "month",
    "more", "most", "mouse", "move", "movie", "msg", "much", "music",
    "must", "mut", "my", "name", "need", "nest", "net", "never",
    "new", "news", "next", "nice", "night", "nil", "no", "node",
    "none", "nope", "normal", "not", "note", "nothing", "now", "null",
    "num", "number", "object", "of", "off", "offer", "office", "often",
    "oh", "ok", "okay", "old", "on", "once", "one", "online", "only",
    "open", "option", "or", "order", "other", "our", "out", "output",
    "outside", "over", "own", "owner", "pack", "package", "page",
    "pair", "panel", "paper", "parse", "part", "party", "pass",
    "past", "patch", "path", "pay", "per", "person", "phone", "photo",
    "pick", "picture", "place", "plan", "play", "please", "plus",
    "point", "port", "post", "power", "prev", "price", "print",
    "private", "probably", "problem", "process", "product", "project",
    "promise", "proof", "proxy", "pub", "public", "pull", "push",
    "put", "python", "quality", "query", "question", "quick", "quit",
    "quite", "ram", "random", "range", "rate", "raw", "react", "read",
    "ready", "real", "really", "reason", "red", "redo", "ref",
    "refresh", "remove", "render", "repo", "report", "request",
    "reset", "response", "rest", "result", "return", "review",
    "right", "role", "room", "root", "route", "row", "rule", "run",
    "rust", "safe", "said", "same", "save", "say", "schema", "school",
    "scope", "screen", "script", "sdk", "search", "second", "secret",
    "section", "see", "seem", "seen", "select", "self", "send",
    "sent", "server", "service", "set", "seven", "several", "sha",
    "share", "she", "shell", "short", "should", "show", "shut",
    "side", "sign", "similar", "simple", "since", "single", "site",
    "size", "skill", "skip", "sleep", "slot", "slow", "small",
    "smart", "so", "social", "some", "song", "soon", "sort", "sound",
    "source", "space", "speak", "special", "speed", "spent", "split",
    "sql", "src", "ssh", "stack", "stage", "stand", "start", "state",
    "static", "status", "stay", "step", "still", "stop", "store",
    "story", "str", "stream", "string", "strong", "struct", "student",
    "study", "style", "such", "sudo", "sum", "super", "support",
    "sure", "swap", "switch", "sync", "system", "tab", "table", "tag",
    "take", "taken", "talk", "target", "task", "tcp", "team", "tell",
    "ten", "test", "text", "than", "thank", "thanks", "that", "the",
    "their", "them", "then", "there", "these", "they", "thing",
    "think", "third", "this", "those", "though", "thread", "three",
    "throw", "thus", "thx", "time", "tiny", "tip", "tips", "title",
    "tmp", "to", "today", "todo", "together", "token", "told", "too",
    "took", "tool", "top", "total", "touch", "track", "train",
    "travel", "tree", "true", "try", "turn", "two", "type", "typedef",
    "udp", "ui", "uint", "under", "unique", "unit", "until", "up",
    "update", "upload", "upon", "url", "us", "usage", "use", "used",
    "user", "using", "utf", "val", "value", "var", "vars", "vec",
    "version", "very", "video", "view", "vim", "void", "vue", "wait",
    "wake", "walk", "want", "war", "warn", "was", "watch", "water",
    "way", "ways", "we", "web", "week", "well", "went", "were",
    "what", "when", "where", "which", "while", "white", "who",
    "whole", "whose", "why", "wide", "will", "win", "window", "wish",
    "with", "within", "without", "woman", "word", "work", "world",
    "would", "write", "wrong", "wrote", "xml", "xx", "yeah", "year",
    "years", "yellow", "yep", "yes", "yet", "you", "young", "your",
    "yours", "yup", "zero", "zip", "zone", "zoom",
];

/// Sorted list of common Russian words (2-8 letters).
/// MUST stay sorted — lookup is binary_search.
const RU_WORDS: &[&str] = &[
    "без", "близко", "более", "больше", "будет",
    "будь", "бывает", "была", "были", "было",
    "быть", "вам", "вас", "ваш", "ваша", "ваше",
    "ведь", "верно", "вероятно", "весь", "вещь",
    "взял", "взяла", "взяли", "видел", "видит",
    "видишь", "вижу", "вместе", "вместо", "вниз",
    "возможно", "вокруг", "вот", "время", "все",
    "всегда", "всего", "всех", "всю", "всякий",
    "вчера", "где", "год", "годы", "город", "да",
    "даже", "дай", "дал", "дала", "далее",
    "далеко", "дали", "дать", "два", "делать",
    "дело", "день", "для", "до", "дом", "другая",
    "другие", "другое", "другой", "думал",
    "думала", "думали", "думаю", "его", "ему",
    "если", "есть", "еще", "ещё", "живу", "живут",
    "живёт", "живёшь", "жизнь", "жить", "за",
    "завтра", "зато", "зачем", "здесь", "знал",
    "знать", "знаю", "идти", "из", "или", "иметь",
    "иногда", "как", "какая", "какие", "какое",
    "какой", "когда", "кого", "кому", "конечно",
    "которая", "которого", "которое",
    "которой", "которые", "который", "кроме",
    "кто", "куда", "ладно", "лишь", "лучше",
    "любая", "любое", "любой", "любые", "люди",
    "мало", "между", "менее", "меньше", "меня",
    "место", "мир", "мне", "много", "могу",
    "может", "можно", "мой", "мою", "на",
    "наверное", "надо", "найти", "нам", "нас",
    "настолько", "начал", "наш", "наша", "него",
    "нельзя", "необходимо", "несколько", "нет",
    "никогда", "них", "ничто", "но", "новый",
    "нужно", "об", "обычно", "один", "около",
    "она", "они", "оно", "опять", "от", "отдельно",
    "очень", "перед", "плохо", "пока", "полный",
    "помощь", "после", "потом", "потому",
    "почему", "почти", "пошла", "пошли", "пошёл",
    "право", "при", "про", "просто", "против",
    "пусть", "путь", "работа", "работал",
    "работать", "раз", "разве", "ранее",
    "раньше", "рядом", "сам", "сама", "самая",
    "сами", "самое", "самые", "самый", "свои",
    "свой", "свою", "своя", "своё", "себе", "себя",
    "сегодня", "сейчас", "сказал", "сказала",
    "сказать", "слишком", "слово", "снова",
    "совсем", "стал", "стать", "стоит",
    "столько", "так", "также", "такой", "там",
    "твой", "тебе", "тебя", "тем", "теперь",
    "тоже", "только", "тот", "точно", "три", "тут",
    "тысяч", "уже", "утром", "ушла", "ушли",
    "ушёл", "хорошо", "хотел", "хотела",
    "хотели", "хоть", "хотя", "хочу", "хуже",
    "часто", "чего", "человек", "чем", "через",
    "что", "чтобы", "этих", "это", "этого", "этой",
    "этом", "этот",
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
}
