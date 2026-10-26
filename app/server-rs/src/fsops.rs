//! 写盘原语：同目录临时文件 + 替换，以及回收站落点。
//! 口径对照 `app/server/sf_server.py` 的 `stage_write / replace_retry / atomic_write / swap_write / trash_target`。
//! 机密的原子写与强制覆盖走这里；票 03 的普通文件写入与删除复用同一套。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util;

pub const TMP_PREFIX: &str = ".sf-tmp-";
pub const WS_NAME: &str = "_工坊";
pub const TRASH_NAME: &str = "回收站";

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

#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTime);
}

/// 本地时间 `%Y%m%d-%H%M%S`（回收站批次戳，和 Python 现实现同格式）。
pub fn stamp() -> String {
    let mut st = SystemTime { year: 0, month: 0, day_of_week: 0, day: 0, hour: 0, minute: 0, second: 0, milliseconds: 0 };
    unsafe {
        GetLocalTime(&mut st);
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.year, st.month, st.day, st.hour, st.minute, st.second
    )
}

/// 把 data 写进同目录 `.sf-tmp-<pid>-<hex>-<原名>` 并 fsync，返回临时文件路径。
pub fn stage_write(target: &Path, data: &[u8]) -> std::io::Result<PathBuf> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = target.with_file_name(format!("{TMP_PREFIX}{}-{}-{name}", std::process::id(), util::rand_hex(4)));
    let mut file = std::fs::File::create(&tmp)?;
    let written = file.write_all(data).and_then(|_| file.flush()).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// `os.replace` 的 Windows 加固：目标被别的句柄打开时短暂退避重试（BG-135）。
pub fn replace_retry(src: &Path, dst: &Path) -> std::io::Result<()> {
    for i in 0..5u64 {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if crate::gate::is_busy(&e) && i < 4 => {
                std::thread::sleep(Duration::from_millis(20 * (i + 1)));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("replace_retry 循环最多四次重试，末次必返回")
}

/// 同目录 tmp 写入 → fsync → 替换；失败不留 tmp。
pub fn atomic_write(target: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = stage_write(target, data)?;
    match replace_retry(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 「旧内容进回收站 + 新内容上位」（机密强制覆盖用）。
/// ① 先写 tmp，失败什么都不动；② 旧文件移入回收站，失败就删 tmp、旧文件原位不动；
/// ③ tmp 改名到位，失败时旧文件已在回收站（可回溯）。
pub fn swap_write(target: &Path, data: &[u8], trash_dest: &Path) -> std::io::Result<()> {
    let tmp = stage_write(target, data)?;
    let moved = (|| -> std::io::Result<()> {
        if let Some(parent) = trash_dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        replace_retry(target, trash_dest)
    })();
    if let Err(e) = moved {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    replace_retry(&tmp, target)
}

pub fn trash_root(root: &Path) -> PathBuf {
    root.join(WS_NAME).join(TRASH_NAME)
}

/// `_工坊/回收站/<时间戳-随机>/<原相对路径>`（保留层级，便于回溯）。
pub fn trash_target(root: &Path, target: &Path) -> PathBuf {
    let rel = target.strip_prefix(root).unwrap_or(target);
    trash_root(root)
        .join(format!("{}-{}", stamp(), util::rand_hex(2)))
        .join(rel)
}
