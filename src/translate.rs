use anyhow::{Result, anyhow};
use crate::i18n;
use crate::settings;
use crate::utils::{truncate, urlencode};
use serde::Deserialize;

/// Translates text. Uses DeepSeek if an API key is configured, otherwise
/// falls back to the free MyMemory API. Returns `(translation, direction)`.
pub fn translate(text: &str) -> Result<(String, String)> {
    let (from, to) = detect_direction(text);
    let direction = format!("{from} -> {to}");

    let key = settings::current().deepseek_api_key;
    let key = key.trim();

    let translated = if !key.is_empty() {
        match translate_deepseek(text, from, to, key) {
            Ok(t) => t,
            Err(e) => {
                println!("[translate] DeepSeek ошибка, fallback на MyMemory: {e}");
                translate_mymemory(text, from, to)?
            }
        }
    } else {
        translate_mymemory(text, from, to)?
    };

    Ok((translated, direction))
}

// ============================================================
// DeepSeek (OpenAI-compatible chat completions)
// ============================================================

const DEEPSEEK_URL: &str = "https://api.deepseek.com/chat/completions";

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    stream: bool,
}

#[derive(serde::Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

fn translate_deepseek(text: &str, from: &str, to: &str, api_key: &str) -> Result<String> {
    let system_prompt = format!(
        "You are a professional translator. Translate the user's text from {from} to {to}. \
         Reply with ONLY the translation — no quotes, no explanations, no source language, \
         no commentary. Preserve line breaks and formatting."
    );

    let req = ChatRequest {
        model: "deepseek-chat",
        messages: vec![
            ChatMessage { role: "system", content: &system_prompt },
            ChatMessage { role: "user",   content: text },
        ],
        temperature: 0.2,
        stream: false,
    };

    println!("[translate] DeepSeek: {from} -> {to}");

    let resp = ureq::post(DEEPSEEK_URL)
        .timeout(std::time::Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(&req).map_err(|e| anyhow!("JSON: {e}"))?)
        .map_err(|e| anyhow!("Сеть: {e}"))?;

    let body: ChatResponse = resp.into_json()
        .map_err(|e| anyhow!("JSON: {e}"))?;

    let translated = body.choices.into_iter().next()
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| anyhow!("пустой ответ"))?;

    if translated.is_empty() {
        return Err(anyhow!("пустой перевод"));
    }

    println!("[translate] DeepSeek OK: {}...", truncate(&translated, 60));
    Ok(translated)
}

// ============================================================
// MyMemory (free fallback)
// ============================================================

#[derive(Deserialize)]
struct MyMemoryResponse {
    #[serde(rename = "responseData")]
    data: MyMemoryData,
    #[serde(rename = "responseStatus")]
    status: u32,
}

#[derive(Deserialize)]
struct MyMemoryData {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

fn translate_mymemory(text: &str, from: &str, to: &str) -> Result<String> {
    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair={}|{}",
        urlencode(text), from, to,
    );

    println!("[translate] MyMemory: {}", truncate(&url, 120));

    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .map_err(|e| anyhow!("Сеть: {e}"))?;

    let body: MyMemoryResponse = resp.into_json()
        .map_err(|e| anyhow!("JSON: {e}"))?;

    if body.status != 200 {
        return Err(anyhow!("API статус {}", body.status));
    }

    println!("[translate] MyMemory OK: {}...", truncate(&body.data.translated_text, 60));
    Ok(body.data.translated_text)
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
    let mut kana = 0usize;      // Hiragana/Katakana (Japanese)
    let mut hangul = 0usize;    // Korean
    let mut han = 0usize;       // Chinese/Japanese Han
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
                'ß' | 'ä' | 'Ä' | 'ö' | 'Ö' | 'ü' | 'Ü' => { latin += 1; de_spec += 1; }
                'ñ' | 'Ñ' => { latin += 1; es_spec += 1; }
                '¿' | '¡' => { es_spec += 1; }
                'ã' | 'Ã' | 'õ' | 'Õ' => { latin += 1; pt_spec += 1; }
                'ç' | 'Ç' | 'œ' | 'Œ' => { latin += 1; fr_spec += 1; }
                'ą'|'Ą'|'ć'|'Ć'|'ę'|'Ę'|'ł'|'Ł'|'ń'|'Ń'|'ś'|'Ś'|'ź'|'Ź'|'ż'|'Ż' => {
                    latin += 1; pl_spec += 1;
                }
                'ğ' | 'Ğ' | 'ı' | 'İ' | 'ş' | 'Ş' => { latin += 1; tr_spec += 1; }
                'à'|'À'|'è'|'È'|'é'|'É'|'ì'|'Ì'|'í'|'Í'|'ò'|'Ò'|'ó'|'Ó'|'ù'|'Ù'|'ú'|'Ú'
                |'â'|'Â'|'ê'|'Ê'|'î'|'Î'|'ô'|'Ô'|'û'|'Û'|'ï'|'Ï' => {
                    latin += 1; romance_accent += 1;
                }
                _ => {}
            },
        }
    }

    // Non-Latin scripts — pick in order of distinctiveness.
    if kana > 0 { return "ja"; }                    // Japanese (any kana)
    if hangul > 0 { return "ko"; }                  // Korean
    if han > 0 { return "zh-CN"; }                  // Chinese (Han only)
    if cyrillic > latin {
        return if ukrainian_spec > 0 { "uk" } else { "ru" };
    }
    if arabic > 0 { return "ar"; }
    if hebrew > 0 { return "he"; }
    if greek > 0 { return "el"; }
    if thai > 0 { return "th"; }
    if devanagari > 0 { return "hi"; }

    // Latin scripts — distinctive markers first, then generic romance, then English.
    if pl_spec > 0 { return "pl"; }
    if tr_spec > 0 { return "tr"; }
    if de_spec > 0 { return "de"; }
    if es_spec > 0 { return "es"; }
    if pt_spec > 0 { return "pt"; }
    if fr_spec > 0 { return "fr"; }
    if romance_accent > 0 { return "it"; }
    "en"
}
