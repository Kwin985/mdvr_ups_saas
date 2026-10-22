//! Casbin RBAC：策略与代码分离（`rbac/model.conf` + `rbac/policy.csv`）。
//!
//! 模板里的按钮显隐只是体验优化，真正的鉴权在 Axum 中间件里完成，
//! 恶意构造的 HTTP 请求会被 Casbin 拦截。

use std::sync::Arc;

use anyhow::{Context, Result};
use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter, MemoryAdapter, MgmtApi};

use crate::config::Config;

pub struct Rbac {
    enforcer: Enforcer,
}

impl Rbac {
    pub async fn load(cfg: &Config) -> Result<Arc<Self>> {
        let enforcer = if cfg.casbin_model.exists() && cfg.casbin_policy.exists() {
            let model = DefaultModel::from_file(&cfg.casbin_model)
                .await
                .with_context(|| format!("加载 {} 失败", cfg.casbin_model.display()))?;
            let adapter = FileAdapter::new(cfg.casbin_policy.to_string_lossy().to_string());
            Enforcer::new(model, adapter)
                .await
                .with_context(|| format!("加载 {} 失败", cfg.casbin_policy.display()))?
        } else {
            // 文件缺失时退化为内置策略，保证服务可启动（生产环境应挂载配置）
            tracing::warn!("未找到 Casbin 配置文件，使用内置策略");
            let model = DefaultModel::from_str(include_str!("../../rbac/model.conf")).await?;
            let adapter = MemoryAdapter::default();
            let mut e = Enforcer::new(model, adapter).await?;
            e.add_policies(vec![
                vec!["viewer".into(), "/analysis".into(), "GET".into()],
                vec!["viewer".into(), "/analysis/export".into(), "GET".into()],
                vec!["viewer".into(), "/devices/*".into(), "GET".into()],
                vec!["viewer".into(), "/ota".into(), "GET".into()],
                vec!["viewer".into(), "/logout".into(), "POST".into()],
                vec!["operator".into(), "/devices/*/command".into(), "POST".into()],
                vec!["firmware_manager".into(), "/ota/upload".into(), "POST".into()],
                vec!["firmware_manager".into(), "/ota/start".into(), "POST".into()],
                vec!["admin".into(), "/*".into(), "(GET|POST|PUT|DELETE)".into()],
                vec!["admin".into(), "firmware_manager".into(), "admin".into()],
            ])
            .await?;
            e
        };

        let s = Self { enforcer };
        // 启动自检：admin 必须能访问任意路径
        if !s.enforce_sync("admin", "/devices/X", "GET") {
            tracing::error!("Casbin 策略自检失败：admin 无法访问 /devices/X");
        }
        Ok(Arc::new(s))
    }

    fn enforce_sync(&self, role: &str, path: &str, method: &str) -> bool {
        self.enforcer
            .enforce(vec![
                role.to_string(),
                path.to_string(),
                method.to_string(),
            ])
            .unwrap_or(false)
    }

    pub fn enforce(&self, role: &str, path: &str, method: &str) -> bool {
        self.enforce_sync(role, path, method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn rbac() -> Arc<Rbac> {
        // 测试工作目录为 crate 根，rbac/*.conf|csv 可直接命中
        Rbac::load(&Config::from_env()).await.expect("加载 RBAC 失败")
    }

    #[tokio::test]
    async fn settings_page_is_admin_only() {
        let r = rbac().await;
        for method in ["GET", "POST"] {
            assert!(
                r.enforce("admin", "/admin/settings", method),
                "admin 应可 {method} /admin/settings"
            );
            for role in ["viewer", "operator", "firmware_manager"] {
                assert!(
                    !r.enforce(role, "/admin/settings", method),
                    "{role} 不应可 {method} /admin/settings"
                );
            }
        }
    }

    #[tokio::test]
    async fn viewer_can_read_analysis() {
        let r = rbac().await;
        for role in ["viewer", "operator", "firmware_manager", "admin"] {
            assert!(
                r.enforce(role, "/analysis", "GET"),
                "{role} 应可 GET /analysis"
            );
            assert!(
                r.enforce(role, "/analysis/export", "GET"),
                "{role} 应可 GET /analysis/export"
            );
        }
        assert!(!r.enforce("viewer", "/dashboard", "GET"), "旧总览页已下线");
    }

    #[tokio::test]
    async fn viewer_cannot_write_devices() {
        let r = rbac().await;
        assert!(r.enforce("viewer", "/devices/X", "GET"));
        assert!(!r.enforce("viewer", "/devices/X/command", "POST"));
        assert!(r.enforce("operator", "/devices/X/command", "POST"));
    }

    /// 用户管理与设备归属只归 admin：六个动作 × 三种非管理员角色全都要拦住。
    /// 这条守的是「用户管理本身」的入口，与页面按钮显隐无关。
    #[tokio::test]
    async fn user_management_is_admin_only() {
        let r = rbac().await;
        let admin_paths = [
            ("/admin/users", "GET"),
            ("/admin/users", "POST"),
            ("/admin/users/password", "POST"),
            ("/admin/users/delete", "POST"),
            ("/admin/users/parent", "POST"),
            ("/devices/UPS-1/assign", "POST"),
        ];
        for (path, method) in admin_paths {
            assert!(r.enforce("admin", path, method), "admin 应可 {method} {path}");
            for role in ["viewer", "operator", "firmware_manager"] {
                assert!(!r.enforce(role, path, method), "{role} 不应可 {method} {path}");
            }
        }
    }

    /// 只读路径不能被上面收紧影响。
    #[tokio::test]
    async fn read_paths_still_open_to_every_role() {
        let r = rbac().await;
        for role in ["viewer", "operator", "firmware_manager", "admin"] {
            for path in ["/", "/monitor", "/api/tree", "/analysis", "/ota"] {
                assert!(r.enforce(role, path, "GET"), "{role} 应可 GET {path}");
            }
        }
    }
}
