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
//!
//! # 与 MDVR 参考实现（`Serial_Ups.cpp`）的取舍
//!
//! 见 `docs/protocol.md` 第 9 节「与 MDVR 参考实现的差异」。要点：
//!   * 参考实现把 `S`/`F` 的载荷发成命令字母本身、`D` 不带 `FrameID`，且把首尾
//!     `0x7E` 也一起转义——这些都与协议文档、与实机抓包矛盾，**没有照抄**；
//!   * 参考实现的升级应答解析不校验帧头帧尾和校验和，且把 `D` 应答的第 2 字节
//!     （若真带 FrameID，那是 FrameID 高字节）当成 ack，**已避免**；
//!   * 参考实现有价值的点已吸收：`D` 应答按 `FrameID` 核对（不再来者不拒）、
//!     `F` 的 0x32 走整轮重来、`F` 超时放宽到 5s。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::timeout;

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

/// 设备回给平台的升级应答。
///
/// `frame_id` 只有 `D` 的应答带（协议：`D + FrameID + ack`），`S`/`F` 的应答是
/// 单字节 ack，所以是 `None`。核对 FrameID 是必须的：4G 链路会丢包和重传，
/// 上一包的迟到应答如果不核对，会被当成当前包的确认，静默跳过一个包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub frame_id: Option<u16>,
    pub code: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobProgress {    pub id: String,
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
    acks: DashMap<String, mpsc::Sender<Ack>>,
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

    /// 接入层收到 ACK 帧后调用。`frame_id` 仅 `D` 命令的应答带（`S`/`F` 为 `None`）。
    pub fn deliver_ack(&self, sn: &str, frame_id: Option<u16>, code: u8) {
        if let Some(tx) = self.acks.get(sn) {
            let _ = tx.try_send(Ack { frame_id, code });
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
        // 设备可能被 `d` 命令改过串口地址；S/D/F 三个阶段帧都要用当前地址，
        // 否则设备一律收不到，升级会卡在「等待设备应答 S 命令」。
        let address = self
            .store
            .device(sn)
            .await
            .ok()
            .flatten()
            .map(|d| d.address.clamp(0, 247) as u8)
            .unwrap_or(codec::DEFAULT_ADDRESS);
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

        let (ack_tx, ack_rx) = mpsc::channel::<Ack>(64);
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
            this.run(job_id, sn_owned, fw, bytes, packets, address, ack_rx, cancel)
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
        address: u8,
        mut ack_rx: mpsc::Receiver<Ack>,
        cancel: Arc<AtomicBool>,
    ) {
        let result = self
            .flow(&job_id, &sn, &bytes, packets, address, &mut ack_rx, &cancel)
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

    #[allow(clippy::too_many_arguments)]
    async fn flow(
        &self,
        job_id: &str,
        sn: &str,
        bytes: &[u8],
        packets: u16,
        address: u8,
        ack_rx: &mut mpsc::Receiver<Ack>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let max_restarts = self.settings.u32("ota_max_restarts", self.cfg.ota_max_restarts);
        let mut restarts = 0u32;
        loop {
            self.begin_phase(job_id, sn, bytes.len() as u16, packets, address, ack_rx, cancel)
                .await?;

            let mut restart = false;
            for frame_id in 0..packets {
                if cancel.load(Ordering::SeqCst) {
                    return Err(OtaError::Cancelled);
                }
                let start = frame_id as usize * codec::OTA_CHUNK_SIZE;
                let end = (start + codec::OTA_CHUNK_SIZE).min(bytes.len());
                match self
                    .send_packet(
                        job_id, sn, frame_id, &bytes[start..end], packets, address, ack_rx, cancel,
                    )
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
                if !self.bump_restart(job_id, sn, &mut restarts, max_restarts) {
                    return Err(OtaError::FlashFail(restarts));
                }
                continue;
            }

            // 结束帧在重试环内：`F` 报 0x32 说明最后这次写 Flash 也失败了，
            // 按协议同样要整轮重来，不能直接把已经传完的任务判失败。
            match self
                .end_phase(job_id, sn, address, ack_rx, cancel)
                .await
            {
                Ok(()) => break,
                Err(OtaError::FlashFail(_)) => {
                    if !self.bump_restart(job_id, sn, &mut restarts, max_restarts) {
                        return Err(OtaError::FlashFail(restarts));
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        self.patch_progress(job_id, |p| {
            p.sent_packets = packets;
            p.percent = 100;
            p.message = "设备已确认升级完成".into();
        });
        Ok(())
    }

    /// 记一次「整轮重来」。返回 false 表示已超过上限，调用方应结束任务。
    fn bump_restart(&self, job_id: &str, sn: &str, restarts: &mut u32, max: u32) -> bool {
        *restarts += 1;
        if *restarts > max {
            return false;
        }
        tracing::warn!(sn = %sn, round = *restarts, "UPS 报告 Flash 写入失败，整轮重来");
        self.patch_progress(job_id, |p| {
            p.state = "restarting".into();
            p.sent_packets = 0;
            p.percent = 0;
            p.message = format!("Flash 写入失败，第 {} 轮重试", *restarts);
        });
        true
    }

    /// 结束帧 `F`：0x30 判定成功；0x31 重发；0x32 交由上层整轮重来。
    ///
    /// 超时按 `ota_end_timeout_ms`（默认 5s）计——`F` 会让设备校验整包并写 Flash，
    /// 明显慢于普通数据包；MDVR 参考实现同样给的是 5s。
    async fn end_phase(
        &self,
        job_id: &str,
        sn: &str,
        address: u8,
        ack_rx: &mut mpsc::Receiver<Ack>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let end_frame = Frame::new(address, cmd::OTA_END, codec::ota_end_payload());
        let wait = Duration::from_millis(
            self.settings
                .u64("ota_end_timeout_ms", self.cfg.ota_end_timeout_ms),
        );
        self.patch_progress(job_id, |p| {
            p.message = "等待设备确认结束帧".into();
        });
        for attempt in 1..=3 {
            if cancel.load(Ordering::SeqCst) {
                return Err(OtaError::Cancelled);
            }
            drain(ack_rx);
            self.send(sn, end_frame.clone()).await?;
            match wait_ack(ack_rx, wait, None).await {
                Some(ack::CONTINUE) => return Ok(()),
                Some(ack::FLASH_FAIL) => return Err(OtaError::FlashFail(0)),
                Some(other) => {
                    tracing::warn!(
                        sn = %sn,
                        attempt,
                        ack = codec::ack_name(other),
                        "结束帧应答异常，重发 F"
                    );
                }
                None => {
                    tracing::warn!(sn = %sn, attempt, "结束帧应答超时，重发 F");
                }
            }
        }
        Err(OtaError::Timeout)
    }

    #[allow(clippy::too_many_arguments)]
    async fn begin_phase(
        &self,
        job_id: &str,
        sn: &str,
        size: u16,
        packets: u16,
        address: u8,
        ack_rx: &mut mpsc::Receiver<Ack>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let payload = codec::ota_begin_payload(size, packets);
        let frame = Frame::new(address, cmd::OTA_BEGIN, payload);
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
            if let Some(code) = wait_ack(ack_rx, interval, None).await {
                match code {
                    ack::CONTINUE => return Ok(()),
                    ack::RETRY => continue,
                    // S 阶段一个字节都还没发，"整轮重来"没有意义；按 0x31 一样继续
                    // 请求 S，由上面的窗口 deadline 兜底（与 MDVR「重试 S 后失败」一致）。
                    ack::FLASH_FAIL => {
                        tracing::warn!(sn = %sn, "开始帧应答 0x32（写 Flash 失败），继续重试 S");
                    }
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
        address: u8,
        ack_rx: &mut mpsc::Receiver<Ack>,
        cancel: &AtomicBool,
    ) -> Result<(), OtaError> {
        let payload = codec::ota_data_payload(frame_id, chunk);
        let frame = Frame::new(address, cmd::OTA_DATA, payload);
        let packet_timeout = Duration::from_millis(self.settings.u64("ota_packet_timeout_ms", self.cfg.ota_packet_timeout_ms));
        let mut attempt = 0u32;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(OtaError::Cancelled);
            }
            drain(ack_rx);
            self.send(sn, frame.clone()).await?;
            // 只认当前 FrameID 的应答：上一包的迟到/重复帧直接丢弃，不当成确认
            match wait_ack(ack_rx, packet_timeout, Some(frame_id)).await {
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

fn drain(rx: &mut mpsc::Receiver<Ack>) {
    while rx.try_recv().is_ok() {}
}

/// 等一个**属于当前阶段**的应答。
///
/// `expect` 是期望的 `FrameID`：数据包阶段传 `Some(id)`，`S`/`F` 阶段传 `None`。
/// 不匹配的应答（上一包的迟到帧、重复帧）直接丢弃继续等，绝不当成当前包的确认——
/// 4G 链路会丢包重传，把旧应答当成新确认会静默跳过一个数据包，而这种损坏要等到
/// 设备校验整包失败才会暴露，很难查。
async fn wait_ack(
    rx: &mut mpsc::Receiver<Ack>,
    dur: Duration,
    expect: Option<u16>,
) -> Option<u8> {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match timeout(left, rx.recv()).await {
            Ok(Some(a)) if a.frame_id == expect => return Some(a.code),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(frame_id: Option<u16>, code: u8) -> Ack {
        Ack { frame_id, code }
    }

    /// 迟到/重复的旧包应答不能被当成当前包的确认。
    /// 4G 链路会重传，若来者不拒，会静默跳过一个数据包。
    #[tokio::test]
    async fn stale_data_ack_is_not_accepted_as_confirmation() {
        let (tx, mut rx) = mpsc::channel::<Ack>(8);
        tx.send(ack(Some(6), ack::CONTINUE)).await.unwrap();
        tx.send(ack(Some(7), ack::CONTINUE)).await.unwrap();

        // 等第 8 包：前两帧都不该被消费成确认
        let got = wait_ack(&mut rx, Duration::from_millis(50), Some(8)).await;
        assert_eq!(got, None, "旧包应答不应被当成第 8 包的确认");
    }

    /// 匹配的 FrameID 才返回 ack，其余照丢。
    #[tokio::test]
    async fn matching_frame_id_returns_ack() {
        let (tx, mut rx) = mpsc::channel::<Ack>(8);
        tx.send(ack(Some(6), ack::CONTINUE)).await.unwrap();
        tx.send(ack(Some(8), ack::RETRY)).await.unwrap();

        let got = wait_ack(&mut rx, Duration::from_millis(200), Some(8)).await;
        assert_eq!(got, Some(ack::RETRY));
    }

    /// `S`/`F` 阶段没有 FrameID，`None` 应答应被接受；带 FrameID 的（异常）则忽略。
    #[tokio::test]
    async fn begin_and_end_phases_accept_id_less_acks() {
        let (tx, mut rx) = mpsc::channel::<Ack>(8);
        tx.send(ack(None, ack::CONTINUE)).await.unwrap();
        assert_eq!(
            wait_ack(&mut rx, Duration::from_millis(100), None).await,
            Some(ack::CONTINUE)
        );

        let (tx, mut rx) = mpsc::channel::<Ack>(8);
        tx.send(ack(Some(3), ack::CONTINUE)).await.unwrap();
        assert_eq!(
            wait_ack(&mut rx, Duration::from_millis(50), None).await,
            None,
            "带 FrameID 的应答不该冒充 S/F 的确认"
        );
    }

    /// OTA 载荷编解码：D 应答按 `FrameID + ack` 解析，与 MDVR 参考实现
    /// （把第 2 字节当 ack）不同——真带 FrameID 时它读到的是 FrameID 高字节。
    #[test]
    fn data_ack_layout_is_frame_id_then_ack() {
        assert_eq!(
            codec::parse_data_ack(&[0x00, 0x08, ack::CONTINUE]),
            Some((8, ack::CONTINUE))
        );
        assert_eq!(codec::parse_data_ack(&[ack::CONTINUE]), None);
        // 载荷太短（少于 FrameID+ack 三字节）一律判非法
        assert_eq!(codec::parse_data_ack(&[0x00, 0x08]), None);
    }
}
