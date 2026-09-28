//! Eyes — the Anthropic Messages API, for questions about a picture.
//!
//! DeepSeek answers text and only text: its API has nowhere to put an image,
//! so no amount of prompting makes it see one.  When a key is set here and the
//! ask window has something attached, the conversation goes to Claude instead,
//! image and all.  Without a key the window falls back to OCR — it reads the
//! text out of the picture locally and hands that to DeepSeek, which covers
//! screenshots of errors, code and documents but not what is *drawn*.
//!
//! Only the picture decides which way a question goes.  Plain text stays on
//! DeepSeek whether a key is set here or not: that is the cheap backend the
//! user is paying for, and quietly moving every question onto the expensive
//! one because a key exists would be a bill they didn't ask for.

use crate::screenshot;
use anyhow::{Result, anyhow};
use base64::Engine;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const URL: &str = "https://api.anthropic.com/v1/messages";

/// The API version this code is written against.  Anthropic pins behaviour to
/// it, so it is a constant rather than something to keep bumping.
const API_VERSION: &str = "2023-06-01";

/// The model.  Opus is the capable end of the range and the right default for
/// "what is wrong with this screenshot" — the questions people actually point
/// a window like this at are the ones a weaker model gets subtly wrong.
pub const MODEL: &str = "claude-opus-5";

/// How hard the model works before answering.  This window is interactive and
/// the user is watching it, so not the ceiling; `medium` is the level that
/// reads a chart properly without spending half a minute on it.  `high` and
/// `xhigh` exist above it if an answer ever seems thin.
const EFFORT: &str = "medium";

/// Ceiling on the reply, not a target — the model is asked for a short answer
/// in the prompt.  It has to cover thinking as well as text, so a tight number
/// here would truncate a reply mid-sentence rather than save anything.
const MAX_TOKENS: u32 = 16000;

/// Safety classifiers can decline a request outright — a screenshot of a
/// security tool is enough to do it.  This asks the API to re-run the refused
/// request on another model instead of handing back nothing, which turns a
/// dead end into an answer without any retry logic here.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Longest edge an image is sent at.  Anything larger is shrunk first: the
/// model works at this resolution anyway, so the only thing full size buys is
/// a slower upload.
pub const MAX_EDGE: u32 = 2576;

/// One turn of the conversation.  `role` is "user" or "assistant"; the system
/// prompt is passed separately, as this API wants it.
pub struct Turn {
    pub role: &'static str,
    pub text: String,
    /// PNG bytes shown alongside the text.  Shared because every follow-up
    /// resends the whole conversation, image included — the API is stateless,
    /// and dropping the picture after the first question would leave the model
    /// answering about something it can no longer see.
    pub image: Option<Arc<Vec<u8>>>,
}

/// Encodes BGRA pixels for sending, shrinking them to `MAX_EDGE` first.
pub fn prepare(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    screenshot::encode_png_capped(bgra, width, height, MAX_EDGE)
}

/// Sends the conversation and returns the reply, trimmed.
///
/// Blocking; the ask window runs it on a worker thread.
pub fn ask(api_key: &str, system: &str, turns: &[Turn], timeout_secs: u64) -> Result<String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(anyhow!("no API key"));
    }

    let messages: Vec<serde_json::Value> = turns
        .iter()
        .map(|t| {
            // The image goes before the text: the question refers to it, and
            // the model reads the content in the order it is given.
            let mut content = Vec::with_capacity(2);
            if let Some(png) = &t.image {
                let b64 = base64::engine::general_purpose::STANDARD.encode(png.as_slice());
                content.push(serde_json::json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": b64 },
                }));
            }
            content.push(serde_json::json!({ "type": "text", "text": t.text }));
            serde_json::json!({ "role": t.role, "content": content })
        })
        .collect();

    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "output_config": { "effort": EFFORT },
        "fallbacks": "default",
        "messages": messages,
    });

    let resp = ureq::post(URL)
        .timeout(Duration::from_secs(timeout_secs))
        .set("x-api-key", key)
        .set("anthropic-version", API_VERSION)
        .set("anthropic-beta", FALLBACK_BETA)
        .set("content-type", "application/json")
        .send_json(body)
        .map_err(describe)?;

    let reply: Reply = resp
        .into_json()
        .map_err(|e| anyhow!("Anthropic JSON: {e}"))?;

    // Checked before the content is read, not after: a refusal comes back as
    // a perfectly successful response that simply has nothing in it.
    if reply.stop_reason.as_deref() == Some("refusal") {
        return Err(anyhow!("the model declined to answer about this image"));
    }

    // Thinking blocks arrive alongside the answer and are empty by default;
    // taking every block would prepend a run of blank lines to every reply.
    let text = reply
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        return Err(anyhow!("empty reply"));
    }
    Ok(text)
}

/// Turns a failed call into something worth showing.  A bare "HTTP 400" sends
/// people to check their network when the server has already said, in the body
/// nobody read, exactly what was wrong with the request.
fn describe(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let detail = resp
                .into_json::<ApiError>()
                .ok()
                .map(|e| e.error.message)
                .unwrap_or_default();
            if detail.is_empty() {
                anyhow!("Anthropic: HTTP {code}")
            } else {
                anyhow!("Anthropic: {detail}")
            }
        }
        e => anyhow!("Anthropic: {e}"),
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
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
