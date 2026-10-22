//! 界面语言：zh-CN（默认，源语言）/ en-US。
//!
//! * 词典查找：`Tr::tr(key)`；`en` 命中英文，未命中回退中文，再未命中回退键名本身
//!   ——因此新增界面文字只需补 `ZH`，不会把英文用户带崩；
//! * 语言来自系统设置（`settings.language`），运行时切换即时生效；
//! * JS 侧需要的文案由 `JS_KEYS` 单独列出，渲染时注入 `window.I18N`。

use std::collections::HashMap;
use std::sync::OnceLock;

pub const DEFAULT_LANG: &str = "zh-CN";

impl From<String> for Tr {
    fn from(lang: String) -> Self {
        Self {
            lang,
            site_title: None,
        }
    }
}

impl From<&str> for Tr {
    fn from(lang: &str) -> Self {
        Self {
            lang: lang.to_string(),
            site_title: None,
        }
    }
}
pub const SUPPORTED_LANGS: &[(&str, &str)] = &[("zh-CN", "简体中文"), ("en-US", "English")];

/// 当前语言的翻译器。按值传递，模板里 `{{ t.tr("key") }}`。
#[derive(Debug, Clone)]
pub struct Tr {
    pub lang: String,
    /// 站点标题（来自 `site_title` 设置），未配置时回退品牌名。
    site_title: Option<String>,
}

impl Tr {
    pub fn new(lang: impl Into<String>) -> Self {
        Self {
            lang: lang.into(),
            site_title: None,
        }
    }

    /// 绑定站点标题（空串视为未配置）。
    pub fn with_site(mut self, title: impl Into<String>) -> Self {
        let title = title.into();
        if !title.trim().is_empty() {
            self.site_title = Some(title.trim().to_string());
        }
        self
    }

    /// 浏览器标题栏 / 侧栏品牌位显示的名称。
    pub fn site_title(&self) -> &str {
        match self.site_title.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => self.tr("brand.name"),
        }
    }

    pub fn tr(&self, key: &str) -> &'static str {
        lookup(&self.lang, key)
    }

    /// 带占位符的文案：模板无法插值，动态串在 Rust 侧拼好后注入。
    pub fn trf(&self, key: &str, pairs: &[(&str, String)]) -> String {
        let mut s = self.tr(key).to_string();
        for (name, val) in pairs {
            s = s.replace(&format!("{{{{{name}}}}}"), val);
        }
        s
    }

    pub fn is_en(&self) -> bool {
        self.lang == "en-US"
    }

    /// `html lang` 属性
    pub fn html_lang(&self) -> &'static str {
        if self.is_en() { "en" } else { "zh-CN" }
    }
}

fn zh() -> &'static HashMap<&'static str, &'static str> {
    static ZH: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    ZH.get_or_init(|| to_map(ZH_ENTRIES))
}

fn en() -> &'static HashMap<&'static str, &'static str> {
    static EN: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    EN.get_or_init(|| to_map(EN_ENTRIES))
}

fn to_map(entries: &'static [(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
    entries.iter().copied().collect()
}

/// 游离函数版 `trf`（供 web 之外、如配置校验拼装错误文案）。
pub fn trf_lang(lang: &str, key: &str, pairs: &[(&str, String)]) -> String {
    let mut s = lookup(lang, key).to_string();
    for (name, val) in pairs {
        s = s.replace(&format!("{{{{{name}}}}}"), val);
    }
    s
}

pub fn lookup(lang: &str, key: &str) -> &'static str {
    if lang == "en-US"
        && let Some(v) = en().get(key)
    {
        return v;
    }
    if let Some(v) = zh().get(key) {
        return v;
    }
    // 未知键：泄漏为 'static 兜底（键名固定、量级极小）
    Box::leak(key.to_string().into_boxed_str())
}

/// JS 侧需要的文案键（注入 window.I18N）。
pub const JS_KEYS: &[&str] = &[
    "js.search_ph",
    "js.syncing",
    "js.sync_now",
    "js.loading_tree",
    "js.pick_hint",
    "js.filter_all",
    "js.filter_online",
    "js.filter_offline",
    "js.confirm_delete",
    "js.group_default",
    "js.unassigned",
    "js.node_online",
    "js.node_offline",
    "js.no_result",
    "js.lang_changed",
    "js.rel_secs",
    "js.rel_mins",
    "js.rel_hours",
    "js.rel_days",
    "dashboard.badge_busy",
    "menu.collapse_short",
    "js.idle_title",
    "js.idle_body",
    "js.idle_note",
    "js.idle_button",
];

/// 渲染 `window.I18N = {...}`（含当前语言）。
pub fn js_dict(lang: &str) -> String {
    let mut out = String::from("window.I18N = {");
    for k in JS_KEYS {
        out.push_str(&format!("{}:{:?},", k, lookup(lang, k)));
    }
    out.push_str("lang:");
    out.push_str(&format!("{lang:?}"));
    out.push('}');
    out
}

include!("i18n_entries.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_lookup_works() {
        assert_eq!(lookup("zh-CN", "nav.monitor"), "实时监控");
        assert_eq!(lookup("zh-CN", "common.save"), "保存");
    }

    #[test]
    fn en_lookup_works() {
        assert_eq!(lookup("en-US", "nav.monitor"), "Live Monitor");
    }

    #[test]
    fn en_missing_falls_back_to_zh() {
        // en 未收录的键（如品牌名）回退中文，不出现空串/键名
        assert_eq!(
            lookup("en-US", "brand.name"),
            zh().get("brand.name").copied().unwrap()
        );
    }

    #[test]
    fn unknown_key_returns_key() {
        assert_eq!(lookup("zh-CN", "no.such.key"), "no.such.key");
    }

    #[test]
    fn every_zh_key_is_named_with_a_dot() {
        for (k, _) in ZH_ENTRIES {
            assert!(k.contains('.'), "键应带命名空间: {k}");
        }
    }

    #[test]
    fn js_dict_covers_all_js_keys() {
        let d = js_dict("en-US");
        for k in JS_KEYS {
            assert!(d.contains(k), "缺少 JS 键 {k}");
        }
    }
}
