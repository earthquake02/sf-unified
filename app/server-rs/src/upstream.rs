//! 出站 HTTP：不跟随重定向。
//!
//! 超时由调用方传入（套接字级，不是整次请求的总时限）。
//! 3xx 原样作为响应返回。状态码和错误文案由各端点自己映射，不要写进这里。
//! 不启用压缩，避免自动加 Accept-Encoding 和解压，与 Python urllib 默认一致。
//! 只用 HTTP/1.1。TLS 走系统 schannel（Windows 证书库），不另带加密提供者。

use std::time::Duration;

/// 造一个不跟随重定向的客户端。
///
/// `socket_timeout` 同时用于连接和读空闲，对应 Python `urlopen(..., timeout=)`
/// 的套接字超时：两次读之间没超过这个间隔就不会被掐，整次耗时可以更长。
/// 调用方要自己设 User-Agent；这里不设默认值，以免搜索和 AI 中转互相盖掉。
pub fn build(socket_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .connect_timeout(socket_timeout)
        .read_timeout(socket_timeout)
        .build()
        .expect("出站客户端初始化失败")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[tokio::test]
    async fn redirect_is_not_followed() {
        let dest = TcpListener::bind("127.0.0.1:0").unwrap();
        let dest_addr = dest.local_addr().unwrap();
        dest.set_nonblocking(true).unwrap();
        let src = TcpListener::bind("127.0.0.1:0").unwrap();
        let src_addr = src.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = src.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{dest_addr}/landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(resp.as_bytes());
        });

        let client = super::build(Duration::from_secs(2));
        let resp = client.get(format!("http://{src_addr}/")).send().await.unwrap();
        assert_eq!(resp.status(), 302, "3xx 应原样返回");
        std::thread::sleep(Duration::from_millis(300));
        assert!(dest.accept().is_err(), "重定向目标不应收到请求");
    }
}
