use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use mdvr_ups::command_bus::CommandBus;
use mdvr_ups::config::Config;
use mdvr_ups::ingest::Ingest;
use mdvr_ups::ingest::registry::Registry;
use mdvr_ups::metrics::Metrics;
use mdvr_ups::ota::OtaManager;
use mdvr_ups::settings::SettingsStore;
use mdvr_ups::storage::{Store, db_file_exists};
use mdvr_ups::web;
use mdvr_ups::web::auth;
use mdvr_ups::web::rbac::Rbac;

const USAGE: &str = "\
UPS Sentinel —— UPS 远程监控与 OTA 平台

用法：
  mdvr_ups                 启动服务（默认）
  mdvr_ups serve           同上
  mdvr_ups purge           预演：只打印将被清空的业务数据，不做任何修改
  mdvr_ups purge --yes     执行清空（须先停止服务，DuckDB 同一时刻只允许一个写进程）
  mdvr_ups --help          显示本帮助

清空范围：设备台账、全部历史样本、固件记录与文件、OTA 任务、审计日志、
          除管理员以外的所有账号。
保留范围：系统设置、管理员账号及其密码。

常用环境变量：UPS_WEB_ADDR / UPS_INGEST_ADDR / UPS_DB_PATH / UPS_FIRMWARE_DIR /
             UPS_ADMIN_USER / UPS_ADMIN_PASSWORD / UPS_JWT_SECRET / UPS_RETENTION_DAYS
";

/// 进程启动模式。
enum Mode {
    Serve,
    /// 清空业务数据；`yes = false` 时只预演
    Purge { yes: bool },
    Help,
}

fn parse_mode() -> Result<Mode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(Mode::Help);
    }
    if args.iter().any(|a| a == "purge" || a == "--purge-data") {
        return Ok(Mode::Purge {
            yes: args.iter().any(|a| a == "--yes" || a == "-y"),
        });
    }
    if let Some(unknown) = args
        .iter()
        .find(|a| a.as_str() != "serve")
    {
        anyhow::bail!("未知参数：{unknown}（用 --help 查看用法）");
    }
    Ok(Mode::Serve)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mdvr_ups=info,tower_http=warn".into()),
        )
        .init();

    match parse_mode()? {
        Mode::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Mode::Purge { yes } => return purge_business_data(yes).await,
        Mode::Serve => {}
    }

    // 重启时新实例会带上该环境变量，延迟启动等旧进程释放监听端口与 DuckDB 文件锁
    if let Ok(ms) = std::env::var("MDVR_UPS_RESTART_DELAY_MS")
        && let Ok(ms) = ms.trim().parse::<u64>()
    {
        tracing::info!(delay_ms = ms, "重启延迟启动");
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    let cfg = Arc::new(Config::from_env());
    warn_insecure_defaults(&cfg);
    std::fs::create_dir_all(&cfg.firmware_dir).ok();

    let store = Store::open(&cfg)?;
    seed_users(&store, &cfg).await?;

    // 系统配置：DB 持久化，空表时按环境变量播种
    let settings = Arc::new(SettingsStore::open(&store, &cfg).await?);

    // 监听地址以系统设置（DB）为准，环境变量仅作首次播种与兜底 —— 页面改端口后重启即生效
    let ingest_addr = parse_addr(&settings.str("ingest_addr"), cfg.ingest_addr);
    let web_addr = parse_addr(&settings.str("web_addr"), cfg.web_addr);
    tracing::info!(%ingest_addr, %web_addr, "监听地址已加载（来自系统设置）");

    let registry = Arc::new(Registry::new());
    let metrics = Arc::new(Metrics::default());
    // 命令应答总线：ingest 收到设备应答后投递给等待中的 web 请求
    let commands = Arc::new(CommandBus::new());
    let ota = OtaManager::new(
        registry.clone(),
        store.clone(),
        cfg.clone(),
        metrics.clone(),
        settings.clone(),
    );
    let rbac = Rbac::load(&cfg).await?;

    // 透明流接入
    let ingest_listener = TcpListener::bind(ingest_addr)
        .await
        .with_context(|| format!("绑定接入端口 {ingest_addr} 失败"))?;
    tracing::info!(addr = %ingest_addr, "MDVR 透传接入已启动");
    let ingest = Ingest {
        registry: registry.clone(),
        store: store.clone(),
        cfg: cfg.clone(),
        ota: ota.clone(),
        metrics: metrics.clone(),
        settings: settings.clone(),
        commands: commands.clone(),
    };
    tokio::spawn(ingest.serve(ingest_listener));

    // 分区清理：启动后与每 6 小时（保留天数取系统设置，运行时可变）
    {
        let store = store.clone();
        let settings = settings.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                let days = settings.i64("retention_days", cfg.retention_days);
                store.clone().cleanup_with_days(days);
            }
        });
    }

    // Web 管理端
    let state = web::build_state(
        cfg.clone(),
        store,
        registry,
        ota,
        metrics,
        rbac,
        settings.clone(),
        commands,
    );
    let app = web::router(state);
    let listener = TcpListener::bind(web_addr)
        .await
        .with_context(|| format!("绑定 Web 端口 {web_addr} 失败"))?;
    tracing::info!("Web 管理端: http://{web_addr}");

    axum::serve(
        listener,
        // into_make_service_with_connect_info：让登录处理器能拿到来源 IP（防爆破用）。
        // 拿不到时处理器用 Option<ConnectInfo> 兜底退化成按用户名限流。
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// 解析 `ip:port`：设置值为空或非法时回退环境变量地址。
fn parse_addr(value: &str, fallback: std::net::SocketAddr) -> std::net::SocketAddr {
    match value.trim().parse::<std::net::SocketAddr>() {
        Ok(a) => a,
        Err(_) => {
            if !value.trim().is_empty() {
                tracing::warn!(value = %value, "系统设置里的监听地址非法，回退环境变量地址");
            }
            fallback
        }
    }
}

async fn seed_users(store: &Store, cfg: &Config) -> Result<()> {
    if store.user(&cfg.admin_user).await?.is_none() {
        let hash = auth::hash_password(&cfg.admin_password)?;
        store.upsert_user(&cfg.admin_user, &hash, "admin", None);
        tracing::warn!(
            user = %cfg.admin_user,
            "已创建初始管理员账号，请尽快修改默认密码"
        );
    }
    if cfg.seed_demo {
        for (name, password, role) in [
            ("viewer", "viewer123", "viewer"),
            ("operator", "operator123", "operator"),
            ("firmware", "firmware123", "firmware_manager"),
        ] {
            if store.user(name).await?.is_none() {
                let hash = auth::hash_password(password)?;
                store.upsert_user(name, &hash, role, Some(&cfg.admin_user));
            }
        }
    }
    // 层级以管理员为根：其余用户（含历史遗留的顶级用户、上级已删除的用户）一律挂到管理员下
    store.normalize_parents(&cfg.admin_user);
    Ok(())
}

/// 上线自检：默认口令与默认 JWT 密钥只适合本地验收，生产部署必须覆盖。
fn warn_insecure_defaults(cfg: &Config) {
    if cfg.admin_password == "admin123" {
        tracing::warn!(
            "管理员仍在用默认口令 admin123，上线前请通过 UPS_ADMIN_PASSWORD 覆盖，并在设置页修改"
        );
    }
    if cfg.jwt_secret == "dev-secret-change-me-in-production" {
        tracing::warn!("JWT 密钥仍是默认值，上线前请通过 UPS_JWT_SECRET 设置独立随机串");
    }
}

/// 清空全部业务数据，保留系统设置与管理员账号。
///
/// 必须独占数据库：服务运行中会因 DuckDB 文件锁而失败，请先停服务。
async fn purge_business_data(yes: bool) -> Result<()> {
    let cfg = Config::from_env();
    println!("数据库    : {}", cfg.db_path.display());
    println!("固件目录  : {}", cfg.firmware_dir.display());
    println!("保留账号  : {}", cfg.admin_user);

    if !db_file_exists(&cfg.db_path) {
        println!("\n数据库文件不存在，无需清空。");
        return Ok(());
    }
    if !yes {
        println!("\n将要清空：设备台账、全部历史样本、固件记录与文件、OTA 任务、审计日志、");
        println!("          除管理员以外的所有账号。");
        println!("将要保留：系统设置、账号 {} 及其密码。", cfg.admin_user);
        println!("\n该操作不可撤销。确认执行请加 --yes：mdvr_ups purge --yes");
        return Ok(());
    }

    let store = Store::open(&cfg).with_context(|| {
        format!(
            "打开数据库失败（{}）。若服务正在运行，请先停止服务——DuckDB 同一时刻只允许一个写进程",
            cfg.db_path.display()
        )
    })?;
    let report = store.purge(&cfg.admin_user).await?;
    drop(store);
    // 等写线程关闭连接，避免进程退出时与 DuckDB 的清理流程抢时序
    std::thread::sleep(Duration::from_millis(200));

    let removed_files = purge_firmware_files(&cfg.firmware_dir)?;

    println!("\n清空完成：");
    println!("  历史分区  : {} 个", report.partitions);
    println!("  历史样本  : {} 行", report.samples);
    println!("  设备台账  : {} 台", report.devices);
    println!(
        "  固件记录  : {} 条（文件 {} 个）",
        report.firmware, removed_files
    );
    println!("  OTA 任务  : {} 条", report.jobs);
    println!("  审计日志  : {} 条", report.audit);
    println!(
        "  删除账号  : {} 个（保留 {}）",
        report.users, cfg.admin_user
    );
    Ok(())
}

/// 删除固件目录下的文件（保留目录本身）。
fn purge_firmware_files(dir: &Path) -> Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除固件文件失败：{}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("收到退出信号，正在关闭");
}
