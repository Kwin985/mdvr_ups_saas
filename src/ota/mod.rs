//! OTA 升级协调器。
//!
//! 与协议对齐的有状态升级流程（`docs/UPS串口通讯协议（RS485）v1.4.doc` 3.4 节）：
//!   * `S`：载荷 `Size(2B BE) + Number(2B BE)`，3 秒内每 100ms 重试，直到应答 0x30；
//!   * `D`：载荷 `FrameID(2B BE) + 64 字节数据`，逐包等待 `FrameID + ack`；
//!       - 0x30 继续
//!       - 0x31 校验错，重发当前包（最多 N 次）
//!       - 0x32 写 Flash 失败，整轮从 S 重新开始
//!   * `F`：结束升级（空载荷），等待 0x30；0x31 重发、0x32 需整轮重来。
//!
//! 升级期间设备置为 busy，普通轮询/配置下发被接入层拒绝，避免应答混淆。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

use crate::config::Config;
use crate::settings::SettingsStore;
use crate::ingest::registry::{Registry, SendError};
use crate::metrics::Metrics;
use crate::protocol::{Frame, ack, cmd, codec};
use crate::storage::{FirmwareRow, NewJob, Store};

#[derive(Debug, thiserror::Error)]
pub enum OtaError {
    #[error("设备不在线")]
    Offline,
    #[error("设备正在升级中")]
    Busy,
    #[error("固件不存在或文件缺失")]
    FirmwareMissing,
    #[error("固件包过大（最多 {0} 包）")]
    TooLarge(usize),
    #[error("设备已取消升级")]
    Cancelled,
    #[error("等待设备应答超时")]
    Timeout,
    #[error("设备拒绝升级，应答 {0:#04X}")]
    Rejected(u8),
    #[error("Flash 写入失败（0x32），已重试 {0} 轮")]
    FlashFail(u32),
    #[error("下发失败: {0}")]
    Send(String),
    #[error("存储错误: {0}")]
    Store(String),
}

impl From<SendError> for OtaError {
    fn from(e: SendError) -> Self {
        OtaError::Send(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {
    pub id: String,
    pub sn: String,
    pub firmware_id: String,
    pub firmware_version: String,
    pub state: String,
    pub total_packets: u16,
    pub sent_packets: u16,
    pub retries: u32,
    pub percent: u8,
    pub message: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

impl JobProgress {
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "success" | "failed" | "aborted")
    }
}

pub struct OtaManager {
    registry: Arc<Registry>,
    store: Store,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    /// 系统配置（运行时可变）：OTA 超时 / 重试 / 窗口参数
    settings: Arc<SettingsStore>,
    /// sn -> 当前升级任务的应答接收端
    acks: DashMap<String, mpsc::Sender<u8>>,
    /// job_id -> 实时进度
    jobs: DashMap<String, JobProgress>,
    /// job_id -> 取消标志
    cancels: DashMap<String, Arc<AtomicBool>>,
}

impl OtaManager {
    pub fn new(
        registry: Arc<Registry>,
        store: Store,
        cfg: Arc<Config>,
        metrics: Arc<Metrics>,
        settings: Arc<SettingsStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            store,
            cfg,
            metrics,
            settings,
            acks: DashMap::new(),
            jobs: DashMap::new(),
            cancels: DashMap::new(),
        })
    }

    /// 接入层收到 ACK 帧后调用。
    pub fn deliver_ack(&self, sn: &str, code: u8) {
        if let Some(tx) = self.acks.get(sn) {
            let _ = tx.try_send(code);
        }
    }

    pub fn progress(&self, job_id: &str) -> Option<JobProgress> {
        self.jobs.get(job_id).map(|j| j.clone())
    }

    pub fn active_jobs(&self) -> Vec<JobProgress> {
        let mut v: Vec<_> = self
            .jobs
            .iter()
            .filter(|j| !j.is_finished())
            .map(|j| j.clone())
            .collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.started_at_ms));
        v
    }

    /// 设备离线时取消其进行中的升级。
    pub async fn on_device_offline(&self, sn: &str) {
        let mut cancelled = false;
        for item in self.cancels.iter() {
            if item.key().starts_with(sn) {
                item.value().store(true, Ordering::SeqCst);
                cancelled = true;
            }
        }
        if cancelled {
            tracing::warn!(sn = %sn, "设备离线，已请求取消 OTA");
        }
    }

    /// 发起升级，立即返回 job_id；实际流程在后台任务中推进。
    pub async fn start(
        self: &Arc<Self>,
        sn: &str,
        firmware_id: &str,
        actor: &str,
    ) -> Result<String, OtaError> {
        let handle = self.registry.get(sn).await.ok_or(OtaError::Offline)?;
        if handle.is_busy() {
            return Err(OtaError::Busy);
        }
        let fw = self
            .store
            .firmware_by_id(firmware_id)
            .await
            .map_err(|e| OtaError::Store(e.to_string()))?
            .ok_or(OtaError::FirmwareMissing)?;
        let path: PathBuf = self.cfg.firmware_dir.join(format!("{}.bin", fw.id));
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| OtaError::FirmwareMissing)?;
        if bytes.is_empty() {
            return Err(OtaError::FirmwareMissing);
        }
        // 文档规定 S 命令的 Size / Number 均为 2 字节，升级包最大 64 KiB
        if bytes.len() > codec::OTA_MAX_SIZE as usize {
            return Err(OtaError::TooLarge(codec::OTA_MAX_SIZE as usize));
        }
        let packets = bytes.len().div_ceil(codec::OTA_CHUNK_SIZE);
        if packets > u16::MAX as usize {
            return Err(OtaError::TooLarge(u16::MAX as usize));
        }
        let packets = packets as u16;

        let job_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();
        let progress = JobProgress {
            id: job_id.clone(),
            sn: sn.to_string(),
            firmware_id: fw.id.clone(),
            firmware_version: fw.version.clone(),
            state: "running".into(),
            total_packets: packets,
            sent_packets: 0,
            retries: 0,
            percent: 0,
            message: "等待设备应答 S 命令".into(),
            started_at_ms: now,
            finished_at_ms: None,
        };
        self.jobs.insert(job_id.clone(), progress.clone());
        self.store.create_job(NewJob {
            id: job_id.clone(),
            sn: sn.to_string(),
            firmware_id: fw.id.clone(),
            state: "running".into(),
            total_packets: packets as i64,
            actor: actor.to_string(),
            message: progress.message.clone(),
        });
        self.store.audit(
            actor,
            "ota.start",
            sn,
            &format!("job={job_id} firmware={} v{}", fw.filename, fw.version),
        );

        let (ack_tx, ack_rx) = mpsc::channel::<u8>(64);
        self.acks.insert(sn.to_string(), ack_tx);
        let cancel = Arc::new(AtomicBool::new(false));
        // key 使用 sn 前缀，便于按设备取消
        self.cancels
            .insert(format!("{sn}:{job_id}"), cancel.clone());
        handle.set_busy(true);
        self.metrics.ota_started.fetch_add(1, Ordering::Relaxed);

        let this = self.clone();
        let sn_owned = sn.to_string();
        tokio::spawn(async move {
            this.run(job_id, sn_owned, fw, bytes, packets, ack_rx, cancel)
                .await;
        });

        Ok(progress.id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        self: Arc<Self>,
        job_id: String,
        sn: String,
        fw: FirmwareRow,
        bytes: Vec<u8>,
        packets: u16,
        mut ack_rx: mpsc::Receiver<u8>,
        cancel: Arc<AtomicBool>,
    ) {
        let result = self
            .flow(&job_id, &sn, &bytes, packets, &mut ack_rx, &cancel)
            .await;

        let (state, message, success) = match &result {
            Ok(()) => ("success", format!("升级完成，{} 包全部确认", packets), true),
            Err(OtaError::Cancelled) => ("aborted", "设备离线，升级中止".to_string(), false),
            Err(e) => ("failed", e.to_string(), false),
        };

        if success {
            self.metrics.ota_succeeded.fetch_add(1, Ordering::Relaxed);
            self.store.update_firmware(&sn, &fw.version);
            self.store.audit(
                "system",
                "ota.success",
                &sn,
                &format!("job={job_id} version={}", fw.version),
            );
        } else {
            self.metrics.ota_failed.fetch_add(1, Ordering::Relaxed);
            self.store.audit(
                "system",
                "ota.fail",
                &sn,
                &format!("job={job_id} reason={message}"),
            );
        }
        tracing::info!(job = %job_id, sn = %sn, state, message = %message, "OTA 结束");

        if let Some(mut p) = self.jobs.get_mut(&job_id) {
            p.state = state.to_string();
            p.message = message.clone();
            p.percent = if success { 100 } else { p.percent };
            p.finished_at_ms = Some(Utc::now().timestamp_millis());
            let p = p.clone();
            self.store.update_job(
                &job_id,
                &p.state,
                p.sent_packets as i64,
                p.retries as i64,
                &p.message,
                true,
            );
        }
        self.acks.remove(&sn);
        self.cancels.remove(&format!("{sn}:{job_id}"));
        if let Some(h) = self.registry.get(&sn).await {
            h.set_busy(false);
        }
    }

    async fn flow(
        &self,
        job_id: &str,
        sn: &str,
        bytes: &[u8],
        packets: u16,
        ack_rx: &mut mpsc::Receiver<u8>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let max_restarts = self.settings.u32("ota_max_restarts", self.cfg.ota_max_restarts);
        let mut restarts = 0u32;
        loop {
            self.begin_phase(job_id, sn, bytes.len() as u16, packets, ack_rx, cancel)
                .await?;

            let mut restart = false;
            for frame_id in 0..packets {
                if cancel.load(Ordering::SeqCst) {
                    return Err(OtaError::Cancelled);
                }
                let start = frame_id as usize * codec::OTA_CHUNK_SIZE;
                let end = (start + codec::OTA_CHUNK_SIZE).min(bytes.len());
                match self
                    .send_packet(job_id, sn, frame_id, &bytes[start..end], packets, ack_rx, cancel)
                    .await
                {
                    Ok(()) => {}
                    Err(OtaError::FlashFail(_)) => {
                        restart = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }

            if restart {
                restarts += 1;
                if restarts > max_restarts {
                    return Err(OtaError::FlashFail(restarts));
                }
                tracing::warn!(sn = %sn, round = restarts, "UPS 报告 Flash 写入失败，整轮重来");
                self.patch_progress(job_id, |p| {
                    p.state = "restarting".into();
                    p.sent_packets = 0;
                    p.percent = 0;
                    p.message = format!("Flash 写入失败，第 {restarts} 轮重试");
                });
                continue;
            }
            break;
        }

        // 结束帧：0x30 写入成功；0x31 校验错重发；0x32 写 Flash 失败需整轮重来
        let end_frame = Frame::new(codec::DEFAULT_ADDRESS, cmd::OTA_END, codec::ota_end_payload());
        let mut confirmed = false;
        for _ in 0..3 {
            drain(ack_rx);
            self.send(sn, end_frame.clone()).await?;
            match timeout(Duration::from_secs(3), ack_rx.recv()).await {
                Ok(Some(code)) if code == ack::CONTINUE => {
                    confirmed = true;
                    break;
                }
                Ok(Some(code)) if code == ack::FLASH_FAIL => {
                    return Err(OtaError::FlashFail(0));
                }
                Ok(Some(code)) => {
                    tracing::warn!(sn = %sn, ack = codec::ack_name(code), "结束帧应答异常，重发 F");
                }
                _ => {}
            }
        }
        if !confirmed {
            return Err(OtaError::Timeout);
        }
        self.patch_progress(job_id, |p| {
            p.sent_packets = packets;
            p.percent = 100;
            p.message = "设备已确认升级完成".into();
        });
        Ok(())
    }

    async fn begin_phase(
        &self,
        job_id: &str,
        sn: &str,
        size: u16,
        packets: u16,
        ack_rx: &mut mpsc::Receiver<u8>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let payload = codec::ota_begin_payload(size, packets);
        let frame = Frame::new(codec::DEFAULT_ADDRESS, cmd::OTA_BEGIN, payload);
        let deadline = Instant::now() + Duration::from_millis(self.settings.u64("ota_begin_window_ms", self.cfg.ota_begin_window_ms));
        let interval = Duration::from_millis(self.settings.u64("ota_begin_interval_ms", self.cfg.ota_begin_interval_ms).max(10));
        self.patch_progress(job_id, |p| {
            p.message = "等待设备应答 S 命令".into();
        });
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(OtaError::Cancelled);
            }
            drain(ack_rx);
            self.send(sn, frame.clone()).await?;
            if let Some(code) = wait(ack_rx, interval).await {
                match code {
                    ack::CONTINUE => return Ok(()),
                    ack::RETRY => continue,
                    ack::FLASH_FAIL => return Err(OtaError::FlashFail(0)),
                    other => return Err(OtaError::Rejected(other)),
                }
            }
            if Instant::now() >= deadline {
                return Err(OtaError::Timeout);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_packet(
        &self,
        job_id: &str,
        sn: &str,
        frame_id: u16,
        chunk: &[u8],
        total: u16,
        ack_rx: &mut mpsc::Receiver<u8>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let payload = codec::ota_data_payload(frame_id, chunk);
        let frame = Frame::new(codec::DEFAULT_ADDRESS, cmd::OTA_DATA, payload);
        let mut attempt = 0u32;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(OtaError::Cancelled);
            }
            drain(ack_rx);
            self.send(sn, frame.clone()).await?;
            match wait(ack_rx, Duration::from_millis(self.settings.u64("ota_packet_timeout_ms", self.cfg.ota_packet_timeout_ms))).await {
                Some(code) if code == ack::CONTINUE => {
                    let sent = frame_id + 1;
                    self.patch_progress(job_id, |p| {
                        p.sent_packets = sent;
                        p.percent = ((sent as u32 * 100) / total.max(1) as u32) as u8;
                        p.message = format!("已确认 {sent}/{total} 包");
                    });
                    if sent.is_multiple_of(8) || sent == total {
                        self.persist_progress(job_id);
                    }
                    return Ok(());
                }
                Some(code) if code == ack::RETRY => {
                    attempt += 1;
                    self.patch_progress(job_id, |p| {
                        p.retries += 1;
                        p.message = format!("第 {frame_id} 包校验错，第 {attempt} 次重发");
                    });
                    if attempt > self.settings.u32("ota_max_retries", self.cfg.ota_max_retries) {
                        return Err(OtaError::Rejected(ack::RETRY));
                    }
                }
                Some(code) if code == ack::FLASH_FAIL => {
                    return Err(OtaError::FlashFail(0));
                }
                Some(other) => return Err(OtaError::Rejected(other)),
                None => {
                    attempt += 1;
                    self.patch_progress(job_id, |p| {
                        p.retries += 1;
                        p.message = format!("第 {frame_id} 包应答超时，第 {attempt} 次重发");
                    });
                    if attempt > self.settings.u32("ota_max_retries", self.cfg.ota_max_retries) {
                        return Err(OtaError::Timeout);
                    }
                }
            }
        }
    }

    async fn send(&self, sn: &str, frame: Frame) -> Result<(), OtaError> {
        // force = true：OTA 自身不受 busy 隔离限制
        self.registry.send(sn, frame, true).await?;
        Ok(())
    }

    fn patch_progress(&self, job_id: &str, f: impl FnOnce(&mut JobProgress)) {
        if let Some(mut p) = self.jobs.get_mut(job_id) {
            f(&mut p);
        }
    }

    fn persist_progress(&self, job_id: &str) {
        if let Some(p) = self.jobs.get(job_id) {
            self.store.update_job(
                job_id,
                &p.state,
                p.sent_packets as i64,
                p.retries as i64,
                &p.message,
                false,
            );
        }
    }
}

fn drain(rx: &mut mpsc::Receiver<u8>) {
    while rx.try_recv().is_ok() {}
}

async fn wait(rx: &mut mpsc::Receiver<u8>, dur: Duration) -> Option<u8> {
    match timeout(dur, rx.recv()).await {
        Ok(Some(code)) => Some(code),
        _ => None,
    }
}
