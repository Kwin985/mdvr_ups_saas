//! 命令应答总线：把设备的上行应答帧投递给正在等待的 HTTP 请求。
//!
//! 命令是异步下发的（`registry.send` 只是把字节写进 TCP），设备的应答走的是
//! 接入线程。HTTP 处理函数要「等设备回一句再答复页面」，就需要一条从 ingest
//! 到 web 的单向通道。按 SN 建 key，并记录期望的命令字——设备每个上报周期都
//! 会发 `a` 状态帧，不筛命令字的话等待者会立刻拿到无关帧。

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::protocol::Frame;

struct Waiter {
    /// 只接受这个命令字的帧。注意请求字与应答字可能不同：
    /// 查询版本发 `r`、设备回 `V`，所以不能拿请求的 `frame.command` 来匹配。
    expect: u8,
    tx: mpsc::Sender<Frame>,
}

/// 见模块注释。
#[derive(Default)]
pub struct CommandBus {
    waiters: Mutex<HashMap<String, Waiter>>,
}

impl CommandBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一次等待，返回的 Receiver 会在 `expect` 命令帧到达时收到它。
    /// 同一设备重复注册会覆盖旧等待者（并发命令极少，新请求优先）。
    pub fn register(&self, sn: &str, expect: u8) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(4);
        let mut map = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(sn.to_string(), Waiter { expect, tx });
        rx
    }

    /// 请求结束（收到应答 / 超时 / 发送失败）后必须调用，别把等待者留在表里。
    pub fn unregister(&self, sn: &str) {
        let mut map = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(sn);
    }

    /// 上行帧到达时调用。命令字不匹配就丢弃。
    pub fn deliver(&self, sn: &str, frame: &Frame) {
        let map = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = map.get(sn) else { return };
        if w.expect == frame.command {
            // try_send：通道满或接收端已放弃时直接忽略，绝不能阻塞接入线程
            let _ = w.tx.try_send(frame.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_only_matching_command() {
        let bus = CommandBus::new();
        let mut rx = bus.register("SN-1", b'V');
        // 周期性的状态帧不能抢答
        bus.deliver("SN-1", &Frame::new(0, b'a', vec![0u8; 4]));
        assert!(rx.try_recv().is_err(), "不匹配的命令字不应投递");
        // 版本帧应投递
        bus.deliver("SN-1", &Frame::new(0, b'V', b"V26052602".to_vec()));
        let got = rx.try_recv().expect("应收到版本帧");
        assert_eq!(got.command, b'V');
        assert_eq!(got.payload, b"V26052602");
        bus.unregister("SN-1");
    }

    #[test]
    fn unknown_sn_is_ignored() {
        let bus = CommandBus::new();
        // 没有等待者时投递不能 panic，也不能阻塞
        bus.deliver("SN-X", &Frame::new(0, b'V', b"V1".to_vec()));
    }

    #[test]
    fn unregister_drops_waiter() {
        let bus = CommandBus::new();
        let mut rx = bus.register("SN-2", b'd');
        bus.unregister("SN-2");
        bus.deliver("SN-2", &Frame::new(0, b'd', vec![0x30]));
        assert!(rx.try_recv().is_err(), "已注销的等待者不应再收到帧");
    }
}
