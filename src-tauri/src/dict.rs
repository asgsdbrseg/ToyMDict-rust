//! 词典读取、普通搜索、高级搜索（正则 / 异体字 / 全文 / 多词典流式）。
//!
//! 线程模型：`mdx::MdxReader` 内部用 `Rc` 缓存解压块，不是 `Send`。
//! 因此全局不持有 reader，每个工作线程用 `thread_local` 打开并复用自己的
//! reader；多词典搜索各自开线程，天然并行且无锁竞争。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mdx::readers::MdxReader;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use tauri::AppHandle;
use url::Url;

use crate::state::{AppState, DictInfo, HitSource, SearchHit};
use crate::{call_js, html};

// 普通搜索与高级搜索都不再限制结果条数。
const FLUSH_BATCH: usize = 30;
const INDEX_BATCH: u64 = 256;
/// 普通前缀搜索的分批大小：一次多取一些索引，减少跨批次调用开销。
const PREFIX_SCAN_BATCH: u64 = 4096;
/// 每线程缓存的 reader 数。thread_local 意味着 rayon 每个工作线程都有独立
/// 缓存：8 个 reader × 数个解压数据块 × 16 核，常驻内存可达数百 MB。
/// 降到 2 后反复切换词典只是重开文件句柄，内存收益远大于重开开销。
const READER_CACHE_MAX: usize = 2;

thread_local! {
    /// 每线程独立的 reader 缓存（key = 词典绝对路径）
    static READERS: RefCell<HashMap<String, MdxReader>> = RefCell::new(HashMap::new());
}

fn open_reader(path: &str) -> Result<MdxReader, String> {
    let url = Url::from_file_path(path)
        .map_err(|_| format!("无法转换为文件 URL: {}", path))?;
    MdxReader::from_url(&url, "toymdict-desktop")
        .map_err(|e| format!("打开词典失败 {}: {}", path, e))
}

/// 在当前线程内取得（或打开）一本词典的 reader，并交给闭包使用。
fn with_reader<T>(path: &str, f: impl FnOnce(&mut MdxReader) -> T) -> Result<T, String> {
    READERS.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(path) {
            let reader = open_reader(path)?;
            if map.len() >= READER_CACHE_MAX {
                if let Some(k) = map.keys().next().cloned() {
                    map.remove(&k);
                }
            }
            map.insert(path.to_string(), reader);
        }
        let reader = map.get_mut(path).unwrap();
        Ok(f(reader))
    })
}

/// 校验并登记一本词典（同时把条目数缓存进 state，供「词典信息」展示）。
pub fn load_dict(state: &Arc<AppState>, path: &str) -> Result<DictInfo, String> {
    let abs = normalize_path(path);
    if !std::path::Path::new(&abs).exists() {
        return Err(format!("文件不存在: {}", abs));
    }
    let count = with_reader(&abs, |r| r.get_entry_count())?;
    let name = std::path::Path::new(&abs)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&abs)
        .to_string();
    let info = DictInfo {
        id: abs.clone(),
        name: name.clone(),
        entry_count: count,
    };
    state.dicts.write().insert(abs.clone(), info.clone());
    Ok(info)
}

/// 词典详情：只保留 Python 版 `get_dict_info` 弹窗真正用到的字段。
/// 其余原始键值统一放进 `all_fields` 折叠区，不再单独存一份。
#[derive(Debug, Clone, Default)]
pub struct DictMetadata {
    pub title: String,
    pub description: String,
    pub version: String,
    pub encoding: String,
    pub creation_date: String,
    pub entry_count: u64,
    /// 标题来源：`header` / `filename`（对齐 Python 版 `_get_title_source`）
    pub title_source: String,
    /// Header 里 `Encrypted` 的原始值（技术参数「加密」用，对齐 Python 版）
    pub encrypted_raw: String,
    /// 「全部 Header 字段」折叠区：直接取 MDX/ZDB Header XML 的原始属性
    pub all_fields: Vec<(String, String)>,
    /// MDD 资源文件（文件名, 字节数）
    pub mdd_files: Vec<(String, u64)>,
    pub file_size: u64,
    pub modified: String,
    /// 词典打不开时的错误说明；此时仍返回文件大小等文件系统信息。
    pub error: String,
}

/// 从 libmdx 保留的 Header XML 中取出根元素的全部属性（保持文件里的顺序），
/// 用于对齐 Python 版「全部 Header 字段」折叠区的 `raw_header`。
/// libmdx 的 `DbInfo` 只保留解析后的字段，原始属性在 `MetaUnit::raw_header_xml`。
fn parse_header_attrs(xml: &str) -> Vec<(String, String)> {
    let s = xml.trim_start_matches('\u{feff}').trim();
    let mut rest = s;
    let tag = loop {
        let lt = match rest.find('<') {
            Some(v) => v,
            None => return Vec::new(),
        };
        rest = &rest[lt + 1..];
        // 跳过 <?xml ...?> / <!-- ... --> / <!DOCTYPE ...>
        if rest.starts_with('?') || rest.starts_with('!') {
            match rest.find('>') {
                Some(gt) => rest = &rest[gt + 1..],
                None => return Vec::new(),
            }
            continue;
        }
        match rest.find('>') {
            Some(gt) => break &rest[..gt],
            None => return Vec::new(),
        }
    };

    let b = tag.as_bytes();
    let mut i = 0usize;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'/' {
        i += 1; // 跳过元素名
    }
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < b.len()
            && (b[i].is_ascii_alphanumeric()
                || b[i] == b'_'
                || b[i] == b'-'
                || b[i] == b':'
                || b[i] == b'.')
        {
            i += 1;
        }
        if i == ks {
            i += 1;
            continue;
        }
        let key = &tag[ks..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || (b[i] != b'"' && b[i] != b'\'') {
            continue;
        }
        let quote = b[i];
        i += 1;
        let vs = i;
        while i < b.len() && b[i] != quote {
            i += 1;
        }
        let value = &tag[vs..i];
        if i < b.len() {
            i += 1;
        }
        out.push((key.to_string(), xml_unescape(value)));
    }
    out
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p + 1..];
        let end = match rest.find(';') {
            Some(v) => v,
            None => {
                out.push('&');
                continue;
            }
        };
        let ent = &rest[..end];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let num = ent
                    .strip_prefix("#x")
                    .or_else(|| ent.strip_prefix("#X"))
                    .map(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').map(|d| d.parse::<u32>().ok()))
                    .flatten();
                num.and_then(char::from_u32)
            }
        };
        match decoded {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(ent);
                out.push(';');
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 从 libmdx 已解析的 `db_info` 读取元数据，再补上磁盘信息。
/// 单本词典打不开不返回 Err，而是把原因放进 `error` 字段。
pub fn dict_metadata(path: &str) -> DictMetadata {
    let abs = normalize_path(path);
    let p = std::path::Path::new(&abs);
    let file_size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
    let modified = std::fs::metadata(&abs)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| crate::state::format_epoch(d.as_secs()))
        .unwrap_or_default();

    // MDD：同名 .mdd 与 name.1.mdd / name.2.mdd … 分卷（对齐 Python 版）
    let mut mdd_files: Vec<(String, u64)> = Vec::new();
    if let (Some(dir), Some(stem)) = (p.parent(), p.file_stem().and_then(|s| s.to_str())) {
        let mut try_push = |file: std::path::PathBuf| -> bool {
            match std::fs::metadata(&file) {
                Ok(m) => {
                    mdd_files.push((
                        file.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        m.len(),
                    ));
                    true
                }
                Err(_) => false,
            }
        };
        try_push(dir.join(format!("{}.mdd", stem)));
        for i in 1..64 {
            if !try_push(dir.join(format!("{}.{}.mdd", stem, i))) {
                break;
            }
        }
    }

    let built = with_reader(&abs, |r| {
        let db = &r.content_db.meta.db_info;
        // 原始 Header 属性（对齐 Python 版 raw_header）：libmdx 的 db_info 只留了
        // 解析后的字段，折叠区要用 XML 里的原始键名与值。
        let raw = parse_header_attrs(&r.content_db.meta.raw_header_xml);
        let raw_get = |k: &str| -> String {
            raw.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };

        let version_fallback = match format!("{:?}", db.version).as_str() {
            "V1" => "1.0",
            "V2" => "2.0",
            "V3" => "3.0",
            s => s,
        }
        .to_string();
        let version = {
            let v = raw_get("GeneratedByEngineVersion");
            if !v.is_empty() {
                v
            } else {
                let v = raw_get("RequiredEngineVersion");
                if v.is_empty() { version_fallback } else { v }
            }
        };
        let encoding = {
            let v = raw_get("Encoding");
            if v.is_empty() { db.encoding_label.clone() } else { v }
        };
        let creation_date = {
            let v = raw_get("CreationDate");
            if v.is_empty() { db.creation_date.clone() } else { v }
        };
        let encrypted_raw = raw_get("Encrypted");

        // 标题与来源：对齐 Python 版 _get_title / _get_title_source
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let raw_title = db.title.trim();
        let cleaned = if raw_title.is_empty()
            || raw_title.contains("No HTML code allowed")
            || raw_title.eq_ignore_ascii_case("title")
        {
            String::new()
        } else {
            raw_title.to_string()
        };
        let (title, title_source) = if cleaned.is_empty() || cleaned == stem {
            (stem, "filename".to_string())
        } else {
            (cleaned, "header".to_string())
        };

        (
            DictMetadata {
                title,
                title_source,
                // 对齐 Python 版 readmdict.py：MDX Header 是 XML，Description 里的
                // &lt;br&gt; 在解析 Header 时已由 XML 解析器解码成 <br>，拿到的是
                // 「可渲染的原始 HTML 文本」。libmdx 的 DbInfo::from_xml 直接取
                // attr.value 原始字节、不做实体解码，所以优先用 parse_header_attrs
                // （内部已 xml_unescape）；取不到时对 db.description 补做一次解码，
                // 保证两条路径都得到同样的内容，交给 iframe srcdoc 渲染。
                description: {
                    let v = raw_get("Description");
                    if v.is_empty() { xml_unescape(db.description.trim()) } else { v }
                },
                version,
                encoding,
                creation_date,
                encrypted_raw,
                all_fields: raw,
                ..Default::default()
            },
            r.get_entry_count(),
        )
    });

    let (mut meta, entry_count) = match built {
        Ok(v) => v,
        Err(e) => (DictMetadata { error: e, ..Default::default() }, 0),
    };
    meta.file_size = file_size;
    meta.modified = modified;
    meta.mdd_files = mdd_files;
    meta.entry_count = entry_count;
    meta
}

pub fn normalize_path(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    std::env::current_dir()
        .map(|c| c.join(p).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

pub fn get_content(path: &str, key: &str, idx: i64) -> Result<String, String> {
    with_reader(path, |r| {
        let ki = if idx >= 0 {
            r.get_index(idx).map_err(|e| e.to_string())
        } else {
            match r.find_index(key, false, false, true) {
                Ok(Some(k)) => Ok(k),
                Ok(None) => Err("未找到词条".to_string()),
                Err(e) => Err(e.to_string()),
            }
        }?;
        r.get_html(&ki).map_err(|e| e.to_string())
    })?
}

/// 资源路径归一化（对齐 Python 版 `normalize_resource_path`）：
/// 小写、反斜杠转正斜杠、去前导 `/`。
fn normalize_resource_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches('/')
        .to_lowercase()
}

/// 路径安全校验（对齐 Python 版 `check_path_safety`）。
/// 只接受词典目录内的相对路径：拒绝 `..` 段、绝对路径、Windows 盘符与 UNC。
fn is_safe_resource_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    if p.split('/').any(|seg| seg == "..") {
        return false;
    }
    if p.starts_with("//") {
        return false;
    }
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return false;
    }
    true
}

/// MDD 查找候选：原样 / 小写 × 正斜杠 / 反斜杠 × 有无前导分隔符。
/// V2 会在 libmdx 内部把 `/` 转成 `\`；V3 用 ICU collation，对 `\`/`/`、
/// 前导斜杠、大小写的组合并不总能命中，故显式枚举。
fn resource_candidates(file: &str) -> Vec<String> {
    let p = file.trim_start_matches('/').replace('\\', "/");
    let lower = normalize_resource_path(file);
    let mut out = Vec::with_capacity(10);
    for base in [p, lower] {
        let win = base.replace('/', "\\");
        out.push(base.clone());
        out.push(format!("/{}", base));
        out.push(win.clone());
        out.push(format!("\\{}", win));
        out.push(format!("/{}", win));
    }
    out.dedup();
    out
}

/// 词典目录下的外挂资源回退（对齐 Python 版 `get_resource` 的
/// `resolve_case_insensitive`）：先按精确相对路径读，再逐段大小写不敏感匹配。
/// 词典样式表常作为独立 `.css` 放在 .mdx 同目录而非打进 MDD，此路径必须覆盖。
fn read_loose_resource(dict_path: &str, file: &str) -> Option<Vec<u8>> {
    let dir = std::path::Path::new(dict_path).parent()?;
    let rel = file.trim_start_matches('/').replace('\\', "/");
    let exact = dir.join(&rel);
    if exact.is_file() {
        if let Ok(b) = std::fs::read(&exact) {
            return Some(b);
        }
    }
    let mut cur = dir.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let target = part.to_lowercase();
        let entries = std::fs::read_dir(&cur).ok()?;
        let mut matched = None;
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().to_lowercase() == target {
                matched = Some(e.path());
                break;
            }
        }
        cur = matched?;
    }
    if cur.is_file() {
        std::fs::read(&cur).ok()
    } else {
        None
    }
}

/// `.css` 强制返回 `text/css; charset=utf-8`（对齐 Python 版 `get_mime_type`）。
/// 浏览器对样式表做严格 MIME 检查，返回 `application/octet-stream` 会被拒绝应用，
/// 而图片不受此限制——这正是「MDD 图片能显示、词典 CSS 不生效」的常见诱因之一。
fn resource_mime(file: &str) -> String {
    let lower = file.to_ascii_lowercase();
    let path_only = lower.split('?').next().unwrap_or(lower.as_str());
    if path_only.ends_with(".css") {
        return "text/css; charset=utf-8".to_string();
    }
    mime_guess::from_path(path_only)
        .first_or_octet_stream()
        .to_string()
}

/// 读取词典资源：先查 MDD，再回退到词典目录下的外挂文件。
pub fn get_resource(path: &str, file: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    let decoded = percent_encoding::percent_decode_str(file)
        .decode_utf8_lossy()
        .into_owned();
    if !is_safe_resource_path(&decoded) {
        return Ok(None);
    }
    let mime = resource_mime(&decoded);
    let candidates = resource_candidates(&decoded);

    // 1) MDD 内查找。词典本身暂时打不开时降级为「未命中」而非直接报错，
    //    这样纯外挂资源（只有 .mdx + 同目录 .css、没有 MDD）仍有机会命中。
    let from_mdd = with_reader(path, |r| {
        let mdd = match r.data_db.as_mut() {
            Some(m) => m,
            None => return None,
        };
        for c in &candidates {
            if let Ok(Some(bytes)) = mdd.get_data_by_key(c) {
                return Some(bytes);
            }
        }
        None
    })
    .unwrap_or(None);
    if let Some(bytes) = from_mdd {
        return Ok(Some((bytes, mime)));
    }

    // 2) 词典目录下的外挂文件（含大小写不敏感匹配）
    if let Some(bytes) = read_loose_resource(path, &decoded) {
        return Ok(Some((bytes, mime)));
    }
    Ok(None)
}

// ---------------------------------------------------------------- 异体字展开

fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x3400..=0x9FFF).contains(&u) || (0x20000..=0x3FFFF).contains(&u)
}

fn class_char(c: char) -> String {
    match c {
        '\\' | ']' | '[' | '^' | '-' => format!("\\{}", c),
        _ => c.to_string(),
    }
}

/// 把正则里的「字面汉字」展开成字符类，例如 `黄钟` -> `[黄黃][钟鍾鐘]`。
/// 跳过 `\` 转义序列与已有字符类，避免破坏正则语法。
pub fn expand_pattern(pattern: &str, variants: &HashMap<String, Vec<String>>) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() * 2);
    let mut i = 0usize;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '[' {
            in_class = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == ']' {
            in_class = false;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_class && is_cjk(c) {
            if let Some(list) = variants.get(&c.to_string()) {
                let mut seen = HashSet::new();
                let mut body = String::new();
                seen.insert(c);
                body.push_str(&class_char(c));
                for v in list {
                    for ch in v.chars() {
                        if seen.insert(ch) {
                            body.push_str(&class_char(ch));
                        }
                    }
                }
                if body.chars().count() > 1 {
                    out.push('[');
                    out.push_str(&body);
                    out.push(']');
                    i += 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 单个字符的候选写法：原字优先，其后是异体字表里的变体（去重）。
fn char_variants(c: char, variants: &HashMap<String, Vec<String>>) -> Vec<char> {
    let mut out: Vec<char> = vec![c];
    if is_cjk(c) {
        if let Some(list) = variants.get(&c.to_string()) {
            for v in list {
                for ch in v.chars() {
                    if !out.contains(&ch) {
                        out.push(ch);
                    }
                }
            }
        }
    }
    out
}

/// 普通搜索的异体字方案，对齐 Python 版 `MdxWrapper.search` 的两步法：
///   1. 逐字符构造「原字 + 异体字」字符类正则（`^` 前缀匹配）；
///   2. **只**用首字的候选写法做前缀搜索，再用该正则过滤结果。
///
/// 这样等价于「所有字符的任意异体组合」，但候选数恒等于首字异体数，
/// 与关键词长度无关，**不存在笛卡尔积组合爆炸，因此不需要任何组合上限**。
/// 关键词里没有任何字符能展开异体字时回退为普通前缀搜索（正则为 None）。
fn build_variant_search(
    keyword: &str,
    variants: &HashMap<String, Vec<String>>,
) -> (Vec<String>, Option<Regex>) {
    let chars: Vec<char> = keyword.chars().collect();
    if chars.is_empty() {
        return (vec![keyword.to_string()], None);
    }
    let mut parts: Vec<String> = Vec::with_capacity(chars.len());
    let mut has_variant = false;
    for &c in &chars {
        let opts = char_variants(c, variants);
        if opts.len() > 1 {
            has_variant = true;
            let body: String = opts.iter().map(|&ch| class_char(ch)).collect();
            parts.push(format!("[{}]", body));
        } else {
            // 非汉字与元字符按字面量处理，防止关键词被当成正则注入。
            parts.push(regex::escape(&c.to_string()));
        }
    }
    if !has_variant {
        return (vec![keyword.to_string()], None);
    }
    match RegexBuilder::new(&format!("^{}", parts.concat()))
        // 普通搜索本就忽略大小写，英文关键词也能命中不同大小写写法。
        .case_insensitive(true)
        .build()
    {
        Ok(re) => (
            char_variants(chars[0], variants)
                .iter()
                .map(|c| c.to_string())
                .collect(),
            Some(re),
        ),
        // 正则构造异常时退回普通前缀搜索，至少保证基础功能不受影响。
        Err(_) => (vec![keyword.to_string()], None),
    }
}

pub fn compile_pattern(
    pattern: &str,
    use_variants: bool,
    variants: &HashMap<String, Vec<String>>,
) -> Result<(Regex, String), String> {
    let expanded = if use_variants {
        expand_pattern(pattern, variants)
    } else {
        pattern.to_string()
    };
    let re = RegexBuilder::new(&expanded)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("正则表达式无效：{}", e))?;
    Ok((re, expanded))
}

// ---------------------------------------------------------------- 普通搜索

/// 与 Python 版 `readmdict.py::search_prefix` 一致的字节级前缀判断：
/// 只折叠 ASCII 大小写（对应 Python 的 `bytes.lower()`），中文 UTF-8 字节
/// 原样比较。用字节切片避免为每个词条分配一个小写 String。
fn starts_with_ignore_ascii_case(key: &str, prefix: &str) -> bool {
    let kb = key.as_bytes();
    let pb = prefix.as_bytes();
    kb.len() >= pb.len() && kb[..pb.len()].eq_ignore_ascii_case(pb)
}

/// 一条词条是否属于本次前缀搜索：以任一候选前缀开头，并在异体字搜索时
/// 通过字符类正则确认其余各位也是原字或异体字。
fn matches_prefix(key: &str, needles: &[&str], filter: Option<&Regex>) -> bool {
    needles
        .iter()
        .any(|p| starts_with_ignore_ascii_case(key, p))
        && filter.map(|re| re.is_match(key)).unwrap_or(true)
}

/// 全量分批扫描索引做前缀搜索，**对齐 Python 版 `search_prefix`**。
///
/// 不能复用 libmdx 的 `get_similar_indexes`：它用「首个命中词条」的
/// sort key 作为比较基准（而不是用户输入的搜索词），且循环中一旦遇到
/// 第一个不相似就 `break`，隐含「同前缀词条在索引里严格连续」的假设。
/// 中文词典常按拼音 / ICU collation 排序，「子」开头的词条并不连续，
/// 于是只返回开头几条 —— 这正是 Rust 版结果远少于 Python 版的原因，
/// 与结果条数上限无关（上限早已取消）。
///
/// 这里逐条遍历全部索引，用字节级 `starts_with` 过滤，中间不匹配的
/// 词条直接跳过、不终止，因此结果与 Python 版一致，也不受块序影响。
fn scan_prefixes(
    r: &mut MdxReader,
    prefixes: &[String],
    filter: Option<&Regex>,
) -> Result<Vec<(String, i64)>, String> {
    let total = r.get_entry_count() as i64;
    let needles: Vec<&str> = prefixes.iter().map(|s| s.as_str()).collect();
    let mut out = Vec::new();
    let mut pos = 0i64;
    while pos < total {
        let list = r
            .get_indexes(pos, PREFIX_SCAN_BATCH)
            .map_err(|e| e.to_string())?;
        if list.is_empty() {
            break;
        }
        pos += list.len() as i64;
        for ki in list.iter() {
            if matches_prefix(&ki.key, &needles, filter) {
                out.push((ki.key.clone(), ki.entry_no));
            }
        }
    }
    Ok(out)
}

/// 普通搜索结果排序：精确匹配（忽略 ASCII 大小写）置顶，其余按词条
/// 「字符数」升序（短词在前），同长再按字典序。
/// 抽成纯函数便于单元测试，不依赖任何词典文件。
pub fn sort_by_length(hits: &mut [SearchHit], keyword: &str) {
    hits.sort_by(|a, b| {
        let ae = a.key.eq_ignore_ascii_case(keyword);
        let be = b.key.eq_ignore_ascii_case(keyword);
        be.cmp(&ae)
            .then_with(|| a.key.chars().count().cmp(&b.key.chars().count()))
            .then_with(|| a.key.cmp(&b.key))
    });
}

pub fn search(state: &Arc<AppState>, keyword: &str, use_variants: bool) -> Vec<SearchHit> {
    let paths = state.group_paths();
    if keyword.trim().is_empty() || paths.is_empty() {
        return Vec::new();
    }
    let variants = state.variants.read().clone();
    // 异体字搜索：首字候选做前缀查找 + 字符类正则过滤（无组合上限）。
    let (keys, filter_re) = if use_variants {
        build_variant_search(keyword, &variants)
    } else {
        (vec![keyword.to_string()], None)
    };
    let cfg = state.config.lock();
    let names: HashMap<String, String> = cfg
        .all_dicts
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    drop(cfg);

    // 并行扫描中失败的词典汇总成一条提示，避免每本词典各弹一次窗。
    let failures: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let per_dict: Vec<Vec<(String, i64)>> = paths
        .par_iter()
        .map(|path| {
            // 一次扫描覆盖全部候选前缀：每个词条只会被遍历一次，天然去重，
            // 也不会像 get_similar_indexes 那样在第一个不相似处提前中断。
            let res = with_reader(path, |r| {
                let out = scan_prefixes(r, &keys, filter_re.as_ref());
                // 批量扫描会往缓存塞满解压数据块；扫完立刻释放，常驻内存回到基线
                r.trim_caches();
                out
            });
            match res {
                // with_reader 返回 Result<T, String>，闭包内又返回 Result，需展平一层
                Ok(Ok(list)) => list,
                Ok(Err(e)) | Err(e) => {
                    state.log(&format!("搜索词典 {} 失败: {}", path, e));
                    if let Ok(mut f) = failures.lock() {
                        f.push(format!("{}：{}", state.dict_name(path), e));
                    }
                    Vec::new()
                }
            }
        })
        .collect();

    if let Ok(fails) = failures.into_inner() {
        if !fails.is_empty() {
            state.report_error(
                "部分词典搜索失败",
                &format!("共 {} 本未能参与本次搜索：{}", fails.len(), fails.join("；")),
            );
        }
    }

    let mut merged: BTreeMap<String, Vec<HitSource>> = BTreeMap::new();
    for (path, hits) in paths.iter().zip(per_dict.into_iter()) {
        let dict_name = names
            .get(path)
            .cloned()
            .unwrap_or_else(|| state.dict_name(path));
        for (key, idx) in hits {
            merged.entry(key).or_default().push(HitSource {
                dict_id: path.clone(),
                dict_name: dict_name.clone(),
                idx,
            });
        }
    }
    let mut out: Vec<SearchHit> = merged
        .into_iter()
        .map(|(key, sources)| SearchHit { key, sources })
        .collect();
    // 精确匹配置顶，其余按词条长度升序（同长按字典序）。
    // 不限制结果条数：每本词典的前缀扫描已取全，最终列表也不再截断。
    sort_by_length(&mut out, keyword);
    out
}

// ---------------------------------------------------------------- 高级搜索

pub struct AdvCtx {
    pub state: Arc<AppState>,
    pub app: AppHandle,
    pub gen: u64,
    pub total: usize,
    pub done: Arc<AtomicUsize>,
    pub hits: Arc<AtomicUsize>,
    pub re: Regex,
    pub scope: String,
}

fn emit_status(ctx: &AdvCtx) {
    call_js(
        &ctx.app,
        "updateAdvancedStatus",
        vec![
            json!(ctx.done.load(Ordering::SeqCst)),
            json!(ctx.total),
            json!(ctx.hits.load(Ordering::SeqCst)),
        ],
    );
}

/// 扫描一本词典，命中即流式追加到主界面结果列表；本词典扫完立刻更新状态，
/// 不等待其它词典、不合并同名条目、不排序。
pub fn scan_one(ctx: Arc<AdvCtx>, path: String) {
    let name = ctx.state.dict_name(&path);
    let mut batch: Vec<Value> = Vec::with_capacity(FLUSH_BATCH);
    let mut local_hits = 0usize;

    let flush = |batch: &mut Vec<Value>, local_hits: &mut usize| {
        if batch.is_empty() {
            return;
        }
        // 与前端 _advResultCount 顺序对齐：push 与 emit 在同一把锁内完成
        {
            let mut results = ctx.state.results.lock();
            for item in batch.iter() {
                let key = item["key"].as_str().unwrap_or("").to_string();
                results.push(SearchHit {
                    key,
                    sources: vec![HitSource {
                        dict_id: path.clone(),
                        dict_name: name.clone(),
                        idx: item["idx"].as_i64().unwrap_or(-1),
                    }],
                });
            }
            let payload: Vec<Value> = batch
                .iter()
                .map(|v| json!({ "key": v["key"] }))
                .collect();
            call_js(&ctx.app, "appendAdvancedResults", vec![json!(payload)]);
        }
        ctx.hits.fetch_add(batch.len(), Ordering::SeqCst);
        *local_hits += batch.len();
        batch.clear();
        emit_status(&ctx);
    };

    let result = with_reader(&path, |r| -> Result<usize, String> {
        let total = r.get_entry_count() as i64;
        let fulltext = ctx.scope == "fulltext";
        let mut pos = 0i64;
        while pos < total {
            if ctx.state.adv_cancelled(ctx.gen) {
                break;
            }
            let list = r.get_indexes(pos, INDEX_BATCH).map_err(|e| e.to_string())?;
            if list.is_empty() {
                break;
            }
            pos += list.len() as i64;
            for ki in list.iter() {
                if ctx.state.adv_cancelled(ctx.gen) {
                    break;
                }
                let matched = if fulltext {
                    match r.get_html(ki) {
                        Ok(raw) => ctx.re.is_match(&html::strip_html(&raw)),
                        Err(_) => false,
                    }
                } else {
                    ctx.re.is_match(&ki.key)
                };
                if matched {
                    batch.push(json!({ "key": ki.key, "idx": ki.entry_no }));
                    if batch.len() >= FLUSH_BATCH {
                        flush(&mut batch, &mut local_hits);
                    }
                }
            }
        }
        Ok(local_hits)
    });

    // 全文扫描会连续解码大量内容块；无论成功取消，都立即释放缓存
    with_reader(&path, |r| r.trim_caches()).ok();
    flush(&mut batch, &mut local_hits);
    if let Err(e) = result {
        ctx.state
            .report_error("高级搜索失败", &format!("{}: {}", name, e));
    }
    ctx.done.fetch_add(1, Ordering::SeqCst);
    emit_status(&ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variants() -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert("黄".to_string(), vec!["黄".to_string(), "黃".to_string()]);
        m.insert(
            "钟".to_string(),
            vec!["钟".to_string(), "鍾".to_string(), "鐘".to_string()],
        );
        m
    }

    #[test]
    fn 异体字展开为字符类() {
        assert_eq!(expand_pattern("黄钟", &variants()), "[黄黃][钟鍾鐘]");
        assert_eq!(expand_pattern("abc", &variants()), "abc");
    }

    #[test]
    fn 展开不破坏正则语法() {
        let out = expand_pattern(r"\黄[钟]黄", &variants());
        assert!(out.starts_with(r"\黄[钟]"), "{}", out);
        assert!(out.ends_with("[黄黃]"), "{}", out);
        // 量词、锚点保持原样
        assert_eq!(expand_pattern("^黄{2}$", &variants()), "^[黄黃]{2}$");
    }

    #[test]
    fn 正则编译与匹配() {
        let (re, expanded) = compile_pattern("南朝宋", false, &variants()).unwrap();
        assert_eq!(expanded, "南朝宋");
        assert!(re.is_match("xx南朝宋yy"));
        let (re2, _) = compile_pattern("黄钟", true, &variants()).unwrap();
        assert!(re2.is_match("黃鐘"));
        assert!(re2.is_match("黄钟"));
        assert!(!re2.is_match("黄铜"));
    }

    #[test]
    fn 非法正则给出错误而非崩溃() {
        assert!(compile_pattern("[", false, &variants()).is_err());
        // Rust regex 不支持反向引用，应给出明确错误
        assert!(compile_pattern(r"(a)\1", false, &variants()).is_err());
    }

    #[test]
    fn 普通搜索异体字_首字前缀加正则过滤() {
        let (keys, re) = build_variant_search("黄钟", &variants());
        // 只展开首字，不再生成「黄钟 / 黄鍾 / 黃鐘 …」的笛卡尔积。
        assert_eq!(keys, vec!["黄".to_string(), "黃".to_string()]);
        let r = re.unwrap();
        assert!(r.is_match("黄钟"));
        assert!(r.is_match("黃鐘"));
        assert!(r.is_match("黄鍾大吕"));
        assert!(!r.is_match("黄铜"));
        assert!(!r.is_match("铜钟"));
    }

    #[test]
    fn 普通搜索异体字_单字展开全部变体() {
        let (keys, re) = build_variant_search("黄", &variants());
        assert_eq!(keys, vec!["黄".to_string(), "黃".to_string()]);
        assert!(re.unwrap().is_match("黃鐘"));
    }

    #[test]
    fn 普通搜索异体字_无可展开字符时回退普通前缀() {
        let (keys, re) = build_variant_search("abc", &variants());
        assert_eq!(keys, vec!["abc".to_string()]);
        assert!(re.is_none());
        // 元字符按字面量处理，不会被当成正则
        let (keys2, re2) = build_variant_search("a.钟", &variants());
        assert_eq!(keys2, vec!["a".to_string()]);
        assert!(re2.unwrap().is_match("a.鐘"));
    }

    #[test]
    fn 普通搜索异体字_首字无异体时后续字仍展开() {
        let (keys, re) = build_variant_search("a钟", &variants());
        assert_eq!(keys, vec!["a".to_string()]);
        let r = re.unwrap();
        assert!(r.is_match("a鐘"));
        assert!(!r.is_match("a铜"));
    }

    #[test]
    fn 普通搜索异体字_取消组合限制() {
        // 20 字长词、每字都有异体：旧实现的笛卡尔积会被 128 条截断，
        // 新实现候选数恒等于首字异体数，与关键词长度无关。
        let long = "黄".repeat(20);
        let (keys, re) = build_variant_search(&long, &variants());
        assert_eq!(keys.len(), 2);
        assert!(re.unwrap().is_match(&"黃".repeat(20)));
    }

    #[test]
    fn 字节级前缀判断对齐python_bytes_lower() {
        assert!(starts_with_ignore_ascii_case("子主", "子"));
        assert!(starts_with_ignore_ascii_case("子儀", "子"));
        assert!(starts_with_ignore_ascii_case("ABCdef", "abc"));
        assert!(!starts_with_ignore_ascii_case("子", "子主")); // key 比前缀短
        assert!(!starts_with_ignore_ascii_case("孔丘", "子"));
        assert!(!starts_with_ignore_ascii_case("aBc", "abd"));
    }

    #[test]
    fn 前缀扫描不中断_不连续匹配也全部命中() {
        // 复现「搜索『子』只返回 3 条」：词典按拼音 / collation 排序时，
        // 「子」开头的词条中间夹着其它字开头的词条；旧实现用
        // get_similar_indexes 遇到第一个不相似就 break，只能拿到开头几条。
        let corpus = [
            "子主", "孔丘", "子儀", "孟軻", "子斷", "子先", "子平", "子英",
        ];
        let needles = ["子"];
        let hits: Vec<&str> = corpus
            .iter()
            .copied()
            .filter(|k| matches_prefix(k, &needles, None))
            .collect();
        assert_eq!(hits, vec!["子主", "子儀", "子斷", "子先", "子平", "子英"]);
    }

    #[test]
    fn 前缀扫描同时套用异体字正则() {
        let (keys, re) = build_variant_search("黄钟", &variants());
        let needles: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let filter = re.as_ref();
        let corpus = ["黄钟", "黃鐘", "黄銅", "黄帝", "黄钟大吕"];
        let hits: Vec<&str> = corpus
            .iter()
            .copied()
            .filter(|k| matches_prefix(k, &needles, filter))
            .collect();
        assert_eq!(hits, vec!["黄钟", "黃鐘", "黄钟大吕"]);
    }

    #[test]
    fn 资源路径归一化() {
        assert_eq!(normalize_resource_path(r"\css\STYLE.CSS"), "css/style.css");
        assert_eq!(normalize_resource_path("/Img/A.PNG"), "img/a.png");
    }

    #[test]
    fn 资源路径安全校验() {
        assert!(is_safe_resource_path("css/style.css"));
        assert!(is_safe_resource_path(r"img\a.png"));
        assert!(!is_safe_resource_path("../secret.css"));
        assert!(!is_safe_resource_path("css/../../secret.css"));
        assert!(!is_safe_resource_path("C:/Windows/win.ini"));
        assert!(!is_safe_resource_path("//server/share"));
    }

    #[test]
    fn 资源候选覆盖大小写与斜杠() {
        let c = resource_candidates("CSS/Style.css");
        for want in [
            "CSS/Style.css",
            "css/style.css",
            r"\css\style.css",
            "/css/style.css",
        ] {
            assert!(c.contains(&want.to_string()), "缺候选 {}: {:?}", want, c);
        }
    }

    #[test]
    fn css_强制样式表_mime() {
        assert_eq!(resource_mime("css/STYLE.CSS"), "text/css; charset=utf-8");
        assert_eq!(resource_mime(r"\css\STYLE.CSS"), "text/css; charset=utf-8");
        assert_eq!(resource_mime("img/a.png"), "image/png");
    }

    #[test]
    fn 外挂文件回退命中大小写不一致的_css() {
        let dir = std::env::temp_dir().join(format!("toymdict_css_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::write(dir.join("css").join("STYLE.CSS"), b"body{color:red}").unwrap();
        let dict = dir.join("dict.mdx");
        std::fs::write(&dict, b"").unwrap();
        let ds = dict.to_str().unwrap().to_string();
        assert_eq!(
            read_loose_resource(&ds, "css/style.css"),
            Some(b"body{color:red}".to_vec())
        );
        assert_eq!(
            read_loose_resource(&ds, "css/STYLE.CSS"),
            Some(b"body{color:red}".to_vec())
        );
        assert_eq!(read_loose_resource(&ds, "css/missing.css"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 词典目录外挂_css_端到端命中并返回正确_mime() {
        // 模拟「词典正文引用 css/style.css，磁盘上是 css/STYLE.CSS」的场景。
        // mdx 内容故意无效：验证词典打不开时仍能走外挂文件回退。
        let dir = std::env::temp_dir().join(format!("toymdict_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("css")).unwrap();
        let css = b"body{font-family:serif}";
        std::fs::write(dir.join("css").join("STYLE.CSS"), css).unwrap();
        let dict = dir.join("dict.mdx");
        std::fs::write(&dict, b"not-a-real-mdx").unwrap();
        let ds = dict.to_str().unwrap().to_string();

        let (bytes, mime) = get_resource(&ds, "css/style.css")
            .unwrap()
            .expect("外挂 CSS 应被找到");
        assert_eq!(bytes, css.to_vec());
        assert_eq!(mime, "text/css; charset=utf-8");

        // 反斜杠路径同样命中
        assert!(get_resource(&ds, r"css\STYLE.CSS").unwrap().is_some());
        // 越界路径必须被拒绝
        assert!(get_resource(&ds, "../secret.css").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hit(key: &str) -> SearchHit {
        SearchHit {
            key: key.to_string(),
            sources: Vec::new(),
        }
    }

    #[test]
    fn 普通搜索结果按词条长度升序() {
        let mut v = vec![
            hit("南朝宋书"),
            hit("南朝宋"),
            hit("宋"),
            hit("南朝宋代史"),
            hit("南朝"),
        ];
        sort_by_length(&mut v, "无关词");
        let keys: Vec<&str> = v.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, vec!["宋", "南朝", "南朝宋", "南朝宋书", "南朝宋代史"]);
    }

    #[test]
    fn 同长度结果按字典序() {
        let mut v = vec![hit("zz"), hit("aa"), hit("m")];
        sort_by_length(&mut v, "无关词");
        let keys: Vec<&str> = v.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(keys, vec!["m", "aa", "zz"]);
    }

    #[test]
    fn 精确匹配置顶其余仍按长度排序() {
        let mut v = vec![hit("大型词典"), hit("大"), hit("大型")];
        sort_by_length(&mut v, "大型词典");
        assert_eq!(v[0].key, "大型词典"); // 精确匹配，即使最长也排第一
        assert_eq!(v[1].key, "大");
        assert_eq!(v[2].key, "大型");
    }

    #[test]
    fn 精确匹配忽略大小写() {
        let mut v = vec![hit("abc"), hit("ab"), hit("ABC")];
        sort_by_length(&mut v, "AbC");
        // abc / ABC 都算精确匹配且同长，再按字典序；短词 ab 排最后。
        assert_eq!(v[0].key, "ABC");
        assert_eq!(v[1].key, "abc");
        assert_eq!(v[2].key, "ab");
    }

    #[test]
    fn 解析原始header属性() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n\
                   <Dictionary GeneratedByEngineVersion=\"2.0\" Encrypted=\"2\" \
                   Title=\"A &amp; B\" Compact=\"Yes\"/>";
        let a = parse_header_attrs(xml);
        assert_eq!(a.len(), 4);
        assert_eq!(a[0].0, "GeneratedByEngineVersion");
        assert_eq!(a[0].1, "2.0");
        assert_eq!(a[1].1, "2");
        assert_eq!(a[2].1, "A & B");
        assert_eq!(a[3].1, "Yes");
    }

    #[test]
    fn header属性支持单引号与数字实体() {
        let a = parse_header_attrs("<Mdx Title='&#x4E2D;&#25991;' />");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].1, "中文");
    }
}
