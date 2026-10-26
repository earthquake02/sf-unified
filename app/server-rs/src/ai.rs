//! AI 中转：`POST /api/ai/models`、`POST /api/ai/chat`。
//!
//! 口径对照 `sf_server.py` 的 `ai_base_check`、`_ai_headers`、`_ai_relay_error`、
//! `_api_ai_models`、`_api_ai_chat`。出站用 `upstream::build`（套接字超时，不跟随重定向）。
//! 3xx 文案留在这里，不写进 upstream。

use std::sync::OnceLock;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use serde_json::{Number, Value};

use crate::http;
use crate::state::VER;

const AI_MAX_BYTES: usize = 8 * 1024 * 1024;
const REDIRECT_MSG: &str = "AI 服务返回重定向（已拦截，请把 Base 填成最终地址）";
const FIRST_CHUNK_MSG: &str = "AI 服务首块响应超时";

static TIMEOUT: OnceLock<Duration> = OnceLock::new();
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// 启动时读 `SF_AI_TIMEOUT`（缺省 600 秒）。非法值直接失败，与 Python 导入时 `float()` 一样。
pub fn init_timeout() -> Result<(), String> {
    let d = read_timeout_env()?;
    let _ = TIMEOUT.set(d);
    Ok(())
}

pub async fn handle(path: &str, raw: &[u8]) -> Response<Body> {
    handle_with(shared_client(), path, raw).await
}

async fn handle_with(client: &reqwest::Client, path: &str, raw: &[u8]) -> Response<Body> {
    let value = match http::parse_body(raw) {
        Ok((value, _)) => value,
        Err(e) => return http::finish(http::from_bad(e), false),
    };
    // serde_json 的 Map 会按键排序。对话正文要按客户端发来的顺序重放，所以再按原文解析一遍。
    let root = match std::str::from_utf8(raw).ok().and_then(parse_ordered) {
        Some(v @ OrdJson::Object(_)) => v,
        _ => value_to_ord(&value),
    };
    match path {
        "/api/ai/models" => models_inner(client, &root).await,
        "/api/ai/chat" => chat_inner(client, &root).await,
        _ => http::finish(http::err(404, "not-found", "无此端点", false), false),
    }
}

fn shared_client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| crate::upstream::build(timeout_duration()))
}

fn timeout_duration() -> Duration {
    *TIMEOUT.get_or_init(|| read_timeout_env().unwrap_or_else(|msg| panic!("{msg}")))
}

fn read_timeout_env() -> Result<Duration, String> {
    let raw = std::env::var("SF_AI_TIMEOUT").unwrap_or_else(|_| "600".into());
    let secs: f64 = raw.trim().parse().map_err(|_| format!("SF_AI_TIMEOUT 不是数字：{raw}"))?;
    if !secs.is_finite() || secs < 0.0 {
        return Err(format!("SF_AI_TIMEOUT 不是数字：{raw}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

async fn models_inner(client: &reqwest::Client, body: &OrdJson) -> Response<Body> {
    let base = match ai_base_check_str(ord_base_text(body)) {
        Ok(base) => base,
        Err(msg) => return http::finish(http::err(400, "bad-base", msg, false), false),
    };
    let url = format!("{base}/models");
    let headers = match ai_headers(&ord_key(body), &hostname_of(&url), false) {
        Ok(headers) => headers,
        Err(msg) => return upstream_msg(502, "ai-upstream", &msg),
    };
    let resp = match client.get(&url).headers(headers).send().await {
        Ok(resp) => resp,
        Err(e) => return map_pre_response(e),
    };
    buffered_upstream(resp, "application/json").await
}

async fn chat_inner(client: &reqwest::Client, body: &OrdJson) -> Response<Body> {
    let base = match ai_base_check_str(ord_base_text(body)) {
        Ok(base) => base,
        Err(msg) => return http::finish(http::err(400, "bad-base", msg, false), false),
    };
    let Some(payload) = obj_get(body, "payload").filter(|v| matches!(v, OrdJson::Object(_))) else {
        return http::finish(http::err(400, "bad-json", "payload 必须是 JSON 对象", false), false);
    };
    let raw = dump_ord(payload).into_bytes();
    let url = if obj_get(body, "absolute").is_some_and(ord_truthy) {
        base
    } else {
        format!("{base}/chat/completions")
    };
    let headers = match ai_headers(&ord_key(body), &hostname_of(&url), true) {
        Ok(headers) => headers,
        Err(msg) => return upstream_msg(502, "ai-upstream", &msg),
    };
    let mut resp = match client.post(&url).headers(headers).body(raw).send().await {
        Ok(resp) => resp,
        Err(e) => return map_pre_response(e),
    };
    let status = resp.status().as_u16();
    if intercepted_redirect(status, resp.headers()) {
        return http::finish(http::err(502, "ai-upstream", REDIRECT_MSG, false), false);
    }
    if !(200..300).contains(&status) {
        return buffered_upstream(resp, "application/json").await;
    }
    let ctype = ctype_or(resp.headers(), "application/octet-stream");
    let first = match next_data(&mut resp).await {
        Ok(chunk) => chunk,
        Err(e) if e.is_timeout() => {
            return http::finish(http::err(504, "ai-timeout", FIRST_CHUNK_MSG, false), false);
        }
        Err(e) => return map_upstream(e),
    };
    let Some(first) = first else {
        return stream_response(status, ctype, Body::empty());
    };
    // 首块已经读出来了。本 crate 的直接依赖里没有 http_body，写不出自己的 Body::poll_frame。
    // 把首块和后续字节从本机回环再读成 bytes_stream，外层仍用 HTTP/1.0 按关闭连接收尾。
    match pipe_body(resp, first).await {
        Ok(body) => stream_response(status, ctype, body),
        Err(msg) => upstream_msg(502, "ai-upstream", &msg),
    }
}

async fn buffered_upstream(mut resp: reqwest::Response, default_ctype: &'static str) -> Response<Body> {
    let status = resp.status().as_u16();
    if intercepted_redirect(status, resp.headers()) {
        return http::finish(http::err(502, "ai-upstream", REDIRECT_MSG, false), false);
    }
    let ctype = ctype_or(resp.headers(), default_ctype);
    match read_limited(&mut resp, AI_MAX_BYTES).await {
        Ok(body) => passthrough(status, &ctype, body),
        Err(e) => map_pre_response(e),
    }
}

async fn read_limited(resp: &mut reqwest::Response, cap: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut out = Vec::new();
    while out.len() < cap {
        match next_data(resp).await? {
            None => break,
            Some(buf) => {
                let room = cap - out.len();
                if buf.len() > room {
                    out.extend_from_slice(&buf[..room]);
                    break;
                }
                out.extend_from_slice(&buf);
            }
        }
    }
    Ok(out)
}

async fn next_data(resp: &mut reqwest::Response) -> Result<Option<Bytes>, reqwest::Error> {
    loop {
        match resp.chunk().await? {
            Some(buf) if buf.is_empty() => continue,
            other => return Ok(other),
        }
    }
}

fn map_pre_response(e: reqwest::Error) -> Response<Body> {
    // 连不上（含连 127.0.0.1:1 等到套接字超时）在 Python 里是 URLError，不是 TimeoutError，走 502。
    // 已经连上、还没拿到响应，才是 504。
    // 端口没人听（127.0.0.1:1 这种会被防火墙拖到超时）在 Python 里是 URLError → 502。
    // 已经连上、对端不给响应，才是 TimeoutError → 504。reqwest 两种都报 TimedOut，用一次短连接区分。
    if e.is_timeout() && !tcp_still_unreachable(&e) {
        let msg = format!("AI 服务响应超时：{e}");
        return http::finish(http::err(504, "ai-timeout", &msg, false), false);
    }
    map_upstream(e)
}

fn tcp_still_unreachable(e: &reqwest::Error) -> bool {
    let Some(url) = e.url() else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let spec = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(spec.as_str()) else {
        return true;
    };
    let Some(addr) = addrs.next() else {
        return true;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err()
}

fn map_upstream(e: reqwest::Error) -> Response<Body> {
    let msg = format!("{}: {e}", exc_name(&e));
    upstream_msg(502, "ai-upstream", &msg)
}

fn upstream_msg(status: u16, error: &'static str, msg: &str) -> Response<Body> {
    http::finish(http::err(status, error, msg, false), false)
}

fn exc_name(e: &reqwest::Error) -> &'static str {
    if e.is_connect() {
        "ConnectError"
    } else if e.is_timeout() {
        "TimeoutError"
    } else if e.is_decode() || e.is_body() {
        "BodyError"
    } else if e.is_request() {
        "RequestError"
    } else if e.is_builder() {
        "BuilderError"
    } else if e.is_redirect() {
        "RedirectError"
    } else {
        "Error"
    }
}

#[cfg(test)]
fn ai_base_check(base: Option<&Value>) -> Result<String, &'static str> {
    let raw = match base {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    ai_base_check_str(raw)
}

fn ai_base_check_str(raw: &str) -> Result<String, &'static str> {
    let b = raw.trim();
    if b.is_empty() || b.chars().count() > 300 {
        return Err("Base 地址为空或过长");
    }
    if b.chars().any(|c| (c as u32) < 32 || c == '\u{7f}') {
        return Err("Base 地址含控制字符");
    }
    if !is_http_base(b) {
        return Err("Base 地址须为 http(s)://主机[:端口][/路径]");
    }
    Ok(b.trim_end_matches('/').to_string())
}

fn is_http_base(url: &str) -> bool {
    let Some((scheme, rest)) = split_scheme(url) else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    !netloc.is_empty()
}

fn hostname_of(url: &str) -> String {
    let Some((scheme, rest)) = split_scheme(url) else {
        return String::new();
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return String::new();
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return String::new();
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = netloc.rsplit_once('@').map(|(_, host)| host).unwrap_or(netloc);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else if let Some(i) = hostport.rfind(':') {
        &hostport[..i]
    } else {
        hostport
    };
    host.to_ascii_lowercase()
}

fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let idx = url.find(':')?;
    let scheme = &url[..idx];
    if scheme.is_empty() || !scheme.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    if !scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme, &url[idx + 1..]))
}

#[cfg(test)]
fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(value) => match value {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => !number_is_zero(n),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        },
    }
}

#[cfg(test)]
fn number_is_zero(n: &Number) -> bool {
    if n.is_f64() {
        return n.as_f64().unwrap_or(1.0) == 0.0;
    }
    if let Some(i) = n.as_i64() {
        return i == 0;
    }
    if let Some(u) = n.as_u64() {
        return u == 0;
    }
    false
}

/// `str(key or '')`：假值是空串；`True` 是 `True`，不是 `true`。
#[cfg(test)]
fn py_key(v: Option<&Value>) -> String {
    let Some(v) = v else {
        return String::new();
    };
    if !py_truthy(Some(v)) {
        return String::new();
    }
    match v {
        Value::Bool(true) => "True".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => py_number(n),
        other => py_container_str(other),
    }
}

#[cfg(test)]
fn py_container_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => py_number(n),
        Value::String(s) => format!("'{s}'"),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_container_str).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("'{k}': {}", py_container_str(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn ai_headers(key: &str, host: &str, json_body: bool) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let opencode = host.contains("opencode.ai");
    if json_body {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    let auth = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| "HeaderError: 钥匙含无法放入请求头的字符".to_string())?;
    headers.insert(header::AUTHORIZATION, auth);
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    let ua = if opencode { "storyforge-personal/0.1" } else { "storyforge/1.0" };
    headers.insert(header::USER_AGENT, HeaderValue::from_static(ua));
    if opencode {
        headers.insert("x-opencode-session", HeaderValue::from_static("storyforge"));
    }
    Ok(headers)
}

/// urllib 只会跟 301/302/303/307/308，且要有 Location/URI，目标 scheme 是 http(s)/ftp/相对。
fn intercepted_redirect(status: u16, headers: &HeaderMap) -> bool {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return false;
    }
    let Some(loc) = header_text(headers, "location").or_else(|| header_text(headers, "uri")) else {
        return false;
    };
    matches!(location_scheme(&loc).as_str(), "http" | "https" | "ftp" | "")
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn location_scheme(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, _)) = split_scheme(url) else {
        return String::new();
    };
    scheme.to_ascii_lowercase()
}

fn ctype_or(headers: &HeaderMap, default: &str) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

fn passthrough(status: u16, ctype: &str, body: Vec<u8>) -> Response<Body> {
    let len = body.len();
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    Response::builder()
        .status(code)
        .header("Content-Type", header_or(ctype, "application/json"))
        .header("Content-Length", len.to_string())
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Server", format!("SFServer/{VER} "))
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// 未知长度的 HTTP/1.1 会被 hyper 1.12 编成 chunked。对话流改走 HTTP/1.0，按关闭连接收尾。
fn stream_response(status: u16, ctype: String, body: Body) -> Response<Body> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    Response::builder()
        .version(axum::http::Version::HTTP_10)
        .status(code)
        .header("Content-Type", header_or(&ctype, "application/octet-stream"))
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Connection", "close")
        .header("Server", format!("SFServer/{VER} "))
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn header_or(value: &str, default: &'static str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(default))
}

/// 保序 JSON。数字在解析时就按 Python `json.dumps` 归一，避免 serde 把键排成字母序。
#[derive(Debug)]
enum OrdJson {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<OrdJson>),
    Object(Vec<(String, OrdJson)>),
}

fn obj_get<'a>(root: &'a OrdJson, key: &str) -> Option<&'a OrdJson> {
    match root {
        OrdJson::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn ord_base_text(root: &OrdJson) -> &str {
    match obj_get(root, "base") {
        Some(OrdJson::String(s)) => s.as_str(),
        _ => "",
    }
}

fn ord_key(root: &OrdJson) -> String {
    match obj_get(root, "key") {
        Some(v) if ord_truthy(v) => match v {
            OrdJson::Bool(true) => "True".into(),
            OrdJson::String(s) => s.clone(),
            OrdJson::Number(n) => n.clone(),
            other => ord_py_str(other),
        },
        _ => String::new(),
    }
}

fn ord_truthy(v: &OrdJson) -> bool {
    match v {
        OrdJson::Null => false,
        OrdJson::Bool(b) => *b,
        OrdJson::Number(n) => n != "0" && n != "0.0" && n != "-0.0",
        OrdJson::String(s) => !s.is_empty(),
        OrdJson::Array(a) => !a.is_empty(),
        OrdJson::Object(o) => !o.is_empty(),
    }
}

fn ord_py_str(v: &OrdJson) -> String {
    match v {
        OrdJson::Null => "None".into(),
        OrdJson::Bool(true) => "True".into(),
        OrdJson::Bool(false) => "False".into(),
        OrdJson::Number(n) => n.clone(),
        OrdJson::String(s) => format!("'{s}'"),
        OrdJson::Array(items) => {
            let parts: Vec<String> = items.iter().map(ord_py_str).collect();
            format!("[{}]", parts.join(", "))
        }
        OrdJson::Object(items) => {
            let parts: Vec<String> = items.iter().map(|(k, v)| format!("'{k}': {}", ord_py_str(v))).collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn dump_ord(v: &OrdJson) -> String {
    match v {
        OrdJson::Null => "null".into(),
        OrdJson::Bool(true) => "true".into(),
        OrdJson::Bool(false) => "false".into(),
        OrdJson::Number(n) => n.clone(),
        OrdJson::String(s) => py_string(s),
        OrdJson::Array(items) => {
            let parts: Vec<String> = items.iter().map(dump_ord).collect();
            format!("[{}]", parts.join(", "))
        }
        OrdJson::Object(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|(k, v)| format!("{}: {}", py_string(k), dump_ord(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn value_to_ord(v: &Value) -> OrdJson {
    match v {
        Value::Null => OrdJson::Null,
        Value::Bool(b) => OrdJson::Bool(*b),
        Value::Number(n) => OrdJson::Number(py_number(n)),
        Value::String(s) => OrdJson::String(s.clone()),
        Value::Array(items) => OrdJson::Array(items.iter().map(value_to_ord).collect()),
        Value::Object(map) => {
            OrdJson::Object(map.iter().map(|(k, v)| (k.clone(), value_to_ord(v))).collect())
        }
    }
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

fn parse_ordered(text: &str) -> Option<OrdJson> {
    let mut p = JsonParser { s: text.as_bytes(), i: 0 };
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.s.len() {
        return None;
    }
    Some(v)
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Option<OrdJson> {
        self.skip_ws();
        match self.s.get(self.i)? {
            b'n' => self.lit(b"null", OrdJson::Null),
            b't' => self.lit(b"true", OrdJson::Bool(true)),
            b'f' => self.lit(b"false", OrdJson::Bool(false)),
            b'"' => self.string().map(OrdJson::String),
            b'[' => self.array(),
            b'{' => self.object(),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn lit(&mut self, word: &[u8], v: OrdJson) -> Option<OrdJson> {
        if self.s.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Some(v)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<OrdJson> {
        self.i += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(OrdJson::Object(items));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.s.get(self.i) != Some(&b':') {
                return None;
            }
            self.i += 1;
            let val = self.value()?;
            items.push((key, val));
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return None,
            }
        }
        Some(OrdJson::Object(items))
    }

    fn array(&mut self) -> Option<OrdJson> {
        self.i += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(OrdJson::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return None,
            }
        }
        Some(OrdJson::Array(items))
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        while let Some(&b) = self.s.get(self.i) {
            self.i += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.s.get(self.i) == Some(&b'\\') && self.s.get(self.i + 1) == Some(&b'u') {
                                    let save = self.i;
                                    self.i += 2;
                                    if let Some(lo) = self.hex4() {
                                        if (0xDC00..=0xDFFF).contains(&lo) {
                                            let u = 0x10000 + (((cp - 0xD800) << 10) | (lo - 0xDC00));
                                            out.push(char::from_u32(u)?);
                                            continue;
                                        }
                                    }
                                    self.i = save;
                                }
                                out.push('\u{FFFD}');
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(cp)?);
                            }
                        }
                        _ => return None,
                    }
                }
                b if b < 0x20 => return None,
                _ => {
                    self.i -= 1;
                    let rest = std::str::from_utf8(&self.s[self.i..]).ok()?;
                    let ch = rest.chars().next()?;
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
        None
    }

    fn hex4(&mut self) -> Option<u32> {
        if self.i + 4 > self.s.len() {
            return None;
        }
        let txt = std::str::from_utf8(&self.s[self.i..self.i + 4]).ok()?;
        self.i += 4;
        u32::from_str_radix(txt, 16).ok()
    }

    fn number(&mut self) -> Option<OrdJson> {
        let start = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let int_start = self.i;
        if self.s.get(self.i) == Some(&b'0') {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
                return None;
            }
        } else if matches!(self.s.get(self.i), Some(b'1'..=b'9')) {
            while matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        } else {
            return None;
        }
        if self.i == int_start {
            return None;
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            let frac = self.i;
            while matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            if self.i == frac {
                return None;
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let exp = self.i;
            while matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            if self.i == exp {
                return None;
            }
        }
        let token = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        Some(OrdJson::Number(normalize_number(token)?))
    }
}

fn normalize_number(token: &str) -> Option<String> {
    let is_float = token.contains('.') || token.contains('e') || token.contains('E');
    if !is_float {
        let neg = token.starts_with('-');
        let digits = token.trim_start_matches('-').trim_start_matches('0');
        if digits.is_empty() {
            return Some("0".into());
        }
        return Some(if neg { format!("-{digits}") } else { digits.to_string() });
    }
    let f: f64 = token.parse().ok()?;
    Some(py_float(f))
}

fn py_number(n: &Number) -> String {
    if n.is_f64() {
        return py_float(n.as_f64().unwrap_or(0.0));
    }
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    n.to_string()
}

fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f.is_sign_positive() { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    python_exponent(&format!("{f:?}"))
}

/// Rust 的 `{:?}` 与 Python `repr(float)` 有效数字一致，指数补成 `e+16` / `e-07`。
fn python_exponent(debug: &str) -> String {
    let Some((mant, exp)) = debug.split_once(['e', 'E']) else {
        return debug.to_string();
    };
    let Ok(exp_n) = exp.parse::<i32>() else {
        return debug.to_string();
    };
    let sign = if exp_n < 0 { '-' } else { '+' };
    format!("{mant}e{sign}{:02}", exp_n.unsigned_abs())
}

fn py_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

static PIPE: OnceLock<reqwest::Client> = OnceLock::new();

fn pipe_client() -> &'static reqwest::Client {
    PIPE.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .http1_only()
            .no_proxy()
            .build()
            .expect("回环管道初始化失败")
    })
}

async fn pipe_body(upstream: reqwest::Response, first: Bytes) -> Result<Body, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("IoError: {e}"))?;
    let addr = listener.local_addr().map_err(|e| format!("IoError: {e}"))?;
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.set_nodelay(true);
            let _ = write_pipe(&mut sock, &first, upstream).await;
        }
    });
    let local = pipe_client()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .map_err(|e| format!("{}: {e}", exc_name(&e)))?;
    Ok(Body::from_stream(local.bytes_stream()))
}

async fn write_pipe(
    sock: &mut tokio::net::TcpStream,
    first: &Bytes,
    mut upstream: reqwest::Response,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    // 先读完请求再写，避免对端还没发出 GET 就把连接关了。
    read_http_head(sock).await?;
    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        .await?;
    write_chunk(sock, first).await?;
    loop {
        match next_data(&mut upstream).await {
            Ok(Some(buf)) => write_chunk(sock, &buf).await?,
            Ok(None) | Err(_) => break,
        }
    }
    sock.write_all(b"0\r\n\r\n").await?;
    sock.shutdown().await
}

async fn read_http_head(sock: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(());
        }
        if buf.len() > 65536 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "head"));
        }
    }
}

async fn write_chunk(sock: &mut tokio::net::TcpStream, buf: &Bytes) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if buf.is_empty() {
        return Ok(());
    }
    sock.write_all(format!("{:x}\r\n", buf.len()).as_bytes()).await?;
    sock.write_all(buf).await?;
    sock.write_all(b"\r\n").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn base_check_matches_python_words() {
        assert_eq!(ai_base_check(None).unwrap_err(), "Base 地址为空或过长");
        assert_eq!(ai_base_check(Some(&Value::from("   "))).unwrap_err(), "Base 地址为空或过长");
        assert_eq!(ai_base_check(Some(&Value::from(1))).unwrap_err(), "Base 地址为空或过长");
        assert_eq!(ai_base_check(Some(&Value::from("ftp://x"))).unwrap_err(), "Base 地址须为 http(s)://主机[:端口][/路径]");
        assert_eq!(ai_base_check(Some(&Value::from("http://"))).unwrap_err(), "Base 地址须为 http(s)://主机[:端口][/路径]");
        assert_eq!(ai_base_check(Some(&Value::from("http:///x"))).unwrap_err(), "Base 地址须为 http(s)://主机[:端口][/路径]");
        assert_eq!(ai_base_check(Some(&Value::from("http://ho\nst"))).unwrap_err(), "Base 地址含控制字符");
        assert_eq!(ai_base_check(Some(&Value::from("http://host\u{7f}"))).unwrap_err(), "Base 地址含控制字符");
        assert_eq!(ai_base_check(Some(&Value::from("a".repeat(301)))).unwrap_err(), "Base 地址为空或过长");
        assert_eq!(ai_base_check(Some(&Value::from("http://host/v1/"))).unwrap(), "http://host/v1");
        assert_eq!(ai_base_check(Some(&Value::from("HTTP://Host/V1/"))).unwrap(), "HTTP://Host/V1");
        assert_eq!(ai_base_check(Some(&Value::from(" http://host/v1 "))).unwrap(), "http://host/v1");
        assert_eq!(ai_base_check(Some(&Value::from("http://host\n"))).unwrap(), "http://host");
        assert_eq!(ai_base_check(Some(&Value::from("http://host/a/b///"))).unwrap(), "http://host/a/b");
        let ok300 = format!("http://{}", "a".repeat(293));
        assert_eq!(ok300.chars().count(), 300);
        assert!(ai_base_check(Some(&Value::from(ok300.clone()))).is_ok());
    }

    #[test]
    fn dumps_use_python_separators_and_order() {
        let dump = |s: &str| dump_ord(&parse_ordered(s).unwrap());
        assert_eq!(
            dump(r#"{"model":"alpha","messages":[{"role":"user","content":"hi"}],"temperature":0.7}"#),
            r#"{"model": "alpha", "messages": [{"role": "user", "content": "hi"}], "temperature": 0.7}"#
        );
        assert_eq!(
            dump(r#"{"z":1,"a":"中\"文","n":null,"t":true}"#),
            r#"{"z": 1, "a": "中\"文", "n": null, "t": true}"#
        );
        assert_eq!(dump(r#"{"s":"b\nc\t\"\\"}"#), r#"{"s": "b\nc\t\"\\"}"#);
        assert_eq!(dump("1e2"), "100.0");
        assert_eq!(dump("1.0"), "1.0");
        assert_eq!(dump("1"), "1");
        assert_eq!(dump("1e16"), "1e+16");
        assert_eq!(dump("1e-7"), "1e-07");
        assert_eq!(dump("-0.0"), "-0.0");
        assert_eq!(dump("-0"), "0");
        assert_eq!(dump("0.30000000000000004"), "0.30000000000000004");
    }

    #[test]
    fn key_and_absolute_follow_python_truth() {
        assert_eq!(py_key(None), "");
        assert_eq!(py_key(Some(&Value::Null)), "");
        assert_eq!(py_key(Some(&Value::Bool(false))), "");
        assert_eq!(py_key(Some(&Value::from(0))), "");
        assert_eq!(py_key(Some(&Value::Bool(true))), "True");
        assert_eq!(py_key(Some(&Value::from("good-key"))), "good-key");
        assert_eq!(py_key(Some(&Value::from(1))), "1");
        assert!(!py_truthy(None));
        assert!(!py_truthy(Some(&Value::from(""))));
        assert!(!py_truthy(Some(&Value::Array(vec![]))));
        assert!(py_truthy(Some(&Value::Bool(true))));
        assert!(py_truthy(Some(&Value::from("0"))));
        assert!(py_truthy(Some(&Value::from(vec![Value::from(1)]))));
    }

    #[test]
    fn opencode_match_is_hostname_substring() {
        assert_eq!(hostname_of("https://API.OpenCode.AI/v1"), "api.opencode.ai");
        assert!(hostname_of("https://notopencode.ai/x").contains("opencode.ai"));
        assert_eq!(hostname_of("http://user:p@Host:80/x"), "host");
        assert_eq!(hostname_of("http://[::1]:8080/v1"), "::1");
        let local = ai_headers("good-key", "127.0.0.1", false).unwrap();
        assert_eq!(local.get(header::USER_AGENT).unwrap(), "storyforge/1.0");
        assert!(local.get("x-opencode-session").is_none());
        assert_eq!(local.get(header::AUTHORIZATION).unwrap(), "Bearer good-key");
        let remote = ai_headers("", "api.opencode.ai", true).unwrap();
        assert_eq!(remote.get(header::USER_AGENT).unwrap(), "storyforge-personal/0.1");
        assert_eq!(remote.get("x-opencode-session").unwrap(), "storyforge");
        assert_eq!(remote.get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(remote.get(header::AUTHORIZATION).unwrap(), "Bearer ");
    }

    #[test]
    fn redirect_only_when_urllib_would_follow() {
        let mut h = HeaderMap::new();
        h.insert("location", HeaderValue::from_static("http://127.0.0.1:9/landed"));
        assert!(intercepted_redirect(302, &h));
        assert!(intercepted_redirect(301, &h));
        assert!(!intercepted_redirect(304, &h));
        assert!(!intercepted_redirect(200, &h));
        h.insert("location", HeaderValue::from_static("/rel"));
        assert!(intercepted_redirect(302, &h));
        h.insert("location", HeaderValue::from_static("javascript:alert(1)"));
        assert!(!intercepted_redirect(302, &h));
        h.remove("location");
        assert!(!intercepted_redirect(302, &h));
    }

    async fn take_json(resp: Response<Body>) -> (u16, Value, axum::http::Version) {
        let status = resp.status().as_u16();
        let version = resp.version();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, version)
    }

    fn client(secs: u64) -> reqwest::Client {
        crate::upstream::build(Duration::from_secs(secs))
    }

    struct Mock {
        addr: std::net::SocketAddr,
        _join: Option<std::thread::JoinHandle<()>>,
    }

    fn spawn_mock<F>(f: F) -> Mock
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let join = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                let _ = sock.set_nodelay(true);
                f(sock);
            }
        });
        Mock { addr, _join: Some(join) }
    }

    fn read_req(sock: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        use std::io::Read;
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).into_owned();
                        let cl = content_len(&head);
                        while buf.len() < i + 4 + cl {
                            match sock.read(&mut tmp) {
                                Ok(0) | Err(_) => break,
                                Ok(m) => buf.extend_from_slice(&tmp[..m]),
                            }
                        }
                        let body = buf[i + 4..].to_vec();
                        return (head, body);
                    }
                }
                Err(_) => break,
            }
        }
        (String::from_utf8_lossy(&buf).into_owned(), Vec::new())
    }

    fn content_len(head: &str) -> usize {
        for line in head.split("\r\n") {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                return v.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn models_passthrough_and_redirect_and_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let land = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        land.set_nonblocking(true).unwrap();
        let land_addr = land.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_nodelay(true);
            let (head, _) = read_req(&mut sock);
            assert!(head.starts_with("GET "), "{head}");
            assert!(head.to_ascii_lowercase().contains("authorization: bearer good-key"), "{head}");
            assert!(head.to_ascii_lowercase().contains("user-agent: storyforge/1.0"), "{head}");
            assert!(!head.to_ascii_lowercase().contains("content-type:"), "{head}");
            let body = "{\"object\": \"list\", \"名\": \"甲\"}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            use std::io::Write;
            let _ = sock.write_all(resp.as_bytes());
        });
        let c = client(2);
        let req = format!(r#"{{"base":"http://{addr}/v1","key":"good-key"}}"#);
        let resp = handle_with(&c, "/api/ai/models", req.as_bytes()).await;
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), "{\"object\": \"list\", \"名\": \"甲\"}".as_bytes());

        let redir = spawn_mock(move |mut sock| {
            let _ = read_req(&mut sock);
            let loc = format!("http://{land_addr}/landed");
            let resp = format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            use std::io::Write;
            let _ = sock.write_all(resp.as_bytes());
        });
        let req = format!(r#"{{"base":"http://{}","key":"k"}}"#, redir.addr);
        let (status, value, version) = take_json(handle_with(&c, "/api/ai/models", req.as_bytes()).await).await;
        assert_eq!(status, 502);
        assert_eq!(version, axum::http::Version::HTTP_11);
        assert_eq!(value["error"], "ai-upstream");
        assert_eq!(value["message"], REDIRECT_MSG);
        std::thread::sleep(Duration::from_millis(200));
        assert!(land.accept().is_err(), "重定向目标不应收到请求");

        let req = br#"{"base":"http://127.0.0.1:1","key":"k"}"#;
        let (status, value, _) = take_json(handle_with(&c, "/api/ai/models", req).await).await;
        assert_eq!(status, 502, "{value}");
        assert_eq!(value["error"], "ai-upstream");

        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hang_addr = hang.local_addr().unwrap();
        std::thread::spawn(move || {
            let (sock, _) = hang.accept().unwrap();
            std::thread::sleep(Duration::from_secs(5));
            drop(sock);
        });
        let raw = format!(r#"{{"base":"http://{hang_addr}","key":"k"}}"#);
        let (status, value, _) = take_json(handle_with(&c, "/api/ai/models", raw.as_bytes()).await).await;
        assert_eq!(status, 504, "{value}");
        assert_eq!(value["error"], "ai-timeout");
        assert!(
            value["message"].as_str().unwrap_or("").starts_with("AI 服务响应超时："),
            "{value}"
        );

        let (status, value, _) = take_json(handle_with(&c, "/api/ai/models", br#"{"base":"ftp://x"}"#).await).await;
        assert_eq!(status, 400);
        assert_eq!(value["message"], "Base 地址须为 http(s)://主机[:端口][/路径]");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_posts_python_json_and_streams_without_chunked() {
        let payload = r#"{"z":1,"a":"中文","messages":[{"role":"user","content":"hi"}]}"#;
        let expect = r#"{"z": 1, "a": "中文", "messages": [{"role": "user", "content": "hi"}]}"#;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let expect_bg: &'static str = expect;
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_nodelay(true);
            let (head, body) = read_req(&mut sock);
            assert!(head.starts_with("POST "), "{head}");
            assert!(head.contains("/v1/chat/completions"), "{head}");
            assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");
            assert!(head.to_ascii_lowercase().contains("authorization: bearer good-key"), "{head}");
            assert_eq!(String::from_utf8_lossy(&body), expect_bg);
            let resp_body = "{\"id\":\"x\",\"choices\":[{\"message\":{\"content\":\"你好\"}}]}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                resp_body.len()
            );
            use std::io::Write;
            let _ = sock.write_all(resp.as_bytes());
        });
        let raw = format!(r#"{{"base":"http://{addr}/v1","key":"good-key","payload":{payload}}}"#);
        let resp = handle_with(&client(2), "/api/ai/chat", raw.as_bytes()).await;
        let status = resp.status().as_u16();
        let ver = resp.version();
        let headers = format!("{:?}", resp.headers());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            (status, ver, bytes.as_ref()),
            (200, axum::http::Version::HTTP_10, "{\"id\":\"x\",\"choices\":[{\"message\":{\"content\":\"你好\"}}]}".as_bytes()),
            "{headers} body {}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(headers.to_ascii_lowercase().contains("connection"));
        assert!(!headers.to_ascii_lowercase().contains("content-length"));

        let (status, value, _) = take_json(
            handle_with(&client(2), "/api/ai/chat", br#"{"base":"http://127.0.0.1:9","key":"k","payload":[]}"#).await,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(value["error"], "bad-json");
        assert_eq!(value["message"], "payload 必须是 JSON 对象");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn absolute_skips_completions_suffix() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let (head, _) = read_req(&mut sock);
            assert!(head.starts_with("POST /v1/systemone "), "{head}");
            assert!(!head.contains("/chat/completions"), "{head}");
            use std::io::Write;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        });
        let raw = format!(
            r#"{{"base":"http://{addr}/v1/systemone","key":"k","absolute":true,"payload":{{"model":"m"}}}}"#
        );
        let resp = handle_with(&client(2), "/api/ai/chat", raw.as_bytes()).await;
        assert_eq!(resp.status(), 200);
        let _ = resp.into_body().collect().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upstream_400_is_raw() {
        let mock = spawn_mock(|mut sock| {
            let _ = read_req(&mut sock);
            use std::io::Write;
            let body = r#"{"error":{"message":"no such model"}}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        let raw = format!(
            r#"{{"base":"http://{}","key":"k","payload":{{"model":"bad"}}}}"#,
            mock.addr
        );
        let resp = handle_with(&client(2), "/api/ai/chat", raw.as_bytes()).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(resp.version(), axum::http::Version::HTTP_11);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], br#"{"error":{"message":"no such model"}}"#);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_chunk_timeout_is_exact_sentence() {
        let mock = spawn_mock(|mut sock| {
            let _ = read_req(&mut sock);
            use std::io::Write;
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            );
            std::thread::sleep(Duration::from_secs(5));
        });
        let raw = format!(
            r#"{{"base":"http://{}","key":"k","payload":{{"model":"hang"}}}}"#,
            mock.addr
        );
        let started = std::time::Instant::now();
        let (status, value, version) = take_json(handle_with(&client(1), "/api/ai/chat", raw.as_bytes()).await).await;
        assert!(started.elapsed() < Duration::from_secs(4), "elapsed {:?}", started.elapsed());
        assert_eq!(status, 504);
        assert_eq!(version, axum::http::Version::HTTP_11);
        assert_eq!(value["error"], "ai-timeout");
        assert_eq!(value["message"], FIRST_CHUNK_MSG);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_drip_uses_idle_timeout_not_total() {
        let mock = spawn_mock(|mut sock| {
            let _ = read_req(&mut sock);
            use std::io::Write;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n");
            let _ = sock.write_all(b"3\r\n\xe7\x94\xb2\r\n");
            std::thread::sleep(Duration::from_millis(1500));
            let _ = sock.write_all(b"3\r\n\xe4\xb9\x99\r\n");
            std::thread::sleep(Duration::from_millis(1500));
            let _ = sock.write_all(b"3\r\n\xe4\xb8\x99\r\n0\r\n\r\n");
        });
        let raw = format!(
            r#"{{"base":"http://{}","key":"k","payload":{{"model":"slow"}}}}"#,
            mock.addr
        );
        let started = std::time::Instant::now();
        let resp = handle_with(&client(2), "/api/ai/chat", raw.as_bytes()).await;
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let elapsed = started.elapsed();
        assert_eq!(bytes.as_ref(), "甲乙丙".as_bytes(), "elapsed {elapsed:?}");
        assert!(elapsed > Duration::from_millis(2500), "应跨过 2 秒总长，实际 {elapsed:?}");
        assert!(elapsed < Duration::from_secs(8), "elapsed {elapsed:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wire_chat_stream_is_http10_close_delimited() {
        let mock = spawn_mock(|mut sock| {
            let _ = read_req(&mut sock);
            use std::io::Write;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
            let _ = sock.write_all(b"3\r\nAAA\r\n");
            std::thread::sleep(Duration::from_millis(150));
            let _ = sock.write_all(b"3\r\nBBB\r\n0\r\n\r\n");
        });
        let raw = format!(
            r#"{{"base":"http://{}","key":"k","payload":{{"model":"s"}}}}"#,
            mock.addr
        );
        let produced = handle_with(&client(2), "/api/ai/chat", raw.as_bytes()).await;
        assert_eq!(produced.version(), axum::http::Version::HTTP_10);
        assert!(produced.headers().get("transfer-encoding").is_none());
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(produced)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/t",
            axum::routing::get({
                let slot = slot.clone();
                move || {
                    let slot = slot.clone();
                    async move { slot.lock().unwrap().take().expect("一次") }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        sock.write_all(b"GET /t HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or_else(|| panic!("no head {text}"));
        assert!(head.starts_with("HTTP/1.0 200"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("transfer-encoding"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("chunked"), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("connection: close")
                || head.to_ascii_lowercase().contains("connection:close"),
            "{head}"
        );
        assert!(head.contains("text/event-stream"), "{head}");
        assert!(head.contains("SFServer/1.2 "), "{head}");
        assert_eq!(body, "AAABBB", "{text}");
    }
}
