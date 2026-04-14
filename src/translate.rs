use anyhow::{Result, anyhow};
use crate::utils::{is_cyrillic, truncate, urlencode};
use serde::Deserialize;

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(rename = "responseData")]
    data: ResponseData,
    #[serde(rename = "responseStatus")]
    status: u32,
}

#[derive(Deserialize)]
struct ResponseData {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

/// Translates text via MyMemory API. Returns `(translation, direction)`.
pub fn translate(text: &str) -> Result<(String, String)> {
    let (from, to) = detect_direction(text);
    let direction = format!("{from} -> {to}");

    let url = format!(
        "https://api.mymemory.translated.net/get?q={}&langpair={}|{}",
        urlencode(text), from, to,
    );

    println!("[translate] Запрос: {}", truncate(&url, 120));

    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .map_err(|e| anyhow!("Сеть: {e}"))?;

    let body: ApiResponse = resp.into_json()
        .map_err(|e| anyhow!("JSON: {e}"))?;

    if body.status != 200 {
        return Err(anyhow!("API статус {}", body.status));
    }

    println!("[translate] OK: {}...", truncate(&body.data.translated_text, 60));

    Ok((body.data.translated_text, direction))
}

fn detect_direction(text: &str) -> (&str, &str) {
    let cyr = text.chars().filter(|c| is_cyrillic(*c)).count();
    let lat = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if cyr > lat { ("ru", "en") } else { ("en", "ru") }
}
