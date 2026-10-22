//! 进程内指标（供 /metrics 与首页展示）。
//!
//! 生产环境可替换为 `metrics` + Prometheus exporter；这里用原子计数保持零依赖。

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Metrics {
    pub active_connections: AtomicI64,
    pub frames_total: AtomicU64,
    pub checksum_errors: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub samples_ingested: AtomicU64,
    pub ota_started: AtomicU64,
    pub ota_succeeded: AtomicU64,
    pub ota_failed: AtomicU64,
}

impl Metrics {
    pub fn conn_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn conn_closed(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            active_connections: self.active_connections.load(Ordering::Relaxed),
            frames_total: self.frames_total.load(Ordering::Relaxed),
            checksum_errors: self.checksum_errors.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            samples_ingested: self.samples_ingested.load(Ordering::Relaxed),
            ota_started: self.ota_started.load(Ordering::Relaxed),
            ota_succeeded: self.ota_succeeded.load(Ordering::Relaxed),
            ota_failed: self.ota_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MetricsSnapshot {
    pub active_connections: i64,
    pub frames_total: u64,
    pub checksum_errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub samples_ingested: u64,
    pub ota_started: u64,
    pub ota_succeeded: u64,
    pub ota_failed: u64,
}
