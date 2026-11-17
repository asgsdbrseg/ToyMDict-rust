#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! ToyMDict —— 基于 raymanzhang/mdx + Tauri 2 的重构版。
//!
//! 单文件部署：前端资源全部编译进 exe；运行时只从 exe 同目录读取
//! `custom.css`、`variants.json`（用户手动放置），并在此目录写
//! `dict_groups.json`（配置，与 Python 版同名同格式，可直接复用）与
//! `toymdict.log`（每次启动清空）。

mod dict;
mod hotkey;
mod html;
mod state;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, FilePath};

use state::{AppConfig, AppState, DictMeta};
use dict::AdvCtx;

/// 统一的前端调用通道：Python 版的 evaluate_js 全部转成 js_call 事件，
/// 由 tauri-bridge.js 分派到 app.js 里同名的全局函数。
#[derive(Clone, Serialize)]
struct JsCall {
    #[serde(rename = "fn")]
    f: String,
    args: Vec<Value>,
}

pub fn call_js(app: &AppHandle, f: &str, args: Vec<Value>) {
    let _ = app.emit(
        "js_call",
        JsCall {
            f: f.to_string(),
            args,
        },
    );
}

/// WebView2 上自定义协议映射为 http://<scheme>.localhost；其它平台用原生 scheme。
fn protocol_base() -> String {
    if cfg!(windows) {
        "http://mdx.localhost".to_string()
    } else {
        "mdx://localhost".to_string()
    }
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn dec(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn query_map(q: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(q) = q {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            let k = it.next().unwrap_or("");
            let v = it.next().unwrap_or("");
            map.insert(dec(k), dec(v));
        }
    }
    map
}

fn response(status: u16, mime: &str, body: Vec<u8>) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(tauri::http::StatusCode::from_u16(status).unwrap())
        .header("Content-Type", mime)
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap()
}

fn not_found(msg: &str) -> tauri::http::Response<Vec<u8>> {
    response(404, "text/plain; charset=utf-8", msg.as_bytes().to_vec())
}

/// `mdx://` 自定义协议：词条页 / 静态资源 / MDD 资源。
fn handle_mdx(state: &Arc<AppState>, uri: &tauri::http::Uri) -> tauri::http::Response<Vec<u8>> {
    let path = uri.path();
    let query = query_map(uri.query());
    let base = protocol_base();

    if let Some(rest) = path.strip_prefix("/static/") {
        let (mime, body): (&str, Vec<u8>) = match rest {
            "custom.css" => ("text/css; charset=utf-8", state.custom_css()),
            "entry.css" => (
                "text/css; charset=utf-8",
                include_bytes!("../../dist/entry.css").to_vec(),
            ),
            "entry.js" => (
                "application/javascript; charset=utf-8",
                include_bytes!("../../dist/entry.js").to_vec(),
            ),
            "style.css" => (
                "text/css; charset=utf-8",
                include_bytes!("../../dist/style.css").to_vec(),
            ),
            "app.js" => (
                "application/javascript; charset=utf-8",
                include_bytes!("../../dist/app.js").to_vec(),
            ),
            _ => return not_found("未知静态资源"),
        };
        return response(200, mime, body);
    }

    if let Some(rest) = path.strip_prefix("/entry/") {
        let dict_id = dec(rest);
        let key = query.get("k").cloned().unwrap_or_default();
        let idx = query
            .get("idx")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(-1);
        let ifid = query
            .get("ifid")
            .cloned()
            .unwrap_or_else(|| "dict-iframe-0".to_string());
        return match dict::get_content(&dict_id, &key, idx) {
            Ok(raw) => response(
                200,
                "text/html; charset=utf-8",
                html::build_entry_html(&raw, &enc(&dict_id), &ifid, &base).into_bytes(),
            ),
            Err(e) => {
                let msg = format!("{} #{}：{}", dict_id, idx, e);
                state.log(&format!("词条加载失败 {}", msg));
                state.report_error("词条加载失败", &msg);
                not_found(&e)
            }
        };
    }

    if let Some(rest) = path.strip_prefix("/mdd/") {
        let mut parts = rest.splitn(2, '/');
        let dict_id = dec(parts.next().unwrap_or(""));
        let file = dec(parts.next().unwrap_or(""));
        if file.is_empty() {
            return not_found("资源路径为空");
        }
        return match dict::get_resource(&dict_id, &file) {
            Ok(Some((bytes, mime))) => response(200, &mime, bytes),
            Ok(None) => not_found(&format!("MDD 中无此资源: {}", file)),
            Err(e) => not_found(&e),
        };
    }

    not_found("未知的 mdx 协议路径")
}

fn refresh_ui(state: &Arc<AppState>, app: &AppHandle) {
    let cfg = state.config.lock();
    let groups: Vec<Value> = cfg
        .groups
        .keys()
        .map(|n| json!({ "name": n }))
        .collect();
    let data = json!({ "groups": groups, "current": cfg.current_group });
    drop(cfg);
    call_js(app, "updateUI", vec![data]);
}

fn init_group_view_inner(state: &Arc<AppState>, app: &AppHandle) {
    let cfg = state.config.lock();
    let current = cfg.current_group.clone();
    let active: Vec<String> = cfg.groups.get(&current).cloned().unwrap_or_default();
    let excluded = cfg.excluded.clone();
    let all: Vec<Value> = cfg
        .all_dicts
        .iter()
        .map(|d| {
            let status = if active.contains(&d.id) {
                "active"
            } else if excluded.contains(&d.id) {
                "excluded"
            } else {
                "none"
            };
            json!({ "id": d.id, "name": d.name, "status": status })
        })
        .collect();
    let groups: Vec<Value> = cfg
        .groups
        .iter()
        .map(|(name, ids)| {
            let dicts: Vec<Value> = ids
                .iter()
                .map(|id| {
                    let n = cfg
                        .all_dicts
                        .iter()
                        .find(|d| &d.id == id)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| state.dict_name(id));
                    json!({ "id": id, "name": n })
                })
                .collect();
            json!({ "name": name, "dicts": dicts })
        })
        .collect();
    drop(cfg);
    call_js(app, "renderGroupView", vec![json!(all), json!(groups), json!(current)]);
}

fn show_entry_inner(state: &Arc<AppState>, app: &AppHandle, index: usize) {
    let item = {
        let results = state.results.lock();
        match results.get(index) {
            Some(v) => v.clone(),
            None => return,
        }
    };
    let highlight = state.adv_highlight.lock().clone();
    let hl = match &highlight {
        Some((expr, scope)) if scope == "fulltext" && !expr.is_empty() => {
            format!("&hl={}", enc(expr))
        }
        _ => String::new(),
    };
    let base = protocol_base();
    let render: Vec<Value> = item
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let url = format!(
                "{}/entry/{}?ifid=dict-iframe-{}&idx={}&k={}{}",
                base,
                enc(&s.dict_id),
                i,
                s.idx,
                enc(&item.key),
                hl
            );
            json!({ "dict_name": s.dict_name, "url": url })
        })
        .collect();
    call_js(app, "setContent", vec![json!(render)]);
}

fn import_paths(state: &Arc<AppState>, app: &AppHandle, paths: Vec<String>) {
    let mut added = 0usize;
    let mut skipped = 0usize;
    for p in paths {
        let abs = dict::normalize_path(&p);
        if !abs.to_ascii_lowercase().ends_with(".mdx") || !Path::new(&abs).exists() {
            skipped += 1;
            continue;
        }
        match dict::load_dict(state, &abs) {
            Ok(info) => {
                let mut cfg = state.config.lock();
                // 与 Python 版一致：导入的词典只登记到"全部词典"(all_dicts)，
                // 不自动加入当前分组；由用户在分组视图中显式添加。
                if !cfg.all_dicts.iter().any(|d| d.id == abs) {
                    cfg.all_dicts.push(DictMeta {
                        id: abs.clone(),
                        name: info.name.clone(),
                    });
                }
                cfg.excluded.retain(|x| x != &abs);
                drop(cfg);
                added += 1;
                state.log(&format!("导入词典: {} ({} 条)", info.name, info.entry_count));
            }
            Err(e) => state.report_error("导入词典失败", &e),
        }
    }
    if skipped > 0 {
        state.report_error(
            "部分文件未导入",
            &format!("{} 个文件不是 .mdx 或已不存在，已跳过", skipped),
        );
    }
    if added > 0 {
        state.save_config();
        refresh_ui(state, app);
        init_group_view_inner(state, app);
    }
}

fn path_string(p: &FilePath) -> String {
    match p {
        FilePath::Path(pb) => pb.to_string_lossy().into_owned(),
        FilePath::Url(u) => u
            .to_file_path()
            .map(|pb| pb.to_string_lossy().into_owned())
            .unwrap_or_else(|_| u.to_string()),
    }
}

// ------------------------------------------------------------------ commands

#[tauri::command]
fn get_protocol_base() -> String {
    protocol_base()
}

/// 手动打开 DevTools。Cargo 里给 tauri 启用了 devtools feature，
/// 但启动流程（setup）绝不调用它——只有用户按 F12 / Ctrl+Shift+I 才会走到这里。
#[tauri::command]
fn open_devtools(app: AppHandle) -> bool {
    match app.get_webview_window("main") {
        Some(w) => {
            w.open_devtools();
            true
        }
        None => false,
    }
}

/// 双击 Ctrl 取词开关（系统级全局快捷键）。
#[tauri::command]
fn get_global_hotkey_enabled() -> bool {
    hotkey::is_enabled()
}

#[tauri::command]
fn set_global_hotkey_enabled(enabled: bool) -> bool {
    hotkey::set_enabled(enabled);
    hotkey::is_enabled()
}

#[tauri::command]
fn open_file(app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    app.dialog()
        .file()
        .add_filter("MDict 词典", &["mdx"])
        .pick_files(move |files: Option<Vec<FilePath>>| {
            let Some(files) = files else { return };
            let paths: Vec<String> = files.iter().map(path_string).collect();
            let ap = app.clone();
            std::thread::spawn(move || import_paths(&st, &ap, paths));
        });
}

#[tauri::command]
fn open_folder(app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    app.dialog().file().pick_folder(move |folder| {
        let Some(folder) = folder else { return };
        let root = path_string(&folder);
        let mut paths = Vec::new();
        collect_mdx(Path::new(&root), &mut paths, 0);
        let ap = app.clone();
        std::thread::spawn(move || import_paths(&st, &ap, paths));
    });
}

fn collect_mdx(dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 3 {
        return;
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_mdx(&p, out, depth + 1);
            } else if p
                .extension()
                .map(|x| x.to_string_lossy().eq_ignore_ascii_case("mdx"))
                .unwrap_or(false)
            {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
}

#[tauri::command]
fn get_pending_errors(app: AppHandle, state: State<'_, Arc<AppState>>) -> Vec<Vec<String>> {
    let st = state.inner();
    // 只在首次拉取时初始化 UI。前端会定时轮询本命令取运行期错误，
    // 若每次都 refresh_ui，会反复重建分组下拉框、打断用户正在做的选择。
    if !st.ui_refreshed.swap(true, Ordering::SeqCst) {
        refresh_ui(st, &app);
    }
    let mut pending = st.pending_errors.lock();
    std::mem::take(&mut *pending)
}

#[tauri::command]
fn switch_group(name: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    {
        let mut cfg = st.config.lock();
        cfg.current_group = name.clone();
    }
    st.save_config();
    refresh_ui(&st, &app);
    init_group_view_inner(&st, &app);
    // 后台预加载当前分组词典，失败路径从配置里清掉
    std::thread::spawn(move || {
        for p in st.group_paths() {
            if let Err(e) = dict::load_dict(&st, &p) {
                st.report_error("切换分组时词典加载失败", &format!("{}：{}", p, e));
            }
        }
    });
}

#[tauri::command]
fn search(keyword: String, use_variants: bool, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner().clone();
    let seq = st.next_search_seq();
    st.adv_seq.fetch_add(1, Ordering::SeqCst); // 作废旧流式搜索
    *st.adv_highlight.lock() = None;
    std::thread::spawn(move || {
        let results = dict::search(&st, &keyword, use_variants);
        if st.search_seq.load(Ordering::SeqCst) != seq {
            return;
        }
        *st.results.lock() = results.clone();
        call_js(&app, "updateResults", vec![json!(results)]);
        // 结果改为按长度排序后，精确匹配不一定还在第一条：
        // 只要结果里存在精确匹配就自动展示它，避免内容区留空。
        if let Some(i) = results
            .iter()
            .position(|r| r.key.eq_ignore_ascii_case(&keyword))
        {
            show_entry_inner(&st, &app, i);
        }
    });
}

#[tauri::command]
fn show_entry(index: usize, app: AppHandle, state: State<'_, Arc<AppState>>) {
    show_entry_inner(state.inner(), &app, index);
}

#[tauri::command]
fn init_group_view(app: AppHandle, state: State<'_, Arc<AppState>>) {
    init_group_view_inner(state.inner(), &app);
}

#[tauri::command]
fn add_group(name: String, app: AppHandle, state: State<'_, Arc<AppState>>) -> Value {
    let st = state.inner();
    let name = name.trim().to_string();
    if name.is_empty() {
        return json!({ "ok": false, "error": "分组名称不能为空" });
    }
    {
        let mut cfg = st.config.lock();
        if cfg.groups.contains_key(&name) {
            return json!({ "ok": false, "error": format!("分组已存在：{}", name) });
        }
        cfg.groups.insert(name.clone(), Vec::new());
    }
    st.save_config();
    refresh_ui(st, &app);
    init_group_view_inner(st, &app);
    json!({ "ok": true, "name": name })
}

#[tauri::command]
fn delete_group(app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner();
    {
        let mut cfg = st.config.lock();
        let cur = cfg.current_group.clone();
        // shift_remove 保留其余分组的原有顺序（IndexMap::remove 是 swap_remove，会打乱顺序）
        cfg.groups.shift_remove(&cur);
        // 与 Python 版一致：当前分组置空，不再自动重建"默认分组"。
        // 需要搜索时由用户显式新建分组并添加词典。
        cfg.current_group.clear();
    }
    st.save_config();
    refresh_ui(st, &app);
    init_group_view_inner(st, &app);
}

#[tauri::command]
fn add_dict_to_group(dict_id: String, app: AppHandle, state: State<'_, Arc<AppState>>) -> Value {
    let st = state.inner();
    let abs = dict::normalize_path(&dict_id);
    if let Err(e) = dict::load_dict(st, &abs) {
        // 前端 add_dict_to_group 只发命令不检查结果，这里同时走错误弹窗通道。
        st.report_error("添加词典到分组失败", &format!("{}：{}", abs, e));
        return json!({ "ok": false, "error": e });
    }
    {
        let mut cfg = st.config.lock();
        if !cfg.groups.contains_key(&cfg.current_group.clone()) {
            if let Some(first) = cfg.groups.keys().next().cloned() {
                cfg.current_group = first;
            } else {
                return json!({ "ok": false, "error": "请先新建一个分组" });
            }
        }
        let group = cfg.current_group.clone();
        let list = cfg.groups.entry(group).or_default();
        if !list.contains(&abs) {
            list.push(abs.clone());
        }
        cfg.excluded.retain(|x| x != &abs);
    }
    st.save_config();
    init_group_view_inner(st, &app);
    json!({ "ok": true })
}

#[tauri::command]
fn remove_dict_from_group(dict_id: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner();
    let abs = dict::normalize_path(&dict_id);
    {
        let mut cfg = st.config.lock();
        let group = cfg.current_group.clone();
        if let Some(list) = cfg.groups.get_mut(&group) {
            list.retain(|x| x != &abs);
        }
    }
    st.save_config();
    init_group_view_inner(st, &app);
}

#[tauri::command]
fn exclude_dict(dict_id: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner();
    let abs = dict::normalize_path(&dict_id);
    {
        let mut cfg = st.config.lock();
        for list in cfg.groups.values_mut() {
            list.retain(|x| x != &abs);
        }
        if !cfg.excluded.contains(&abs) {
            cfg.excluded.push(abs);
        }
    }
    st.save_config();
    init_group_view_inner(st, &app);
}

#[tauri::command]
fn reload_excluded_dict(dict_id: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner();
    let abs = dict::normalize_path(&dict_id);
    if let Err(e) = dict::load_dict(st, &abs) {
        st.report_error("恢复词典失败", &e);
    }
    {
        let mut cfg = st.config.lock();
        cfg.excluded.retain(|x| x != &abs);
    }
    st.save_config();
    init_group_view_inner(st, &app);
}

#[tauri::command]
fn move_dict(dict_id: String, action: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    let st = state.inner();
    let abs = dict::normalize_path(&dict_id);
    {
        let mut cfg = st.config.lock();
        let group = cfg.current_group.clone();
        if let Some(ids) = cfg.groups.get_mut(&group) {
            if let Some(i) = ids.iter().position(|x| x == &abs) {
                match action.as_str() {
                    "up" if i > 0 => ids.swap(i, i - 1),
                    "down" if i + 1 < ids.len() => ids.swap(i, i + 1),
                    "top" => {
                        let id = ids.remove(i);
                        ids.insert(0, id);
                    }
                    "bottom" => {
                        let id = ids.remove(i);
                        ids.push(id);
                    }
                    _ => {}
                }
            }
        }
    }
    st.save_config();
    init_group_view_inner(st, &app);
}

/// 词典信息弹窗：HTML 结构、字段、措辞与样式对齐 Python 版 `get_dict_info`。
#[tauri::command]
fn get_dict_info(dict_id: String, app: AppHandle, state: State<'_, Arc<AppState>>) {
    // Python 版用 f"{num:,}" 显示词条数，这里手写千分位。
    fn thousands(n: u64) -> String {
        let s = n.to_string();
        let mut out = String::with_capacity(s.len() + s.len() / 3);
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out
    }
    // 前端对超长 Header 值做 300 字符截断（与 Python 版 v[:300] + "..." 一致）
    fn clip(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            return s.to_string();
        }
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out
    }
    // Python 的 html.escape(quote=True)：所有字段（含描述 iframe 的 srcdoc）
    // 都用它做 HTML 属性 / 文本转义。
    fn esc(s: &str) -> String {
        html::escape_html_attribute(s)
    }

    let st = state.inner().clone();
    let ap = app.clone();
    std::thread::spawn(move || {
        let abs = dict::normalize_path(&dict_id);
        let name = st.dict_name(&abs);
        let meta = dict::dict_metadata(&abs);
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!(
            "<p><b>文件路径:</b> <code style='word-break:break-all;'>{}</code></p>",
            esc(&abs)
        ));

        if meta.error.is_empty() {
            let label = match meta.title_source.as_str() {
                "header" => "（来自Header）",
                "filename" => "（来自文件名）",
                _ => "",
            };
            parts.push(format!(
                "<p style='margin-top:6px;'><b>标题:</b> {} <span style='color:#888;font-size:16px;'>{}</span></p>",
                esc(&meta.title), label
            ));
            if !meta.description.trim().is_empty() {
                // 与 Python 版 get_dict_info 完全一致：描述放进 iframe 的 srcdoc，
                // 属性转义后由浏览器再解一层，iframe 内按真正的 HTML 渲染。
                parts.push(html::dict_info_description_html(&meta.description));
            }

            // 技术参数只保留 Python 版的 6 项，空值不显示。
            let mut tech: Vec<String> = Vec::new();
            if !meta.version.is_empty() {
                tech.push(format!("<li><b>MDX版本:</b> v{}</li>", esc(&meta.version)));
            }
            if !meta.encoding.is_empty() {
                tech.push(format!("<li><b>编码:</b> {}</li>", esc(&meta.encoding)));
            }
            if !meta.encrypted_raw.is_empty() {
                tech.push(format!("<li><b>加密:</b> {}</li>", esc(&meta.encrypted_raw)));
            }
            if !meta.creation_date.is_empty() {
                tech.push(format!("<li><b>创建日期:</b> {}</li>", esc(&meta.creation_date)));
            }
            if meta.entry_count > 0 {
                tech.push(format!("<li><b>词条数:</b> {}</li>", thousands(meta.entry_count)));
            }
            if !meta.mdd_files.is_empty() {
                tech.push(format!("<li><b>MDD文件:</b> {}</li>", meta.mdd_files.len()));
            }
            if !tech.is_empty() {
                parts.push("<p style='margin-top:8px;'><b>技术参数:</b></p>".to_string());
                parts.push(format!(
                    "<ul style='margin-left:20px;font-size:16px;'>{}</ul>",
                    tech.join("")
                ));
            }

            if !meta.all_fields.is_empty() {
                parts.push(
                    "<details style='margin-top:8px;'>\
                     <summary style='cursor:pointer;color:#2196F3;font-size:18px;'>全部 Header 字段</summary>\
                     <div style='overflow:auto;background:#f5f5f5;padding:8px;border-radius:4px;font-size:16px;margin-top:6px;'>"
                        .to_string(),
                );
                for (k, v) in &meta.all_fields {
                    parts.push(format!(
                        "<p style='margin:2px 0;'><b>{}:</b> <code style='word-break:break-all;'>{}</code></p>",
                        esc(k),
                        esc(&clip(v, 300))
                    ));
                }
                parts.push("</div></details>".to_string());
            }
        } else {
            parts.push(
                "<p style='color:#888;margin-top:8px;'>词典文件未加载，无法读取 Header 信息。</p>"
                    .to_string(),
            );
            parts.push(format!(
                "<p style='color:#f44336;margin-top:8px;'>⚠ {}</p>",
                esc(&meta.error)
            ));
        }

        if !meta.error.is_empty() {
            st.report_error("读取词典信息失败", &format!("{}：{}", name, meta.error));
        }
        call_js(&ap, "showDictInfoModal", vec![json!(name), json!(parts.join(""))]);
    });
}

#[tauri::command]
fn get_advanced_search_dicts(state: State<'_, Arc<AppState>>) -> Vec<Value> {
    let st = state.inner();
    let cfg = st.config.lock();
    let group = cfg.current_group.clone();
    let paths = cfg.groups.get(&group).cloned().unwrap_or_default();
    let names: HashMap<String, String> = cfg
        .all_dicts
        .iter()
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect();
    drop(cfg);
    paths
        .iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| {
            json!({
                "id": p,
                "name": names.get(p).cloned().unwrap_or_else(|| st.dict_name(p))
            })
        })
        .collect()
}

#[tauri::command]
fn advanced_search(
    pattern: String,
    scope: String,
    use_variants: bool,
    dict_ids: Vec<String>,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Value {
    let st = state.inner().clone();
    let pattern = pattern.trim().to_string();
    if pattern.is_empty() {
        return json!({ "ok": false, "error": "请输入搜索表达式" });
    }
    if scope != "entry" && scope != "fulltext" {
        return json!({ "ok": false, "error": "未知的搜索范围" });
    }
    let ids: Vec<String> = dict_ids
        .into_iter()
        .map(|p| dict::normalize_path(&p))
        .filter(|p| !p.is_empty())
        .collect();
    if ids.is_empty() {
        return json!({ "ok": false, "error": "请至少勾选一本词典" });
    }
    let variants = st.variants.read().clone();
    let (re, expanded) = match dict::compile_pattern(&pattern, use_variants, &variants) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let gen = st.next_adv_seq();
    st.search_seq.fetch_add(1, Ordering::SeqCst);
    *st.results.lock() = Vec::new();
    *st.adv_highlight.lock() = Some((expanded.clone(), scope.clone()));
    call_js(&app, "clearAdvancedResults", vec![json!(expanded.clone())]);

    let ctx = Arc::new(AdvCtx {
        state: st,
        app,
        gen,
        total: ids.len(),
        done: Arc::new(AtomicUsize::new(0)),
        hits: Arc::new(AtomicUsize::new(0)),
        re,
        scope,
    });
    for path in &ids {
        let c = ctx.clone();
        let p = path.clone();
        std::thread::spawn(move || dict::scan_one(c, p));
    }
    json!({ "ok": true, "total": ids.len(), "expanded": expanded })
}

#[tauri::command]
fn cancel_advanced_search(app: AppHandle, state: State<'_, Arc<AppState>>) -> Value {
    state.adv_seq.fetch_add(1, Ordering::SeqCst);
    call_js(&app, "finishAdvancedStatus", vec![]);
    json!({ "ok": true })
}

fn main() {
    let state = AppState::new();

    // 启动期清理失效词典路径
    {
        let mut cfg = state.config.lock();
        let before = cfg.all_dicts.len();
        cfg.all_dicts.retain(|d| Path::new(&d.id).exists());
        for list in cfg.groups.values_mut() {
            list.retain(|p| Path::new(p).exists());
        }
        cfg.excluded.retain(|p| Path::new(p).exists());
        if cfg.groups.is_empty() {
            *cfg = AppConfig::with_defaults();
        }
        if before != cfg.all_dicts.len() {
            state.report_error(
                "词典文件失效",
                &format!(
                    "已清理 {} 条不存在的词典路径（文件可能被移动或重命名）",
                    before - cfg.all_dicts.len()
                ),
            );
        }
    }
    state.save_config();

    let st_for_protocol = state.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .register_uri_scheme_protocol("mdx", move |_ctx, request| {
            handle_mdx(&st_for_protocol, request.uri())
        })
        .invoke_handler(tauri::generate_handler![
            get_protocol_base,
            open_devtools,
            get_global_hotkey_enabled,
            set_global_hotkey_enabled,
            open_file,
            open_folder,
            get_pending_errors,
            switch_group,
            search,
            show_entry,
            init_group_view,
            add_group,
            delete_group,
            add_dict_to_group,
            remove_dict_from_group,
            exclude_dict,
            reload_excluded_dict,
            move_dict,
            get_dict_info,
            get_advanced_search_dicts,
            advanced_search,
            cancel_advanced_search,
        ])
        .setup(move |app| {
            let st = app.state::<Arc<AppState>>().inner().clone();
            let ap = app.handle().clone();
            // 系统级全局快捷键：双击 Ctrl 取词。默认常开，非 Windows 平台自动降级。
            // 注意这里只安装键盘钩子，不打开 DevTools。
            hotkey::start(app.handle().clone());
            std::thread::spawn(move || {
                for p in st.group_paths() {
                    if let Err(e) = dict::load_dict(&st, &p) {
                        st.report_error("词典加载失败", &format!("{}: {}", p, e));
                    }
                }
                refresh_ui(&st, &ap);
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 ToyMDict 失败");
}
