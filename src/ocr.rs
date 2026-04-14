use anyhow::{Result, anyhow};
use base64::Engine;
use crate::utils::{truncate, urlencode};
use serde::Deserialize;

/// Recognises text from BGRA pixels.
/// Primary: OCR.space API (free, good quality).
/// Fallback: Windows WinRT OCR.
pub fn recognize(bgra: &[u8], width: u32, height: u32) -> Result<String> {
    match recognize_ocrspace(bgra, width, height) {
        Ok(text) if !text.trim().is_empty() => {
            println!("[OCR] OK (OCR.space): {}", truncate(&text, 80));
            return Ok(text);
        }
        Ok(_) => println!("[OCR] OCR.space вернул пустой результат, пробуем Windows OCR..."),
        Err(e) => println!("[OCR] OCR.space ошибка: {e}, пробуем Windows OCR..."),
    }
    recognize_windows(bgra, width, height)
}

// ============================================================
// OCR.space API
// ============================================================

const OCR_SPACE_URL: &str = "https://api.ocr.space/parse/image";
/// OCR.space API key. The default value is the public demo key `"helloworld"`
/// (very low rate limits). Set the `OCR_SPACE_API_KEY` environment variable
/// at build time to embed your own free key (get one at https://ocr.space/ocrapi).
const OCR_SPACE_KEY: &str = match option_env!("OCR_SPACE_API_KEY") {
    Some(k) => k,
    None => "helloworld",
};

#[derive(Deserialize)]
struct OcrSpaceResponse {
    #[serde(rename = "ParsedResults")]
    parsed_results: Option<Vec<ParsedResult>>,
    #[serde(rename = "IsErroredOnProcessing")]
    is_errored: Option<bool>,
    #[serde(rename = "ErrorMessage")]
    error_message: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ParsedResult {
    #[serde(rename = "ParsedText")]
    parsed_text: String,
}

fn recognize_ocrspace(bgra: &[u8], width: u32, height: u32) -> Result<String> {
    let png_bytes = crate::screenshot::encode_png(bgra, width, height)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let data_url = format!("data:image/png;base64,{b64}");

    println!("[OCR] Отправка на OCR.space ({} KB)...", png_bytes.len() / 1024);

    let form_body = format!(
        "base64Image={}&language=auto&OCREngine=2&isOverlayRequired=false&scale=true&detectOrientation=true",
        urlencode(&data_url)
    );

    let resp = ureq::post(OCR_SPACE_URL)
        .timeout(std::time::Duration::from_secs(30))
        .set("apikey", OCR_SPACE_KEY)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form_body)
        .map_err(|e| anyhow!("OCR.space: {e}"))?;

    let body: OcrSpaceResponse = resp.into_json()
        .map_err(|e| anyhow!("OCR.space JSON: {e}"))?;

    if body.is_errored == Some(true) {
        let msg = body.error_message
            .map(|v| v.join("; "))
            .unwrap_or_else(|| "Unknown error".into());
        return Err(anyhow!("OCR.space: {msg}"));
    }

    let text = body.parsed_results
        .map(|results| results.into_iter().map(|r| r.parsed_text).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();

    Ok(text.trim().to_string())
}

// ============================================================
// Windows WinRT OCR (fallback)
// ============================================================

fn recognize_windows(bgra: &[u8], width: u32, height: u32) -> Result<String> {
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapPixelFormat, SoftwareBitmap,
    };
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::InMemoryRandomAccessStream;
    use windows::core::HSTRING;

    let (data, w, h) = scale_for_ocr(bgra, width, height);
    println!("[OCR] Windows OCR: {width}x{height} → {w}x{h}");

    let bitmap = {
        let stream = InMemoryRandomAccessStream::new().map_err(|e| anyhow!("Stream: {e}"))?;
        let encoder_id = BitmapEncoder::BmpEncoderId().map_err(|e| anyhow!("BmpEncoderId: {e}"))?;
        let encoder = BitmapEncoder::CreateAsync(encoder_id, &stream)
            .map_err(|e| anyhow!("Encoder: {e}"))?.get().map_err(|e| anyhow!("Encoder.get: {e}"))?;

        encoder.SetPixelData(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied,
            w, h, 96.0, 96.0, &data).map_err(|e| anyhow!("SetPixelData: {e}"))?;
        encoder.FlushAsync().map_err(|e| anyhow!("Flush: {e}"))?
            .get().map_err(|e| anyhow!("Flush.get: {e}"))?;
        stream.Seek(0).map_err(|e| anyhow!("Seek: {e}"))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| anyhow!("Decoder: {e}"))?.get().map_err(|e| anyhow!("Decoder.get: {e}"))?;
        let bmp = decoder.GetSoftwareBitmapAsync()
            .map_err(|e| anyhow!("GetBitmap: {e}"))?.get().map_err(|e| anyhow!("GetBitmap.get: {e}"))?;
        SoftwareBitmap::Convert(&bmp, BitmapPixelFormat::Bgra8)
            .map_err(|e| anyhow!("Convert: {e}"))?
    };

    // Try: user profile → en-US → ru
    for tag in &["", "en-US", "ru"] {
        let engine = if tag.is_empty() {
            OcrEngine::TryCreateFromUserProfileLanguages().ok()
        } else {
            Language::CreateLanguage(&HSTRING::from(*tag))
                .ok()
                .and_then(|l| OcrEngine::TryCreateFromLanguage(&l).ok())
        };
        if let Some(eng) = engine {
            if let Ok(result) = eng.RecognizeAsync(&bitmap).and_then(|a| a.get()) {
                if let Ok(text) = result.Text() {
                    let t = text.to_string();
                    if !t.trim().is_empty() {
                        let label = if tag.is_empty() { "profile" } else { tag };
                        println!("[OCR] Windows OK ({label}): {}", truncate(&t, 80));
                        return Ok(t);
                    }
                }
            }
        }
    }

    Err(anyhow!("Текст не распознан"))
}

// ============================================================
// Image scaling (nearest-neighbour upscale for small captures)
// ============================================================

fn scale_for_ocr(pixels: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let max_dim = w.max(h);
    if max_dim < 600 { return scale(pixels, w, h, 3); }
    if max_dim < 1200 { return scale(pixels, w, h, 2); }
    (pixels.to_vec(), w, h)
}

/// Nearest-neighbour upscale by an integer factor.
/// Iterates source pixels once and writes each factor×factor block,
/// avoiding per-destination bounds checks.
fn scale(pixels: &[u8], w: u32, h: u32, factor: u32) -> (Vec<u8>, u32, u32) {
    let nw = w * factor;
    let nh = h * factor;
    let src_stride = (w * 4) as usize;
    let dst_stride = (nw * 4) as usize;
    let mut out = vec![0u8; dst_stride * nh as usize];

    for sy in 0..h {
        let src_row = (sy as usize) * src_stride;
        let dst_base_row = (sy * factor) as usize * dst_stride;

        for sx in 0..w {
            let si = src_row + (sx as usize) * 4;
            let pixel = &pixels[si..si + 4];
            let dx_base = (sx * factor) as usize * 4;

            // Write the first row of the factor×factor block.
            let di = dst_base_row + dx_base;
            for fx in 0..factor as usize {
                out[di + fx * 4..di + fx * 4 + 4].copy_from_slice(pixel);
            }
        }

        // Duplicate that first row (factor - 1) more times.
        for fy in 1..factor as usize {
            let src_off = dst_base_row;
            let dst_off = dst_base_row + fy * dst_stride;
            out.copy_within(src_off..src_off + dst_stride, dst_off);
        }
    }

    (out, nw, nh)
}
