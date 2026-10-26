//! 指纹与随机串：几处端点共用，不在各模块各写一遍。

use sha2::{Digest, Sha256};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn sha256_hex(data: &[u8]) -> String {
    let dig = Sha256::digest(data);
    let mut s = String::with_capacity(dig.len() * 2);
    for b in dig {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// `secrets.token_hex(n)`：2n 位小写十六进制。
pub fn rand_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("系统随机数不可用");
    let mut s = String::with_capacity(bytes * 2);
    for b in buf {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}
