use std::net::SocketAddr;
use std::path::PathBuf;

/// 全部运行参数来自环境变量，带可用默认值，便于容器化部署。
#[derive(Debug, Clone)]
pub struct Config {
    /// Web 管理端监听地址
    pub web_addr: SocketAddr,
    /// MDVR 4G 透传接入地址
    pub ingest_addr: SocketAddr,
    pub db_path: PathBuf,
    pub firmware_dir: PathBuf,
    pub jwt_secret: String,
    /// 控制台空闲超时（分钟）：超过该时长没有操作，会话失效、页面停止刷新。
    /// 有操作时每次请求滑动续期，所以"一直在用"不会被踢。
    pub session_idle_min: i64,
    /// 状态历史保留天数，超过后整分区删除
    pub retention_days: i64,
    /// 下行帧最小间隔，协议要求 >= 10ms
    pub downlink_gap_ms: u64,
    /// 注册握手探测窗口：真实 MDVR 透传设备不会发自定义注册行，
    /// 窗口内没等到注册就按回退身份放行，绝不因此断开。
    ///
    /// 默认 5s 而不是 1.5s：平台重启后上万台设备同时重连时，首帧到达
    /// 会被排队，窗口太短会让大批设备退化成 `UPS-<IP>` 占位身份，
    /// 随后又触发大量占位→真实 SN 的历史迁移。
    pub reg_probe_ms: u64,
    /// OTA 单包应答超时
    pub ota_packet_timeout_ms: u64,
    /// OTA 单包最大重试次数（0x31）
    pub ota_max_retries: u32,
    /// 0x32 触发的整轮重启上限
    pub ota_max_restarts: u32,
    /// OTA 开始阶段的应答窗口与重试间隔
    pub ota_begin_window_ms: u64,
    pub ota_begin_interval_ms: u64,
    pub admin_user: String,
    pub admin_password: String,
    pub casbin_model: PathBuf,
    pub casbin_policy: PathBuf,
    pub static_dir: PathBuf,
    /// 是否启动时写入演示设备（仅用于本地验收）
    pub seed_demo: bool,
    /// DuckDB 工作线程数上限；`None` 表示用 DuckDB 默认（= CPU 核数）。
    /// 小规格云主机（如 e2-micro 0.25 vCPU）建议设 1，避免多线程争抢。
    pub db_threads: Option<u32>,
    /// DuckDB 内存上限（MB）；`None` 表示用 DuckDB 默认（物理内存的 80%）。
    /// 1GB 内存的机器务必设 256 左右，否则大范围聚合查询可能把进程撑爆。
    pub db_memory_limit_mb: Option<u64>,
    /// DuckDB 异步 I/O 线程数上限（2.0 新增的 `async_threads`）；`None` 表示用默认（64）。
    /// 2.0 把 I/O 从查询线程里拆出来单独开池，小规格云主机同样需要跟着收紧。
    pub db_async_threads: Option<u32>,
    /// 单条 SQL 的最长执行时间（毫秒，2.0 新增的 `max_execution_time`）；`None` 表示不限。
    /// 所有查询都串行排在唯一的写线程上，一条失控的聚合会挡住后面所有写入。
    pub db_query_timeout_ms: Option<u64>,
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 可选数值项：未设置、解析失败或为 0 都返回 `None`（交回 DuckDB 默认值）。
fn env_opt<T: std::str::FromStr + PartialEq + Default>(key: &str) -> Option<T> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<T>()
        .ok()
        .filter(|v| *v != T::default())
}

/// 布尔开关兼容 `1/0`、`true/false`、`yes/no`、`on/off`，避免只认 "true" 带来的坑。
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            // 默认可直接对局域网提供服务：设备走 1688，控制台走 18080
            // （8080 在本机被 Apache 占用，故不用常见端口）
            web_addr: env_str("UPS_WEB_ADDR", "0.0.0.0:18080")
                .parse()
                .expect("UPS_WEB_ADDR 格式应为 ip:port"),
            ingest_addr: env_str("UPS_INGEST_ADDR", "0.0.0.0:1688")
                .parse()
                .expect("UPS_INGEST_ADDR 格式应为 ip:port"),
            db_path: PathBuf::from(env_str("UPS_DB_PATH", "data/ups.duckdb")),
            firmware_dir: PathBuf::from(env_str("UPS_FIRMWARE_DIR", "data/firmware")),
            jwt_secret: env_str("UPS_JWT_SECRET", "dev-secret-change-me-in-production"),
            session_idle_min: env_parse("UPS_SESSION_IDLE_MIN", 15).clamp(1, 720),
            retention_days: env_parse("UPS_RETENTION_DAYS", 180),
            downlink_gap_ms: env_parse("UPS_DOWNLINK_GAP_MS", 10),
            reg_probe_ms: env_parse("UPS_REG_PROBE_MS", 5000),
            ota_packet_timeout_ms: env_parse("UPS_OTA_PACKET_TIMEOUT_MS", 2000),
            ota_max_retries: env_parse("UPS_OTA_MAX_RETRIES", 3),
            ota_max_restarts: env_parse("UPS_OTA_MAX_RESTARTS", 2),
            ota_begin_window_ms: env_parse("UPS_OTA_BEGIN_WINDOW_MS", 3000),
            ota_begin_interval_ms: env_parse("UPS_OTA_BEGIN_INTERVAL_MS", 100),
            admin_user: env_str("UPS_ADMIN_USER", "admin"),
            admin_password: env_str("UPS_ADMIN_PASSWORD", "admin123"),
            casbin_model: PathBuf::from(env_str("UPS_CASBIN_MODEL", "rbac/model.conf")),
            casbin_policy: PathBuf::from(env_str("UPS_CASBIN_POLICY", "rbac/policy.csv")),
            static_dir: PathBuf::from(env_str("UPS_STATIC_DIR", "static")),
            seed_demo: env_bool("UPS_SEED_DEMO", false),
            db_threads: env_opt("UPS_DB_THREADS"),
            db_memory_limit_mb: env_opt("UPS_DB_MEMORY_LIMIT_MB"),
            db_async_threads: env_opt("UPS_DB_ASYNC_THREADS"),
            db_query_timeout_ms: env_opt("UPS_DB_QUERY_TIMEOUT_MS"),
        }
    }
}
