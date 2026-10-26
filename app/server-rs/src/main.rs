//! 故事工坊本地服务。不带 `--no-window` 时在同一进程开壳窗口。
//! `--no-window` / `--no-browser` 只跑服务，不开窗口、不打开浏览器。
// 发布形态（frozen，票11）不带控制台，对齐 PyInstaller --windowed 壳变体；
// 无窗口跑法的失败说明走退出码，开发构建仍带控制台。
#![cfg_attr(feature = "frozen", windows_subsystem = "windows")]

/// 无控制台形态下 stdout 句柄无效，直接打印写失败会 panic（对齐 PyInstaller
/// --windowed 的 NullWriter）：写失败静默丢弃，日志面仍有 故事工坊日志.txt。
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
    }};
}

mod ai;
mod dpapi;
mod fsapi;
mod fsops;
mod gate;
mod hotkey;
mod http;
mod lock;
mod mcp;
mod scan;
mod search;
mod secret;
mod shell;
mod skillmeta;
mod state;
mod upstream;
mod util;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use state::{App, APP_TITLE, CFG_NAME, PORT_HI, PORT_LO};

pub(crate) struct Args {
    pub(crate) no_ui: bool,
    pub(crate) app: String,
    pub(crate) data: String,
    pub(crate) port: u16,
}

pub(crate) struct Boot {
    pub app: Arc<App>,
    pub listener: std::net::TcpListener,
    pub url: String,
}

pub(crate) enum BootEarly {
    Ready(Boot),
    Reuse { url: String },
}

struct DataLockGuard<'a>(&'a App);

impl Drop for DataLockGuard<'_> {
    fn drop(&mut self) {
        if let Some((root, _)) = self.0.data_snapshot() {
            lock::release(self.0, &root);
        }
    }
}

fn main() {
    let code = match boot() {
        Ok(code) => code,
        Err(msg) => {
            outln!("{msg}");
            1
        }
    };
    std::process::exit(code);
}

fn boot() -> Result<i32, String> {
    // 与 Python 模块导入时读 SF_AI_TIMEOUT 一样：非法值不起服务。
    ai::init_timeout()?;
    let args = parse_args()?;
    if args.no_ui {
        serve_headless(args)
    } else {
        shell::run(args)
    }
}

fn serve_headless(args: Args) -> Result<i32, String> {
    match boot_server(&args, false)? {
        BootEarly::Reuse { url: _ } => {
            outln!("[SF] reuse");
            outln!("已有一个故事工坊服务在运行，直接打开页面。");
            Ok(0)
        }
        BootEarly::Ready(boot) => {
            let url = boot.url.clone();
            // 日志行口径与 Python 壳一致（sf_shell.py 的 --no-window 路径同样落日志文件）。
            shell::log_line(&format!(
                "[SF] shell start (pid {}, frozen={})",
                std::process::id(),
                cfg!(feature = "frozen")
            ));
            shell::log_line(&format!("程序目录：{}", boot.app.exe_dir.display()));
            shell::log_line(&format!("[SF] no-window serving {url}"));
            let root = boot
                .app
                .data_snapshot()
                .map(|(p, _)| path_text(&p))
                .unwrap_or_else(|| "（尚未设置，请在页面里填一个路径）".into());
            outln!("[SF] serving {url}");
            outln!("{APP_TITLE}本地服务已启动：{url}");
            outln!("数据文件夹：{root}");
            outln!("保持本窗口开着；用完按 Ctrl+C（或直接关掉本窗口）。");
            let guard_app = boot.app.clone();
            let serving = boot.app.clone();
            let listener = boot.listener;
            let _lock_guard = DataLockGuard(&guard_app);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("运行时启动失败：{e}"))?;
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("监听转入运行时失败");
                let router = axum::Router::new().fallback(http::dispatch).with_state(serving);
                let shutdown = async {
                    let _ = tokio::signal::ctrl_c().await;
                };
                let _ = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;
            });
            Ok(0)
        }
    }
}

pub(crate) fn boot_server(args: &Args, shell_mode: bool) -> Result<BootEarly, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("无法定位程序位置：{e}"))?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg_path = exe_dir.join(CFG_NAME);
    let mut cfg = load_cfg(&cfg_path);
    let mut port = if args.port != 0 { args.port } else { json_port(&cfg) };

    if args.port == 0 && port < 1024 {
        let pick = (PORT_LO..=PORT_HI).find(|p| port_free(*p)).ok_or_else(|| {
            format!("找不到空闲端口（{PORT_LO}~{PORT_HI} 都被占用），请腾出一个后重试。")
        })?;
        port = pick;
        let map = cfg.as_object_mut().ok_or_else(|| "配置不是对象".to_string())?;
        if !map.contains_key("app") {
            map.insert("app".into(), json!(APP_TITLE));
        }
        map.insert("port".into(), json!(pick));
        map.insert("ver".into(), json!(1));
        save_cfg(&cfg_path, &cfg)?;
        outln!("[SF] port fixed: {pick}");
        outln!("已固定本地端口 {pick}（记在 {CFG_NAME}；请勿修改，否则地址会变）");
    }

    if !port_free(port) {
        if our_server(port) {
            return Ok(BootEarly::Reuse { url: format!("http://127.0.0.1:{port}/") });
        }
        return Err(format!(
            "端口 {port} 被别的程序占用了。请关掉占用者再运行本服务；不要改端口（改了地址就变）。"
        ));
    }

    let mut origins = std::collections::HashSet::new();
    let mut hosts = std::collections::HashSet::new();
    origins.insert(format!("http://127.0.0.1:{port}"));
    origins.insert(format!("http://localhost:{port}"));
    hosts.insert(format!("127.0.0.1:{port}"));
    hosts.insert(format!("localhost:{port}"));

    let app = Arc::new(App {
        token: new_token(),
        port,
        exe_dir,
        data: std::sync::Mutex::new(None),
        lock: std::sync::Mutex::new(None),
        cfg: std::sync::Mutex::new(cfg.clone()),
        write_lock: std::sync::Mutex::new(()),
        app_override: if args.app.is_empty() { None } else { Some(PathBuf::from(&args.app)) },
        shell: shell_mode,
        origins,
        hosts,
    });

    if !args.data.is_empty() {
        match gate::check_data_root(&args.data) {
            Ok(path) => app.install_data(path),
            Err(e) => return Err(format!("--data 参数不可用：{}", e.message)),
        }
    } else if let Some(raw) = cfg.get("dataRoot").and_then(|v| v.as_str()) {
        if !raw.trim().is_empty() {
            match gate::check_data_root(raw) {
                Ok(path) => app.install_data(path),
                Err(e) => outln!("配置里的数据文件夹当前不可用：{}（打开设置页重新指定）", e.message),
            }
        }
    }

    if let Some((root, _)) = app.data_snapshot() {
        match lock::acquire(&app, &root) {
            Ok(info) => outln!("[SF] lock acquired (pid {}, port {port})", info.pid),
            Err(e) => return Err(format!("起服务被拒：{}", e.message)),
        }
    }

    let listener = match bind_listener(port) {
        Ok(listener) => listener,
        Err(e) => {
            if let Some((root, _)) = app.data_snapshot() {
                lock::release(&app, &root);
            }
            return Err(format!("监听 127.0.0.1:{port} 失败：{e}"));
        }
    };
    Ok(BootEarly::Ready(Boot {
        app,
        listener,
        url: format!("http://127.0.0.1:{port}/"),
    }))
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args { no_ui: false, app: String::new(), data: String::new(), port: 0 };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--no-browser" | "--no-window" => args.no_ui = true,
            "--app" => {
                i += 1;
                if i < argv.len() {
                    args.app = argv[i].clone();
                }
            }
            "--data" => {
                i += 1;
                if i < argv.len() {
                    args.data = argv[i].clone();
                }
            }
            "--port" => {
                i += 1;
                if i >= argv.len() {
                    return Err("--port 缺少端口号".into());
                }
                args.port = argv[i].parse::<u16>().map_err(|_| format!("--port 不是端口号：{}", argv[i]))?;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(args)
}

fn new_token() -> String {
    let mut buf = [0u8; 24];
    getrandom::fill(&mut buf).expect("系统随机数不可用");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn load_cfg(path: &Path) -> Value {
    let Ok(bytes) = std::fs::read(path) else {
        return json!({});
    };
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice::<Value>(bytes).ok().filter(|v| v.is_object()).unwrap_or_else(|| json!({}))
}

pub(crate) fn save_cfg(path: &Path, cfg: &Value) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_file_name(format!("{CFG_NAME}.tmp"));
    std::fs::write(&tmp, body).map_err(|e| format!("写配置失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("写配置失败：{e}"))?;
    Ok(())
}

fn json_port(cfg: &Value) -> u16 {
    match cfg.get("port") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as u16,
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn port_free(port: u16) -> bool {
    let Ok(socket) = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)) else {
        return false;
    };
    socket.bind(&sock_addr(port)).is_ok()
}

fn bind_listener(port: u16) -> std::io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&sock_addr(port))?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(std::net::TcpListener::from(socket))
}

fn sock_addr(port: u16) -> SockAddr {
    SockAddr::from(SocketAddr::from(([127, 0, 0, 1], port)))
}

pub(crate) fn our_server(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET /__sf_ping HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if std::io::Write::write_all(&mut stream, req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut stream, &mut buf);
    String::from_utf8_lossy(&buf).contains(state::PING_MARK)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}
