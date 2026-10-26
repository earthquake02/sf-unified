//! 技能 SKILL.md 元数据解析（ADR 0020，技能支持 L2）：build.rs（frozen 编译期预解析）与
//! http.rs（dev 通道/非 frozen 运行时现读现解析）共用同一实现，杜绝双侧漂移。
//! 口径与前端 `cpSkillParse`/`cpSkillFnId`/`estimateTokens` 逐条对齐（sim_skill_meta 双边对算钉住）：
//! - frontmatter：首行 `---[ \t]*`、封闭行 `---[ \t]*`，平键 `键: 值`（冒号半角/全角，键名小写化）；
//! - `适用`/`triggers`：去 `[` `]` 后按 `[,，、]` 拆数组；适用值做中文标签→功能 id 归一；
//! - tokens：CJK 区间（0x2E80-0x9FFF／0xF900-0xFAFF／0xFF00-0xFFEF／0x20000-0x3FFFF）每字符 1.5、
//!   其余 0.25，四舍五入——对正文（去 frontmatter、剥开头空行）估算，与前端 estimateTokens 同值。

/// 功能 id ⇄ 中文标签（与前端 CP_SKILL_FN_LABELS 同表；整理＝隐藏管线不在适用面，助理＝L3 起接入目录按需装载）。
const FN_LABELS: [(&str, &str); 6] = [
    ("beats", "节拍"),
    ("rewrite", "改写/文风"),
    ("summarize", "摘要"),
    ("continue", "推演"),
    ("prose", "推演"),
    ("assistant", "助理"),
];

/// tokens 估算（与前端 estimateTokens 同权重）：n = 1.5·cjk + 0.25·other = (6·cjk+other)/4，
/// Math.round 等价于非负数的 floor(x+0.5)＝整数除法 (6·cjk+other+2)/4。Rust chars() 按标量值遍历，
/// 与 JS codePointAt（跳代理对）同口径。
pub fn estimate_tokens(text: &str) -> u64 {
    let mut cjk: u64 = 0;
    let mut other: u64 = 0;
    for c in text.chars() {
        let u = c as u32;
        if (0x2E80..=0x9FFF).contains(&u)
            || (0xF900..=0xFAFF).contains(&u)
            || (0xFF00..=0xFFEF).contains(&u)
            || (0x20000..=0x3FFFF).contains(&u)
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (6 * cjk + other + 2) / 4
}

/// 适用值归一为功能 id：id 原样/小写或中文标签皆认；认不得返回 ""。规则同前端 cpSkillFnId。
pub fn fn_id(v: &str) -> &'static str {
    let s = v.trim();
    for (id, _) in FN_LABELS {
        if s == id {
            return id;
        }
    }
    let low = s.to_lowercase();
    for (id, _) in FN_LABELS {
        if low == id {
            return id;
        }
    }
    for (id, label) in FN_LABELS {
        if s == label {
            return id;
        }
    }
    // 旧适用标签别名
    if s == "改写" {
        return "rewrite";
    }
    if s == "续写" || s == "成文" {
        return "continue";
    }
    ""
}

/// 切 frontmatter：与前端 cpSkillParse 的正则
/// `/^---[ \t]*\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)([\s\S]*)$/` 同口径——首行必须带换行，
/// 封闭栅栏取第一个「---[ \t]*」独占行；正文剥开头空行（同 JS 的 `replace(/^\r?\n+/,'')`）。
/// 无 frontmatter 返回 None（调用侧整文件即正文）。
pub fn split_frontmatter(t: &str) -> Option<(&str, &str)> {
    let bytes = t.as_bytes();
    let first_nl = t.find('\n')?;
    let mut first_line = &t[..first_nl];
    if let Some(x) = first_line.strip_suffix('\r') {
        first_line = x;
    }
    if !first_line.starts_with("---") || !first_line[3..].bytes().all(|b| b == b' ' || b == b'\t') {
        return None;
    }
    let fm_start = first_nl + 1;
    let mut pos = fm_start;
    loop {
        let line_end = match t[pos..].find('\n') {
            Some(i) => pos + i,
            None => t.len(),
        };
        let mut line = &t[pos..line_end];
        if let Some(x) = line.strip_suffix('\r') {
            line = x;
        }
        if line.starts_with("---") && line[3..].bytes().all(|b| b == b' ' || b == b'\t') {
            // fm 区止于栅栏行前的换行（JS：([\s\S]*?)\r?\n---）；\r\n 一并去掉
            let mut fm_end = pos;
            if fm_end > fm_start && bytes[fm_end - 1] == b'\n' {
                fm_end -= 1;
            }
            if fm_end > fm_start && bytes[fm_end - 1] == b'\r' {
                fm_end -= 1;
            }
            let mut body = if line_end < t.len() { &t[line_end + 1..] } else { "" };
            loop {
                if let Some(b) = body.strip_prefix("\r\n") {
                    body = b;
                } else if let Some(b) = body.strip_prefix('\n') {
                    body = b;
                } else {
                    break;
                }
            }
            return Some((&t[fm_start..fm_end], body));
        }
        if line_end >= t.len() {
            return None; // 无封闭栅栏：不构成 frontmatter
        }
        pos = line_end + 1;
    }
}

/// frontmatter 平键行解析：键首字符 [A-Za-z_一-鿿]、后续可带数字与 `-`，冒号半角/全角；
/// 返回 (小写键, 去空白值)。与前端 cpSkillParse 的行正则同口径。
fn parse_kv_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let mut chars = line.char_indices();
    let (_, c0) = chars.next()?;
    let c0_ok = c0.is_ascii_alphabetic() || c0 == '_' || (0x4E00..=0x9FFF).contains(&(c0 as u32));
    if !c0_ok {
        return None;
    }
    let mut end = c0.len_utf8();
    for (i, c) in chars {
        let ok = c.is_ascii_alphanumeric()
            || c == '_'
            || c == '-'
            || (0x4E00..=0x9FFF).contains(&(c as u32));
        if !ok {
            end = i;
            break;
        }
        end = i + c.len_utf8();
    }
    let key = &line[..end];
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：'))?;
    Some((key.to_lowercase(), rest.trim().to_string()))
}

/// 按键序取第一个非空值（同 JS 的 `raw.a || raw.b || ''` 短路）。
fn first_non_empty<'a>(kv: &'a [(String, String)], keys: &[&str]) -> &'a str {
    for k in keys {
        if let Some((_, v)) = kv.iter().find(|(kk, _)| kk == k) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    ""
}

/// 列表值拆解：去首尾 `[` `]` 后按 `[,，、]` 切分、逐项去空白、弃空项（同前端 .replace(/^\[|\]$/g,'') ＋ split）。
fn split_list(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('[').unwrap_or(s);
    let s = s.strip_suffix(']').unwrap_or(s);
    s.split([',', '，', '、'])
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// 单技能元数据 JSON 对象：`{"dir","name","description","cats":[],"triggers":[],"tokens","refs":[]}`。
/// name/description 键中英文双认（name/名称、description/描述），triggers 认 triggers/触发；
/// tokens 对正文（去 frontmatter）估算。无 frontmatter 时 name/desc 空、cats/triggers 空、正文＝全文。
/// refs＝该技能 `references/` 子目录的合法参考文件名清单（L3 按需读取；调用方排好序传入，纯元数据不读内容）。
pub fn meta_json(dir: &str, text: &str, refs: &[String]) -> String {
    let (name, desc, cats, triggers, body) = match split_frontmatter(text) {
        Some((fm, body)) => {
            let kv: Vec<(String, String)> = fm.split('\n').filter_map(parse_kv_line).collect();
            let name = first_non_empty(&kv, &["name", "名称"]).to_string();
            let desc = first_non_empty(&kv, &["description", "描述"]).to_string();
            let cats = split_list(first_non_empty(&kv, &["适用", "apply", "cats"]))
                .iter()
                .map(|s| fn_id(s))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect::<Vec<String>>();
            let triggers = split_list(first_non_empty(&kv, &["triggers", "触发"]));
            (name, desc, cats, triggers, body)
        }
        None => (String::new(), String::new(), Vec::new(), Vec::new(), text),
    };
    let tokens = estimate_tokens(body);
    let mut out = String::from("{\"dir\":");
    out.push_str(&json_str(dir));
    out.push_str(",\"name\":");
    out.push_str(&json_str(&name));
    out.push_str(",\"description\":");
    out.push_str(&json_str(&desc));
    out.push_str(",\"cats\":[");
    out.push_str(&cats.iter().map(|c| json_str(c)).collect::<Vec<_>>().join(","));
    out.push_str("],\"triggers\":[");
    out.push_str(&triggers.iter().map(|c| json_str(c)).collect::<Vec<_>>().join(","));
    out.push_str(&format!("],\"tokens\":{tokens},\"refs\":["));
    out.push_str(&refs.iter().map(|c| json_str(c)).collect::<Vec<_>>().join(","));
    out.push_str("]}");
    out
}

/// 参考文件名白名单（L3 references 按需读取，与 build.rs/http.rs 共用）：单段文件名——
/// 不含 `/`、不以 `.` 开头、路径段不得为 `..`，扩展名 `.md`/`.txt`（大小写不敏感）。
pub fn ref_file_ok(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    let lower = name.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".txt")
}

/// JSON 字符串转义（引号/反斜杠/控制字符；非 ASCII 直出 UTF-8）。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
