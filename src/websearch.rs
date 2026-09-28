//! Web search through Tavily.
//!
//! Gemini has a search tool of its own, but Google does not hand it to free-tier
//! keys: a grounded request comes back refused on quota, whatever the model.
//! Google's own Programmable Search is no help either — since January 2026 new
//! engines can only search a list of sites you name, never the whole web.
//!
//! So the window searches for itself.  Tavily is what it uses: a free allowance
//! of a thousand queries a month, no card, and it answers with the readable part
//! of each page already extracted — which is exactly the shape a model wants,
//! and saves fetching and stripping the HTML here.
//!
//! With the key field blank the window simply doesn't search.

use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::time::Duration;

const URL: &str = "https://api.tavily.com/search";

/// How many results to ask for.  Enough for the model to cross-check a fact
/// without burying the real question under a page of snippets.
pub const RESULTS: u8 = 5;

/// Why a key check didn't come back clean.  The settings window shows these
/// apart: a rejected key is the user's to fix, an unreachable server is not.
pub enum KeyCheck {
    Valid,
    Rejected,
    Unreachable(String),
}

/// Verifies a key without spending any of the monthly allowance — the usage
/// endpoint reports the account's own figures rather than running a search, so
/// it answers for a good key and refuses a bad one at no cost.
pub fn check_key(api_key: &str) -> KeyCheck {
    let key = api_key.trim();
    if key.is_empty() {
        return KeyCheck::Rejected;
    }

    match ureq::get("https://api.tavily.com/usage")
        .timeout(Duration::from_secs(12))
        .set("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(_) => KeyCheck::Valid,
        Err(ureq::Error::Status(400 | 401 | 403, _)) => KeyCheck::Rejected,
        Err(ureq::Error::Status(code, _)) => KeyCheck::Unreachable(format!("HTTP {code}")),
        Err(e) => KeyCheck::Unreachable(e.to_string()),
    }
}

/// One result: what the model is told, and what the user is shown as a source.
pub struct Hit {
    pub title: String,
    pub link: String,
    pub snippet: String,
}

/// Runs a search and returns the top hits.
///
/// Blocking — the ask window calls it from the same worker thread that then
/// talks to the model.
pub fn search(api_key: &str, query: &str) -> Result<Vec<Hit>> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(anyhow!("search not configured"));
    }

    let resp = ureq::post(URL)
        // Long enough for the service to fetch and summarise a few pages, short
        // enough that a wedged search doesn't hold the answer hostage — the
        // question still gets answered without it.
        .timeout(Duration::from_secs(12))
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({
            "query": query,
            // "basic" costs one credit; "advanced" costs two and mostly buys
            // depth this window has no room to show.
            "search_depth": "basic",
            "max_results": RESULTS,
        }))
        .map_err(describe)?;

    let body: Reply = resp.into_json().map_err(|e| anyhow!("Search JSON: {e}"))?;

    Ok(body
        .results
        .into_iter()
        .filter(|r| !r.url.is_empty())
        .map(|r| Hit {
            title: if r.title.is_empty() {
                r.url.clone()
            } else {
                r.title
            },
            link: r.url,
            snippet: r.content,
        })
        .collect())
}

/// The hits written out for the model: numbered, with the extract under each,
/// so it can quote a fact and say which source it came from.
pub fn as_context(hits: &[Hit]) -> String {
    let mut out = String::from(
        "Web search results for the user's question, fetched just now. \
         Use them when the question needs current information, and say plainly \
         if they don't answer it:\n",
    );
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {} — {}\n{}\n",
            i + 1,
            h.title,
            h.link,
            h.snippet
        ));
    }
    out
}

/// Turns a failed call into something worth showing.  A spent monthly allowance
/// and a mistyped key both answer with a message that says so, and repeating it
/// beats "HTTP 401".
fn describe(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let detail = resp
                .into_json::<ApiError>()
                .ok()
                .and_then(|e| e.message())
                .unwrap_or_default();
            if detail.is_empty() {
                anyhow!("Search: HTTP {code}")
            } else {
                anyhow!("Search: {detail}")
            }
        }
        e => anyhow!("Search: {e}"),
    }
}

// ============================================================
// Wire format
// ============================================================

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// The readable extract Tavily pulled from the page.
    #[serde(default)]
    content: String,
}

/// Errors come back under more than one name depending on what went wrong, so
/// both are accepted rather than losing the message to a missing field.
#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    detail: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
}

impl ApiError {
    fn message(self) -> Option<String> {
        if let Some(e) = self.error {
            return Some(e);
        }
        match self.detail? {
            serde_json::Value::String(s) => Some(s),
            // FastAPI-style: {"detail": {"error": "..."}}
            serde_json::Value::Object(map) => map
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    }
}
