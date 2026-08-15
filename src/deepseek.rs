//! DeepSeek chat-completions client.
//!
//! One place that knows the endpoints, the auth header and the shape of the
//! response.  Two callers sit on top of it: the translator, which sends a
//! single instruction-shaped turn, and the ask window, which sends a whole
//! conversation.

use anyhow::{Result, anyhow};
use serde::Deserialize;

const CHAT_URL: &str = "https://api.deepseek.com/chat/completions";
const MODELS_URL: &str = "https://api.deepseek.com/models";

pub const MODEL: &str = "deepseek-chat";

/// Why a key check didn't come back clean.  The settings window shows these
/// differently: a rejected key is the user's problem to fix, an unreachable
/// server is not, and saying "invalid key" when the network is down would be
/// a lie that costs someone an afternoon.
pub enum KeyCheck {
    Valid,
    Rejected,
    Unreachable(String),
}

/// Verifies a key without spending any tokens — the models endpoint answers
/// 200 for a good key and 401 for a bad one.
pub fn check_key(api_key: &str) -> KeyCheck {
    let key = api_key.trim();
    if key.is_empty() {
        return KeyCheck::Rejected;
    }

    match ureq::get(MODELS_URL)
        .timeout(std::time::Duration::from_secs(12))
        .set("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(_) => KeyCheck::Valid,
        Err(ureq::Error::Status(401 | 403, _)) => KeyCheck::Rejected,
        Err(ureq::Error::Status(code, _)) => KeyCheck::Unreachable(format!("HTTP {code}")),
        Err(e) => KeyCheck::Unreachable(e.to_string()),
    }
}

/// One turn of a conversation.  `role` is "system", "user" or "assistant".
pub struct Turn {
    pub role: &'static str,
    pub content: String,
}

impl Turn {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system",
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant",
            content: content.into(),
        }
    }
}

/// Sends `turns` and returns the assistant's reply, trimmed.
///
/// Blocking — every caller runs it on a worker thread.  The timeout is
/// generous because a long answer legitimately takes a while, and cutting one
/// off mid-sentence is worse than waiting.
pub fn chat(api_key: &str, turns: &[Turn], temperature: f32, timeout_secs: u64) -> Result<String> {
    let req = ChatRequest {
        model: MODEL,
        messages: turns
            .iter()
            .map(|t| ChatMessage {
                role: t.role,
                content: &t.content,
            })
            .collect(),
        temperature,
        stream: false,
    };

    let resp = ureq::post(CHAT_URL)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .set("Authorization", &format!("Bearer {}", api_key.trim()))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(&req).map_err(|e| anyhow!("JSON: {e}"))?)
        .map_err(|e| anyhow!("Сеть: {e}"))?;

    let body: ChatResponse = resp.into_json().map_err(|e| anyhow!("JSON: {e}"))?;

    let reply = body
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| anyhow!("пустой ответ"))?;

    if reply.is_empty() {
        return Err(anyhow!("пустой ответ"));
    }
    Ok(reply)
}

// ============================================================
// Wire format
// ============================================================

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
