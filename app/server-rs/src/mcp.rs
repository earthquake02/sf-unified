//! 本地 MCP stdio 单会话。口径对照 `app/server/sf_mcp.py` 与 `sf_server.py` 的五个 handler。
//! 进程级单例，不放进 `App`（`gate.rs` 的测试自己构造 App）。
//!
//! stdio：`Command` 直接 CreateProcess，不经 cmd.exe；三路管道；Windows `CREATE_NO_WINDOW`。
//! 写入是 `ChildStdin::write_all` + `flush`，不用 `BufWriter`。读线程 4096、stderr 512。
//! `Child::kill` 即 TerminateProcess，最多等 2 秒再 kill。stdout/stderr 在进程被终止、
//! 读调用返回后由读线程 drop 关闭，避免另一线程关正在 ReadFile 的管道句柄。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::gate::{self, Bad};
use crate::state::VER;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "storyforge";
const DEFAULT_TIMEOUT: f64 = 60.0;
const MAX_FRAME: i64 = 8 * 1024 * 1024;
const STDERR_KEEP: usize = 4000;
const STDERR_SHOW: usize = 2000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
struct McpError {
    code: &'static str,
    message: String,
}

impl McpError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

struct WaitSlot {
    done: bool,
    outcome: Option<Result<Value, McpError>>,
}

struct Waiter {
    mu: Mutex<WaitSlot>,
    cv: Condvar,
}

impl Waiter {
    fn new() -> Self {
        Self {
            mu: Mutex::new(WaitSlot { done: false, outcome: None }),
            cv: Condvar::new(),
        }
    }

    fn finish(&self, outcome: Result<Value, McpError>) {
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if g.done {
            return;
        }
        g.done = true;
        g.outcome = Some(outcome);
        self.cv.notify_one();
    }

    fn succeed(&self, value: Value) {
        self.finish(Ok(value));
    }

    fn fail(&self, err: McpError) {
        self.finish(Err(err));
    }

    fn wait(&self, dur: Duration) {
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        while !g.done {
            let left = dur.saturating_sub(start.elapsed());
            if left.is_zero() {
                return;
            }
            let (next, timed) = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner());
            g = next;
            if timed.timed_out() {
                return;
            }
        }
    }

    fn take_ready(&self) -> Option<Result<Value, McpError>> {
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if g.done {
            g.outcome.take()
        } else {
            None
        }
    }
}

struct Inner {
    alive: bool,
    connected: bool,
    next_id: i64,
    pending: HashMap<i64, Arc<Waiter>>,
    tools: Vec<Value>,
    meta: Map<String, Value>,
    stderr_tail: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            alive: false,
            connected: false,
            next_id: 1,
            pending: HashMap::new(),
            tools: Vec::new(),
            meta: Map::new(),
            stderr_tail: String::new(),
            child: None,
            stdin: None,
            reader: None,
            stderr_thread: None,
        }
    }
}

struct Hub {
    /// 串行 connect/list/call/disconnect。等待应答时只放开 `inner`，读线程才能投递。
    op: Mutex<()>,
    inner: Arc<Mutex<Inner>>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub {
        op: Mutex::new(()),
        inner: Arc::new(Mutex::new(Inner::new())),
    })
}

fn op_lock() -> MutexGuard<'static, ()> {
    hub().op.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn to_bad(err: McpError) -> Bad {
    let code = match err.code {
        "bad-command" | "spawn-failed" | "bad-name" | "bad-arguments" | "not-connected"
        | "timeout" | "rpc-error" | "read-failed" | "write-failed" | "disconnected" | "bad-frame" => {
            err.code
        }
        _ => "mcp-error",
    };
    gate::bad(400, code, err.message)
}

async fn spawn_ok<T, F>(work: F) -> Result<T, Bad>
where
    F: FnOnce() -> Result<T, Bad> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(_) => Err(gate::bad(500, "internal", "MCP 处理失败")),
    }
}

pub async fn status() -> Result<Value, Bad> {
    spawn_ok(|| Ok(status_blocking())).await
}

pub async fn connect(raw: Vec<u8>) -> Result<Value, Bad> {
    spawn_ok(move || connect_blocking(&raw)).await
}

pub async fn disconnect() -> Result<Value, Bad> {
    spawn_ok(|| Ok(disconnect_blocking())).await
}

pub async fn list_tools() -> Result<Value, Bad> {
    spawn_ok(list_blocking).await
}

pub async fn call(raw: Vec<u8>) -> Result<Value, Bad> {
    spawn_ok(move || call_blocking(&raw)).await
}

fn status_blocking() -> Value {
    let mut g = lock(&hub().inner);
    status_from(&mut g)
}

fn disconnect_blocking() -> Value {
    let _op = op_lock();
    let inner = hub().inner.clone();
    disconnect_full(&inner);
    let mut g = lock(&inner);
    status_from(&mut g)
}

fn list_blocking() -> Result<Value, Bad> {
    let _op = op_lock();
    let inner = hub().inner.clone();
    {
        let mut g = lock(&inner);
        ensure_alive(&mut g).map_err(to_bad)?;
    }
    // list 不读 body，超时固定 60（sf_server.py 1877–1880）。
    let listed = request(&inner, "tools/list", json!({}), DEFAULT_TIMEOUT).map_err(to_bad)?;
    let tools = normalize_listed(&listed);
    let mut g = lock(&inner);
    g.tools = tools;
    Ok(status_from(&mut g))
}

fn call_blocking(raw: &[u8]) -> Result<Value, Bad> {
    let body = parse_object(raw)?;
    let name = field_str(&body, "name");
    let arguments = match body.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err(gate::bad(400, "bad-arguments", "arguments 必须是对象")),
    };
    let timeout = timeout_of(&body);
    // handler 先拒 arguments，再进 call_tool 拒空名字（sf_server.py 1888–1896，sf_mcp.py 121–129）。
    if name.is_empty() {
        return Err(gate::bad(400, "bad-name", "缺少工具名"));
    }
    let _op = op_lock();
    let inner = hub().inner.clone();
    {
        let mut g = lock(&inner);
        ensure_alive(&mut g).map_err(to_bad)?;
    }
    let result = request(
        &inner,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
        timeout,
    )
    .map_err(to_bad)?;
    Ok(json!({"ok": true, "name": name, "result": result}))
}

fn connect_blocking(raw: &[u8]) -> Result<Value, Bad> {
    let body = parse_object(raw)?;
    let command = field_str(&body, "command");
    if command.is_empty() {
        return Err(gate::bad(400, "bad-command", "请填写启动命令"));
    }
    let args = args_of(&body);
    let env = env_of(&body);
    let timeout = timeout_of(&body);
    let _op = op_lock();
    let inner = hub().inner.clone();
    disconnect_full(&inner);
    let mut child = match spawn_child(&command, &args, env.as_ref()) {
        Ok(child) => child,
        Err(err) => return Err(to_bad(err)),
    };
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(gate::bad(500, "internal", "MCP 管道未建立"));
    };
    {
        let mut g = lock(&inner);
        g.alive = true;
        g.connected = false;
        g.stderr_tail.clear();
        g.pending.clear();
        g.next_id = 1;
        g.tools.clear();
        let mut meta = Map::new();
        meta.insert("command".into(), Value::String(command));
        meta.insert("args".into(), Value::Array(args.into_iter().map(Value::String).collect()));
        g.meta = meta;
        g.stdin = Some(stdin);
        g.child = Some(child);
        let reader_inner = inner.clone();
        g.reader = Some(thread::spawn(move || read_loop(stdout, reader_inner)));
        let err_inner = inner.clone();
        g.stderr_thread = Some(thread::spawn(move || drain_stderr(stderr, err_inner)));
    }
    let handshake = handshake(&inner, timeout);
    match handshake {
        Ok(st) => Ok(st),
        Err(err) => {
            disconnect_full(&inner);
            Err(to_bad(err))
        }
    }
}

fn handshake(inner: &Arc<Mutex<Inner>>, timeout: f64) -> Result<Value, McpError> {
    let init = request(inner, "initialize", init_params(), timeout)?;
    notify(inner, "notifications/initialized", json!({}))?;
    let listed = request(inner, "tools/list", json!({}), timeout)?;
    let tools = normalize_listed(&listed);
    let mut g = lock(inner);
    g.tools = tools;
    g.connected = true;
    g.meta.insert("serverInfo".into(), meta_field(&init, "serverInfo"));
    g.meta.insert("protocolVersion".into(), meta_field(&init, "protocolVersion"));
    Ok(status_from(&mut g))
}

fn init_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": CLIENT_NAME, "version": VER},
    })
}

fn meta_field(init: &Value, key: &str) -> Value {
    match init {
        Value::Object(map) => map.get(key).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn ensure_alive(g: &mut Inner) -> Result<(), McpError> {
    if g.child.is_none() || !child_running(&mut g.child) || !g.connected {
        Err(McpError::new("not-connected", "MCP 未连接或进程已退出"))
    } else {
        Ok(())
    }
}

fn child_running(child: &mut Option<Child>) -> bool {
    let Some(child) = child.as_mut() else {
        return false;
    };
    matches!(child.try_wait(), Ok(None))
}

fn status_from(g: &mut Inner) -> Value {
    let connected = g.connected && child_running(&mut g.child);
    json!({
        "connected": connected,
        "tools": g.tools.clone(),
        "meta": Value::Object(g.meta.clone()),
        "stderr": tail_chars(&g.stderr_tail, STDERR_SHOW),
    })
}

fn disconnect_full(inner: &Arc<Mutex<Inner>>) {
    let (child, stdin, reader, stderr_thread, pending) = {
        let mut g = lock(inner);
        g.alive = false;
        g.connected = false;
        g.tools.clear();
        let pending = std::mem::take(&mut g.pending);
        (
            g.child.take(),
            g.stdin.take(),
            g.reader.take(),
            g.stderr_thread.take(),
            pending,
        )
    };
    for waiter in pending.into_values() {
        waiter.fail(McpError::new("disconnected", "MCP 已断开"));
    }
    if let Some(mut child) = child {
        reap(&mut child);
    }
    drop(stdin);
    if let Some(handle) = reader {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_thread {
        let _ = handle.join();
    }
}

fn reap(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let start = Instant::now();
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if start.elapsed() >= Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_child(command: &str, args: &[String], env: Option<&Map<String, Value>>) -> Result<Child, McpError> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = env {
        for (key, value) in env {
            if key.is_empty() || value.is_null() {
                continue;
            }
            cmd.env(key, py_str(value));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(2) => {
            Err(McpError::new("spawn-failed", format!("找不到命令：{command}")))
        }
        Err(err) => Err(McpError::new("spawn-failed", format!("无法启动：{err}"))),
    }
}

fn read_loop(mut stdout: ChildStdout, inner: Arc<Mutex<Inner>>) {
    let mut acc = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        {
            let mut g = lock(&inner);
            if !g.alive || !child_running(&mut g.child) {
                break;
            }
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(err) => {
                fail_pending(&inner, McpError::new("read-failed", format!("读取 MCP 输出失败：{err}")));
                break;
            }
        }
        loop {
            match try_pop_frame(&acc) {
                Ok(Pop::Need(kept)) => {
                    acc = kept;
                    break;
                }
                Ok(Pop::Again(rest)) => {
                    if rest.len() >= acc.len() {
                        acc = rest;
                        break;
                    }
                    acc = rest;
                }
                Ok(Pop::Frame(msg, rest)) => {
                    acc = rest;
                    handle_message(&inner, msg);
                }
                Err(err) => {
                    fail_pending(
                        &inner,
                        McpError::new("read-failed", format!("读取 MCP 输出失败：{err}")),
                    );
                    reader_finally(&inner);
                    return;
                }
            }
        }
    }
    reader_finally(&inner);
}

fn drain_stderr(mut stderr: ChildStderr, inner: Arc<Mutex<Inner>>) {
    let mut buf = [0u8; 512];
    loop {
        {
            let mut g = lock(&inner);
            if !g.alive || !child_running(&mut g.child) {
                break;
            }
        }
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                let mut g = lock(&inner);
                push_limited(&mut g.stderr_tail, &text, STDERR_KEEP);
            }
            Err(_) => break,
        }
    }
}

fn fail_pending(inner: &Mutex<Inner>, err: McpError) {
    let pending = {
        let mut g = lock(inner);
        std::mem::take(&mut g.pending)
    };
    for waiter in pending.into_values() {
        waiter.fail(err.clone());
    }
}

fn reader_finally(inner: &Mutex<Inner>) {
    let pending = {
        let mut g = lock(inner);
        if g.connected {
            g.connected = false;
        }
        std::mem::take(&mut g.pending)
    };
    for waiter in pending.into_values() {
        waiter.fail(McpError::new("disconnected", "MCP 进程已结束"));
    }
}

fn handle_message(inner: &Mutex<Inner>, msg: Value) {
    let Value::Object(map) = msg else {
        return;
    };
    if !map.contains_key("id") || (!map.contains_key("result") && !map.contains_key("error")) {
        return;
    }
    let Some(id) = id_of(map.get("id")) else {
        return;
    };
    let mut g = lock(inner);
    let Some(waiter) = g.pending.remove(&id) else {
        return;
    };
    if map.get("error").is_some_and(|err| !err.is_null()) {
        let err = map.get("error").cloned().unwrap_or(Value::Null);
        waiter.fail(McpError::new("rpc-error", rpc_message(&err)));
    } else {
        waiter.succeed(map.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn id_of(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(id) = value.as_i64() {
        return Some(id);
    }
    value.as_u64().and_then(|id| i64::try_from(id).ok())
}

fn rpc_message(err: &Value) -> String {
    if let Value::Object(map) = err {
        if let Some(message) = map.get("message") {
            if py_truthy(message) {
                return py_str(message);
            }
        }
        return py_repr(err);
    }
    py_str(err)
}

fn request(inner: &Arc<Mutex<Inner>>, method: &str, params: Value, timeout: f64) -> Result<Value, McpError> {
    let waiter = Arc::new(Waiter::new());
    let id = {
        let mut g = lock(inner);
        let id = g.next_id;
        g.next_id = g.next_id.saturating_add(1);
        g.pending.insert(id, waiter.clone());
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let Some(stdin) = g.stdin.as_mut() else {
            g.pending.remove(&id);
            return Err(McpError::new("not-connected", "MCP 未连接"));
        };
        if let Err(err) = write_frame(stdin, &msg) {
            g.pending.remove(&id);
            return Err(err);
        }
        id
    };
    waiter.wait(duration_from_secs(timeout));
    let mut g = lock(inner);
    if let Some(outcome) = waiter.take_ready() {
        g.pending.remove(&id);
        return outcome;
    }
    g.pending.remove(&id);
    Err(McpError::new("timeout", format!("MCP 调用超时（{method}）")))
}

fn notify(inner: &Arc<Mutex<Inner>>, method: &str, params: Value) -> Result<(), McpError> {
    let mut g = lock(inner);
    let Some(stdin) = g.stdin.as_mut() else {
        return Err(McpError::new("not-connected", "MCP 未连接"));
    };
    let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
    write_frame(stdin, &msg)
}

fn write_frame(stdin: &mut ChildStdin, msg: &Value) -> Result<(), McpError> {
    let frame = encode_frame(msg)?;
    stdin
        .write_all(&frame)
        .and_then(|_| stdin.flush())
        .map_err(|err| McpError::new("write-failed", format!("写入 MCP 失败：{err}")))
}

fn encode_frame(msg: &Value) -> Result<Vec<u8>, McpError> {
    let raw = serde_json::to_vec(msg)
        .map_err(|err| McpError::new("write-failed", format!("写入 MCP 失败：{err}")))?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", raw.len()).into_bytes();
    frame.extend_from_slice(&raw);
    Ok(frame)
}

fn duration_from_secs(secs: f64) -> Duration {
    if !secs.is_finite() || secs <= 0.0 {
        Duration::from_secs_f64(DEFAULT_TIMEOUT)
    } else {
        Duration::from_secs_f64(secs)
    }
}

#[derive(Debug)]
enum Pop {
    Need(Vec<u8>),
    Again(Vec<u8>),
    Frame(Value, Vec<u8>),
}

fn try_pop_frame(buf: &[u8]) -> Result<Pop, McpError> {
    let crlf = find_sub(buf, b"\r\n\r\n");
    let lf = find_sub(buf, b"\n\n");
    if crlf.is_none() && lf.is_none() {
        if buf.contains(&b'\n') && lstrip_ascii_ws(buf).starts_with(b"{") {
            if let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line = trim_ascii(&buf[..i]);
                let rest = buf[i + 1..].to_vec();
                if !line.is_empty() {
                    return match serde_json::from_slice::<Value>(line) {
                        Ok(msg) => Ok(Pop::Frame(msg, rest)),
                        Err(_) => Ok(Pop::Need(buf.to_vec())),
                    };
                }
            }
        }
        return Ok(Pop::Need(buf.to_vec()));
    }
    let (header, rest) = if let Some(i) = crlf {
        (&buf[..i], &buf[i + 4..])
    } else {
        let i = lf.unwrap();
        (&buf[..i], &buf[i + 2..])
    };
    let Some(length) = header_length(header) else {
        return Ok(Pop::Again(rest.to_vec()));
    };
    if length < 0 || length > MAX_FRAME {
        return Err(McpError::new("bad-frame", format!("帧过大或非法：{length}")));
    }
    let length = length as usize;
    if rest.len() < length {
        let mut kept = header.to_vec();
        kept.extend_from_slice(b"\r\n\r\n");
        kept.extend_from_slice(rest);
        return Ok(Pop::Need(kept));
    }
    let (body, rest2) = rest.split_at(length);
    match serde_json::from_slice::<Value>(body) {
        Ok(msg) => Ok(Pop::Frame(msg, rest2.to_vec())),
        Err(err) => Err(McpError::new("bad-frame", format!("JSON 无法解析：{err}"))),
    }
}

fn header_length(header: &[u8]) -> Option<i64> {
    let mut length = None;
    for line in replace_crlf(header).split(|b| *b == b'\n') {
        if starts_with_ignore_case(line, b"content-length:") {
            let value = trim_ascii(&line[b"content-length:".len()..]);
            length = parse_py_int(value);
        }
    }
    length
}

fn replace_crlf(header: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.len());
    let mut i = 0;
    while i < header.len() {
        if i + 1 < header.len() && header[i] == b'\r' && header[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(header[i]);
            i += 1;
        }
    }
    out
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|window| window == needle)
}

fn starts_with_ignore_case(line: &[u8], prefix: &[u8]) -> bool {
    line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn lstrip_ascii_ws(buf: &[u8]) -> &[u8] {
    let start = buf.iter().position(|b| !is_ascii_ws(*b)).unwrap_or(buf.len());
    &buf[start..]
}

fn trim_ascii(buf: &[u8]) -> &[u8] {
    let start = buf.iter().position(|b| !is_ascii_ws(*b)).unwrap_or(buf.len());
    let end = buf.iter().rposition(|b| !is_ascii_ws(*b)).map(|i| i + 1).unwrap_or(start);
    &buf[start..end]
}

fn parse_py_int(raw: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let cleaned = strip_digit_underscores(text)?;
    match cleaned.parse::<i128>() {
        Ok(value) if value > i64::MAX as i128 => Some(i64::MAX),
        Ok(value) if value < i64::MIN as i128 => Some(i64::MIN),
        Ok(value) => Some(value as i64),
        Err(_) => None,
    }
}

fn normalize_listed(listed: &Value) -> Vec<Value> {
    let Some(Value::Array(items)) = listed.get("tools") else {
        return Vec::new();
    };
    items.iter().filter(|item| item.is_object()).map(norm_tool).collect()
}

fn norm_tool(tool: &Value) -> Value {
    let name = match tool.get("name") {
        Some(value) if py_truthy(value) => py_str(value),
        _ => String::new(),
    };
    let description = match tool.get("description") {
        Some(value) if py_truthy(value) => py_str(value).chars().take(2000).collect(),
        _ => String::new(),
    };
    let schema = match tool.get("inputSchema") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({}),
    };
    json!({"name": name, "description": description, "inputSchema": schema})
}

fn parse_object(raw: &[u8]) -> Result<Map<String, Value>, Bad> {
    let text = std::str::from_utf8(raw).map_err(|_| gate::bad(400, "bad-json", "请求体不是合法 JSON"))?;
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(gate::bad(400, "bad-json", "请求体必须是 JSON 对象")),
        Err(_) => Err(gate::bad(400, "bad-json", "请求体不是合法 JSON")),
    }
}

fn field_str(body: &Map<String, Value>, key: &str) -> String {
    match body.get(key) {
        Some(value) if py_truthy(value) => py_str(value).trim().to_string(),
        _ => String::new(),
    }
}

fn args_of(body: &Map<String, Value>) -> Vec<String> {
    match body.get("args") {
        Some(Value::Array(items)) => items.iter().map(py_str).collect(),
        Some(Value::String(text)) if !text.trim().is_empty() => split_args(text),
        _ => Vec::new(),
    }
}

fn env_of(body: &Map<String, Value>) -> Option<Map<String, Value>> {
    match body.get("env") {
        Some(Value::Object(map)) => Some(map.clone()),
        _ => None,
    }
}

/// `sf_server.py` `_mcp_timeout`：缺省或解析失败为 60，再钳到 5..=180。
fn timeout_of(body: &Map<String, Value>) -> f64 {
    match body.get("timeout") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(value) => match as_py_float(value) {
            Some(number) => clamp_timeout(number),
            None => DEFAULT_TIMEOUT,
        },
    }
}

fn clamp_timeout(value: f64) -> f64 {
    // Python `max(5.0, min(value, 180.0))`：比较失败时留下前一个数，所以 NaN 落到 5。
    py_max(5.0, py_min(value, 180.0))
}

fn py_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

fn as_py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        Value::Number(number) => number.as_f64().or_else(|| number.as_i64().map(|i| i as f64)),
        Value::String(text) => py_float_parse(text),
        _ => None,
    }
}

fn py_float_parse(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Some(f64::NAN),
        _ => {}
    }
    let cleaned = strip_digit_underscores(trimmed)?;
    cleaned.parse::<f64>().ok()
}

fn strip_digit_underscores(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (i, ch) in chars.iter().copied().enumerate() {
        if ch == '_' {
            let prev = *chars.get(i.wrapping_sub(1))?;
            let next = *chars.get(i + 1)?;
            if !prev.is_ascii_digit() || !next.is_ascii_digit() {
                return None;
            }
            continue;
        }
        out.push(ch);
    }
    if out.is_empty() { None } else { Some(out) }
}

fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        if in_quote {
            if ch == '"' {
                in_quote = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_quote = true;
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0) || number.as_f64().is_none(),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => py_num(number),
        Value::String(text) => py_repr_str(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", py_repr_str(key), py_repr(item)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn py_repr_str(text: &str) -> String {
    let has_single = text.contains('\'');
    let has_double = text.contains('"');
    let (open, close) = if has_single && !has_double { ('"', '"') } else { ('\'', '\'') };
    let mut out = String::new();
    out.push(open);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' if open == '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => {
                let code = c as u32;
                out.push_str(&format!("\\x{code:02x}"));
            }
            c => out.push(c),
        }
    }
    out.push(close);
    out
}

fn py_num(number: &serde_json::Number) -> String {
    if number.is_i64() {
        return number.as_i64().unwrap_or(0).to_string();
    }
    if number.is_u64() {
        return number.as_u64().unwrap_or(0).to_string();
    }
    match number.as_f64() {
        Some(value) => py_float_text(value),
        None => number.to_string(),
    }
}

fn py_float_text(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() { "-inf".into() } else { "inf".into() };
    }
    let text = format!("{value}");
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

fn push_limited(buf: &mut String, add: &str, max_chars: usize) {
    buf.push_str(add);
    let count = buf.chars().count();
    if count > max_chars {
        *buf = buf.chars().skip(count - max_chars).collect();
    }
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        text.to_string()
    } else {
        text.chars().skip(count - max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_args_matches_python_quotes() {
        assert_eq!(split_args("a b"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_args("\"a b\" c"), vec!["a b".to_string(), "c".to_string()]);
        assert_eq!(split_args("a \"b c\" d"), vec!["a".to_string(), "b c".to_string(), "d".to_string()]);
        assert_eq!(split_args("\"a b\"c"), vec!["a bc".to_string()]);
        assert!(split_args("\"\"").is_empty());
        assert_eq!(split_args("  a\tb  "), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn timeout_clamp_matches_python() {
        let body = |timeout: Value| {
            let mut map = Map::new();
            map.insert("timeout".into(), timeout);
            timeout_of(&map)
        };
        assert_eq!(timeout_of(&Map::new()), 60.0);
        assert_eq!(body(Value::Null), 60.0);
        assert_eq!(body(json!(1)), 5.0);
        assert_eq!(body(json!(1.5)), 5.0);
        assert_eq!(body(json!(5.5)), 5.5);
        assert_eq!(body(json!(9999)), 180.0);
        assert_eq!(body(json!("no")), 60.0);
        assert_eq!(body(json!("  5 ")), 5.0);
        assert_eq!(body(json!("1e2")), 100.0);
        assert_eq!(body(json!("1_0")), 10.0);
        assert_eq!(body(Value::Bool(true)), 5.0);
        assert_eq!(body(Value::Bool(false)), 5.0);
        assert_eq!(body(json!("nan")), 5.0);
        assert_eq!(body(json!("inf")), 180.0);
    }

    #[test]
    fn frames_accept_length_prefix_and_ndjson() {
        let frame = encode_frame(&json!({"n": "中文"})).unwrap();
        let text = String::from_utf8(frame.clone()).unwrap();
        assert!(text.contains("中文"), "{text}");
        assert!(!text.contains("\\u"));
        assert!(text.starts_with("Content-Length: "));
        match try_pop_frame(&frame).unwrap() {
            Pop::Frame(msg, rest) => {
                assert_eq!(msg["n"], "中文");
                assert!(rest.is_empty());
            }
            other => panic!("expected frame, got need/again {}", matches!(other, Pop::Need(_))),
        }

        let note = encode_frame(&json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})).unwrap();
        let note_text = String::from_utf8(note).unwrap();
        assert!(note_text.contains("\"method\":\"notifications/initialized\""));
        assert!(!note_text.contains("\"id\""));

        let line = b"{\"a\":1}\n{\"b\":2}\n";
        match try_pop_frame(line).unwrap() {
            Pop::Frame(msg, rest) => {
                assert_eq!(msg["a"], 1);
                match try_pop_frame(&rest).unwrap() {
                    Pop::Frame(msg2, rest2) => {
                        assert_eq!(msg2["b"], 2);
                        assert!(rest2.is_empty());
                    }
                    _ => panic!("second ndjson"),
                }
            }
            _ => panic!("ndjson"),
        }

        let lf = b"Content-Length: 7\n\n{\"a\":1}";
        match try_pop_frame(lf).unwrap() {
            Pop::Frame(msg, _) => assert_eq!(msg["a"], 1),
            _ => panic!("lf frame"),
        }

        let dropped = b"Foo: 1\n\n{\"a\":1}\n";
        match try_pop_frame(dropped).unwrap() {
            Pop::Again(rest) => match try_pop_frame(&rest).unwrap() {
                Pop::Frame(msg, _) => assert_eq!(msg["a"], 1),
                _ => panic!("after drop"),
            },
            _ => panic!("drop header"),
        }

        let big = b"Content-Length: 99999999\r\n\r\n";
        let err = try_pop_frame(big).unwrap_err();
        assert_eq!(err.code, "bad-frame");
        assert!(err.message.starts_with("帧过大或非法："));

        let neg = b"Content-Length: -1\r\n\r\n";
        assert_eq!(try_pop_frame(neg).unwrap_err().code, "bad-frame");
    }

    #[test]
    fn norm_tool_matches_python_rules() {
        let tool = json!({
            "name": 12,
            "description": "长".repeat(2500),
            "inputSchema": "nope",
            "extra": 1
        });
        let norm = norm_tool(&tool);
        assert_eq!(norm["name"], "12");
        assert_eq!(norm["description"].as_str().unwrap().chars().count(), 2000);
        assert_eq!(norm["inputSchema"], json!({}));
        assert!(norm.get("extra").is_none());
        let empty = norm_tool(&json!({"name": "", "description": 0}));
        assert_eq!(empty["name"], "");
        assert_eq!(empty["description"], "");
    }

    #[test]
    fn session_stdio_roundtrip_kill_and_spawn_errors() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tools")
            .join("_tmp")
            .join("mcp09-cargo");
        let _rm = RmDir(dir.clone());
        let _guard = SessionGuard;
        let missing = connect_blocking(br#"{"command":"sf-mcp-no-such-command-09"}"#).unwrap_err();
        assert_eq!(missing.error, "spawn-failed");
        assert_eq!(missing.message, "找不到命令：sf-mcp-no-such-command-09");
        let empty = connect_blocking(br#"{"command":"   "}"#).unwrap_err();
        assert_eq!(empty.error, "bad-command");
        assert_eq!(empty.message, "请填写启动命令");
        let bad_args = call_blocking(br#"{"name":"echo","arguments":[]}"#).unwrap_err();
        assert_eq!(bad_args.error, "bad-arguments");
        assert_eq!(bad_args.message, "arguments 必须是对象");
        let bad_name = call_blocking(br#"{"name":"  ","arguments":{}}"#).unwrap_err();
        assert_eq!(bad_name.error, "bad-name");
        assert_eq!(bad_name.message, "缺少工具名");

        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake.py");
        std::fs::write(&script, FAKE_PY).unwrap();
        let log1 = dir.join("log1.txt");
        let log2 = dir.join("log2.txt");
        let log3 = dir.join("log3.txt");

        let first = connect_fake(&script, &log1, "frame").unwrap_or_else(|err| {
            panic!(
                "{err:?} log={} status={}",
                std::fs::read_to_string(&log1).unwrap_or_else(|e| format!("no-log {e}")),
                status_blocking()
            )
        });
        assert_eq!(first["connected"], true);
        let pid1 = log_pid(&log1);
        assert!(pid_alive(pid1), "first pid {pid1} should be alive");
        let params = log_init_params(&log1);
        assert_eq!(params["protocolVersion"], "2024-11-05");
        assert_eq!(params["clientInfo"]["name"], "storyforge");
        assert_eq!(params["clientInfo"]["version"], "1.2");
        assert_eq!(params["capabilities"], json!({}));
        assert_eq!(first["tools"][0]["name"], "echo");
        assert_eq!(first["tools"][1]["name"], "12");
        assert_eq!(first["tools"][1]["inputSchema"], json!({}));
        assert_eq!(first["meta"]["serverInfo"]["name"], "fake");

        let called = call_blocking(
            &serde_json::to_vec(&json!({"name": "echo", "arguments": {"text": "中文"}})).unwrap(),
        )
        .unwrap();
        assert_eq!(called["ok"], true);
        assert_eq!(called["result"]["echo"], "中文");

        let second = connect_fake(&script, &log2, "frame").unwrap();
        assert_eq!(second["connected"], true);
        assert!(!pid_alive(pid1), "first pid {pid1} still alive after reconnect");
        let pid2 = log_pid(&log2);
        assert!(pid_alive(pid2));

        let gone = disconnect_blocking();
        assert_eq!(gone["connected"], false);
        assert_eq!(gone["tools"].as_array().unwrap().len(), 0);
        assert!(!pid_alive(pid2), "pid {pid2} still alive after disconnect");

        let line = connect_fake(&script, &log3, "ndjson").unwrap();
        assert_eq!(line["connected"], true);
        let line_params = log_init_params(&log3);
        assert_eq!(line_params["clientInfo"]["version"], "1.2");
        let _ = disconnect_blocking();

        let not_conn = list_blocking().unwrap_err();
        assert_eq!(not_conn.error, "not-connected");
        assert_eq!(not_conn.message, "MCP 未连接或进程已退出");
    }

    fn connect_fake(script: &std::path::Path, log: &std::path::Path, mode: &str) -> Result<Value, Bad> {
        let body = json!({
            "command": "python",
            "args": ["-u", script.to_string_lossy(), log.to_string_lossy(), mode],
            "env": {"PYTHONUNBUFFERED": "1"},
            "timeout": 8,
        });
        connect_blocking(&serde_json::to_vec(&body).unwrap())
    }

    fn log_pid(path: &std::path::Path) -> u32 {
        log_events(path)
            .into_iter()
            .find_map(|event| event.get("pid").and_then(|id| id.as_u64()))
            .unwrap() as u32
    }

    fn log_init_params(path: &std::path::Path) -> Value {
        log_events(path)
            .into_iter()
            .find(|event| event.get("event").and_then(|v| v.as_str()) == Some("initialize"))
            .unwrap()
            .get("params")
            .cloned()
            .unwrap()
    }

    fn log_events(path: &std::path::Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }

    fn pid_alive(pid: u32) -> bool {
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                false
            } else {
                CloseHandle(handle);
                true
            }
        }
    }

    struct SessionGuard;

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            disconnect_blocking();
        }
    }

    struct RmDir(std::path::PathBuf);

    impl Drop for RmDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const FAKE_PY: &str = r#"
import json, os, sys
log_path = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "frame"

def log(obj):
    with open(log_path, "a", encoding="utf-8") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")

log({"pid": os.getpid(), "event": "start"})

def send(obj):
    raw = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if mode == "ndjson":
        os.write(1, raw + b"\n")
    else:
        os.write(1, ("Content-Length: %d\r\n\r\n" % len(raw)).encode("ascii") + raw)

buf = b""
while True:
    chunk = os.read(0, 4096)
    if not chunk:
        break
    buf += chunk
    while True:
        if b"\r\n\r\n" not in buf:
            break
        header, _, rest = buf.partition(b"\r\n\r\n")
        length = None
        for line in header.replace(b"\r\n", b"\n").split(b"\n"):
            if line.lower().startswith(b"content-length:"):
                try:
                    length = int(line.split(b":", 1)[1].strip())
                except Exception:
                    length = None
        if length is None:
            buf = rest
            continue
        if len(rest) < length:
            break
        body, buf = rest[:length], rest[length:]
        msg = json.loads(body.decode("utf-8"))
        method = msg.get("method")
        if method == "initialize":
            log({"pid": os.getpid(), "event": "initialize", "params": msg.get("params")})
            send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "0"},
            }})
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {"tools": [
                {"name": "echo", "description": "回声", "inputSchema": {"type": "object"}},
                {"name": 12, "description": None, "inputSchema": "nope"},
                "skip-me",
            ]}})
        elif method == "tools/call":
            params = msg.get("params") or {}
            args = params.get("arguments") or {}
            send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {"echo": args.get("text")}})
"#;
}
