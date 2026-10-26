//! 速记体验重做票04：全局「快速记录」快捷键（默认右 Ctrl 单击＝按下即松开、期间未按其他键）。
//! Windows 低级键盘钩子（WH_KEYBOARD_LL）观测系统全部按键：RegisterHotKey 注册不了裸修饰键，
//! 只有钩子能做「tap」判定；只观测不吞键（恒 CallNextHookEx 放行），右 Ctrl 与其他键的组合用法
//! 不受影响；注入事件忽略（LLKHF_INJECTED，防回馈环）。触发＝主线程调 shell::toggle_quick_window
//! （与托盘左键同一开关：快记窗在鼠标处弹出/再按收起）。
//! 配置＝数据夹 `_工坊/settings.json` 的 `quickHotkey`（KeyboardEvent.code）与 `quickHotkeyEnabled`（缺省 true；false＝不触发）。code 与设置页
//! HK_SUPPORTED 同一集合：左右 Ctrl/Alt/Shift/Win、A–Z、0–9、F1–F12）；缺省 ControlRight。
//! 独立线程 1.5s 轮询文件 mtime 热生效——数据夹可后装/可换，每次经 ShellState.http 快照现取。
//! 本文件沿用 shell.rs 的裸 extern "system" FFI 风格（不引 windows crate，零新依赖）。

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tauri::Manager;

use crate::shell::ShellState;

/* Win32 常量（WinUser.h）：钩子/消息/虚拟键码，仅用到的子集。 */
const WH_KEYBOARD_LL: i32 = 13;
const HC_ACTION: i32 = 0;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const LLKHF_INJECTED: u32 = 0x10;
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4; // 左 Alt
const VK_RMENU: u16 = 0xA5; // 右 Alt
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
const VK_F1: u16 = 0x70;

/// 默认快捷键：右 Ctrl（先生裁定：仅右 Ctrl，左 Ctrl 不触发）。
pub(crate) const DEFAULT_CODE: &str = "ControlRight";

#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    _pad1: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    _pad2: u32,
    x: i32,
    y: i32,
}

#[repr(C)]
struct KbdLl {
    vk: u32,
    scan: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowsHookExW(hook: i32, proc: isize, module: isize, thread: u32) -> isize;
    fn CallNextHookEx(hook: isize, code: i32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
}

/* tap 判定状态机（纯逻辑，可单测）：修饰键配置在按下时 armed、期间任何其他键按下则 dirty，
   松开时干净才算触发——右 Ctrl+C 之类的组合不误触；非修饰键配置在 keydown 直接触发。
   同键连续 down（自动重复）不重复计按。 */
#[derive(Default)]
pub(crate) struct TapState {
    armed: Option<u16>,
    dirty: bool,
    last_down: Option<u16>,
}

impl TapState {
    /// 键按下；返回 true＝触发快捷键（仅非修饰键配置会在此触发）。
    pub(crate) fn on_keydown(&mut self, vk: u16, spec_vk: u16, spec_is_mod: bool) -> bool {
        if self.last_down == Some(vk) {
            return false; // 自动重复
        }
        self.last_down = Some(vk);
        if spec_is_mod {
            if vk == spec_vk {
                self.armed = Some(vk);
                self.dirty = false;
            } else if self.armed.is_some() {
                self.dirty = true;
            }
            false
        } else {
            vk == spec_vk
        }
    }
    /// 键松开；返回 true＝触发快捷键（仅修饰键配置的干净 tap 会在此触发）。
    pub(crate) fn on_keyup(&mut self, vk: u16, spec_vk: u16, spec_is_mod: bool) -> bool {
        self.last_down = None;
        if spec_is_mod && self.armed == Some(vk) {
            let fire = vk == spec_vk && !self.dirty;
            self.armed = None;
            self.dirty = false;
            fire
        } else {
            false
        }
    }
    /// 配置变更后清态（避免旧 armed 悬空）。
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// code→(VK 码, 是否修饰键)；支持集合与设置页 HK_SUPPORTED 同步，集合外返回 None。
pub(crate) fn vk_of_code(code: &str) -> Option<(u16, bool)> {
    let (vk, is_mod) = match code {
        "ControlRight" => (VK_RCONTROL, true),
        "ControlLeft" => (VK_LCONTROL, true),
        "AltRight" => (VK_RMENU, true),
        "AltLeft" => (VK_LMENU, true),
        "ShiftRight" => (VK_RSHIFT, true),
        "ShiftLeft" => (VK_LSHIFT, true),
        "MetaLeft" => (VK_LWIN, true),
        "MetaRight" => (VK_RWIN, true),
        _ => {
            let b = code.as_bytes();
            if code.len() == 4 && code.starts_with("Key") && b[3].is_ascii_uppercase() {
                (b[3] as u16, false) // 'A'..'Z' 恰为 VK_A..VK_Z
            } else if code.len() == 6 && code.starts_with("Digit") && b[5].is_ascii_digit() {
                (b[5] as u16, false) // '0'..'9' 恰为 VK_0..VK_9
            } else if let Some(n) = code.strip_prefix('F') {
                let n: u16 = n.parse().ok()?;
                if !(1..=12).contains(&n) {
                    return None;
                }
                (VK_F1 + n - 1, false)
            } else {
                return None;
            }
        }
    };
    Some((vk, is_mod))
}

struct Shared {
    spec: (u16, bool),
    tap: TapState,
    enabled: bool,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

fn shared() -> &'static Mutex<Shared> {
    SHARED.get_or_init(|| {
        let spec = vk_of_code(DEFAULT_CODE).expect("默认码必在支持集合");
        Mutex::new(Shared { spec, tap: TapState::default(), enabled: true })
    })
}

extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HC_ACTION {
        let msg = wparam as u32;
        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        let up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
        if down || up {
            let ks = unsafe { &*(lparam as *const KbdLl) };
            if ks.flags & LLKHF_INJECTED == 0 {
                let mut fire = false;
                if let Ok(mut sh) = shared().lock() {
                    if sh.enabled {
                        let (vk, is_mod) = sh.spec;
                        fire = if down {
                            sh.tap.on_keydown(ks.vk as u16, vk, is_mod)
                        } else {
                            sh.tap.on_keyup(ks.vk as u16, vk, is_mod)
                        };
                    } else {
                        sh.tap.reset(); // 停用期间清态，避免再启用时吃到旧 armed
                    }
                }
                if fire {
                    fire_toggle();
                }
            }
        }
    }
    unsafe { CallNextHookEx(0, code, wparam, lparam) } // 恒放行：不吞键
}

fn fire_toggle() {
    if let Some(app) = APP.get().cloned() {
        let inner = app.clone();
        let _ = app.run_on_main_thread(move || crate::shell::toggle_quick_window(&inner));
    }
}

/// 装钩子（幂等：二启不装第二个）＋配置轮询。壳 setup 成功后调用一次；失败只记日志不拖死主窗。
pub(crate) fn start(app: &tauri::AppHandle) {
    if APP.set(app.clone()).is_err() {
        return;
    }
    std::thread::spawn(|| unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        if module == 0 {
            crate::shell::log_line("[SF] hotkey off：模块句柄取不到，全局快捷键未装（速记仍可托盘左键）");
            return;
        }
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc as usize as isize, module, 0);
        if hook == 0 {
            crate::shell::log_line("[SF] hotkey off：低级键盘钩子安装失败（速记仍可托盘左键）");
            return;
        }
        crate::shell::log_line("[SF] hotkey ready：全局快速记录快捷键（默认右 Ctrl 单击；配置＝quickHotkey／quickHotkeyEnabled）");
        let mut msg = Msg { hwnd: 0, message: 0, _pad1: 0, wparam: 0, lparam: 0, time: 0, _pad2: 0, x: 0, y: 0 };
        loop {
            // 0＝WM_QUIT、-1＝错误：都结束钩子线程（进程退出时随进程收尾，不必显式 Unhook）
            if GetMessageW(&mut msg, 0, 0, 0) <= 0 {
                break;
            }
        }
    });
    std::thread::spawn(poll_settings);
}

/// 1.5s 轮询设置文件 mtime：变了就读 quickHotkey 热换键（坏值不改，保持当前生效键）。
fn poll_settings() {
    let mut last_mtime: Option<Option<std::time::SystemTime>> = None;
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        let Some(app) = APP.get().cloned() else { continue };
        let Some(state) = app.try_state::<ShellState>() else { continue };
        let Some(http) = state.http.lock().unwrap_or_else(|e| e.into_inner()).clone() else { continue };
        let Some((root, _real)) = http.data_snapshot() else { continue };
        let path = root.join("_工坊").join("settings.json");
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let mtime = meta.modified().ok();
        if last_mtime == Some(mtime) {
            continue; // 没变（含 None==None：取不到 mtime 的文件只在首遇时读一次）
        }
        last_mtime = Some(mtime);
        let cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        let code = cfg
            .as_ref()
            .and_then(|v| v.get("quickHotkey").and_then(|x| x.as_str()).map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_CODE.to_owned());
        // 缺字段＝启用（兼容旧档）；仅显式 false 才停用
        let enabled = cfg
            .as_ref()
            .and_then(|v| v.get("quickHotkeyEnabled"))
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let Some(spec) = vk_of_code(&code) else { continue };
        if let Ok(mut sh) = shared().lock() {
            if sh.spec != spec || sh.enabled != enabled {
                sh.spec = spec;
                sh.enabled = enabled;
                sh.tap.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vk(code: &str) -> (u16, bool) {
        vk_of_code(code).expect("测试码都在支持集合内")
    }

    /* code→VK 映射：默认右 Ctrl、左右修饰键、字母数字、F 键与集合外拒绝。 */
    #[test]
    fn vk_mapping() {
        assert_eq!(vk("ControlRight"), (0xA3, true));
        assert_eq!(vk("ControlLeft"), (0xA2, true));
        assert_eq!(vk("AltRight"), (0xA5, true));
        assert_eq!(vk("MetaLeft"), (0x5B, true));
        assert_eq!(vk("KeyQ"), (b'Q' as u16, false));
        assert_eq!(vk("Digit7"), (b'7' as u16, false));
        assert_eq!(vk("F1"), (0x70, false));
        assert_eq!(vk("F12"), (0x7B, false));
        for bad in ["", "Control", "keyQ", "Keyq", "Digit", "Digit99", "F0", "F13", "Space", "Comma", "ControlRight "].iter() {
            assert!(vk_of_code(bad).is_none(), "集合外应拒绝：{bad}");
        }
        assert_eq!(DEFAULT_CODE, "ControlRight");
    }

    /* tap 语义：右 Ctrl 干净单击触发；按下期间夹其他键（组合用法）不触发；左 Ctrl 永不触发。 */
    #[test]
    fn tap_right_ctrl_clean_only() {
        let (spec_vk, _) = vk("ControlRight");
        let mut t = TapState::default();
        assert!(!t.on_keydown(0xA3, spec_vk, true));
        assert!(t.on_keyup(0xA3, spec_vk, true), "干净 tap 触发");

        t.reset();
        assert!(!t.on_keydown(0xA3, spec_vk, true));
        assert!(!t.on_keydown(0x51, spec_vk, true), "右Ctrl+Q（组合）不触发");
        assert!(!t.on_keyup(0xA3, spec_vk, true), "期间按过别的键＝脏 tap 不触发");
        assert!(!t.on_keyup(0x51, spec_vk, true));

        t.reset();
        assert!(!t.on_keydown(0xA2, spec_vk, true), "左 Ctrl 不 arm");
        assert!(!t.on_keyup(0xA2, spec_vk, true), "左 Ctrl 不触发");
        assert!(!t.on_keyup(0xA3, spec_vk, true), "无 armed 的右 Ctrl 松开也不触发");

        // 自动重复：同键连续 down 只算一次按下
        t.reset();
        assert!(!t.on_keydown(0xA3, spec_vk, true));
        assert!(!t.on_keydown(0xA3, spec_vk, true));
        assert!(t.on_keyup(0xA3, spec_vk, true), "重复 down 不破坏干净 tap");
    }

    /* 非修饰键配置（如 F9）：keydown 直接触发，keyup 不触发。 */
    #[test]
    fn tap_non_modifier_fires_on_down() {
        let (spec_vk, is_mod) = vk("F9");
        let mut t = TapState::default();
        assert!(t.on_keydown(0x78, spec_vk, is_mod));
        assert!(!t.on_keyup(0x78, spec_vk, is_mod));
        t.reset();
        assert!(!t.on_keydown(0x79, spec_vk, is_mod), "别的键不触发");
    }
}
