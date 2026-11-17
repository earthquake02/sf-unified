//! 派生启动缓存。不是数据真源：校验失败、元数据不一致或落盘失败都只回退，不改用户文稿。
//! 缓存哈希只核对缓存里的文本，不能代替当次磁盘 SHA-256。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::gate::{self, bad, Bad};
use crate::state::{App, MAX_READ, VER};
use crate::{fsops, scan, util};

const FORMAT: u64 = 1;
const MAX_CACHE: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 8000;
const MAX_DIR_KEYS: usize = 16_000;
const MAX_CHILDREN: usize = 32_000;
const QUIET: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(5);
const BOOK_SUBS: &[&str] = &["正文", "设定", "伏笔", "片段", "会话", "对话", "图"];
const WS_FILES: &[&str] = &["提示词.json", "文风.json", "用量.json", "大纲预设.json", "书库.json"];

struct Shared {
    mu: Mutex<Inner>,
    cv: Condvar,
}

struct Inner {
    root_id: Option<String>,
    target: Option<PathBuf>,
    dirs: Map<String, Value>,
    files: Map<String, Value>,
    warm: bool,
    generation: u64,
    dirty: bool,
    dirty_since: Option<Instant>,
    last_edit: Option<Instant>,
    quiet: Duration,
    max_wait: Duration,
    stop: bool,
}

pub struct Cache {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                mu: Mutex::new(Inner {
                    root_id: None,
                    target: None,
                    dirs: Map::new(),
                    files: Map::new(),
                    warm: false,
                    generation: 0,
                    dirty: false,
                    dirty_since: None,
                    last_edit: None,
                    quiet: QUIET,
                    max_wait: MAX_WAIT,
                    stop: false,
                }),
                cv: Condvar::new(),
            }),
            worker: Mutex::new(None),
        }
    }

    fn poke(&self) {
        self.ensure_worker();
        self.shared.cv.notify_one();
    }

    fn ensure_worker(&self) {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        match std::thread::Builder::new().name("sf-startup-cache".into()).spawn(move || worker_loop(shared)) {
            Ok(handle) => *slot = Some(handle),
            Err(_) => log_fail("thread"),
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        {
            let mut inner = self.shared.mu.lock().unwrap_or_else(|e| e.into_inner());
            inner.stop = true;
        }
        self.shared.cv.notify_all();
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

struct View {
    dirs: Map<String, Value>,
    files: Map<String, Value>,
    generation: u64,
}

/// 读缓存。命中前会重枚举目录元数据；对不上就 404，不返回可能旧的正文。
pub fn load(app: &App) -> Result<Value, Bad> {
    let Some((root, real)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let id = root_id(&real);
    if let Some(view) = memory_view(app, &id) {
        let generation = view.generation;
        return match confirm(&root, &real, &view) {
            Ok(()) => Ok(hit_body(&view)),
            Err(e) => {
                discard_if(app, generation);
                Err(e)
            }
        };
    }
    match read_disk(&root, &real, &id) {
        Ok(view) => {
            confirm(&root, &real, &view)?;
            install_clean_if_cold(app, &root, &id, &view);
            Ok(hit_body(&view))
        }
        Err(e) => Err(e),
    }
}

/// 用刚成功的完整快照替换内存缓存，并安排关键路径之外的落盘。不改快照本身。
pub fn remember_snapshot(app: &App, snapshot: &Value) {
    let Some((root, real)) = app.data_snapshot() else { return };
    let Some(dirs) = snapshot.get("dirs").and_then(|v| v.as_object()) else { return };
    let Some(files) = snapshot.get("files").and_then(|v| v.as_object()) else { return };
    let dirs = strip_dirs(dirs.clone());
    let files = strip_files(files.clone());
    {
        let mut inner = lock(app);
        inner.root_id = Some(root_id(&real));
        inner.target = Some(cache_file(&root));
        inner.dirs = dirs;
        inner.files = files;
        inner.warm = true;
        mark_dirty(&mut inner);
    }
    app.startup_cache.poke();
}

pub fn after_write(app: &App, rel: &str, text: &str) {
    mutate(app, false, |inner, root| apply_write(inner, root, rel, text));
}

pub fn after_mkdir(app: &App, rel: &str) {
    mutate(app, true, |inner, root| apply_mkdir(inner, root, rel));
}

pub fn after_delete(app: &App, rel: &str) {
    mutate(app, false, |inner, root| {
        if forbidden_rel(rel) {
            return None;
        }
        remove_tree(&mut inner.dirs, &mut inner.files, rel);
        refresh_ancestors(&mut inner.dirs, root, rel);
        demote_book_if_needed(&mut inner.dirs, &mut inner.files, root, rel);
        Some(true)
    });
}

/// 换数据根或无法增量表示时丢掉内存态。不删除盘上的缓存文件。
pub fn invalidate(app: &App) {
    let mut inner = lock(app);
    clear_warm(&mut inner);
}

/// 尽力把内存缓存原子写入磁盘。失败只记一行不含正文的日志。
pub fn flush_best_effort(app: &App) {
    let _ = write_once(&app.startup_cache.shared);
}

/// 退出路径：先落缓存，再释放数据锁。
pub(crate) fn release_data(app: &App, root: &Path) {
    flush_best_effort(app);
    crate::lock::release(app, root);
}

fn mutate(app: &App, structural: bool, op: impl FnOnce(&mut Inner, &Path) -> Option<bool>) {
    let Some((root, real)) = app.data_snapshot() else { return };
    let id = root_id(&real);
    let mut dirty = false;
    {
        let mut inner = lock(app);
        if !inner.warm || inner.root_id.as_deref() != Some(id.as_str()) {
            return;
        }
        let saved = if structural {
            Some((inner.dirs.clone(), inner.files.clone()))
        } else {
            None
        };
        match op(&mut inner, &root) {
            Some(true) => {
                mark_dirty(&mut inner);
                dirty = true;
            }
            Some(false) => {
                if let Some((dirs, files)) = saved {
                    inner.dirs = dirs;
                    inner.files = files;
                }
                clear_warm(&mut inner);
            }
            None => {}
        }
    }
    if dirty {
        app.startup_cache.poke();
    }
}

fn apply_write(inner: &mut Inner, root: &Path, rel: &str, text: &str) -> Option<bool> {
    if forbidden_rel(rel) || rel.is_empty() {
        return None;
    }
    let (parent, name) = split_parent(rel);
    if parent_ready(&inner.dirs, parent) && !needs_new_dir(inner, root, rel) {
        return Some(update_file(inner, root, rel, parent, name, text));
    }
    if !ensure_chain(inner, root, parent, Some(rel)) {
        return Some(false);
    }
    if !parent_ready(&inner.dirs, parent) {
        // 快照根本不会收录这条路径，只把已有目录清单补到能对上的那一层。
        return Some(touch_visible_ancestors(inner, root, rel));
    }
    Some(update_file(inner, root, rel, parent, name, text))
}

fn apply_mkdir(inner: &mut Inner, root: &Path, rel: &str) -> Option<bool> {
    if forbidden_rel(rel) || rel.is_empty() {
        return None;
    }
    if !ensure_chain(inner, root, rel, None) {
        return Some(false);
    }
    refresh_ancestors(&mut inner.dirs, root, rel);
    Some(true)
}

fn needs_new_dir(inner: &Inner, root: &Path, file_rel: &str) -> bool {
    let parent = split_parent(file_rel).0;
    if parent.is_empty() {
        return false;
    }
    let mut acc = String::new();
    for seg in parent.split('/') {
        let parent_rel = acc.clone();
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        let abs = abs_of(root, &acc);
        if dir_gets_key(&parent_rel, seg, &abs) && !inner.dirs.contains_key(&acc) {
            return true;
        }
    }
    false
}

fn parent_ready(dirs: &Map<String, Value>, parent: &str) -> bool {
    dirs.contains_key(parent)
}

fn update_file(inner: &mut Inner, root: &Path, rel: &str, parent: &str, name: &str, text: &str) -> bool {
    let abs = abs_of(root, rel);
    let Ok(meta) = std::fs::metadata(&abs) else { return false };
    if !meta.is_file() {
        return false;
    }
    let mtime = scan::mtime_seconds(&meta);
    let bytes = meta.len();
    if parent_scans_files(parent, name) && text_like(name) && bytes <= MAX_READ && bytes == text.as_bytes().len() as u64 {
        inner.files.insert(rel.to_string(), file_value(text, bytes, mtime));
    } else {
        inner.files.remove(rel);
    }
    if parent_ready(&inner.dirs, parent) && !upsert_entry(&mut inner.dirs, parent, name, bytes, mtime, false) {
        return false;
    }
    refresh_ancestors(&mut inner.dirs, root, rel);
    true
}

fn touch_visible_ancestors(inner: &mut Inner, root: &Path, rel: &str) -> bool {
    let mut acc = String::new();
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.is_empty() {
        return true;
    }
    for (i, seg) in parts.iter().enumerate() {
        let parent = acc.clone();
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        if i + 1 == parts.len() {
            break;
        }
        if parent_ready(&inner.dirs, &parent) {
            if !upsert_meta(&mut inner.dirs, root, &parent, seg) {
                return false;
            }
        }
    }
    true
}

/// `allow_file` 是本次写入的文件。新目录里若还有别的可预读文本，就放弃增量、等完整快照。
fn ensure_chain(inner: &mut Inner, root: &Path, dir_rel: &str, allow_file: Option<&str>) -> bool {
    if dir_rel.is_empty() {
        return inner.dirs.contains_key("");
    }
    let allow_abs = allow_file.map(|rel| abs_of(root, rel));
    let mut acc = String::new();
    for seg in dir_rel.split('/') {
        let parent = acc.clone();
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        let abs = abs_of(root, &acc);
        if parent_ready(&inner.dirs, &parent) && !upsert_meta(&mut inner.dirs, root, &parent, seg) {
            return false;
        }
        if dir_gets_key(&parent, seg, &abs) && !inner.dirs.contains_key(&acc) {
            if !adopt_tree(&mut inner.dirs, root, &acc, allow_abs.as_deref()) {
                return false;
            }
        }
    }
    true
}

fn adopt_tree(dirs: &mut Map<String, Value>, root: &Path, dir_rel: &str, allow: Option<&Path>) -> bool {
    let abs = abs_of(root, dir_rel);
    if gate::is_reparse(&abs) || !abs.is_dir() {
        return false;
    }
    let mut entries = Vec::new();
    let mut subdirs = Vec::new();
    let rd = match std::fs::read_dir(&abs) {
        Ok(rd) => rd,
        Err(_) => return false,
    };
    for ent in rd {
        let Ok(ent) = ent else { return false };
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with(fsops::TMP_PREFIX) {
            continue;
        }
        if dir_rel == "_工坊" && forbidden_ws_child(&name) {
            continue;
        }
        let path = ent.path();
        if gate::is_reparse(&path) {
            return false;
        }
        let Ok(meta) = std::fs::metadata(&path) else { return false };
        if meta.is_file() && text_like(&name) && meta.len() <= MAX_READ && allow != Some(path.as_path()) {
            return false;
        }
        if meta.is_dir() {
            subdirs.push(name.clone());
        }
        entries.push(entry_value(&name, meta.is_dir(), &meta));
    }
    sort_entries(&mut entries);
    dirs.insert(dir_rel.to_string(), Value::Array(entries));
    for name in subdirs {
        let child = if dir_rel.is_empty() { name.clone() } else { format!("{dir_rel}/{name}") };
        let child_abs = abs_of(root, &child);
        if dir_gets_key(dir_rel, &name, &child_abs) && !adopt_tree(dirs, root, &child, allow) {
            return false;
        }
    }
    true
}

fn demote_book_if_needed(dirs: &mut Map<String, Value>, files: &mut Map<String, Value>, root: &Path, rel: &str) {
    let (parent, name) = split_parent(rel);
    if name != "项目.json" || parent.is_empty() || parent.contains('/') {
        return;
    }
    let book = abs_of(root, parent);
    if !book.is_dir() || book.join("项目.json").is_file() {
        return;
    }
    let prefix = format!("{parent}/");
    dirs.retain(|k, _| k != parent && !k.starts_with(&prefix));
    files.retain(|k, _| !k.starts_with(&prefix));
    let _ = upsert_meta(dirs, root, "", parent);
}

fn remove_tree(dirs: &mut Map<String, Value>, files: &mut Map<String, Value>, rel: &str) {
    let prefix = format!("{rel}/");
    files.retain(|k, _| k != rel && !k.starts_with(&prefix));
    dirs.retain(|k, _| k != rel && !k.starts_with(&prefix));
    let (parent, name) = split_parent(rel);
    if let Some(Value::Array(entries)) = dirs.get_mut(parent) {
        entries.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(name));
    }
}

fn upsert_meta(dirs: &mut Map<String, Value>, root: &Path, parent: &str, name: &str) -> bool {
    let rel = if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") };
    let abs = abs_of(root, &rel);
    let Ok(meta) = std::fs::metadata(&abs) else { return false };
    let bytes = if meta.is_dir() { 0 } else { meta.len() };
    upsert_entry(dirs, parent, name, bytes, scan::mtime_seconds(&meta), meta.is_dir())
}

fn upsert_entry(dirs: &mut Map<String, Value>, parent: &str, name: &str, bytes: u64, mtime: f64, is_dir: bool) -> bool {
    if parent == "_工坊" && forbidden_ws_child(name) {
        return true;
    }
    let Some(Value::Array(entries)) = dirs.get_mut(parent) else { return false };
    let entry = entry_parts(name, if is_dir { "dir" } else { "file" }, bytes, mtime);
    if let Some(pos) = entries.iter().position(|e| e.get("name").and_then(|v| v.as_str()) == Some(name)) {
        entries[pos] = entry;
    } else {
        entries.push(entry);
    }
    sort_entries(entries);
    true
}

fn refresh_ancestors(dirs: &mut Map<String, Value>, root: &Path, rel: &str) {
    let mut parent = split_parent(rel).0.to_string();
    while !parent.is_empty() {
        let (grand, name) = split_parent(&parent);
        if dirs.contains_key(grand) {
            let _ = upsert_meta(dirs, root, grand, name);
        }
        if grand.is_empty() {
            break;
        }
        parent = grand.to_string();
    }
}

fn confirm(root: &Path, real: &Path, view: &View) -> Result<(), Bad> {
    let live = strip_dirs(scan::enumerated_dirs(root, real));
    if !dirs_equal(&view.dirs, &live) || !files_align(&view.files, &live) {
        return Err(miss("与磁盘不一致"));
    }
    Ok(())
}

fn memory_view(app: &App, id: &str) -> Option<View> {
    let inner = lock(app);
    if inner.warm && inner.root_id.as_deref() == Some(id) {
        Some(View { dirs: inner.dirs.clone(), files: inner.files.clone(), generation: inner.generation })
    } else {
        None
    }
}

fn install_clean_if_cold(app: &App, root: &Path, id: &str, view: &View) {
    let mut inner = lock(app);
    if inner.warm {
        return;
    }
    inner.root_id = Some(id.to_string());
    inner.target = Some(cache_file(root));
    inner.dirs = view.dirs.clone();
    inner.files = view.files.clone();
    inner.warm = true;
    inner.dirty = false;
    inner.dirty_since = None;
    inner.last_edit = None;
}

fn discard_if(app: &App, generation: u64) {
    let mut inner = lock(app);
    if inner.generation == generation {
        clear_warm(&mut inner);
    }
}

fn clear_warm(inner: &mut Inner) {
    inner.warm = false;
    inner.dirs.clear();
    inner.files.clear();
    inner.dirty = false;
    inner.dirty_since = None;
    inner.last_edit = None;
    inner.root_id = None;
    inner.target = None;
    inner.generation = inner.generation.wrapping_add(1);
}

/// `_工坊`、`_工坊/缓存`、`启动快照.json` 任一级是重解析点，或实路径越出数据根、落进机密，都不读写。
/// 文件可以不存在。数据根自身的重解析点不在这三级里，越界与否交给 `reparse_blocked`。
fn cache_chain_blocked(root: &Path, real: &Path) -> bool {
    let mut cur = root.to_path_buf();
    for seg in ["_工坊", "缓存", "启动快照.json"] {
        cur.push(seg);
        if gate::is_reparse(&cur) {
            return true;
        }
        if std::fs::symlink_metadata(&cur).is_ok() && gate::reparse_blocked(root, real, &cur) {
            return true;
        }
    }
    false
}

fn cache_write_blocked(path: &Path) -> bool {
    let Some(root) = path.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) else {
        return true;
    };
    let real = gate::try_realpath(root).unwrap_or_else(|| root.to_path_buf());
    cache_chain_blocked(root, &real)
}

fn read_disk(root: &Path, real: &Path, expect_root: &str) -> Result<View, Bad> {
    if cache_chain_blocked(root, real) {
        return Err(miss("重解析"));
    }
    let path = cache_file(root);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(miss("不存在")),
        Err(_) => return Err(miss("损坏")),
    };
    if !meta.is_file() {
        return Err(miss("损坏"));
    }
    if meta.len() == 0 {
        return Err(miss("为空"));
    }
    if meta.len() > MAX_CACHE {
        return Err(miss("过大"));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Err(miss("损坏")),
    };
    if bytes.len() as u64 != meta.len() {
        return Err(miss("截断"));
    }
    parse_envelope(&bytes, expect_root)
}

fn parse_envelope(bytes: &[u8], expect_root: &str) -> Result<View, Bad> {
    if bytes.is_empty() {
        return Err(miss("为空"));
    }
    let value: Value = match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if value.is_object() => value,
        _ => return Err(miss("损坏或截断")),
    };
    let obj = value.as_object().unwrap();
    let Some(format) = obj.get("format").and_then(|v| v.as_u64()) else { return Err(miss("形状不符")) };
    if format != FORMAT {
        return Err(miss("版本不符"));
    }
    let Some(app) = obj.get("app").and_then(|v| v.as_str()) else { return Err(miss("形状不符")) };
    if app != VER {
        return Err(miss("版本不符"));
    }
    let Some(root) = obj.get("root").and_then(|v| v.as_str()) else { return Err(miss("形状不符")) };
    if root != expect_root {
        return Err(miss("数据根不符"));
    }
    let Some(generated) = obj.get("generated").and_then(|v| v.as_str()) else { return Err(miss("形状不符")) };
    if generated.is_empty() || generated.len() > 40 || generated.chars().any(|c| (c as u32) < 32) {
        return Err(miss("形状不符"));
    }
    let Some(dirs_v) = obj.get("dirs").and_then(|v| v.as_object()) else { return Err(miss("形状不符")) };
    let Some(files_v) = obj.get("files").and_then(|v| v.as_object()) else { return Err(miss("形状不符")) };
    if dirs_v.len() > MAX_DIR_KEYS || files_v.len() > MAX_FILES {
        return Err(miss("条目过多"));
    }
    let mut children = 0usize;
    let mut dirs = Map::new();
    for (key, entries) in dirs_v {
        if !valid_rel(key, true) {
            return Err(miss("路径非法"));
        }
        let Some(list) = entries.as_array() else { return Err(miss("形状不符")) };
        children = children.saturating_add(list.len());
        if children > MAX_CHILDREN {
            return Err(miss("条目过多"));
        }
        let mut clean = Vec::with_capacity(list.len());
        let mut names = BTreeMap::new();
        for ent in list {
            let item = clean_entry(ent)?;
            let name = item["name"].as_str().unwrap().to_string();
            if key == "_工坊" && forbidden_ws_child(&name) {
                return Err(miss("路径非法"));
            }
            if names.insert(name, ()).is_some() {
                return Err(miss("形状不符"));
            }
            clean.push(item);
        }
        dirs.insert(key.to_string(), Value::Array(clean));
    }
    if !dirs.contains_key("") {
        return Err(miss("形状不符"));
    }
    let mut files = Map::new();
    for (key, file) in files_v {
        if !valid_rel(key, false) || forbidden_rel(key) {
            return Err(miss("路径非法"));
        }
        let item = clean_file(file)?;
        let (parent, name) = split_parent(key);
        if !files_align_one(&dirs, parent, name, &item) {
            return Err(miss("形状不符"));
        }
        files.insert(key.to_string(), item);
    }
    Ok(View { dirs, files, generation: 0 })
}

fn clean_entry(v: &Value) -> Result<Value, Bad> {
    let Some(name) = v.get("name").and_then(|x| x.as_str()) else { return Err(miss("形状不符")) };
    if !valid_segment(name) {
        return Err(miss("路径非法"));
    }
    let Some(typ) = v.get("type").and_then(|x| x.as_str()) else { return Err(miss("形状不符")) };
    if typ != "dir" && typ != "file" {
        return Err(miss("形状不符"));
    }
    let Some(bytes) = v.get("bytes").and_then(|x| x.as_u64()) else { return Err(miss("形状不符")) };
    if typ == "dir" && bytes != 0 {
        return Err(miss("形状不符"));
    }
    let mtime = mtime_from_disk(v.get("mtime").unwrap_or(&Value::Null))?;
    Ok(entry_parts(name, typ, bytes, mtime))
}

fn clean_file(v: &Value) -> Result<Value, Bad> {
    let Some(text) = v.get("text").and_then(|x| x.as_str()) else { return Err(miss("形状不符")) };
    let Some(bytes) = v.get("bytes").and_then(|x| x.as_u64()) else { return Err(miss("形状不符")) };
    let Some(hash) = v.get("hash").and_then(|x| x.as_str()) else { return Err(miss("形状不符")) };
    if !hex64(hash) || bytes > MAX_READ {
        return Err(miss("形状不符"));
    }
    let mtime = mtime_from_disk(v.get("mtime").unwrap_or(&Value::Null))?;
    if !hash_matches(text, hash, bytes) {
        return Err(miss("形状不符"));
    }
    Ok(json!({"text": text, "bytes": bytes, "hash": hash, "mtime": mtime}))
}

fn files_align(files: &Map<String, Value>, dirs: &Map<String, Value>) -> bool {
    files.iter().all(|(key, file)| {
        let (parent, name) = split_parent(key);
        files_align_one(dirs, parent, name, file)
    })
}

fn files_align_one(dirs: &Map<String, Value>, parent: &str, name: &str, file: &Value) -> bool {
    let Some(entries) = dirs.get(parent).and_then(|v| v.as_array()) else { return false };
    let Some(ent) = entries.iter().find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name)) else { return false };
    ent.get("type").and_then(|v| v.as_str()) == Some("file") && u64_eq(ent.get("bytes"), file.get("bytes")) && f64_eq(ent.get("mtime"), file.get("mtime"))
}

fn dirs_equal(cached: &Map<String, Value>, live: &Map<String, Value>) -> bool {
    if cached.len() != live.len() {
        return false;
    }
    cached.iter().all(|(key, entries)| live.get(key).is_some_and(|other| entries_equal(entries, other)))
}

fn entries_equal(a: &Value, b: &Value) -> bool {
    let (Some(a), Some(b)) = (a.as_array(), b.as_array()) else { return false };
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| entry_eq(x, y))
}

fn entry_eq(a: &Value, b: &Value) -> bool {
    let Some(name) = a.get("name").and_then(|v| v.as_str()) else { return false };
    if b.get("name").and_then(|v| v.as_str()) != Some(name) {
        return false;
    }
    let typ = a.get("type").and_then(|v| v.as_str());
    if b.get("type").and_then(|v| v.as_str()) != typ {
        return false;
    }
    if !u64_eq(a.get("bytes"), b.get("bytes")) {
        return false;
    }
    if typ == Some("dir") && name == "_工坊" {
        return true;
    }
    f64_eq(a.get("mtime"), b.get("mtime"))
}

fn hit_body(view: &View) -> Value {
    json!({"ok": true, "snapshot": {"dirs": view.dirs, "files": view.files}})
}

fn miss(reason: &str) -> Bad {
    bad(404, "cache-miss", format!("启动缓存不可用：{reason}"))
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let mut guard = shared.mu.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if guard.stop {
                let dirty = guard.dirty && guard.warm;
                drop(guard);
                if dirty {
                    let _ = write_once(&shared);
                }
                return;
            }
            if guard.dirty && guard.warm {
                let now = Instant::now();
                if due_inner(&guard, now) {
                    break;
                }
                let wait = wait_for(&guard, now);
                let (next, _) = shared.cv.wait_timeout(guard, wait).unwrap_or_else(|e| e.into_inner());
                guard = next;
                continue;
            }
            guard = shared.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        drop(guard);
        let _ = write_once(&shared);
    }
}

fn due_inner(inner: &Inner, now: Instant) -> bool {
    let since_dirty = inner.dirty_since.map(|t| now.saturating_duration_since(t)).unwrap_or_default();
    let since_edit = inner.last_edit.map(|t| now.saturating_duration_since(t)).unwrap_or_default();
    flush_due(since_dirty, since_edit, inner.quiet, inner.max_wait)
}

fn wait_for(inner: &Inner, now: Instant) -> Duration {
    let quiet_at = inner.last_edit.unwrap_or(now) + inner.quiet;
    let max_at = inner.dirty_since.unwrap_or(now) + inner.max_wait;
    let at = quiet_at.min(max_at);
    at.saturating_duration_since(now).max(Duration::from_millis(1))
}

fn flush_due(since_dirty: Duration, since_edit: Duration, quiet: Duration, max_wait: Duration) -> bool {
    since_dirty >= max_wait || since_edit >= quiet
}

fn write_once(shared: &Shared) -> bool {
    let payload = {
        let guard = shared.mu.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.dirty || !guard.warm {
            return true;
        }
        let (Some(path), Some(root_id)) = (guard.target.clone(), guard.root_id.clone()) else {
            return true;
        };
        let generation = guard.generation;
        let body = envelope(&root_id, &guard.dirs, &guard.files);
        let bytes = match serde_json::to_vec(&body) {
            Ok(bytes) => bytes,
            Err(_) => {
                log_fail("encode");
                return false;
            }
        };
        if bytes.len() as u64 > MAX_CACHE {
            log_fail("oversize");
            drop(guard);
            let mut guard = shared.mu.lock().unwrap_or_else(|e| e.into_inner());
            if guard.generation == generation {
                guard.dirty = false;
            }
            return false;
        }
        (generation, path, bytes)
    };
    let (generation, path, bytes) = payload;
    let blocked = cache_write_blocked(&path);
    let result = if blocked {
        Err(std::io::Error::new(std::io::ErrorKind::Other, "reparse"))
    } else {
        fsops::atomic_write(&path, &bytes)
    };
    match result {
        Ok(()) => {
            let mut guard = shared.mu.lock().unwrap_or_else(|e| e.into_inner());
            if guard.generation == generation {
                guard.dirty = false;
                guard.dirty_since = None;
            }
            true
        }
        Err(_) => {
            log_fail("io");
            let mut guard = shared.mu.lock().unwrap_or_else(|e| e.into_inner());
            if guard.generation == generation && guard.dirty {
                if blocked {
                    // 重解析点不会因为重试消失。放弃这一次，避免每秒刷日志。
                    guard.dirty = false;
                    guard.dirty_since = None;
                } else {
                    let now = Instant::now();
                    guard.dirty_since = Some(now);
                    guard.last_edit = Some(now);
                }
            }
            false
        }
    }
}

fn mark_dirty(inner: &mut Inner) {
    let now = Instant::now();
    if !inner.dirty {
        inner.dirty_since = Some(now);
    }
    inner.last_edit = Some(now);
    inner.dirty = true;
    inner.generation = inner.generation.wrapping_add(1);
}

fn envelope(root_id: &str, dirs: &Map<String, Value>, files: &Map<String, Value>) -> Value {
    // JSON 数字不能无损装下全部 f64。mtime 以 IEEE 位型整数落盘，读回后再和磁盘比。
    json!({
        "format": FORMAT,
        "app": VER,
        "root": root_id,
        "generated": generated_now(),
        "dirs": seal_dirs(dirs),
        "files": seal_files(files),
    })
}

fn seal_dirs(dirs: &Map<String, Value>) -> Map<String, Value> {
    dirs.iter()
        .map(|(key, value)| {
            let entries = value.as_array().map(|list| list.iter().map(seal_entry).collect()).unwrap_or_default();
            (key.clone(), Value::Array(entries))
        })
        .collect()
}

fn seal_files(files: &Map<String, Value>) -> Map<String, Value> {
    files.iter()
        .map(|(key, value)| {
            let mtime = value.get("mtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
            (
                key.clone(),
                json!({
                    "text": value.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    "bytes": value.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                    "hash": value.get("hash").and_then(|v| v.as_str()).unwrap_or(""),
                    "mtime": mtime.to_bits(),
                }),
            )
        })
        .collect()
}

fn seal_entry(value: &Value) -> Value {
    let mtime = value.get("mtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
    json!({
        "name": value.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "type": value.get("type").and_then(|v| v.as_str()).unwrap_or(""),
        "bytes": value.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        "mtime": mtime.to_bits(),
    })
}

fn mtime_from_disk(value: &Value) -> Result<f64, Bad> {
    let Some(bits) = value.as_u64() else { return Err(miss("形状不符")) };
    let mtime = f64::from_bits(bits);
    if !mtime.is_finite() || mtime < 0.0 {
        return Err(miss("形状不符"));
    }
    Ok(mtime)
}

fn generated_now() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis().to_string()).unwrap_or_else(|_| "0".into())
}

fn log_fail(kind: &str) {
    crate::shell::log_line(&format!("[SF] 启动缓存落盘失败（{kind}），已忽略"));
}

fn cache_file(root: &Path) -> PathBuf {
    root.join("_工坊").join("缓存").join("启动快照.json")
}

fn lock(app: &App) -> std::sync::MutexGuard<'_, Inner> {
    app.startup_cache.shared.mu.lock().unwrap_or_else(|e| e.into_inner())
}

fn root_id(real: &Path) -> String {
    let mut text = real.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        text = format!("\\\\{rest}");
    } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
        text = rest.to_string();
    }
    text.to_lowercase()
}

fn strip_dirs(mut dirs: Map<String, Value>) -> Map<String, Value> {
    dirs.retain(|k, _| k.is_empty() || !forbidden_rel(k));
    if let Some(Value::Array(entries)) = dirs.get_mut("_工坊") {
        entries.retain(|e| e.get("name").and_then(|v| v.as_str()).is_some_and(|n| !forbidden_ws_child(n)));
    }
    dirs
}

fn strip_files(mut files: Map<String, Value>) -> Map<String, Value> {
    files.retain(|k, _| !forbidden_rel(k));
    files
}

fn forbidden_rel(rel: &str) -> bool {
    let n = rel.to_ascii_lowercase();
    if n == "_工坊/settings.json" || n == "_工坊/服务锁.json" || n == "_工坊/服务锁.json.tmp" {
        return true;
    }
    for tree in ["_工坊/机密", "_工坊/回收站", "_工坊/缓存"] {
        let tree = tree.to_ascii_lowercase();
        if n == tree || n.starts_with(&(tree + "/")) {
            return true;
        }
    }
    false
}

fn forbidden_ws_child(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "settings.json" || n == "服务锁.json" || n == "服务锁.json.tmp" || name == "机密" || name == "回收站" || name == "缓存"
}

fn valid_rel(rel: &str, allow_empty: bool) -> bool {
    if rel.is_empty() {
        return allow_empty;
    }
    if rel.starts_with('/') || rel.ends_with('/') || rel.contains('\\') || rel.contains(':') {
        return false;
    }
    rel.split('/').all(valid_segment) && !forbidden_rel(rel)
}

fn valid_segment(seg: &str) -> bool {
    if seg.is_empty() || seg == "." || seg == ".." || seg.len() > 255 {
        return false;
    }
    if seg.ends_with(' ') || seg.ends_with('.') || seg.starts_with(fsops::TMP_PREFIX) {
        return false;
    }
    if seg.chars().any(|c| (c as u32) < 32 || c == '\\' || c == '/' || c == ':') {
        return false;
    }
    let head = seg.split('.').next().unwrap_or("");
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    !RESERVED.iter().any(|n| head.eq_ignore_ascii_case(n))
}

fn dir_gets_key(parent: &str, name: &str, abs: &Path) -> bool {
    if parent.is_empty() {
        return name == "_工坊" || abs.join("项目.json").is_file();
    }
    if parent == "_工坊" {
        return name == "共享设定";
    }
    if !parent.contains('/') && parent != "_工坊" {
        return BOOK_SUBS.contains(&name);
    }
    walked(parent)
}

fn parent_scans_files(parent: &str, name: &str) -> bool {
    if parent == "_工坊" {
        return WS_FILES.iter().any(|n| name.eq_ignore_ascii_case(n));
    }
    if parent.is_empty() {
        return false;
    }
    if !parent.contains('/') && parent != "_工坊" {
        return name == "项目.json" || name == "标签.json";
    }
    walked(parent)
}

fn walked(rel: &str) -> bool {
    let mut parts = rel.split('/');
    let first = parts.next().unwrap_or("");
    let second = parts.next().unwrap_or("");
    if first == "_工坊" {
        return second == "共享设定";
    }
    BOOK_SUBS.contains(&second)
}

fn text_like(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".json") || n.ends_with(".html") || n.ends_with(".md") || n.ends_with(".txt")
}

fn hash_matches(text: &str, hash: &str, bytes: u64) -> bool {
    let utf8 = text.as_bytes();
    if utf8.len() as u64 == bytes {
        return util::sha256_hex(utf8) == hash;
    }
    if bytes == utf8.len() as u64 + 3 {
        let mut raw = Vec::with_capacity(utf8.len() + 3);
        raw.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        raw.extend_from_slice(utf8);
        return util::sha256_hex(&raw) == hash;
    }
    false
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn file_value(text: &str, bytes: u64, mtime: f64) -> Value {
    json!({"text": text, "bytes": bytes, "hash": util::sha256_hex(text.as_bytes()), "mtime": mtime})
}

fn entry_parts(name: &str, typ: &str, bytes: u64, mtime: f64) -> Value {
    json!({"name": name, "type": typ, "bytes": bytes, "mtime": mtime})
}

fn entry_value(name: &str, is_dir: bool, meta: &std::fs::Metadata) -> Value {
    let bytes = if is_dir { 0 } else { meta.len() };
    entry_parts(name, if is_dir { "dir" } else { "file" }, bytes, scan::mtime_seconds(meta))
}

fn sort_entries(entries: &mut [Value]) {
    entries.sort_by(|a, b| {
        let ad = a.get("type").and_then(|v| v.as_str()) != Some("dir");
        let bd = b.get("type").and_then(|v| v.as_str()) != Some("dir");
        ad.cmp(&bd).then_with(|| {
            a.get("name").and_then(|v| v.as_str()).unwrap_or("").cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
        })
    });
}

fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

fn abs_of(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    if !rel.is_empty() {
        for seg in rel.split('/') {
            path.push(seg);
        }
    }
    path
}

fn u64_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a.and_then(|v| v.as_u64()), b.and_then(|v| v.as_u64())) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn f64_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a.and_then(|v| v.as_f64()), b.and_then(|v| v.as_f64())) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
fn widen_flush_window(app: &App) {
    let mut inner = lock(app);
    inner.quiet = Duration::from_secs(3600);
    inner.max_wait = Duration::from_secs(3600);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::Request;
    use http_body_util::BodyExt;

    const CANARY: &str = "SF-CACHE-CANARY-7c1e9a-settings-key";
    const SECRET_CANARY: &str = "SF-SECRET-CANARY-b4e0c2-not-a-manuscript";
    const BODY_MARKER: &str = "SF-CACHE-BODY-MARKER-chapter";

    struct Temp {
        path: PathBuf,
        junctions: Vec<PathBuf>,
    }

    impl Temp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sf-cache-{name}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(path.join("_工坊")).unwrap();
            Self { path, junctions: Vec::new() }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn keep_junction(&mut self, path: PathBuf) {
            self.junctions.push(path);
        }

        fn app(&self) -> App {
            let real = gate::try_realpath(&self.path).unwrap_or_else(|| self.path.clone());
            App {
                token: "tok".into(),
                port: 1,
                exe_dir: self.path.clone(),
                data: Mutex::new(Some(crate::state::DataRoot { path: self.path.clone(), real })),
                lock: Mutex::new(None),
                cfg: Mutex::new(Value::Null),
                write_lock: Mutex::new(()),
                app_override: None,
                shell: false,
                origins: std::collections::HashSet::new(),
                hosts: std::collections::HashSet::from(["127.0.0.1".into()]),
                startup_cache: Cache::new(),
            }
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            for link in &self.junctions {
                let _ = std::fs::remove_dir(link);
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_rel(root: &Path, rel: &str, body: &[u8]) {
        let path = abs_of(root, rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn book(root: &Path) {
        write_rel(root, "书/项目.json", b"{}");
        write_rel(root, "书/正文/a.txt", "甲".as_bytes());
    }

    fn post(app: &App, path: &str, body: Value) -> Result<Value, Bad> {
        let raw = serde_json::to_vec(&body).unwrap();
        match path {
            "write" => crate::fsapi::write_file(app, &raw),
            "mkdir" => crate::fsapi::mkdir(app, &raw),
            "delete" => crate::fsapi::delete(app, &raw),
            _ => unreachable!(),
        }
    }

    fn cache_bytes(root: &Path) -> Option<Vec<u8>> {
        std::fs::read(cache_file(root)).ok()
    }

    fn log_tail(from: u64) -> String {
        let path = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("故事工坊日志.txt")));
        let Some(path) = path else { return String::new() };
        let Ok(bytes) = std::fs::read(path) else { return String::new() };
        let start = (from as usize).min(bytes.len());
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }

    fn log_len() -> u64 {
        let path = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("故事工坊日志.txt")));
        path.and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0)
    }

    #[test]
    fn missing_cache_is_a_miss_and_not_a_stale_success() {
        let dir = Temp::new("miss");
        let app = dir.app();
        let err = load(&app).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.error, "cache-miss");
        assert!(!err.message.contains("甲"));
        let bare = App {
            token: "t".into(),
            port: 1,
            exe_dir: dir.path().to_path_buf(),
            data: Mutex::new(None),
            lock: Mutex::new(None),
            cfg: Mutex::new(Value::Null),
            write_lock: Mutex::new(()),
            app_override: None,
            shell: false,
            origins: std::collections::HashSet::new(),
            hosts: std::collections::HashSet::new(),
            startup_cache: Cache::new(),
        };
        let err = load(&bare).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.error, "no-data-root");
    }

    #[tokio::test]
    async fn http_reports_miss_then_hit_without_envelope() {
        let dir = Temp::new("http");
        book(dir.path());
        write_rel(dir.path(), "_工坊/settings.json", format!(r#"{{"key":"{CANARY}"}}"#).as_bytes());
        let app = Arc::new(dir.app());
        let miss = crate::http::dispatch(State(Arc::clone(&app)), get("/api/fs/startup-cache")).await;
        assert_eq!(miss.status(), 404);
        let miss_body: Value = serde_json::from_slice(&miss.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(miss_body["ok"], false);
        assert_eq!(miss_body["error"], "cache-miss");
        assert!(miss_body.get("snapshot").is_none());
        assert!(!miss_body.to_string().contains(CANARY));

        let snap = scan::snapshot(&app).unwrap();
        assert_eq!(snap["files"]["_工坊/settings.json"]["text"].as_str().unwrap(), format!(r#"{{"key":"{CANARY}"}}"#));
        let hit = crate::http::dispatch(State(Arc::clone(&app)), get("/api/fs/startup-cache")).await;
        assert_eq!(hit.status(), 200);
        let body: Value = serde_json::from_slice(&hit.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["ok"], true);
        assert!(body.get("format").is_none());
        assert!(body.get("root").is_none());
        assert_eq!(body["snapshot"]["files"]["书/正文/a.txt"]["text"], "甲");
        assert!(body["snapshot"]["files"].get("_工坊/settings.json").is_none());
        assert!(!body.to_string().contains(CANARY));
        let names = dir_names(&body["snapshot"], "_工坊");
        assert!(!names.iter().any(|n| n == "缓存" || n == "机密" || n == "回收站" || n == "settings.json"));
    }

    #[test]
    fn cache_file_omits_settings_secrets_trash_and_canary() {
        let dir = Temp::new("canary");
        let app = dir.app();
        widen_flush_window(&app);
        book(dir.path());
        write_rel(dir.path(), "_工坊/机密/密钥.txt", SECRET_CANARY.as_bytes());
        write_rel(dir.path(), "_工坊/回收站/旧.txt", "trash-marker".as_bytes());
        write_rel(dir.path(), "_工坊/settings.json", format!(r#"{{"apiKey":"{CANARY}"}}"#).as_bytes());
        write_rel(dir.path(), "书/正文/a.txt", BODY_MARKER.as_bytes());
        let real = gate::try_realpath(dir.path()).unwrap_or_else(|| dir.path().to_path_buf());
        if let Ok(target) = crate::secret::path(dir.path(), &real, "供应商") {
            if let Ok(blob) = crate::secret::seal(SECRET_CANARY.as_bytes()) {
                let _ = fsops::atomic_write(&target, &blob);
            }
        }
        let before = log_len();
        scan::snapshot(&app).unwrap();
        post(&app, "write", json!({"path": "_工坊/settings.json", "text": format!(r#"{{"apiKey":"{CANARY}-2"}}"#)})).unwrap();
        flush_best_effort(&app);
        let bytes = cache_bytes(dir.path()).expect("缓存应落盘");
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(CANARY), "设置里的 Key 进了缓存文件");
        assert!(!text.contains(SECRET_CANARY), "机密进了缓存文件");
        assert!(!text.contains("trash-marker"));
        assert!(!text.contains("_工坊/settings.json"));
        assert!(!text.contains("_工坊/机密"));
        assert!(!text.contains("_工坊/回收站"));
        assert!(text.contains(BODY_MARKER));
        let log = log_tail(before);
        assert!(!log.contains(CANARY));
        assert!(!log.contains(SECRET_CANARY));
        assert!(!log.contains(BODY_MARKER));
        let hit = load(&app).unwrap();
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["text"], BODY_MARKER);
        let settings = std::fs::read_to_string(dir.path().join("_工坊").join("settings.json")).unwrap();
        assert!(settings.contains(CANARY));
    }

    #[test]
    fn damaged_caches_miss_and_are_not_deleted() {
        let dir = Temp::new("bad");
        let app = dir.app();
        widen_flush_window(&app);
        book(dir.path());
        scan::snapshot(&app).unwrap();
        flush_best_effort(&app);
        let good = cache_bytes(dir.path()).unwrap();
        let parsed: Value = serde_json::from_slice(&good).unwrap();

        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("empty", Vec::new(), "为空"),
            ("trunc", good[..good.len() / 2].to_vec(), "损坏或截断"),
            ("junk", b"{".to_vec(), "损坏或截断"),
            ("bad-hash", mutate_hash(&parsed), "形状不符"),
        ];
        for (name, bytes, reason) in cases {
            std::fs::write(cache_file(dir.path()), &bytes).unwrap();
            invalidate(&app);
            let err = load(&app).unwrap_err();
            assert_eq!(err.error, "cache-miss", "{name}");
            assert!(err.message.contains(reason), "{name} {}", err.message);
            assert_eq!(std::fs::read(cache_file(dir.path())).unwrap(), bytes, "{name} 被删或被改写了");
        }

        let mut wrong_ver = parsed.clone();
        wrong_ver["format"] = json!(99);
        std::fs::write(cache_file(dir.path()), serde_json::to_vec(&wrong_ver).unwrap()).unwrap();
        invalidate(&app);
        let err = load(&app).unwrap_err();
        assert!(err.message.contains("版本不符"));

        let mut wrong_app = parsed.clone();
        wrong_app["app"] = json!("9.9");
        std::fs::write(cache_file(dir.path()), serde_json::to_vec(&wrong_app).unwrap()).unwrap();
        invalidate(&app);
        assert!(load(&app).unwrap_err().message.contains("版本不符"));

        let mut wrong_root = parsed.clone();
        wrong_root["root"] = json!("d:\\other-root");
        std::fs::write(cache_file(dir.path()), serde_json::to_vec(&wrong_root).unwrap()).unwrap();
        invalidate(&app);
        assert!(load(&app).unwrap_err().message.contains("数据根不符"));

        let mut illegal = parsed.clone();
        illegal["files"]["../outside.txt"] = json!({"text":"x","bytes":1,"hash":util::sha256_hex(b"x"),"mtime":1.0});
        std::fs::write(cache_file(dir.path()), serde_json::to_vec(&illegal).unwrap()).unwrap();
        let outside = dir.path().join("..").join("outside-canary.txt");
        let _ = std::fs::write(&outside, b"outside-canary");
        invalidate(&app);
        let err = load(&app).unwrap_err();
        assert_eq!(err.error, "cache-miss");
        assert!(!err.message.contains("outside-canary"));
        let _ = std::fs::remove_file(&outside);

        let big = cache_file(dir.path());
        let file = std::fs::OpenOptions::new().write(true).open(&big).unwrap();
        file.set_len(MAX_CACHE + 1).unwrap();
        drop(file);
        invalidate(&app);
        let err = load(&app).unwrap_err();
        assert!(err.message.contains("过大"));
        assert_eq!(std::fs::metadata(&big).unwrap().len(), MAX_CACHE + 1);

        let mut too_many = parsed.clone();
        let files = too_many["files"].as_object_mut().unwrap();
        for i in 0..MAX_FILES + 1 {
            let text = "z";
            files.insert(
                format!("书/正文/n{i}.txt"),
                json!({"text": text, "bytes": 1, "hash": util::sha256_hex(text.as_bytes()), "mtime": 1.0}),
            );
        }
        std::fs::write(cache_file(dir.path()), serde_json::to_vec(&too_many).unwrap()).unwrap();
        invalidate(&app);
        assert!(load(&app).unwrap_err().message.contains("条目过多"));
        assert!(cache_file(dir.path()).is_file());
    }

    #[test]
    fn path_size_and_mtime_changes_are_misses() {
        let dir = Temp::new("meta");
        let app = dir.app();
        book(dir.path());
        scan::snapshot(&app).unwrap();
        let hit = load(&app).unwrap();
        let old = hit["snapshot"]["files"]["书/正文/a.txt"]["text"].as_str().unwrap().to_string();

        write_rel(dir.path(), "书/正文/b.txt", b"new");
        let err = load(&app).unwrap_err();
        assert_eq!(err.error, "cache-miss");
        assert!(!err.message.contains(&old));
        assert!(err.message.contains("与磁盘不一致"));

        scan::snapshot(&app).unwrap();
        std::fs::remove_file(abs_of(dir.path(), "书/正文/b.txt")).unwrap();
        assert_eq!(load(&app).unwrap_err().error, "cache-miss");

        scan::snapshot(&app).unwrap();
        write_rel(dir.path(), "书/正文/a.txt", "甲甲".as_bytes());
        assert_eq!(load(&app).unwrap_err().error, "cache-miss");

        scan::snapshot(&app).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(abs_of(dir.path(), "书/正文/a.txt")).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(10)).unwrap();
        drop(file);
        assert_eq!(load(&app).unwrap_err().error, "cache-miss");
    }

    #[test]
    fn same_size_and_mtime_does_not_promote_cache_hash_to_disk_hash() {
        let dir = Temp::new("samemeta");
        let app = dir.app();
        write_rel(dir.path(), "书/项目.json", b"{}");
        write_rel(dir.path(), "书/正文/a.txt", b"abcd");
        scan::snapshot(&app).unwrap();
        let before = load(&app).unwrap();
        let old_hash = before["snapshot"]["files"]["书/正文/a.txt"]["hash"].as_str().unwrap().to_string();
        let path = abs_of(dir.path(), "书/正文/a.txt");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"wxyz").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(mtime).unwrap();
        drop(file);
        let hit = load(&app).unwrap();
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["text"], "abcd");
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["hash"], old_hash);
        assert_ne!(old_hash, util::sha256_hex(b"wxyz"));
        scan::snapshot(&app).unwrap();
        let fresh = load(&app).unwrap();
        assert_eq!(fresh["snapshot"]["files"]["书/正文/a.txt"]["text"], "wxyz");
        assert_eq!(fresh["snapshot"]["files"]["书/正文/a.txt"]["hash"], util::sha256_hex(b"wxyz"));
    }

    #[test]
    fn writes_mkdir_and_deletes_keep_the_cache_aligned() {
        let dir = Temp::new("edit");
        let app = dir.app();
        book(dir.path());
        scan::snapshot(&app).unwrap();
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": "乙"})).unwrap();
        let hit = load(&app).unwrap();
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["text"], "乙");
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["hash"], util::sha256_hex("乙".as_bytes()));

        post(&app, "mkdir", json!({"path": "书/正文/章"})).unwrap();
        post(&app, "write", json!({"path": "书/正文/章/c.txt", "text": "丙"})).unwrap();
        let hit = load(&app).unwrap();
        assert_eq!(hit["snapshot"]["files"]["书/正文/章/c.txt"]["text"], "丙");
        assert!(dir_names(&hit["snapshot"], "书/正文").contains(&"章".to_string()));

        post(&app, "delete", json!({"path": "书/正文/章/c.txt"})).unwrap();
        let hit = load(&app).unwrap();
        assert!(hit["snapshot"]["files"].get("书/正文/章/c.txt").is_none());

        post(&app, "mkdir", json!({"path": "书/正文/章/段"})).unwrap();
        post(&app, "write", json!({"path": "书/正文/章/段/d.txt", "text": "丁"})).unwrap();
        post(&app, "delete", json!({"path": "书/正文/章"})).unwrap();
        let hit = load(&app).unwrap();
        let files = hit["snapshot"]["files"].as_object().unwrap();
        assert!(files.keys().all(|k| !k.starts_with("书/正文/章")));
        assert!(hit["snapshot"]["dirs"].get("书/正文/章").is_none());
        assert_eq!(files["书/正文/a.txt"]["text"], "乙");

        let old = hit.to_string();
        let err = post(&app, "write", json!({"path": "书/正文/a.txt", "text": "错", "baseHash": "deadbeef"})).unwrap_err();
        assert_eq!(err.error, "conflict");
        let again = load(&app).unwrap();
        assert_eq!(again["snapshot"]["files"]["书/正文/a.txt"]["text"], "乙");
        assert_eq!(again["snapshot"]["files"]["书/正文/a.txt"]["text"].as_str().unwrap(), "乙");
        assert!(old.contains("乙"));
        let err = post(&app, "mkdir", json!({"path": "书/正文/a.txt"})).unwrap_err();
        assert_eq!(err.error, "conflict");
        assert_eq!(load(&app).unwrap()["snapshot"]["files"]["书/正文/a.txt"]["text"], "乙");
    }

    #[test]
    fn edits_do_not_rewrite_the_cache_file_until_flush() {
        let dir = Temp::new("debounce");
        let app = dir.app();
        widen_flush_window(&app);
        book(dir.path());
        scan::snapshot(&app).unwrap();
        assert!(cache_bytes(dir.path()).is_none(), "快照返回前不应同步写缓存");
        flush_best_effort(&app);
        let first = String::from_utf8(cache_bytes(dir.path()).unwrap()).unwrap();
        assert!(first.contains("甲"));
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": "一次"})).unwrap();
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": "两次"})).unwrap();
        let mid = String::from_utf8(cache_bytes(dir.path()).unwrap()).unwrap();
        assert!(mid.contains("甲"));
        assert!(!mid.contains("两次"));
        assert_eq!(load(&app).unwrap()["snapshot"]["files"]["书/正文/a.txt"]["text"], "两次");
        flush_best_effort(&app);
        let saved = String::from_utf8(cache_bytes(dir.path()).unwrap()).unwrap();
        assert!(saved.contains("两次"));
        assert!(!saved.contains("一次"));
        assert!(flush_due(Duration::from_secs(5), Duration::from_millis(200), QUIET, MAX_WAIT));
        assert!(!flush_due(Duration::from_millis(900), Duration::from_millis(100), QUIET, MAX_WAIT));
        assert!(flush_due(Duration::from_secs(1), Duration::from_secs(1), QUIET, MAX_WAIT));
    }

    #[test]
    fn release_data_persists_before_dropping_the_lock() {
        let src = include_str!("startup_cache.rs");
        let start = src.find("fn release_data").expect("release_data");
        let body = &src[start..start + 180];
        let flush_at = body.find("flush_best_effort").expect("flush");
        let unlock_at = body.find("lock::release").expect("release");
        assert!(flush_at < unlock_at);
        let dir = Temp::new("unlock");
        let app = dir.app();
        widen_flush_window(&app);
        book(dir.path());
        crate::lock::acquire(&app, dir.path()).unwrap();
        scan::snapshot(&app).unwrap();
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": "收尾"})).unwrap();
        assert!(!cache_bytes(dir.path()).unwrap_or_default().windows(6).any(|w| w == "收尾".as_bytes()));
        release_data(&app, dir.path());
        let saved = String::from_utf8(cache_bytes(dir.path()).unwrap()).unwrap();
        assert!(saved.contains("收尾"));
        assert!(!dir.path().join("_工坊").join("服务锁.json").exists());
    }

    #[test]
    fn cache_write_failure_does_not_block_manuscripts_or_snapshot() {
        let dir = Temp::new("iofail");
        let app = dir.app();
        widen_flush_window(&app);
        book(dir.path());
        std::fs::write(dir.path().join("_工坊").join("缓存"), b"not-a-directory").unwrap();
        let before = log_len();
        let snap = scan::snapshot(&app).unwrap();
        assert_eq!(snap["files"]["书/正文/a.txt"]["text"], "甲");
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": BODY_MARKER})).unwrap();
        assert!(!flush_best_effort_ok(&app));
        let on_disk = std::fs::read(abs_of(dir.path(), "书/正文/a.txt")).unwrap();
        assert_eq!(on_disk, BODY_MARKER.as_bytes());
        let log = log_tail(before);
        assert!(log.contains("启动缓存落盘失败"));
        assert!(!log.contains(BODY_MARKER));
        assert!(!log.contains(CANARY));
        let again = scan::snapshot(&app).unwrap();
        assert_eq!(again["ok"], true);
        assert_eq!(again["files"]["书/正文/a.txt"]["text"], BODY_MARKER);
    }

    #[test]
    fn switching_root_drops_memory_until_a_new_snapshot() {
        let a = Temp::new("root-a");
        let b = Temp::new("root-b");
        book(a.path());
        book(b.path());
        write_rel(b.path(), "书/正文/a.txt", "乙库".as_bytes());
        let app = a.app();
        widen_flush_window(&app);
        scan::snapshot(&app).unwrap();
        post(&app, "write", json!({"path": "书/正文/a.txt", "text": "甲改"})).unwrap();
        app.install_data(b.path().to_path_buf());
        let saved = String::from_utf8(cache_bytes(a.path()).unwrap()).unwrap();
        assert!(saved.contains("甲改"));
        assert!(!saved.contains("乙库"));
        let err = load(&app).unwrap_err();
        assert_eq!(err.error, "cache-miss");
        assert!(cache_bytes(b.path()).is_none());
        scan::snapshot(&app).unwrap();
        assert_eq!(load(&app).unwrap()["snapshot"]["files"]["书/正文/a.txt"]["text"], "乙库");
    }

    #[test]
    fn bom_text_matches_the_cached_raw_hash() {
        let dir = Temp::new("bom");
        let app = dir.app();
        write_rel(dir.path(), "书/项目.json", b"{}");
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice("甲".as_bytes());
        write_rel(dir.path(), "书/正文/a.txt", &raw);
        scan::snapshot(&app).unwrap();
        flush_best_effort(&app);
        invalidate(&app);
        let hit = load(&app).unwrap();
        let file = &hit["snapshot"]["files"]["书/正文/a.txt"];
        assert_eq!(file["text"], "甲");
        assert_eq!(file["hash"], util::sha256_hex(&raw));
        assert_ne!(file["hash"], util::sha256_hex("甲".as_bytes()));
    }

    #[test]
    fn cache_directory_stays_out_of_snapshot_and_golden_fingerprint() {
        let dir = Temp::new("exclude");
        let app = dir.app();
        book(dir.path());
        write_rel(dir.path(), "_工坊/缓存/启动快照.json", b"{\"text\":\"should-not-leak\"}");
        write_rel(dir.path(), "_工坊/缓存/note.txt", b"cache-note");
        let snap = scan::snapshot(&app).unwrap();
        let names = dir_names(&snap, "_工坊");
        assert!(!names.iter().any(|n| n == "缓存"));
        let blob = serde_json::to_string(&snap).unwrap();
        assert!(!blob.contains("should-not-leak"));
        assert!(!blob.contains("cache-note"));
        assert!(snap["dirs"].get("_工坊/缓存").is_none());

        let root = PathBuf::from(r"D:\sf-unified\.scratch\startup-cold-fastpath\runtime\corpus");
        if !root.is_dir() {
            println!("SKIP 黄金语料不在：{}", root.display());
            return;
        }
        let corpus = app_at(&root);
        let before = scan::snapshot(&corpus).unwrap();
        let fp_before = fingerprint_via_node(&before);
        assert_eq!(fp_before.digest, "2c49e2dda27fb614edd05ede2bacbb1f2ebf8267888e634d980de6b0c5e682fc");
        assert_eq!(fp_before.files, 5532);
        assert_eq!(fp_before.dirs, 974);
        assert_eq!(fp_before.bytes, 20_971_520);
        flush_best_effort(&corpus);
        assert!(cache_file(&root).is_file(), "完整快照后应生成缓存");
        invalidate(&corpus);
        let cached = load(&corpus).expect("语料缓存应在元数据未变时命中");
        let cached_files = cached["snapshot"]["files"].as_object().unwrap();
        assert!(cached_files.get("_工坊/settings.json").is_none());
        for (key, file) in before["files"].as_object().unwrap() {
            if key == "_工坊/settings.json" {
                continue;
            }
            assert_eq!(cached_files.get(key).and_then(|v| v.get("text")), file.get("text"), "{key}");
        }
        let after = scan::snapshot(&corpus).unwrap();
        let fp_after = fingerprint_via_node(&after);
        assert_eq!(fp_after.digest, fp_before.digest);
        assert_eq!(fp_after.files, fp_before.files);
        assert_eq!(fp_after.dirs, fp_before.dirs);
        assert_eq!(fp_after.bytes, fp_before.bytes);
        assert_eq!(file_hash_blob(&before), file_hash_blob(&after));
        let ws = dir_names(&after, "_工坊");
        assert!(!ws.iter().any(|n| n == "缓存"));
        assert!(after["files"].as_object().unwrap().keys().all(|k| !k.contains("缓存/")));
        invalidate(&corpus);
        let _ = std::fs::remove_dir_all(root.join("_工坊").join("缓存"));
    }

    #[test]
    fn cache_dir_junction_outside_root_is_not_read_or_written() {
        let outside = Temp::new("cout-ext");
        let mut dir = Temp::new("cout");
        refuse_cache_dir_link(&mut dir, outside.path(), "SF-EXTERNAL-MARKER-a91c");
    }

    #[test]
    fn cache_dir_junction_into_secrets_is_not_read_or_written() {
        let mut dir = Temp::new("csec");
        let secret = dir.path().join("_工坊").join("机密");
        std::fs::create_dir_all(&secret).unwrap();
        refuse_cache_dir_link(&mut dir, &secret, "SF-SECRET-MARKER-b7e2");
    }

    #[test]
    fn junction_data_root_still_hits_its_own_cache() {
        let dir = Temp::new("root-real");
        let marker = "SF-ROOT-LINK-MARKER-c2";
        let app = dir.app();
        widen_flush_window(&app);
        write_rel(dir.path(), "书/项目.json", b"{}");
        write_rel(dir.path(), "书/正文/a.txt", marker.as_bytes());
        scan::snapshot(&app).unwrap();
        assert!(write_once(&app.startup_cache.shared));
        let link = std::env::temp_dir().join(format!(
            "sf-cache-rootlink-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        if let Err(reason) = junction(&link, dir.path()) {
            assert!(!reason.trim().is_empty(), "跳过链接测试必须写明原因");
            println!("SKIP 链接测试：{reason}");
            return;
        }
        let _guard = LinkGuard { path: link.clone() };
        let via = app_at(&link);
        let hit = load(&via).expect("数据根自身是联接时，缓存仍应命中");
        assert_eq!(hit["snapshot"]["files"]["书/正文/a.txt"]["text"], marker);
    }

    fn refuse_cache_dir_link(dir: &mut Temp, target: &Path, marker: &str) {
        let filler = "Y".repeat(marker.len());
        assert_ne!(filler, marker);
        let app = dir.app();
        widen_flush_window(&app);
        write_rel(dir.path(), "书/项目.json", b"{}");
        write_rel(dir.path(), "书/正文/a.txt", marker.as_bytes());
        scan::snapshot(&app).unwrap();
        assert!(write_once(&app.startup_cache.shared), "正常目录应能落盘");
        let planted = std::fs::read(cache_file(dir.path())).unwrap();
        assert!(planted.windows(marker.len()).any(|w| w == marker.as_bytes()));
        let mtimes = saved_mtimes(dir.path());
        std::fs::write(abs_of(dir.path(), "书/正文/a.txt"), filler.as_bytes()).unwrap();
        std::fs::remove_dir_all(dir.path().join("_工坊").join("缓存")).unwrap();
        std::fs::create_dir_all(target).unwrap();
        std::fs::write(target.join("启动快照.json"), &planted).unwrap();
        std::fs::write(target.join("标记.txt"), marker.as_bytes()).unwrap();
        let link = dir.path().join("_工坊").join("缓存");
        assert!(std::fs::symlink_metadata(&link).is_err(), "旧缓存目录未删掉");
        if let Err(reason) = junction(&link, target) {
            if gate::is_reparse(&link) {
                dir.keep_junction(link);
            }
            assert!(!reason.trim().is_empty(), "跳过链接测试必须写明原因");
            println!("SKIP 链接测试：{reason}");
            return;
        }
        dir.keep_junction(link);
        for (path, mtime) in &mtimes {
            set_mtime(path, *mtime);
        }
        // 这份字节若被读入就会命中并带出标记。未命中不能只是因为夹具对不上。
        assert_would_hit(dir.path(), &planted);
        invalidate(&app);
        match load(&app) {
            Ok(hit) => panic!("重解析缓存不应命中：{hit}"),
            Err(err) => {
                assert_eq!(err.error, "cache-miss");
                let rendered = format!("{} {}", err.error, err.message);
                assert!(!rendered.contains(marker), "读缓存带进了标记：{rendered}");
                assert!(!rendered.contains(&filler));
            }
        }
        scan::snapshot(&app).unwrap();
        let before = snapshot_tree(target);
        assert!(before.iter().all(|(_, bytes)| !contains_bytes(bytes, filler.as_bytes())));
        let at = log_len();
        assert!(!write_once(&app.startup_cache.shared), "重解析点上不应落盘");
        let after = snapshot_tree(target);
        assert_eq!(before, after, "缓存写改了联接目标");
        assert!(after.iter().all(|(_, bytes)| !contains_bytes(bytes, filler.as_bytes())), "快照正文写进了联接目标");
        let log = log_tail(at);
        assert!(log.contains("启动缓存落盘失败"), "{log}");
        assert!(!log.contains(marker), "{log}");
        assert!(!log.contains(&filler), "{log}");
    }

    fn flush_best_effort_ok(app: &App) -> bool {
        write_once(&app.startup_cache.shared)
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().method("GET").uri(path).header("host", "127.0.0.1").header("x-sf-token", "tok").body(Body::empty()).unwrap()
    }

    fn dir_names(snap: &Value, key: &str) -> Vec<String> {
        snap["dirs"][key].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
    }

    fn mutate_hash(parsed: &Value) -> Vec<u8> {
        let mut bad = parsed.clone();
        let files = bad["files"].as_object_mut().unwrap();
        let key = files.keys().next().unwrap().clone();
        files[&key]["hash"] = json!("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        serde_json::to_vec(&bad).unwrap()
    }

    fn app_at(root: &Path) -> App {
        let real = gate::try_realpath(root).unwrap_or_else(|| root.to_path_buf());
        App {
            token: "t".into(),
            port: 1,
            exe_dir: root.to_path_buf(),
            data: Mutex::new(Some(crate::state::DataRoot { path: root.to_path_buf(), real })),
            lock: Mutex::new(None),
            cfg: Mutex::new(Value::Null),
            write_lock: Mutex::new(()),
            app_override: None,
            shell: false,
            origins: std::collections::HashSet::new(),
            hosts: std::collections::HashSet::new(),
            startup_cache: Cache::new(),
        }
    }

    struct Fp {
        digest: String,
        files: usize,
        dirs: usize,
        bytes: u64,
    }

    fn fingerprint_via_node(snap: &Value) -> Fp {
        let path = std::env::temp_dir().join(format!("sf-cache-fp-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_vec(snap).unwrap()).unwrap();
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = "import {fingerprintSnapshot} from './tools/startup_perf.mjs'; import fs from 'node:fs'; const snap = JSON.parse(fs.readFileSync(process.argv[1], 'utf8')); const fp = fingerprintSnapshot(snap); process.stdout.write(JSON.stringify({digest:fp.digest,files:fp.files,dirs:fp.dirs,bytes:fp.bytes}));";
        let output = std::process::Command::new("node")
            .current_dir(&repo)
            .arg("--input-type=module")
            .arg("-e")
            .arg(script)
            .arg(&path)
            .output()
            .expect("node");
        let _ = std::fs::remove_file(&path);
        assert!(output.status.success(), "node fingerprint failed: {}", String::from_utf8_lossy(&output.stderr));
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        Fp {
            digest: v["digest"].as_str().unwrap().to_string(),
            files: v["files"].as_u64().unwrap() as usize,
            dirs: v["dirs"].as_u64().unwrap() as usize,
            bytes: v["bytes"].as_u64().unwrap(),
        }
    }

    fn file_hash_blob(snap: &Value) -> String {
        let mut lines = Vec::new();
        for (key, file) in snap["files"].as_object().unwrap() {
            lines.push(format!("{key}\t{}\t{}", file["hash"].as_str().unwrap(), file["bytes"]));
        }
        lines.sort();
        util::sha256_hex(lines.join("\n").as_bytes())
    }

    struct LinkGuard {
        path: PathBuf,
    }

    impl Drop for LinkGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.path);
        }
    }

    fn assert_would_hit(root: &Path, bytes: &[u8]) {
        let real = gate::try_realpath(root).unwrap_or_else(|| root.to_path_buf());
        let view = parse_envelope(bytes, &root_id(&real)).unwrap_or_else(|e| panic!("夹具缓存应能解析：{}", e.message));
        confirm(root, &real, &view).unwrap_or_else(|e| panic!("夹具应与磁盘元数据一致，否则测试是空的：{}", e.message));
    }

    fn saved_mtimes(root: &Path) -> Vec<(PathBuf, SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(cur) = stack.pop() {
            let rel = cur.strip_prefix(root).ok().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            if rel == "_工坊/缓存" || rel.starts_with("_工坊/缓存/") {
                continue;
            }
            if cur != root && gate::is_reparse(&cur) {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&cur) {
                if let Ok(mtime) = meta.modified() {
                    out.push((cur.clone(), mtime));
                }
            }
            if !cur.is_dir() {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&cur) {
                for ent in rd.flatten() {
                    stack.push(ent.path());
                }
            }
        }
        out
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap_or_else(|e| panic!("打开 {} 以恢复 mtime 失败：{e}", path.display()));
        file.set_modified(mtime).unwrap_or_else(|e| panic!("恢复 mtime {} 失败：{e}", path.display()));
    }

    fn snapshot_tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let rd = std::fs::read_dir(dir).unwrap();
            for ent in rd {
                let path = ent.unwrap().path();
                let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                if gate::is_reparse(&path) {
                    out.push((rel, b"<reparse>".to_vec()));
                    continue;
                }
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    walk(&path, root, out);
                } else if meta.is_file() {
                    out.push((rel, std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
    }

    fn junction(link: &Path, target: &Path) -> Result<(), String> {
        if std::fs::symlink_metadata(link).is_ok() {
            return Err(format!("链接已存在：{}", link.display()));
        }
        let output = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .map_err(|e| format!("无法启动 mklink：{e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let detail = format!("{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        Err(format!("mklink /J 失败，状态 {}。权限不足或不支持目录联接。{detail}", output.status))
    }
}
