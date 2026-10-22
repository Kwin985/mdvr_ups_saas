//! DuckDB 存储层。
//!
//! * 单线程独占一个 `duckdb::Connection`，所有写入经 mpsc 汇聚后批量 Append；
//! * 状态历史按 **月分区表** 存储（`ups_status_YYYYMM`），对外暴露同名视图
//!   `ups_status`，过期清理直接 `DROP TABLE`，避免大表 DELETE；
//! * 读查询同样走该线程（请求/响应通道），保证连接不跨线程共享。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, TimeZone, Utc};
use duckdb::types::Value;
use duckdb::{Connection, params};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::protocol::StatusA;

/// 设备接入（幂等）：新设备插入，老设备刷新透传参数。
/// 预编译后可在同一个事务里批量执行，避免逐条提交。
const UPSERT_DEVICE_SQL: &str = "INSERT INTO devices (sn, mdvr_id, address, baudrate, report_interval,
                                  firmware_version, last_seen_ms, online, config_json,
                                  created_at_ms, updated_at_ms)
     VALUES (?,?,?,?,?,?,?,?,?,?,?)
     ON CONFLICT (sn) DO UPDATE SET
        mdvr_id = excluded.mdvr_id,
        address = excluded.address,
        baudrate = excluded.baudrate,
        report_interval = excluded.report_interval,
        updated_at_ms = excluded.updated_at_ms";

const SET_ONLINE_SQL: &str =
    "UPDATE devices SET online = ?, last_seen_ms = ?, updated_at_ms = ? WHERE sn = ?";

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Sample {
    pub sn: String,
    pub ts_ms: i64,
    pub input_voltage: f32,
    pub output_voltage: f32,
    pub temperature: f32,
    pub battery: u8,
    pub health: u8,
    pub load_percent: u8,
    pub firmware: String,
    pub raw: Vec<u8>,
}

impl Sample {
    /// 由 `a` 实时状态帧构造样本。
    ///
    /// 列名沿用历史表结构：`input_voltage` 存 PWR（电源电压），
    /// `output_voltage` 存 VOL（UPS 电压）；`a` 帧没有负载字段，
    /// `load_percent` 恒为 0，版本号来自 `V` 帧，此处留空。
    pub fn from_status_a(sn: &str, ts_ms: i64, st: &StatusA, raw: Vec<u8>) -> Self {
        Self {
            sn: sn.to_string(),
            ts_ms,
            input_voltage: st.power_voltage,
            output_voltage: st.ups_voltage,
            temperature: st.temperature,
            battery: st.battery_level,
            health: st.smart_status,
            load_percent: 0,
            firmware: String::new(),
            raw,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceMeta {
    pub sn: String,
    pub mdvr_id: String,
    pub address: i64,
    pub baudrate: i64,
    pub report_interval: i64,
    pub firmware_version: String,
    pub config_json: String,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceRow {
    pub sn: String,
    pub mdvr_id: String,
    pub address: i64,
    pub baudrate: i64,
    pub report_interval: i64,
    pub firmware_version: String,
    pub last_seen_ms: i64,
    pub online: bool,
    pub config_json: String,
    /// 归属用户（空表示未分配，仅管理员可见）
    pub owner: String,
    /// 设备树中的分组名（同一归属用户下按此分组）
    pub group_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct SampleRow {
    pub ts_ms: i64,
    pub input_voltage: f64,
    pub output_voltage: f64,
    pub temperature: f64,
    pub battery: i64,
    pub health: i64,
    pub load_percent: i64,
    pub firmware: String,
}

#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub ts_ms: i64,
    pub avg_input: f64,
    pub avg_output: f64,
    pub avg_temp: f64,
    pub avg_battery: f64,
    pub min_battery: i64,
    pub max_battery: i64,
    pub samples: i64,
}

/// 每台设备的最新一条状态（设备树用，一次查询取全量）
#[derive(Debug, Clone, Default)]
pub struct LatestRow {
    pub sn: String,
    pub battery: i64,
    pub health: i64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct UserRow {
    pub username: String,
    pub role: String,
    pub created_at_ms: i64,
    /// 上级用户名；空表示顶级用户
    pub parent: String,
}

#[derive(Debug, Clone)]
pub struct UserCredential {
    pub username: String,
    pub password_hash: String,
    pub role: String,
}

#[derive(Debug, Clone, Default)]
pub struct FirmwareRow {
    pub id: String,
    pub filename: String,
    pub version: String,
    pub size: i64,
    pub sha256: String,
    pub uploaded_at_ms: i64,
    pub uploaded_by: String,
}

#[derive(Debug, Clone, Default)]
pub struct JobRow {
    pub id: String,
    pub sn: String,
    pub firmware_id: String,
    pub state: String,
    pub total_packets: i64,
    pub sent_packets: i64,
    pub retries: i64,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub message: String,
    pub actor: String,
}

#[derive(Debug, Clone, Default)]
pub struct DashboardStats {
    pub devices: i64,
    pub samples_24h: i64,
    pub ota_ok: i64,
    pub ota_done: i64,
    pub samples_total: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub id: String,
    pub sn: String,
    pub firmware_id: String,
    pub state: String,
    pub total_packets: i64,
    pub actor: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub sn: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub limit: i64,
}

// ---------------------------------------------------------------------------
// 通道协议
// ---------------------------------------------------------------------------

enum Query {
    Devices,
    History(HistoryQuery),
    Buckets {
        sn: String,
        from_ms: i64,
        to_ms: i64,
        bucket_ms: i64,
    },
    Latest {
        sn: String,
    },
    LatestAll,
    Dashboard,
    Users,
    User(String),
    Firmware,
    FirmwareById(String),
    Jobs(i64),
    Job(String),
    SettingsAll,
}

enum StoreResult {
    Devices(Vec<DeviceRow>),
    Samples(Vec<SampleRow>),
    Buckets(Vec<Bucket>),
    OptionalSample(Option<SampleRow>),
    LatestAll(Vec<LatestRow>),
    Stats(DashboardStats),
    Users(Vec<UserRow>),
    OptionalUser(Option<UserCredential>),
    Firmware(Vec<FirmwareRow>),
    OptionalFirmware(Option<FirmwareRow>),
    Jobs(Vec<JobRow>),
    OptionalJob(Option<JobRow>),
    Settings(HashMap<String, String>),
    Unit,
}

enum Cmd {
    Samples(Vec<Sample>),
    UpsertDevice(DeviceMeta),
    SetOnline {
        sn: String,
        online: bool,
        ts_ms: i64,
    },
    UpdateFirmware {
        sn: String,
        version: String,
    },
    CreateJob(NewJob),
    UpdateJob {
        id: String,
        state: String,
        sent_packets: i64,
        retries: i64,
        message: String,
        finished: bool,
    },
    InsertFirmware(FirmwareRow),
    UpsertUser {
        username: String,
        password_hash: String,
        role: String,
        parent: String,
    },
    DeleteUser(String),
    AssignDevice {
        sn: String,
        owner: String,
        group_name: String,
    },
    SetUserParent {
        username: String,
        parent: String,
    },
    /// 把没有上级（或上级已不存在）的用户统一挂到根管理员下
    NormalizeParents {
        root: String,
    },
    Audit {
        id: String,
        actor: String,
        action: String,
        target: String,
        detail: String,
    },
    /// 占位 SN → 真实 SN：迁移 devices 行与所有月分区样本
    MigrateDeviceSn {
        old_sn: String,
        new_sn: String,
    },
    Cleanup {
        retention_days: i64,
    },
    /// 清空全部业务数据（设备/历史/固件/任务/审计/非管理员账号），保留系统设置
    Purge {
        keep_admin: String,
        reply: oneshot::Sender<Result<PurgeReport>>,
    },
    /// 写入/更新一条系统配置
    SettingSet {
        key: String,
        value: String,
    },
    Query(Query, oneshot::Sender<StoreResult>),
}

// ---------------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    tx: mpsc::UnboundedSender<Cmd>,
    pub stats: Arc<StoreStats>,
}

#[derive(Debug, Default)]
pub struct StoreStats {
    pub samples_written: std::sync::atomic::AtomicU64,
    pub batches: std::sync::atomic::AtomicU64,
    pub partitions_dropped: std::sync::atomic::AtomicU64,
}

/// 「清空业务数据」的结果，用于运维输出核对。
///
/// 清空范围只包含**业务数据**：设备、历史样本、固件记录、OTA 任务、审计日志、
/// 以及除管理员以外的账号。系统设置与管理员账号本身保留，避免清空后连登录都进不去。
#[derive(Debug, Default, Clone, Copy)]
pub struct PurgeReport {
    /// 被整表删除的历史分区数
    pub partitions: u64,
    pub samples: u64,
    pub devices: u64,
    pub firmware: u64,
    pub jobs: u64,
    pub audit: u64,
    pub users: u64,
}

impl Store {
    pub fn open(cfg: &Config) -> Result<Self> {
        if let Some(parent) = cfg.db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let path = cfg.db_path.clone();
        let retention = cfg.retention_days;
        let limits = DbLimits {
            threads: cfg.db_threads,
            memory_limit_mb: cfg.db_memory_limit_mb,
        };
        let stats = Arc::new(StoreStats::default());
        let writer_stats = stats.clone();
        std::thread::Builder::new()
            .name("duckdb-writer".into())
            .spawn(move || run_writer(path, retention, limits, rx, ready_tx, writer_stats))
            .context("启动 DuckDB 写线程失败")?;
        ready_rx
            .recv()
            .context("DuckDB 初始化无响应")??;
        Ok(Self { tx, stats })
    }

    pub fn insert_samples(&self, samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        let _ = self.tx.send(Cmd::Samples(samples));
    }

    pub fn upsert_device(&self, meta: DeviceMeta) {
        let _ = self.tx.send(Cmd::UpsertDevice(meta));
    }

    pub fn set_online(&self, sn: &str, online: bool) {
        let _ = self.tx.send(Cmd::SetOnline {
            sn: sn.to_string(),
            online,
            ts_ms: Utc::now().timestamp_millis(),
        });
    }

    pub fn update_firmware(&self, sn: &str, version: &str) {
        let _ = self.tx.send(Cmd::UpdateFirmware {
            sn: sn.to_string(),
            version: version.to_string(),
        });
    }

    /// 设备首帧 `a` 上报真实 SN 后，把占位设备（含历史样本）整体迁移到正式 SN。
    pub fn migrate_device_sn(&self, old_sn: &str, new_sn: &str) {
        let _ = self.tx.send(Cmd::MigrateDeviceSn {
            old_sn: old_sn.to_string(),
            new_sn: new_sn.to_string(),
        });
    }

    pub fn create_job(&self, job: NewJob) {
        let _ = self.tx.send(Cmd::CreateJob(job));
    }

    pub fn update_job(
        &self,
        id: &str,
        state: &str,
        sent: i64,
        retries: i64,
        message: &str,
        finished: bool,
    ) {
        let _ = self.tx.send(Cmd::UpdateJob {
            id: id.to_string(),
            state: state.to_string(),
            sent_packets: sent,
            retries,
            message: message.to_string(),
            finished,
        });
    }

    pub fn insert_firmware(&self, row: FirmwareRow) {
        let _ = self.tx.send(Cmd::InsertFirmware(row));
    }

    pub fn upsert_user(&self, username: &str, hash: &str, role: &str, parent: Option<&str>) {
        let _ = self.tx.send(Cmd::UpsertUser {
            username: username.to_string(),
            password_hash: hash.to_string(),
            role: role.to_string(),
            parent: parent.unwrap_or_default().to_string(),
        });
    }

    /// 调整设备归属与分组；owner 为空表示退回「未分配」。
    pub fn assign_device(&self, sn: &str, owner: &str, group_name: &str) {
        let _ = self.tx.send(Cmd::AssignDevice {
            sn: sn.to_string(),
            owner: owner.to_string(),
            group_name: group_name.to_string(),
        });
    }

    /// 调整用户的上级（空字符串表示提升为顶级用户）。
    pub fn set_user_parent(&self, username: &str, parent: &str) {
        let _ = self.tx.send(Cmd::SetUserParent {
            username: username.to_string(),
            parent: parent.to_string(),
        });
    }

    /// 启动时归一化用户层级：除根管理员外，所有无上级 / 上级断链的用户都挂到根管理员下。
    pub fn normalize_parents(&self, root: &str) {
        let _ = self.tx.send(Cmd::NormalizeParents {
            root: root.to_string(),
        });
    }

    pub fn delete_user(&self, username: &str) {
        let _ = self.tx.send(Cmd::DeleteUser(username.to_string()));
    }

    pub fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) {
        let _ = self.tx.send(Cmd::Audit {
            id: uuid::Uuid::new_v4().to_string(),
            actor: actor.to_string(),
            action: action.to_string(),
            target: target.to_string(),
            detail: detail.to_string(),
        });
    }

    /// 过期分区清理：保留天数由调用方（系统设置）给出，运行时可变。
    pub fn cleanup_with_days(&self, retention_days: i64) {
        let _ = self.tx.send(Cmd::Cleanup { retention_days });
    }

    /// 清空全部业务数据，保留系统设置与 `keep_admin` 账号。
    ///
    /// 只能在**独占**数据库时调用（服务已停止）；服务运行时文件锁会直接报错。
    pub async fn purge(&self, keep_admin: &str) -> Result<PurgeReport> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Purge {
                keep_admin: keep_admin.to_string(),
                reply: tx,
            })
            .map_err(|_| anyhow!("存储线程已退出"))?;
        rx.await.map_err(|_| anyhow!("清空任务被取消"))?
    }

    /// 持久化一条系统配置。
    pub fn setting_set(&self, key: &str, value: &str) {
        let _ = self.tx.send(Cmd::SettingSet {
            key: key.to_string(),
            value: value.to_string(),
        });
    }

    async fn ask(&self, q: Query) -> Result<StoreResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Query(q, tx))
            .map_err(|_| anyhow!("存储线程已退出"))?;
        rx.await.map_err(|_| anyhow!("存储查询被取消"))
    }

    pub async fn devices(&self) -> Result<Vec<DeviceRow>> {
        match self.ask(Query::Devices).await? {
            StoreResult::Devices(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn latest_all(&self) -> Result<Vec<LatestRow>> {
        match self.ask(Query::LatestAll).await? {
            StoreResult::LatestAll(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn history(&self, q: HistoryQuery) -> Result<Vec<SampleRow>> {
        match self.ask(Query::History(q)).await? {
            StoreResult::Samples(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn buckets(
        &self,
        sn: &str,
        from_ms: i64,
        to_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<Bucket>> {
        match self
            .ask(Query::Buckets {
                sn: sn.to_string(),
                from_ms,
                to_ms,
                bucket_ms,
            })
            .await?
        {
            StoreResult::Buckets(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn latest(&self, sn: &str) -> Result<Option<SampleRow>> {
        match self
            .ask(Query::Latest {
                sn: sn.to_string(),
            })
            .await?
        {
            StoreResult::OptionalSample(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn settings_all(&self) -> Result<HashMap<String, String>> {
        match self.ask(Query::SettingsAll).await? {
            StoreResult::Settings(m) => Ok(m),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn dashboard(&self) -> Result<DashboardStats> {
        match self.ask(Query::Dashboard).await? {
            StoreResult::Stats(s) => Ok(s),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn users(&self) -> Result<Vec<UserRow>> {
        match self.ask(Query::Users).await? {
            StoreResult::Users(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn user(&self, username: &str) -> Result<Option<UserCredential>> {
        match self.ask(Query::User(username.to_string())).await? {
            StoreResult::OptionalUser(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn firmware(&self) -> Result<Vec<FirmwareRow>> {
        match self.ask(Query::Firmware).await? {
            StoreResult::Firmware(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn firmware_by_id(&self, id: &str) -> Result<Option<FirmwareRow>> {
        match self.ask(Query::FirmwareById(id.to_string())).await? {
            StoreResult::OptionalFirmware(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn jobs(&self, limit: i64) -> Result<Vec<JobRow>> {
        match self.ask(Query::Jobs(limit)).await? {
            StoreResult::Jobs(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }

    pub async fn job(&self, id: &str) -> Result<Option<JobRow>> {
        match self.ask(Query::Job(id.to_string())).await? {
            StoreResult::OptionalJob(v) => Ok(v),
            _ => Err(anyhow!("意外的查询结果")),
        }
    }
}

// ---------------------------------------------------------------------------
// 写线程
// ---------------------------------------------------------------------------

/// 老库补列：DuckDB 的 ADD COLUMN IF NOT EXISTS 是幂等的，可每次启动执行。
fn migrate_columns(conn: &Connection) -> Result<()> {
    for sql in [
        "ALTER TABLE devices ADD COLUMN IF NOT EXISTS owner VARCHAR DEFAULT ''",
        "ALTER TABLE devices ADD COLUMN IF NOT EXISTS group_name VARCHAR DEFAULT '默认分组'",
        "ALTER TABLE users ADD COLUMN IF NOT EXISTS parent VARCHAR DEFAULT ''",
    ] {
        if let Err(e) = conn.execute_batch(sql) {
            // 表尚未创建时忽略，后续 init_schema 会建全字段
            tracing::debug!(error = %e, sql, "跳过列迁移");
        }
    }
    Ok(())
}

/// DuckDB 资源上限（小规格云主机用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct DbLimits {
    pub threads: Option<u32>,
    pub memory_limit_mb: Option<u64>,
}

fn run_writer(
    db_path: std::path::PathBuf,
    retention_days: i64,
    limits: DbLimits,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    ready: std::sync::mpsc::Sender<Result<()>>,
    stats: Arc<StoreStats>,
) {
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("打开 DuckDB 失败: {e}")));
            return;
        }
    };
    // 默认 DuckDB 会按物理核数开线程、按内存 80% 设上限；在 1GB / 0.25 vCPU 的
    // 小机器上必须手动收紧，否则一次大范围聚合就可能 OOM 或把 CPU 抢干。
    if let Some(t) = limits.threads {
        match conn.execute_batch(&format!("PRAGMA threads={t};")) {
            Ok(()) => tracing::info!(threads = t, "DuckDB 线程数已限制"),
            Err(e) => tracing::warn!(error = %e, "设置 DuckDB 线程数失败"),
        }
    }
    if let Some(mb) = limits.memory_limit_mb {
        match conn.execute_batch(&format!("PRAGMA memory_limit='{mb}MB';")) {
            Ok(()) => tracing::info!(limit_mb = mb, "DuckDB 内存上限已限制"),
            Err(e) => tracing::warn!(error = %e, "设置 DuckDB 内存上限失败"),
        }
    }
    if let Err(e) = init_schema(&conn) {
        let _ = ready.send(Err(e));
        return;
    }
    if let Err(e) = migrate_columns(&conn) {
        let _ = ready.send(Err(e));
        return;
    }
    let mut state = WriterState {
        conn,
        partitions: BTreeSet::new(),
        stats: stats.clone(),
    };
    if let Err(e) = state.bootstrap_partitions() {
        let _ = ready.send(Err(e));
        return;
    }
    state.cleanup(retention_days);
    let _ = ready.send(Ok(()));

    // 一轮 drain：把样本、设备接入、在线状态、身份重绑四类**高频**写命令
    // 攒起来一次性落地。建连风暴（上万台同时重连）时，逐条写入会让写线程
    // 被逐行的索引维护拖死，样本写入被完全饿死；集合式批量后同样的量级
    // 从「几十秒」降到「几毫秒」（见 device_write_strategies_benchmark）。
    while let Some(cmd) = rx.blocking_recv() {
        let mut pending = PendingWrites::default();
        let mut next = Some(cmd);
        loop {
            match next.take() {
                Some(Cmd::Samples(batch)) => pending.samples.extend(batch),
                // 同一 SN 在一轮里出现多次（重连抖动）时只保留最后一次
                Some(Cmd::UpsertDevice(meta)) => {
                    pending.devices.insert(meta.sn.clone(), meta);
                }
                Some(Cmd::SetOnline { sn, online, ts_ms }) => {
                    pending.online.insert(sn, (online, ts_ms));
                }
                Some(Cmd::MigrateDeviceSn { old_sn, new_sn }) => {
                    pending.migrations.push((old_sn, new_sn));
                }
                Some(other) => {
                    // 先落地已攒的写操作，保证随后的查询能看到最新状态
                    pending.flush(&mut state);
                    state.handle(other);
                }
                None => break,
            }
            // 队列持续有货时也要定期落地，避免攒出无界内存与无界延迟
            if pending.len() >= PENDING_FLUSH_THRESHOLD {
                pending.flush(&mut state);
            }
            match rx.try_recv() {
                Ok(more) => next = Some(more),
                Err(_) => break,
            }
        }
        pending.flush(&mut state);
    }
}

/// 单轮最多攒多少条写命令就强制落地。
const PENDING_FLUSH_THRESHOLD: usize = 4096;

/// 一轮 drain 攒下的写命令。
#[derive(Default)]
struct PendingWrites {
    samples: Vec<Sample>,
    /// 按 SN 去重，保留同一轮里的最后一次元数据
    devices: HashMap<String, DeviceMeta>,
    /// 按 SN 去重，保留同一轮里的最后一次在线状态
    online: HashMap<String, (bool, i64)>,
    /// 占位 SN → 真实 SN 的重绑请求（保序，允许同一 SN 出现多次）
    migrations: Vec<(String, String)>,
}

impl PendingWrites {
    fn len(&self) -> usize {
        self.samples.len() + self.devices.len() + self.online.len() + self.migrations.len()
    }

    fn flush(&mut self, state: &mut WriterState) {
        // 重绑必须排在设备写入之前：bulk 写入会先删掉同名新行，
        // 顺序颠倒会把刚写入的真实 SN 行删掉。
        if !self.migrations.is_empty() {
            let pairs = std::mem::take(&mut self.migrations);
            if let Err(e) = state.bulk_migrate_device_sn(&pairs) {
                tracing::error!(error = %e, count = pairs.len(), "批量重绑设备身份失败");
            }
        }
        if !self.devices.is_empty() || !self.online.is_empty() {
            let devices: Vec<DeviceMeta> = self.devices.drain().map(|(_, m)| m).collect();
            let online: Vec<(String, bool, i64)> = self
                .online
                .drain()
                .map(|(sn, (on, ts))| (sn, on, ts))
                .collect();
            let (n_dev, n_on) = (devices.len(), online.len());
            match state.apply_device_batch(&devices, &online) {
                Ok(()) => tracing::debug!(devices = n_dev, online = n_on, "批量写入设备接入状态"),
                Err(e) => tracing::error!(error = %e, devices = n_dev, online = n_on, "批量写入设备接入状态失败"),
            }
        }
        if !self.samples.is_empty() {
            let batch = std::mem::take(&mut self.samples);
            let count = batch.len() as u64;
            match state.insert_samples(&batch) {
                Ok(()) => {
                    state
                        .stats
                        .samples_written
                        .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                    state
                        .stats
                        .batches
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => tracing::error!(error = %e, "写入状态样本失败"),
            }
        }
    }
}

struct WriterState {
    conn: Connection,
    partitions: BTreeSet<String>,
    stats: Arc<StoreStats>,
}

impl WriterState {
    /// 处理低频命令。高频的 Samples / UpsertDevice / SetOnline / MigrateDeviceSn
    /// 在 `run_writer` 里被 `PendingWrites` 拦下走集合式批量，不会走到这里；
    /// 这几个分支保留为单条兜底，保证语义完整。
    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Samples(batch) => {
                if let Err(e) = self.insert_samples(&batch) {
                    tracing::error!(error = %e, "写入状态样本失败");
                }
            }
            Cmd::UpsertDevice(meta) => {
                if let Err(e) = self.upsert_device(&meta) {
                    tracing::error!(error = %e, sn = %meta.sn, "写入设备元数据失败");
                }
            }
            Cmd::SetOnline { sn, online, ts_ms } => {
                if let Err(e) = self.set_online(&sn, online, ts_ms) {
                    tracing::error!(error = %e, sn = %sn, "更新在线状态失败");
                }
            }
            Cmd::UpdateFirmware { sn, version } => {
                if let Err(e) = self.conn.execute(
                    "UPDATE devices SET firmware_version = ?, updated_at_ms = ? WHERE sn = ?",
                    params![version, Utc::now().timestamp_millis(), sn],
                ) {
                    tracing::error!(error = %e, "更新固件版本失败");
                }
            }
            Cmd::MigrateDeviceSn { old_sn, new_sn } => {
                if let Err(e) = self.migrate_device_sn(&old_sn, &new_sn) {
                    tracing::error!(
                        error = %e,
                        old = %old_sn,
                        new = %new_sn,
                        "迁移设备 SN 失败"
                    );
                }
            }
            Cmd::CreateJob(job) => {
                if let Err(e) = self.create_job(&job) {
                    tracing::error!(error = %e, "创建 OTA 任务失败");
                }
            }
            Cmd::UpdateJob {
                id,
                state,
                sent_packets,
                retries,
                message,
                finished,
            } => {
                if let Err(e) =
                    self.update_job(&id, &state, sent_packets, retries, &message, finished)
                {
                    tracing::error!(error = %e, "更新 OTA 任务失败");
                }
            }
            Cmd::InsertFirmware(row) => {
                if let Err(e) = self.insert_firmware(&row) {
                    tracing::error!(error = %e, "写入固件元数据失败");
                }
            }
            Cmd::UpsertUser {
                username,
                password_hash,
                role,
                parent,
            } => {
                if let Err(e) = self.upsert_user(&username, &password_hash, &role, &parent) {
                    tracing::error!(error = %e, "写入用户失败");
                }
            }
            Cmd::SettingSet { key, value } => {
                if let Err(e) = self.conn.execute(
                    "INSERT INTO settings (key, value, updated_at_ms) VALUES (?,?,?)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
                    params![key, value, Utc::now().timestamp_millis()],
                ) {
                    tracing::error!(error = %e, key = %key, "写入系统配置失败");
                }
            }
            Cmd::AssignDevice {
                sn,
                owner,
                group_name,
            } => {
                if let Err(e) = self.assign_device(&sn, &owner, &group_name) {
                    tracing::error!(error = %e, sn = %sn, "更新设备归属失败");
                }
            }
            Cmd::SetUserParent { username, parent } => {
                if let Err(e) = self.set_user_parent(&username, &parent) {
                    tracing::error!(error = %e, "更新用户层级失败");
                }
            }
            Cmd::NormalizeParents { root } => {
                if let Err(e) = self.normalize_parents(&root) {
                    tracing::error!(error = %e, "归一化用户层级失败");
                }
            }
            Cmd::DeleteUser(username) => {
                if let Err(e) = self.delete_user(&username) {
                    tracing::error!(error = %e, "删除用户失败");
                }
            }
            Cmd::Audit {
                id,
                actor,
                action,
                target,
                detail,
            } => {
                if let Err(e) = self.conn.execute(
                    "INSERT INTO audit_log (id, ts_ms, actor, action, target, detail) VALUES (?,?,?,?,?,?)",
                    params![id, Utc::now().timestamp_millis(), actor, action, target, detail],
                ) {
                    tracing::error!(error = %e, "写入审计日志失败");
                }
            }
            Cmd::Cleanup { retention_days } => self.cleanup(retention_days),
            Cmd::Purge { keep_admin, reply } => {
                let _ = reply.send(self.purge(&keep_admin));
            }
            Cmd::Query(q, reply) => {
                let result = self.run_query(q);
                let _ = reply.send(result);
            }
        }
    }

    fn bootstrap_partitions(&mut self) -> Result<()> {
        let names: Vec<String> = self
            .conn
            .prepare("SELECT table_name FROM duckdb_tables() WHERE table_name LIKE 'ups_status_%'")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?;
        for n in names {
            if let Some(key) = n.strip_prefix("ups_status_") {
                self.partitions.insert(key.to_string());
            }
        }
        let now = Utc::now();
        self.ensure_partition(now.timestamp_millis())?;
        Ok(())
    }

    fn ensure_partition(&mut self, ts_ms: i64) -> Result<String> {
        let key = month_key(ts_ms);
        if !self.partitions.contains(&key) {
            self.conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS ups_status_{key} (
                    sn VARCHAR,
                    ts_ms BIGINT,
                    input_voltage DOUBLE,
                    output_voltage DOUBLE,
                    temperature DOUBLE,
                    battery INTEGER,
                    health INTEGER,
                    load_percent INTEGER,
                    firmware VARCHAR,
                    raw BLOB
                );"
            ))?;
            self.partitions.insert(key.clone());
            self.rebuild_view()?;
            tracing::info!(partition = %key, "已创建状态历史分区");
        }
        Ok(key)
    }

    fn rebuild_view(&mut self) -> Result<()> {
        if self.partitions.is_empty() {
            return Ok(());
        }
        let union = self
            .partitions
            .iter()
            .map(|k| format!("SELECT * FROM ups_status_{k}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        self.conn
            .execute_batch(&format!("CREATE OR REPLACE VIEW ups_status AS {union};"))?;
        Ok(())
    }

    fn insert_samples(&mut self, batch: &[Sample]) -> Result<()> {
        use std::collections::HashMap;
        let mut grouped: HashMap<String, Vec<&Sample>> = HashMap::new();
        for s in batch {
            let key = month_key(s.ts_ms);
            self.ensure_partition(s.ts_ms)?;
            grouped.entry(key).or_default().push(s);
        }
        for (key, rows) in grouped {
            let mut app = self.conn.appender(&format!("ups_status_{key}"))?;
            for s in &rows {
                app.append_row(params![
                    s.sn,
                    s.ts_ms,
                    s.input_voltage as f64,
                    s.output_voltage as f64,
                    s.temperature as f64,
                    s.battery as i32,
                    s.health as i32,
                    s.load_percent as i32,
                    s.firmware,
                    Value::Blob(s.raw.clone()),
                ])?;
            }
            app.flush()?;
        }
        tracing::debug!(count = batch.len(), "批量写入状态样本");
        Ok(())
    }

    /// 把一轮 drain 攒下的「设备接入 + 在线状态」一次性落地。
    ///
    /// 关键点：**不要逐行 `INSERT ... ON CONFLICT`**。DuckDB 的 upsert 每行都要
    /// 维护主键索引，上千台同时接入时单行成本会涨到十几毫秒（实测 2000 行 34s），
    /// 写线程被彻底占满、样本写入被饿死。改成「临时表 Appender 灌入 + 两条
    /// 集合式语句」后同样的量级降到毫秒级。
    fn apply_device_batch(
        &mut self,
        devices: &[DeviceMeta],
        online: &[(String, bool, i64)],
    ) -> Result<()> {
        if devices.is_empty() && online.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("BEGIN TRANSACTION;")?;
        match self.write_device_rows(devices, online) {
            Ok(()) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(())
            }
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK;") {
                    tracing::error!(error = %re, "回滚设备接入事务失败，写线程可能已停在事务中");
                }
                Err(e)
            }
        }
    }

    fn write_device_rows(
        &mut self,
        devices: &[DeviceMeta],
        online: &[(String, bool, i64)],
    ) -> Result<()> {
        if !devices.is_empty() {
            self.bulk_upsert_devices(devices)?;
        }
        if !online.is_empty() {
            self.bulk_set_online(online)?;
        }
        Ok(())
    }

    /// 临时表 + Appender 灌入，再两条集合式语句完成 upsert。
    fn bulk_upsert_devices(&mut self, devices: &[DeviceMeta]) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE bulk_devices (
                sn VARCHAR, mdvr_id VARCHAR, address BIGINT, baudrate BIGINT,
                report_interval BIGINT, firmware_version VARCHAR, config_json VARCHAR,
                ts BIGINT
             );",
        )?;
        {
            let mut app = self.conn.appender("bulk_devices")?;
            for m in devices {
                app.append_row(params![
                    m.sn,
                    m.mdvr_id,
                    m.address,
                    m.baudrate,
                    m.report_interval,
                    m.firmware_version,
                    m.config_json,
                    now
                ])?;
            }
            app.flush()?;
        }
        self.conn.execute_batch(
            "UPDATE devices SET
                mdvr_id = b.mdvr_id,
                address = b.address,
                baudrate = b.baudrate,
                report_interval = b.report_interval,
                updated_at_ms = b.ts
             FROM bulk_devices b WHERE devices.sn = b.sn;

             INSERT INTO devices (sn, mdvr_id, address, baudrate, report_interval,
                                  firmware_version, last_seen_ms, online, config_json,
                                  created_at_ms, updated_at_ms)
             SELECT b.sn, b.mdvr_id, b.address, b.baudrate, b.report_interval,
                    b.firmware_version, b.ts, true, b.config_json, b.ts, b.ts
             FROM bulk_devices b
             WHERE NOT EXISTS (SELECT 1 FROM devices d WHERE d.sn = b.sn);",
        )?;
        Ok(())
    }

    /// 临时表 + 一条集合式 UPDATE 更新在线状态。
    fn bulk_set_online(&mut self, rows: &[(String, bool, i64)]) -> Result<()> {
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE bulk_online (sn VARCHAR, online BOOLEAN, ts BIGINT);",
        )?;
        {
            let mut app = self.conn.appender("bulk_online")?;
            for (sn, on, ts) in rows {
                app.append_row(params![sn, *on, *ts])?;
            }
            app.flush()?;
        }
        self.conn.execute_batch(
            "UPDATE devices SET online = b.online, last_seen_ms = b.ts, updated_at_ms = b.ts
             FROM bulk_online b WHERE devices.sn = b.sn;",
        )?;
        Ok(())
    }

    fn upsert_device(&mut self, meta: &DeviceMeta) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn.execute(
            UPSERT_DEVICE_SQL,
            params![
                meta.sn,
                meta.mdvr_id,
                meta.address,
                meta.baudrate,
                meta.report_interval,
                meta.firmware_version,
                now,
                true,
                meta.config_json,
                now,
                now
            ],
        )?;
        Ok(())
    }

    fn set_online(&mut self, sn: &str, online: bool, ts_ms: i64) -> Result<()> {
        self.conn
            .execute(SET_ONLINE_SQL, params![online, ts_ms, ts_ms, sn])?;
        Ok(())
    }

    /// 占位 SN → 真实 SN：先清掉可能残留的同名旧行（避免主键冲突），
    /// 再迁移 devices 行（保留 owner / group_name 等归属），最后迁移所有月分区样本。
    fn migrate_device_sn(&mut self, old_sn: &str, new_sn: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn
            .execute("DELETE FROM devices WHERE sn = ?", params![new_sn])?;
        self.conn.execute(
            "UPDATE devices SET sn = ?, updated_at_ms = ? WHERE sn = ?",
            params![new_sn, now, old_sn],
        )?;
        let mut moved = 0u64;
        for key in self.partitions.clone() {
            let table = format!("ups_status_{key}");
            moved += self
                .conn
                .execute(
                    &format!("UPDATE \"{table}\" SET sn = ? WHERE sn = ?"),
                    params![new_sn, old_sn],
                )?
                as u64;
        }
        tracing::info!(
            old = %old_sn,
            new = %new_sn,
            samples = moved,
            "设备已从占位 SN 重绑到真实 SN"
        );
        Ok(())
    }

    /// 批量「占位 SN → 真实 SN」重绑：一次 drain 里可能同时有上千台设备
    /// 上报真实 SN，逐台三条语句会重新拖死写线程，这里同样走临时表 + 集合式。
    fn bulk_migrate_device_sn(&mut self, pairs: &[(String, String)]) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let now = Utc::now().timestamp_millis();
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE bulk_migrate (old_sn VARCHAR, new_sn VARCHAR);",
        )?;
        {
            let mut app = self.conn.appender("bulk_migrate")?;
            for (old, new) in pairs {
                app.append_row(params![old, new])?;
            }
            app.flush()?;
        }
        // 目标 SN 可能已有残留行，先清掉避免主键冲突
        self.conn.execute_batch(
            "DELETE FROM devices WHERE sn IN (SELECT new_sn FROM bulk_migrate);",
        )?;
        self.conn.execute(
            "UPDATE devices SET sn = m.new_sn, updated_at_ms = ?
             FROM bulk_migrate m WHERE devices.sn = m.old_sn",
            params![now],
        )?;
        let mut moved = 0u64;
        for key in self.partitions.clone() {
            let table = format!("ups_status_{key}");
            moved += self.conn.execute(
                &format!(
                    "UPDATE \"{table}\" SET sn = m.new_sn
                     FROM bulk_migrate m WHERE \"{table}\".sn = m.old_sn"
                ),
                [],
            )? as u64;
        }
        tracing::info!(
            devices = pairs.len(),
            samples = moved,
            "批量重绑设备身份（占位 SN → 真实 SN）"
        );
        Ok(())
    }

    fn create_job(&mut self, job: &NewJob) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ota_jobs (id, sn, firmware_id, state, total_packets, sent_packets, retries,
                                   started_at_ms, finished_at_ms, message, actor)
             VALUES (?,?,?,?,?,0,0,?,NULL,?,?)",
            params![
                job.id,
                job.sn,
                job.firmware_id,
                job.state,
                job.total_packets,
                Utc::now().timestamp_millis(),
                job.message,
                job.actor
            ],
        )?;
        Ok(())
    }

    fn update_job(
        &mut self,
        id: &str,
        state: &str,
        sent: i64,
        retries: i64,
        message: &str,
        finished: bool,
    ) -> Result<()> {
        let finished_at = if finished {
            Some(Utc::now().timestamp_millis())
        } else {
            None
        };
        self.conn.execute(
            "UPDATE ota_jobs SET state=?, sent_packets=?, retries=?, message=?,
                finished_at_ms = COALESCE(?, finished_at_ms) WHERE id=?",
            params![state, sent, retries, message, finished_at, id],
        )?;
        Ok(())
    }

    fn insert_firmware(&mut self, row: &FirmwareRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO firmware (id, filename, version, size, sha256, uploaded_at_ms, uploaded_by)
             VALUES (?,?,?,?,?,?,?)",
            params![
                row.id,
                row.filename,
                row.version,
                row.size,
                row.sha256,
                row.uploaded_at_ms,
                row.uploaded_by
            ],
        )?;
        Ok(())
    }

    fn upsert_user(&mut self, username: &str, hash: &str, role: &str, parent: &str) -> Result<()> {
        // parent 为空时不覆盖已有层级，避免重复 upsert（如启动时补种管理员）把树结构冲掉
        self.conn.execute(
            "INSERT INTO users (username, password_hash, role, created_at_ms, parent)
             VALUES (?,?,?,?,?)
             ON CONFLICT (username) DO UPDATE SET
                password_hash = excluded.password_hash,
                role = excluded.role,
                parent = CASE WHEN excluded.parent = '' THEN users.parent ELSE excluded.parent END",
            params![username, hash, role, Utc::now().timestamp_millis(), parent],
        )?;
        Ok(())
    }

    /// 删除用户。删不存在的用户是 0 行影响、不算错误，由调用方决定是否提示。
    fn delete_user(&mut self, username: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM users WHERE username = ?", params![username])?;
        Ok(())
    }

    /// 调整上级。空串表示顶级用户——handler 层会先换算成根管理员再传进来。
    fn set_user_parent(&mut self, username: &str, parent: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE users SET parent = ? WHERE username = ?",
            params![parent, username],
        )?;
        Ok(())
    }

    /// 启动时归一化用户层级。除根管理员外，把以下四类坏数据挂回根管理员：
    /// 无上级、上级不存在、自指、成环（A→B→C→B）。
    fn normalize_parents(&mut self, root: &str) -> Result<()> {
        let mut all: Vec<(String, String)> = Vec::new();
        if let Ok(mut stmt) = self.conn.prepare("SELECT username, parent FROM users")
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
        {
            all = rows.flatten().collect();
        }
        let known: HashSet<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        let mut pending: Vec<String> = Vec::new();
        // 第一遍：无上级、上级不存在、自指
        for (name, parent) in &all {
            if name == root {
                continue;
            }
            if parent.is_empty() || parent == name || !known.contains(parent.as_str()) {
                pending.push(name.clone());
            }
        }
        // 第二遍：成环。顺着 parent 链走，走到重复节点即成环，链上所有人挂回根。
        // 正常情况下走不到这里，属于脏数据自愈。
        let parent_of: HashMap<&str, &str> =
            all.iter().map(|(n, p)| (n.as_str(), p.as_str())).collect();
        for (name, _) in &all {
            if name == root || pending.contains(name) {
                continue;
            }
            let mut seen: HashSet<&str> = HashSet::new();
            let mut cur = name.as_str();
            loop {
                if !seen.insert(cur) {
                    pending.push(name.clone());
                    break;
                }
                match parent_of.get(cur) {
                    Some(p) if !p.is_empty() && known.contains(*p) => cur = p,
                    _ => break,
                }
            }
        }
        for name in &pending {
            self.conn
                .execute("UPDATE users SET parent = ? WHERE username = ?", params![root, name])?;
        }
        if !pending.is_empty() {
            tracing::info!(
                count = pending.len(),
                root = %root,
                "已把无上级 / 断链 / 自指 / 成环的用户挂到根管理员下"
            );
        }
        Ok(())
    }

    fn assign_device(&mut self, sn: &str, owner: &str, group_name: &str) -> Result<()> {
        let group = if group_name.trim().is_empty() {
            "默认分组"
        } else {
            group_name.trim()
        };
        self.conn.execute(
            "UPDATE devices SET owner = ?, group_name = ?, updated_at_ms = ? WHERE sn = ?",
            params![owner, group, Utc::now().timestamp_millis(), sn],
        )?;
        Ok(())
    }

    fn cleanup(&mut self, retention_days: i64) {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);
        let cutoff_key = month_key(cutoff.timestamp_millis());
        let expired: Vec<String> = self
            .partitions
            .iter()
            .filter(|k| k.as_str() < cutoff_key.as_str())
            .cloned()
            .collect();
        if expired.is_empty() {
            return;
        }
        for key in &expired {
            if let Err(e) = self
                .conn
                .execute_batch(&format!("DROP TABLE IF EXISTS ups_status_{key};"))
            {
                tracing::error!(error = %e, partition = %key, "删除过期分区失败");
                return;
            }
            self.partitions.remove(key);
            self.stats
                .partitions_dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(partition = %key, "已删除过期分区");
        }
        if let Err(e) = self.rebuild_view() {
            tracing::error!(error = %e, "重建 ups_status 视图失败");
        }
    }

    /// 清空全部业务数据（不可撤销）。
    ///
    /// 历史样本按分区整表 `DROP`，比 `DELETE` 快几个数量级；随后清空设备、固件、
    /// OTA 任务、审计日志与非管理员账号。整体包在一个事务里，中途失败自动回滚。
    fn purge(&mut self, keep_admin: &str) -> Result<PurgeReport> {
        let mut report = PurgeReport::default();
        self.conn.execute_batch("BEGIN TRANSACTION;")?;
        match self.purge_rows(keep_admin, &mut report) {
            Ok(()) => self.conn.execute_batch("COMMIT;")?,
            Err(e) => {
                if let Err(re) = self.conn.execute_batch("ROLLBACK;") {
                    tracing::error!(error = %re, "回滚清空事务失败，写线程可能已停在事务中");
                }
                // 分区集合已被改动，回滚后按库内实际状态重新对齐
                self.partitions.clear();
                if let Err(re) = self.bootstrap_partitions() {
                    tracing::error!(error = %re, "清空回滚后重新加载分区失败");
                }
                return Err(e);
            }
        }
        // 立即压缩：否则已删除的行仍占着文件与 WAL 空间
        if let Err(e) = self.conn.execute_batch("CHECKPOINT;") {
            tracing::warn!(error = %e, "清空后 CHECKPOINT 失败（不影响数据正确性）");
        }
        Ok(report)
    }

    /// 事务内的实际清理动作。
    fn purge_rows(&mut self, keep_admin: &str, report: &mut PurgeReport) -> Result<()> {
        let keys: Vec<String> = self.partitions.iter().cloned().collect();
        for key in keys {
            let table = format!("ups_status_{key}");
            report.samples += count_rows(&self.conn, &format!("SELECT count(*) FROM {table}"));
            report.partitions += 1;
            self.conn
                .execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
            self.partitions.remove(&key);
        }
        // 分区全删后视图会指向不存在的表，先删掉，稍后按当前月重建
        self.conn.execute_batch("DROP VIEW IF EXISTS ups_status;")?;

        report.devices = count_rows(&self.conn, "SELECT count(*) FROM devices");
        self.conn.execute_batch("DELETE FROM devices;")?;

        report.firmware = count_rows(&self.conn, "SELECT count(*) FROM firmware");
        self.conn.execute_batch("DELETE FROM firmware;")?;

        report.jobs = count_rows(&self.conn, "SELECT count(*) FROM ota_jobs");
        self.conn.execute_batch("DELETE FROM ota_jobs;")?;

        report.audit = count_rows(&self.conn, "SELECT count(*) FROM audit_log");
        self.conn.execute_batch("DELETE FROM audit_log;")?;

        // 保留管理员账号，否则清空后没人能登录
        report.users = self
            .conn
            .query_row(
                "SELECT count(*) FROM users WHERE username <> ?",
                params![keep_admin],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;
        self.conn.execute(
            "DELETE FROM users WHERE username <> ?",
            params![keep_admin],
        )?;

        // 立刻建好当前月分区，保证清空后视图可用
        self.ensure_partition(Utc::now().timestamp_millis())?;
        Ok(())
    }

    fn run_query(&mut self, q: Query) -> StoreResult {
        match self.try_query(q) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "DuckDB 查询失败");
                StoreResult::Unit
            }
        }
    }

    fn try_query(&mut self, q: Query) -> Result<StoreResult> {
        match q {
            Query::Devices => {
                let mut stmt = self.conn.prepare(
                    "SELECT sn, mdvr_id, address, baudrate, report_interval, firmware_version,
                            last_seen_ms, online, config_json,
                            COALESCE(owner, ''), COALESCE(group_name, '默认分组')
                     FROM devices ORDER BY sn",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(DeviceRow {
                            sn: row.get(0)?,
                            mdvr_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            address: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                            baudrate: row.get::<_, Option<i64>>(3)?.unwrap_or(9600),
                            report_interval: row.get::<_, Option<i64>>(4)?.unwrap_or(30),
                            firmware_version: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                            last_seen_ms: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                            online: row.get::<_, Option<bool>>(7)?.unwrap_or(false),
                            config_json: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                            owner: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                            group_name: row
                                .get::<_, Option<String>>(10)?
                                .unwrap_or_else(|| "默认分组".into()),
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Devices(rows))
            }
            Query::LatestAll => {
                // 每台设备取最新一行：QUALIFY + row_number 在 DuckDB 里比自连接更快
                let mut stmt = self.conn.prepare(
                    "SELECT sn, battery, health, ts_ms FROM ups_status
                     QUALIFY row_number() OVER (PARTITION BY sn ORDER BY ts_ms DESC) = 1",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(LatestRow {
                            sn: row.get(0)?,
                            battery: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                            health: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                            ts_ms: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::LatestAll(rows))
            }
            Query::History(h) => {
                let mut stmt = self.conn.prepare(
                    "SELECT ts_ms, input_voltage, output_voltage, temperature, battery, health,
                            load_percent, firmware
                     FROM ups_status WHERE sn = ? AND ts_ms BETWEEN ? AND ?
                     ORDER BY ts_ms DESC LIMIT ?",
                )?;
                let rows = stmt
                    .query_map(params![h.sn, h.from_ms, h.to_ms, h.limit], map_sample)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Samples(rows))
            }
            Query::Buckets {
                sn,
                from_ms,
                to_ms,
                bucket_ms,
            } => {
                let bucket_ms = bucket_ms.max(1000);
                let mut stmt = self.conn.prepare(
                    // 注意：DuckDB 的 `/` 是浮点除法，会把每个样本分成一组导致聚合失效；
                    // 整除必须用 `//`（或 CAST(FLOOR(ts_ms / ?) AS BIGINT)）。
                    "SELECT (ts_ms // ?) * ? AS bucket,
                            avg(input_voltage), avg(output_voltage), avg(temperature),
                            avg(battery), min(battery), max(battery), count(*)
                     FROM ups_status WHERE sn = ? AND ts_ms BETWEEN ? AND ?
                     GROUP BY bucket ORDER BY bucket ASC",
                )?;
                let rows = stmt
                    .query_map(
                        params![bucket_ms, bucket_ms, sn, from_ms, to_ms],
                        |row| {
                            Ok(Bucket {
                                ts_ms: row.get(0)?,
                                avg_input: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                                avg_output: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                                avg_temp: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                                avg_battery: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                                min_battery: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                                max_battery: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                                samples: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                            })
                        },
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Buckets(rows))
            }
            Query::Latest { sn } => {
                let mut stmt = self.conn.prepare(
                    "SELECT ts_ms, input_voltage, output_voltage, temperature, battery, health,
                            load_percent, firmware
                     FROM ups_status WHERE sn = ? ORDER BY ts_ms DESC LIMIT 1",
                )?;
                let mut rows = stmt.query_map(params![sn], map_sample)?;
                Ok(StoreResult::OptionalSample(rows.next().transpose()?))
            }
            Query::SettingsAll => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT key, value FROM settings")?;
                let mut map = HashMap::new();
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (k, v) in rows {
                    map.insert(k, v);
                }
                Ok(StoreResult::Settings(map))
            }
            Query::Dashboard => {
                let since = Utc::now().timestamp_millis() - 24 * 3600 * 1000;
                let mut stmt = self.conn.prepare(
                    "SELECT
                        (SELECT count(*) FROM devices),
                        (SELECT count(*) FROM ups_status WHERE ts_ms >= ?),
                        (SELECT count(*) FROM ups_status),
                        (SELECT count(*) FROM ota_jobs WHERE state = 'success'),
                        (SELECT count(*) FROM ota_jobs WHERE state IN ('success','failed','aborted'))",
                )?;
                let stats = stmt.query_row(params![since], |row| {
                    Ok(DashboardStats {
                        devices: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        samples_24h: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        samples_total: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        ota_ok: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        ota_done: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    })
                })?;
                Ok(StoreResult::Stats(stats))
            }
            Query::Users => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT username, role, created_at_ms, COALESCE(parent, '')
                         FROM users ORDER BY username",
                    )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(UserRow {
                            username: row.get(0)?,
                            role: row.get(1)?,
                            created_at_ms: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                            parent: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Users(rows))
            }
            Query::User(name) => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT username, password_hash, role FROM users WHERE username = ?")?;
                let mut rows = stmt.query_map(params![name], |row| {
                    Ok(UserCredential {
                        username: row.get(0)?,
                        password_hash: row.get(1)?,
                        role: row.get(2)?,
                    })
                })?;
                Ok(StoreResult::OptionalUser(rows.next().transpose()?))
            }
            Query::Firmware => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, filename, version, size, sha256, uploaded_at_ms, uploaded_by
                     FROM firmware ORDER BY uploaded_at_ms DESC",
                )?;
                let rows = stmt
                    .query_map([], map_firmware)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Firmware(rows))
            }
            Query::FirmwareById(id) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, filename, version, size, sha256, uploaded_at_ms, uploaded_by
                     FROM firmware WHERE id = ?",
                )?;
                let mut rows = stmt.query_map(params![id], map_firmware)?;
                Ok(StoreResult::OptionalFirmware(rows.next().transpose()?))
            }
            Query::Jobs(limit) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, sn, firmware_id, state, total_packets, sent_packets, retries,
                            started_at_ms, finished_at_ms, message, actor
                     FROM ota_jobs ORDER BY started_at_ms DESC LIMIT ?",
                )?;
                let rows = stmt
                    .query_map(params![limit], map_job)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(StoreResult::Jobs(rows))
            }
            Query::Job(id) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, sn, firmware_id, state, total_packets, sent_packets, retries,
                            started_at_ms, finished_at_ms, message, actor
                     FROM ota_jobs WHERE id = ?",
                )?;
                let mut rows = stmt.query_map(params![id], map_job)?;
                Ok(StoreResult::OptionalJob(rows.next().transpose()?))
            }
        }
    }
}

fn map_sample(row: &duckdb::Row<'_>) -> duckdb::Result<SampleRow> {
    Ok(SampleRow {
        ts_ms: row.get(0)?,
        input_voltage: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
        output_voltage: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
        temperature: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
        battery: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
        health: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        load_percent: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        firmware: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
    })
}

fn map_firmware(row: &duckdb::Row<'_>) -> duckdb::Result<FirmwareRow> {
    Ok(FirmwareRow {
        id: row.get(0)?,
        filename: row.get(1)?,
        version: row.get(2)?,
        size: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
        sha256: row.get(4)?,
        uploaded_at_ms: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        uploaded_by: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
    })
}

fn map_job(row: &duckdb::Row<'_>) -> duckdb::Result<JobRow> {
    Ok(JobRow {
        id: row.get(0)?,
        sn: row.get(1)?,
        firmware_id: row.get(2)?,
        state: row.get(3)?,
        total_packets: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
        sent_packets: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        retries: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        started_at_ms: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
        finished_at_ms: row.get(8)?,
        message: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        actor: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
    })
}

/// 统计行数；表不存在或查询失败按 0 计（清空是运维操作，不该因此中断）。
fn count_rows(conn: &Connection, sql: &str) -> u64 {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as u64
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS devices (
            sn VARCHAR PRIMARY KEY,
            mdvr_id VARCHAR,
            address INTEGER,
            baudrate INTEGER,
            report_interval INTEGER,
            firmware_version VARCHAR,
            last_seen_ms BIGINT,
            online BOOLEAN,
            config_json VARCHAR,
            created_at_ms BIGINT,
            updated_at_ms BIGINT,
            owner VARCHAR DEFAULT '',
            group_name VARCHAR DEFAULT '默认分组'
        );
        CREATE TABLE IF NOT EXISTS users (
            username VARCHAR PRIMARY KEY,
            password_hash VARCHAR,
            role VARCHAR,
            created_at_ms BIGINT,
            parent VARCHAR DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS firmware (
            id VARCHAR PRIMARY KEY,
            filename VARCHAR,
            version VARCHAR,
            size BIGINT,
            sha256 VARCHAR,
            uploaded_at_ms BIGINT,
            uploaded_by VARCHAR
        );
        CREATE TABLE IF NOT EXISTS ota_jobs (
            id VARCHAR PRIMARY KEY,
            sn VARCHAR,
            firmware_id VARCHAR,
            state VARCHAR,
            total_packets INTEGER,
            sent_packets INTEGER,
            retries INTEGER,
            started_at_ms BIGINT,
            finished_at_ms BIGINT,
            message VARCHAR,
            actor VARCHAR
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id VARCHAR PRIMARY KEY,
            ts_ms BIGINT,
            actor VARCHAR,
            action VARCHAR,
            target VARCHAR,
            detail VARCHAR
        );
        CREATE TABLE IF NOT EXISTS settings (
            key VARCHAR PRIMARY KEY,
            value VARCHAR,
            updated_at_ms BIGINT
        );",
    )?;
    Ok(())
}

fn month_key(ts_ms: i64) -> String {
    let dt = Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .unwrap_or_else(Utc::now);
    format!("{:04}{:02}", dt.year(), dt.month())
}

/// 供外部检查数据库文件是否可读（启动自检用）。
pub fn db_file_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bucket SQL 的防回归测试：曾因 DuckDB `/` 是浮点除法导致每个样本自成一组、
    /// 「5 分钟聚合」实际输出原始样本（曲线锯齿、噪声大）。这里锁死整数分桶行为。
    #[test]
    fn bucket_sql_aggregates_by_time_window() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ups_status_202610 (
                sn VARCHAR, ts_ms BIGINT, input_voltage DOUBLE, output_voltage DOUBLE,
                temperature DOUBLE, battery INTEGER, health INTEGER, load_percent INTEGER,
                firmware VARCHAR, raw BLOB)",
        )
        .unwrap();

        // 40 分钟、每 30s 一个样本 = 80 行；5 分钟分桶应得 8 桶、每桶 10 行
        // 基准时间对齐到 5 分钟网格（分桶按绝对时刻取整，不对齐会多跨一个桶）
        let base = (1_767_200_000_000i64 / 300_000) * 300_000;
        {
            let mut app = conn.appender("ups_status_202610").unwrap();
            for i in 0..80 {
                app.append_row(params![
                    "UPS-T",
                    base + i as i64 * 30_000,
                    10.0 + (i % 3) as f64,
                    13.0,
                    60.0 + (i % 4) as f64,
                    (i % 100),
                    0,
                    0,
                    "",
                    Value::Blob(vec![]),
                ])
                .unwrap();
            }
            app.flush().unwrap();
        }

        let bucket_ms = 5 * 60 * 1000i64;
        let mut stmt = conn
            .prepare(
                "SELECT (ts_ms // ?) * ? AS bucket, count(*), avg(battery)
                 FROM ups_status_202610 WHERE ts_ms BETWEEN ? AND ?
                 GROUP BY bucket ORDER BY bucket ASC",
            )
            .unwrap();
        let rows: Vec<(i64, i64, f64)> = stmt
            .query_map(
                params![bucket_ms, bucket_ms, base, base + 40 * 60 * 1000],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(rows.len(), 8, "应聚合成 8 个 5 分钟桶，实际 {:?}", rows.len());
        assert!(
            rows.iter().all(|(_, n, _)| *n == 10),
            "每桶应含 10 个样本: {rows:?}"
        );
        // 第一桶 battery = 0,1,2,...,9 的均值 4.5
        assert!((rows[0].2 - 4.5).abs() < 1e-9);
    }

    fn test_state() -> WriterState {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        WriterState {
            conn,
            partitions: BTreeSet::new(),
            stats: Arc::new(StoreStats::default()),
        }
    }

    fn meta(sn: &str, mdvr: &str) -> DeviceMeta {
        DeviceMeta {
            sn: sn.to_string(),
            mdvr_id: mdvr.to_string(),
            address: 0,
            baudrate: 9600,
            report_interval: 30,
            firmware_version: String::new(),
            config_json: "{}".into(),
        }
    }

    /// 建连风暴的核心保证：一批设备接入写操作要么全成、要么全不成，
    /// 且重复 SN 的元数据与在线状态以最后一次为准。
    #[test]
    fn device_batch_writes_all_rows_in_one_transaction() {
        let mut state = test_state();
        let devices = vec![
            meta("UPS-1", "MDVR-A"),
            meta("UPS-2", "MDVR-B"),
            meta("UPS-3", "MDVR-C"),
        ];
        let online = vec![
            ("UPS-1".to_string(), true, 1_000),
            ("UPS-2".to_string(), true, 1_000),
            ("UPS-3".to_string(), true, 1_000),
        ];
        state.apply_device_batch(&devices, &online).unwrap();

        // 第二轮：UPS-1 换了 MDVR 且下线，其余不变
        state
            .apply_device_batch(&[meta("UPS-1", "MDVR-A2")], &[("UPS-1".to_string(), false, 2_000)])
            .unwrap();

        let mut stmt = state
            .conn
            .prepare("SELECT sn, mdvr_id, online, last_seen_ms FROM devices ORDER BY sn")
            .unwrap();
        let rows: Vec<(String, String, bool, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(rows.len(), 3, "三台设备都应落库: {rows:?}");
        assert_eq!(rows[0].0, "UPS-1");
        assert_eq!(rows[0].1, "MDVR-A2", "重复接入应覆盖元数据");
        assert!(!rows[0].2, "最后一次在线状态应生效");
        assert_eq!(rows[0].3, 2_000);
        assert_eq!(rows[1].0, "UPS-2");
        assert_eq!(rows[1].1, "MDVR-B");
        assert!(rows[1].2);
    }

    /// 事务中途失败必须整批回滚，不能留下半截接入记录。
    #[test]
    fn device_batch_rolls_back_partial_rows_on_error() {
        let mut state = test_state();
        state
            .conn
            .execute_batch(
                "DROP TABLE devices;
                 CREATE TABLE devices (
                    sn VARCHAR PRIMARY KEY,
                    mdvr_id VARCHAR,
                    address BIGINT CHECK (address >= 0),
                    baudrate BIGINT,
                    report_interval BIGINT,
                    firmware_version VARCHAR,
                    last_seen_ms BIGINT,
                    online BOOLEAN,
                    config_json VARCHAR,
                    created_at_ms BIGINT,
                    updated_at_ms BIGINT
                 );",
            )
            .unwrap();

        let mut bad = meta("UPS-BAD", "MDVR-B");
        bad.address = -1; // 触发 CHECK 约束
        let err = state.apply_device_batch(&[meta("UPS-OK", "MDVR-A"), bad], &[]);
        assert!(err.is_err(), "违反 CHECK 约束应报错");

        let n: i64 = state
            .conn
            .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "整批必须回滚，不能留下 UPS-OK 的半截数据");
    }

    /// 批量重绑：devices 行与历史样本都要跟着换 SN，且不能残留占位行。
    #[test]
    fn bulk_migrate_moves_device_and_samples() {
        let mut state = test_state();
        let ts = 1_767_200_000_000i64;
        let olds = ["UPS-1.2.3.4", "UPS-1.2.3.5"];
        let devices: Vec<DeviceMeta> = olds.iter().map(|s| meta(s, "MDVR-A")).collect();
        state
            .apply_device_batch(&devices, &[(olds[0].into(), true, ts), (olds[1].into(), true, ts)])
            .unwrap();

        state.ensure_partition(ts).unwrap();
        let samples: Vec<Sample> = olds
            .iter()
            .map(|sn| Sample {
                sn: sn.to_string(),
                ts_ms: ts,
                input_voltage: 10.0,
                output_voltage: 13.0,
                temperature: 25.0,
                battery: 80,
                health: 0,
                load_percent: 0,
                firmware: String::new(),
                raw: Vec::new(),
            })
            .collect();
        state.insert_samples(&samples).unwrap();

        let pairs = vec![
            (olds[0].to_string(), "UPS-REAL-0".to_string()),
            (olds[1].to_string(), "UPS-REAL-1".to_string()),
        ];
        state.bulk_migrate_device_sn(&pairs).unwrap();

        let placeholder: i64 = state
            .conn
            .query_row(
                "SELECT count(*) FROM devices WHERE sn LIKE 'UPS-1.%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(placeholder, 0, "占位设备行必须全部迁走");

        let real: i64 = state
            .conn
            .query_row("SELECT count(*) FROM devices WHERE sn LIKE 'UPS-REAL-%'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(real, 2);

        let moved: i64 = state
            .conn
            .query_row(
                "SELECT count(*) FROM ups_status WHERE sn LIKE 'UPS-REAL-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(moved, 2, "历史样本的 SN 也要一起迁移");
    }

    /// 存储密度基准（手动运行）：`cargo test --release storage_density -- --ignored --nocapture`
    ///
    /// 用来把「能存多少台 / 保留多少天」换算成实际磁盘占用。
    /// 30s 上报 = 2880 行/设备/天。
    #[test]
    #[ignore]
    fn storage_density_benchmark() {
        const ROWS: usize = 200_000;
        let dir = std::env::temp_dir().join(format!("ups_density_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("density.duckdb");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        let mut state = WriterState {
            conn,
            partitions: BTreeSet::new(),
            stats: Arc::new(StoreStats::default()),
        };
        let base = (1_767_200_000_000i64 / 300_000) * 300_000;
        let batch: Vec<Sample> = (0..ROWS)
            .map(|i| Sample {
                sn: format!("UPS-{:05}", i % 2000),
                ts_ms: base + (i as i64 / 2000) * 30_000,
                input_voltage: 10.0 + (i % 7) as f32 * 0.1,
                output_voltage: 13.0 + (i % 5) as f32 * 0.05,
                temperature: 25.0 + (i % 11) as f32,
                battery: (60 + i % 40) as u8,
                health: 0,
                load_percent: 0,
                firmware: String::new(),
                raw: Vec::new(),
            })
            .collect();
        let t = std::time::Instant::now();
        state.insert_samples(&batch).unwrap();
        let write = t.elapsed();
        state.conn.execute_batch("CHECKPOINT;").unwrap();
        drop(state);

        let bytes = std::fs::metadata(&path).unwrap().len();
        let per_row = bytes as f64 / ROWS as f64;
        let per_dev_day = per_row * 2880.0;
        println!("写入 {ROWS} 行耗时 {write:?}");
        println!("库文件 {:.2} MB", bytes as f64 / 1_048_576.0);
        println!("单行 {per_row:.1} B");
        println!("单台/天(30s) {:.1} KB", per_dev_day / 1024.0);
        println!("1 万台/天 {:.0} MB", per_dev_day * 10_000.0 / 1_048_576.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一轮 drain 内同一 SN 的多次写只保留最后一次，避免重复提交。
    #[test]
    fn pending_writes_dedupe_by_sn() {        let mut p = PendingWrites::default();
        p.devices.insert("UPS-1".into(), meta("UPS-1", "MDVR-A"));
        p.devices.insert("UPS-1".into(), meta("UPS-1", "MDVR-B"));
        p.online.insert("UPS-1".into(), (false, 10));
        p.online.insert("UPS-1".into(), (true, 20));
        p.samples.push(Sample {
            sn: "UPS-1".into(),
            ts_ms: 1,
            input_voltage: 10.0,
            output_voltage: 13.0,
            temperature: 25.0,
            battery: 80,
            health: 0,
            load_percent: 0,
            firmware: String::new(),
            raw: Vec::new(),
        });

        assert_eq!(p.len(), 3, "样本 1 条 + 设备 1 条 + 在线 1 条");
        assert_eq!(p.devices["UPS-1"].mdvr_id, "MDVR-B");
        assert_eq!(p.online["UPS-1"], (true, 20));
    }

    /// 性能基准（手动运行）：`cargo test --release device_write_strategies -- --ignored --nocapture`
    ///
    /// 结论：逐行 `INSERT ... ON CONFLICT` 在 DuckDB 里是 O(行) 的索引维护，
    /// 且放进显式事务后反而更慢（undo 版本化）；批量接入应走
    /// 「临时表 Appender + 两条集合式语句」。
    #[test]
    #[ignore]
    fn device_write_strategies_benchmark() {
        use std::time::Instant;
        const N: usize = 3000;

        fn setup() -> WriterState {
            test_state()
        }
        fn rows() -> Vec<DeviceMeta> {
            (0..N).map(|i| meta(&format!("UPS-{i:05}"), "MDVR-A")).collect()
        }

        // 1) 逐行，无显式事务
        let mut st = setup();
        let devs = rows();
        let t = Instant::now();
        for m in &devs {
            st.upsert_device(m).unwrap();
        }
        println!("逐行 无事务       : {:?}", t.elapsed());

        // 2) 逐行，包在一个事务里
        let mut st = setup();
        let t = Instant::now();
        st.conn.execute_batch("BEGIN TRANSACTION;").unwrap();
        for m in &devs {
            st.upsert_device(m).unwrap();
        }
        st.conn.execute_batch("COMMIT;").unwrap();
        println!("逐行 单事务       : {:?}", t.elapsed());

        // 3) 临时表 Appender + 集合式 UPDATE / INSERT
        let mut st = setup();
        let t = Instant::now();
        st.bulk_upsert_devices(&devs).unwrap();
        println!("临时表 集合式     : {:?}", t.elapsed());

        // 4) 第二轮（设备已存在，模拟重连）：逐行 vs 集合式
        let t = Instant::now();
        for m in &devs {
            st.upsert_device(m).unwrap();
        }
        println!("逐行 无事务 重连  : {:?}", t.elapsed());

        let t = Instant::now();
        st.bulk_upsert_devices(&devs).unwrap();
        println!("临时表 集合式 重连: {:?}", t.elapsed());

        let n: i64 = st
            .conn
            .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, N);
    }

    /// 清空业务数据：设备/样本/固件/任务/审计/非管理员账号全清，
    /// 系统设置与管理员账号必须保留（否则清空后无法登录、保留策略丢失）。
    #[test]
    fn purge_clears_business_data_but_keeps_admin_and_settings() {
        let mut st = test_state();
        st.conn
            .execute(
                "INSERT INTO users (username, password_hash, role, created_at_ms, parent)
                 VALUES ('admin','h','admin',1,''), ('viewer','h','viewer',1,'')",
                [],
            )
            .unwrap();
        st.conn
            .execute(
                "INSERT INTO settings (key, value, updated_at_ms)
                 VALUES ('retention_days','180',1)",
                [],
            )
            .unwrap();
        st.apply_device_batch(&[meta("UPS-1", "MDVR-1"), meta("UPS-2", "MDVR-2")], &[])
            .unwrap();
        st.conn
            .execute(
                "INSERT INTO firmware (id, filename, version, size, sha256, uploaded_at_ms, uploaded_by)
                 VALUES ('f1','a.bin','1.0',10,'x',1,'admin')",
                [],
            )
            .unwrap();
        st.conn
            .execute(
                "INSERT INTO ota_jobs (id, sn, firmware_id, state, total_packets, sent_packets,
                                       retries, started_at_ms, finished_at_ms, message, actor)
                 VALUES ('j1','UPS-1','f1','done',1,1,0,1,2,'','admin')",
                [],
            )
            .unwrap();
        st.conn
            .execute(
                "INSERT INTO audit_log (id, ts_ms, actor, action, target, detail)
                 VALUES ('a1',1,'admin','login','','')",
                [],
            )
            .unwrap();
        st.insert_samples(&[Sample {
            sn: "UPS-1".into(),
            ts_ms: 1_700_000_000_000,
            input_voltage: 10.0,
            output_voltage: 14.5,
            temperature: 30.0,
            battery: 90,
            health: 0,
            load_percent: 0,
            firmware: String::new(),
            raw: Vec::new(),
        }])
        .unwrap();

        let report = st.purge("admin").unwrap();
        assert_eq!(report.devices, 2);
        assert_eq!(report.samples, 1);
        assert_eq!(report.firmware, 1);
        assert_eq!(report.jobs, 1);
        assert_eq!(report.audit, 1);
        assert_eq!(report.users, 1, "只删非管理员账号");

        for table in ["devices", "firmware", "ota_jobs", "audit_log", "ups_status"] {
            assert_eq!(
                count_rows(&st.conn, &format!("SELECT count(*) FROM {table}")),
                0,
                "{table} 应被清空"
            );
        }
        assert_eq!(count_rows(&st.conn, "SELECT count(*) FROM users"), 1);
        assert_eq!(
            count_rows(&st.conn, "SELECT count(*) FROM users WHERE username='admin'"),
            1
        );
        assert_eq!(count_rows(&st.conn, "SELECT count(*) FROM settings"), 1);
        // 清空后当前月分区已重建，`ups_status` 视图仍可用
        assert!(!st.partitions.is_empty());
    }

    // ---------------- 用户层级 ----------------

    fn parent_of(st: &WriterState, name: &str) -> String {
        st.conn
            .query_row(
                "SELECT parent FROM users WHERE username = ?",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn user_count(st: &WriterState) -> i64 {
        st.conn
            .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap()
    }

    /// 重置密码走的是 `upsert_user(.., parent=None)`：口令要变，层级不能被冲掉。
    #[test]
    fn upsert_user_without_parent_keeps_hierarchy() {
        let mut st = test_state();
        st.upsert_user("alice", "hash1", "viewer", "admin").unwrap();
        assert_eq!(parent_of(&st, "alice"), "admin");
        st.upsert_user("alice", "hash2", "viewer", "").unwrap();
        assert_eq!(parent_of(&st, "alice"), "admin", "parent 为空应保留原上级");
        let hash: String = st
            .conn
            .query_row(
                "SELECT password_hash FROM users WHERE username='alice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash, "hash2", "口令应被更新");
    }

    #[test]
    fn delete_user_removes_row() {
        let mut st = test_state();
        st.upsert_user("tmp", "h", "viewer", "admin").unwrap();
        assert_eq!(user_count(&st), 1);
        st.delete_user("tmp").unwrap();
        assert_eq!(user_count(&st), 0);
        // 删不存在的用户不应报错，也不应影响别的行
        st.upsert_user("keep", "h", "viewer", "admin").unwrap();
        st.delete_user("ghost").unwrap();
        assert_eq!(user_count(&st), 1);
    }

    #[test]
    fn set_user_parent_moves_user() {
        let mut st = test_state();
        st.upsert_user("a", "h", "operator", "admin").unwrap();
        st.upsert_user("b", "h", "viewer", "admin").unwrap();
        st.set_user_parent("b", "a").unwrap();
        assert_eq!(parent_of(&st, "b"), "a");
        // 空 parent 会被写进去，由 handler 层统一换算成根管理员
        st.set_user_parent("b", "").unwrap();
        assert_eq!(parent_of(&st, "b"), "");
    }

    /// 启动自愈：无上级、上级不存在、自指、成环，四种坏数据都要挂回根管理员。
    #[test]
    fn normalize_parents_repairs_broken_hierarchies() {
        let mut st = test_state();
        st.upsert_user("admin", "h", "admin", "").unwrap();
        st.upsert_user("orphan", "h", "viewer", "").unwrap(); // 无上级
        st.upsert_user("dangling", "h", "viewer", "ghost").unwrap(); // 上级不存在
        st.upsert_user("selfref", "h", "viewer", "selfref").unwrap(); // 自指
        st.upsert_user("ring_a", "h", "viewer", "ring_b").unwrap(); // 成环
        st.upsert_user("ring_b", "h", "viewer", "ring_a").unwrap();
        st.upsert_user("ok", "h", "viewer", "admin").unwrap(); // 好的别动

        st.normalize_parents("admin").unwrap();

        for bad in ["orphan", "dangling", "selfref", "ring_a", "ring_b"] {
            assert_eq!(parent_of(&st, bad), "admin", "{bad} 应被挂回根管理员");
        }
        assert_eq!(parent_of(&st, "ok"), "admin", "正常层级不受影响");
        assert_eq!(parent_of(&st, "admin"), "", "根管理员保持顶级");
    }

    /// 成环且链尾不在环上的情况：环成员修掉，链外的人也要有稳定归宿。
    #[test]
    fn normalize_parents_handles_tail_leading_into_ring() {
        let mut st = test_state();
        st.upsert_user("admin", "h", "admin", "").unwrap();
        st.upsert_user("tail", "h", "viewer", "ring_a").unwrap(); // 环外，指向环
        st.upsert_user("ring_a", "h", "viewer", "ring_b").unwrap();
        st.upsert_user("ring_b", "h", "viewer", "ring_a").unwrap();

        st.normalize_parents("admin").unwrap();

        // 环上两人一定修掉；tail 至少有确定上级（不会悬空）
        assert_eq!(parent_of(&st, "ring_a"), "admin");
        assert_eq!(parent_of(&st, "ring_b"), "admin");
        assert!(!parent_of(&st, "tail").is_empty());
    }
}
