//! 数据夹单写者锁：独占创建、进程映像判活、残锁接管。

use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::gate::{bad, bad_extra, Bad, WS_NAME};
use crate::state::{App, APP_TITLE, VER};

const LOCK_NAME: &str = "服务锁.json";
const OUR_IMAGES: &[&str] = &["故事工坊.exe", "python.exe", "pythonw.exe", "sf-server.exe"];
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const STILL_ACTIVE: u32 = 259;

#[derive(Clone, Serialize)]
pub struct LockInfo {
    pub app: &'static str,
    pub ver: &'static str,
    pub pid: u32,
    pub port: u16,
    pub at: String,
}

pub struct HeldLock {
    pub root: PathBuf,
    pub info: LockInfo,
}

struct Holder {
    info: Value,
}

pub fn acquire(app: &App, root: &Path) -> Result<LockInfo, Bad> {
    let info = LockInfo {
        app: APP_TITLE,
        ver: VER,
        pid: std::process::id(),
        port: app.port,
        at: local_time(),
    };
    let path = lock_file(root);
    std::fs::create_dir_all(path.parent().unwrap_or(root))
        .map_err(|e| bad(500, "lock-failed", format!("创建 _工坊 失败：{e}")))?;

    for _ in 0..3 {
        if let Some(holder) = lock_holder(root) {
            if !lock_is_mine(&holder.info, app.port) {
                return Err(locked(holder));
            }
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let body = serde_json::to_vec_pretty(&info)
                    .map_err(|e| bad(500, "lock-failed", format!("写服务锁失败：{e}")))?;
                file.write_all(&body)
                    .map_err(|e| bad(500, "lock-failed", format!("写服务锁失败：{e}")))?;
                drop(file);
                return Ok(take(app, root, info));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_read(root).is_some_and(|current| lock_is_mine(&current, app.port)) {
                    let tmp = path.with_file_name(format!("{LOCK_NAME}.tmp"));
                    let body = serde_json::to_vec_pretty(&info)
                        .map_err(|e| bad(500, "lock-failed", format!("写服务锁失败：{e}")))?;
                    std::fs::write(&tmp, body)
                        .and_then(|_| std::fs::rename(&tmp, &path))
                        .map_err(|e| bad(500, "lock-failed", format!("写服务锁失败：{e}")))?;
                    return Ok(take(app, root, info));
                }
                if let Some(other) = lock_holder(root) {
                    return Err(locked(other));
                }
                if let Some(other) = lock_holder(root) {
                    return Err(locked(other));
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => return Err(bad(500, "lock-failed", format!("写服务锁失败：{e}"))),
        }
    }
    Err(bad(500, "lock-failed", "服务锁争用未成，请稍后重试"))
}

pub fn release(app: &App, root: &Path) {
    if let Some(info) = lock_read(root) {
        if value_int(info.get("pid")) == i64::from(std::process::id())
            && value_int(info.get("port")) == i64::from(app.port)
        {
            let _ = std::fs::remove_file(lock_file(root));
        }
    }
    let mut current = app.lock.lock().unwrap_or_else(|e| e.into_inner());
    let same = current
        .as_ref()
        .is_none_or(|held| same_root(&held.root, root));
    if same {
        *current = None;
    }
}

fn take(app: &App, root: &Path, info: LockInfo) -> LockInfo {
    let mut current = app.lock.lock().unwrap_or_else(|e| e.into_inner());
    *current = Some(HeldLock {
        root: root.to_path_buf(),
        info: info.clone(),
    });
    info
}

fn lock_file(root: &Path) -> PathBuf {
    root.join(WS_NAME).join(LOCK_NAME)
}

fn lock_read(root: &Path) -> Option<Value> {
    let bytes = std::fs::read(lock_file(root)).ok()?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .filter(Value::is_object)
}

fn lock_is_mine(info: &Value, port: u16) -> bool {
    value_int(info.get("pid")) == i64::from(std::process::id())
        && value_int(info.get("port")) == i64::from(port)
}

fn lock_holder(root: &Path) -> Option<Holder> {
    let info = lock_read(root)?;
    let port = value_int(info.get("port"));
    let pid = value_int(info.get("pid"));
    if pid > 0 {
        if !pid_alive(pid) {
            return None;
        }
        let image = proc_image(pid as u32);
        if !image.is_empty() && !our_image(&image) {
            return None;
        }
        return Some(Holder { info });
    }
    if port > 0 && u16::try_from(port).ok().is_some_and(crate::our_server) {
        return Some(Holder { info });
    }
    None
}

fn locked(holder: Holder) -> Bad {
    let pid = holder.info.get("pid").cloned().unwrap_or(Value::Null);
    let port = holder.info.get("port").cloned().unwrap_or(Value::Null);
    let message = format!(
        "这个数据文件夹已被另一个故事工坊实例占用（pid {}，端口 {}）。请改用那个实例（http://127.0.0.1:{}/），或先把它关掉再启动；若确认没有实例在跑，可删除 _工坊/{} 后重试。",
        py_text(&pid),
        py_text(&port),
        py_text(&port),
        LOCK_NAME
    );
    let e = bad(409, "locked", message);
    let e = bad_extra(e, "holderPort", port);
    bad_extra(e, "holderPid", pid)
}

fn value_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|v| i64::try_from(v).ok()))
            .or_else(|| n.as_f64().map(|v| v as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(v)) => i64::from(*v),
        _ => 0,
    }
}

fn py_text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(s) => s.clone(),
        Value::Bool(v) => if *v { "True" } else { "False" }.into(),
        _ => value.to_string(),
    }
}

pub fn same_root(a: &Path, b: &Path) -> bool {
    normcase(a) == normcase(b)
}

fn normcase(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if text.starts_with("\\\\?\\UNC\\") {
        text = format!("\\\\{}", &text[8..]);
    } else if text.starts_with("\\\\?\\") {
        text = text[4..].to_string();
    }
    text.to_lowercase()
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i64::from(u32::MAX) {
        return false;
    }
    if pid == i64::from(std::process::id()) {
        return true;
    }
    let Some(handle) = ProcessHandle::open(pid as u32) else {
        return false;
    };
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(handle.0, &mut code) != 0 && code == STILL_ACTIVE }
}

fn proc_image(pid: u32) -> String {
    let Some(handle) = ProcessHandle::open(pid) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle.0, 0, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn our_image(image: &str) -> bool {
    Path::new(image)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_lowercase())
        .is_some_and(|name| OUR_IMAGES.contains(&name.as_str()))
}

struct ProcessHandle(*mut c_void);

impl ProcessHandle {
    fn open(pid: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut c_void,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLocalTime(time: *mut SystemTime);
}

#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

pub(crate) fn local_time() -> String {
    let mut time = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut time) }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.year, time.month, time.day, time.hour, time.minute, time.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_images_include_rust_and_python() {
        for image in [
            r"C:\story\故事工坊.exe",
            r"C:\Python\python.exe",
            r"C:\Python\pythonw.exe",
            r"D:\sf-unified\app\server-rs\target\debug\sf-server.exe",
        ] {
            assert!(our_image(image), "{image}");
        }
        assert!(!our_image(r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn same_root_accepts_windows_aliases() {
        assert!(same_root(Path::new(r"D:\Book"), Path::new(r"d:\book")));
        assert!(same_root(Path::new(r"\\?\D:\Book"), Path::new(r"d:\book")));
        assert!(same_root(
            Path::new(r"\\?\UNC\server\share\Book"),
            Path::new(r"\\server\share\book"),
        ));
        assert!(!same_root(Path::new(r"D:\Book"), Path::new(r"D:\Other")));
    }

    #[test]
    fn invalid_pid_never_counts_as_alive() {
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
        assert!(!pid_alive(i64::from(u32::MAX) + 1));
    }
}
