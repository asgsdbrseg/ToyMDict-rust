//! 词条 HTML 处理：资源路径重写、标签透明化（全文搜索用）、iframe 骨架。

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};

// 注意：Rust regex 不支持反向引用（\1 / \2），不能用它配对齐引号。
// 结尾改用宽松的引号类，捕获组编号保持不变。
static ATTR_RE: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r#"(?i)\b(src|href)\s*=\s*(["'])([^"']*)["']"#)
        .build()
        .unwrap()
});
static CSS_URL_RE: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r#"(?i)url\(\s*(["']?)([^"')]*)["']?\s*\)"#)
        .build()
        .unwrap()
});
static CSS_IMPORT_RE: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r#"(?i)(@import\s+)(["']?)([^"';\s]+)["']?"#)
        .build()
        .unwrap()
});
static SCRIPT_RE: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r#"(?is)<script\b[^>]*>.*?</script>"#)
        .build()
        .unwrap()
});
static STYLE_RE: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r#"(?is)<style\b[^>]*>.*?</style>"#)
        .build()
        .unwrap()
});
static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").unwrap());

const PASS_THROUGH: &[&str] = &[
    "http:", "https:", "data:", "javascript:", "mailto:", "tel:", "#", "mdd:", "mdx:",
];

fn is_pass_through(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    PASS_THROUGH.iter().any(|p| lower.starts_with(p)) || lower.starts_with("//")
}

/// 把词典正文里的本地资源路径改成相对 `<base>` 的形式。
fn rewrite_local_path(url: &str) -> String {
    let u = url.trim();
    if u.is_empty() || is_pass_through(u) {
        return u.to_string();
    }
    let mut p = if u.len() >= 7 && u[..7].eq_ignore_ascii_case("file://") {
        let rest = &u[7..];
        if rest.starts_with("//") {
            rest[1..].to_string()
        } else {
            rest.to_string()
        }
    } else {
        u.to_string()
    };
    p = p.replace('\\', "/");
    if p.starts_with('/') && !p.starts_with("//") {
        p.remove(0);
    }
    p
}

fn rewrite_attr_value(url: &str) -> String {
    rewrite_local_path(url)
}

fn rewrite_css(text: &str) -> String {
    let out = CSS_URL_RE.replace_all(text, |c: &regex::Captures| {
        let quote = &c[1];
        let path = &c[2];
        if path.is_empty() || is_pass_through(path) {
            return c[0].to_string();
        }
        format!("url({}{}{})", quote, rewrite_local_path(path), quote)
    });
    CSS_IMPORT_RE.replace_all(&out, |c: &regex::Captures| {
        let path = &c[3];
        if is_pass_through(path) {
            return c[0].to_string();
        }
        format!("{}{}{}{}", &c[1], &c[2], rewrite_local_path(path), &c[2])
    })
    .into_owned()
}

/// 与 Python 版 `rewrite_html_links` 对齐：
/// - 修正部分生成器产出的 `href="href="...` 畸形链接
/// - src/href 的本地路径去掉前导 `/`
/// - 内联 style 与 `<style>` 块里的 url() / @import 同样处理
pub fn rewrite_html_links(raw: &str) -> String {
    let fixed = raw.replace("href=\"href=\"", "href=\"");
    let mut out = ATTR_RE
        .replace_all(&fixed, |c: &regex::Captures| {
            let attr = &c[1];
            let quote = &c[2];
            let value = &c[3];
            format!("{}={}{}{}", attr, quote, rewrite_attr_value(value), quote)
        })
        .into_owned();

    // 内联 style="..."
    out = RegexBuilder::new(r#"(?i)style\s*=\s*"([^"]*)""#)
        .build()
        .unwrap()
        .replace_all(&out, |c: &regex::Captures| {
            format!("style=\"{}\"", rewrite_css(&c[1]).replace('"', "&quot;"))
        })
        .into_owned();

    // <style> 块
    out = RegexBuilder::new(r#"(?is)(<style\b[^>]*>)(.*?)(</style>)"#)
        .build()
        .unwrap()
        .replace_all(&out, |c: &regex::Captures| {
            format!("{}{}{}", &c[1], rewrite_css(&c[2]), &c[3])
        })
        .into_owned();
    out
}

/// 标签透明化：`<u>南朝</u><u>宋</u>` -> `南朝宋`，用于全文正则匹配。
pub fn strip_html(raw: &str) -> String {
    let a = SCRIPT_RE.replace_all(raw, " ");
    let b = STYLE_RE.replace_all(&a, " ");
    let c = TAG_RE.replace_all(&b, "");
    decode_entities(&c)
}

/// 与 Python `html.escape(s, quote=True)` 完全一致：把任意文本安全地放进
/// HTML 属性。词典信息里的字段值、以及描述 iframe 的 `srcdoc` 都用它。
pub fn escape_html_attribute(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// 「词典信息」弹窗里的「描述」区块，逐字对齐 Python 版 `get_dict_info`：
/// 一个固定高度的 iframe，描述经属性转义后放进 `srcdoc`。
/// 浏览器解析 srcdoc 属性时会把实体解回原始 HTML 文本，iframe 内部再把
/// `<font>` / `<br>` / `<img>` / `<a>` 等当作真正的标签渲染，且与主窗口
/// 样式隔离。不做额外清理，行为与 Python 版保持一致（词典文件本身可信）。
pub fn dict_info_description_html(desc: &str) -> String {
    format!(
        "<p style='margin-top:8px;'><b>描述:</b></p>\
         <iframe srcdoc=\"{}\" style='width:100%;height:150px;border:1px solid #e0e0e0;border-radius:4px;background:#fff;'></iframe>",
        escape_html_attribute(desc)
    )
}

fn decode_entities(s: &str) -> String {
    let mut out = s
        .replace("&nbsp;", " ")
        .replace("&ensp;", " ")
        .replace("&emsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    // 常见数字实体（十进制）
    if out.contains("&#") {
        let re = Regex::new(r"&#(\d+);").unwrap();
        out = re
            .replace_all(&out, |c: &regex::Captures| {
                c[1]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
    }
    out
}

/// 词条 iframe 内的选区回传脚本。iframe 走自定义协议、与主窗口不同源，
/// 父窗口读不到 `contentDocument` / `getSelection()`。
/// 除了响应父窗口的 `toymdict-get-selection` 请求，它还自己监听双击 Ctrl：
/// 焦点在 iframe 内时主窗口根本收不到 keydown，只靠父窗口广播会漏掉取词。
const SELECTION_BRIDGE: &str = r#"<script>
(function () {
    var last = 0, down = false;
    function selectionText() {
        var text = '';
        try {
            var el = document.activeElement;
            if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {
                text = (el.value || '').slice(el.selectionStart || 0, el.selectionEnd || 0);
            }
            if (!text && window.getSelection) text = window.getSelection().toString();
        } catch (err) {}
        return text || '';
    }
    function reply(type, text) {
        try {
            (window.parent || window).postMessage({ type: type, text: text }, '*');
        } catch (err) {}
    }
    // 焦点在词条 iframe 内时，主窗口的 keydown 监听收不到按键，这里自行判定。
    document.addEventListener('keydown', function (e) {
        if (e.key !== 'Control' || e.altKey || e.shiftKey || e.repeat) {
            last = 0; down = false; return;
        }
        if (down) return;
        down = true;
        var now = Date.now();
        if (last && now - last <= 400) {
            last = 0;
            var text = selectionText();
            if (text.trim()) reply('toymdict-hotkey-selection', text.trim());
        } else {
            last = now;
        }
    }, true);
    document.addEventListener('keyup', function (e) {
        if (e.key === 'Control') down = false;
    }, true);
    window.addEventListener('message', function (e) {
        var d = e && e.data;
        if (!d || !d.type) return;
        if (d.type === 'toymdict-get-selection') {
            reply('toymdict-selection', selectionText());
        } else if (d.type === 'toymdict-hotkey-selection-request') {
            var text = selectionText();
            if (text.trim()) reply('toymdict-hotkey-selection', text.trim());
        }
    });
})();
</script>"#;

/// 生成词条 iframe 的完整 HTML。
/// `base` 形如 `mdx://localhost`（Linux/macOS）或 `http://mdx.localhost`（Windows）。
pub fn build_entry_html(raw: &str, dict_id_enc: &str, iframe_id: &str, base: &str) -> String {
    let body = rewrite_html_links(raw);
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="UTF-8">
<link rel="stylesheet" href="{base}/static/custom.css">
<link rel="stylesheet" href="{base}/static/entry.css">
<base href="{base}/mdd/{dict}/">
</head>
<body data-iframe-id="{ifid}">
<div id="mdx-content">{body}</div>
<script src="{base}/static/entry.js"></script>
{bridge}
</body></html>"#,
        base = base,
        dict = dict_id_enc,
        ifid = iframe_id,
        body = body,
        bridge = SELECTION_BRIDGE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 标签透明匹配() {
        assert_eq!(strip_html("<u>南朝</u><u>宋</u>"), "南朝宋");
        assert_eq!(strip_html("<b>南朝</b><span>宋</span>"), "南朝宋");
    }

    #[test]
    fn 去除脚本样式与实体解码() {
        assert_eq!(
            strip_html("<script>var x=1;</script><style>b{}</style>A&nbsp;&amp;B"),
            "  A &B"
        );
    }

    #[test]
    fn 畸形链接与根路径重写() {
        let out = rewrite_html_links(
            r#"<img src="/img/a.png" href="href="x.png"><div style="background:url(/b.png)"></div>"#,
        );
        assert!(out.contains(r#"src="img/a.png""#), "{}", out);
        assert!(out.contains(r#"href="x.png""#), "{}", out);
        assert!(out.contains("url(b.png)"), "{}", out);
    }

    #[test]
    fn 外部链接不被改写() {
        let out = rewrite_html_links(
            r#"<a href="https://x.com/a.png"><img src="data:image/png;base64,AA"></a>"#,
        );
        assert!(out.contains("https://x.com/a.png"));
        assert!(out.contains("data:image/png;base64,AA"));
    }

    #[test]
    fn 词条骨架包含样式脚本与base() {
        let h = build_entry_html("<u>南朝</u><u>宋</u>", "D%3A%2Fa", "dict-iframe-0", "mdx://localhost");
        assert!(h.contains(r#"<base href="mdx://localhost/mdd/D%3A%2Fa/">"#));
        assert!(h.contains("/static/custom.css"));
        assert!(h.contains("/static/entry.js"));
        assert!(h.contains(r#"data-iframe-id="dict-iframe-0""#));
        assert!(h.contains("<u>南朝</u><u>宋</u>"));
        // 跨域 iframe 的选区回传脚本必须随词条页一起下发
        assert!(h.contains("toymdict-get-selection"));
        assert!(h.contains("toymdict-selection"));
        // iframe 内也要能自行判定双击 Ctrl（焦点在 iframe 时主窗口收不到按键）
        assert!(h.contains("toymdict-hotkey-selection"));
        assert!(h.contains("toymdict-hotkey-selection-request"));
    }

    #[test]
    fn 词典样式表_link_保持相对路径交给base解析() {
        // <base> 已指向 /mdd/{dict}/，link 的 href 必须保持相对路径，
        // 这样浏览器才会请求到 mdx://.../mdd/{dict}/css/STYLE.CSS。
        let out = rewrite_html_links(
            r#"<link rel="stylesheet" type="text/css" href="css/STYLE.CSS"><img src="/img/a.png">"#,
        );
        assert!(out.contains(r#"href="css/STYLE.CSS""#), "{}", out);
        assert!(out.contains(r#"src="img/a.png""#), "{}", out);
    }

    #[test]
    fn 词典样式表反斜杠路径归一化() {
        let out = rewrite_html_links(r#"<link href="\css\STYLE.CSS" rel="stylesheet">"#);
        assert!(out.contains(r#"href="css/STYLE.CSS""#), "{}", out);
    }

    #[test]
    fn html属性转义对齐python_html_escape() {
        let raw = r#"<font color="red">A & B</font><br/>O'Neil"#;
        assert_eq!(
            escape_html_attribute(raw),
            "&lt;font color=&quot;red&quot;&gt;A &amp; B&lt;/font&gt;&lt;br/&gt;O&#x27;Neil"
        );
    }

    #[test]
    fn 描述srcdoc属性解码后还原为可渲染标签() {
        // 模拟浏览器解析 srcdoc 属性：实体解一层后就是原始 HTML 文本，
        // iframe 内会把 <br> / <hr> 当真标签渲染，而不是显示尖括号源码。
        let desc = "<font color=\"red\">汉语大字典</font><br/>收录<hr color=#AA8A57>";
        let attr = escape_html_attribute(desc);
        let decoded = attr
            .replace("&quot;", "\"")
            .replace("&#x27;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        assert_eq!(decoded, desc);
    }

    #[test]
    fn 词典信息描述使用iframe_srcdoc对齐python版() {
        let out = dict_info_description_html("<font color=\"red\">汉语</font><br/>第二行");
        assert!(out.contains("<p style='margin-top:8px;'><b>描述:</b></p>"), "{}", out);
        assert!(out.contains("<iframe srcdoc=\""), "{}", out);
        assert!(out.contains("width:100%;height:150px"), "{}", out);
        assert!(out.contains("border:1px solid #e0e0e0"), "{}", out);
        assert!(out.contains("&lt;font color=&quot;red&quot;&gt;"), "{}", out);
        assert!(out.contains("&lt;br/&gt;"), "{}", out);
        // 标签必须先转义，不能直接插进主文档
        assert!(!out.contains("<font"), "{}", out);
        assert!(!out.contains("<br/>"), "{}", out);
    }

    #[test]
    fn 属性转义只做单层不二次解码() {
        // 与 Python html.escape 一致：纯转义、不解码。&amp;lt; 不会被误还原
        // 成 < 标签，srcdoc 里只会显示成字面文本。
        let out = escape_html_attribute("A&amp;lt;img src=x onerror=alert(1)&amp;gt;B");
        assert_eq!(out, "A&amp;amp;lt;img src=x onerror=alert(1)&amp;amp;gt;B");
        assert!(!out.contains("<img"), "{}", out);
    }
}
