use crate::deepseek;
use crate::i18n;
use crate::settings;
use crate::utils::{truncate, urlencode};
use anyhow::{Result, anyhow};
use serde::Deserialize;

/// Which service produced a translation — shown on the popup, because the
/// two are not alike: DeepSeek reads through typos and wrong-layout text,
/// MyMemory translates the letters it's given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    DeepSeek,
    /// No DeepSeek key set.
    MyMemory,
    /// A DeepSeek key is set, but DeepSeek didn't answer; MyMemory did.
    Fallback(FallbackReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    KeyRejected,
    NoBalance,
    /// Network, timeout, server error — anything that isn't the account.
    Unavailable,
}

pub struct Translation {
    pub text: String,
    /// "en -> ru".
    pub direction: String,
    pub engine: Engine,
}

/// Translates text. Uses DeepSeek if an API key is configured, otherwise
/// falls back to the free MyMemory API.
pub fn translate(text: &str) -> Result<Translation> {
    let (from, to) = detect_direction(text);
    let direction = format!("{from} -> {to}");

    let key = settings::current().deepseek_api_key;
    let key = key.trim();

    let (translated, engine) = if !key.is_empty() {
        match translate_deepseek(text, from, to, key) {
            Ok(t) => (t, Engine::DeepSeek),
            Err(e) => {
                println!("[translate] DeepSeek ошибка, fallback на MyMemory: {e}");
                let reason = match e.downcast_ref::<deepseek::Refusal>() {
                    Some(deepseek::Refusal::KeyRejected) => FallbackReason::KeyRejected,
                    Some(deepseek::Refusal::NoBalance) => FallbackReason::NoBalance,
                    None => FallbackReason::Unavailable,
                };
                (translate_mymemory(text, from, to)?, Engine::Fallback(reason))
            }
        }
    } else {
        (translate_mymemory(text, from, to)?, Engine::MyMemory)
    };

    Ok(Translation {
        text: translated,
        direction,
        engine,
    })
}

// ============================================================
// DeepSeek
// ============================================================

fn translate_deepseek(text: &str, from: &str, to: &str, api_key: &str) -> Result<String> {
    let system_prompt = format!(
        "You are a professional translator. Translate the user's text from {from} to {to}. \
         Reply with ONLY the translation — no quotes, no explanations, no source language, \
         no commentary. Preserve line breaks and formatting."
    );

    println!("[translate] DeepSeek: {from} -> {to}");

    let turns = [
        deepseek::Turn::system(system_prompt),
        deepseek::Turn::user(text),
    ];
    let translated = deepseek::chat(api_key, &turns, 0.2, 30)?;

    println!("[translate] DeepSeek OK: {}...", truncate(&translated, 60));
    Ok(translated)
}

// ============================================================
// MyMemory (free fallback)
// ============================================================

/// MyMemory refuses a query over 500 characters — characters, not bytes:
/// 480 Cyrillic characters (873 bytes) go through, 520 Latin ones don't.
/// Pieces are kept a little under that.
const MYMEMORY_MAX_CHARS: usize = 450;

#[derive(Deserialize)]
struct MyMemoryResponse {
    #[serde(rename = "responseData", default)]
    data: MyMemoryData,
    /// A number on success — but on an error it comes back as a *string*
    /// ("403").  Declared as a number, that used to fail the whole parse and
    /// bury MyMemory's own reason under a serde message.
    #[serde(rename = "responseStatus", default)]
    status: serde_json::Value,
    #[serde(rename = "responseDetails", default)]
    details: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct MyMemoryData {
    #[serde(rename = "translatedText", default)]
    translated_text: String,
}

/// Translates through MyMemory, a piece at a time when the text is longer
/// than it takes — screen OCR often is.
///
/// Every piece keeps the whitespace it was cut at, and only what's between
/// goes to MyMemory, so line breaks and paragraphs survive the round trip.
fn translate_mymemory(text: &str, from: &str, to: &str) -> Result<String> {
    let pieces = split_for_mymemory(text, MYMEMORY_MAX_CHARS);
    if pieces.len() > 1 {
        println!(
            "[translate] MyMemory: {} chars in {} pieces",
            text.chars().count(),
            pieces.len()
        );
    }

    let mut out = String::with_capacity(text.len());
    for piece in pieces {
        let core = piece.trim();
        if core.is_empty() {
            out.push_str(piece);
            continue;
        }
        let lead = &piece[..piece.len() - piece.trim_start().len()];
        let trail = &piece[piece.trim_end().len()..];
        out.push_str(lead);
        out.push_str(&mymemory_request(core, from, to)?);
        out.push_str(trail);
    }
    Ok(out)
}

fn mymemory_request(text: &str, from: &str, to: &str) -> Result<String> {
    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair={}|{}",
        urlencode(text),
        from,
        to,
    );

    println!("[translate] MyMemory: {}", truncate(&url, 120));

    let body = match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .call()
    {
        Ok(resp) => resp.into_string().map_err(|e| anyhow!("Сеть: {e}"))?,
        // Errors usually come back as 200 with the reason inside, but not
        // always; when there's a body, it still says why.
        Err(ureq::Error::Status(code, resp)) => resp
            .into_string()
            .unwrap_or_else(|_| format!("{{\"responseStatus\":{code}}}")),
        Err(e) => return Err(anyhow!("Сеть: {e}")),
    };

    let translated = parse_mymemory(&body)?;
    println!("[translate] MyMemory OK: {}...", truncate(&translated, 60));
    Ok(translated)
}

/// Reads a MyMemory reply: the translation, or MyMemory's own words for why
/// there isn't one.
fn parse_mymemory(body: &str) -> Result<String> {
    let resp: MyMemoryResponse =
        serde_json::from_str(body).map_err(|e| anyhow!("MyMemory: {e}"))?;
    let status = match &resp.status {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let text = resp.data.translated_text;

    // The daily free allowance running out is reported in the translation
    // field itself, which would otherwise be shown as if it were the answer.
    let quota_warning = text.trim_start().starts_with("MYMEMORY WARNING");
    if status == Some(200) && !quota_warning {
        return Ok(text);
    }

    let reason = resp
        .details
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !quota_warning)
        .map(str::to_string)
        .or_else(|| Some(text.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("status {}", resp.status));
    Err(anyhow!("MyMemory: {reason}"))
}

/// Cuts `text` into pieces of at most `max` characters that join back into
/// exactly `text`.  Each cut goes at the best place in reach: a line break,
/// else the end of a sentence, else a space — only a single unbroken run
/// longer than `max` is cut mid-word.
fn split_for_mymemory(text: &str, max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        let cut = best_cut(rest, max);
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

/// Where to end the next piece of `s`: a byte index within its first `max`
/// characters, just after the separator it cuts at.
fn best_cut(s: &str, max: usize) -> usize {
    let limit = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
    let head = &s[..limit];
    // A break too close to the start would leave a sliver of a piece and one
    // more request than needed; only the last two thirds of the window count.
    let floor = head.len() / 3;

    if let Some(i) = head.rfind('\n').filter(|&i| i >= floor) {
        return i + 1;
    }

    let mut sentence_end = None;
    let mut chars = head.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?' | '…' | ';')
            && let Some(&(j, next)) = chars.peek()
            && next.is_whitespace()
        {
            sentence_end = Some(j + next.len_utf8());
        }
    }
    if let Some(end) = sentence_end.filter(|&e| e >= floor) {
        return end;
    }

    if let Some((i, c)) = head.char_indices().rev().find(|(_, c)| c.is_whitespace())
        && i > 0
    {
        return i + c.len_utf8();
    }
    limit
}

// ============================================================
// Language detection
// ============================================================

/// Picks source (auto-detected from the text) and target (user's UI language,
/// falling back to English if it would equal the source).
fn detect_direction(text: &str) -> (&'static str, &'static str) {
    let from = detect_source(text);
    let ui = i18n::current().code();
    let to = if lang_eq(from, ui) {
        if lang_eq(from, "en") { "ru" } else { "en" }
    } else {
        ui
    };
    (from, to)
}

/// Compares two language codes ignoring region (`zh-CN` == `zh`).
fn lang_eq(a: &str, b: &str) -> bool {
    let base = |s: &str| s.split('-').next().unwrap_or(s).to_ascii_lowercase();
    base(a) == base(b)
}

/// Detects source language by Unicode script and distinctive Latin characters.
/// Defaults to English when nothing specific is found.
fn detect_source(text: &str) -> &'static str {
    let mut cyrillic = 0usize;
    let mut ukrainian_spec = 0usize;
    let mut kana = 0usize; // Hiragana/Katakana (Japanese)
    let mut hangul = 0usize; // Korean
    let mut han = 0usize; // Chinese/Japanese Han
    let mut arabic = 0usize;
    let mut hebrew = 0usize;
    let mut greek = 0usize;
    let mut thai = 0usize;
    let mut devanagari = 0usize;
    let mut latin = 0usize;

    let mut de_spec = 0usize;
    let mut es_spec = 0usize;
    let mut fr_spec = 0usize;
    let mut pt_spec = 0usize;
    let mut pl_spec = 0usize;
    let mut tr_spec = 0usize;
    let mut romance_accent = 0usize; // shared among fr/it/es/pt (mostly Italian marker)

    for c in text.chars() {
        let cp = c as u32;
        match cp {
            0x0400..=0x052F => {
                cyrillic += 1;
                if matches!(c, 'ґ' | 'Ґ' | 'є' | 'Є' | 'і' | 'І' | 'ї' | 'Ї') {
                    ukrainian_spec += 1;
                }
            }
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7AF | 0x1100..=0x11FF => hangul += 1,
            0x4E00..=0x9FFF => han += 1,
            0x0600..=0x06FF | 0x0750..=0x077F => arabic += 1,
            0x0590..=0x05FF => hebrew += 1,
            0x0370..=0x03FF => greek += 1,
            0x0E00..=0x0E7F => thai += 1,
            0x0900..=0x097F => devanagari += 1,
            _ if c.is_ascii_alphabetic() => latin += 1,
            _ => match c {
                'ß' | 'ä' | 'Ä' | 'ö' | 'Ö' | 'ü' | 'Ü' => {
                    latin += 1;
                    de_spec += 1;
                }
                'ñ' | 'Ñ' => {
                    latin += 1;
                    es_spec += 1;
                }
                '¿' | '¡' => {
                    es_spec += 1;
                }
                'ã' | 'Ã' | 'õ' | 'Õ' => {
                    latin += 1;
                    pt_spec += 1;
                }
                'ç' | 'Ç' | 'œ' | 'Œ' => {
                    latin += 1;
                    fr_spec += 1;
                }
                'ą' | 'Ą' | 'ć' | 'Ć' | 'ę' | 'Ę' | 'ł' | 'Ł' | 'ń' | 'Ń' | 'ś' | 'Ś' | 'ź'
                | 'Ź' | 'ż' | 'Ż' => {
                    latin += 1;
                    pl_spec += 1;
                }
                'ğ' | 'Ğ' | 'ı' | 'İ' | 'ş' | 'Ş' => {
                    latin += 1;
                    tr_spec += 1;
                }
                'à' | 'À' | 'è' | 'È' | 'é' | 'É' | 'ì' | 'Ì' | 'í' | 'Í' | 'ò' | 'Ò' | 'ó'
                | 'Ó' | 'ù' | 'Ù' | 'ú' | 'Ú' | 'â' | 'Â' | 'ê' | 'Ê' | 'î' | 'Î' | 'ô' | 'Ô'
                | 'û' | 'Û' | 'ï' | 'Ï' => {
                    latin += 1;
                    romance_accent += 1;
                }
                _ => {}
            },
        }
    }

    // Non-Latin scripts — pick in order of distinctiveness.
    if kana > 0 {
        return "ja";
    } // Japanese (any kana)
    if hangul > 0 {
        return "ko";
    } // Korean
    if han > 0 {
        return "zh-CN";
    } // Chinese (Han only)
    if cyrillic > latin {
        return if ukrainian_spec > 0 { "uk" } else { "ru" };
    }
    if arabic > 0 {
        return "ar";
    }
    if hebrew > 0 {
        return "he";
    }
    if greek > 0 {
        return "el";
    }
    if thai > 0 {
        return "th";
    }
    if devanagari > 0 {
        return "hi";
    }

    // Latin scripts — distinctive markers first, then generic romance, then English.
    if pl_spec > 0 {
        return "pl";
    }
    if tr_spec > 0 {
        return "tr";
    }
    if de_spec > 0 {
        return "de";
    }
    if es_spec > 0 {
        return "es";
    }
    if pt_spec > 0 {
        return "pt";
    }
    if fr_spec > 0 {
        return "fr";
    }
    if romance_accent > 0 {
        return "it";
    }
    "en"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mymemory_success_is_read() {
        let body = r#"{"responseData":{"translatedText":"Hello, world"},"responseDetails":"","responseStatus":200}"#;
        assert_eq!(parse_mymemory(body).unwrap(), "Hello, world");
    }

    /// Verbatim from MyMemory for a 520-character query: the status is a
    /// string, and the reason has to reach the user rather than a serde error.
    #[test]
    fn mymemory_error_with_string_status_says_why() {
        let body = r#"{"responseData":{"translatedText":"QUERY LENGTH LIMIT EXCEEDED. MAX ALLOWED QUERY : 500 CHARS"},"quotaFinished":null,"mtLangSupported":null,"responseDetails":"QUERY LENGTH LIMIT EXCEEDED. MAX ALLOWED QUERY : 500 CHARS","responseStatus":"403","responderId":null}"#;
        let err = parse_mymemory(body).unwrap_err().to_string();
        assert_eq!(err, "MyMemory: QUERY LENGTH LIMIT EXCEEDED. MAX ALLOWED QUERY : 500 CHARS");
    }

    #[test]
    fn mymemory_quota_warning_is_an_error_not_a_translation() {
        let body = r#"{"responseData":{"translatedText":"MYMEMORY WARNING: YOU USED ALL AVAILABLE FREE TRANSLATIONS FOR TODAY. NEXT AVAILABLE IN 10 HOURS"},"responseDetails":"","responseStatus":200}"#;
        let err = parse_mymemory(body).unwrap_err().to_string();
        assert!(err.starts_with("MyMemory: MYMEMORY WARNING"), "{err}");
    }

    fn check_split(text: &str, max: usize) -> Vec<&str> {
        let pieces = split_for_mymemory(text, max);
        assert_eq!(pieces.concat(), text, "pieces must join back into the text");
        for p in &pieces {
            assert!(p.chars().count() <= max, "piece over {max}: {p:?}");
        }
        pieces
    }

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(check_split("Привет, мир", 450), vec!["Привет, мир"]);
    }

    #[test]
    fn long_text_is_cut_at_line_breaks_then_sentences() {
        let line = "Это довольно длинное предложение, которое повторяется. ";
        let paragraph = line.repeat(6);
        let text = format!("{paragraph}\n{paragraph}\n{paragraph}");
        let pieces = check_split(&text, 450);
        assert!(pieces.len() >= 3);
        // Every cut lands after a line break or a sentence end, never mid-word.
        for p in &pieces[..pieces.len() - 1] {
            assert!(p.ends_with('\n') || p.ends_with(". "), "bad cut: {p:?}");
        }
    }

    #[test]
    fn a_run_with_no_spaces_is_cut_hard_on_char_boundaries() {
        let text = "ж".repeat(1000);
        let pieces = check_split(&text, 450);
        assert_eq!(pieces.len(), 3);
    }

    /// Against the real service: a text MyMemory would refuse whole comes
    /// back translated, with its line breaks.  Spends ~1300 characters of
    /// the anonymous daily allowance.
    #[test]
    #[ignore = "network"]
    fn mymemory_translates_text_over_its_limit() {
        let paragraph = "Сегодня хорошая погода, и мы решили пойти гулять в парк. \
                         Там было много людей, которые катались на велосипедах. "
            .repeat(4);
        let text = format!("{paragraph}\n\n{paragraph}\n{paragraph}");
        assert!(text.chars().count() > 1000);
        let out = translate_mymemory(&text, "ru", "en").unwrap();
        println!("{out}");
        assert!(out.contains("weather") || out.contains("park"), "{out}");
        assert_eq!(out.matches('\n').count(), 3, "line breaks lost: {out:?}");
    }

    #[test]
    fn words_without_punctuation_are_cut_at_spaces() {
        let text = "слово ".repeat(200);
        for p in &check_split(&text, 450)[..2] {
            assert!(p.ends_with(' '), "cut mid-word: {p:?}");
        }
    }
}
