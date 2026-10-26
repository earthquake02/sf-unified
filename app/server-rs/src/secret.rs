//! 机密（DPAPI）：`_工坊/机密/<name>.dat`，文件头 `SFSEC1\n`，密文与 Python 现实现互解。
//! CAS 按**明文**指纹比——密文每次封装都不同，不能拿密文比。

use std::path::{Path, PathBuf};

use crate::dpapi;
use crate::gate::{self, Bad};

pub const MAGIC: &[u8] = b"SFSEC1\n";
const UNREADABLE: &str = "机密文件解不开：它由另一个 Windows 账号（或另一台电脑）加密，也可能已损坏。\
请重新填写 API Key——原密文会被移入回收站保留，不静默销毁。";

pub fn path(root: &Path, real_root: &Path, name: &str) -> Result<PathBuf, Bad> {
    gate::secret_path(root, real_root, name)
}

/// 读密文并解封；文件不存在 → None；存在但解不开 → 409 secret-unreadable。
pub fn plain(target: &Path) -> Result<Option<Vec<u8>>, Bad> {
    if target.is_dir() {
        return Err(gate::bad(400, "is-dir", "目标是文件夹，不是文件"));
    }
    let blob = match std::fs::read(target) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(gate::io_bad(e)),
    };
    if !blob.starts_with(MAGIC) {
        return Err(gate::bad(409, "secret-unreadable", "机密文件不是本程序写出的密文（可能被外部程序改过）"));
    }
    match dpapi::unprotect(&blob[MAGIC.len()..]) {
        Ok(plain) => Ok(Some(plain)),
        Err(_) => Err(gate::bad(409, "secret-unreadable", UNREADABLE)),
    }
}

/// 文件头 + DPAPI 封装。封装失败不动调用方盘上的旧文件。
pub fn seal(plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut blob = Vec::with_capacity(MAGIC.len() + plain.len() + 64);
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&dpapi::protect(plain)?);
    Ok(blob)
}
