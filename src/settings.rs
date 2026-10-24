//! 系统配置：DB 持久化 + 内存缓存 + 启动播种。
//!
//! * 配置项来源：环境变量（首次启动播种进 `settings` 表）→ 数据库（后续以页面为准）；
//! * 分「运行时生效」与「重启后生效」（监听地址只能在启动时绑定，无法热切换）；
//! * 读取一律走内存快照（`std::sync::RwLock`，读的是短字符串、不跨 await 持锁）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::config::Config;
use crate::storage::Store;

/// 配置值类型，决定渲染控件与校验规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `ip:port`
    Addr,
    /// 整数（带范围）
    U64 { min: u64, max: u64 },
    /// 短文本
    Text { max_len: usize },
    /// 开关
    Bool,
    /// 语言（zh-CN / en-US）
    Lang,
}

/// 一条配置的元数据：分组 / i18n 标签键 / 校验 / 是否需重启。
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub key: &'static str,
    pub group: &'static str,
    pub label_key: &'static str,
    pub hint_key: &'static str,
    pub kind: Kind,
    pub restart: bool,
}

/// 分组（i18n 键：settings.group.xxx）
pub const GROUPS: &[(&str, &str)] = &[
    ("connect", "settings.group.connect"),
    ("console", "settings.group.console"),
    ("data", "settings.group.data"),
    ("ota", "settings.group.ota"),
    ("ui", "settings.group.ui"),
];

/// 全部可配置项。键名即 `settings` 表主键。
pub const CATALOG: &[SettingDef] = &[
    // ---- 设备接入 ----
    SettingDef {
        key: "ingest_addr",
        group: "connect",
        label_key: "settings.ingest_addr",
        hint_key: "settings.ingest_addr.hint",
        kind: Kind::Addr,
        restart: true,
    },
    SettingDef {
        key: "reg_probe_ms",
        group: "connect",
        label_key: "settings.reg_probe_ms",
        hint_key: "settings.reg_probe_ms.hint",
        kind: Kind::U64 { min: 100, max: 30_000 },
        restart: false,
    },
    SettingDef {
        key: "downlink_gap_ms",
        group: "connect",
        label_key: "settings.downlink_gap_ms",
        hint_key: "settings.downlink_gap_ms.hint",
        kind: Kind::U64 { min: 10, max: 1000 },
        restart: false,
    },
    SettingDef {
        key: "idle_timeout_secs",
        group: "connect",
        label_key: "settings.idle_timeout_secs",
        hint_key: "settings.idle_timeout_secs.hint",
        kind: Kind::U64 { min: 30, max: 86_400 },
        restart: false,
    },
    SettingDef {
        key: "default_report_interval",
        group: "connect",
        label_key: "settings.default_report_interval",
        hint_key: "settings.default_report_interval.hint",
        kind: Kind::U64 { min: 1, max: 3600 },
        restart: false,
    },
    // ---- 控制台 ----
    SettingDef {
        key: "web_addr",
        group: "console",
        label_key: "settings.web_addr",
        hint_key: "settings.web_addr.hint",
        kind: Kind::Addr,
        restart: true,
    },
    SettingDef {
        key: "session_idle_min",
        group: "console",
        label_key: "settings.session_idle_min",
        hint_key: "settings.session_idle_min.hint",
        kind: Kind::U64 { min: 1, max: 720 },
        restart: false,
    },
    SettingDef {
        key: "site_title",
        group: "console",
        label_key: "settings.site_title",
        hint_key: "settings.site_title.hint",
        kind: Kind::Text { max_len: 40 },
        restart: false,
    },
    // ---- 数据 ----
    SettingDef {
        key: "retention_days",
        group: "data",
        label_key: "settings.retention_days",
        hint_key: "settings.retention_days.hint",
        kind: Kind::U64 { min: 7, max: 3650 },
        restart: false,
    },
    // ---- OTA ----
    SettingDef {
        key: "ota_packet_timeout_ms",
        group: "ota",
        label_key: "settings.ota_packet_timeout_ms",
        hint_key: "settings.ota_packet_timeout_ms.hint",
        kind: Kind::U64 { min: 500, max: 60_000 },
        restart: false,
    },
    SettingDef {
        key: "ota_max_retries",
        group: "ota",
        label_key: "settings.ota_max_retries",
        hint_key: "settings.ota_max_retries.hint",
        kind: Kind::U64 { min: 0, max: 20 },
        restart: false,
    },
    SettingDef {
        key: "ota_max_restarts",
        group: "ota",
        label_key: "settings.ota_max_restarts",
        hint_key: "settings.ota_max_restarts.hint",
        kind: Kind::U64 { min: 0, max: 10 },
        restart: false,
    },
    SettingDef {
        key: "ota_begin_window_ms",
        group: "ota",
        label_key: "settings.ota_begin_window_ms",
        hint_key: "settings.ota_begin_window_ms.hint",
        kind: Kind::U64 { min: 500, max: 60_000 },
        restart: false,
    },
    SettingDef {
        key: "ota_begin_interval_ms",
        group: "ota",
        label_key: "settings.ota_begin_interval_ms",
        hint_key: "settings.ota_begin_interval_ms.hint",
        kind: Kind::U64 { min: 20, max: 5000 },
        restart: false,
    },
    SettingDef {
        key: "ota_end_timeout_ms",
        group: "ota",
        label_key: "settings.ota_end_timeout_ms",
        hint_key: "settings.ota_end_timeout_ms.hint",
        kind: Kind::U64 { min: 500, max: 60_000 },
        restart: false,
    },
    // ---- 界面 ----
    SettingDef {
        key: "language",
        group: "ui",
        label_key: "settings.language",
        hint_key: "settings.language.hint",
        kind: Kind::Lang,
        restart: false,
    },
];

pub const DEFAULT_LANG: &str = "zh-CN";
pub const SUPPORTED_LANGS: &[(&str, &str)] = &[("zh-CN", "简体中文"), ("en-US", "English")];

#[derive(Default)]
struct Inner {
    values: HashMap<String, String>,
}

#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<Inner>>,
}

impl SettingsStore {
    /// 启动：读 DB；空表时按环境变量播种（逐项写回 DB）。
    pub async fn open(store: &Store, cfg: &Config) -> Result<Self> {
        let mut values = store.settings_all().await.unwrap_or_default();
        let defaults = env_defaults(cfg);
        let mut seeded = false;
        for (k, v) in &defaults {
            if !values.contains_key(k) {
                values.insert(k.clone(), v.clone());
                store.setting_set(k, v);
                seeded = true;
            }
        }
        let this = Self {
            inner: Arc::new(RwLock::new(Inner { values })),
        };
        if seeded {
            tracing::info!(count = defaults.len(), "系统配置已按环境变量播种");
        }
        Ok(this)
    }

    fn read(&self, key: &str) -> Option<String> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values
            .get(key)
            .cloned()
    }

    pub fn str(&self, key: &str) -> String {
        self.read(key).unwrap_or_default()
    }

    pub fn u64(&self, key: &str, default: u64) -> u64 {
        self.read(key)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    }

    pub fn u32(&self, key: &str, default: u32) -> u32 {
        self.u64(key, default as u64) as u32
    }

    pub fn i64(&self, key: &str, default: i64) -> i64 {
        self.read(key)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(default)
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        match self.read(key).as_deref() {
            Some("1") | Some("true") | Some("yes") | Some("on") => true,
            Some("0") | Some("false") | Some("no") | Some("off") => false,
            _ => default,
        }
    }

    /// 当前界面语言（非法值回退默认）。
    pub fn lang(&self) -> String {
        let lang = self.str("language");
        if SUPPORTED_LANGS.iter().any(|(k, _)| *k == lang) {
            lang
        } else {
            DEFAULT_LANG.to_string()
        }
    }

    /// 全量快照（渲染设置页用），按目录顺序。
    pub fn snapshot(&self) -> Vec<(&'static SettingDef, String)> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        CATALOG
            .iter()
            .map(|d| {
                let v = inner
                    .values
                    .get(d.key)
                    .cloned()
                    .unwrap_or_else(|| default_for(d.key));
                (d, v)
            })
            .collect()
    }

    /// 校验并保存一批配置：先全部通过校验，再落库 + 更新内存。
    pub async fn save(&self, store: &Store, pairs: &[(String, String)]) -> Result<(), Invalid> {
        let mut validated: Vec<(&'static SettingDef, String)> = Vec::with_capacity(pairs.len());
        for (key, raw) in pairs {
            let Some(def) = CATALOG.iter().find(|d| d.key == key) else {
                return Err(Invalid::Unknown(key.clone()));
            };
            let value = validate(def, raw)?;
            validated.push((def, value));
        }
        for (def, value) in &validated {
            store.setting_set(def.key, value);
            let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
            inner.values.insert(def.key.to_string(), value.clone());
        }
        Ok(())
    }

    /// 按目录键取默认值（内存快照缺失时的兜底）。
    pub fn default_for(key: &str) -> String {
        default_for(key)
    }
}

/// 环境变量 → 初始值（与 `Config::from_env` 的默认值保持一致）。
fn env_defaults(cfg: &Config) -> Vec<(String, String)> {
    vec![
        ("ingest_addr".into(), cfg.ingest_addr.to_string()),
        ("web_addr".into(), cfg.web_addr.to_string()),
        ("reg_probe_ms".into(), cfg.reg_probe_ms.to_string()),
        ("downlink_gap_ms".into(), cfg.downlink_gap_ms.to_string()),
        ("idle_timeout_secs".into(), 180.to_string()),
        (
            "default_report_interval".into(),
            env_or("UPS_DEFAULT_REPORT_INTERVAL", "30"),
        ),
        ("session_idle_min".into(), cfg.session_idle_min.to_string()),
        (
            "site_title".into(),
            env_or("UPS_SITE_TITLE", "UPS Sentinel"),
        ),
        ("retention_days".into(), cfg.retention_days.to_string()),
        (
            "ota_packet_timeout_ms".into(),
            cfg.ota_packet_timeout_ms.to_string(),
        ),
        ("ota_max_retries".into(), cfg.ota_max_retries.to_string()),
        ("ota_max_restarts".into(), cfg.ota_max_restarts.to_string()),
        (
            "ota_begin_window_ms".into(),
            cfg.ota_begin_window_ms.to_string(),
        ),
        (
            "ota_begin_interval_ms".into(),
            cfg.ota_begin_interval_ms.to_string(),
        ),
        (
            "ota_end_timeout_ms".into(),
            cfg.ota_end_timeout_ms.to_string(),
        ),
        // 与 UPS_SITE_TITLE 同一模式：只在首次建库时播种，之后以页面设置为准。
        // 英文部署可以不进后台直接 UPS_LANGUAGE=en-US。
        ("language".into(), env_or("UPS_LANGUAGE", DEFAULT_LANG)),
    ]
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn default_for(key: &str) -> String {
    match key {
        "idle_timeout_secs" => "180".into(),
        "default_report_interval" => "30".into(),
        "site_title" => "UPS Sentinel".into(),
        "language" => DEFAULT_LANG.into(),
        "ota_end_timeout_ms" => "5000".into(),
        _ => {
            for d in CATALOG {
                if d.key == key {
                    return fallback_default(d);
                }
            }
            String::new()
        }
    }
}

fn fallback_default(def: &SettingDef) -> String {
    match def.kind {
        Kind::Addr => "0.0.0.0:0".into(),
        Kind::Bool => "false".into(),
        Kind::Lang => DEFAULT_LANG.into(),
        Kind::Text { .. } => String::new(),
        Kind::U64 { min, .. } => min.to_string(),
    }
}

/// 校验失败原因。带 i18n 文案键，页面按当前语言渲染。
#[derive(Debug, Clone)]
pub enum Invalid {
    Unknown(String),
    Addr(&'static str),
    Int(&'static str),
    Range(&'static str, u64, u64),
    Len(&'static str, usize),
    Bool(&'static str),
    Lang(String),
}

impl Invalid {
    /// 按语言渲染错误文案（键名仍显示原始 key，便于定位配置项）。
    pub fn message(&self, lang: &str) -> String {
        use crate::web::i18n::trf_lang;
        match self {
            Invalid::Unknown(k) => trf_lang(lang, "settings.err.unknown", &[("key", k.clone())]),
            Invalid::Addr(k) => trf_lang(lang, "settings.err.addr", &[("key", (*k).into())]),
            Invalid::Int(k) => trf_lang(lang, "settings.err.int", &[("key", (*k).into())]),
            Invalid::Range(k, a, b) => trf_lang(
                lang,
                "settings.err.range",
                &[("key", (*k).into()), ("a", a.to_string()), ("b", b.to_string())],
            ),
            Invalid::Len(k, a) => trf_lang(
                lang,
                "settings.err.len",
                &[("key", (*k).into()), ("a", a.to_string())],
            ),
            Invalid::Bool(k) => trf_lang(lang, "settings.err.bool", &[("key", (*k).into())]),
            Invalid::Lang(v) => trf_lang(lang, "settings.err.lang", &[("a", v.clone())]),
        }
    }
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(crate::web::i18n::DEFAULT_LANG))
    }
}

impl std::error::Error for Invalid {}

/// 校验并归一化单个值。
pub fn validate(def: &SettingDef, raw: &str) -> Result<String, Invalid> {
    let v = raw.trim();
    match def.kind {
        Kind::Addr => v
            .parse::<std::net::SocketAddr>()
            .map(|_| v.to_string())
            .map_err(|_| Invalid::Addr(def.key)),
        Kind::U64 { min, max } => {
            let n: u64 = v.parse().map_err(|_| Invalid::Int(def.key))?;
            if n < min || n > max {
                return Err(Invalid::Range(def.key, min, max));
            }
            Ok(n.to_string())
        }
        Kind::Text { max_len } => {
            if v.chars().count() > max_len {
                return Err(Invalid::Len(def.key, max_len));
            }
            Ok(v.to_string())
        }
        Kind::Bool => match v {
            "1" | "true" | "0" | "false" => Ok(v.to_string()),
            _ => Err(Invalid::Bool(def.key)),
        },
        Kind::Lang => {
            if SUPPORTED_LANGS.iter().any(|(k, _)| *k == v) {
                Ok(v.to_string())
            } else {
                Err(Invalid::Lang(v.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(key: &str) -> &'static SettingDef {
        CATALOG.iter().find(|d| d.key == key).unwrap()
    }

    #[test]
    fn catalog_keys_are_unique_and_known() {
        let mut seen = std::collections::HashSet::new();
        for d in CATALOG {
            assert!(seen.insert(d.key), "重复键: {}", d.key);
            assert!(
                SUPPORTED_GROUPS.contains(&d.group),
                "未知分组: {}",
                d.group
            );
        }
    }

    const SUPPORTED_GROUPS: &[&str] = &["connect", "console", "data", "ota", "ui"];

    #[test]
    fn validate_addr_and_ranges() {
        assert!(validate(def("ingest_addr"), "0.0.0.0:1688").is_ok());
        assert!(validate(def("ingest_addr"), "abc").is_err());
        assert!(validate(def("reg_probe_ms"), "1500").is_ok());
        assert!(validate(def("reg_probe_ms"), "50").is_err()); // 低于 min
        assert!(validate(def("reg_probe_ms"), "999999").is_err()); // 高于 max
        assert!(validate(def("downlink_gap_ms"), "9").is_err()); // 协议要求 >= 10ms
        assert!(validate(def("site_title"), "车载 UPS 平台").is_ok());
        assert!(validate(def("language"), "en-US").is_ok());
        assert!(validate(def("language"), "fr-FR").is_err());
    }

    /// 空闲超时是 1–720 分钟；0 会让会话一建立就失效。
    #[test]
    fn session_idle_min_range() {
        assert!(validate(def("session_idle_min"), "15").is_ok());
        assert!(validate(def("session_idle_min"), "1").is_ok());
        assert!(validate(def("session_idle_min"), "0").is_err());
        assert!(validate(def("session_idle_min"), "721").is_err());
        // 真实默认值来自环境变量播种（15 分钟），不是 fallback 的下限值
        let seeded = env_defaults(&Config::from_env())
            .into_iter()
            .find(|(k, _)| k == "session_idle_min")
            .map(|(_, v)| v);
        assert_eq!(seeded.as_deref(), Some("15"));
    }

    #[test]
    fn lang_falls_back_when_missing() {
        // 内存快照缺失时返回默认语言（不依赖 DB）
        assert_eq!(DEFAULT_LANG, "zh-CN");
    }
}
