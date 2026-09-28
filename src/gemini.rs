//! Gemini (Google Generative Language) client for the ask window.
//!
//! One model covers what the ask window used three paths for: it answers text,
//! it looks at an attached picture natively, and it searches the web itself
//! through Google Search grounding.  DeepSeek stays in the tree for the
//! translator, untouched — this is only what the spotlight talks to.
//!
//! The classic `:generateContent` endpoint is used rather than the newer
//! Interactions API: it takes an inline base64 image directly, with no separate
//! upload step, which is exactly what a one-shot question about a screenshot
//! wants.

use crate::screenshot;
use anyhow::{Result, anyhow};
use base64::Engine;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// The model.  Flash is the free-tier workhorse: fast, generous limits, and it
/// still sees images and grounds answers on search.  A key that isn't cleared
/// for this exact id comes back with a plain error the ask window shows as-is,
/// which is the honest way to find out rather than guessing here.
pub const MODEL: &str = "gemini-3.6-flash";

/// Longest edge an image is sent at.  Gemini tiles images down internally, so
/// full resolution only buys a slower upload; this is smaller than Claude's cap
/// for the same reason.
pub const MAX_EDGE: u32 = 2048;

/// Where a turn's file lives.
#[derive(Clone)]
pub enum Data {
    /// Bytes carried in the request itself.  Simple and one round trip, but the
    /// whole request has to stay under about 20 MB.
    Inline(Arc<Vec<u8>>),
    /// A file already uploaded through the Files API, referenced by URI.  This
    /// is how anything too big to inline gets sent.
    Uploaded(String),
}

/// A file sent with a turn, and its MIME type.  Gemini takes images, PDFs, text
/// and more, so it is deliberately not image-only.  Cloned rather than moved
/// because every follow-up resends the whole conversation, files included — the
/// API is stateless.
#[derive(Clone)]
pub struct Attach {
    pub data: Data,
    pub mime: String,
}

/// One turn of the conversation.  `role` is "user" or "model" — Gemini's name
/// for the assistant side, and the reason this can't just reuse DeepSeek's
/// "assistant".  The system prompt is passed separately, as this API wants it.
pub struct Turn {
    pub role: &'static str,
    pub text: String,
    pub files: Vec<Attach>,
}

/// One web source the grounded answer leaned on.
pub struct Source {
    pub title: String,
    pub uri: String,
}

/// A reply plus the sources search turned up, if any.  Kept apart so the caller
/// decides how to present them — the answer goes in the transcript, the sources
/// get a labelled block under it in the UI language.
pub struct Answer {
    pub text: String,
    pub sources: Vec<Source>,
}

/// Why a key check didn't come back clean.  The settings window shows these
/// apart: a rejected key is the user's to fix, an unreachable server is not.
pub enum KeyCheck {
    Valid,
    Rejected,
    Unreachable(String),
}

/// Verifies a key without spending any quota — asking the metadata endpoint for
/// this exact model answers 200 when the key can see it, and 400/403/404 when it
/// can't.  It confirms the key is valid and the model is visible to it; it does
/// *not* say anything about remaining request quota, which only a real call can,
/// and which shows up at ask time instead.
pub fn check_key(api_key: &str) -> KeyCheck {
    let key = api_key.trim();
    if key.is_empty() {
        return KeyCheck::Rejected;
    }

    let url = format!("{BASE}/{MODEL}");
    match ureq::get(&url)
        .timeout(Duration::from_secs(12))
        .set("x-goog-api-key", key)
        .call()
    {
        Ok(_) => KeyCheck::Valid,
        Err(ureq::Error::Status(400 | 401 | 403 | 404, _)) => KeyCheck::Rejected,
        Err(ureq::Error::Status(code, _)) => KeyCheck::Unreachable(format!("HTTP {code}")),
        Err(e) => KeyCheck::Unreachable(e.to_string()),
    }
}

/// Largest file still worth putting in the request itself.  Above this the
/// whole request would approach the API's ~20 MB ceiling once base64 has
/// inflated the bytes by a third, so it goes through the Files API instead.
pub const INLINE_LIMIT: usize = 6 * 1024 * 1024;

const UPLOAD_URL: &str = "https://generativelanguage.googleapis.com/upload/v1beta/files";
const FILES_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Uploads a file through the Files API and returns the URI a turn can point at.
///
/// Two round trips, the protocol Google's resumable upload wants: the first asks
/// where to put the bytes, the second sends them and finalises.  Blocking, and
/// only ever called from a worker thread.
///
/// Uploaded files live about 48 hours on Google's side, which outlasts any
/// conversation this window holds.
pub fn upload(api_key: &str, data: &[u8], mime: &str, name: &str) -> Result<String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key"));
    }

    let start = ureq::post(UPLOAD_URL)
        .timeout(Duration::from_secs(60))
        .set("x-goog-api-key", key)
        .set("X-Goog-Upload-Protocol", "resumable")
        .set("X-Goog-Upload-Command", "start")
        .set("X-Goog-Upload-Header-Content-Length", &data.len().to_string())
        .set("X-Goog-Upload-Header-Content-Type", mime)
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({ "file": { "display_name": name } }))
        .map_err(describe)?;

    let url = start
        .header("x-goog-upload-url")
        .ok_or_else(|| anyhow!("Gemini: upload URL missing"))?
        .to_string();

    // The upload itself gets a long timeout: this is the leg that actually moves
    // the megabytes, over whatever connection the user happens to have.
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(600))
        .set("Content-Length", &data.len().to_string())
        .set("X-Goog-Upload-Offset", "0")
        .set("X-Goog-Upload-Command", "upload, finalize")
        .send_bytes(data)
        .map_err(describe)?;

    let info: UploadReply = resp
        .into_json()
        .map_err(|e| anyhow!("Gemini upload JSON: {e}"))?;
    let file = info.file.ok_or_else(|| anyhow!("Gemini: upload returned no file"))?;
    if file.uri.is_empty() {
        return Err(anyhow!("Gemini: upload returned no URI"));
    }

    // Video and audio are transcoded before they can be used; everything else
    // comes back ACTIVE right away.  Asking too early gets the file rejected as
    // "not in an ACTIVE state", so wait it out rather than fail.
    if file.state == "PROCESSING" && !file.name.is_empty() {
        wait_active(key, &file.name)?;
    }

    Ok(file.uri)
}

/// Polls an uploaded file until the server finishes processing it.
fn wait_active(key: &str, name: &str) -> Result<()> {
    let url = format!("{FILES_BASE}/{name}");
    // Up to about two minutes, which covers the clips this window is ever
    // handed; past that something is wrong and saying so beats hanging.
    for _ in 0..60 {
        std::thread::sleep(Duration::from_secs(2));
        let resp = ureq::get(&url)
            .timeout(Duration::from_secs(20))
            .set("x-goog-api-key", key)
            .call()
            .map_err(describe)?;
        let f: FileInfo = resp.into_json().map_err(|e| anyhow!("Gemini JSON: {e}"))?;
        match f.state.as_str() {
            "ACTIVE" => return Ok(()),
            "FAILED" => return Err(anyhow!("Gemini: file processing failed")),
            _ => {}
        }
    }
    Err(anyhow!("Gemini: file still processing"))
}

/// Encodes BGRA pixels for sending, shrinking them to `MAX_EDGE` first.
pub fn prepare(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    screenshot::encode_png_capped(bgra, width, height, MAX_EDGE)
}

/// Sends the conversation and returns the reply.
///
/// `search` turns on Google Search grounding — the model decides for itself
/// whether a question needs the web.  Blocking; the ask window runs it on a
/// worker thread.
pub fn ask(
    api_key: &str,
    system: &str,
    turns: &[Turn],
    search: bool,
    schema: Option<serde_json::Value>,
    temperature: f32,
    timeout_secs: u64,
) -> Result<Answer> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key"));
    }

    let contents: Vec<serde_json::Value> = turns
        .iter()
        .map(|t| {
            // The files go before the text: the question refers to them, and the
            // model reads the parts in the order they are given.
            let mut parts = Vec::with_capacity(t.files.len() + 1);
            for f in &t.files {
                parts.push(match &f.data {
                    Data::Inline(bytes) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes.as_slice());
                        serde_json::json!({
                            "inline_data": { "mime_type": f.mime, "data": b64 }
                        })
                    }
                    Data::Uploaded(uri) => serde_json::json!({
                        "file_data": { "mime_type": f.mime, "file_uri": uri }
                    }),
                });
            }
            parts.push(serde_json::json!({ "text": t.text }));
            serde_json::json!({ "role": t.role, "parts": parts })
        })
        .collect();

    let mut body = serde_json::json!({
        "system_instruction": { "parts": [ { "text": system } ] },
        "contents": contents,
        "generation_config": { "temperature": temperature },
    });
    if search {
        body["tools"] = serde_json::json!([ { "google_search": {} } ]);
    }
    // Structured output and tools are mutually exclusive on this API, so the
    // caller only ever passes a schema when it isn't asking for search.
    if let Some(schema) = schema {
        body["generation_config"]["response_mime_type"] = serde_json::json!("application/json");
        body["generation_config"]["response_schema"] = schema;
    }

    let url = format!("{BASE}/{MODEL}:generateContent");

    // One retry, and only for a connection that never got established: a flaky
    // link to Google is common enough that failing the question outright on the
    // first dropped packet is the wrong call.  A server that answered — quota,
    // bad request — is not retried; it would say the same thing twice.
    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let r = ureq::post(&url)
            .timeout(Duration::from_secs(timeout_secs))
            .set("x-goog-api-key", key)
            .set("Content-Type", "application/json")
            .send_json(body.clone());
        match r {
            Ok(resp) => break resp,
            Err(ureq::Error::Transport(t)) if attempt == 1 => {
                println!("[gemini] transport error ({t}); retrying once");
            }
            Err(e) => return Err(describe(e)),
        }
    };

    let reply: Reply = resp.into_json().map_err(|e| anyhow!("Gemini JSON: {e}"))?;

    // A blocked prompt comes back as a successful response with no candidate,
    // so it has to be checked before the answer is read.
    if let Some(reason) = reply.prompt_feedback.and_then(|f| f.block_reason) {
        return Err(anyhow!("запрос отклонён ({reason})"));
    }

    let cand = reply
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("пустой ответ"))?;

    let text = cand
        .content
        .map(|c| {
            c.parts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
        .trim()
        .to_string();

    if text.is_empty() {
        // A truncation or a safety stop lands here — say which, so an empty
        // panel isn't the whole of the explanation.
        let why = cand.finish_reason.unwrap_or_else(|| "empty".into());
        return Err(anyhow!("пустой ответ ({why})"));
    }

    let sources = cand
        .grounding
        .map(|g| {
            g.chunks
                .into_iter()
                .filter_map(|c| c.web)
                .filter(|w| !w.uri.is_empty())
                .map(|w| Source {
                    title: if w.title.is_empty() {
                        w.uri.clone()
                    } else {
                        w.title
                    },
                    uri: w.uri,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Answer { text, sources })
}

/// Turns a failed call into something worth showing.  A bare "HTTP 400" sends
/// people to check their network when the server has already said, in the body
/// nobody read, exactly what was wrong — a key not cleared for this model, or a
/// daily quota spent, both of which arrive this way.
fn describe(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let detail = resp
                .into_json::<ApiError>()
                .ok()
                .map(|e| e.error.message)
                .unwrap_or_default();
            if detail.is_empty() {
                anyhow!("Gemini: HTTP {code}")
            } else {
                anyhow!("Gemini: {detail}")
            }
        }
        e => anyhow!("Сеть: {e}"),
    }
}

// ============================================================
// Wire format — request is built inline; these decode the reply.
// Google serialises responses in camelCase, hence the renames.
// ============================================================

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default, rename = "promptFeedback")]
    prompt_feedback: Option<PromptFeedback>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<Content>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
    #[serde(default, rename = "groundingMetadata")]
    grounding: Option<Grounding>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Grounding {
    #[serde(default, rename = "groundingChunks")]
    chunks: Vec<GroundingChunk>,
}

#[derive(Deserialize)]
struct GroundingChunk {
    #[serde(default)]
    web: Option<WebChunk>,
}

#[derive(Deserialize)]
struct WebChunk {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct PromptFeedback {
    #[serde(default, rename = "blockReason")]
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct UploadReply {
    #[serde(default)]
    file: Option<FileInfo>,
}

#[derive(Deserialize)]
struct FileInfo {
    /// Resource name, "files/xyz" — what the status endpoint is addressed by.
    #[serde(default)]
    name: String,
    #[serde(default)]
    uri: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    message: String,
}
