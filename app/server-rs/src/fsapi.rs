//! 普通文件改盘端点：write（tmp＋替换、按整文件指纹 CAS）、mkdir、delete（进数据夹内回收站）。
//! 口径对照 `app/server/sf_server.py` 的 _api_write / _api_mkdir / _api_delete / guard_protected。
//! 写盘原语在 fsops，路径闸在 gate，系统错误映射用 gate::io_bad（占用→忙，其余→500）。

use std::io::Read;
use std::path::{Component, Path};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{fsops, gate, state::App, util};

const LOCK_NAME: &str = "服务锁.json";

pub fn write_file(app: &App, raw: &[u8]) -> Result<Value, gate::Bad> {
    let body = parse_body(raw)?;
    let Some(text) = body.get("text").and_then(Value::as_str) else {
        return Err(gate::bad(400, "bad-body", "text 必须是字符串"));
    };
    let rel = body.get("path").and_then(Value::as_str).unwrap_or("");
    let (parts, target) = gate::resolve(app, rel)?;
    let Some((root, _)) = app.data_snapshot() else {
        return Err(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    guard_protected(&root, &target, Op::Write)?;
    let data = text.as_bytes().to_vec();
    let base = match body.get("baseHash") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(gate::bad(400, "bad-body", "baseHash 必须是字符串")),
    };
    // CAS 检查＋落盘整段串行：两个同 baseHash 的并发写不会双双通过（对照 Python WRITE_LOCK）。
    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    if is_dir(&target) {
        return Err(gate::bad(409, "conflict", "目标是文件夹，不能当文件写"));
    }
    let created = !path_exists(&target);
    if let Some(base) = &base {
        let cur = if path_exists(&target) && is_file(&target) {
            Some(sha256_file(&target).map_err(gate::io_bad)?)
        } else {
            None
        };
        if base.is_empty() {
            if let Some(cur) = cur {
                let e = gate::bad(409, "conflict", "目标已存在（baseHash 为空表示只新建）");
                return Err(gate::bad_extra(e, "actualHash", cur));
            }
        } else if cur.as_deref() != Some(base.as_str()) {
            let e = gate::bad(409, "conflict", "目标已被其他写入改动（baseHash 不匹配）");
            return Err(gate::bad_extra(e, "actualHash", cur.unwrap_or_default()));
        }
    }
    fsops::atomic_write(&target, &data).map_err(gate::io_bad)?;
    Ok(json!({
        "ok": true,
        "path": parts.join("/"),
        "bytes": data.len(),
        "hash": util::sha256_hex(&data),
        "created": created,
    }))
}

pub fn mkdir(app: &App, raw: &[u8]) -> Result<Value, gate::Bad> {
    let body = parse_body(raw)?;
    let rel = body.get("path").and_then(Value::as_str).unwrap_or("");
    let (parts, target) = gate::resolve(app, rel)?;
    let Some((root, _)) = app.data_snapshot() else {
        return Err(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    guard_protected(&root, &target, Op::Write)?;
    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    if path_exists(&target) {
        if !is_dir(&target) {
            return Err(gate::bad(409, "conflict", "同名文件已存在，不能建文件夹"));
        }
        return Ok(json!({ "ok": true, "path": parts.join("/"), "created": false }));
    }
    // Python 口径：建目录失败（含占用）统一 500 mkdir-failed，不说成忙。
    std::fs::create_dir_all(&target)
        .map_err(|e| gate::bad(500, "mkdir-failed", format!("建文件夹失败：{e}")))?;
    Ok(json!({ "ok": true, "path": parts.join("/"), "created": true }))
}

pub fn delete(app: &App, raw: &[u8]) -> Result<Value, gate::Bad> {
    let body = parse_body(raw)?;
    let rel = body.get("path").and_then(Value::as_str).unwrap_or("");
    let (parts, target) = gate::resolve(app, rel)?;
    let Some((root, _)) = app.data_snapshot() else {
        return Err(gate::bad(409, "no-data-root", "尚未设置数据文件夹"));
    };
    guard_protected(&root, &target, Op::Delete)?;
    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    if !path_exists(&target) {
        return Err(gate::bad(404, "not-found", "目标不存在"));
    }
    let dest = fsops::trash_target(&root, &target);
    let fail = |e: std::io::Error| gate::bad(500, "delete-failed", format!("删除失败（可能被占用）：{e}"));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    fsops::replace_retry(&target, &dest).map_err(fail)?;
    let dir = std::fs::metadata(&dest).map(|m| m.is_dir()).unwrap_or(false);
    Ok(json!({
        "ok": true,
        "path": parts.join("/"),
        "trashed": trashed_text(&root, &dest),
        "type": if dir { "dir" } else { "file" },
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Write,
    Delete,
}

/// 受保护目标：数据夹本体 / `_工坊` 本体 / 回收站整棵子树 / 服务锁（含其 tmp）/ 机密目录。
/// 机密目录删除放行（清空数据要能把机密一起送进回收站，可回溯、不物理销毁）。
/// 比较按 Windows 口径不区分大小写（Python PureWindowsPath 相等即如此）。
pub fn guard_protected(root: &Path, target: &Path, op: Op) -> Result<(), gate::Bad> {
    let ws = root.join(gate::WS_NAME);
    let trash = fsops::trash_root(root);
    let sec = ws.join(gate::SECRET_NAME);
    if eq_ci(target, root) {
        return Err(gate::bad(403, "protected", "不能改动数据文件夹本身"));
    }
    if eq_ci(target, &ws) {
        return Err(gate::bad(403, "protected", "不能改动 _工坊（服务自己的区域）"));
    }
    if eq_ci(target, &trash) || has_parent_ci(target, &trash) {
        return Err(gate::bad(403, "protected", "回收站是服务管理的区域，不可直接读写（清空留后续端点）"));
    }
    if eq_ci(target, &ws.join(LOCK_NAME)) || eq_ci(target, &ws.join(format!("{LOCK_NAME}.tmp"))) {
        return Err(gate::bad(403, "protected", "不能改动服务锁（拆了它＝放进第二个写者）"));
    }
    if eq_ci(target, &sec) || has_parent_ci(target, &sec) {
        if op == Op::Delete {
            return Ok(());
        }
        return Err(gate::bad(403, "protected", "机密目录只由 /api/secret/* 读写（要删请走删除端点，会进回收站）"));
    }
    Ok(())
}

/// 请求体一律是 JSON 对象；解析失败 → 400 bad-json。
/// Python 的 json 收下孤立代理对、把报错推迟到 text.encode（500 internal），这里同口径收口。
fn parse_body(raw: &[u8]) -> Result<Value, gate::Bad> {
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text,
        Err(_) => return Err(gate::bad(400, "bad-json", "请求体不是合法 JSON")),
    };
    match serde_json::from_str::<Value>(text) {
        Ok(value) => {
            if value.is_object() {
                Ok(value)
            } else {
                Err(gate::bad(400, "bad-json", "请求体必须是 JSON 对象"))
            }
        }
        Err(_) => {
            if has_lone_surrogate_escape(text) {
                Err(gate::bad(
                    500,
                    "internal",
                    "UnicodeEncodeError: 'utf-8' codec can't encode character: surrogates not allowed",
                ))
            } else {
                Err(gate::bad(400, "bad-json", "请求体不是合法 JSON"))
            }
        }
    }
}

/// 找不成对的 `\uD800`~`\uDFFF` 转义（成对代理对不算）。
fn has_lone_surrogate_escape(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 6 <= b.len() {
        if b[i] == b'\\' && b[i + 1] == b'u' {
            if let Some(v) = hex4(&b[i + 2..i + 6]) {
                if (0xD800..=0xDBFF).contains(&v) {
                    if i + 12 <= b.len() && b[i + 6] == b'\\' && b[i + 7] == b'u' {
                        if let Some(v2) = hex4(&b[i + 8..i + 12]) {
                            if (0xDC00..=0xDFFF).contains(&v2) {
                                i += 12;
                                continue;
                            }
                        }
                    }
                    return true;
                }
                if (0xDC00..=0xDFFF).contains(&v) {
                    return true;
                }
                i += 6;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn hex4(b: &[u8]) -> Option<u32> {
    if b.len() != 4 || !b.iter().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(b).ok()?, 16).ok()
}

/// 存在性判定与 Python `Path.exists/is_dir/is_file` 同为「吞掉 OSError 当不存在」。
fn path_exists(p: &Path) -> bool {
    std::fs::metadata(p).is_ok()
}

fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(p)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex_lower(&hash.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// 回执里的回收站相对路径：`_工坊/回收站/<时间戳-随机>/<原相对路径>`（正斜杠）。
fn trashed_text(root: &Path, dest: &Path) -> String {
    let rel = dest.strip_prefix(fsops::trash_root(root)).unwrap_or(dest);
    let mut out = format!("{}/{}", gate::WS_NAME, fsops::TRASH_NAME);
    for comp in rel.components() {
        if let Component::Normal(seg) = comp {
            out.push('/');
            out.push_str(&seg.to_string_lossy());
        }
    }
    out
}

/// Windows 口径的大小写不敏感路径相等（对照 PureWindowsPath ==）。
fn eq_ci(a: &Path, b: &Path) -> bool {
    normcase(a) == normcase(b)
}

/// target 是否在 dir 之下（不含相等；对照 `dir in target.parents`）。
fn has_parent_ci(target: &Path, dir: &Path) -> bool {
    target.ancestors().skip(1).any(|a| eq_ci(a, dir))
}

fn normcase(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(r"D:\数据夹")
    }

    #[test]
    fn protected_zone_shapes() {
        let root = root();
        let ws = root.join("_工坊");
        for (target, op, protected, why) in [
            (&root, Op::Write, true, "数据夹本体"),
            (&ws, Op::Delete, true, "_工坊 本体"),
            (&ws.join("回收站"), Op::Write, true, "回收站本体"),
            (&ws.join("回收站").join("20260101-000000-abcd").join("书"), Op::Delete, true, "回收站子树"),
            (&ws.join("服务锁.json"), Op::Write, true, "服务锁"),
            (&ws.join("服务锁.json.tmp"), Op::Write, true, "服务锁 tmp"),
            (&ws.join("机密"), Op::Write, true, "机密目录（写）"),
            (&ws.join("机密").join("AI密钥.dat"), Op::Write, true, "机密内文件（写）"),
            (&ws.join("机密"), Op::Delete, false, "机密目录（删，放行）"),
            (&ws.join("机密").join("AI密钥.dat"), Op::Delete, false, "机密内文件（删，放行）"),
            (&root.join("书").join("第1章"), Op::Write, false, "普通路径"),
        ] {
            let got = guard_protected(&root, target, op).is_err();
            assert_eq!(got, protected, "{why}");
        }
    }

    #[test]
    fn protected_compares_case_insensitive() {
        let root = root();
        // NTFS 不区分大小写：大小写变体若按字节比，就会绕开护栏改到真锁文件。
        let lock_variant = root.join("_工坊").join("服务锁.JSON");
        assert!(guard_protected(&root, &lock_variant, Op::Write).is_err());
        let trash_variant = root.join("_工坊").join("回收站").join("x");
        assert!(guard_protected(&root, &trash_variant, Op::Delete).is_err());
        // 普通路径的大小写变体不受牵连。
        assert!(guard_protected(&root, &root.join("服务锁.JSON"), Op::Write).is_ok());
    }
}
