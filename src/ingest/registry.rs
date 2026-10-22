//! 在线设备注册表。
//!
//! 每条 TCP 连接对应一个 [`DeviceHandle`]；下行发送统一走 mpsc 队列 + 单写任务，
//! 天然保证帧的原子性（不会出现两个任务同时写同一个 socket 造成字节交错）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use chrono::Utc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::protocol::{Frame, ParserStats};

#[derive(Debug)]
pub struct DeviceHandle {
    pub conn_id: Uuid,
    /// 设备 SN 可变：设备首帧 `a` 上报真实 SN 后，占位身份会被替换
    pub sn: RwLock<String>,
    pub mdvr_id: String,
    pub remote: SocketAddr,
    pub connected_at_ms: i64,
    pub tx: mpsc::Sender<Vec<u8>>,
    last_seen_ms: AtomicI64,
    /// OTA 期间置位，普通轮询/配置下发被拒绝（升级隔离）
    busy: AtomicBool,
    pub parser_stats: std::sync::Mutex<ParserStats>,
}

impl DeviceHandle {
    pub fn new(
        conn_id: Uuid,
        sn: String,
        mdvr_id: String,
        remote: SocketAddr,
        tx: mpsc::Sender<Vec<u8>>,
        connected_at_ms: i64,
    ) -> Self {
        Self {
            conn_id,
            sn: RwLock::new(sn),
            mdvr_id,
            remote,
            connected_at_ms,
            tx,
            last_seen_ms: AtomicI64::new(connected_at_ms),
            busy: AtomicBool::new(false),
            parser_stats: std::sync::Mutex::new(ParserStats::default()),
        }
    }

    pub async fn sn(&self) -> String {
        self.sn.read().await.clone()
    }

    pub async fn set_sn(&self, sn: String) {
        *self.sn.write().await = sn;
    }

    pub fn last_seen_ms(&self) -> i64 {
        self.last_seen_ms.load(Ordering::Relaxed)
    }

    pub fn touch(&self) {
        self.last_seen_ms.store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
    }

    pub fn uptime_secs(&self) -> i64 {
        (Utc::now().timestamp_millis() - self.connected_at_ms) / 1000
    }
}

#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub sn: String,
    pub mdvr_id: String,
    pub remote: SocketAddr,
    pub connected_at_ms: i64,
    pub last_seen_ms: i64,
    pub uptime_secs: i64,
    pub busy: bool,
    pub parser_stats: ParserStats,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("设备 {0} 不在线")]
    Offline(String),
    #[error("设备 {0} 正在 OTA 升级，已隔离其他下行命令")]
    Busy(String),
    #[error("设备 {0} 写队列已关闭")]
    Closed(String),
}

#[derive(Default)]
pub struct Registry {
    devices: RwLock<HashMap<String, Arc<DeviceHandle>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, handle: Arc<DeviceHandle>) {
        let mut map = self.devices.write().await;
        let key = handle.sn().await;
        if let Some(old) = map.insert(key.clone(), handle.clone()) {
            tracing::warn!(
                sn = %key,
                old_conn = %old.conn_id,
                new_conn = %handle.conn_id,
                "同一 UPS 重复接入，旧连接将被替换"
            );
        }
    }

    /// 设备上报真实 SN 后把占位身份换成正式 SN。
    /// 仅当 `old` 上的连接仍是 `conn_id` 且 `new` 未被占用时才允许。
    pub async fn rebind(&self, old_sn: &str, new_sn: &str, conn_id: Uuid) -> bool {
        if old_sn == new_sn {
            return true;
        }
        let mut map = self.devices.write().await;
        let Some(handle) = map.get(old_sn).cloned() else {
            return false;
        };
        if handle.conn_id != conn_id {
            return false;
        }
        if map.contains_key(new_sn) {
            tracing::warn!(
                old = %old_sn,
                new = %new_sn,
                "目标 SN 已被其他连接占用，放弃重绑"
            );
            return false;
        }
        handle.set_sn(new_sn.to_string()).await;
        map.remove(old_sn);
        map.insert(new_sn.to_string(), handle);
        true
    }

    /// 仅当 conn_id 匹配时才移除，避免旧连接的清理任务误删新连接。
    pub async fn unregister(&self, sn: &str, conn_id: Uuid) -> bool {
        let mut map = self.devices.write().await;
        if map.get(sn).map(|h| h.conn_id) == Some(conn_id) {
            map.remove(sn);
            true
        } else {
            false
        }
    }

    pub async fn get(&self, sn: &str) -> Option<Arc<DeviceHandle>> {
        self.devices.read().await.get(sn).cloned()
    }

    pub async fn list(&self) -> Vec<DeviceSnapshot> {
        let map = self.devices.read().await;
        let mut out = Vec::with_capacity(map.len());
        for h in map.values() {
            out.push(DeviceSnapshot {
                sn: h.sn().await,
                mdvr_id: h.mdvr_id.clone(),
                remote: h.remote,
                connected_at_ms: h.connected_at_ms,
                last_seen_ms: h.last_seen_ms(),
                uptime_secs: h.uptime_secs(),
                busy: h.is_busy(),
                parser_stats: *h.parser_stats.lock().unwrap(),
            });
        }
        out.sort_by(|a, b| a.sn.cmp(&b.sn));
        out
    }

    pub async fn online_count(&self) -> usize {
        self.devices.read().await.len()
    }

    /// 下行发送。`force` 用于 OTA 自身，可绕过 busy 隔离。
    pub async fn send(&self, sn: &str, frame: Frame, force: bool) -> Result<(), SendError> {
        let handle = self
            .get(sn)
            .await
            .ok_or_else(|| SendError::Offline(sn.to_string()))?;
        if handle.is_busy() && !force {
            return Err(SendError::Busy(sn.to_string()));
        }
        handle
            .tx
            .send(frame.encode())
            .await
            .map_err(|_| SendError::Closed(sn.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handle(sn: &str) -> Arc<DeviceHandle> {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        Arc::new(DeviceHandle::new(
            Uuid::new_v4(),
            sn.to_string(),
            format!("MDVR-{sn}"),
            "127.0.0.1:5000".parse().unwrap(),
            tx,
            Utc::now().timestamp_millis(),
        ))
    }

    #[tokio::test]
    async fn rebind_moves_handle_to_real_sn() {
        let reg = Registry::new();
        let h = make_handle("UPS-192.168.1.169");
        let conn = h.conn_id;
        reg.register(h.clone()).await;
        assert!(reg.get("UPS-192.168.1.169").await.is_some());

        assert!(reg.rebind("UPS-192.168.1.169", "UPS-9001", conn).await);
        assert!(reg.get("UPS-192.168.1.169").await.is_none());
        let moved = reg.get("UPS-9001").await.unwrap();
        assert_eq!(moved.conn_id, conn);
        assert_eq!(moved.sn().await, "UPS-9001");
        assert_eq!(reg.online_count().await, 1);

        // 重绑后旧 key 的清理必须按新 key 走，不能误删
        assert!(reg.unregister("UPS-9001", conn).await);
        assert_eq!(reg.online_count().await, 0);
    }

    #[tokio::test]
    async fn rebind_rejects_wrong_conn_and_occupied_target() {
        let reg = Registry::new();
        let h = make_handle("UPS-192.168.1.169");
        let conn = h.conn_id;
        reg.register(h).await;

        // 别的连接无权替它重绑
        assert!(!reg.rebind("UPS-192.168.1.169", "UPS-9001", Uuid::new_v4()).await);
        assert!(reg.get("UPS-192.168.1.169").await.is_some());

        // 目标 SN 已被占用时放弃
        let other = make_handle("UPS-9001");
        reg.register(other).await;
        assert!(!reg.rebind("UPS-192.168.1.169", "UPS-9001", conn).await);
        assert_eq!(reg.online_count().await, 2);
    }

    #[tokio::test]
    async fn list_reflects_rebound_sn() {
        let reg = Registry::new();
        let h = make_handle("UPS-10.0.0.2");
        let conn = h.conn_id;
        reg.register(h).await;
        reg.rebind("UPS-10.0.0.2", "UPS-7777", conn).await;
        let names: Vec<String> = reg.list().await.into_iter().map(|d| d.sn).collect();
        assert_eq!(names, vec!["UPS-7777".to_string()]);
    }
}
