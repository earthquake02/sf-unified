//! Windows DPAPI（CryptProtectData / CryptUnprotectData）。
//!
//! 绑定当前 Windows 账号（用户作用域），附加熵 `storyforge-secrets-v1`：与
//! `app/server/sf_server.py` 的 `dpapi()` 同口径，同账号同机两边密文互解。
//! 直接调系统库，不引第三方依赖。

use std::ffi::c_void;
use std::ptr;

pub const ENTROPY: &[u8] = b"storyforge-secrets-v1";
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        p_data_in: *const DataBlob,
        sz_data_descr: *const u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *const c_void,
        p_prompt_struct: *const c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        p_data_in: *const DataBlob,
        ppsz_data_descr: *mut *mut u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *const c_void,
        p_prompt_struct: *const c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
}

pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    call(true, data)
}

pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    call(false, data)
}

fn call(protect: bool, data: &[u8]) -> Result<Vec<u8>, String> {
    // 空明文也要给一个有效指针（与 Python create_string_buffer(1) 同口径）。
    let mut plain = if data.is_empty() { vec![0u8] } else { data.to_vec() };
    let mut entropy = ENTROPY.to_vec();
    let din = DataBlob { cb_data: data.len() as u32, pb_data: plain.as_mut_ptr() };
    let ent = DataBlob { cb_data: entropy.len() as u32, pb_data: entropy.as_mut_ptr() };
    let mut out = DataBlob { cb_data: 0, pb_data: ptr::null_mut() };
    let ok = unsafe {
        if protect {
            CryptProtectData(&din, ptr::null(), &ent, ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(
                &din,
                ptr::null_mut(),
                &ent,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 || out.pb_data.is_null() {
        return Err("DPAPI 调用失败".into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec() };
    unsafe {
        LocalFree(out.pb_data as *mut c_void);
    }
    Ok(bytes)
}
