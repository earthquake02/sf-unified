//! 列举、单路径状态、整夹快照。只读，不写盘。
//! 口径对照 `app/server/sf_server.py` 的 `_api_list` / `_api_stat` / `_api_snapshot`。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use sha2::{Digest, Sha256};

use crate::gate::{self, bad, Bad, ScanFacts};
use crate::state::{App, MAX_READ};

const TMP_PREFIX: &str = ".sf-tmp-";
const SNAPSHOT_MAX_PREREAD_FILES: usize = 8000;
pub(crate) const SNAPSHOT_MAX_PREREAD_BYTES: u64 = 48 * 1024 * 1024;
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

#[derive(Clone, Copy)]
struct Limits {
    max_files: usize,
    max_bytes: u64,
}

const DEFAULT_LIMITS: Limits = Limits {
    max_files: SNAPSHOT_MAX_PREREAD_FILES,
    max_bytes: SNAPSHOT_MAX_PREREAD_BYTES,
};

#[derive(Clone, Debug)]
pub struct ScanMetrics {
    pub enum_ms: f64,
    pub read_ms: f64,
    pub assemble_ms: f64,
    pub serialize_ms: f64,
    pub files: usize,
    pub bytes: u64,
    pub workers: usize,
    pub conc: usize,
}

impl ScanMetrics {
    pub fn server_timing(&self) -> String {
        format!(
            "enum;dur={:.3}, read;dur={:.3}, assemble;dur={:.3}, serialize;dur={:.3}, files;desc=\"{}\", bytes;desc=\"{}\", conc;desc=\"{}\", workers;desc=\"{}\"",
            self.enum_ms,
            self.read_ms,
            self.assemble_ms,
            self.serialize_ms,
            self.files,
            self.bytes,
            self.conc,
            self.workers
        )
    }

    pub fn perf_line(&self) -> String {
        format!(
            "[SF] perf snapshot enum_ms={:.3} read_ms={:.3} assemble_ms={:.3} serialize_ms={:.3} files={} bytes={} workers={} conc={}",
            self.enum_ms,
            self.read_ms,
            self.assemble_ms,
            self.serialize_ms,
            self.files,
            self.bytes,
            self.workers,
            self.conc
        )
    }
}

pub struct SnapshotParts {
    pub body: Vec<u8>,
    pub metrics: ScanMetrics,
}

struct Built {
    value: Value,
    metrics: ScanMetrics,
}

struct Candidate {
    key: String,
    abs: PathBuf,
    meta_bytes: u64,
    mtime: f64,
}

struct ReadOk {
    text: String,
    hash: String,
    raw_len: u64,
    meta_unchanged: bool,
}

enum ReadOutcome {
    Ok(ReadOk),
    Fail,
}

trait FileRead: Sync {
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
}

struct FsRead;

impl FileRead for FsRead {
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

pub fn parse_scan_workers(raw: Option<&str>) -> usize {
    match raw.map(str::trim) {
        Some("1") => 1,
        Some("4") => 4,
        Some("8") => 8,
        Some("16") => 16,
        Some("32") => 32,
        _ => 8,
    }
}

fn scan_workers() -> usize {
    parse_scan_workers(std::env::var("SF_SCAN_WORKERS").ok().as_deref())
}

fn perf_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

pub fn snapshot(app: &App) -> Result<Value, Bad> {
    let built = build_snapshot(app, &FsRead, scan_workers(), DEFAULT_LIMITS)?;
    crate::startup_cache::remember_snapshot(app, &built.value);
    Ok(built.value)
}

pub fn snapshot_parts(app: &App) -> Result<SnapshotParts, Bad> {
    let built = build_snapshot(app, &FsRead, scan_workers(), DEFAULT_LIMITS)?;
    // 磁盘上的缓存副本放到调用方的关键路径之外；这里只换内存。
    crate::startup_cache::remember_snapshot(app, &built.value);
    let started = Instant::now();
    let body = serde_json::to_vec(&built.value).map_err(|e| bad(500, "internal", format!("JSON: {e}")))?;
    let mut metrics = built.metrics;
    metrics.serialize_ms = elapsed_ms(started);
    if perf_enabled(std::env::var("SF_PERF").ok().as_deref().map(str::trim)) {
        crate::shell::log_line(&metrics.perf_line());
    }
    Ok(SnapshotParts { body, metrics })
}

fn build_snapshot<R: FileRead>(
    app: &App,
    reader: &R,
    workers: usize,
    limits: Limits,
) -> Result<Built, Bad> {
    let Some((root, real_root)) = app.data_snapshot() else {
        return Err(bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    let workers = workers.max(1);
    let facts = ScanFacts::collect(&root);
    let mut retried = false;
    loop {
        let started = Instant::now();
        let (dirs, candidates) = enumerate(&root, &real_root, &facts);
        let enum_ms = elapsed_ms(started);
        let started = Instant::now();
        let (oks, conc) = read_candidates(reader, &candidates, workers, limits)?;
        let read_ms = elapsed_ms(started);
        let metadata_stable = oks.iter().flatten().all(|ok| ok.meta_unchanged);
        let started = Instant::now();
        let (files, file_count, byte_count) = commit(&candidates, oks, limits);
        let assemble_ms = elapsed_ms(started);
        if metadata_stable {
            return Ok(Built {
                value: json!({ "ok": true, "dirs": dirs, "files": files }),
                metrics: ScanMetrics {
                    enum_ms,
                    read_ms,
                    assemble_ms,
                    serialize_ms: 0.0,
                    files: file_count,
                    bytes: byte_count,
                    workers,
                    conc,
                },
            });
        }
        if retried {
            return Err(bad(409, "snapshot-changed", "扫描期间数据文件夹持续变化，请稍后重试"));
        }
        retried = true;
    }
}

fn enumerate(root: &Path, real_root: &Path, facts: &ScanFacts) -> (Map<String, Value>, Vec<Candidate>) {
    let mut dirs = Map::new();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let top = list_dir(root, real_root, root, false, &mut dirs);
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
            let all = list_dir(root, real_root, &e.abs, e.saw, &mut dirs);
            // 缓存目录是派生物。推进快照会自包含，目录清单和指纹都会漂。
            let kept: Vec<Node> = all
                .into_iter()
                .filter(|n| n.name != "机密" && n.name != "回收站" && n.name != "缓存")
                .collect();
            if let Some(key) = rel_key(root, &e.abs) {
                dirs.insert(key, Value::Array(kept.iter().map(entry_json).collect()));
            }
            for we in kept {
                if we.kind == "file" {
                    if WS_TOP.contains(&we.name.as_str()) {
                        consider(root, &we, facts, &mut seen, &mut candidates);
                    }
                } else if we.name == "共享设定" {
                    walk(root, real_root, &we.abs, we.saw, &mut dirs, facts, &mut seen, &mut candidates);
                }
            }
            continue;
        }
        if !book_names.contains(&e.name) {
            continue;
        }
        let book_items = list_dir(root, real_root, &e.abs, e.saw, &mut dirs);
        for be in book_items {
            if be.kind == "file" {
                if be.name == "项目.json" || be.name == "标签.json" {
                    consider(root, &be, facts, &mut seen, &mut candidates);
                }
            } else if BOOK_SUBS.contains(&be.name.as_str()) {
                walk(root, real_root, &be.abs, be.saw, &mut dirs, facts, &mut seen, &mut candidates);
            }
        }
    }
    (dirs, candidates)
}

/// 子目录后进先出，与 Python 快照的栈一致，预读额度才会切在同一批文件上。
fn walk(
    root: &Path,
    real_root: &Path,
    start: &Path,
    saw: bool,
    dirs: &mut Map<String, Value>,
    facts: &ScanFacts,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<Candidate>,
) {
    let mut stack = vec![(start.to_path_buf(), saw)];
    while let Some((cur, cur_saw)) = stack.pop() {
        let kids = list_dir(root, real_root, &cur, cur_saw, dirs);
        let mut later = Vec::new();
        for ke in kids {
            if ke.kind == "file" {
                consider(root, &ke, facts, seen, candidates);
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

fn consider(
    root: &Path,
    node: &Node,
    facts: &ScanFacts,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<Candidate>,
) {
    let Some(key) = rel_key(root, &node.abs) else { return };
    if seen.contains(&key) || node.name.starts_with(TMP_PREFIX) {
        return;
    }
    let low = node.name.to_lowercase();
    if !low.ends_with(".json") && !low.ends_with(".html") && !low.ends_with(".md") && !low.ends_with(".txt") {
        return;
    }
    if node.bytes > MAX_READ {
        return;
    }
    if facts.blocks_text_read(&node.abs, node.saw) {
        return;
    }
    seen.insert(key.clone());
    candidates.push(Candidate {
        key,
        abs: node.abs.clone(),
        meta_bytes: node.bytes,
        mtime: node.mtime,
    });
}

/// 预算按元数据大小沿原顺序判断。读失败或非法 UTF-8 不占额度；
/// 若因此空出额度，再补读后面的文件，包含结果与串行一致。
fn read_candidates<R: FileRead>(
    reader: &R,
    candidates: &[Candidate],
    workers: usize,
    limits: Limits,
) -> Result<(Vec<Option<ReadOk>>, usize), Bad> {
    let mut state = (0..candidates.len()).map(|_| None).collect::<Vec<_>>();
    let mut conc = 0usize;
    let mut finished = false;
    for _ in 0..=candidates.len() {
        let mut batch = Vec::new();
        let mut budget = Budget { files: 0, bytes: 0 };
        for (i, cand) in candidates.iter().enumerate() {
            if !budget_allows_lim(limits, budget.files, budget.bytes, cand.meta_bytes) {
                continue;
            }
            match &state[i] {
                Some(ReadOutcome::Fail) => {}
                Some(ReadOutcome::Ok(ok)) => {
                    budget.files += 1;
                    budget.bytes += ok.raw_len;
                }
                None => {
                    batch.push(i);
                    budget.files += 1;
                    budget.bytes += cand.meta_bytes;
                }
            }
        }
        if batch.is_empty() {
            finished = true;
            break;
        }
        conc = conc.max(read_parallel(reader, candidates, &batch, workers, &mut state));
        if batch.iter().any(|&i| state[i].is_none()) {
            return Err(bad(500, "snapshot-failed", "快照读取没有完成"));
        }
    }
    if !finished {
        return Err(bad(500, "snapshot-failed", "快照读取没有完成"));
    }
    let oks = state
        .into_iter()
        .map(|slot| match slot {
            Some(ReadOutcome::Ok(ok)) => Some(ok),
            _ => None,
        })
        .collect();
    Ok((oks, conc))
}

fn read_parallel<R: FileRead>(
    reader: &R,
    candidates: &[Candidate],
    batch: &[usize],
    workers: usize,
    state: &mut [Option<ReadOutcome>],
) -> usize {
    if batch.is_empty() {
        return 0;
    }
    let n = workers.max(1).min(batch.len());
    let next = AtomicUsize::new(0);
    let inflight = AtomicUsize::new(0);
    let max_inflight = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<ReadOutcome>>> = (0..batch.len()).map(|_| Mutex::new(None)).collect();
    let take = |job_i: usize| {
        let now = inflight.fetch_add(1, Ordering::AcqRel) + 1;
        max_inflight.fetch_max(now, Ordering::AcqRel);
        let outcome = read_one(reader, &candidates[batch[job_i]]);
        inflight.fetch_sub(1, Ordering::AcqRel);
        *slots[job_i].lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    };
    if n == 1 {
        for job_i in 0..batch.len() {
            take(job_i);
        }
    } else {
        std::thread::scope(|scope| {
            for _ in 0..n {
                scope.spawn(|| {
                    loop {
                        let job_i = next.fetch_add(1, Ordering::Relaxed);
                        if job_i >= batch.len() {
                            break;
                        }
                        take(job_i);
                    }
                });
            }
        });
    }
    for (job_i, &idx) in batch.iter().enumerate() {
        state[idx] = slots[job_i].lock().unwrap_or_else(|e| e.into_inner()).take();
    }
    max_inflight.load(Ordering::Acquire)
}

fn read_one<R: FileRead>(reader: &R, cand: &Candidate) -> ReadOutcome {
    let data = match reader.read_file(&cand.abs) {
        Ok(data) => data,
        Err(_) => return ReadOutcome::Fail,
    };
    let hash = sha256_hex(&data);
    let raw_len = data.len() as u64;
    // 读后复查普通元数据；同元数据未被辨识的改写及复查后的写入都由 CAS 拦截。
    let meta_unchanged = std::fs::metadata(&cand.abs)
        .map(|meta| meta.is_file() && meta.len() == cand.meta_bytes && mtime_of(&meta) == cand.mtime)
        .unwrap_or(false);
    match decode_utf8_sig(&data) {
        Ok(text) => ReadOutcome::Ok(ReadOk { text, hash, raw_len, meta_unchanged }),
        Err(()) => ReadOutcome::Fail,
    }
}

fn commit(candidates: &[Candidate], mut oks: Vec<Option<ReadOk>>, limits: Limits) -> (Map<String, Value>, usize, u64) {
    let mut files = Map::new();
    let mut budget = Budget { files: 0, bytes: 0 };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for (i, cand) in candidates.iter().enumerate() {
        if !budget_allows_lim(limits, budget.files, budget.bytes, cand.meta_bytes) {
            continue;
        }
        let Some(ok) = oks[i].take() else { continue };
        budget.files += 1;
        budget.bytes += ok.raw_len;
        count += 1;
        bytes += ok.raw_len;
        files.insert(
            cand.key.clone(),
            json!({
                "text": ok.text,
                "bytes": ok.raw_len,
                "hash": ok.hash,
                "mtime": cand.mtime,
            }),
        );
    }
    (files, count, bytes)
}

fn budget_allows(files: usize, bytes: u64, size: u64) -> bool {
    budget_allows_lim(DEFAULT_LIMITS, files, bytes, size)
}

fn budget_allows_lim(limits: Limits, files: usize, bytes: u64, size: u64) -> bool {
    files < limits.max_files && bytes.saturating_add(size) <= limits.max_bytes
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
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

pub(crate) fn enumerated_dirs(root: &Path, real_root: &Path) -> Map<String, Value> {
    let facts = ScanFacts::collect(root);
    enumerate(root, real_root, &facts).0
}

pub(crate) fn mtime_seconds(meta: &std::fs::Metadata) -> f64 {
    mtime_of(meta)
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

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

    #[test]
    fn scan_worker_values_fall_back_to_eight() {
        assert_eq!(parse_scan_workers(None), 8);
        for raw in ["1", "4", "8", "16", "32", " 8 ", "32\n"] {
            let n: usize = raw.trim().parse().unwrap();
            assert_eq!(parse_scan_workers(Some(raw)), n, "{raw}");
        }
        for raw in ["", "0", "2", "3", "7", "08", "64", "8.0", "abc", "-1"] {
            assert_eq!(parse_scan_workers(Some(raw)), 8, "{raw}");
        }
        assert!(!perf_enabled(None));
        assert!(!perf_enabled(Some("0")));
        assert!(perf_enabled(Some("1")));
    }

    #[test]
    fn parallel_read_stays_within_cap_and_actually_overlaps() {
        for workers in [4usize, 8, 16, 32] {
            let dir = Temp::new("cap");
            write_book(dir.path(), &[("项目.json", b"{}")]);
            let mut names = Vec::new();
            for i in 0..workers * 2 {
                names.push(format!("f{i:02}.txt"));
            }
            for name in &names {
                write_rel(dir.path(), &format!("书/正文/{name}"), b"x");
            }
            let probe = Probe::new(workers);
            let built = build_snapshot(&dir.app(), &probe, workers, DEFAULT_LIMITS).unwrap();
            assert!(!probe.overflow.load(Ordering::Acquire), "workers={workers}");
            assert_eq!(probe.max.load(Ordering::Acquire), workers, "workers={workers}");
            assert_eq!(built.metrics.conc, workers, "workers={workers}");
            assert_eq!(built.metrics.workers, workers, "workers={workers}");
            assert!(built.metrics.files >= workers, "workers={workers}");
        }
    }

    #[test]
    fn parallel_assembly_keeps_enumeration_order() {
        let dir = Temp::new("order");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/a.txt", b"A");
        write_rel(dir.path(), "书/正文/b.txt", b"B");
        write_rel(dir.path(), "书/正文/c.txt", b"C");
        write_rel(dir.path(), "书/正文/m/z.txt", b"MZ");
        write_rel(dir.path(), "书/正文/z/a.txt", b"ZA");
        let reverse = ReverseReader { next: AtomicUsize::new(2), lock: Mutex::new(()), cv: Condvar::new() };
        let snap = build_snapshot(&dir.app(), &reverse, 4, DEFAULT_LIMITS).unwrap().value;
        assert_eq!(snap["files"]["书/正文/a.txt"]["text"], "A");
        assert_eq!(snap["files"]["书/正文/b.txt"]["text"], "B");
        assert_eq!(snap["files"]["书/正文/c.txt"]["text"], "C");
        assert_eq!(snap["files"]["书/正文/m/z.txt"]["text"], "MZ");
        assert_eq!(snap["files"]["书/正文/z/a.txt"]["text"], "ZA");
        let serial = snapshot_value(&dir.app(), 1);
        let parallel = snapshot_value(&dir.app(), 8);
        assert_eq!(object_keys(&serial, "files"), object_keys(&parallel, "files"));
        assert_eq!(object_keys(&serial, "dirs"), object_keys(&parallel, "dirs"));
        assert_eq!(serial, parallel);
    }

    #[test]
    fn failed_read_and_bad_utf8_leave_siblings_and_free_budget() {
        let dir = Temp::new("utf");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/a.txt", &[0xff, 0xfe, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85]);
        write_rel(dir.path(), "书/正文/b.txt", b"01234567");
        write_rel(dir.path(), "书/正文/好.txt", "好".as_bytes());
        let bom = b"\xEF\xBB\xBF\xe4\xbd\xa0\xe5\xa5\xbd";
        write_rel(dir.path(), "书/正文/第一章.txt", bom);
        let limits = Limits { max_files: 8000, max_bytes: 10 };
        let snap = build_snapshot(&dir.app(), &FsRead, 8, limits).unwrap().value;
        let files = snap["files"].as_object().unwrap();
        assert!(files.contains_key("书/项目.json"));
        assert!(!files.contains_key("书/正文/a.txt"), "非法 UTF-8 不得进入快照");
        assert!(files.contains_key("书/正文/b.txt"), "失败文件不得占用预算");
        assert_eq!(files["书/正文/b.txt"]["text"], "01234567");
        assert!(!files.contains_key("书/正文/好.txt"), "字节预算仍要截住后面的文件");
        let full = snapshot_value(&dir.app(), 4);
        assert_eq!(full["files"]["书/正文/好.txt"]["text"], "好");
        assert_eq!(full["files"]["书/正文/第一章.txt"]["text"], "你好");
        assert_eq!(full["files"]["书/正文/第一章.txt"]["bytes"], bom.len());
        assert_eq!(full["files"]["书/正文/第一章.txt"]["hash"], sha256_hex(bom));
        assert!(!full["files"].as_object().unwrap().contains_key("书/正文/a.txt"));
        let again = snapshot_value(&dir.app(), 1);
        assert_eq!(object_keys(&full, "files"), object_keys(&again, "files"));
        assert_eq!(full["files"]["书/正文/第一章.txt"]["hash"], again["files"]["书/正文/第一章.txt"]["hash"]);
    }

    #[test]
    fn file_cap_skips_later_names_but_keeps_directory_entries() {
        let dir = Temp::new("capfiles");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/a.txt", b"a");
        write_rel(dir.path(), "书/正文/b.txt", b"b");
        let limits = Limits { max_files: 2, max_bytes: SNAPSHOT_MAX_PREREAD_BYTES };
        let snap = build_snapshot(&dir.app(), &FsRead, 8, limits).unwrap().value;
        assert_eq!(object_keys(&snap, "files"), vec!["书/正文/a.txt", "书/正文/b.txt"]);
        let names = dir_names(&snap, "书/正文");
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
    }

    #[test]
    fn filters_skip_temp_unknown_and_oversize_without_reading_them() {
        let dir = Temp::new("filter");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/keep.txt", b"keep");
        write_rel(dir.path(), "书/正文/note.bin", b"bin");
        write_rel(dir.path(), "书/正文/.sf-tmp-x.txt", b"tmp");
        let big = dir.path().join("书").join("正文").join("big.txt");
        std::fs::write(&big, b"x").unwrap();
        let big_file = std::fs::OpenOptions::new().write(true).open(&big).unwrap();
        big_file.set_len(MAX_READ + 1).unwrap();
        let seen = Seen::default();
        let snap = build_snapshot(&dir.app(), &seen, 4, DEFAULT_LIMITS).unwrap().value;
        assert!(!seen.saw_big.load(Ordering::Acquire), "超限文件不应读取");
        assert!(!seen.saw_tmp.load(Ordering::Acquire));
        assert!(!seen.saw_bin.load(Ordering::Acquire));
        let files = snap["files"].as_object().unwrap();
        assert_eq!(files["书/正文/keep.txt"]["text"], "keep");
        assert!(!files.contains_key("书/正文/note.bin"));
        assert!(!files.contains_key("书/正文/big.txt"));
        assert!(!files.contains_key("书/正文/.sf-tmp-x.txt"));
        let names = dir_names(&snap, "书/正文");
        assert!(names.contains(&"note.bin".to_string()));
        assert!(names.contains(&"big.txt".to_string()));
        assert!(!names.iter().any(|n| n.starts_with(".sf-tmp-")));
        let line = build_snapshot(&dir.app(), &FsRead, 8, DEFAULT_LIMITS).unwrap().metrics.perf_line();
        assert!(!line.contains("keep"));
        assert!(!line.contains("书"));
        let timing = build_snapshot(&dir.app(), &FsRead, 8, DEFAULT_LIMITS).unwrap().metrics.server_timing();
        assert!(timing.contains("read;dur="));
        assert!(timing.contains("conc;desc="));
        assert!(!timing.contains("keep"));
    }

    #[test]
    fn ordinary_snapshot_does_not_walk_parent_chains() {
        let dir = Temp::new("chain");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        std::fs::create_dir_all(dir.path().join("_工坊").join("机密")).unwrap();
        std::fs::write(dir.path().join("_工坊").join("机密").join("密钥.txt"), "secret-marker").unwrap();
        std::fs::write(dir.path().join("_工坊").join("settings.json"), b"{\"ok\":true}").unwrap();
        for i in 0..12 {
            write_rel(dir.path(), &format!("书/正文/n{i}.txt"), b"n");
        }
        gate::reset_chain_walks();
        let snap = snapshot_value(&dir.app(), 8);
        assert_eq!(gate::chain_walks(), 0, "普通快照不应逐文件查询父目录链");
        let blob = serde_json::to_string(&snap).unwrap();
        assert!(!blob.contains("secret-marker"), "机密目录正文不得进入快照");
        assert_eq!(snap["files"]["_工坊/settings.json"]["text"], "{\"ok\":true}");
        assert!(snap["files"].as_object().unwrap().keys().all(|k| !k.contains("机密")));
    }

    #[test]
    fn worker_panic_does_not_return_a_snapshot() {
        let dir = Temp::new("panic");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/ok.txt", b"ok");
        write_rel(dir.path(), "书/正文/boom.txt", b"boom");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = build_snapshot(&dir.app(), &Boom, 4, DEFAULT_LIMITS);
        }));
        assert!(caught.is_err(), "worker panic 必须让整次快照失败，而不是返回半截结果");
    }

    #[test]
    fn read_error_omits_one_file_only() {
        let dir = Temp::new("io");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/ok.txt", b"ok");
        write_rel(dir.path(), "书/正文/bad.txt", b"bad");
        let snap = build_snapshot(&dir.app(), &FailBad, 4, DEFAULT_LIMITS).unwrap().value;
        assert_eq!(snap["ok"], true);
        assert_eq!(snap["files"]["书/正文/ok.txt"]["text"], "ok");
        assert!(snap["files"].get("书/正文/bad.txt").is_none());
    }

    #[test]
    fn empty_root_snapshot_is_complete() {
        let dir = Temp::new("empty");
        std::fs::create_dir_all(dir.path()).unwrap();
        let built = build_snapshot(&dir.app(), &FsRead, 8, DEFAULT_LIMITS).unwrap();
        assert_eq!(snapshot(&dir.app()).unwrap(), built.value);
        assert_eq!(built.value["ok"], true);
        assert_eq!(built.metrics.files, 0);
        assert_eq!(built.metrics.bytes, 0);
        assert_eq!(built.metrics.conc, 0);
        assert_eq!(built.metrics.workers, 8);
        assert!(built.value["dirs"].as_object().unwrap().contains_key(""));
    }

    #[test]
    fn link_matrix_rejects_escape_secret_and_cache_targets() {
        let mut dir = Temp::new("link");
        let outside = Temp::new("link-out");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        write_rel(dir.path(), "书/正文/普通.txt", "ordinary-marker".as_bytes());
        write_rel(dir.path(), "书/正文/真/内文.txt", "inside-ok".as_bytes());
        std::fs::create_dir_all(dir.path().join("_工坊").join("机密")).unwrap();
        std::fs::write(dir.path().join("_工坊").join("机密").join("密码.txt"), "secret-marker").unwrap();
        std::fs::write(dir.path().join("_工坊").join("settings.json"), b"settings-ok").unwrap();
        std::fs::create_dir_all(outside.path()).unwrap();
        std::fs::write(outside.path().join("外文.txt"), "outside-marker").unwrap();
        let inner = dir.path().join("书").join("正文").join("内");
        let real = dir.path().join("书").join("正文").join("真");
        let escape = dir.path().join("书").join("正文").join("外");
        let steal = dir.path().join("书").join("正文").join("偷");
        let cache = dir.path().join("_工坊").join("缓存");
        let secret = dir.path().join("_工坊").join("机密");
        dir.junctions.push(inner.clone());
        dir.junctions.push(escape.clone());
        dir.junctions.push(steal.clone());
        dir.junctions.push(cache.clone());
        if let Err(reason) = junction(&inner, &real)
            .and_then(|_| junction(&escape, outside.path()))
            .and_then(|_| junction(&steal, &secret))
            .and_then(|_| junction(&cache, outside.path()))
        {
            assert!(!reason.trim().is_empty(), "跳过链接测试必须写明原因");
            println!("SKIP 链接测试：{reason}");
            return;
        }
        let snap = snapshot_value(&dir.app(), 8);
        let serial = snapshot_value(&dir.app(), 1);
        assert_eq!(object_keys(&snap, "files"), object_keys(&serial, "files"));
        assert_eq!(snap["files"]["书/正文/普通.txt"]["text"], "ordinary-marker");
        assert_eq!(snap["files"]["书/正文/真/内文.txt"]["text"], "inside-ok");
        assert_eq!(snap["files"]["书/正文/内/内文.txt"]["text"], "inside-ok");
        assert_eq!(snap["files"]["_工坊/settings.json"]["text"], "settings-ok");
        let body_names = dir_names(&snap, "书/正文");
        assert!(!body_names.iter().any(|n| n == "外" || n == "偷"), "逃逸链接和机密链接不得出现在目录里");
        let ws_names = dir_names(&snap, "_工坊");
        assert!(!ws_names.iter().any(|n| n == "缓存" || n == "机密"));
        let blob = serde_json::to_string(&snap).unwrap();
        assert!(!blob.contains("outside-marker"), "指向数据根外的链接被读进来了");
        assert!(!blob.contains("secret-marker"), "机密目录链接被读进来了");
        assert_eq!(snap, serial);
    }

    #[test]
    fn golden_corpus_matches_workers_and_ticket01_when_present() {
        let root = PathBuf::from(r"D:\sf-unified\.scratch\startup-cold-fastpath\runtime\corpus");
        if !root.is_dir() {
            println!("SKIP 黄金语料不在：{}", root.display());
            return;
        }
        let app = app_at(&root);
        let serial = build_snapshot(&app, &FsRead, 1, DEFAULT_LIMITS).unwrap();
        let parallel = build_snapshot(&app, &FsRead, 8, DEFAULT_LIMITS).unwrap();
        assert_eq!(object_keys(&serial.value, "files"), object_keys(&parallel.value, "files"));
        assert_eq!(object_keys(&serial.value, "dirs"), object_keys(&parallel.value, "dirs"));
        assert_eq!(serial.metrics.files, parallel.metrics.files);
        assert_eq!(serial.metrics.bytes, parallel.metrics.bytes);
        let serial_hash = file_hash_blob(&serial.value);
        let parallel_hash = file_hash_blob(&parallel.value);
        assert_eq!(serial_hash, parallel_hash, "workers=1 与默认并行的文件哈希不一致");
        let fp = fingerprint_via_node(&parallel.value);
        assert_eq!(fp.files, 5532, "files");
        assert_eq!(fp.dirs, 974, "dirs");
        assert_eq!(fp.bytes, 20_971_520, "bytes");
        assert_eq!(fp.digest, "2c49e2dda27fb614edd05ede2bacbb1f2ebf8267888e634d980de6b0c5e682fc");
    }

    struct Temp {
        path: PathBuf,
        junctions: Vec<PathBuf>,
    }

    impl Temp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sf-scan-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path, junctions: Vec::new() }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn app(&self) -> App {
            app_at(&self.path)
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
            origins: HashSet::new(),
            hosts: HashSet::new(),
            startup_cache: crate::startup_cache::Cache::new(),
        }
    }

    fn write_rel(root: &Path, rel: &str, body: &[u8]) {
        let path = root.join(rel.replace('/', "\\"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn write_book(root: &Path, files: &[(&str, &[u8])]) {
        for (name, body) in files {
            write_rel(root, &format!("书/{name}"), body);
        }
    }

    fn snapshot_value(app: &App, workers: usize) -> Value {
        build_snapshot(app, &FsRead, workers, DEFAULT_LIMITS).unwrap().value
    }

    fn object_keys(value: &Value, field: &str) -> Vec<String> {
        value[field].as_object().unwrap().keys().cloned().collect()
    }

    fn dir_names(snap: &Value, key: &str) -> Vec<String> {
        snap["dirs"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|ent| ent["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn file_hash_blob(snap: &Value) -> String {
        let mut lines = Vec::new();
        for (key, file) in snap["files"].as_object().unwrap() {
            lines.push(format!("{key}\t{}\t{}", file["hash"].as_str().unwrap(), file["bytes"]));
        }
        lines.sort();
        sha256_hex(lines.join("\n").as_bytes())
    }

    struct Probe {
        limit: usize,
        active: AtomicUsize,
        max: AtomicUsize,
        entered: AtomicUsize,
        overflow: AtomicBool,
        release: AtomicBool,
        lock: Mutex<()>,
        cv: Condvar,
    }

    impl Probe {
        fn new(limit: usize) -> Self {
            Self {
                limit,
                active: AtomicUsize::new(0),
                max: AtomicUsize::new(0),
                entered: AtomicUsize::new(0),
                overflow: AtomicBool::new(false),
                release: AtomicBool::new(false),
                lock: Mutex::new(()),
                cv: Condvar::new(),
            }
        }
    }

    impl FileRead for Probe {
        fn read_file(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
            let now = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.max.fetch_max(now, Ordering::AcqRel);
            if now > self.limit {
                self.overflow.store(true, Ordering::Release);
            }
            let n = self.entered.fetch_add(1, Ordering::AcqRel) + 1;
            let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            if n >= self.limit {
                self.release.store(true, Ordering::Release);
                self.cv.notify_all();
            }
            while !self.release.load(Ordering::Acquire) {
                guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            drop(guard);
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(b"x".to_vec())
        }
    }

    struct ReverseReader {
        next: AtomicUsize,
        lock: Mutex<()>,
        cv: Condvar,
    }

    impl FileRead for ReverseReader {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let idx = match name {
                "a.txt" if path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()) == Some("正文") => 0,
                "b.txt" => 1,
                "c.txt" => 2,
                _ => {
                    return Ok(std::fs::read(path)?);
                }
            };
            let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            while self.next.load(Ordering::Acquire) != idx {
                guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            if idx > 0 {
                self.next.store(idx - 1, Ordering::Release);
            }
            self.cv.notify_all();
            drop(guard);
            Ok(match idx {
                0 => b"A".to_vec(),
                1 => b"B".to_vec(),
                _ => b"C".to_vec(),
            })
        }
    }

    #[derive(Default)]
    struct Seen {
        saw_big: AtomicBool,
        saw_tmp: AtomicBool,
        saw_bin: AtomicBool,
    }

    impl FileRead for Seen {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "big.txt" {
                self.saw_big.store(true, Ordering::Release);
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "should-not-read"));
            }
            if name.starts_with(".sf-tmp-") {
                self.saw_tmp.store(true, Ordering::Release);
            }
            if name.ends_with(".bin") {
                self.saw_bin.store(true, Ordering::Release);
            }
            std::fs::read(path)
        }
    }

    struct Boom;

    impl FileRead for Boom {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            if path.file_name().and_then(|s| s.to_str()) == Some("boom.txt") {
                panic!("worker-read");
            }
            Ok(b"ok".to_vec())
        }
    }

    struct FailBad;

    impl FileRead for FailBad {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            if path.file_name().and_then(|s| s.to_str()) == Some("bad.txt") {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "read-failed"));
            }
            std::fs::read(path)
        }
    }

    struct RewriteAfterRead {
        target: PathBuf,
        replacement: &'static [u8],
        restore_mtime: bool,
        changed: AtomicBool,
    }

    impl FileRead for RewriteAfterRead {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            let data = std::fs::read(path)?;
            if path == self.target && !self.changed.swap(true, Ordering::AcqRel) {
                let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                std::fs::write(path, self.replacement)?;
                if self.restore_mtime {
                    if let Some(mtime) = modified {
                        let file = std::fs::OpenOptions::new().write(true).open(path)?;
                        file.set_modified(mtime)?;
                    }
                }
            }
            Ok(data)
        }
    }

    #[test]
    fn metadata_rewrite_after_read_is_rechecked_and_claimed_by_retry() {
        let dir = Temp::new("rewrite-after-read");
        write_book(dir.path(), &[("项目.json", b"{}")]);
        let target = dir.path().join("书").join("正文").join("a.txt");
        write_rel(dir.path(), "书/正文/a.txt", b"old");
        let reader = RewriteAfterRead {
            target,
            replacement: b"new-content",
            restore_mtime: false,
            changed: AtomicBool::new(false),
        };
        let snap = build_snapshot(&dir.app(), &reader, 1, DEFAULT_LIMITS).unwrap().value;
        assert!(reader.changed.load(Ordering::Acquire));
        assert_eq!(snap["files"]["书/正文/a.txt"]["text"], "new-content");
        assert_eq!(snap["files"]["书/正文/a.txt"]["bytes"], 11);
    }

    #[test]
    fn same_meta_rewrite_after_read_is_post_scan_and_cas_protects_it() {
        let dir = Temp::new("same-meta-after-read");
        let app = dir.app();
        write_book(dir.path(), &[("项目.json", b"{}")]);
        let target = dir.path().join("书").join("正文").join("a.txt");
        write_rel(dir.path(), "书/正文/a.txt", b"abcd");
        let reader = RewriteAfterRead {
            target: target.clone(),
            replacement: b"wxyz",
            restore_mtime: true,
            changed: AtomicBool::new(false),
        };
        let snap = build_snapshot(&app, &reader, 1, DEFAULT_LIMITS).unwrap().value;
        assert_eq!(snap["files"]["书/正文/a.txt"]["text"], "abcd");
        assert_eq!(std::fs::read(&target).unwrap(), b"wxyz");
        let body = serde_json::to_vec(&json!({
            "path": "书/正文/a.txt",
            "text": "window-version",
            "baseHash": snap["files"]["书/正文/a.txt"]["hash"].as_str().unwrap(),
        }))
        .unwrap();
        let err = crate::fsapi::write_file(&app, &body).unwrap_err();
        assert_eq!(err.error, "conflict");
        assert_eq!(std::fs::read(&target).unwrap(), b"wxyz");
    }

    struct Fp {
        digest: String,
        files: usize,
        dirs: usize,
        bytes: u64,
    }

    fn fingerprint_via_node(snap: &Value) -> Fp {
        let path = std::env::temp_dir().join(format!("sf-golden-{}.json", std::process::id()));
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
        let text = String::from_utf8(output.stdout).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        Fp {
            digest: v["digest"].as_str().unwrap().to_string(),
            files: v["files"].as_u64().unwrap() as usize,
            dirs: v["dirs"].as_u64().unwrap() as usize,
            bytes: v["bytes"].as_u64().unwrap(),
        }
    }

    fn junction(link: &Path, target: &Path) -> Result<(), String> {
        if link.exists() {
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
        let detail = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Err(format!("mklink /J 失败，状态 {}。权限不足或不支持目录联接。{detail}", output.status))
    }
}
