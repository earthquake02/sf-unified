//! HTTP 面：回环、令牌、页面、入门图、单文件读、列举、状态与整夹快照。写入在后续票。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response};
use http_body_util::BodyExt;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::gate::{self, Bad};
use crate::state::{App, DEFAULT_DATA_NAME, MAX_BODY, MAX_READ, MAX_SECRET, PING_MARK, VER};
use crate::{fsops, secret, util};

const WELCOME: &str = include_str!("../pages/welcome.html");
const DEBUG_PAGE: &str = include_str!("../pages/debug.html");
/* 片段深化票03：托盘快记窗（Rust 侧建的 quick 窗口指本页；循 welcome.html 内嵌先例，
   页面同样经 inject 拿到 x-sf-token 与端口）。 */
const QUICK_PAGE: &str = include_str!("../pages/quick.html");

// 发布形态（frozen）：程序页与入门功能卡图编译期内嵌（对齐 Python frozen 的 MEI 副本）。
#[cfg(feature = "frozen")]
const APP_PAGE_EMBED: &[u8] = include_bytes!("../../index.html");
#[cfg(feature = "frozen")]
include!(concat!(env!("OUT_DIR"), "/guide_assets.rs"));
// 发布形态（frozen）：内置技能编译期内嵌；空表合法（技能库可空、验证 demo 可摘除，ADR 0020）。
#[cfg(feature = "frozen")]
include!(concat!(env!("OUT_DIR"), "/skills_assets.rs"));

pub(crate) struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
    close: bool,
}

pub async fn dispatch(State(app): State<Arc<App>>, req: Request<Body>) -> Response<Body> {
    let command = req.method().clone();
    match command {
        Method::OPTIONS => {
            return finish(err(405, "method-not-allowed", "本服务不接受 OPTIONS（无 CORS 面）", true), false);
        }
        Method::PUT => {
            return finish(err(405, "method-not-allowed", "本服务不接受 PUT", false), false);
        }
        Method::DELETE => {
            return finish(err(405, "method-not-allowed", "删除请用 POST /api/fs/delete", false), false);
        }
        Method::PATCH => {
            return finish(err(405, "method-not-allowed", "本服务不接受 PATCH", false), false);
        }
        Method::GET | Method::HEAD | Method::POST => {}
        _ => {
            return finish(
                err(501, "http-error", &format!("Unsupported method ('{command}')"), true),
                false,
            );
        }
    }
    let head = command == Method::HEAD;
    let method = if head { Method::GET } else { command };
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    if !host_ok(&headers, &app) {
        return finish(err(403, "forbidden", "请求被拒（host）", method == Method::POST), head);
    }
    if path.starts_with("/api/") {
        if let Some(why) = auth_fail(&headers, &app) {
            return finish(err(403, "forbidden", &format!("请求被拒（{why}）"), method == Method::POST), head);
        }
    }

    let raw = match take_body(req, &method).await {
        Ok(bytes) => bytes,
        Err(reply) => return finish(reply, head),
    };

    // 对话流要先等上游首块再写响应头，不能走同步 route。别的方法仍落到「无此端点」。
    if method == Method::POST && (path == "/api/ai/models" || path == "/api/ai/chat") {
        return crate::ai::handle(&path, &raw).await;
    }

    if path == "/api/search" {
        let reply = if method == Method::GET {
            match crate::search::api_search(&query_first(&query, "q")).await {
                Ok(body) => json(200, &body, false),
                Err(e) => from_bad(e),
            }
        } else {
            err(404, "not-found", "无此端点", false)
        };
        return finish(reply, head);
    }

    // MCP 会等子进程，所以在这里 await。会话是进程级单例，不在 App 上。
    let reply = if method == Method::GET && path == "/api/mcp/status" {
        mcp_status().await
    } else if method == Method::POST && path == "/api/mcp/probe" {
        mcp_probe(&raw).await
    } else if method == Method::POST && path == "/api/mcp/connect" {
        mcp_connect(&raw).await
    } else if method == Method::POST && path == "/api/mcp/disconnect" {
        mcp_disconnect().await
    } else if method == Method::POST && path == "/api/mcp/list" {
        mcp_list().await
    } else if method == Method::POST && path == "/api/mcp/call" {
        mcp_call(&raw).await
    } else {
        route(&app, &method, &path, &query, &raw)
    };
    finish(reply, head)
}

fn with_ok(st: Value) -> Value {
    let mut out = Map::new();
    out.insert("ok".into(), Value::Bool(true));
    if let Value::Object(map) = st {
        for (key, value) in map {
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

async fn mcp_status() -> Reply {
    match crate::mcp::status().await {
        Ok(st) => json(200, &with_ok(st), false),
        Err(e) => from_bad(e),
    }
}

async fn mcp_probe(raw: &[u8]) -> Reply {
    match crate::mcp::probe(raw.to_vec()).await {
        Ok(body) => json(200, &with_ok(body), false),
        Err(e) => from_bad(e),
    }
}

async fn mcp_connect(raw: &[u8]) -> Reply {
    match crate::mcp::connect(raw.to_vec()).await {
        Ok(st) => json(200, &with_ok(st), false),
        Err(e) => from_bad(e),
    }
}

async fn mcp_disconnect() -> Reply {
    match crate::mcp::disconnect().await {
        Ok(st) => json(200, &with_ok(st), false),
        Err(e) => from_bad(e),
    }
}

async fn mcp_list() -> Reply {
    match crate::mcp::list_tools().await {
        Ok(st) => json(200, &with_ok(st), false),
        Err(e) => from_bad(e),
    }
}

async fn mcp_call(raw: &[u8]) -> Reply {
    match crate::mcp::call(raw.to_vec()).await {
        Ok(body) => json(200, &body, false),
        Err(e) => from_bad(e),
    }
}

fn route(app: &App, method: &Method, path: &str, query: &str, raw: &[u8]) -> Reply {
    if path == "/__sf_ping" {
        return text(200, PING_MARK);
    }
    if let Some(name) = path.strip_prefix("/assets/guide/") {
        return guide(name);
    }
    if let Some(rest) = path.strip_prefix("/assets/skills/") {
        return skills(app, rest);
    }
    if path == "/index.html" {
        return match app_page_bytes(app) {
            Ok(bytes) => html(&inject(&bytes, app)),
            Err(e) => from_bad(e),
        };
    }
    if path == "/" || path == "/__sf_setup" || path == "/__sf_debug" {
        let page = if want_debug(path, query) { DEBUG_PAGE } else { WELCOME };
        return html(&inject(page.as_bytes(), app));
    }
    if path == "/__sf_quick" {
        return html(&inject(QUICK_PAGE.as_bytes(), app));
    }
    if !path.starts_with("/api/") {
        return err(404, "not-found", "无此路径", false);
    }
    if path == "/api/sys/status" && method == Method::GET {
        return status(app);
    }
    if path == "/api/sys/config" && method == Method::POST {
        return configure(app, raw);
    }
    if path == "/api/sys/open-data" && method == Method::POST {
        return open_data(app);
    }
    if path == "/api/fs/read" && method == Method::GET {
        return read_file(app, query);
    }
    if path == "/api/disk/read" && method == Method::GET {
        return disk_read(app, query);
    }
    if path == "/api/fs/write" && method == Method::POST {
        return crate::fsapi::write_file(app, raw).map_or_else(from_bad, |v| json(200, &v, false));
    }
    if path == "/api/fs/mkdir" && method == Method::POST {
        return crate::fsapi::mkdir(app, raw).map_or_else(from_bad, |v| json(200, &v, false));
    }
    if path == "/api/fs/delete" && method == Method::POST {
        return crate::fsapi::delete(app, raw).map_or_else(from_bad, |v| json(200, &v, false));
    }
    if path == "/api/fs/list" && method == Method::GET {
        return from_fs(crate::scan::list(app, &query_first(query, "path")));
    }
    if path == "/api/fs/stat" && method == Method::GET {
        return from_fs(crate::scan::stat(app, &query_first(query, "path")));
    }
    if path == "/api/fs/snapshot" && method == Method::GET {
        return from_fs(crate::scan::snapshot(app));
    }
    if path == "/api/secret/read" && method == Method::GET {
        return secret_read(app, query);
    }
    if path == "/api/secret/write" && method == Method::POST {
        return secret_write(app, raw);
    }
    err(404, "not-found", "无此端点", false)
}

fn from_fs(result: Result<Value, Bad>) -> Reply {
    match result {
        Ok(body) => json(200, &body, false),
        Err(e) => from_bad(e),
    }
}

async fn take_body(req: Request<Body>, method: &Method) -> Result<Vec<u8>, Reply> {
    let headers = req.headers();
    let te = headers
        .get("transfer-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    if !te.is_empty() {
        return Err(err(411, "length-required", "请求体必须带 Content-Length（不支持 chunked）", true));
    }
    let Some(cl) = headers.get("content-length") else {
        if method == Method::POST {
            return Err(err(411, "length-required", "请求体必须带 Content-Length", true));
        }
        return Ok(Vec::new());
    };
    let n: i64 = cl.to_str().ok().and_then(|s| s.parse().ok()).unwrap_or(-1);
    if n < 0 || n as usize > MAX_BODY {
        return Err(err(
            413,
            "too-large",
            &format!("请求体超过上限（{MAX_BODY} 字节）"),
            true,
        ));
    }
    if n == 0 || method != Method::POST {
        // GET/HEAD 的体读掉丢掉，避免长连接错位；本票的探活不依赖长连接，长度已核过即可。
        if n > 0 && method != Method::POST {
            let _ = req.into_body().collect().await;
        }
        return Ok(Vec::new());
    }
    match req.into_body().collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            if bytes.len() > MAX_BODY {
                Err(err(413, "too-large", &format!("请求体超过上限（{MAX_BODY} 字节）"), true))
            } else {
                Ok(bytes.to_vec())
            }
        }
        Err(_) => Err(err(400, "http-error", "读取请求体失败", true)),
    }
}

fn host_ok(headers: &HeaderMap, app: &App) -> bool {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    app.hosts.contains(&host)
}

fn auth_fail(headers: &HeaderMap, app: &App) -> Option<&'static str> {
    let got = headers.get("x-sf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct_eq(got.as_bytes(), app.token.as_bytes()) {
        return Some("token");
    }
    if let Some(value) = headers.get("origin") {
        match value.to_str() {
            Ok(org) if org.is_empty() => {}
            Ok(org) if app.origins.contains(&org.to_ascii_lowercase()) => {}
            _ => return Some("origin"),
        }
    }
    if let Some(value) = headers.get("sec-fetch-site") {
        match value.to_str() {
            Ok(sfs) if sfs.is_empty() || sfs == "same-origin" || sfs == "none" => {}
            _ => return Some("site"),
        }
    }
    None
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn want_debug(path: &str, query: &str) -> bool {
    if path == "/__sf_debug" {
        return true;
    }
    let flag = query_first(query, "debug");
    if matches!(flag.as_str(), "1" | "true" | "yes") {
        return true;
    }
    matches!(
        std::env::var("SF_SETUP_DEBUG").unwrap_or_default().trim(),
        "1" | "true" | "yes"
    )
}

fn status(app: &App) -> Reply {
    let snap = app.data_snapshot();
    let (data_root, data_ok) = match &snap {
        Some((path, _)) => (path_text(path), path.is_dir()),
        None => (String::new(), false),
    };
    let (app_hash, app_bytes) = match app_page_bytes(app) {
        Ok(bytes) => (util::sha256_hex(&bytes), bytes.len()),
        Err(_) => (String::new(), 0),
    };
    let body = StatusJson {
        ok: true,
        app: crate::state::APP_TITLE,
        ver: VER,
        port: app.port,
        data_root,
        data_ok,
        configured: snap.is_some(),
        default_data_root: path_text(&app.exe_dir.join(DEFAULT_DATA_NAME)),
        frozen: cfg!(feature = "frozen"),
        app_hash,
        app_bytes,
        lock: match app.lock_snapshot() {
            Some(info) => LockJson { pid: Some(info.pid), port: Some(info.port), at: Some(info.at) },
            None => LockJson { pid: None, port: None, at: None },
        },
    };
    json(200, &body, false)
}

fn configure(app: &App, raw: &[u8]) -> Reply {
    let (body, _) = match parse_body(raw) {
        Ok(body) => body,
        Err(e) => return from_bad(e),
    };
    let raw = match body.get("dataRoot") {
        Some(Value::String(path)) if !path.trim().is_empty() => path.clone(),
        _ => path_text(&app.exe_dir.join(DEFAULT_DATA_NAME)),
    };
    let (path, created) = match gate::check_data_root_created(&raw) {
        Ok(result) => result,
        Err(e) => return from_bad(e),
    };
    let old = app.data_snapshot();
    if let Err(e) = crate::lock::acquire(app, &path) {
        return from_bad(e);
    }
    let new_real = gate::try_realpath(&path).unwrap_or_else(|| path.clone());
    app.install_data(path.clone());
    if let Err(e) = app.persist_data_root(&path) {
        return err(500, "internal", &format!("OSError: {e}"), false);
    }
    if let Some((old_path, old_real)) = old {
        if !crate::lock::same_root(&old_real, &new_real) {
            crate::lock::release(app, &old_path);
        }
    }
    json(200, &serde_json::json!({"ok": true, "dataRoot": path_text(&path), "created": created}), false)
}

fn open_data(app: &App) -> Reply {
    let Some((root, _)) = app.data_snapshot() else {
        return from_bad(gate::bad(409, "no-data-root", "尚未设置可用的数据文件夹"));
    };
    if !root.is_dir() {
        return from_bad(gate::bad(409, "no-data-root", "尚未设置可用的数据文件夹"));
    }
    use std::os::windows::ffi::OsStrExt;
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            5,
        )
    };
    if result as isize <= 32 {
        return from_bad(gate::bad(500, "open-failed", format!("打开失败：ShellExecuteW 错误码 {}", result as isize)));
    }
    json(200, &serde_json::json!({"ok": true, "dataRoot": path_text(&root)}), false)
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        owner: *mut std::ffi::c_void,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> *mut std::ffi::c_void;
}

fn read_file(app: &App, query: &str) -> Reply {
    let rel = query_first(query, "path");
    let (parts, target) = match gate::resolve(app, &rel) {
        Ok(v) => v,
        Err(e) => return from_bad(e),
    };
    let Some((root, _)) = app.data_snapshot() else {
        return from_bad(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    if let Err(e) = gate::guard_secret_read(&root, &target) {
        return from_bad(e);
    }
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err(404, "not-found", "文件不存在", false);
        }
        Err(e) => return err(500, "read-failed", &format!("读文件失败：{e}"), false),
    };
    if meta.is_dir() {
        return err(400, "is-dir", "目标是文件夹，不是文件", false);
    }
    if meta.len() > MAX_READ {
        return err(413, "too-large", &format!("文件超过单次读上限（{MAX_READ} 字节）"), false);
    }
    let data = match std::fs::read(&target) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return err(409, "busy", &format!("目标被占用或只读，系统拒绝访问：{e}"), false);
        }
        Err(e) => return err(500, "internal", &format!("Error: {e}"), false),
    };
    let text = match decode_utf8_sig(&data) {
        Ok(text) => text,
        Err(()) => return err(415, "not-utf8", "文件不是 UTF-8 文本（二进制/其他编码暂不支持）", false),
    };
    let body = ReadJson {
        ok: true,
        path: parts.join("/"),
        text,
        bytes: data.len(),
        hash: util::sha256_hex(&data),
        mtime: mtime_of(&meta),
    };
    json(200, &body, false)
}

/// 整盘读取：只读数据夹外的一个文件。不写、不列举、不执行命令；机密目录仍拒绝。
fn disk_read(app: &App, query: &str) -> Reply {
    let raw = query_first(query, "path");
    let target = match gate::disk_read_target(app, &raw) {
        Ok(target) => target,
        Err(e) => return from_bad(e),
    };
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        // Python 的 `Path.exists()` 对任何 OSError 都当「不存在」
        Err(_) => return err(404, "not-found", "文件不存在", false),
    };
    if meta.is_dir() {
        return err(400, "is-dir", "目标是文件夹，不是文件", false);
    }
    if meta.len() > MAX_READ {
        return err(413, "too-large", &format!("文件超过单次读上限（{MAX_READ} 字节）"), false);
    }
    let data = match std::fs::read(&target) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return err(409, "busy", &format!("目标被占用或只读，系统拒绝访问：{e}"), false);
        }
        Err(e) => return err(500, "internal", &format!("OSError: {e}"), false),
    };
    let text = match decode_utf8_sig(&data) {
        Ok(text) => text,
        Err(()) => return err(415, "not-utf8", "文件不是 UTF-8 文本（二进制/其他编码暂不支持）", false),
    };
    let body = ReadJson {
        ok: true,
        path: raw.replace('\\', "/"),
        text,
        bytes: data.len(),
        hash: util::sha256_hex(&data),
        mtime: mtime_of(&meta),
    };
    json(200, &body, false)
}

fn secret_read(app: &App, query: &str) -> Reply {
    let Some((root, real)) = app.data_snapshot() else {
        return from_bad(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let name = query_first(query, "name");
    let target = match secret::path(&root, &real, &name) {
        Ok(target) => target,
        Err(e) => return from_bad(e),
    };
    let plain = match secret::plain(&target) {
        Ok(Some(plain)) => plain,
        Ok(None) => return err(404, "not-found", "机密文件不存在（尚未写过）", false),
        Err(e) => return from_bad(e),
    };
    let text = match String::from_utf8(plain.clone()) {
        Ok(text) => text,
        Err(_) => {
            return err(409, "secret-unreadable", "机密内容不是 UTF-8 文本（文件可能被外部改过）", false);
        }
    };
    let mtime = std::fs::metadata(&target).map(|m| mtime_of(&m)).unwrap_or(0.0);
    let body = SecretReadJson {
        ok: true,
        name: &name,
        path: secret_rel(&name),
        text,
        bytes: plain.len(),
        hash: util::sha256_hex(&plain),
        mtime,
    };
    json(200, &body, false)
}

fn secret_write(app: &App, raw: &[u8]) -> Reply {
    if app.data_snapshot().is_none() {
        return from_bad(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    }
    let (body, had_lone_surrogate) = match parse_body(raw) {
        Ok(parsed) => parsed,
        Err(e) => return from_bad(e),
    };
    let Some(text) = body.get("text").and_then(Value::as_str) else {
        return from_bad(gate::bad(400, "bad-body", "text 必须是字符串"));
    };
    if had_lone_surrogate && text.contains('\u{FFFD}') {
        return from_bad(gate::bad(
            400,
            "bad-body",
            "text 含非法字符（无法编码为 UTF-8）：surrogates not allowed",
        ));
    }
    let plain = text.as_bytes();
    if plain.len() > MAX_SECRET {
        return from_bad(gate::bad(413, "too-large", format!("机密明文超过上限（{MAX_SECRET} 字节）")));
    }
    let base = match body.get("baseHash") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return from_bad(gate::bad(400, "bad-body", "baseHash 必须是字符串")),
    };
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    let (root, real) = app.data_snapshot().expect("已确认数据夹存在");
    let target = match secret::path(&root, &real, name) {
        Ok(target) => target,
        Err(e) => return from_bad(e),
    };

    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    if target.is_dir() {
        return from_bad(gate::bad(409, "conflict", "目标是文件夹，不能当文件写"));
    }
    let created = !target.exists();
    let cur = if created {
        None
    } else {
        match secret::plain(&target) {
            Ok(plain) => plain,
            // 旧密文解不开：任何 baseHash 都对不上（客户端走强制覆盖）。
            Err(e) if e.error == "secret-unreadable" => None,
            Err(e) => return from_bad(e),
        }
    };
    if let Some(base) = &base {
        let cur_hash = cur.as_deref().map(util::sha256_hex).unwrap_or_default();
        if base.is_empty() {
            if !created {
                let e = gate::bad(409, "conflict", "目标已存在（baseHash 为空表示只新建）");
                return from_bad(gate::bad_extra(e, "actualHash", cur_hash));
            }
        } else if &cur_hash != base {
            let e = gate::bad(409, "conflict", "机密文件已被其他写入改动（baseHash 不匹配）");
            return from_bad(gate::bad_extra(e, "actualHash", cur_hash));
        }
    }
    let blob = match secret::seal(plain) {
        Ok(blob) => blob,
        Err(_) => return err(500, "internal", "OSError: DPAPI 调用失败", false),
    };
    let written = if !created && base.is_none() {
        fsops::swap_write(&target, &blob, &fsops::trash_target(&root, &target))
    } else {
        fsops::atomic_write(&target, &blob)
    };
    if let Err(e) = written {
        return from_bad(gate::io_bad(e));
    }
    let body = SecretWriteJson {
        ok: true,
        name,
        path: secret_rel(name),
        bytes: plain.len(),
        hash: util::sha256_hex(plain),
        created,
    };
    json(200, &body, false)
}

fn secret_rel(name: &str) -> String {
    format!("{}/{}/{}{}", gate::WS_NAME, gate::SECRET_NAME, name, gate::SECRET_EXT)
}

/// 请求体一律是 JSON 对象：解析失败 → 400 bad-json。孤立代理对按 Python 口径留到端点里判
/// （Python 的 json 接受代理对，报错发生在 text.encode('utf-8')）。
pub(crate) fn parse_body(raw: &[u8]) -> Result<(Value, bool), Bad> {
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text,
        Err(_) => return Err(bad_body_json("请求体不是合法 JSON")),
    };
    match serde_json::from_str::<Value>(text) {
        Ok(value) => expect_object(value, false),
        Err(_) => {
            let (fixed, had) = repair_lone_surrogates(text);
            if !had {
                return Err(bad_body_json("请求体不是合法 JSON"));
            }
            match serde_json::from_str::<Value>(&fixed) {
                Ok(value) => expect_object(value, true),
                Err(_) => Err(bad_body_json("请求体不是合法 JSON")),
            }
        }
    }
}

fn expect_object(value: Value, had_lone_surrogate: bool) -> Result<(Value, bool), Bad> {
    if value.is_object() {
        Ok((value, had_lone_surrogate))
    } else {
        Err(bad_body_json("请求体必须是 JSON 对象"))
    }
}

fn bad_body_json(message: &str) -> Bad {
    gate::bad(400, "bad-json", message)
}

/// 把不成对的 `\uD800`~`\uDFFF` 转义换成 `\uFFFD`，让 serde_json 能读进来。
/// 返回 (替换后的文本, 是否替换过)。只在首次解析失败后调用。
fn repair_lone_surrogates(s: &str) -> (String, bool) {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut had = false;
    while i < b.len() {
        if b[i] == b'\\' {
            if i + 1 < b.len() && b[i + 1] == b'u' && i + 6 <= b.len() {
                if let Some(v) = hex4(&b[i + 2..i + 6]) {
                    if (0xD800..=0xDBFF).contains(&v) {
                        if i + 12 <= b.len() && b[i + 6] == b'\\' && b[i + 7] == b'u' {
                            if let Some(v2) = hex4(&b[i + 8..i + 12]) {
                                if (0xDC00..=0xDFFF).contains(&v2) {
                                    out.push_str(&s[i..i + 12]);
                                    i += 12;
                                    continue;
                                }
                            }
                        }
                        out.push_str("\\uFFFD");
                        had = true;
                        i += 6;
                        continue;
                    }
                    if (0xDC00..=0xDFFF).contains(&v) {
                        out.push_str("\\uFFFD");
                        had = true;
                        i += 6;
                        continue;
                    }
                }
                out.push_str(&s[i..i + 2]);
                i += 2;
                continue;
            }
            if i + 1 < b.len() && b[i + 1].is_ascii() {
                out.push_str(&s[i..i + 2]);
                i += 2;
            } else {
                out.push('\\');
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().expect("字符边界");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, had)
}

fn hex4(b: &[u8]) -> Option<u32> {
    if b.len() != 4 || !b.iter().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(b).ok()?, 16).ok()
}

fn guide(name: &str) -> Reply {
    let name = name.split('?').next().unwrap_or("");
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || !guide_name_ok(name)
    {
        return text(404, "not found");
    }
    match guide_bytes(name) {
        Some((data, ctype)) => Reply { status: 200, body: data, content_type: ctype, close: false },
        None => text(404, "not found"),
    }
}

/// frozen 形态：入门图取编译期内嵌表；名字先过 guide_name_ok，表外即 404。
#[cfg(feature = "frozen")]
fn guide_bytes(name: &str) -> Option<(Vec<u8>, &'static str)> {
    GUIDE_ASSETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| (bytes.to_vec(), guide_ctype(name)))
}

#[cfg(not(feature = "frozen"))]
fn guide_bytes(name: &str) -> Option<(Vec<u8>, &'static str)> {
    let root = dev_app_dir().join("assets").join("guide");
    let target = root.join(name);
    let (Ok(root_c), Ok(target_c)) = (std::fs::canonicalize(&root), std::fs::canonicalize(&target)) else {
        return None;
    };
    if !target_c.starts_with(&root_c) || !target_c.is_file() {
        return None;
    }
    std::fs::read(&target_c).ok().map(|data| (data, guide_ctype(name)))
}

fn guide_ctype(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "webp" => "image/webp",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn guide_name_ok(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("guide-") else {
        return false;
    };
    let Some((base, ext)) = rest.rsplit_once('.') else {
        return false;
    };
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return false;
    }
    matches!(ext, "webp" | "png" | "jpg" | "jpeg")
}

/// 内置技能只读分发（ADR 0020）：`manifest.json`＝技能**元数据**数组（L2：dir/name/description/
/// cats/triggers/tokens，L3 附 refs 参考文件名清单——列表/过滤/触发判定只读元数据、不拉正文；
/// tokens 与前端 estimateTokens 同权重，由 skillmeta 预估算）；`<名>/SKILL.md`＝正文（勾选装载
/// 那一刻才拉）；`<名>/references/<文件>`＝参考文件（L3 助理 skill-load 的 ref 按需读取，
/// 仅一层单文件、扩展名 md/txt 白名单）。
/// 与 guide 不同处：dev 通道（`--app` 指向工作树页面）优先读同一工作树的 skills 目录，
/// 技能文件改完刷新即生效；frozen 无 override 才落编译期内嵌表。
fn skills(app: &App, rest: &str) -> Reply {
    let rest = rest.split('?').next().unwrap_or("");
    if rest == "manifest.json" {
        return Reply {
            status: 200,
            body: skill_manifest(app).into_bytes(),
            content_type: "application/json; charset=utf-8",
            close: false,
        };
    }
    let Some((name, file)) = rest.split_once('/') else {
        return text(404, "not found");
    };
    if !skill_dir_ok(name) {
        return text(404, "not found");
    }
    if file == "SKILL.md" {
        return match skill_file(app, name) {
            Some(bytes) => Reply {
                status: 200,
                body: bytes,
                content_type: "text/markdown; charset=utf-8",
                close: false,
            },
            None => text(404, "not found"),
        };
    }
    /* L3 参考文件：仅 `references/<单文件>` 一层（深层子路径 split_once 后仍含 '/'，直接拒绝）；
    文件段经 pct_decode（中文名 URL 编码到达）后过 ref_file_ok 白名单。 */
    let Some(ref_name) = file.strip_prefix("references/") else {
        return text(404, "not found");
    };
    let ref_name = pct_decode(ref_name);
    if ref_name.contains('/') || !crate::skillmeta::ref_file_ok(&ref_name) {
        return text(404, "not found");
    }
    match skill_ref_file(app, name, &ref_name) {
        Some(bytes) => {
            let ct = if ref_name.to_lowercase().ends_with(".md") {
                "text/markdown; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            };
            Reply { status: 200, body: bytes, content_type: ct, close: false }
        }
        None => text(404, "not found"),
    }
}

/// manifest 元数据数组 JSON：frozen 用编译期内嵌串（build.rs 预解析）；dev 通道/非 frozen
/// 运行时现读工作树逐技能解析（skillmeta 同一实现，双侧零漂移）。
fn skill_manifest(app: &App) -> String {
    if let Some(dir) = dev_skill_dir(app) {
        return skill_manifest_rt(&dir);
    }
    #[cfg(feature = "frozen")]
    return SKILL_MANIFEST.to_string();
    #[cfg(not(feature = "frozen"))]
    return skill_manifest_rt(&dev_app_dir().join("skills"));
}

/// 运行时现读现解析目录下全部技能的元数据数组（dev 通道：技能内容迭代频繁，刷新即生效）。
/// L3：每技能附 references/ 合法参考文件名清单（skillmeta::ref_file_ok 白名单，排序）。
fn skill_manifest_rt(dir: &Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    for name in list_skill_dir(dir) {
        if let Some(bytes) = read_under(&dir.join(&name).join("SKILL.md"), dir) {
            let text = String::from_utf8_lossy(&bytes);
            let refs = list_ref_files(&dir.join(&name).join("references"));
            entries.push(crate::skillmeta::meta_json(&name, &text, &refs));
        }
    }
    format!("[{}]", entries.join(","))
}

/// 列 references/ 子目录合法参考文件名（与 build.rs list_ref_files 同口径）。
fn list_ref_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if crate::skillmeta::ref_file_ok(&name) && entry.path().is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

fn skill_file(app: &App, name: &str) -> Option<Vec<u8>> {
    if let Some(dir) = dev_skill_dir(app) {
        return read_under(&dir.join(name).join("SKILL.md"), &dir);
    }
    #[cfg(feature = "frozen")]
    return SKILL_FILES.iter().find(|(n, _)| *n == name).map(|(_, b)| b.to_vec());
    #[cfg(not(feature = "frozen"))]
    return read_under(
        &dev_app_dir().join("skills").join(name).join("SKILL.md"),
        &dev_app_dir().join("skills"),
    );
}

/// 参考文件字节（L3）：解析顺序同 skill_file——dev 通道工作树／frozen 内嵌 SKILL_REFS／
/// 非 frozen dev_app_dir。调用方已过 ref_file_ok 白名单（仅一层单文件）。
fn skill_ref_file(app: &App, name: &str, ref_name: &str) -> Option<Vec<u8>> {
    if let Some(dir) = dev_skill_dir(app) {
        return read_under(&dir.join(name).join("references").join(ref_name), &dir);
    }
    #[cfg(feature = "frozen")]
    {
        let rel = format!("references/{ref_name}");
        return SKILL_REFS
            .iter()
            .find(|(n, r, _)| *n == name && *r == rel)
            .map(|(_, _, b)| b.to_vec());
    }
    #[cfg(not(feature = "frozen"))]
    return read_under(
        &dev_app_dir().join("skills").join(name).join("references").join(ref_name),
        &dev_app_dir().join("skills"),
    );
}

/// dev 通道（`--app` 指向工作树页面）时，内置技能与页面同源：从该工作树读。
fn dev_skill_dir(app: &App) -> Option<PathBuf> {
    app.app_override.as_ref().and_then(|p| p.parent()).map(|d| d.join("skills"))
}

fn list_skill_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if skill_dir_ok(&name) && entry.path().join("SKILL.md").is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

fn read_under(target: &Path, root: &Path) -> Option<Vec<u8>> {
    let (Ok(root_c), Ok(target_c)) = (std::fs::canonicalize(root), std::fs::canonicalize(target)) else {
        return None;
    };
    if !target_c.starts_with(&root_c) || !target_c.is_file() {
        return None;
    }
    std::fs::read(&target_c).ok()
}

/// 技能目录名白名单：小写字母数字＋`-`/`_`（与 build.rs embed_skills 同一口径）。
fn skill_dir_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn app_page_bytes(app: &App) -> Result<Vec<u8>, Bad> {
    if let Some(path) = &app.app_override {
        return std::fs::read(path).map_err(|e| gate::bad(500, "internal", format!("OSError: {e}")));
    }
    #[cfg(feature = "frozen")]
    return Ok(APP_PAGE_EMBED.to_vec());
    #[cfg(not(feature = "frozen"))]
    return std::fs::read(dev_app_dir().join("index.html"))
        .map_err(|e| gate::bad(500, "internal", format!("OSError: {e}")));
}

#[cfg(not(feature = "frozen"))]
fn dev_app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn inject(html: &[u8], app: &App) -> Vec<u8> {
    let mut s = match String::from_utf8(html.to_vec()) {
        Ok(s) => s,
        Err(_) => return html.to_vec(),
    };
    let tok = serde_json::to_string(&app.token).unwrap_or_else(|_| "\"\"".to_string());
    let ver = serde_json::to_string(VER).unwrap_or_else(|_| "\"\"".to_string());
    let mut snippet = format!("<script>window.__SF_TOKEN__={tok};window.__SF_PORT__={};window.__SF_VER__={ver}", app.port);
    if app.shell {
        snippet.push_str(";window.__SF_SHELL__=true");
    }
    snippet.push_str(";</script>");
    if let Some(i) = s.find("</head>") {
        s.insert_str(i, &snippet);
    } else {
        s.insert_str(0, &snippet);
    }
    s.into_bytes()
}

fn query_first(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if pct_decode(k) == key {
            return pct_decode(v);
        }
    }
    String::new()
}

fn pct_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) {
            out.push((hex_val(bytes[i + 1]) << 4) | hex_val(bytes[i + 2]));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn decode_utf8_sig(data: &[u8]) -> Result<String, ()> {
    let body = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    String::from_utf8(body.to_vec()).map_err(|_| ())
}

fn mtime_of(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn path_text(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn text(status: u16, body: &str) -> Reply {
    Reply {
        status,
        body: body.as_bytes().to_vec(),
        content_type: "text/plain; charset=utf-8",
        close: false,
    }
}

fn html(body: &[u8]) -> Reply {
    Reply {
        status: 200,
        body: body.to_vec(),
        content_type: "text/html; charset=utf-8",
        close: false,
    }
}

pub(crate) fn err(status: u16, error: &str, message: &str, close: bool) -> Reply {
    let body = ErrJson { ok: false, error, message };
    json(status, &body, close)
}

pub(crate) fn from_bad(e: Bad) -> Reply {
    if e.extra.is_empty() {
        return err(e.status, e.error, &e.message, false);
    }
    let mut obj = Map::new();
    obj.insert("ok".into(), Value::Bool(false));
    obj.insert("error".into(), Value::String(e.error.into()));
    obj.insert("message".into(), Value::String(e.message));
    for (key, value) in e.extra {
        obj.insert(key.into(), value);
    }
    json(e.status, &Value::Object(obj), false)
}

fn json<T: Serialize>(status: u16, body: &T, close: bool) -> Reply {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| br#"{"ok":false,"error":"internal","message":"JSON"}"#.to_vec());
    Reply {
        status,
        body: bytes,
        content_type: "application/json; charset=utf-8",
        close,
    }
}

pub(crate) fn finish(reply: Reply, head: bool) -> Response<Body> {
    let len = reply.body.len();
    let body = if head { Body::empty() } else { Body::from(reply.body) };
    let mut builder = Response::builder()
        .status(reply.status)
        .header("Content-Type", reply.content_type)
        .header("Content-Length", len.to_string())
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Server", format!("SFServer/{VER} "));
    if reply.close {
        builder = builder.header("Connection", "close");
    }
    builder.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
}

#[derive(Serialize)]
struct ErrJson<'a> {
    ok: bool,
    error: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
struct StatusJson<'a> {
    ok: bool,
    app: &'a str,
    ver: &'a str,
    port: u16,
    #[serde(rename = "dataRoot")]
    data_root: String,
    #[serde(rename = "dataOk")]
    data_ok: bool,
    configured: bool,
    #[serde(rename = "defaultDataRoot")]
    default_data_root: String,
    frozen: bool,
    #[serde(rename = "appHash")]
    app_hash: String,
    #[serde(rename = "appBytes")]
    app_bytes: usize,
    lock: LockJson,
}

#[derive(Serialize)]
struct LockJson {
    pid: Option<u32>,
    port: Option<u16>,
    at: Option<String>,
}

#[derive(Serialize)]
struct ReadJson {
    ok: bool,
    path: String,
    text: String,
    bytes: usize,
    hash: String,
    mtime: f64,
}

#[derive(Serialize)]
struct SecretReadJson<'a> {
    ok: bool,
    name: &'a str,
    path: String,
    text: String,
    bytes: usize,
    hash: String,
    mtime: f64,
}

#[derive(Serialize)]
struct SecretWriteJson<'a> {
    ok: bool,
    name: &'a str,
    path: String,
    bytes: usize,
    hash: String,
    created: bool,
}
