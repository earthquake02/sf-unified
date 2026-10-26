//! 路径闸。读文件用这一套；后面的写入、列举、快照复用，不各写一套。
//! 口径对照 `app/server/sf_server.py` 的 norm_rel / resolve / 机密读保护。

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::state::{App, MAX_PATH};

pub const WS_NAME: &str = "_工坊";
pub const SECRET_NAME: &str = "机密";
pub const SECRET_EXT: &str = ".dat";

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
const TMP_PREFIX: &str = ".sf-tmp-";
const REPARSE: u32 = 0x400;

#[derive(Debug)]
pub struct Bad {
    pub status: u16,
    pub error: &'static str,
    pub message: String,
    pub extra: Vec<(&'static str, Value)>,
}

pub fn bad(status: u16, error: &'static str, message: impl Into<String>) -> Bad {
    Bad { status, error, message: message.into(), extra: Vec::new() }
}

pub fn bad_extra(mut e: Bad, key: &'static str, value: impl Into<Value>) -> Bad {
    e.extra.push((key, value.into()));
    e
}

/// 占用类系统错误：Windows 的访问拒绝（5）、共享冲突（32）、锁冲突（33）。
/// `std::io::ErrorKind` 只把 5 归进 `PermissionDenied`，共享冲突不会，需按原始错误码补上。
pub fn is_busy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

/// 改盘动作的系统错误：占用 → 忙；其余 → 500（与 Python 现实现的映射一致）。
pub fn io_bad(e: std::io::Error) -> Bad {
    if is_busy(&e) {
        bad(409, "busy", format!("目标被占用或只读，系统拒绝访问：{e}"))
    } else {
        bad(500, "internal", format!("OSError: {e}"))
    }
}

pub fn norm_rel(rel: &str) -> Result<Vec<String>, Bad> {
    if !py_strip(rel).is_empty() {
        // 有内容，继续
    } else {
        return Err(bad(400, "bad-path", "路径为空"));
    }
    if rel.chars().any(|c| (c as u32) < 32) {
        return Err(bad(400, "bad-path", "路径含控制字符"));
    }
    let r = rel.replace('\\', "/");
    if r != py_strip(&r) {
        return Err(bad(403, "bad-path", "路径首尾不得有空白"));
    }
    if r.starts_with('/') || is_drive(&r) || r.starts_with("//") {
        return Err(bad(403, "bad-path", "只接受数据文件夹内的相对路径"));
    }
    let mut parts = Vec::new();
    for seg in r.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(bad(403, "bad-path", "路径不得包含 .."));
        }
        if seg.contains(':') {
            return Err(bad(403, "bad-path", "路径不得包含冒号"));
        }
        if seg.trim_end_matches([' ', '.']) != seg {
            return Err(bad(403, "bad-path", "路径段不得以空格或点结尾"));
        }
        let head = seg.split('.').next().unwrap_or("");
        if RESERVED.iter().any(|n| head.eq_ignore_ascii_case(n)) {
            return Err(bad(403, "bad-path", "路径段是 Windows 保留名"));
        }
        if seg.starts_with(TMP_PREFIX) {
            return Err(bad(
                403,
                "bad-path",
                format!("路径段以 {TMP_PREFIX} 开头：那是服务原子写的中间态，不可寻址"),
            ));
        }
        parts.push(seg.to_string());
    }
    if parts.is_empty() {
        return Err(bad(400, "bad-path", "路径为空"));
    }
    Ok(parts)
}

/// 相对路径 → 数据夹内绝对路径。自第一个重解析点起，每段都做实路径越界检查。
pub fn resolve(app: &App, rel: &str) -> Result<(Vec<String>, PathBuf), Bad> {
    let parts = norm_rel(rel)?;
    let Some((root, real_root)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let mut target = root.clone();
    for seg in &parts {
        target.push(seg);
    }
    if path_chars(&target) > MAX_PATH {
        return Err(bad(
            400,
            "bad-path",
            format!("路径过长（绝对路径超 {MAX_PATH} 字符，Windows 建不出来）"),
        ));
    }
    let mut cur = root;
    let mut saw_reparse = false;
    for seg in &parts {
        cur.push(seg);
        if !saw_reparse {
            if is_reparse(&cur) {
                saw_reparse = true;
            } else {
                continue;
            }
        }
        if lexists(&cur) && !within_real(&real_root, &cur) {
            return Err(bad(403, "escape", "路径越出数据文件夹"));
        }
    }
    Ok((parts, target))
}

pub fn guard_secret_read(root: &Path, target: &Path) -> Result<(), Bad> {
    let sec = secret_dir(root);
    if target == &sec || target.starts_with(&sec) || under_secret(root, target) {
        return Err(bad(403, "protected", "机密目录不经 /api/fs 读取（密文不可当文本读走）"));
    }
    Ok(())
}

// ---- 整盘读取（助理只读数据夹外的一个文件） ----

const DISK_MAX_RAW: usize = 4096;
const DISK_MAX_TARGET: usize = 32767;

/// 整盘读取的目标解析。允许绝对路径，也允许相对数据夹、可以走出数据夹的路径；
/// 设备路径、机密目录与 Windows 危险写法一律拒绝。
/// 口径照 `app/server/sf_server.py` 的 `disk_read_target`。
pub fn disk_read_target(app: &App, raw: &str) -> Result<PathBuf, Bad> {
    if py_strip(raw).is_empty() {
        return Err(bad(400, "bad-path", "路径为空"));
    }
    if raw.chars().count() > DISK_MAX_RAW {
        return Err(bad(400, "bad-path", "路径过长"));
    }
    if raw.chars().any(|c| (c as u32) < 32) {
        return Err(bad(400, "bad-path", "路径含控制字符"));
    }
    if raw != py_strip(raw) {
        return Err(bad(403, "bad-path", "路径首尾不得有空白"));
    }
    if raw.starts_with("\\\\?\\")
        || raw.starts_with("\\\\.\\")
        || raw.starts_with("//?/")
        || raw.starts_with("//./")
    {
        return Err(bad(403, "bad-path", "不接受设备路径"));
    }
    if secret_in_path(raw) {
        return Err(bad(403, "protected", "机密目录读不到"));
    }
    let Some((root, _)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };

    let slash = raw.replace('\\', "/");
    let unc = slash.starts_with("//");
    let drive = is_drive(&slash);
    let rooted = slash.starts_with('/') && !unc;
    if drive && !(slash.chars().count() == 2 || slash.chars().nth(2) == Some('/')) {
        return Err(bad(403, "bad-path", "路径不能读"));
    }
    if unc {
        let segs: Vec<&str> = slash.split('/').collect();
        let ok = segs.len() >= 4
            && !segs[2].is_empty()
            && !segs[3].is_empty()
            && segs[2] != "."
            && segs[2] != ".."
            && segs[3] != "."
            && segs[3] != "..";
        if !ok {
            return Err(bad(403, "bad-path", "路径不能读"));
        }
    }
    let parts_src: String = if drive {
        let rest = &slash[2..];
        rest.strip_prefix('/').unwrap_or(rest).to_string()
    } else if unc {
        slash.split('/').skip(4).collect::<Vec<_>>().join("/")
    } else if rooted {
        slash[1..].to_string()
    } else {
        slash.clone()
    };
    for seg in parts_src.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            continue;
        }
        if seg.contains(':') || seg.trim_end_matches([' ', '.']) != seg || reserved_seg(seg) {
            return Err(bad(403, "bad-path", "路径不能读"));
        }
    }

    let native = slash.replace('/', "\\");
    let target = if unc || drive {
        PathBuf::from(native)
    } else if rooted {
        PathBuf::from(normpath_win(&format!("{}{}", current_drive(), native)))
    } else {
        PathBuf::from(normpath_win(&format!("{}\\{}", root.to_string_lossy(), native)))
    };
    if under_secret(&root, &abs_norm(&target)) {
        return Err(bad(403, "protected", "机密目录读不到"));
    }
    if target.to_string_lossy().chars().count() > DISK_MAX_TARGET {
        return Err(bad(400, "bad-path", "路径过长"));
    }
    Ok(target)
}

/// 路径文字里是否点到机密目录：先看原文相邻的 `_工坊/机密`，再折叠 `.`/`..` 看落点。
fn secret_in_path(raw: &str) -> bool {
    let slash = raw.replace('\\', "/");
    const NEEDLE: &str = "_工坊/机密";
    let mut from = 0usize;
    while let Some(rel) = slash[from..].find(NEEDLE) {
        let at = from + rel;
        let end = at + NEEDLE.len();
        let before_ok = at == 0 || slash.as_bytes()[at - 1] == b'/';
        let after_ok = end == slash.len() || slash.as_bytes()[end] == b'/';
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    let mut stack: Vec<&str> = Vec::new();
    for seg in slash.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if stack.pop().is_none() {
                stack.push(seg);
            }
            continue;
        }
        stack.push(seg);
    }
    stack
        .windows(2)
        .any(|w| w[0].to_lowercase() == "_工坊" && w[1] == "机密")
}

fn reserved_seg(seg: &str) -> bool {
    let head = seg.split('.').next().unwrap_or("");
    RESERVED.iter().any(|n| head.eq_ignore_ascii_case(n))
}

/// Python `os.path.abspath` 的 Windows 口径：补成绝对路径并词法归一（不碰文件系统）。
fn abs_norm(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace('/', "\\");
    if has_drive_prefix(&s) {
        PathBuf::from(normpath_win(&s))
    } else if s.starts_with('\\') {
        PathBuf::from(normpath_win(&format!("{}{}", current_drive(), s)))
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        PathBuf::from(normpath_win(&format!(
            "{}\\{}",
            cwd.to_string_lossy().trim_end_matches('\\'),
            s
        )))
    }
}

/// 当前盘符（`D:`）。取不到就用兜底值。
fn current_drive() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|d| d.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()))
        .filter(|p| p.len() >= 2 && p.as_bytes()[1] == b':')
        .unwrap_or_else(|| "C:".to_string())
}

fn has_drive_prefix(s: &str) -> bool {
    s.starts_with("\\\\") || is_drive(s)
}

/// Windows 词法归一（Python `os.path.normpath` 口径）：折叠 `.`/`..` 与重复分隔符，
/// 不碰文件系统。盘符以上的 `..` 丢弃；无盘符时保留前导 `..`。
fn normpath_win(s: &str) -> String {
    let (prefix, rest) = split_drive_win(s);
    let initial_slashes = !prefix.is_empty() && rest.starts_with('\\');
    let mut comps: Vec<&str> = Vec::new();
    for seg in rest.split('\\') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            match comps.last() {
                Some(last) if *last != ".." => {
                    comps.pop();
                }
                _ => {
                    if !initial_slashes {
                        comps.push(seg);
                    }
                }
            }
            continue;
        }
        comps.push(seg);
    }
    let body = comps.join("\\");
    if prefix.is_empty() {
        if rest.starts_with('\\') {
            format!("\\{body}")
        } else if body.is_empty() {
            ".".to_string()
        } else {
            body
        }
    } else if rest.starts_with('\\') {
        format!("{prefix}\\{body}")
    } else if body.is_empty() {
        prefix
    } else {
        format!("{prefix}{body}")
    }
}

/// 拆盘符前缀：UNC（`\\server\share`）或盘符（`C:`），没有就空。
fn split_drive_win(s: &str) -> (String, String) {
    if s.starts_with("\\\\") {
        let rest = &s[2..];
        let (server, after) = match rest.split_once('\\') {
            Some((a, b)) => (a, b),
            None => (rest, ""),
        };
        let (share, tail) = match after.split_once('\\') {
            Some((a, b)) => (a, b),
            None => (after, ""),
        };
        if server.is_empty() || share.is_empty() {
            return (String::new(), s.to_string());
        }
        let prefix = format!("\\\\{server}\\{share}");
        let rest = if tail.is_empty() {
            String::new()
        } else {
            format!("\\{tail}")
        };
        return (prefix, rest);
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return (s[..2].to_ascii_uppercase(), s[2..].to_string());
    }
    (String::new(), s.to_string())
}

/// `GET /api/fs/list` 把这些写法当成数据夹根。先于 `norm_rel`，`.` 在别处是空路径。
pub fn is_list_root(rel: &str) -> bool {
    matches!(py_strip(rel), "" | "." | "./" | ".\\" | "/")
}

/// 快照遇到重解析点（或祖先已经是）时：越出数据夹、落进机密、或判不清，都跳过该入口。
pub fn reparse_blocked(root: &Path, real_root: &Path, target: &Path) -> bool {
    if !within_real(real_root, target) {
        return true;
    }
    under_secret(root, target)
}

/// `_工坊/机密/<name>.dat`。名字限中英文/数字/下划线/连字符 1~32 字。
/// 逐级做与 `resolve` 同口径的逃逸校验：机密目录被换成指向夹外的重解析点时拒绝。
pub fn secret_path(root: &Path, real_root: &Path, name: &str) -> Result<PathBuf, Bad> {
    if !secret_name_ok(name) {
        return Err(bad(400, "bad-name", "机密名不合法（只允许中英文/数字/下划线/连字符，1~32 字）"));
    }
    let file = format!("{name}{SECRET_EXT}");
    let target = root.join(WS_NAME).join(SECRET_NAME).join(&file);
    let mut cur = root.to_path_buf();
    for seg in [WS_NAME, SECRET_NAME, file.as_str()] {
        cur.push(seg);
        if lexists(&cur) && !within_real(real_root, &cur) {
            return Err(bad(403, "escape", "路径越出数据文件夹"));
        }
    }
    Ok(target)
}

fn secret_name_ok(name: &str) -> bool {
    let n = name.chars().count();
    (1..=32).contains(&n)
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || ('\u{4e00}'..='\u{9fa5}').contains(&c)
        })
}

fn secret_dir(root: &Path) -> PathBuf {
    root.join("_工坊").join("机密")
}

fn under_secret(root: &Path, target: &Path) -> bool {
    if lexical_under_secret(root, target) {
        return true;
    }
    let ws = root.join("_工坊");
    let sec = secret_dir(root);
    let need_real = is_reparse(&ws) || is_reparse(&sec) || path_chain_has_reparse(root, target);
    if !need_real {
        return false;
    }
    if lexists(target) || lexists(&sec) {
        return real_under_secret(&sec, target);
    }
    false
}

fn lexical_under_secret(root: &Path, target: &Path) -> bool {
    let sec = normcase(&abs_lexical(&secret_dir(root)));
    let lexical = normcase(&abs_lexical(target));
    lexical == sec || lexical.starts_with(&(sec + "\\"))
}

/// 实路径落在机密里，或判不清。判不清就当在里面。
fn real_under_secret(sec: &Path, target: &Path) -> bool {
    let (Some(rs), Some(rt)) = (try_realpath(sec), try_realpath(target)) else {
        return true;
    };
    is_within(&rs, &rt) || normcase(&rs) == normcase(&rt)
}

fn path_chain_has_reparse(root: &Path, target: &Path) -> bool {
    let Ok(root_p) = std::path::absolute(root) else {
        return true;
    };
    let Ok(mut cur) = std::path::absolute(target) else {
        return true;
    };
    let root_key = normcase(&root_p);
    loop {
        if is_reparse(&cur) {
            return true;
        }
        if normcase(&cur) == root_key {
            return false;
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => return false,
        }
    }
}

fn within_real(real_root: &Path, target: &Path) -> bool {
    match try_realpath(target) {
        Some(real_target) => is_within(real_root, &real_target),
        None => false,
    }
}

fn is_within(root: &Path, target: &Path) -> bool {
    let a = split_path(&normcase(root));
    let b = split_path(&normcase(target));
    if b.len() < a.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

pub fn is_reparse(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            use std::os::windows::fs::MetadataExt;
            meta.file_type().is_symlink() || (meta.file_attributes() & REPARSE) != 0
        }
        Err(_) => false,
    }
}

fn lexists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// 逐段解析重解析点。最终组件可以不存在：沿已存在的前缀解析，再接上剩余名字。
pub fn try_realpath(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let mut resolved = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => resolved.push(p.as_os_str()),
            Component::RootDir => resolved.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(seg) => {
                resolved.push(seg);
                if is_reparse(&resolved) {
                    let canon = std::fs::canonicalize(&resolved).ok()?;
                    resolved = strip_verbatim(canon);
                }
            }
        }
    }
    Some(resolved)
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn abs_lexical(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn normcase(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn split_path(s: &str) -> Vec<String> {
    s.split('\\').filter(|p| !p.is_empty()).map(|p| p.to_string()).collect()
}

fn path_chars(path: &Path) -> usize {
    path.to_string_lossy().chars().count()
}

fn is_drive(rel: &str) -> bool {
    let b = rel.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Python `str.strip()`：Unicode 空白，不含只去空格。
fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

fn py_isspace(c: char) -> bool {
    c.is_whitespace()
}

/// 把用户给的绝对路径收成 Python `str(Path(...))` 那种反斜杠形式，不折叠 `..`。
pub fn slash_path(raw: &str) -> PathBuf {
    PathBuf::from(raw.replace('/', "\\"))
}

pub fn check_data_root(raw: &str) -> Result<PathBuf, Bad> {
    check_data_root_created(raw).map(|(path, _)| path)
}

pub fn check_data_root_created(raw: &str) -> Result<(PathBuf, bool), Bad> {
    let trimmed = raw.trim();
    let unquoted = trimmed.trim_matches(|c| c == '"' || c == '\'');
    let expanded = expand_vars(unquoted);
    if expanded.trim().is_empty() {
        return Err(bad(400, "bad-root", "路径为空"));
    }
    if !is_abs_win(&expanded) {
        return Err(bad(400, "bad-root", r"请填绝对路径，例如 D:\故事工坊数据"));
    }
    if is_drive_root(&expanded) {
        return Err(bad(400, "bad-root", "请不要把整个盘符当数据文件夹，另建一个文件夹"));
    }
    let path = slash_path(&expanded);
    let created = !path.exists();
    if path.exists() && !path.is_dir() {
        return Err(bad(400, "bad-root", "该路径已存在但不是文件夹"));
    }
    std::fs::create_dir_all(&path).map_err(|e| bad(400, "bad-root", format!("无法创建或写入该文件夹：{e}")))?;
    std::fs::create_dir_all(path.join("_工坊"))
        .map_err(|e| bad(400, "bad-root", format!("无法创建或写入该文件夹：{e}")))?;
    Ok((path, created))
}

fn is_abs_win(p: &str) -> bool {
    if p.starts_with("\\\\") || p.starts_with("//") {
        return true;
    }
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn is_drive_root(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return p[2..].chars().all(|c| c == '\\' || c == '/');
    }
    false
}

fn expand_vars(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(end) = b[i + 1..].iter().position(|c| *c == b'%') {
                let name = &raw[i + 1..i + 1 + end];
                if !name.is_empty() {
                    if let Ok(val) = std::env::var(name) {
                        out.push_str(&val);
                        i += end + 2;
                        continue;
                    }
                }
            }
        }
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_shapes_match_python_classes() {
        let cases = [
            ("../外面.txt", 403),
            ("..\\外面.txt", 403),
            ("a/../../外面.txt", 403),
            ("C:/Windows/x.txt", 403),
            ("C:\\Windows\\x.txt", 403),
            ("/etc/passwd", 403),
            ("_工坊/a:b.txt", 403),
            ("_工坊/CON", 403),
            ("_工坊/尾点.", 403),
            ("_工坊/尾空格 ", 403),
            ("_工坊/\u{0000}x", 400),
            ("_工坊/.sf-tmp-1", 403),
            ("", 400),
            (".", 400),
        ];
        for (rel, status) in cases {
            let err = norm_rel(rel).unwrap_err();
            assert_eq!(err.status, status, "{rel}");
            assert_eq!(err.error, "bad-path", "{rel}");
        }
    }

    #[test]
    fn list_root_aliases_match_python() {
        assert!(is_list_root(""));
        assert!(is_list_root("."));
        assert!(is_list_root("./"));
        assert!(is_list_root(".\\"));
        assert!(is_list_root("/"));
        assert!(is_list_root("  .  "));
        assert!(!is_list_root("书"));
        assert!(!is_list_root("./书"));
    }

    #[test]
    fn within_is_component_not_prefix() {
        let root = Path::new(r"D:\data");
        assert!(is_within(root, Path::new(r"D:\data\a")));
        assert!(is_within(root, root));
        assert!(!is_within(root, Path::new(r"D:\data2\a")));
        assert!(!is_within(root, Path::new(r"E:\data\a")));
    }

    fn disk_app() -> App {
        let root = std::env::temp_dir().join("sf-disk-gate-test");
        let _ = std::fs::create_dir_all(root.join("_工坊").join("机密"));
        App {
            token: "t".into(),
            port: 1,
            exe_dir: root.clone(),
            data: std::sync::Mutex::new(Some(crate::state::DataRoot { path: root.clone(), real: root.clone() })),
            lock: std::sync::Mutex::new(None),
            cfg: std::sync::Mutex::new(Value::Null),
            write_lock: std::sync::Mutex::new(()),
            app_override: None,
            shell: false,
            origins: std::collections::HashSet::new(),
            hosts: std::collections::HashSet::new(),
        }
    }

    #[test]
    fn disk_gate_rejects_dangerous_shapes() {
        let app = disk_app();
        let long = "x".repeat(5000);
        let cases: Vec<(String, u16, &str)> = vec![
            ("".into(), 400, "bad-path"),
            ("   ".into(), 400, "bad-path"),
            (" x ".into(), 403, "bad-path"),
            ("a\u{0}b".into(), 400, "bad-path"),
            ("a\nb".into(), 400, "bad-path"),
            (long, 400, "bad-path"),
            ("C:foo".into(), 403, "bad-path"),
            ("C:/CON".into(), 403, "bad-path"),
            ("C:/x/尾点.".into(), 403, "bad-path"),
            ("C:/x/尾空格 ".into(), 403, "bad-path"),
            ("C:/x/a:b".into(), 403, "bad-path"),
            ("//server".into(), 403, "bad-path"),
            ("//server/.".into(), 403, "bad-path"),
            ("//server//share".into(), 403, "bad-path"),
            (r"\\?\C:\x".into(), 403, "bad-path"),
            ("//?/C:/x".into(), 403, "bad-path"),
            (r"\\.\C:\x".into(), 403, "bad-path"),
            ("_工坊/机密/probe.txt".into(), 403, "protected"),
            ("C:/data/_工坊/机密/x".into(), 403, "protected"),
            ("../_工坊/机密/x".into(), 403, "protected"),
            ("_工坊/公开/../机密/x".into(), 403, "protected"),
        ];
        for (raw, status, error) in cases {
            let err = disk_read_target(&app, &raw).unwrap_err();
            assert_eq!(err.status, status, "{raw}");
            assert_eq!(err.error, error, "{raw}");
        }
    }

    #[test]
    fn disk_gate_allows_outside_reads() {
        let app = disk_app();
        for raw in [
            r"C:\Windows\win.ini",
            "../outside.txt",
            "notes/../../outside.txt",
            "memo.txt",
            "/foo/bar",
            r"\\server\share\a\b",
        ] {
            assert!(disk_read_target(&app, raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn normpath_matches_python() {
        assert_eq!(normpath_win(r"D:\data\notes\..\..\outside.txt"), r"D:\outside.txt");
        assert_eq!(normpath_win(r"D:\a\..\..\b"), r"D:\b");
        assert_eq!(normpath_win(r"a\b"), r"a\b");
        assert_eq!(normpath_win(r"a\..\..\b"), r"..\b");
        assert_eq!(normpath_win(r"\a\..\b"), r"\b");
        assert_eq!(normpath_win(r"C:"), "C:");
        assert_eq!(normpath_win(r"\\server\share\a\..\b"), r"\\server\share\b");
    }

    #[test]
    fn secret_in_path_matches_python() {
        for raw in [
            "_工坊/机密/x",
            "_工坊/公开/../机密/x",
            "../_工坊/机密/x",
            "C:/d/_工坊/机密/x",
            r"_工坊\机密\x",
            "_工坊/机密",
            "x/_工坊/机密/",
        ] {
            assert!(secret_in_path(raw), "{raw}");
        }
        for raw in ["_工坊/公开/x", "机密/x", "a/机密/b"] {
            assert!(!secret_in_path(raw), "{raw}");
        }
    }
}
