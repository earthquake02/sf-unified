//! 联网搜索。只请求固定搜索页，不跟随重定向，不读数据夹。
//! 口径源自 sf_server.py（Python 对照系已于票13 退役）；期望值冻结在下方测试里。

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::gate::{self, Bad};

const SEARCH_HITS: usize = 5;
const SEARCH_QUERY_MAX: usize = 200;
const SEARCH_TIMEOUT_SECS: u64 = 12;
const SEARCH_MAX_BYTES: usize = 1_000_000;
const SEARCH_URL: &str = "https://html.duckduckgo.com/html/?q=";
const SEARCH_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

/// `quote(safe='')`：保留 A-Za-z0-9 与 `-._~`，其余按 UTF-8 百分号编码（空格是 %20）。
const QUERY_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

struct Parts {
    scheme: String,
    netloc: String,
    query: String,
}

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

fn title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*class="result__a"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
            .expect("标题正则")
    })
}

fn snip_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*class="result__snippet"[^>]*>(.*?)</a>"#).expect("摘要正则")
    })
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("标签正则"))
}

fn ws_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("空白正则"))
}

/// 空、纯空白、首尾空白、控制字符、超长拒绝。DEL（127）不拒绝。盘符与 `..` 仍是搜索词。
fn search_query_check(raw: &str) -> Result<&str, &'static str> {
    if raw.trim().is_empty() {
        return Err("没有给出搜索词");
    }
    if raw != raw.trim() || raw.chars().any(|c| (c as u32) < 32) {
        return Err("搜索词不能查");
    }
    if raw.chars().count() > SEARCH_QUERY_MAX {
        return Err("搜索词太长");
    }
    Ok(raw)
}

fn search_text(fragment: &str, limit: usize) -> String {
    let stripped = tag_re().replace_all(fragment, "");
    let decoded = html_escape::decode_html_entities(stripped.as_ref());
    let collapsed = ws_re().replace_all(decoded.as_ref(), " ");
    collapsed.trim().chars().take(limit).collect()
}

fn search_url(href: &str) -> String {
    let mut href = html_escape::decode_html_entities(href).trim().to_string();
    if href.starts_with("//") {
        href.insert_str(0, "https:");
    }
    let Ok(parsed) = split_url(&href) else {
        return String::new();
    };
    let host = hostname_of(&parsed.netloc);
    if is_ddg_host(&host) {
        if let Some(uddg) = first_query_value(&parsed.query, "uddg") {
            href = html_escape::decode_html_entities(&uddg).trim().to_string();
            let Ok(parsed) = split_url(&href) else {
                return String::new();
            };
            return accept_url(href, &parsed);
        }
    }
    accept_url(href, &parsed)
}

fn accept_url(href: String, parsed: &Parts) -> String {
    if parsed.scheme != "http" && parsed.scheme != "https" {
        return String::new();
    }
    if parsed.netloc.is_empty() {
        return String::new();
    }
    if href.chars().count() > 500 {
        return String::new();
    }
    href
}

fn is_ddg_host(host: &str) -> bool {
    host == "duckduckgo.com" || host.ends_with(".duckduckgo.com")
}

/// Python 3.14 `urlsplit` 里和搜索链接有关的部分。解析失败返回 Err，调用方丢掉这条。
/// 非 ASCII netloc 的 NFKC 检查没有依赖可做，这里不复现。
fn split_url(raw: &str) -> Result<Parts, ()> {
    let trimmed = raw.trim_start_matches(|c: char| (c as u32) <= 0x20);
    let cleaned: String = trimmed.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest = cleaned.as_str();
    let mut scheme = String::new();
    if let Some(i) = rest.find(':') {
        if i > 0 {
            let head = &rest[..i];
            let bytes = head.as_bytes();
            if bytes[0].is_ascii_alphabetic()
                && head.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            {
                scheme = head.to_ascii_lowercase();
                rest = &rest[i + 1..];
            }
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let delim = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..delim].to_string();
        rest = &after[delim..];
        let open = netloc.contains('[');
        let close = netloc.contains(']');
        if open != close {
            return Err(());
        }
        if open && close {
            check_bracketed(&netloc)?;
        }
    }
    if let Some(i) = rest.find('#') {
        rest = &rest[..i];
    }
    let query = match rest.find('?') {
        Some(i) => rest[i + 1..].to_string(),
        None => String::new(),
    };
    Ok(Parts { scheme, netloc, query })
}

fn check_bracketed(netloc: &str) -> Result<(), ()> {
    let hostinfo = match netloc.rfind('@') {
        Some(i) => &netloc[i + 1..],
        None => netloc,
    };
    let Some(br) = hostinfo.find('[') else {
        return Ok(());
    };
    if br != 0 {
        return Err(());
    }
    let bracketed = &hostinfo[br + 1..];
    let Some(end) = bracketed.find(']') else {
        return Err(());
    };
    let hostname = &bracketed[..end];
    let port = &bracketed[end + 1..];
    if !port.is_empty() && !port.starts_with(':') {
        return Err(());
    }
    check_bracketed_host(hostname)
}

fn check_bracketed_host(hostname: &str) -> Result<(), ()> {
    if let Some(rest) = hostname.strip_prefix('v') {
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i > 0 && i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() {
            return Ok(());
        }
        return Err(());
    }
    if hostname.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(());
    }
    hostname.parse::<std::net::Ipv6Addr>().map(|_| ()).map_err(|_| ())
}

fn hostname_of(netloc: &str) -> String {
    let hostinfo = match netloc.rfind('@') {
        Some(i) => &netloc[i + 1..],
        None => netloc,
    };
    let hostname = if let Some(br) = hostinfo.find('[') {
        let bracketed = &hostinfo[br + 1..];
        match bracketed.find(']') {
            Some(end) => &bracketed[..end],
            None => "",
        }
    } else {
        match hostinfo.find(':') {
            Some(i) => &hostinfo[..i],
            None => hostinfo,
        }
    };
    if let Some((host, zone)) = hostname.split_once('%') {
        format!("{}%{zone}", host.to_lowercase())
    } else {
        hostname.to_lowercase()
    }
}

/// `parse_qs` 默认丢掉空值，取同名的第一项。`+` 先变空格，再百分号解码。
fn first_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if unquote_plus(name) == key {
            return Some(unquote_plus(value));
        }
    }
    None
}

fn unquote_plus(s: &str) -> String {
    let spaced = s.replace('+', " ");
    let bytes = spaced.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) {
            out.push((hex_val(bytes[i + 1]) << 4) | hex_val(bytes[i + 2]));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn parse_search_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let titles: Vec<_> = title_re().captures_iter(html).collect();
    let snips: Vec<_> = snip_re().captures_iter(html).collect();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, hit) in titles.iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        let href = hit.get(1).map(|m| m.as_str()).unwrap_or("");
        let url = search_url(href);
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let inner = hit.get(2).map(|m| m.as_str()).unwrap_or("");
        let text = search_text(inner, 180);
        let title = if text.is_empty() { url.clone() } else { text };
        let snippet = if i < snips.len() {
            let frag = snips[i].get(1).map(|m| m.as_str()).unwrap_or("");
            search_text(frag, 240)
        } else {
            String::new()
        };
        out.push(SearchHit { title, url, snippet });
    }
    out
}

fn quote_query(query: &str) -> String {
    percent_encoding::utf8_percent_encode(query, QUERY_ENCODE).to_string()
}

fn search_request_url(query: &str) -> String {
    format!("{SEARCH_URL}{}", quote_query(query))
}

fn search_failed() -> Bad {
    gate::bad(502, "search-failed", "联网搜索失败")
}

/// 抓取给定 URL。生产调用传入 `SEARCH_URL + quote`，测试传入本地桩地址。
async fn fetch_search_html(url: &str) -> Result<String, Bad> {
    let client = crate::upstream::build(Duration::from_secs(SEARCH_TIMEOUT_SECS));
    let mut resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, SEARCH_UA)
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await
        .map_err(|_| search_failed())?;
    if resp.status().as_u16() != 200 {
        return Err(search_failed());
    }
    let mut raw = read_capped(&mut resp).await.map_err(|_| search_failed())?;
    if raw.len() > SEARCH_MAX_BYTES {
        raw.truncate(SEARCH_MAX_BYTES);
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

async fn read_capped(resp: &mut reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut buf = Vec::new();
    let cap = SEARCH_MAX_BYTES + 1;
    while buf.len() < cap {
        let Some(chunk) = resp.chunk().await? else {
            break;
        };
        let need = cap - buf.len();
        if chunk.len() > need {
            buf.extend_from_slice(&chunk[..need]);
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    Ok(buf)
}

/// `q` 已按查询串解码。缺失由调用方当成空串。
pub async fn api_search(raw: &str) -> Result<Value, Bad> {
    let query = search_query_check(raw).map_err(|msg| gate::bad(400, "bad-query", msg))?;
    let html = fetch_search_html(&search_request_url(query)).await?;
    Ok(serde_json::json!({
        "ok": true,
        "query": query,
        "results": parse_search_html(&html, SEARCH_HITS),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    const D9_HTML: &str = concat!(
        r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&amp;rut=abc">Example &amp; Domain</a>"#,
        "\n",
        r#"<a class="result__snippet" href="x">a <b>snippet</b></a>"#,
        "\n",
        r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=z">Second</a>"#,
        "\n",
        r#"<a class="result__snippet">second snippet</a>"#,
        "\n",
        r#"<a class="result__a" href="javascript:alert(1)">Bad</a>"#,
    );

    /* 票13（后端 Rust 化退役批）：Python 对照系已删出仓库，下面的期望值冻结自
       sf_server.py 删除前的实测输出（黄金值抓取记录在票13 工单），不再拉起 Python。 */

    #[test]
    fn query_check_cases() {
        let long_ok = "灯".repeat(SEARCH_QUERY_MAX);
        let cases: Vec<(String, Result<String, &'static str>)> = vec![
            (String::new(), Err("没有给出搜索词")),
            ("   ".into(), Err("没有给出搜索词")),
            ("\t\n".into(), Err("没有给出搜索词")),
            ("\u{00a0}".into(), Err("没有给出搜索词")),
            (" foo".into(), Err("搜索词不能查")),
            ("foo ".into(), Err("搜索词不能查")),
            ("\u{00a0}foo".into(), Err("搜索词不能查")),
            ("a\nb".into(), Err("搜索词不能查")),
            ("a\u{0001}b".into(), Err("搜索词不能查")),
            ("a\u{007f}b".into(), Ok("a\u{007f}b".into())),
            ("\u{007f}a".into(), Ok("\u{007f}a".into())),
            ("C:/Windows/win.ini".into(), Ok("C:/Windows/win.ini".into())),
            ("../etc/passwd".into(), Ok("../etc/passwd".into())),
            (long_ok.clone(), Ok(long_ok)),
            ("灯".repeat(SEARCH_QUERY_MAX + 1), Err("搜索词太长")),
            ("普通搜索".into(), Ok("普通搜索".into())),
        ];
        for (raw, exp) in cases {
            match &exp {
                Ok(want) => assert_eq!(search_query_check(&raw), Ok(want.as_str()), "query {raw:?}"),
                Err(msg) => assert_eq!(search_query_check(&raw), Err(*msg), "query {raw:?}"),
            }
        }
    }

    #[test]
    fn parse_d9_golden() {        let full = serde_json::to_value(parse_search_html(D9_HTML, 5)).unwrap();
        let one = serde_json::to_value(parse_search_html(D9_HTML, 1)).unwrap();
        assert_eq!(
            full,
            serde_json::json!([
                {"snippet":"a snippet","title":"Example & Domain","url":"https://example.com/"},
                {"snippet":"second snippet","title":"Second","url":"https://example.com/page"},
            ]),
            "D9 limit 5"
        );
        assert_eq!(
            one,
            serde_json::json!([
                {"snippet":"a snippet","title":"Example & Domain","url":"https://example.com/"},
            ]),
            "D9 limit 1"
        );
        assert_eq!(one.as_array().unwrap().len(), 1);
    }

    #[test]
    fn parse_edges_golden() {
        let prefix = "https://example.com/";
        let keep = format!("{prefix}{}", "b".repeat(500 - prefix.chars().count()));
        let drop_href = format!("{keep}x");
        assert_eq!(keep.chars().count(), 500);
        assert_eq!(drop_href.chars().count(), 501);
        let long_title = "灯".repeat(181);
        let long_snip = "摘".repeat(241);
        let cases = vec![
            format!(
                r#"<a class="result__a" href="https://example.com/empty"></a><a class="result__snippet">kept</a><a class="result__a" href="https://example.com/blank">   </a><a class="result__snippet">also</a>"#
            ),
            format!(
                r#"<a class="result__a" href="https://example.com/dup">First</a><a class="result__snippet">s1</a><a class="result__a" href="https://example.com/dup">Second</a><a class="result__snippet">s2</a><a class="result__a" href="https://example.com/other">Third</a><a class="result__snippet">s3</a>"#
            ),
            format!(
                r#"<a class="result__a" href="https://example.com/a">A</a><a class="result__a" href="https://example.com/b">B</a><a class="result__snippet">only-a</a>"#
            ),
            format!(
                r#"<a class="result__a" href="//example.com/path">Host</a><a class="result__snippet">s</a>"#
            ),
            format!(
                r#"<a class="result__a" href="{drop_href}">Drop</a><a class="result__snippet">sd</a><a class="result__a" href="{keep}">Keep</a><a class="result__snippet">sk</a>"#
            ),
            format!(
                r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa+b">Plus</a><a class="result__snippet">sp</a>"#
            ),
            format!(
                r#"<a class="result__a" href="//html.duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fz">Sub</a>"#
            ),
            format!(
                r#"<a class="result__a" href="//duckduckgo.com.evil/l/?uddg=https%3A%2F%2Fexample.com%2Fstolen">Evil</a>"#
            ),
            format!(
                r#"<a class="result__a" href="https://example.com/t">{long_title}</a><a class="result__snippet">{long_snip}</a>"#
            ),
            format!(
                r#"<a class="result__a" href="HTTP://example.com/Up">Up</a><a class="result__a" href="data:text/html,hi">Data</a><a class="result__a" href="https://example.com/after-data">After</a>"#
            ),
        ];
        let want: Vec<Value> = vec![
            // 空标题/纯空白标题 → 标题回退为链接地址
            serde_json::json!([
                {"snippet":"kept","title":"https://example.com/empty","url":"https://example.com/empty"},
                {"snippet":"also","title":"https://example.com/blank","url":"https://example.com/blank"},
            ]),
            // 同 URL 第二条起丢弃
            serde_json::json!([
                {"snippet":"s1","title":"First","url":"https://example.com/dup"},
                {"snippet":"s3","title":"Third","url":"https://example.com/other"},
            ]),
            // 标题多于摘要：多出的标题保留、摘要留空
            serde_json::json!([
                {"snippet":"only-a","title":"A","url":"https://example.com/a"},
                {"snippet":"","title":"B","url":"https://example.com/b"},
            ]),
            // 协议相对链接补 https
            serde_json::json!([
                {"snippet":"s","title":"Host","url":"https://example.com/path"},
            ]),
            // 501 字符链接整条丢弃；恰 500 保留
            serde_json::json!([
                {"snippet":"sk","title":"Keep","url":keep},
            ]),
            // uddg 里 %2B 解回 + 后不转空格
            serde_json::json!([
                {"snippet":"sp","title":"Plus","url":"https://example.com/a b"},
            ]),
            // duckduckgo 子域的重定向壳不认，按解出的地址收
            serde_json::json!([
                {"snippet":"","title":"Sub","url":"https://example.com/z"},
            ]),
            // 近似域不是 duckduckgo：不拆壳，按原链接保留
            serde_json::json!([
                {"snippet":"","title":"Evil","url":"https://duckduckgo.com.evil/l/?uddg=https%3A%2F%2Fexample.com%2Fstolen"},
            ]),
            // 标题截 180、摘要截 240（纯截断，不加省略号）
            serde_json::json!([
                {"snippet":"摘".repeat(240),"title":"灯".repeat(180),"url":"https://example.com/t"},
            ]),
            // 大写 HTTP 原样保留；data: 丢弃，后面的照收
            serde_json::json!([
                {"snippet":"","title":"Up","url":"HTTP://example.com/Up"},
                {"snippet":"","title":"After","url":"https://example.com/after-data"},
            ]),
        ];
        assert_eq!(cases.len(), want.len());
        for (i, html) in cases.iter().enumerate() {
            let got = serde_json::to_value(parse_search_html(html, 5)).unwrap();
            assert_eq!(got, want[i], "edge {i}: {html}");
        }
    }

    #[test]
    fn quote_and_constants() {
        let cases = [
            ("", "", "https://html.duckduckgo.com/html/?q="),
            ("a b", "a%20b", "https://html.duckduckgo.com/html/?q=a%20b"),
            ("a/b", "a%2Fb", "https://html.duckduckgo.com/html/?q=a%2Fb"),
            ("a+b", "a%2Bb", "https://html.duckduckgo.com/html/?q=a%2Bb"),
            ("-._~", "-._~", "https://html.duckduckgo.com/html/?q=-._~"),
            ("灯", "%E7%81%AF", "https://html.duckduckgo.com/html/?q=%E7%81%AF"),
            ("C:/Windows/win.ini", "C%3A%2FWindows%2Fwin.ini", "https://html.duckduckgo.com/html/?q=C%3A%2FWindows%2Fwin.ini"),
            ("a&b=c?d#e", "a%26b%3Dc%3Fd%23e", "https://html.duckduckgo.com/html/?q=a%26b%3Dc%3Fd%23e"),
            ("你好 world", "%E4%BD%A0%E5%A5%BD%20world", "https://html.duckduckgo.com/html/?q=%E4%BD%A0%E5%A5%BD%20world"),
            ("a\u{00a0}b", "a%C2%A0b", "https://html.duckduckgo.com/html/?q=a%C2%A0b"),
        ];
        for (q, quoted, url) in cases {
            assert_eq!(quote_query(q), quoted, "quote {q:?}");
            assert_eq!(search_request_url(q), url, "url {q:?}");
        }
        // 冻结常量（sf_server.py 删除前的口径）：命中数/查询上限/超时/抓取上限/固定搜索页/UA
        assert_eq!(SEARCH_HITS, 5);
        assert_eq!(SEARCH_QUERY_MAX, 200);
        assert_eq!(SEARCH_TIMEOUT_SECS, 12);
        assert_eq!(SEARCH_MAX_BYTES, 1_000_000);
        assert_eq!(SEARCH_URL, "https://html.duckduckgo.com/html/?q=");
        assert_eq!(
            SEARCH_UA,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36"
        );
        assert!(!SEARCH_URL.starts_with("http://"), "生产地址是固定 HTTPS");
    }

    struct Stub {
        addr: std::net::SocketAddr,
        req: std::sync::mpsc::Receiver<String>,
    }

    fn spawn_stub(status: u16, extra_headers: &str, body: &[u8]) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_vec();
        let extra_headers = extra_headers.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
                    break;
                }
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.write_all(&body);
        });
        Stub { addr, req: rx }
    }

    #[tokio::test]
    async fn fetch_sends_browser_headers_and_quoted_query() {
        let query = "a b/灯-._~";
        let quoted = quote_query(query);
        let stub = spawn_stub(200, "", b"<html>ok</html>");
        let url = format!("http://{}/html/?q={quoted}", stub.addr);
        let html = fetch_search_html(&url).await.expect("200");
        assert_eq!(html, "<html>ok</html>");
        let req = stub.req.recv_timeout(Duration::from_secs(5)).expect("请求头");
        assert!(req.starts_with("GET "), "{req}");
        assert!(req.contains(&quoted), "请求行应带 quote 结果\n{req}");
        assert!(req.contains(SEARCH_UA), "{req}");
        let lower = req.to_ascii_lowercase();
        assert!(lower.contains("accept: text/html"), "{req}");
        assert!(!lower.contains("*/*"), "不要默认 Accept */*\n{req}");
        assert!(!url.starts_with(SEARCH_URL), "桩测试不打真实搜索站");
    }

    #[tokio::test]
    async fn fetch_redirect_is_not_followed() {
        let dest = TcpListener::bind("127.0.0.1:0").unwrap();
        let dest_addr = dest.local_addr().unwrap();
        dest.set_nonblocking(true).unwrap();
        let stub = spawn_stub(
            302,
            &format!("Location: http://{dest_addr}/landed\r\n"),
            b"redirect-body",
        );
        let err = fetch_search_html(&format!("http://{}/", stub.addr)).await.expect_err("302");
        assert_eq!(err.status, 502);
        assert_eq!(err.error, "search-failed");
        assert_eq!(err.message, "联网搜索失败");
        assert!(!err.message.contains("redirect-body"));
        std::thread::sleep(Duration::from_millis(300));
        assert!(dest.accept().is_err(), "重定向目标不应收到请求");
    }

    #[tokio::test]
    async fn fetch_non_200_is_search_failed() {
        for status in [201u16, 202, 204, 404, 500] {
            let stub = spawn_stub(status, "", b"UPSTREAM-BODY-SHOULD-NOT-LEAK");
            let err = fetch_search_html(&format!("http://{}/", stub.addr))
                .await
                .expect_err("non-200");
            assert_eq!(err.status, 502, "{status}");
            assert_eq!(err.error, "search-failed", "{status}");
            assert_eq!(err.message, "联网搜索失败", "{status}");
            assert!(!format!("{err:?}").contains("UPSTREAM"), "{status} {err:?}");
        }
    }

    #[tokio::test]
    async fn fetch_truncates_to_max_bytes() {
        let mut body = vec![b'B'; SEARCH_MAX_BYTES];
        body.extend_from_slice(b"TAILMARKER-SHOULD-DROP");
        let stub = spawn_stub(200, "", &body);
        let html = fetch_search_html(&format!("http://{}/", stub.addr)).await.expect("200");
        assert_eq!(html.len(), SEARCH_MAX_BYTES);
        assert!(html.chars().all(|c| c == 'B'));
        assert!(!html.contains("TAILMARKER"));
    }

    #[tokio::test]
    async fn fetch_invalid_utf8_replaces_bad_bytes() {
        let raw = [0xff, 0xfe, b'A'];
        let stub = spawn_stub(200, "", &raw);
        let html = fetch_search_html(&format!("http://{}/", stub.addr)).await.expect("200");
        // 冻结自 Python decode('utf-8','replace') 口径：每个坏字节一枚替换符（票13 起不再拉 Python 对照）
        assert_eq!(html, "\u{fffd}\u{fffd}A");
    }

    #[tokio::test]
    async fn fetch_transport_error_hides_os_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = fetch_search_html(&format!("http://{addr}/gone")).await.expect_err("refused");
        assert_eq!(err.status, 502);
        assert_eq!(err.error, "search-failed");
        assert_eq!(err.message, "联网搜索失败");
    }

    #[tokio::test]
    async fn api_search_rejected_queries_do_not_fetch() {
        let samples = [
            "",
            "   ",
            " foo",
            "a\nb",
            "a\u{0001}b",
        ];
        let long = "灯".repeat(SEARCH_QUERY_MAX + 1);
        for raw in samples.into_iter().chain(std::iter::once(long.as_str())) {
            let err = api_search(raw).await.expect_err("应拒绝");
            assert_eq!(err.status, 400);
            assert_eq!(err.error, "bad-query");
            assert_eq!(err.message, search_query_check(raw).unwrap_err());
        }
    }
}
