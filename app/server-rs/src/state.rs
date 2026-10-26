use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::lock::{HeldLock, LockInfo};

pub const VER: &str = "1.2";
pub const APP_TITLE: &str = "故事工坊";
pub const PING_MARK: &str = "SF-LAUNCHER-OK";
pub const CFG_NAME: &str = "故事工坊设置.json";
pub const DEFAULT_DATA_NAME: &str = "故事工坊数据";
pub const PORT_LO: u16 = 8788;
pub const PORT_HI: u16 = 8820;
pub const MAX_BODY: usize = 64 * 1024 * 1024;
pub const MAX_READ: u64 = 64 * 1024 * 1024;
pub const MAX_PATH: usize = 255;
pub const MAX_SECRET: usize = 4 * 1024 * 1024;

pub struct DataRoot {
    pub path: PathBuf,
    pub real: PathBuf,
}

pub struct App {
    pub token: String,
    pub port: u16,
    pub exe_dir: PathBuf,
    pub data: Mutex<Option<DataRoot>>,
    pub lock: Mutex<Option<HeldLock>>,
    pub cfg: Mutex<Value>,
    pub write_lock: Mutex<()>,
    pub app_override: Option<PathBuf>,
    pub shell: bool,
    pub origins: HashSet<String>,
    pub hosts: HashSet<String>,
}

impl App {
    pub fn data_snapshot(&self) -> Option<(PathBuf, PathBuf)> {
        let guard = self.data.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|d| (d.path.clone(), d.real.clone()))
    }

    pub fn lock_snapshot(&self) -> Option<LockInfo> {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|held| held.info.clone())
    }

    pub fn install_data(&self, path: PathBuf) {
        let real = crate::gate::try_realpath(&path).unwrap_or_else(|| path.clone());
        let mut guard = self.data.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(DataRoot { path, real });
    }

    pub fn persist_data_root(&self, path: &Path) -> Result<(), String> {
        let mut current = self.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = current.clone();
        let map = next.as_object_mut().ok_or_else(|| "配置不是对象".to_string())?;
        map.insert("dataRoot".into(), Value::String(path.to_string_lossy().replace('/', "\\")));
        map.insert("ver".into(), Value::from(1));
        crate::save_cfg(&self.exe_dir.join(CFG_NAME), &next)?;
        *current = next;
        Ok(())
    }
}
