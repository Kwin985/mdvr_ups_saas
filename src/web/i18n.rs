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

    /// 平台版本标签（`v0.1.0+3f9a2c1`）。
    ///
    /// 挂在 `Tr` 上是因为它已经是每个模板都能拿到的视图上下文
    /// （与 `site_title()` 同理），否则得给十几个模板结构体各加一个字段。
    pub fn app_version(&self) -> String {
        crate::version::label()
    }

    /// 构建时间，未知时为空串（模板用 `is_empty()` 判断是否渲染）。
    pub fn build_time(&self) -> &'static str {
        crate::version::built_at()
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
    "js.battery",
    "js.unassigned",
    "js.unassigned_group",
    "js.menu_expand",
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

    /// `/ups` 是纯文案页，几十条键全靠词典。英文漏一条就会在英文部署下突然冒出
    /// 一句中文（回退逻辑），非常扎眼。这里强制 ups.* 中英齐全。
    #[test]
    fn ups_showcase_keys_have_english() {
        let missing: Vec<&str> = ZH_ENTRIES
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| k.starts_with("ups."))
            .filter(|k| !EN_ENTRIES.iter().any(|(ek, _)| ek == k))
            .collect();
        assert!(missing.is_empty(), "英文词典缺少这些 ups.* 键：{missing:?}");
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

    fn has_cjk(s: &str) -> bool {
        s.chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    /// 英文界面冒出中文，绝大多数是「只加了 ZH、忘了 EN」——回退逻辑会把中文
    /// 原样显示出来。这里把整个词典（不只是 ups.*）都锁住。
    #[test]
    fn every_zh_key_has_an_english_entry() {
        // 品牌名两种语言都是 "UPS Sentinel"，无需翻译
        const EXEMPT: &[&str] = &["brand.name"];
        let missing: Vec<&str> = ZH_ENTRIES
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| !EXEMPT.contains(k))
            .filter(|k| !EN_ENTRIES.iter().any(|(ek, _)| ek == k))
            .collect();
        assert!(missing.is_empty(), "英文词典缺少这些键：{missing:?}");
    }

    /// 英文界面不该出现任何汉字。历史上两次踩坑：只加 ZH 忘了 EN（回退成中文），
    /// 以及 OTA 停用原因/默认分组名直接硬编码中文。这里把词典这一层彻底锁住。
    #[test]
    fn english_entries_contain_no_chinese() {
        let bad: Vec<(&str, &str)> = EN_ENTRIES
            .iter()
            .copied()
            .filter(|(_, v)| has_cjk(v))
            .collect();
        assert!(bad.is_empty(), "英文词条里混入了中文：{bad:?}");
    }

    /// JS 侧 window.I18N 只注入 JS_KEYS，漏一条就会用 JS 里的英文兜底值，
    /// 但兜底値不该是中文。这里同时检查「有 EN 条目」与「条目确实是英文」。
    #[test]
    fn js_keys_have_real_english_translations() {
        let bad: Vec<&str> = JS_KEYS
            .iter()
            .copied()
            .filter(|k| {
                !EN_ENTRIES
                    .iter()
                    .any(|(ek, ev)| ek == k && !has_cjk(ev))
            })
            .collect();
        assert!(bad.is_empty(), "这些 JS 键缺英文翻译：{bad:?}");
    }

    #[test]
    fn dictionary_keys_are_unique() {
        for (name, entries) in [("ZH", ZH_ENTRIES), ("EN", EN_ENTRIES)] {
            let mut seen = std::collections::HashSet::new();
            for (k, _) in entries {
                assert!(seen.insert(*k), "{name} 词典有重复键：{k}");
            }
        }
    }
}
