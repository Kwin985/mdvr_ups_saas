//! 平台自身的版本号（与 UPS 固件版本无关，别混用）。
//!
//! * `VERSION` —— 语义化版本，唯一来源是 `Cargo.toml` 的 `version`；
//! * `COMMIT`  —— 构建时的短提交号（8 位），无 git 时为 `unknown`；
//! * 编译时工作区有未提交改动 → 展示时带 `.dirty` 后缀，避免“看着像正式版其实带改动”。
//!
//! 展示用三个入口：`label()`（`v0.1.0+3f9a2c1`）、`built_at()`（构建时间）、
//! `banner()`（启动日志 / `--version` 一行摘要）。

use std::sync::OnceLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const COMMIT: &str = match option_env!("UPS_GIT_HASH") {
    Some(s) => s,
    None => "unknown",
};

/// 编译时工作区是否有未提交改动。
///
/// 不能用 `const`：`str` 比较在常量上下文里还不稳定（E0658），
/// 这点开销（一次字符串比较）在渲染路径上可忽略。
fn dirty() -> bool {
    option_env!("UPS_GIT_DIRTY").is_some_and(|s| s == "1")
}

fn build_epoch() -> i64 {
    option_env!("UPS_BUILD_EPOCH")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
}

/// `v0.1.0+3f9a2c1`；有未提交改动时 `v0.1.0+3f9a2c1.dirty`。
pub fn label() -> String {
    let mut s = format!("v{VERSION}+{COMMIT}");
    if dirty() {
        s.push_str(".dirty");
    }
    s
}

/// 构建时间，按**运行环境本地时区**渲染（与 `journalctl` 的时间戳对得上）。
/// 时间戳缺失（手工构建）时返回空串。
pub fn built_at() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| {
        let epoch = build_epoch();
        if epoch <= 0 {
            return String::new();
        }
        chrono::DateTime::from_timestamp(epoch, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default()
    })
}

/// 启动日志与 `--version` 的一行摘要：`UPS Sentinel v0.1.0+3f9a2c1 (built 2026-10-28 11:20)`。
pub fn banner() -> String {
    let at = built_at();
    if at.is_empty() {
        format!("UPS Sentinel {}", label())
    } else {
        format!("UPS Sentinel {} (built {at})", label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_always_has_semver_and_commit() {
        let l = label();
        assert!(l.starts_with('v'), "{l}");
        assert!(l.contains(VERSION), "{l}");
        assert!(l.contains(COMMIT), "{l}");
    }

    #[test]
    fn banner_contains_label() {
        assert!(banner().contains(&label()));
    }
}
