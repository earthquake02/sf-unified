//! 列举、单路径状态、整夹快照。只读，不写盘。
//! 口径对照 `app/server/sf_server.py` 的 `_api_list` / `_api_stat` / `_api_snapshot`。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Map, Value};

use sha2::{Digest, Sha256};

use crate::gate::{self, bad, Bad};
use crate::state::{App, MAX_READ};

const TMP_PREFIX: &str = ".sf-tmp-";
const SNAPSHOT_MAX_PREREAD_FILES: usize = 8000;
const SNAPSHOT_MAX_PREREAD_BYTES: u64 = 48 * 1024 * 1024;
const BOOK_SUBS: &[&str] = &["正文", "设定", "伏笔", "片段", "会话", "对话", "图"];
const WS_TOP: &[&str] = &[
    "提示词.json",
    "文风.json",
    "用量.json",
    "大纲预设.json",
    "settings.json",
    "书库.json",
];

struct Budget {
    files: usize,
    bytes: u64,
}

struct Node {
    name: String,
    kind: &'static str,
    bytes: u64,
    mtime: f64,
    abs: PathBuf,
    saw: bool,
}

pub fn list(app: &App, rel: &str) -> Result<Value, Bad> {
    let Some((root, _)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let (shown, target) = if gate::is_list_root(rel) {
        (String::new(), root.clone())
    } else {
        let (parts, target) = gate::resolve(app, rel)?;
        gate::guard_secret_read(&root, &target)?;
        (parts.join("/"), target)
    };
    match std::fs::metadata(&target) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(bad(400, "not-dir", "目标是文件，不是文件夹")),
        Err(_) => return Err(bad(404, "not-found", "文件夹不存在")),
    }
    let rd = match std::fs::read_dir(&target) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(bad(409, "busy", format!("目标被占用或只读，系统拒绝访问：{e}")));
        }
        Err(e) => return Err(bad(500, "internal", format!("OSError: {e}"))),
    };
    let mut items = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(ent.path()) else { continue };
        let is_dir = meta.is_dir();
        items.push(Node {
            name,
            kind: if is_dir { "dir" } else { "file" },
            bytes: if is_dir { 0 } else { meta.len() },
            mtime: mtime_of(&meta),
            abs: ent.path(),
            saw: false,
        });
    }
    items.sort_by(|a, b| (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.cmp(&b.name)));
    Ok(json!({
        "ok": true,
        "path": shown,
        "entries": items.iter().map(entry_json).collect::<Vec<_>>(),
    }))
}

pub fn stat(app: &App, rel: &str) -> Result<Value, Bad> {
    let (parts, target) = gate::resolve(app, rel)?;
    let Some((root, _)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    gate::guard_secret_read(&root, &target)?;
    let meta = match std::fs::metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(bad(404, "not-found", "目标不存在"));
        }
        Err(e) => return Err(bad(500, "stat-failed", format!("看元数据失败：{e}"))),
    };
    let is_dir = meta.is_dir();
    let mut out = json!({
        "ok": true,
        "path": parts.join("/"),
        "type": if is_dir { "dir" } else { "file" },
        "bytes": if is_dir { 0 } else { meta.len() },
        "mtime": mtime_of(&meta),
    });
    if !is_dir {
        if meta.len() > MAX_READ {
            return Err(bad(
                413,
                "too-large",
                format!("文件超过单次读上限（{MAX_READ} 字节），无法算 hash"),
            ));
        }
        let data = std::fs::read(&target).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => bad(404, "not-found", "目标不存在"),
            std::io::ErrorKind::PermissionDenied => {
                bad(409, "busy", format!("目标被占用或只读，系统拒绝访问：{e}"))
            }
            _ => bad(500, "internal", format!("OSError: {e}")),
        })?;
        out["hash"] = Value::String(sha256_hex(&data));
    }
    Ok(out)
}

pub fn snapshot(app: &App) -> Result<Value, Bad> {
    let Some((root, real_root)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let mut dirs = Map::new();
    let mut files = Map::new();
    let mut budget = Budget { files: 0, bytes: 0 };
    let top = list_dir(&root, &real_root, &root, false, &mut dirs);
    let mut book_names = HashSet::new();
    for e in &top {
        if e.kind == "dir" && e.abs.join("项目.json").is_file() {
            book_names.insert(e.name.clone());
        }
    }
    for e in top {
        if e.kind != "dir" {
            continue;
        }
        if e.name == "_工坊" {
            let all = list_dir(&root, &real_root, &e.abs, e.saw, &mut dirs);
            let kept: Vec<Node> = all
                .into_iter()
                .filter(|n| n.name != "机密" && n.name != "回收站")
                .collect();
            if let Some(key) = rel_key(&root, &e.abs) {
                dirs.insert(key, Value::Array(kept.iter().map(entry_json).collect()));
            }
            for we in kept {
                if we.kind == "file" {
                    if WS_TOP.contains(&we.name.as_str()) {
                        try_read(&root, &we, &mut files, &mut budget);
                    }
                } else if we.name == "共享设定" {
                    walk(&root, &real_root, &we.abs, we.saw, &mut dirs, &mut files, &mut budget);
                }
            }
            continue;
        }
        if !book_names.contains(&e.name) {
            continue;
        }
        let book_items = list_dir(&root, &real_root, &e.abs, e.saw, &mut dirs);
        for be in book_items {
            if be.kind == "file" {
                if be.name == "项目.json" || be.name == "标签.json" {
                    try_read(&root, &be, &mut files, &mut budget);
                }
            } else if BOOK_SUBS.contains(&be.name.as_str()) {
                walk(&root, &real_root, &be.abs, be.saw, &mut dirs, &mut files, &mut budget);
            }
        }
    }
    Ok(json!({ "ok": true, "dirs": dirs, "files": files }))
}

/// 子目录后进先出，与 Python 快照的栈一致，预读额度才会切在同一批文件上。
fn walk(
    root: &Path,
    real_root: &Path,
    start: &Path,
    saw: bool,
    dirs: &mut Map<String, Value>,
    files: &mut Map<String, Value>,
    budget: &mut Budget,
) {
    let mut stack = vec![(start.to_path_buf(), saw)];
    while let Some((cur, cur_saw)) = stack.pop() {
        let kids = list_dir(root, real_root, &cur, cur_saw, dirs);
        let mut later = Vec::new();
        for ke in kids {
            if ke.kind == "file" {
                try_read(root, &ke, files, budget);
            } else {
                later.push((ke.abs, ke.saw));
            }
        }
        for item in later {
            stack.push(item);
        }
    }
}

fn list_dir(
    root: &Path,
    real_root: &Path,
    abs_path: &Path,
    parent_saw: bool,
    dirs: &mut Map<String, Value>,
) -> Vec<Node> {
    let Some(key) = rel_key(root, abs_path) else {
        return Vec::new();
    };
    let entries = match std::fs::read_dir(abs_path) {
        Ok(rd) => {
            let mut v = Vec::new();
            for ent in rd {
                match ent {
                    Ok(ent) => v.push(ent),
                    Err(_) => {
                        dirs.insert(key, json!([]));
                        return Vec::new();
                    }
                }
            }
            v
        }
        Err(_) => {
            dirs.insert(key, json!([]));
            return Vec::new();
        }
    };
    let mut items = Vec::new();
    for ent in entries {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        let path = ent.path();
        let Some(saw) = accept(root, real_root, &path, parent_saw) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let is_dir = meta.is_dir();
        items.push(Node {
            name,
            kind: if is_dir { "dir" } else { "file" },
            bytes: if is_dir { 0 } else { meta.len() },
            mtime: mtime_of(&meta),
            abs: path,
            saw,
        });
    }
    items.sort_by(|a, b| (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.cmp(&b.name)));
    dirs.insert(key, Value::Array(items.iter().map(entry_json).collect()));
    items
}

fn accept(root: &Path, real_root: &Path, path: &Path, parent_saw: bool) -> Option<bool> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    let mut saw = parent_saw;
    let is_rp = gate::is_reparse(path);
    if is_rp {
        saw = true;
    }
    if (saw || is_rp) && gate::reparse_blocked(root, real_root, path) {
        return None;
    }
    Some(saw)
}

fn try_read(root: &Path, node: &Node, files: &mut Map<String, Value>, budget: &mut Budget) {
    let Some(key) = rel_key(root, &node.abs) else { return };
    if files.contains_key(&key) || node.name.starts_with(TMP_PREFIX) {
        return;
    }
    let low = node.name.to_lowercase();
    if !low.ends_with(".json") && !low.ends_with(".html") && !low.ends_with(".md") && !low.ends_with(".txt") {
        return;
    }
    if node.bytes > MAX_READ || !budget_allows(budget.files, budget.bytes, node.bytes) {
        return;
    }
    if gate::guard_secret_read(root, &node.abs).is_err() {
        return;
    }
    let Ok(data) = std::fs::read(&node.abs) else { return };
    let Ok(text) = decode_utf8_sig(&data) else { return };
    files.insert(
        key,
        json!({
            "text": text,
            "bytes": data.len(),
            "hash": sha256_hex(&data),
            "mtime": node.mtime,
        }),
    );
    budget.files += 1;
    budget.bytes += data.len() as u64;
}

fn budget_allows(files: usize, bytes: u64, size: u64) -> bool {
    files < SNAPSHOT_MAX_PREREAD_FILES && bytes.saturating_add(size) <= SNAPSHOT_MAX_PREREAD_BYTES
}

fn entry_json(n: &Node) -> Value {
    json!({
        "name": n.name,
        "type": n.kind,
        "bytes": n.bytes,
        "mtime": n.mtime,
    })
}

fn rel_key(root: &Path, path: &Path) -> Option<String> {
    if path == root {
        return Some(String::new());
    }
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn mtime_of(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let dig = Sha256::digest(data);
    let mut s = String::with_capacity(dig.len() * 2);
    for b in dig {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn decode_utf8_sig(data: &[u8]) -> Result<String, ()> {
    let body = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    String::from_utf8(body.to_vec()).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preread_budget_matches_python() {
        let max = SNAPSHOT_MAX_PREREAD_BYTES;
        assert!(budget_allows(0, 0, max));
        assert!(!budget_allows(0, 0, max + 1));
        assert!(budget_allows(7999, 0, 1));
        assert!(!budget_allows(8000, 0, 1));
        assert!(!budget_allows(0, max - 10, 11));
        assert!(budget_allows(0, max - 10, 10));
        assert!(!budget_allows(0, 0, 49 * 1024 * 1024));
    }
}
