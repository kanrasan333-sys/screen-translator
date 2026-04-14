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
        let is_russian = (layout.0 as usize & 0xFFFF) == 0x0419;

        match vk {
            // Letters A-Z (layout-independent VK codes)
            0x41..=0x5A => {
                if let Some(ch) = vk_to_char(vk, shift, caps, is_russian) {
                    WORD_BUF.lock().unwrap().push(ch);
                }
            }
            // OEM keys that produce Cyrillic LETTERS on Russian layout:
            //   OEM_1(;)→ж  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
            //   OEM_4([)→х  OEM_6(])→ъ  OEM_7(')→э  OEM_3(`)→ё
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0
                if is_russian =>
            {
                if let Some(ch) = oem_to_ru_char(vk, shift, caps) {
                    WORD_BUF.lock().unwrap().push(ch);
                }
            }
            // OEM keys as punctuation (English layout, or keys that stay
            // punctuation in Russian: OEM_2(/?)→.,  OEM_5(\|)  OEM_PLUS  OEM_MINUS)
            0xBA | 0xBC | 0xBE | 0xDB | 0xDD | 0xDE | 0xC0 |
            0xBF | 0xDC | 0xBB | 0xBD => {
                let ch = oem_boundary_char(vk, shift, is_russian);
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
        3 => 8,
        4 => 5,
        _ => 3,
    }
}

/// Length-adaptive threshold for RU→EN detection.
fn threshold_ru_to_en(len: usize) -> i32 {
    match len {
        0..=2 => 999, // 2-char words: whitelist only
        3 => 10,
        4 => 6,
        _ => 4,
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

    score
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

    score
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

/// Sorted list of common English words (2-6 letters).
/// Includes everyday + programming/tech vocabulary.
const EN_WORDS: &[&str] = &[
    "about", "add", "after", "all", "also", "and", "any", "api", "app",
    "are", "arg", "ask", "async",
    "back", "bad", "base", "been", "big", "bin", "bit", "bool", "both",
    "buf", "bug", "but", "buy", "byte",
    "call", "came", "can", "case", "char", "class", "clip", "cmd",
    "code", "come", "conf", "copy", "cpu", "css", "ctx", "cut",
    "data", "date", "day", "def", "dev", "did", "dir", "disk", "dns",
    "doc", "does", "done", "down", "drop",
    "each", "edit", "else", "end", "enum", "env", "err", "even", "event",
    "every", "exec", "exit",
    "fail", "false", "far", "few", "file", "find", "first", "fit",
    "fix", "flag", "flow", "fmt", "for", "form", "found", "from",
    "full", "func",
    "gave", "get", "git", "give", "glob", "goes", "gone", "good",
    "got", "great", "grep", "gui",
    "had", "has", "hash", "have", "heap", "help", "her", "here", "hex",
    "high", "him", "his", "home", "host", "hot", "how", "html", "http",
    "idea", "idx", "impl", "info", "init", "int", "into", "its",
    "job", "join", "json", "just",
    "keep", "key", "kind", "know",
    "last", "left", "len", "let", "like", "line", "link", "list",
    "live", "load", "lock", "log", "long", "look", "loop",
    "made", "main", "make", "many", "map", "max", "may", "mem", "menu",
    "min", "mix", "mod", "mode", "more", "most", "move", "msg", "much",
    "must", "mut",
    "name", "need", "net", "new", "next", "nil", "node", "none", "not",
    "note", "now", "null", "num",
    "off", "old", "once", "one", "only", "open", "opt", "order",
    "other", "our", "out", "over", "own",
    "pack", "page", "pair", "part", "pass", "path", "pay", "per",
    "pick", "play", "port", "post", "prev", "pub", "pull", "push", "put",
    "quit",
    "ram", "raw", "read", "real", "red", "ref", "repo", "rest", "right",
    "root", "row", "rule", "run", "rust",
    "safe", "said", "same", "save", "say", "sdk", "self", "send", "set",
    "sha", "she", "show", "shut", "sign", "since", "size", "skip",
    "slot", "small", "some", "sort", "sql", "src", "ssh", "step",
    "still", "stop", "str", "such", "sum", "sure", "swap", "sync",
    "tab", "tag", "take", "talk", "tcp", "tell", "ten", "test", "text",
    "than", "that", "the", "them", "then", "there", "these", "they",
    "thing", "think", "this", "three", "time", "tmp", "todo", "too",
    "tool", "top", "tree", "true", "try", "turn", "two", "type",
    "udp", "uint", "under", "unit", "until", "upon", "url", "use",
    "used", "user", "utf",
    "val", "var", "vec", "very", "view", "vim", "void",
    "wait", "want", "war", "was", "way", "web", "well", "went",
    "what", "when", "where", "which", "while", "who", "whole",
    "why", "wide", "will", "win", "wish", "with", "word", "work",
    "world", "would", "write",
    "xml",
    "year", "yes", "yet", "you", "your",
    "zero", "zip",
];

/// Sorted list of common Russian words (2-6 letters).
const RU_WORDS: &[&str] = &[
    "без", "более", "будет", "будь", "было", "были", "была", "быть",
    "вам", "вас", "ваш", "ваша", "ваше", "ведь", "верно", "весь",
    "вещь", "видел", "вниз", "вот", "время", "все", "всего", "всех",
    "всю", "вчера",
    "где", "год", "годы", "город", "да", "даже", "дай", "далее",
    "два", "дело", "день", "для", "до", "дом", "думаю",
    "его", "ему", "если", "есть", "еще", "ещё",
    "жизнь", "жить",
    "за", "зато", "зачем", "здесь", "знал", "знать", "знаю",
    "из", "или", "иметь",
    "как", "какой", "когда", "кого", "кому", "кроме", "кто", "куда",
    "ладно", "лишь", "лучше", "люди",
    "мало", "менее", "меня", "место", "между", "мир", "мне",
    "много", "могу", "может", "можно", "мой", "мою",
    "на", "надо", "найти", "нам", "нас", "начал", "наш", "наша",
    "него", "нет", "них", "ничто", "но", "новый", "нужно",
    "об", "один", "она", "они", "оно", "опять", "от", "очень",
    "пока", "полный", "помощь", "после", "потом", "потому",
    "почему", "почти", "право", "при", "про", "просто", "пусть",
    "путь",
    "раз", "разве", "ранее", "раньше", "рядом",
    "сам", "сама", "свой", "свою", "себе", "себя", "сейчас",
    "слово", "снова", "совсем", "стал", "стать", "стоит",
    "так", "также", "такой", "там", "твой", "тебе", "тебя", "тем",
    "теперь", "тоже", "только", "тот", "точно", "три", "тут",
    "тысяч",
    "уже", "утром",
    "хотя", "хочу", "хоть",
    "часто", "чего", "чем", "через", "что", "чтобы",
    "это", "этих", "этого", "этой", "этом", "этот",
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

fn vk_to_char(vk: u32, shift: bool, caps: bool, is_russian: bool) -> Option<char> {
    if !(0x41..=0x5A).contains(&vk) { return None; }
    let c = (vk as u8) as char;
    // CapsLock inverts the shift behaviour.
    let upper = shift ^ caps;
    let en = if upper { c } else { c.to_ascii_lowercase() };
    if is_russian { Some(crate::layout::en_to_ru(en)) } else { Some(en) }
}

/// Maps OEM VK codes to their punctuation character for the boundary replacement.
/// Used when OEM keys are treated as word boundaries (English layout, or
/// non-letter OEM keys in Russian layout).
fn oem_boundary_char(vk: u32, shift: bool, is_russian: bool) -> Option<char> {
    if is_russian {
        // Russian layout: only a few OEM keys produce punctuation
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

/// Maps OEM VK codes to Russian Cyrillic letters.
/// Standard Russian keyboard:
///   OEM_3(`)→ё  OEM_4([)→х  OEM_6(])→ъ  OEM_1(;)→ж
///   OEM_7(')→э  OEM_COMMA(,)→б  OEM_PERIOD(.)→ю
fn oem_to_ru_char(vk: u32, shift: bool, caps: bool) -> Option<char> {
    let upper = shift ^ caps;
    let ch = match vk {
        0xC0 => 'ё', // OEM_3 (backtick/tilde)
        0xDB => 'х', // OEM_4 (left bracket)
        0xDD => 'ъ', // OEM_6 (right bracket)
        0xBA => 'ж', // OEM_1 (semicolon)
        0xDE => 'э', // OEM_7 (apostrophe)
        0xBC => 'б', // OEM_COMMA
        0xBE => 'ю', // OEM_PERIOD
        _ => return None,
    };
    if upper {
        // Cyrillic uppercase
        Some(ch.to_uppercase().next().unwrap_or(ch))
    } else {
        Some(ch)
    }
}
