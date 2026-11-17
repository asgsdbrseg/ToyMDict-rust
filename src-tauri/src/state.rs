//! 全局状态、配置持久化、日志与异体字表。
//!
//! 单文件部署约定：`dict_groups.json` / `toymdict.log` / `custom.css` /
//! `variants.json` 全部相对于 exe 所在目录读写，不写入用户目录、不产生
//! `_internal` 之类的附属文件夹。
//!
//! 配置文件名与 Python 版一致（`dict_groups.json`），可直接复用同一份分组配置；
//! 只读取 `dict_groups.json`，不再兼容旧版 `toymdict.json`。

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

// Python 版 all_dicts 的条目只要求有 id，name 可能缺失；配置整体也可能缺字段。
// 加 #[serde(default)] 后可直接读取 Python 版写出的 dict_groups.json。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DictMeta {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub all_dicts: Vec<DictMeta>,
    /// 分组名 -> 词典绝对路径列表（列表内顺序即显示顺序）。
    /// 用 IndexMap：JSON 里怎么写就怎么显示、怎么写回，不按分组名重排。
    pub groups: IndexMap<String, Vec<String>>,
    pub current_group: String,
    pub excluded: Vec<String>,
}

impl AppConfig {
    /// 首次启动的默认配置：与 Python 版一致，不预建任何分组，
    /// current_group 为空串（由用户显式新建分组并添加词典）。
    pub fn with_defaults() -> Self {
        Self {
            all_dicts: Vec::new(),
            groups: IndexMap::new(),
            current_group: String::new(),
            excluded: Vec::new(),
        }
    }

    /// 读取外部（尤其是 Python 版写出的）配置后补齐不变量：
    /// 不创建默认分组；current_group 只允许指向真实存在的分组，
    /// 否则清空（保持与 Python 版一致：无分组时 current_group 为空串）。
    pub fn normalized(mut self) -> Self {
        if self.current_group.is_empty() || !self.groups.contains_key(&self.current_group) {
            // 有分组则指向首个；无分组则保持空串（绝不创建"默认分组"）。
            self.current_group = self.groups.keys().next().cloned().unwrap_or_default();
        }
        self
    }
}

/// 词典元数据。`mdx::MdxReader` 内部含 `Rc` 块缓存、不是 `Send`，
/// 因此不跨线程持有 reader：每个工作线程各自打开并用 thread_local 复用。
#[derive(Debug, Clone)]
pub struct DictInfo {
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub entry_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitSource {
    pub dict_id: String,
    pub dict_name: String,
    pub idx: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub key: String,
    pub sources: Vec<HitSource>,
}

pub struct AppState {
    /// exe 所在目录
    pub base_dir: PathBuf,
    pub config: Mutex<AppConfig>,
    pub dicts: RwLock<HashMap<String, DictInfo>>,
    pub results: Mutex<Vec<SearchHit>>,
    /// 普通搜索序号，用于丢弃过期结果
    pub search_seq: AtomicU64,
    /// 高级搜索代号，递增即让在途扫描线程自行退出
    pub adv_seq: AtomicU64,
    /// (展开后的正则, 范围)，仅全文搜索时用于词条正文高亮
    pub adv_highlight: Mutex<Option<(String, String)>>,
    pub variants: RwLock<HashMap<String, Vec<String>>>,
    /// 启动期间暂存的报错，前端就绪后由 get_pending_errors 拉走。
    /// 运行期也会 push，前端桥接层会定时拉取并弹窗。
    pub pending_errors: Mutex<Vec<Vec<String>>>,
    /// get_pending_errors 只在首次调用时刷新 UI，避免前端定时拉错误时
    /// 反复重建分组下拉框、打断用户正在做的选择。
    pub ui_refreshed: AtomicBool,
    pub log_path: Mutex<PathBuf>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let base_dir = exe_dir();
        let log_path = base_dir.join("toymdict.log");
        // 每次启动清空，只记录本次运行（与 Python 版一致）
        let _ = fs::write(&log_path, b"");

        let config_path = base_dir.join("dict_groups.json");
        let config_bytes = fs::read(&config_path);
        // 文件不存在 = 首次启动，不算错误；文件存在却解析失败必须弹窗提示，
        // 否则用户会以为词典与分组莫名丢失。
        let config_error = match &config_bytes {
            Ok(b) => serde_json::from_slice::<AppConfig>(b)
                .err()
                .map(|e| format!("dict_groups.json 解析失败：{}（已改用默认配置）", e)),
            Err(_) => None,
        };
        let config = config_bytes
            .ok()
            .and_then(|b| serde_json::from_slice::<AppConfig>(&b).ok())
            .map(AppConfig::normalized)
            .unwrap_or_else(AppConfig::with_defaults);

        let variants_path = base_dir.join("variants.json");
        let variants_bytes = fs::read(&variants_path);
        let variants_error = match &variants_bytes {
            Ok(b) => serde_json::from_slice::<HashMap<String, Vec<String>>>(b)
                .err()
                .map(|e| format!("variants.json 解析失败：{}（异体字功能不可用）", e)),
            Err(_) => None,
        };
        let variants: HashMap<String, Vec<String>> = variants_bytes
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        let state = Arc::new(Self {
            base_dir: base_dir.clone(),
            config: Mutex::new(config),
            dicts: RwLock::new(HashMap::new()),
            results: Mutex::new(Vec::new()),
            search_seq: AtomicU64::new(0),
            adv_seq: AtomicU64::new(0),
            adv_highlight: Mutex::new(None),
            variants: RwLock::new(variants),
            pending_errors: Mutex::new(Vec::new()),
            ui_refreshed: AtomicBool::new(false),
            log_path: Mutex::new(log_path),
        });
        state.log(&format!(
            "ToyMDict 启动 | base_dir={} | variants={} | dicts={}",
            base_dir.display(),
            state.variants.read().len(),
            state.config.lock().all_dicts.len()
        ));
        if !variants_path.exists() {
            state.log("提示：exe 旁未找到 variants.json，异体字功能不可用");
        }
        if !base_dir.join("custom.css").exists() {
            state.log("提示：exe 旁未找到 custom.css，使用内置兜底样式");
        }
        if let Some(e) = config_error {
            state.report_error("配置文件读取失败", &e);
        }
        if let Some(e) = variants_error {
            state.report_error("异体字表读取失败", &e);
        }
        state
    }

    pub fn log(&self, msg: &str) {
        let line = format!("[{}] {}\n", timestamp(), msg);
        if let Some(path) = self.log_path.try_lock() {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path.as_path()) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        eprintln!("{}", line.trim_end());
    }

    pub fn report_error(&self, title: &str, body: &str) {
        self.log(&format!("ERROR {}: {}", title, body));
        self.pending_errors
            .lock()
            .push(vec![title.to_string(), body.to_string(), "warning".to_string()]);
    }

    pub fn save_config(&self) {
        let cfg = self.config.lock().clone();
        let path = self.base_dir.join("dict_groups.json");
        match serde_json::to_vec_pretty(&cfg) {
            Ok(data) => {
                if let Err(e) = fs::write(&path, data) {
                    self.report_error(
                        "保存配置失败",
                        &format!("写入 {} 失败：{}", path.display(), e),
                    );
                }
            }
            Err(e) => self.report_error("保存配置失败", &format!("序列化失败：{}", e)),
        }
    }

    /// 供前端页面使用的自定义样式；文件由用户手动放在 exe 旁。
    pub fn custom_css(&self) -> Vec<u8> {
        fs::read(self.base_dir.join("custom.css")).unwrap_or_else(|_| {
            "/* 将 custom.css 放在 exe 同目录即可覆盖词典字体 */"
                .as_bytes()
                .to_vec()
        })
    }

    pub fn dict_name(&self, id: &str) -> String {
        self.config
            .lock()
            .all_dicts
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| {
                Path::new(id)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(id)
                    .to_string()
            })
    }

    pub fn group_paths(&self) -> Vec<String> {
        let cfg = self.config.lock();
        cfg.groups
            .get(&cfg.current_group)
            .cloned()
            .unwrap_or_default()
    }

    pub fn next_search_seq(&self) -> u64 {
        self.search_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn next_adv_seq(&self) -> u64 {
        self.adv_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn adv_cancelled(&self, gen: u64) -> bool {
        self.adv_seq.load(Ordering::SeqCst) != gen
    }
}

/// exe 所在目录。单文件部署时所有外部文件都放在它旁边。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 把 Unix 秒格式化为 UTC 时间字符串（不引入 chrono）。
/// 日志与「词典信息」里的文件修改时间共用同一套换算。
pub fn format_epoch(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    // 1970-01-01 起算的公历日期（够用于日志）
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_epoch(secs)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 可直接复用_python_版_dict_groups_json() {
        // Python 版 _normalize 写出的结构：all_dicts 条目可能只有 id，
        // groups 是普通对象，current_group 允许为空串。
        let raw = r#"{
            "all_dicts": [{"id": "D:/dict/a.mdx"}],
            "groups": {"我的分组": ["D:/dict/a.mdx"]},
            "excluded": [],
            "current_group": ""
        }"#;
        let cfg = serde_json::from_str::<AppConfig>(raw)
            .unwrap()
            .normalized();
        assert_eq!(cfg.all_dicts.len(), 1);
        assert_eq!(cfg.all_dicts[0].id, "D:/dict/a.mdx");
        assert_eq!(cfg.all_dicts[0].name, ""); // 缺 name 不报错
        assert_eq!(cfg.current_group, "我的分组"); // 空串自动补成首个分组
        assert!(cfg.groups.contains_key("我的分组"));
    }

    #[test]
    fn 分组顺序按_json_写入顺序保留() {
        // 故意用非字典序：BTreeMap 会重排成「乙组/丙组/甲组」，
        // IndexMap 必须原样保留「丙组/甲组/乙组」。
        let raw = r#"{
            "all_dicts": [],
            "groups": {"丙组": [], "甲组": [], "乙组": []},
            "excluded": [],
            "current_group": "丙组"
        }"#;
        let cfg = serde_json::from_str::<AppConfig>(raw).unwrap().normalized();
        let names: Vec<&str> = cfg.groups.keys().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["丙组", "甲组", "乙组"]);

        // 写回 JSON 再读回来，顺序不能变。
        let written = serde_json::to_string_pretty(&cfg).unwrap();
        let back = serde_json::from_str::<AppConfig>(&written).unwrap();
        let names2: Vec<&str> = back.groups.keys().map(|s| s.as_str()).collect();
        assert_eq!(names2, names);

        // 新建追加在末尾；删除必须用 shift_remove，普通 remove 会 swap 打乱顺序。
        let mut cfg2 = cfg.clone();
        cfg2.groups.insert("丁组".to_string(), Vec::new());
        cfg2.groups.shift_remove("甲组");
        let names3: Vec<&str> = cfg2.groups.keys().map(|s| s.as_str()).collect();
        assert_eq!(names3, vec!["丙组", "乙组", "丁组"]);
    }

    #[test]
    fn 缺字段的配置也能读取_且不创建默认分组() {
        let cfg = serde_json::from_str::<AppConfig>(r#"{"groups": {}, "current_group": ""}"#)
            .unwrap()
            .normalized();
        // 不再自动创建"默认分组"：分组保持空，current_group 保持空串。
        assert_eq!(cfg.current_group, "");
        assert!(cfg.groups.is_empty());
        assert!(cfg.excluded.is_empty());

        // with_defaults 首次启动同样不预建分组。
        let fresh = AppConfig::with_defaults();
        assert!(fresh.groups.is_empty());
        assert_eq!(fresh.current_group, "");
    }
}
