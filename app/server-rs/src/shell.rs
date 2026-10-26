//! 壳窗口。服务仍是本进程里的 axum；`--no-window` 不走这里。
//! 插件只注册 single-instance 与 dialog，且 single-instance 在前。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{LogicalPosition, Manager, PhysicalPosition, Position, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

use crate::state::{App, APP_TITLE};
use crate::{boot_server, Args, BootEarly};

const WIN_W: f64 = 1600.0;
const WIN_H: f64 = 1000.0;
const WIN_MIN_W: f64 = 1200.0;
const WIN_MIN_H: f64 = 700.0;
const CLOSE_WAIT: Duration = Duration::from_millis(600);

/* 片段深化票03：托盘常驻＋快记窗。左键＝快记窗显隐切换；右键菜单＝显示主窗口/退出；
   关主窗＝隐藏（页面继续运行、自动保存照常）；退出仅托盘菜单（与 shell_quit 命令同一函数）。 */
const QUICK_LABEL: &str = "quick";
const QUICK_TITLE: &str = "速记片段";
const QUICK_W: f64 = 380.0;
const QUICK_H: f64 = 300.0;
const QUICK_EDGE: f64 = 12.0;
const TRAY_TOOLTIP: &str = "故事工坊 · 速记片段";
const TRAY_ICON: &[u8] = include_bytes!("../../assets/故事工坊.ico");

const CLOSE_SCRIPT: &str = r#"(function(){var busy=false,failed=false;try{busy=!!(window.ST&&(window.ST.writing||window.ST.pending));}catch(e){failed=true;}try{var core=window.__TAURI__&&window.__TAURI__.core;if(core&&typeof core.invoke==='function'){core.invoke('shell_close_answer',{busy:busy,failed:failed});}}catch(e){}})();"#;

struct CloseAnswer {
    busy: bool,
    failed: bool,
}

struct CloseGate {
    deciding: AtomicBool,
    force: AtomicBool,
    pending: Mutex<Option<mpsc::Sender<CloseAnswer>>>,
}

impl CloseGate {
    fn new() -> Self {
        Self {
            deciding: AtomicBool::new(false),
            force: AtomicBool::new(false),
            pending: Mutex::new(None),
        }
    }
}

pub(crate) struct ShellState {
    pub(crate) http: Mutex<Option<Arc<App>>>,
    close: Arc<CloseGate>,
    origin: Mutex<Option<String>>,   // 片段深化票03：本进程页面来源（自建/复用同一服务），快记窗 URL 由此出
    /* 反馈修复（2026-10-23）：快记窗最近一次 show 的时刻。WebView2 首次显示时焦点会在壳窗与
       WebView2 内部 hwnd 间抖动（实测 shown 后 1~35ms 内连发 Focused(true)→false×2），「失焦=收起」
       会把刚弹出的窗立即藏掉——首击闪没、第二击才正常。弹出后 QUICK_FOCUS_GRACE 内的失焦不收起
       （判定见 blur_should_hide）。 */
    quick_shown: Mutex<Option<std::time::Instant>>,
}

struct OwnedServer {
    app: Arc<App>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl OwnedServer {
    fn shutdown_and_release(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
        if let Some((root, _)) = self.app.data_snapshot() {
            crate::lock::release(&self.app, &root);
        }
    }
}

impl Drop for OwnedServer {
    fn drop(&mut self) {
        self.shutdown_and_release();
    }
}

pub(crate) fn run(args: Args) -> Result<i32, String> {
    // 日志行口径与 Python 壳一致（sf_shell.py）：壳冒烟按这些行断言，缺了就是行为差异。
    log_line(&format!(
        "[SF] shell start (pid {}, frozen={})",
        std::process::id(),
        cfg!(feature = "frozen")
    ));
    log_line(&format!("程序目录：{}", exe_dir().display()));
    let failed = Arc::new(AtomicBool::new(false));
    let failed_setup = failed.clone();
    let args_setup = args;
    let built = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            log_line("[SF] reuse：端口上已有自家实例");
            if argv.iter().any(|a| a == "--data") {
                log_line("[SF] 注意：已有实例在运行，本次 --data 未生效（数据夹由那个实例决定）");
            }
            let confirmed = focus_existing(&app);
            log_line(if confirmed {
                "[SF] foreground ok：已把已有窗口前置，本实例退出"
            } else {
                "[SF] foreground partial：已把已有窗口前置，本实例退出"
            });
        }))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![pick_folder, shell_close_answer, shell_quit])
        .setup(move |app| {
            let state = ShellState {
                http: Mutex::new(None),
                close: Arc::new(CloseGate::new()),
                origin: Mutex::new(None),
                quick_shown: Mutex::new(None),
            };
            app.manage(state);
            if let Err(msg) = launch(app.handle(), &args_setup) {
                failed_setup.store(true, Ordering::SeqCst);
                log_line(&format!("启动失败：{msg}"));
                if show_error_window(app.handle(), &msg).is_err() {
                    message_box(&msg);
                }
            } else {
                if let Err(msg) = setup_tray(app.handle()) {
                    // 托盘起不来不拖死主窗口：记日志，关主窗仍隐藏但只能经任务管理器退出（如实登记）
                    log_line(&format!("托盘起不来：{msg}"));
                }
                crate::hotkey::start(app.handle());   // 速记体验重做票04：全局快速记录快捷键（失败只记日志）
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| {
            let msg = format!("窗口起不来：{e}");
            log_line(&msg);
            message_box(&msg);
            msg
        })?;
    let failed_run = failed.clone();
    // 事件循环在退出时直接结束进程，不会回到这里。锁要在 Exit 里放，失败退出码要在 ExitRequested 里改。
    built.run(move |app, event| match event {
        tauri::RunEvent::ExitRequested { code, api, .. } => {
            if failed_run.load(Ordering::SeqCst) && code != Some(1) {
                api.prevent_exit();
                app.exit(1);
            }
        }
        tauri::RunEvent::Exit => close_summary(app, failed_run.load(Ordering::SeqCst)),
        _ => {}
    });
    Ok(if failed.load(Ordering::SeqCst) { 1 } else { 0 })
}

/// 关窗收尾日志（对齐 sf_shell.py 的 run_window 尾段）：持有服务＝停服务拆锁，借来的＝不动别人。
fn close_summary(app: &tauri::AppHandle, failed: bool) {
    log_line("[SF] window closed");
    if failed {
        log_line("启动失败退出（服务与锁已在失败路径处理）");
        return;
    }
    let owned = app.try_state::<OwnedServer>();
    release_managed(app);
    log_line(match owned {
        Some(_) => "服务已停、数据夹锁已拆",
        None => "复用了已有实例，未动其服务与锁",
    });
}

fn release_managed(app: &tauri::AppHandle) {
    if let Some(owned) = app.try_state::<OwnedServer>() {
        owned.shutdown_and_release();
    }
}

fn launch(app: &tauri::AppHandle, args: &Args) -> Result<(), String> {
    match boot_server(args, true)? {
        BootEarly::Reuse { url } => {
            log_line("[SF] reuse：端口上已有自家实例");
            if !args.data.is_empty() {
                log_line("[SF] 注意：已有实例在运行，本次 --data 未生效（数据夹由那个实例决定）");
            }
            log_line(&format!(
                "[SF] foreground miss（端口上已有服务且没有本进程的窗口）：降级为本实例开一个窗口指过去 {url}"
            ));
            // 复用分支同样要记来源：快记窗 URL 由此出（2026-10-23 复验修复漏写）
            if let Some(state) = app.try_state::<ShellState>() {
                *state.origin.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.clone());
            }
            open_page(app, &url)?;
            Ok(())
        }
        BootEarly::Ready(boot) => {
            let owned = match spawn_server(boot.app.clone(), boot.listener) {
                Ok(owned) => owned,
                Err(e) => {
                    release_now(&boot.app);
                    return Err(e);
                }
            };
            if let Some(state) = app.try_state::<ShellState>() {
                *state.http.lock().unwrap_or_else(|e| e.into_inner()) = Some(owned.app.clone());
                *state.origin.lock().unwrap_or_else(|e| e.into_inner()) = Some(boot.url.clone());
            }
            let start = start_url(&boot.url, owned.app.data_snapshot().is_some());
            log_line(&format!("[SF] shell serving {}", boot.url));
            log_line(&format!("[SF] window open：{start}"));
            let win = match open_page(app, &start) {
                Ok(win) => win,
                Err(e) => {
                    drop(owned);
                    return Err(e);
                }
            };
            watch_root(win, owned.app.clone(), boot.url);
            app.manage(owned);
            Ok(())
        }
    }
}

fn release_now(app: &App) {
    if let Some((root, _)) = app.data_snapshot() {
        crate::lock::release(app, &root);
    }
}

fn spawn_server(app: Arc<App>, listener: std::net::TcpListener) -> Result<OwnedServer, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let serving = app.clone();
    std::thread::Builder::new()
        .name("sf-http".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log_line(&format!("运行时启动失败：{e}"));
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(e) => {
                        log_line(&format!("监听转入运行时失败：{e}"));
                        return;
                    }
                };
                let router = axum::Router::new().fallback(crate::http::dispatch).with_state(serving);
                let shutdown = async {
                    let _ = rx.await;
                };
                let _ = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;
            });
        })
        .map_err(|e| format!("服务线程起不来：{e}"))?;
    Ok(OwnedServer {
        app,
        shutdown: Mutex::new(Some(tx)),
    })
}

fn start_url(origin: &str, has_root: bool) -> String {
    let base = origin.trim_end_matches('/');
    let debug = std::env::var("SF_SETUP_DEBUG")
        .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if has_root {
        format!("{base}/index.html")
    } else if debug {
        format!("{base}/__sf_debug")
    } else {
        format!("{base}/__sf_setup")
    }
}

fn open_page(app: &tauri::AppHandle, url: &str) -> Result<WebviewWindow, String> {
    let parsed: url::Url = url.parse().map_err(|e| format!("地址无效：{e}"))?;
    let gate = app
        .try_state::<ShellState>()
        .map(|s| s.close.clone())
        .unwrap_or_else(|| Arc::new(CloseGate::new()));
    let win = build_window(app, WebviewUrl::External(parsed), WIN_W, WIN_H, true, None)?;
    attach_close(win.clone(), app.clone(), gate, true);
    Ok(win)
}

fn show_error_window(app: &tauri::AppHandle, msg: &str) -> Result<(), String> {
    let html = error_html(msg);
    let js = format!(
        "document.open();document.write({});document.close();",
        serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".into())
    );
    let gate = app
        .try_state::<ShellState>()
        .map(|s| s.close.clone())
        .unwrap_or_else(|| Arc::new(CloseGate::new()));
    let win = build_window(
        app,
        WebviewUrl::App("index.html".into()),
        760.0,
        420.0,
        false,
        Some(&js),
    )?;
    attach_close(win, app.clone(), gate, false);
    Ok(())
}

fn build_window(
    app: &tauri::AppHandle,
    url: WebviewUrl,
    width: f64,
    height: f64,
    with_min: bool,
    init_script: Option<&str>,
) -> Result<WebviewWindow, String> {
    let data = webview_data_dir();
    let _ = std::fs::create_dir_all(&data);
    let nav_app = app.clone();
    let mut builder = WebviewWindowBuilder::new(app, "main", url)
        .title(APP_TITLE)
        .inner_size(width, height)
        .background_color(tauri::utils::config::Color(0x14, 0x14, 0x16, 255))
        .data_directory(data)
        // 照译 pywebview 壳 OPEN_EXTERNAL_LINKS_IN_BROWSER=False：新窗请求（window.open／
        // target=_blank）不开新窗、不弹系统浏览器，当前窗口整页跳转过去（说明书同款口径）。
        // 不注册时 wry 的默认口径是静默拒绝——点了没反应，与旧壳可观察行为对不上。
        .on_new_window(move |url, _features| {
            if let Some(win) = nav_app.get_webview_window("main") {
                if let Ok(u) = serde_json::to_string(url.as_str()) {
                    let _ = win.eval(&format!("location.href={u}"));
                }
            }
            tauri::webview::NewWindowResponse::Deny
        });
    if with_min {
        builder = builder.min_inner_size(WIN_MIN_W, WIN_MIN_H);
    }
    if let Some(script) = init_script {
        builder = builder.initialization_script(script);
    }
    if let Ok(extra) = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        if !extra.trim().is_empty() {
            let args = format!("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection {extra}");
            builder = builder.additional_browser_args(&args);
        }
    }
    let built = builder.build().map_err(|e| format!("窗口起不来：{e}"))?;
    hide_aux_windows();
    Ok(built)
}

/// 隐藏壳进程里 WebView2 运行时与 tao 事件循环在宿主进程创建的辅助顶层窗
/// （特征＝WS_EX_TOOLWINDOW＋WS_EX_NOACTIVATE＋WS_EX_TRANSPARENT，永不接收输入）。
/// 它们带 WS_VISIBLE，会把「本进程树可见窗口恰为主窗口」的壳口径撑成 3 个；
/// pywebview 壳没有这批窗，照此隐藏对齐。周期巡检：WebView2 建窗晚于页面导航。
/// 关窗前必须恢复（unhide_aux_windows）：tao 靠事件目标窗的 WM_PAINT 派发跨线程
/// 事件（destroy/eval 都走这条路），隐藏的窗口收不到 WM_PAINT，进程会退不出去。
static AUX_HIDE_STOP: AtomicBool = AtomicBool::new(false);

fn hide_aux_windows() {
    AUX_HIDE_STOP.store(false, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("sf-aux-hide".into())
        .spawn(|| loop {
            if AUX_HIDE_STOP.load(Ordering::SeqCst) {
                break;
            }
            unsafe {
                EnumWindows(hide_aux_cb, 0);
            }
            std::thread::sleep(Duration::from_millis(800));
        })
        .ok();
}

/// 关窗时停巡检并把辅助窗恢复显示，把事件派发交还给已验证的默认退出路径。
fn unhide_aux_windows() {
    AUX_HIDE_STOP.store(true, Ordering::SeqCst);
    unsafe {
        EnumWindows(unhide_aux_cb, 0);
    }
}

const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

fn aux_signature(hwnd: isize) -> bool {
    let ex = unsafe { GetWindowLongW(hwnd, -20) } as u32;
    ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
        == (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
}

extern "system" fn hide_aux_cb(hwnd: isize, _: isize) -> i32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid != std::process::id() || !aux_signature(hwnd) {
        return 1;
    }
    unsafe {
        ShowWindow(hwnd, 0);
    }
    1
}

extern "system" fn unhide_aux_cb(hwnd: isize, _: isize) -> i32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid != std::process::id() || !aux_signature(hwnd) {
        return 1;
    }
    unsafe {
        ShowWindow(hwnd, 5);
    }
    1
}

fn error_html(msg: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let log = exe_dir().join("故事工坊日志.txt");
    format!(
        "<!DOCTYPE html><html lang=\"zh\"><head><meta charset=\"utf-8\"><title>{APP_TITLE}</title></head>\
         <body style=\"margin:0;padding:26px 30px;font:15px/1.7 'Microsoft YaHei',sans-serif;color:#1f2430\">\
         <h2 style=\"margin:0 0 10px\">{APP_TITLE} 起不来</h2>\
         <pre style=\"white-space:pre-wrap;word-break:break-all;background:#f6f7f9;border:1px solid #e3e6ec;border-radius:10px;padding:14px\">{}</pre>\
         <p style=\"color:#5b6474\">关掉本窗口即退出。日志：{}</p></body></html>",
        esc(msg),
        esc(&log.display().to_string())
    )
}

fn attach_close(win: WebviewWindow, app: tauri::AppHandle, gate: Arc<CloseGate>, confirm: bool) {
    win.clone().on_window_event(move |event| {
        let tauri::WindowEvent::CloseRequested { api, .. } = event else {
            return;
        };
        // tao 的跨线程事件派发依赖事件目标窗的 WM_PAINT；隐藏期收不到，关窗前先恢复。
        unhide_aux_windows();
        if gate.force.swap(false, Ordering::SeqCst) {
            return;
        }
        if !confirm {
            return;
        }
        /* 片段深化票03：关主窗＝隐藏常驻（页面继续运行、自动保存照常；主窗关按钮不再弹忙确认），
           退出仅托盘菜单「退出」（quit_gracefully 走 decide_close 忙确认→destroy→RunEvent 收尾链）。
           托盘缺席（起不来）时回退旧行为：关窗仍走 decide_close→destroy，不留「藏起退不出」的死路。 */
        if app.tray_by_id(TRAY_ID).is_some() {
            api.prevent_close();
            let _ = win.hide();
            hide_aux_windows();   // 巡检恢复（aux 窗维持隐藏，关窗动作到此为止）
            return;
        }
        api.prevent_close();
        if gate.deciding.swap(true, Ordering::SeqCst) {
            return;
        }
        let gate = gate.clone();
        let win = win.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let allow = decide_close(&win, &app, &gate);
            if allow {
                gate.force.store(true, Ordering::SeqCst);
                let _ = win.destroy();
            } else {
                hide_aux_windows();   // 关窗被取消：恢复「恰 1 可见窗口」口径
            }
            gate.deciding.store(false, Ordering::SeqCst);
        });
    });
}

/// 托盘「退出」与 shell_quit 命令的唯一出口：忙确认（保留既有 decide_close）→ destroy 全部窗口
/// → RunEvent::Exit 的 close_summary 收尾（停服务拆锁、日志摘要）。关主窗（隐藏）不走这里。
fn quit_gracefully(app: &tauri::AppHandle) {
    let gate = app
        .try_state::<ShellState>()
        .map(|s| s.close.clone())
        .unwrap_or_else(|| Arc::new(CloseGate::new()));
    if gate.deciding.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let allow = match app.get_webview_window("main") {
            Some(win) => decide_close(&win, &app, &gate),
            None => true,
        };
        if allow {
            unhide_aux_windows();   // destroy/eval 的跨线程派发要有可见事件目标窗
            if let Some(quick) = app.get_webview_window(QUICK_LABEL) {
                gate.force.store(true, Ordering::SeqCst);
                let _ = quick.destroy();
            }
            if let Some(win) = app.get_webview_window("main") {
                gate.force.store(true, Ordering::SeqCst);
                let _ = win.destroy();
            }
        } else {
            hide_aux_windows();   // 退出被取消：恢复常驻隐藏口径
        }
        gate.deciding.store(false, Ordering::SeqCst);
    });
}

#[tauri::command]
fn shell_quit(app: tauri::AppHandle) {
    // 页面无「退出」入口；本命令只供壳冒烟自动化调用，与托盘菜单「退出」共用 quit_gracefully 同一收尾链。
    quit_gracefully(&app);
}

fn setup_tray(app: &tauri::AppHandle) -> Result<(), String> {
    let show = MenuItemBuilder::with_id("show", "显示主窗口")
        .build(app)
        .map_err(|e| format!("{e}"))?;
    let quit = MenuItemBuilder::with_id("quit", "退出")
        .build(app)
        .map_err(|e| format!("{e}"))?;
    let menu = MenuBuilder::new(app)
        .items(&[&show, &quit])
        .build()
        .map_err(|e| format!("{e}"))?;
    let icon = tauri::image::Image::from_bytes(TRAY_ICON).map_err(|e| format!("托盘图标解码失败：{e}"))?;
    TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(TRAY_TOOLTIP)
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)   // 左键＝快记窗显隐；菜单只在右键
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                focus_existing(app);
            }
            "quit" => quit_gracefully(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_quick_window(tray.app_handle());
            }
        })
        .build(app)
        .map_err(|e| format!("{e}"))?;
    log_line("[SF] tray ready：托盘常驻（左键＝速记片段，右键＝显示主窗口/退出）");
    Ok(())
}

const TRAY_ID: &str = "sf-tray";

/// 左键切换快记窗：已见→隐藏；未见/未建→（建好后）挪到鼠标处显示＋聚焦，并通知页面「新起一条」。
pub(crate) fn toggle_quick_window(app: &tauri::AppHandle) {
    match get_or_build_quick(app) {
        Ok(win) => {
            let visible = win.is_visible().unwrap_or(false);
            if visible {
                let _ = win.hide();
                log_line("[SF] quick hidden：速记片段窗隐藏");
            } else {
                place_quick_at_cursor(&win);   // 速记体验重做票04：显示前先挪到全局光标处（取不到回退右下角）
                mark_quick_shown(app);
                let _ = win.show();
                let _ = win.set_focus();
                // 每次打开＝新起一条（旧条已存不清）；页面未就绪时 eval 失败无害（首建页面自带新条）
                let _ = win.eval("window.__sfQuickNew&&window.__sfQuickNew()");
                log_line("[SF] quick shown：速记片段窗弹出");
            }
        }
        Err(msg) => log_line(&format!("速记片段窗起不来：{msg}")),
    }
}

fn get_or_build_quick(app: &tauri::AppHandle) -> Result<WebviewWindow, String> {
    if let Some(win) = app.get_webview_window(QUICK_LABEL) {
        return Ok(win);
    }
    let origin = app
        .try_state::<ShellState>()
        .and_then(|s| s.origin.lock().unwrap_or_else(|e| e.into_inner()).clone());
    let Some(origin) = origin else {
        return Err("服务地址未就绪".into());
    };
    let parsed: url::Url = format!("{}/__sf_quick", origin.trim_end_matches('/'))
        .parse()
        .map_err(|e| format!("地址无效：{e}"))?;
    let data = webview_data_dir();
    let _ = std::fs::create_dir_all(&data);
    let win = WebviewWindowBuilder::new(app, QUICK_LABEL, WebviewUrl::External(parsed))
        .title(QUICK_TITLE)
        .inner_size(QUICK_W, QUICK_H)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)   // 先隐式建窗定位，再由 toggle 显示，避免先闪在屏幕中央
        .data_directory(data)
        .build()
        .map_err(|e| format!("窗口起不来：{e}"))?;
    // 建窗先落主显示器右下角初始位（避免先闪屏幕中央；每次显示前 toggle 会再按光标重定位）
    place_quick_bottom_right(&win);
    // 快记窗关窗＝隐藏（内容实时保存已过，无确认）；退出链（force）照常放行 destroy；
    // 速记体验重做票04：失焦＝收起（点了窗外任一处——页面侧 blur 已先冲刷挂起写，等同 Esc）。
    // 反馈修复（2026-10-23）：弹出后 QUICK_FOCUS_GRACE 内的失焦不收起——WebView2 首次显示的
    // 焦点抖动会在 1~35ms 内假性失焦，照收会把窗刚弹出就藏掉（首击闪没）。
    let gate = app
        .try_state::<ShellState>()
        .map(|s| s.close.clone())
        .unwrap_or_else(|| Arc::new(CloseGate::new()));
    let closer = win.clone();
    let state_app = app.clone();
    closer.clone().on_window_event(move |event| match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            if gate.force.swap(false, Ordering::SeqCst) {
                return;
            }
            api.prevent_close();
            let _ = closer.hide();
        }
        tauri::WindowEvent::Focused(false) => {
            let shown = state_app
                .try_state::<ShellState>()
                .and_then(|s| *s.quick_shown.lock().unwrap_or_else(|e| e.into_inner()));
            if blur_should_hide(shown, std::time::Instant::now()) {
                let _ = closer.hide();
            }
        }
        _ => {}
    });
    Ok(win)
}

/// 快记窗失焦宽限（反馈修复 2026-10-23）：WebView2 首次显示的焦点抖动实测在数十毫秒内平息，
/// 500ms 覆盖一个数量级余量；宽限外的失焦照旧收起（点了窗外任一处）。
const QUICK_FOCUS_GRACE: Duration = Duration::from_millis(500);

/// 失焦是否收起（纯函数，可单测）：从未 show 过（旧状态兜底）或距最近一次 show 已过宽限期＝收起。
fn blur_should_hide(last_show: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last_show {
        Some(t) => now.duration_since(t) >= QUICK_FOCUS_GRACE,
        None => true,
    }
}

/// 记快记窗本次 show 的时刻（toggle 显示分支调用；隐藏/收起不清除——宽限只认最近一次弹出）。
fn mark_quick_shown(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<ShellState>() {
        *state.quick_shown.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }
}

/// 建窗初始位/回退位：主显示器右下角（距边 QUICK_EDGE；小屏夹到 0）。
fn place_quick_bottom_right(win: &WebviewWindow) {
    if let Ok(Some(monitor)) = win.primary_monitor() {
        let scale = monitor.scale_factor();
        let size = monitor.size();
        let (x, y) = quick_xy(size.width as f64 / scale, size.height as f64 / scale);
        let _ = win.set_position(Position::Logical(LogicalPosition::new(x, y)));
    }
}

/* 速记体验重做票04：显示前把快记窗挪到全局光标处——取含光标显示器的 work_area，窗中心偏左上对齐光标后夹边（多显示器
   不越界、不压任务栏）；取不到光标/显示器回退主显示器右下角（place_quick_bottom_right 旧口径）。 */
fn place_quick_at_cursor(win: &WebviewWindow) {
    let Ok(cursor) = win.cursor_position() else {
        place_quick_bottom_right(win);
        return;
    };
    let monitor = win
        .available_monitors()
        .unwrap_or_default()
        .into_iter()
        .find(|m| {
            let (mx, my) = (m.position().x as f64, m.position().y as f64);
            let (mw, mh) = (m.size().width as f64, m.size().height as f64);
            cursor.x >= mx && cursor.x < mx + mw && cursor.y >= my && cursor.y < my + mh
        })
        .or_else(|| win.primary_monitor().ok().flatten());
    let Some(m) = monitor else {
        place_quick_bottom_right(win);
        return;
    };
    let scale = m.scale_factor();
    let wa = m.work_area();
    let (x, y) = quick_xy_at(
        cursor.x as i32,
        cursor.y as i32,
        (wa.position.x, wa.position.y, wa.size.width as i32, wa.size.height as i32),
        (QUICK_W * scale).round() as i32,
        (QUICK_H * scale).round() as i32,
        (QUICK_EDGE * scale).round() as i32,
    );
    let _ = win.set_position(Position::Physical(PhysicalPosition::new(x, y)));
}

/// 光标落点夹边（纯函数）：窗口「中心偏左上」（约 40%/40%）对齐光标，夹进工作区；
/// 窗口大于工作区时贴工作区左上（不越负界）。正中偏左上＝少挪鼠即可点到输入区。
fn quick_xy_at(cx: i32, cy: i32, area: (i32, i32, i32, i32), w: i32, h: i32, edge: i32) -> (i32, i32) {
    let (ax, ay, aw, ah) = area;
    let min_x = ax + edge;
    let max_x = (ax + aw - w - edge).max(min_x);
    let min_y = ay + edge;
    let max_y = (ay + ah - h - edge).max(min_y);
    let x = cx - (w * 2) / 5;
    let y = cy - (h * 2) / 5;
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// 快记窗在主显示器逻辑尺寸下的落点（右下角、距边 QUICK_EDGE；小屏夹到 0）。
fn quick_xy(logical_w: f64, logical_h: f64) -> (f64, f64) {
    (
        (logical_w - QUICK_W - QUICK_EDGE).max(0.0),
        (logical_h - QUICK_H - QUICK_EDGE).max(0.0),
    )
}

fn decide_close(win: &WebviewWindow, app: &tauri::AppHandle, gate: &CloseGate) -> bool {
    match probe_busy(win, gate) {
        Ok(false) => true,
        Err(()) => true,
        Ok(true) => confirm_close(app),
    }
}

fn probe_busy(win: &WebviewWindow, gate: &CloseGate) -> Result<bool, ()> {
    if std::env::var("SF_CLOSE_PROBE").ok().as_deref() == Some("fail") {
        return Err(());
    }
    let (tx, rx) = mpsc::channel();
    {
        let mut slot = gate.pending.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(tx);
    }
    if win.eval(CLOSE_SCRIPT).is_err() {
        let mut slot = gate.pending.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
        return Err(());
    }
    match rx.recv_timeout(CLOSE_WAIT) {
        Ok(ans) if ans.failed => Err(()),
        Ok(ans) => Ok(ans.busy),
        Err(_) => {
            let mut slot = gate.pending.lock().unwrap_or_else(|e| e.into_inner());
            *slot = None;
            Err(())
        }
    }
}

fn confirm_close(app: &tauri::AppHandle) -> bool {
    let shown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.dialog()
            .message("还有文件没写完。现在关闭可能丢掉未写完的部分。仍要关闭？")
            .title("正在写入数据文件夹")
            .buttons(MessageDialogButtons::YesNo)
            .blocking_show()
    }));
    match shown {
        Ok(yes) => yes,
        Err(_) => {
            log_line("关窗确认框失败，按放行");
            true
        }
    }
}

fn watch_root(window: WebviewWindow, app: Arc<App>, origin: String) {
    std::thread::spawn(move || {
        let mut last = app.data_snapshot().map(|(path, _)| path);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if window.title().is_err() {
                break;
            }
            let now = app.data_snapshot().map(|(path, _)| path);
            if same_path(last.as_deref(), now.as_deref()) {
                continue;
            }
            if now.is_some() && last.is_none() {
                let url = format!("{}index.html", origin);
                if let Ok(parsed) = url.parse() {
                    let _ = window.navigate(parsed);
                    log_line("数据夹已配置，壳内跳转应用页");
                }
            }
            last = now;
        }
    });
}

fn same_path(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle, state: tauri::State<'_, ShellState>) -> Result<String, String> {
    let start = pick_start(&state);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_directory(start).pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    match rx.await {
        Ok(Some(path)) => match path.into_path() {
            Ok(p) => Ok(p.to_string_lossy().replace('/', "\\")),
            Err(_) => Err("选择文件夹失败".into()),
        },
        Ok(None) => Ok(String::new()),
        Err(_) => Err("选择文件夹失败".into()),
    }
}

fn pick_start(state: &ShellState) -> PathBuf {
    let guard = state.http.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(app) = guard.as_ref() {
        if let Some((path, _)) = app.data_snapshot() {
            if path.is_dir() {
                return path;
            }
        }
    }
    home_dir()
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

#[tauri::command]
fn shell_close_answer(state: tauri::State<'_, ShellState>, busy: bool, failed: bool) {
    let mut slot = state.close.pending.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = slot.take() {
        let _ = tx.send(CloseAnswer { busy, failed });
    }
}

fn webview_data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SF_WEBVIEW_DATA") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join(APP_TITLE).join("壳缓存")
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn log_line(text: &str) {
    let stamp = crate::lock::local_time();
    let row = format!("{stamp} {text}");
    outln!("{row}");
    let path = exe_dir().join("故事工坊日志.txt");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(file, "{row}");
    }
}

fn message_box(text: &str) {
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
    let body = wide(text);
    let caption = wide(APP_TITLE);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), body.as_ptr(), caption.as_ptr(), 0x10);
    }
}

fn focus_existing(app: &tauri::AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
    foreground_our_windows()
}

/// 前置本进程的「故事工坊」窗口。返回是否确认拿到前台（ok/partial 口径与 sf_shell.py 一致）。
fn foreground_our_windows() -> bool {
    {
        let mut hits = FOCUS_HITS.lock().unwrap_or_else(|e| e.into_inner());
        hits.clear();
    }
    unsafe {
        EnumWindows(collect_hwnd, 0);
    }
    let hits = FOCUS_HITS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for &hwnd in &hits {
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, 9);
            } else {
                ShowWindow(hwnd, 5);
            }
            let ok = SetForegroundWindow(hwnd) != 0;
            let fg = GetForegroundWindow();
            if !ok || fg != hwnd {
                let tid_me = GetCurrentThreadId();
                let tid_fg = if fg == 0 { 0 } else { GetWindowThreadProcessId(fg, std::ptr::null_mut()) };
                if tid_fg != 0 && tid_fg != tid_me {
                    AttachThreadInput(tid_me, tid_fg, 1);
                    SetForegroundWindow(hwnd);
                    BringWindowToTop(hwnd);
                    AttachThreadInput(tid_me, tid_fg, 0);
                }
            }
        }
    }
    if hits.is_empty() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg == 0 {
        return false;
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(fg, &mut pid);
    }
    pid == std::process::id()
}

static FOCUS_HITS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

extern "system" fn collect_hwnd(hwnd: isize, _: isize) -> i32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid != std::process::id() {
        return 1;
    }
    let n = unsafe { GetWindowTextLengthW(hwnd) };
    if n <= 0 {
        return 1;
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if got <= 0 {
        return 1;
    }
    let title = String::from_utf16_lossy(&buf[..got as usize]);
    if title.starts_with(APP_TITLE) {
        if let Ok(mut hits) = FOCUS_HITS.lock() {
            hits.push(hwnd);
        }
    }
    1
}

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowTextLengthW(hwnd: isize) -> i32;
    fn GetWindowLongW(hwnd: isize, index: i32) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
    fn BringWindowToTop(hwnd: isize) -> i32;
    fn AttachThreadInput(from: u32, to: u32, attach: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, typ: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /* 快记窗几何：右下角、距边 12px；小屏夹到 0（不让窗口落到屏幕外）。 */
    #[test]
    fn quick_geometry_bottom_right() {
        assert_eq!(quick_xy(1920.0, 1080.0), (1920.0 - 380.0 - 12.0, 1080.0 - 300.0 - 12.0));
        assert_eq!(quick_xy(400.0, 300.0), (8.0, 0.0));
        assert_eq!(quick_xy(300.0, 200.0), (0.0, 0.0));
    }

    /* 速记体验重做反馈：光标定位——窗中心偏左上（约40%/40%）对齐光标、夹进工作区；
       近右下贴边收回；负坐标收回左上；副显示器带偏移；窗口大于工作区时贴工作区左上不越负界。 */
    #[test]
    fn quick_xy_at_clamps_to_work_area() {
        assert_eq!(quick_xy_at(960, 540, (0, 0, 1920, 1040), 380, 300, 12), (960 - 152, 540 - 120));
        assert_eq!(quick_xy_at(1900, 1000, (0, 0, 1920, 1040), 380, 300, 12), (1920 - 380 - 12, 1040 - 300 - 12));
        assert_eq!(quick_xy_at(-50, -50, (0, 0, 1920, 1040), 380, 300, 12), (12, 12));
        assert_eq!(quick_xy_at(3200, 700, (1920, 0, 1280, 720), 380, 300, 12), (1920 + 1280 - 380 - 12, 720 - 300 - 12));
        assert_eq!(quick_xy_at(500, 500, (0, 0, 300, 200), 380, 300, 12), (12, 12));
    }

    /* 速记体验重做票04：失焦收起与光标定位都要挂在快记窗（静态钉防回归）。 */
    #[test]
    fn quick_blur_and_cursor_contract() {
        let src = include_str!("shell.rs");
        assert!(src.contains("WindowEvent::Focused(false)"), "快记窗要挂失焦收起");
        assert!(src.contains("place_quick_at_cursor(&win)"), "显示前要按光标定位");
        assert!(src.contains("cursor_position()"), "光标取自全局查询");
        assert!(src.contains("place_quick_bottom_right(&win);"), "建窗仍有右下角初始位");
    }

    /* 托盘与快记窗的静态契约：label、tooltip、capabilities 覆盖（quick 进 windows 白名单、
       退出走同一收尾链的命令与显示/隐藏权限都登记）。 */
    #[test]
    fn tray_and_quick_contract() {
        assert_eq!(QUICK_LABEL, "quick");
        assert_eq!(QUICK_TITLE, "速记片段");
        assert!(TRAY_TOOLTIP.contains("速记片段"));
        let caps: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).expect("capabilities 是合法 JSON");
        let windows: Vec<String> = caps["windows"].as_array().expect("windows 数组").iter()
            .filter_map(|v| v.as_str().map(String::from)).collect();
        assert!(windows.iter().any(|w| w == "main"));
        assert!(windows.iter().any(|w| w == "quick"), "quick 窗口要进 capabilities");
        let perms: Vec<String> = caps["permissions"].as_array().expect("permissions 数组").iter()
            .filter_map(|v| v.as_str().map(String::from)).collect();
        assert!(perms.iter().any(|p| p == "allow-shell-quit"), "shell_quit 命令要有权限");
        assert!(perms.iter().any(|p| p == "core:window:allow-hide"), "快记窗 Esc 隐藏要 window hide 权限");
        let remote = caps["remote"]["urls"].as_array().expect("remote.urls");
        assert!(remote.iter().any(|u| u.as_str().map(|s| s.contains("127.0.0.1")).unwrap_or(false)));
    }

    /* 快记窗页面契约：令牌/写删端点/静默去重键/文件形状与主页面片段投影同源/新条与 Esc 行为钩子。 */
    #[test]
    fn quick_page_contract() {
        let page = include_str!("../pages/quick.html");
        for needle in [
            "__SF_TOKEN__",
            "x-sf-token",
            "/api/fs/list",
            "/api/fs/stat",
            "/api/fs/write",
            "/api/fs/delete",
            "sf-quick-ping",
            "novel-studio-entry-v1",
            "# 正文",
            "window.__sfQuickNew",
            "window.__TAURI__.window.getCurrentWindow().hide()",
            "resetWhenIdle",
            "ping(true,gone)",
            "灵感",
            "即刻记录灵感……",
            "自动保存",
            "addEventListener('blur'",
        ] {
            assert!(page.contains(needle), "quick.html 缺：{needle}");
        }
    }

    /* 托盘图标复用 app/assets/故事工坊.ico，编译期内嵌后必须可解码。 */
    #[test]
    fn tray_icon_decodes() {
        assert!(tauri::image::Image::from_bytes(TRAY_ICON).is_ok());
    }

    /* 快记窗 URL 来源钉：launch 的自建（Ready）与复用（Reuse）两分支都必须把服务地址写进
       ShellState.origin——漏写则 get_or_build_quick 恒报「服务地址未就绪」，托盘左键永远打不开
       快记窗（2026-10-23 两轴复验发现的致命遗漏，静态钉防回归）。 */
    #[test]
    fn quick_origin_recorded_contract() {
        let src = include_str!("shell.rs");
        let writes = src.matches("state.origin.lock()").count();
        assert!(writes >= 2, "launch 两个分支都要写 state.origin（找到 {writes} 处）");
        assert!(src.contains("BootEarly::Reuse") && src.contains("BootEarly::Ready"));
    }

    /* 反馈修复（2026-10-23）：首击焦点抖动宽限——WebView2 首显实测在 shown 后 1~35ms 内连发
       Focused(true)→false×2（插桩时间线），宽限内失焦不收起；宽限外照旧收起；从未 show 兜底＝收起。 */
    #[test]
    fn quick_blur_grace_window() {
        let now = std::time::Instant::now();
        let churn = now.checked_sub(Duration::from_millis(10)).unwrap();
        let inside = now.checked_sub(QUICK_FOCUS_GRACE - Duration::from_millis(1)).unwrap();
        let settled = now.checked_sub(Duration::from_millis(600)).unwrap();
        assert!(!blur_should_hide(Some(churn), now), "弹出后 10ms 的失焦＝首显焦点抖动，不收起");
        assert!(!blur_should_hide(Some(inside), now), "宽限边界内不收起");
        assert!(blur_should_hide(Some(settled), now), "弹出 600ms 后的失焦＝真失焦，收起");
        assert!(blur_should_hide(None, now), "从未 show（兜底）＝收起");
    }

    /* 静态钉：show 分支记弹出时刻、失焦收起经宽限判定（防回归到无脑 hide，首击再闪没）。 */
    #[test]
    fn quick_blur_grace_contract() {
        let src = include_str!("shell.rs");
        assert!(src.contains("mark_quick_shown(app);"), "show 分支要记弹出时刻");
        assert!(src.contains("blur_should_hide(shown,"), "失焦收起要经宽限判定");
        assert!(
            QUICK_FOCUS_GRACE >= Duration::from_millis(400) && QUICK_FOCUS_GRACE <= Duration::from_millis(1000),
            "宽限要在 400~1000ms（实测抖动 ~35ms，留一个量级余量）"
        );
    }
}
