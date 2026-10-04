// Chat clients — Anthropic (the original integration, with web search) and any
// OpenAI-compatible endpoint (DeepSeek, Zhipu GLM, OpenRouter, OpenAI, or a
// custom base URL): multi-turn chat, files sent as inline text or image blocks.
//
// Everything happens here rather than in the island: API keys never leave the
// OS keyring, and file bytes never cross the IPC boundary.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secrets;

const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;
/// Images go out as base64 data URLs; anything larger is skipped (base64 adds 4/3).
const MAX_INLINE_IMAGE: u64 = 4_000_000;

pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const DEFAULT_PROVIDER: &str = "anthropic";
/// Credential Manager key for the custom provider's key, when it has one.
pub const CUSTOM_KEY: &str = "custom-api-key";

/// A cloud provider the chat can talk to. Everything except Anthropic speaks
/// the OpenAI chat-completions dialect.
struct CloudProvider {
    id: &'static str,
    /// Name used in the error messages shown in the island.
    name: &'static str,
    /// OpenAI-compatible base URL. `None` → the Anthropic Messages path.
    base_url: Option<&'static str>,
    /// Credential Manager key holding the API key.
    key_name: &'static str,
}

const PROVIDERS: &[CloudProvider] = &[
    CloudProvider { id: "anthropic", name: "Claude", base_url: None, key_name: "anthropic-api-key" },
    CloudProvider { id: "deepseek", name: "DeepSeek", base_url: Some("https://api.deepseek.com"), key_name: "deepseek-api-key" },
    CloudProvider { id: "zhipu", name: "GLM", base_url: Some("https://open.bigmodel.cn/api/paas/v4"), key_name: "zhipu-api-key" },
    CloudProvider { id: "openrouter", name: "OpenRouter", base_url: Some("https://openrouter.ai/api/v1"), key_name: "openrouter-api-key" },
    CloudProvider { id: "openai", name: "OpenAI", base_url: Some("https://api.openai.com/v1"), key_name: "openai-api-key" },
];

/// The `custom` provider points at any OpenAI-compatible endpoint (a proxy, a
/// self-hosted vLLM, Ollama's `/v1`…) and has no entry in the table above.
fn is_known_provider(id: &str) -> bool {
    id == "custom" || PROVIDERS.iter().any(|p| p.id == id)
}

/// Lock-free history helpers — the same operations on either store.
fn history_is_empty(store: &Mutex<Vec<Value>>) -> bool {
    store.lock().unwrap().is_empty()
}

fn history_push(store: &Mutex<Vec<Value>>, message: Value) {
    store.lock().unwrap().push(message);
}

fn history_pop(store: &Mutex<Vec<Value>>) {
    store.lock().unwrap().pop();
}

fn history_snapshot(store: &Mutex<Vec<Value>>) -> Vec<Value> {
    store.lock().unwrap().clone()
}

#[derive(Default)]
pub struct Chat {
    /// Multi-turn history in Anthropic block format (tool_use blocks included,
    /// which the web search continuation needs).
    anthropic: Mutex<Vec<Value>>,
    /// Multi-turn history in OpenAI string format, for every other provider.
    openai: Mutex<Vec<Value>>,
}

impl Chat {
    pub fn reset(&self) {
        self.anthropic.lock().unwrap().clear();
        self.openai.lock().unwrap().clear();
    }
}

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    chat: &Chat,
    provider_id: &str,
    model: &str,
    custom_base_url: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    if !is_known_provider(provider_id) {
        return Err(format!("Unknown chat provider {provider_id}. Open settings."));
    }

    if provider_id == "custom" {
        let base = custom_base_url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err("No base URL for the custom provider. Open settings.".into());
        }
        // Local servers need no key; Bearer only goes out when one is stored.
        let key = secrets::get(CUSTOM_KEY).unwrap_or_default();
        return send_openai(chat, "Chat", &base, &key, model, query, context, false).await;
    }

    let def = PROVIDERS.iter().find(|p| p.id == provider_id).unwrap();
    let key = secrets::get(def.key_name)
        .ok_or_else(|| format!("{} API key missing. Open settings.", def.name))?;

    match def.base_url {
        None => send_anthropic(chat, &key, model, query, context).await,
        Some(base) => {
            // OpenAI's newest models want the renamed token limit; every other
            // OpenAI-compatible endpoint still speaks "max_tokens".
            send_openai(chat, def.name, base, &key, model, query, context, def.id == "openai")
                .await
        }
    }
}

/// The Anthropic Messages path — web search included, tool_use history kept.
async fn send_anthropic(
    chat: &Chat,
    key: &str,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let history = &chat.anthropic;
    let mut content: Vec<Value> = Vec::new();

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if history_is_empty(history) {
        match &context {
            Some(ChatContext::File { name, path }) => {
                if let Some(block) = file_block(path) {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));

    history_push(history, json!({ "role": "user", "content": content }));

    let body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": SYSTEM_PROMPT,
        "tools": [{ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }],
        "fallbacks": "default",
        "messages": history_snapshot(history),
    });

    let response = match call_anthropic(key, &body).await {
        Ok(v) => v,
        Err(err) => {
            history_pop(history); // keep the history consistent with what the model saw
            return Err(err);
        }
    };

    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        history_pop(history);
        let why = response
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .unwrap_or("Claude declined this one.");
        return Err(why.to_string());
    }

    let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
        history_pop(history);
        return Err("Unexpected API response.".into());
    };

    // Store the whole content — tool_use / tool_result blocks included — so the
    // next turn has the right context.
    history_push(history, json!({ "role": "assistant", "content": blocks.clone() }));

    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply { text })
}

/// The OpenAI-compatible path shared by DeepSeek, Zhipu GLM, OpenRouter, OpenAI
/// and the custom provider: one `chat/completions` call per turn, history kept
/// as plain strings. No web search — that tool is Anthropic-only.
async fn send_openai(
    chat: &Chat,
    name: &str,
    base_url: &str,
    key: &str,
    model: &str,
    query: String,
    context: Option<ChatContext>,
    newest_token_field: bool,
) -> Result<ChatReply, String> {
    let history = &chat.openai;
    let first = history_is_empty(history);

    history_push(
        history,
        json!({ "role": "user", "content": oa_content(&query, &context, first) }),
    );

    let mut messages = vec![json!({ "role": "system", "content": SYSTEM_PROMPT })];
    messages.extend(history_snapshot(history));

    let mut body = json!({ "model": model, "messages": messages, "stream": false });
    if newest_token_field {
        body["max_completion_tokens"] = json!(MAX_TOKENS);
    } else {
        body["max_tokens"] = json!(MAX_TOKENS);
    }

    let response = match call_openai(name, &chat_completions_url(base_url), key, &body).await {
        Ok(v) => v,
        Err(err) => {
            history_pop(history); // keep the history consistent with what the model saw
            return Err(err);
        }
    };

    let Some(text) = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(strip_think)
    else {
        history_pop(history);
        return Err("Unexpected API response.".into());
    };
    let text = text.trim().to_string();

    if text.is_empty() {
        history_pop(history);
        return Err("No response text.".into());
    }
    history_push(history, json!({ "role": "assistant", "content": text }));
    Ok(ChatReply { text })
}

/// The user turn: the question, with the first-turn file/window context folded
/// in. A string, or an array of parts when an image rides along.
fn oa_content(query: &str, context: &Option<ChatContext>, first: bool) -> Value {
    let mut text = String::new();
    let mut image: Option<Value> = None;

    if first {
        match context {
            Some(ChatContext::File { name, path }) => {
                if let Some(part) = oa_image_part(path) {
                    image = Some(part);
                    text.push_str(&format!("File: {name}\n"));
                } else if let Some(contents) = text_file(path) {
                    text.push_str(&format!("File: {name}\nFile contents:\n{contents}\n\n"));
                } else {
                    text.push_str(&format!("File: {name}\n"));
                }
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut ctx = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    ctx.push_str(&format!(", URL: {url}"));
                }
                text.push_str(&ctx);
                text.push_str("\n\n");
            }
            None => {}
        }
    }
    text.push_str(query);

    match image {
        Some(part) => json!([{ "type": "text", "text": text }, part]),
        None => json!(text),
    }
}

/// Small text and code files ride along inline; anything larger is skipped.
fn text_file(path: &str) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Images go out as base64 data URLs — the OpenAI vision format. Models without
/// vision (DeepSeek for one) answer with an API error, which the island shows.
fn oa_image_part(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };

    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_INLINE_IMAGE {
        return None;
    }
    Some(json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{}", base64(&bytes)) },
    }))
}

/// `{base}/chat/completions`, tolerating a base that already ends with the path.
fn chat_completions_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// Reasoning models (DeepSeek-R1, GLM thinking…) sometimes wrap their chain of
/// thought in a think block, like the local servers on macOS. Hidden while the
/// block is open, removed from the final answer.
fn strip_think(text: &str) -> String {
    let open = concat!("<", "think>");
    let close = concat!("</", "think>");
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(close) {
            Some(end) => rest = &after[end + close.len()..],
            None => return out, // still open: nothing of it is visible yet
        }
    }
    out.push_str(rest);
    out
}

/// The OpenAI-compatible endpoint — Bearer key, standard body, no extras.
async fn call_openai(name: &str, url: &str, key: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let mut request = client.post(url).header("content-type", "application/json");
    if !key.is_empty() {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let response = request
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                let err = v.get("error")?;
                err.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| err.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("{name} API {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// The Anthropic endpoint — version header, beta fallback, x-api-key.
async fn call_anthropic(key: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client
        .post(ANTHROPIC_ENDPOINT)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface the API's own message, which is what makes a bad key obvious.
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Claude API {status}: {detail}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("Bad API response: {e}"))
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
fn file_block(path: &str) -> Option<Value> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = std::fs::read(path).ok()?;
        return Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        }));
    }

    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_INLINE_TEXT {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, chat_completions_url, is_known_provider, strip_think, PROVIDERS};

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn strip_think_removes_reasoning_blocks() {
        let open = concat!("<", "think>");
        let close = concat!("</", "think>");
        let input = format!("visible{open}hidden reasoning{close}still visible");
        assert_eq!(strip_think(&input), "visiblestill visible");
        // A block that never closes hides the tail, like macOS does.
        let input = format!("start{open}not shown");
        assert_eq!(strip_think(&input), "start");
        assert_eq!(strip_think("plain answer"), "plain answer");
    }

    #[test]
    fn chat_completions_url_appends_the_path() {
        assert_eq!(
            chat_completions_url("https://api.deepseek.com"),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        // A base that already ends with the path stays untouched.
        assert_eq!(
            chat_completions_url("https://host/v1/chat/completions"),
            "https://host/v1/chat/completions"
        );
    }

    #[test]
    fn providers_reference_known_secret_keys() {
        for p in PROVIDERS {
            assert!(crate::secrets::KNOWN_KEYS.contains(&p.key_name));
        }
        assert!(is_known_provider("custom"));
        assert!(is_known_provider("deepseek"));
        assert!(!is_known_provider("nope"));
    }
}
