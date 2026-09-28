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

/// The current name for DeepSeek's fast model, and the one the API lists.
/// `deepseek-chat` still answers as a legacy alias, but it's gone from the
/// model list and the docs, and a retired alias would silently drop every
/// translation to the MyMemory fallback.
///
/// Unlike the old alias, this model reasons before answering unless told not
/// to — see `ChatRequest::thinking`.
pub const MODEL: &str = "deepseek-flash";

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

/// A refusal the caller can name, rather than a network error to pass on.
/// The translator falls back to MyMemory on any error, but it tells the user
/// why — and "the key was rejected" and "the balance ran out" each have a
/// fix, which "DeepSeek is unreachable" doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 401 / 403: the key itself.
    KeyRejected,
    /// 402: the account is out of balance.
    NoBalance,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::KeyRejected => write!(f, "key rejected"),
            Refusal::NoBalance => write!(f, "insufficient balance"),
        }
    }
}

impl std::error::Error for Refusal {}

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
    // Kept for symmetry with the other roles; the translator only ever sends
    // system + user, so nothing calls this today.
    #[allow(dead_code)]
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
    let req = request(turns, temperature);

    let resp = ureq::post(CHAT_URL)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .set("Authorization", &format!("Bearer {}", api_key.trim()))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(&req).map_err(|e| anyhow!("JSON: {e}"))?)
        .map_err(|e| match e {
            ureq::Error::Status(401 | 403, _) => anyhow::Error::new(Refusal::KeyRejected),
            ureq::Error::Status(402, _) => anyhow::Error::new(Refusal::NoBalance),
            e => anyhow!("Сеть: {e}"),
        })?;

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

fn request(turns: &[Turn], temperature: f32) -> ChatRequest<'_> {
    ChatRequest {
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
        thinking: Thinking { kind: "disabled" },
    }
}

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    stream: bool,
    /// Off.  With it on, `deepseek-flash` spent 85 reasoning tokens on
    /// translating "Hello, world" and took twice as long, for the same four
    /// words — thinking buys nothing for a translation or a short answer.
    thinking: Thinking,
}

#[derive(serde::Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The request the API was checked with by hand: the current model name,
    /// and reasoning off.
    #[test]
    fn request_uses_current_model_with_thinking_off() {
        let turns = [Turn::system("Translate."), Turn::user("Hello, world")];
        let v = serde_json::to_value(request(&turns, 0.2)).unwrap();
        assert_eq!(v["model"], "deepseek-flash");
        assert_eq!(v["thinking"], serde_json::json!({ "type": "disabled" }));
        assert_eq!(v["stream"], false);
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "Hello, world");
    }
}
