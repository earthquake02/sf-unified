//! AI 协议分支：OpenAI Chat Completions、Anthropic Messages、Google Gemini generateContent。
//!
//! 前端仍发送 OpenAI 形的 `payload`；本模块负责选协议、拼上游 URL、改写请求体，
//! 并把上游响应（含“先非流式再包成 SSE”）译回 OpenAI Chat Completions 形状，
//! 这样 `aiStream` / `aiOnce` 不必分三套解析。

use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
    Google,
}

impl Protocol {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Google => "google",
        }
    }

    pub fn parse_explicit(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "chat" | "chat-completions" | "completions" => Some(Protocol::OpenAi),
            "anthropic" | "claude" | "messages" => Some(Protocol::Anthropic),
            "google" | "gemini" | "generate-content" | "generatecontent" => Some(Protocol::Google),
            _ => None,
        }
    }
}

pub fn detect_protocol(base: &str, model: &str, explicit: &str) -> Protocol {
    if let Some(p) = Protocol::parse_explicit(explicit) {
        return p;
    }
    let b = base.to_ascii_lowercase();
    let m = model.trim().to_ascii_lowercase();
    if b.contains("generativelanguage.googleapis.com") || b.contains("/google/v1beta") {
        return Protocol::Google;
    }
    if b.contains("api.anthropic.com") || b.contains("/anthropic/") {
        return Protocol::Anthropic;
    }
    if is_opencode_inference(base) {
        if m.starts_with("gemini") {
            return Protocol::Google;
        }
        if m.starts_with("claude") || m.starts_with("qwen") {
            return Protocol::Anthropic;
        }
    }
    Protocol::OpenAi
}

pub fn is_opencode_inference(base: &str) -> bool {
    let Some(host) = hostname_of(base) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    if host != "opencode.ai" && !host.ends_with(".opencode.ai") {
        return false;
    }
    path_of(base).starts_with("/inference")
}

fn hostname_of(url: &str) -> Option<String> {
    let rest = strip_scheme(url)?;
    let rest = rest.strip_prefix("//")?;
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    if netloc.is_empty() {
        return None;
    }
    let hostport = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else if let Some(i) = hostport.rfind(':') {
        if hostport[..i].contains(':') {
            hostport
        } else {
            &hostport[..i]
        }
    } else {
        hostport
    };
    Some(host.to_ascii_lowercase())
}

fn strip_scheme(url: &str) -> Option<&str> {
    let idx = url.find(':')?;
    Some(&url[idx + 1..])
}

fn path_of(url: &str) -> String {
    let Some(rest) = strip_scheme(url).and_then(|r| r.strip_prefix("//")) else {
        return String::new();
    };
    let after = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
    let path = after.split(['?', '#']).next().unwrap_or("");
    if path.is_empty() {
        "/".into()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

pub fn trim_slash(base: &str) -> &str {
    base.trim().trim_end_matches('/')
}

fn is_full_chat_url(base: &str) -> bool {
    let b = trim_slash(base).to_ascii_lowercase();
    b.ends_with("/chat/completions")
        || b.ends_with("/messages")
        || b.contains(":generatecontent")
        || b.contains(":streamgeneratecontent")
        || b.ends_with("/responses")
}

pub fn chat_url(base: &str, model: &str, protocol: Protocol, stream: bool) -> String {
    let base = trim_slash(base).to_string();
    if is_full_chat_url(&base) {
        return match protocol {
            Protocol::Google => swap_google_method(&base, stream),
            _ => base,
        };
    }
    match protocol {
        Protocol::OpenAi => {
            if is_opencode_inference(&base) && !path_of(&base).contains("/openai/") {
                format!("{base}/openai/v1/chat/completions")
            } else {
                format!("{base}/chat/completions")
            }
        }
        Protocol::Anthropic => {
            if is_opencode_inference(&base) && !path_of(&base).contains("/anthropic/") {
                format!("{base}/anthropic/v1/messages")
            } else if path_of(&base).ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
        Protocol::Google => {
            let method = if stream { "streamGenerateContent" } else { "generateContent" };
            let model = model.trim();
            let url = if let Some(prefix) = base.find("/models/").map(|i| &base[..i]) {
                // Base already includes /models or /models/{id}
                let after = &base[prefix.len() + "/models".len()..];
                let after = after.trim_start_matches('/');
                if after.is_empty() {
                    format!("{prefix}/models/{model}:{method}")
                } else {
                    format!("{base}:{method}")
                }
            } else if is_opencode_inference(&base) {
                format!("{base}/google/v1beta/models/{model}:{method}")
            } else {
                format!("{base}/models/{model}:{method}")
            };
            if stream && !url.contains("alt=") {
                format!("{url}?alt=sse")
            } else {
                url
            }
        }
    }
}

fn swap_google_method(base: &str, stream: bool) -> String {
    let method = if stream { "streamGenerateContent" } else { "generateContent" };
    let (path, query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let path = if let Some(i) = path.rfind(':') {
        format!("{}:{method}", &path[..i])
    } else {
        format!("{path}:{method}")
    };
    let mut q = query.unwrap_or("").to_string();
    if stream && !q.contains("alt=") {
        if q.is_empty() {
            q = "alt=sse".into();
        } else {
            q = format!("{q}&alt=sse");
        }
    }
    if q.is_empty() {
        path
    } else {
        format!("{path}?{q}")
    }
}

pub fn models_url(base: &str, protocol: Protocol) -> String {
    let base = trim_slash(base).to_string();
    let p = path_of(&base).to_ascii_lowercase();
    if p.ends_with("/models") || p.ends_with("/v1/models") {
        return base;
    }
    if is_opencode_inference(&base) {
        return format!("{base}/v1/models");
    }
    match protocol {
        Protocol::Anthropic => format!("{base}/v1/models"),
        _ => format!("{base}/models"),
    }
}

pub fn wants_stream(payload: &Value) -> bool {
    match payload.get("stream") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64() == Some(1) || n.as_u64() == Some(1),
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true") || s == "1",
        _ => false,
    }
}

pub fn payload_model(payload: &Value) -> String {
    payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn translate_request(protocol: Protocol, payload: &Value) -> Value {
    match protocol {
        Protocol::OpenAi => payload.clone(),
        Protocol::Anthropic => openai_to_anthropic(payload),
        Protocol::Google => openai_to_google(payload),
    }
}

fn messages_of(payload: &Value) -> &[Value] {
    payload.get("messages").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[])
}

fn text_of_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if let Value::String(s) = p {
                    return Some(s.as_str());
                }
                p.get("text").and_then(Value::as_str).or_else(|| p.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| map.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn role_of(msg: &Value) -> &str {
    msg.get("role").and_then(Value::as_str).unwrap_or("user")
}

fn openai_to_anthropic(payload: &Value) -> Value {
    let mut system = String::new();
    let mut messages = Vec::new();
    for msg in messages_of(payload) {
        let role = role_of(msg);
        let text = text_of_content(msg.get("content").unwrap_or(&Value::Null));
        if role == "system" {
            if !system.is_empty() {
                system.push('\n');
            }
            system.push_str(&text);
            continue;
        }
        let r = if role == "assistant" { "assistant" } else { "user" };
        messages.push(json!({"role": r, "content": text}));
    }
    let mut out = Map::new();
    if let Some(m) = payload.get("model") {
        out.insert("model".into(), m.clone());
    }
    if !system.is_empty() {
        out.insert("system".into(), Value::String(system));
    }
    out.insert("messages".into(), Value::Array(messages));
    let max = payload
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("max_tokens").and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()))
        .filter(|n| *n > 0)
        .unwrap_or(4096);
    out.insert("max_tokens".into(), json!(max));
    // 部分新 Anthropic 模型会拒绝 temperature（例如 OpenCode 上的 claude-haiku-5-5）。
    // 温度仍可由用户在附加请求体里自行承担风险；默认翻译不转发该字段。
    Value::Object(out)
}

fn openai_to_google(payload: &Value) -> Value {
    let mut system = String::new();
    let mut contents = Vec::new();
    for msg in messages_of(payload) {
        let role = role_of(msg);
        let text = text_of_content(msg.get("content").unwrap_or(&Value::Null));
        if role == "system" {
            if !system.is_empty() {
                system.push('\n');
            }
            system.push_str(&text);
            continue;
        }
        let r = if role == "assistant" { "model" } else { "user" };
        contents.push(json!({"role": r, "parts": [{"text": text}]}));
    }
    let mut out = Map::new();
    if !system.is_empty() {
        out.insert("systemInstruction".into(), json!({"parts": [{"text": system}]}));
    }
    out.insert("contents".into(), Value::Array(contents));
    let mut gen = Map::new();
    if let Some(t) = payload.get("temperature") {
        gen.insert("temperature".into(), t.clone());
    }
    if let Some(n) = payload
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("max_tokens").and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()))
        .filter(|n| *n > 0)
    {
        gen.insert("maxOutputTokens".into(), json!(n));
    }
    if !gen.is_empty() {
        out.insert("generationConfig".into(), Value::Object(gen));
    }
    Value::Object(out)
}

fn finish_reason(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "stop" | "end_turn" | "stop_sequence" | "" => "stop",
        "length" | "max_tokens" | "max_tokens_stop" => "length",
        "content_filter" | "safety" => "content_filter",
        "tool_use" | "tool_calls" | "function_call" => "tool_calls",
        _ => "stop",
    }
}

pub fn translate_response(protocol: Protocol, upstream: &Value, model: &str) -> Value {
    match protocol {
        Protocol::OpenAi => upstream.clone(),
        Protocol::Anthropic => anthropic_to_openai(upstream, model),
        Protocol::Google => google_to_openai(upstream, model),
    }
}

fn anthropic_to_openai(upstream: &Value, model: &str) -> Value {
    let mut text = String::new();
    if let Some(arr) = upstream.get("content").and_then(Value::as_array) {
        for b in arr {
            if b.get("type").and_then(Value::as_str).unwrap_or("text") == "text" {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
        }
    }
    if text.is_empty() {
        if let Some(t) = upstream.get("text").and_then(Value::as_str) {
            text = t.to_string();
        }
    }
    let fr = upstream.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn");
    let usage = usage_from(
        upstream.get("usage").and_then(|u| u.get("input_tokens")).and_then(Value::as_u64),
        upstream.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_u64),
        upstream.get("usage"),
    );
    openai_completion(
        upstream.get("id").and_then(Value::as_str).unwrap_or(""),
        upstream.get("model").and_then(Value::as_str).unwrap_or(model),
        &text,
        finish_reason(fr),
        usage,
    )
}

fn google_parts_text(val: &Value) -> String {
    let mut text = String::new();
    if let Some(cands) = val.get("candidates").and_then(Value::as_array) {
        if let Some(cand) = cands.first() {
            if let Some(parts) = cand.pointer("/content/parts").and_then(Value::as_array) {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    text
}

fn google_to_openai(upstream: &Value, model: &str) -> Value {
    let text = google_parts_text(upstream);
    let fr = upstream
        .pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
        .unwrap_or("STOP");
    let um = upstream.get("usageMetadata");
    let usage = usage_from(
        um.and_then(|u| u.get("promptTokenCount")).and_then(Value::as_u64),
        um.and_then(|u| u.get("candidatesTokenCount")).and_then(Value::as_u64),
        um,
    );
    openai_completion(
        upstream.get("responseId").and_then(Value::as_str).unwrap_or(""),
        upstream.get("modelVersion").and_then(Value::as_str).unwrap_or(model),
        &text,
        finish_reason(fr),
        usage,
    )
}

fn usage_from(prompt: Option<u64>, completion: Option<u64>, extra: Option<&Value>) -> Value {
    let p = prompt.unwrap_or(0);
    let c = completion.unwrap_or(0);
    let mut u = json!({"prompt_tokens": p, "completion_tokens": c, "total_tokens": p + c});
    if let Some(Value::Object(map)) = extra {
        if let Some(cost) = map.get("cost") {
            u["cost"] = cost.clone();
        }
        if let Some(oc) = map.get("opencode") {
            u["opencode"] = oc.clone();
        }
    }
    u
}

fn openai_completion(id: &str, model: &str, text: &str, finish: &str, usage: Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": finish
        }],
        "usage": usage
    })
}

/// 把已经译好的 OpenAI Chat Completions 对象包成前端 `aiStream` 能读的 SSE。
pub fn openai_as_sse(completion: &Value) -> Vec<u8> {
    let text = completion
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let finish = completion
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let usage = completion.get("usage").cloned().unwrap_or(json!({}));
    let mut out = String::new();
    if !text.is_empty() {
        let chunk = json!({
            "id": completion.get("id").cloned().unwrap_or(json!("")),
            "object": "chat.completion.chunk",
            "model": completion.get("model").cloned().unwrap_or(json!("")),
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": Value::Null}]
        });
        out.push_str("data: ");
        out.push_str(&chunk.to_string());
        out.push_str("\n\n");
    }
    let tail = json!({
        "id": completion.get("id").cloned().unwrap_or(json!("")),
        "object": "chat.completion.chunk",
        "model": completion.get("model").cloned().unwrap_or(json!("")),
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
        "usage": usage
    });
    out.push_str("data: ");
    out.push_str(&tail.to_string());
    out.push_str("\n\n");
    out.push_str("data: [DONE]\n\n");
    out.into_bytes()
}

pub fn normalize_models_list(raw: &Value) -> Value {
    if raw.get("data").and_then(Value::as_array).is_some() {
        return raw.clone();
    }
    let list = if let Some(arr) = raw.as_array() {
        arr.clone()
    } else if let Some(arr) = raw.get("models").and_then(Value::as_array) {
        arr.clone()
    } else if let Some(arr) = raw.get("items").and_then(Value::as_array) {
        arr.clone()
    } else {
        return raw.clone();
    };
    let data: Vec<Value> = list
        .into_iter()
        .filter_map(|m| {
            if let Value::String(id) = m {
                return Some(json!({"id": id}));
            }
            if m.get("id").and_then(Value::as_str).is_some() {
                return Some(m);
            }
            let name = m.get("name").and_then(Value::as_str)?.to_string();
            let mut obj = m;
            obj["id"] = json!(name);
            Some(obj)
        })
        .collect();
    json!({"object": "list", "data": data})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_opencode_gemini_is_google() {
        let b = "https://opencode.ai/inference/go";
        assert_eq!(detect_protocol(b, "gemini-4-argon", ""), Protocol::Google);
        assert_eq!(detect_protocol(b, "kimi-k3.1-pro", ""), Protocol::OpenAi);
        assert_eq!(detect_protocol(b, "claude-haiku-5-5", ""), Protocol::Anthropic);
        assert_eq!(detect_protocol(b, "gemini-4-argon", "openai"), Protocol::OpenAi);
        assert!(is_opencode_inference(b));
        assert!(!is_opencode_inference("https://api.openai.com/v1"));
        assert!(!is_opencode_inference("https://notopencode.ai/inference/go"));
    }

    #[test]
    fn chat_urls_for_opencode_and_official() {
        let go = "https://opencode.ai/inference/go/";
        assert_eq!(
            chat_url(go, "kimi-k3.1-pro", Protocol::OpenAi, false),
            "https://opencode.ai/inference/go/openai/v1/chat/completions"
        );
        assert_eq!(
            chat_url(go, "claude-haiku-5-5", Protocol::Anthropic, false),
            "https://opencode.ai/inference/go/anthropic/v1/messages"
        );
        assert_eq!(
            chat_url(go, "gemini-4-argon", Protocol::Google, false),
            "https://opencode.ai/inference/go/google/v1beta/models/gemini-4-argon:generateContent"
        );
        assert_eq!(
            chat_url(go, "gemini-4-argon", Protocol::Google, true),
            "https://opencode.ai/inference/go/google/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            chat_url("https://api.openai.com/v1", "gpt-4o-mini", Protocol::OpenAi, false),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_url("https://api.anthropic.com", "claude-haiku-5-5", Protocol::Anthropic, false),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            chat_url("https://api.anthropic.com/v1", "claude-haiku-5-5", Protocol::Anthropic, false),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            chat_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini-2.5-flash",
                Protocol::Google,
                false
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            models_url(go, Protocol::OpenAi),
            "https://opencode.ai/inference/go/v1/models"
        );
        assert_eq!(
            models_url("https://api.openai.com/v1", Protocol::OpenAi),
            "https://api.openai.com/v1/models"
        );
        // already-complete OpenAI url stays
        assert_eq!(
            chat_url(
                "https://opencode.ai/inference/go/openai/v1/chat/completions",
                "kimi-k3",
                Protocol::OpenAi,
                false
            ),
            "https://opencode.ai/inference/go/openai/v1/chat/completions"
        );
    }

    #[test]
    fn request_and_response_roundtrip() {
        let src = json!({
            "model": "gemini-4-argon",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "Reply with exactly: pong"}
            ],
            "temperature": 0,
            "max_tokens": 16,
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        let g = openai_to_google(&src);
        assert_eq!(g["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(g["contents"][0]["role"], "user");
        assert_eq!(g["generationConfig"]["maxOutputTokens"], 16);
        assert!(g.get("stream").is_none());

        let a = openai_to_anthropic(&src);
        assert_eq!(a["system"], "sys");
        assert_eq!(a["max_tokens"], 16);
        assert_eq!(a["messages"][0]["role"], "user");
        assert!(a.get("stream").is_none());
        assert!(a.get("temperature").is_none());

        let google_raw = json!({
            "responseId": "r1",
            "modelVersion": "gemini-4-argon",
            "candidates": [{"finishReason": "STOP", "content": {"role": "model", "parts": [{"text": "pong"}]}}],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 1}
        });
        let out = google_to_openai(&google_raw, "gemini-4-argon");
        assert_eq!(out["choices"][0]["message"]["content"], "pong");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 8);

        let ant_raw = json!({
            "id": "msg_1",
            "model": "claude-haiku-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "pong"}],
            "usage": {"input_tokens": 18, "output_tokens": 4}
        });
        let out = anthropic_to_openai(&ant_raw, "claude-haiku-5-5");
        assert_eq!(out["choices"][0]["message"]["content"], "pong");
        let sse = String::from_utf8(openai_as_sse(&out)).unwrap();
        assert!(sse.contains("data: "));
        assert!(sse.contains("pong"));
        assert!(sse.contains("data: [DONE]"));
    }
}
