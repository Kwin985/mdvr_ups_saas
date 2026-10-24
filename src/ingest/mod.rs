//! 透明流接入层。
//!
//! 每个 MDVR TCP 连接：
//!   1. 先做一次**可选**注册探测（JSON 行 / 竖线 / 二进制注册帧）。
//!      真实 MDVR 透传设备不发注册，窗口结束即按 `UPS-<IP>` 回退身份放行，
//!      之后首个 `a` 帧里上报真实 SN 时自动重绑（含历史样本迁移）；
//!   2. 随后进入纯字节流模式，所有帧语义交给 [`FrameParser`]；
//!   3. 下行统一投递到写任务队列，写任务串行发送并保证 >= 10ms 的帧间隔。

pub mod registry;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use uuid::Uuid;

use crate::config::Config;
use crate::metrics::Metrics;
use crate::settings::SettingsStore;
use crate::ota::OtaManager;
use crate::protocol::{Frame, FrameParser, ParseEvent, StatusA, Version, cmd, codec};
use crate::storage::{DeviceMeta, Sample, Store};

use registry::{DeviceHandle, Registry};

#[derive(Clone)]
pub struct Ingest {
    pub registry: Arc<Registry>,
    pub store: Store,
    pub cfg: Arc<Config>,
    pub ota: Arc<OtaManager>,
    pub metrics: Arc<Metrics>,
    /// 系统配置（运行时可变）：探测窗口 / 下行间隔 / 离线判定等
    pub settings: Arc<SettingsStore>,
}

/// 注册握手结果。身份信息都是可选的，按可信度从高到低：
/// * 设备发了注册行/注册帧 → `mdvr_id` + `sn` 都拿到；
/// * 设备直接发业务帧且首帧就是状态帧 → SN 就在载荷里，取真实 SN（缺 `mdvr_id`）；
/// * 窗口内什么都没发 → 全 `None`，用 `MDVR-<IP>` / `UPS-<IP>` 回退身份，
///   之后首个 `a` 帧上报真实 SN 时再重绑。
struct Handshake {
    mdvr_id: Option<String>,
    sn: Option<String>,
    leftover: Vec<u8>,
}

/// 回退身份：`MDVR-<IP>` / `UPS-<IP>`。设备随后在 `a` 帧里上报真实 SN 时会自动重绑。
fn fallback_identity(peer: SocketAddr) -> (String, String) {
    (
        format!("MDVR-{}", peer.ip()),
        format!("UPS-{}", peer.ip()),
    )
}

fn is_fallback_sn(sn: &str) -> bool {
    sn.strip_prefix("UPS-")
        .and_then(|rest| rest.parse::<std::net::Ipv4Addr>().ok())
        .is_some()
}

impl Ingest {
    pub async fn serve(self, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_connection(stream, peer).await {
                            tracing::warn!(%peer, error = %e, "MDVR 连接结束");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "accept 失败");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    async fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) -> Result<()> {
        stream.set_nodelay(true).ok();
        self.metrics.conn_opened();
        let result = self.run_connection(stream, peer).await;
        self.metrics.conn_closed();
        result
    }

    async fn run_connection(&self, mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
        // ---------------- 1. 注册握手（可选探测） ----------------
        // 真实 MDVR 透传设备不会发自定义注册行，探测窗口内没等到就放行：
        // 用回退身份上线，后续 'a' 帧里上报真实 SN 时再自动重绑。
        let probe = Duration::from_millis(self.settings.u64("reg_probe_ms", self.cfg.reg_probe_ms).max(50));
        let hs = probe_registration(&mut stream, probe).await?;
        let (mdvr_id, initial_sn) = match (hs.mdvr_id, hs.sn) {
            (Some(mdvr), Some(sn)) => {
                tracing::info!(%peer, mdvr = %mdvr, sn = %sn, "MDVR 注册成功");
                (mdvr, sn)
            }
            // 首帧就是状态帧：SN 直接从载荷里拿，省掉「占位上线 → 首帧重绑」的整段迁移
            (mdvr, Some(sn)) => {
                let mdvr = mdvr.unwrap_or_else(|| format!("MDVR-{}", peer.ip()));
                tracing::info!(%peer, mdvr = %mdvr, sn = %sn, "首帧携带真实 SN，直接以正式身份上线");
                (mdvr, sn)
            }
            _ => {
                let (mdvr, sn) = fallback_identity(peer);
                tracing::info!(
                    %peer,
                    mdvr = %mdvr,
                    sn = %sn,
                    "MDVR 直连透传（探测窗口内无注册帧），以回退身份上线"
                );
                (mdvr, sn)
            }
        };

        let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
        let conn_id = Uuid::new_v4();
        let now = Utc::now().timestamp_millis();
        let handle = Arc::new(DeviceHandle::new(
            conn_id,
            initial_sn.clone(),
            mdvr_id.clone(),
            peer,
            tx,
            now,
        ));

        self.registry.register(handle.clone()).await;
        self.store.upsert_device(DeviceMeta {
            sn: initial_sn.clone(),
            mdvr_id: mdvr_id.clone(),
            address: codec::DEFAULT_ADDRESS as i64,
            baudrate: 9600,
            report_interval: self.settings.i64("default_report_interval", 30),
            firmware_version: String::new(),
            config_json: "{}".into(),
        });
        self.store.set_online(&initial_sn, true);

        // 注册应答（可选，MDVR 可忽略）
        let ack = Frame::new(0x01, cmd::REGISTER, b"OK".to_vec()).encode();
        stream.write_all(&ack).await.ok();
        self.metrics
            .bytes_out
            .fetch_add(ack.len() as u64, std::sync::atomic::Ordering::Relaxed);

        // ---------------- 2. 读写分离 ----------------
        let (mut reader, mut writer) = stream.into_split();
        let writer_task = {
            let gap = Duration::from_millis(self.settings.u64("downlink_gap_ms", self.cfg.downlink_gap_ms).max(1));
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                let mut rx = rx;
                let mut last: Option<Instant> = None;
                while let Some(bytes) = rx.recv().await {
                    if let Some(prev) = last {
                        let elapsed = prev.elapsed();
                        if elapsed < gap {
                            tokio::time::sleep(gap - elapsed).await;
                        }
                    }
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                    metrics
                        .bytes_out
                        .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    last = Some(Instant::now());
                }
            })
        };

        // 绑定 SN（占位 → 可能在首个 'a' 帧后被替换为真实 SN）
        let mut sn = initial_sn;
        let mut parser = FrameParser::new();
        let mut buf = [0u8; 4096];
        let idle = Duration::from_secs(self.settings.u64("idle_timeout_secs", 180).max(30));

        if !hs.leftover.is_empty() {
            self.handle_bytes(&mut parser, &hs.leftover, &mut sn, conn_id)
                .await;
        }

        loop {
            let n = match timeout(idle, reader.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "读取失败");
                    break;
                }
                Err(_) => {
                    tracing::warn!(sn = %sn, "180s 无数据，主动断开");
                    break;
                }
            };
            self.metrics
                .bytes_in
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            self.handle_bytes(&mut parser, &buf[..n], &mut sn, conn_id)
                .await;
            if let Some(h) = self.registry.get(&sn).await {
                h.touch();
                *h.parser_stats.lock().unwrap() = parser.stats;
            }
        }

        writer_task.abort();
        let removed = self.registry.unregister(&sn, conn_id).await;
        if removed {
            self.store.set_online(&sn, false);
            self.ota.on_device_offline(&sn).await;
        }
        tracing::info!(sn = %sn, "MDVR 连接关闭");
        Ok(())
    }

    async fn handle_bytes(
        &self,
        parser: &mut FrameParser,
        data: &[u8],
        sn: &mut String,
        conn_id: Uuid,
    ) {
        for event in parser.feed(data) {
            match event {
                ParseEvent::Frame(frame) => {
                    self.metrics
                        .frames_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.handle_frame(frame, sn, conn_id).await;
                }
                ParseEvent::Invalid(err) => {
                    self.metrics
                        .checksum_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(sn = %sn, error = %err, "丢弃非法帧");
                }
            }
        }
    }

    async fn handle_frame(&self, frame: Frame, sn: &mut String, conn_id: Uuid) {
        match frame.command {
            cmd::STATUS_A => match StatusA::parse(&frame.payload) {
                Ok(st) => {
                    let ts = Utc::now().timestamp_millis();
                    self.metrics
                        .samples_ingested
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // 设备 SN 未写入时 'a' 帧的 SN 字段是 20 个空格；
                    // 回退身份期间首次拿到真实 SN 就整体重绑（含历史样本），
                    // 正式注册的 SN 不一致时只提示、不擅自改绑定。
                    if !st.sn.is_empty() && st.sn != *sn {
                        if is_fallback_sn(sn)
                            && self.registry.rebind(sn, &st.sn, conn_id).await
                        {
                            self.store.migrate_device_sn(sn, &st.sn);
                            tracing::info!(
                                old = %sn,
                                new = %st.sn,
                                "设备上报真实 SN，占位身份已重绑"
                            );
                            *sn = st.sn.clone();
                        } else {
                            tracing::debug!(
                                sn = %sn,
                                frame_sn = %st.sn,
                                "'a' 帧携带的 SN 与注册 SN 不一致"
                            );
                        }
                    }
                    self.store
                        .insert_samples(vec![Sample::from_status_a(sn, ts, &st, frame.encode())]);
                }
                Err(e) => tracing::warn!(sn = %sn, error = %e, "'a' 状态载荷解析失败"),
            },
            cmd::VERSION => match Version::raw_string(&frame.payload) {
                Some(v) => {
                    tracing::info!(sn = %sn, version = %v, "收到 UPS 版本信息");
                    self.store.update_firmware(sn, &v);
                }
                None => tracing::warn!(
                    sn = %sn,
                    payload = ?frame.payload,
                    "'V' 版本载荷无法解析为文本"
                ),
            },
            // OTA 三个阶段：S / F 的应答是 1 字节 ack，D 的应答是 FrameID + ack
            cmd::OTA_BEGIN | cmd::OTA_END => {
                if let Some(code) = codec::parse_ack(&frame.payload) {
                    tracing::debug!(
                        sn = %sn,
                        cmd = codec::command_name(frame.command),
                        ack = codec::ack_name(code),
                        "升级应答"
                    );
                    self.ota.deliver_ack(sn, code);
                }
            }
            cmd::OTA_DATA => match codec::parse_data_ack(&frame.payload) {
                Some((frame_id, code)) => {
                    tracing::debug!(
                        sn = %sn,
                        frame_id,
                        ack = codec::ack_name(code),
                        "升级数据应答"
                    );
                    self.ota.deliver_ack(sn, code);
                }
                None => tracing::warn!(sn = %sn, payload = ?frame.payload, "'D' 应答载荷非法"),
            },
            // 设置类命令（b/d/f/N）的应答：1 字节 ack，只记录结果
            other if other == cmd::SET_BAUDRATE
                || other == cmd::SET_ADDRESS
                || other == cmd::SET_REPORT_INTERVAL
                || other == cmd::SET_SN =>
            {
                match codec::parse_ack(&frame.payload) {
                    Some(code) => tracing::info!(
                        sn = %sn,
                        cmd = codec::command_name(other),
                        ack = codec::ack_name(code),
                        "设置命令应答"
                    ),
                    None => tracing::warn!(sn = %sn, "'{}' 应答载荷为空", codec::command_name(other)),
                }
            }
            cmd::REGISTER => {
                tracing::debug!(sn = %sn, "重复注册帧，已忽略");
            }
            other => {
                tracing::debug!(
                    sn = %sn,
                    cmd = format_args!("0x{other:02X}"),
                    name = codec::command_name(other),
                    "未处理的上行命令"
                );
            }
        }
    }
}

/// 探测注册信息。支持
///   * JSON 行：`{"mdvr_id":"MDVR-001","ups_sn":"UPS-9001"}\n`
///   * 竖线文本：`MDVR-001|UPS-9001\n`
///   * 二进制注册帧：cmd = 0x80，载荷 `MDVR_ID|UPS_SN`
///
/// 与旧实现的区别：真实 MDVR 透传设备根本不会发注册行，所以这里只做
/// **有限窗口探测**——窗口内等到合法注册就用；遇到普通业务帧（首字节
/// `0x7E` 但命令不是注册）时，若该帧正好是状态帧 `a`，SN 就在载荷里，
/// 直接按真实 SN 上线（省掉后续的占位→重绑迁移）；窗口超时则返回
/// 全 `None`，由调用方用 IP 回退身份。所有已读字节都作为 leftover
/// 交给帧解析器，连接放行不中断。
async fn probe_registration(
    stream: &mut TcpStream,
    probe: Duration,
) -> Result<Handshake> {
    let mut buf = Vec::with_capacity(512);
    let mut tmp = [0u8; 512];
    // 总探测上限 = 窗口 × 4：低速挤字节也不能无限续期
    let deadline = Instant::now() + probe * 4;
    loop {
        if !buf.is_empty() {
            // 文本行优先
            if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                let leftover = buf[pos + 1..].to_vec();
                if let Ok((mdvr_id, sn)) = parse_registration_line(&line) {
                    return Ok(Handshake {
                        mdvr_id: Some(mdvr_id),
                        sn: Some(sn),
                        leftover,
                    });
                }
                // 不像注册行：透传数据，整段留给帧解析器
                return Ok(Handshake {
                    mdvr_id: None,
                    sn: None,
                    leftover: buf,
                });
            }
            // 二进制注册帧 / 业务帧
            if buf[0] == crate::protocol::frame::FLAG
                && let Some(end) = buf[1..]
                    .iter()
                    .position(|b| *b == crate::protocol::frame::FLAG)
                    .map(|p| p + 1)
            {
                let decoded = crate::protocol::frame::unescape(&buf[1..end])
                    .ok()
                    .and_then(|body| Frame::decode_body(&body).ok());
                return Ok(match decoded {
                    Some(f) => {
                        let (mdvr_id, sn) = identity_from_frame(&f);
                        // 注册帧已消费掉；其余情况整段留给帧解析器
                        let leftover = if mdvr_id.is_some() {
                            buf[end + 1..].to_vec()
                        } else {
                            buf
                        };
                        Handshake {
                            mdvr_id,
                            sn,
                            leftover,
                        }
                    }
                    None => Handshake {
                        mdvr_id: None,
                        sn: None,
                        leftover: buf,
                    },
                });
            }
        }
        // 每次读取都受探测窗口约束：设备什么都不发也会在窗口结束后放行
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Handshake {
                mdvr_id: None,
                sn: None,
                leftover: buf,
            });
        }
        let n = match timeout(remaining.min(probe), stream.read(&mut tmp)).await {
            Ok(Ok(0)) => return Err(anyhow!("连接在注册前关闭")),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(anyhow!("读取注册帧失败: {e}")),
            Err(_) => {
                return Ok(Handshake {
                    mdvr_id: None,
                    sn: None,
                    leftover: buf,
                });
            }
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 4096 {
            // 远超注册帧体量：不是注册，整段交给帧解析器处理
            return Ok(Handshake {
                mdvr_id: None,
                sn: None,
                leftover: buf,
            });
        }
    }
}

/// 从首个业务帧推断设备身份：
/// * 注册帧 → `(mdvr_id, sn)` 齐全；
/// * 状态帧 `a` → 载荷里带真实 SN，`mdvr_id` 缺失（由调用方按 IP 补）；
/// * 其它帧 / 载荷解析失败 / SN 为空白 → 全 `None`，走 IP 回退身份。
fn identity_from_frame(f: &Frame) -> (Option<String>, Option<String>) {
    match f.command {
        cmd::REGISTER => match codec::parse_registration(&f.payload) {
            Ok((mdvr_id, sn)) => (Some(mdvr_id), Some(sn)),
            Err(_) => (None, None),
        },
        cmd::STATUS_A => {
            let sn = StatusA::parse(&f.payload)
                .ok()
                .map(|st| st.sn)
                .filter(|s| !s.is_empty());
            (None, sn)
        }
        _ => (None, None),
    }
}

fn parse_registration_line(line: &str) -> Result<(String, String)> {
    if line.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(line).context("注册 JSON 解析失败")?;
        let mdvr = v
            .get("mdvr_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("缺少 mdvr_id"))?;
        let sn = v
            .get("ups_sn")
            .or_else(|| v.get("sn"))
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("缺少 ups_sn"))?;
        return Ok((mdvr.to_string(), sn.to_string()));
    }
    let (a, b) = line
        .split_once('|')
        .ok_or_else(|| anyhow!("注册行格式应为 MDVR_ID|UPS_SN"))?;
    if a.trim().is_empty() || b.trim().is_empty() {
        return Err(anyhow!("注册字段为空"));
    }
    Ok((a.trim().to_string(), b.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_identity_uses_peer_ip() {
        let peer: SocketAddr = "192.168.1.169:46776".parse().unwrap();
        let (mdvr, sn) = fallback_identity(peer);
        assert_eq!(mdvr, "MDVR-192.168.1.169");
        assert_eq!(sn, "UPS-192.168.1.169");
        assert!(is_fallback_sn(&sn));
    }

    #[test]
    fn fallback_sn_only_matches_ipv4_shape() {
        assert!(is_fallback_sn("UPS-10.0.0.7"));
        assert!(!is_fallback_sn("UPS-9001"));
        assert!(!is_fallback_sn("ups-10.0.0.7"));
        assert!(!is_fallback_sn("MDVR-10.0.0.7"));
    }

    #[test]
    fn registration_frame_yields_full_identity() {
        let payload = codec::registration_payload("MDVR-001", "UPS-9002");
        let frame = Frame::new(0x01, cmd::REGISTER, payload);
        assert_eq!(
            identity_from_frame(&frame),
            (Some("MDVR-001".to_string()), Some("UPS-9002".to_string()))
        );
    }

    /// 首帧就是状态帧时，SN 直接从载荷取，不再先占位成 `UPS-<IP>` 再迁移。
    #[test]
    fn first_status_frame_supplies_real_sn() {
        let st = StatusA {
            power_voltage: 10.5,
            ups_voltage: 14.6,
            temperature: 32.0,
            battery_level: 88,
            smart_status: 1,
            sn: "UPS-9001".to_string(),
        };
        let frame = Frame::new(codec::DEFAULT_ADDRESS, cmd::STATUS_A, st.encode());
        let (mdvr, sn) = identity_from_frame(&frame);
        assert_eq!(mdvr, None, "状态帧不携带 MDVR 标识，应由调用方按 IP 补齐");
        assert_eq!(sn.as_deref(), Some("UPS-9001"));
        // 真实 SN 不是占位身份，后续 'a' 帧不会再触发重绑/历史迁移
        assert!(!is_fallback_sn(sn.as_deref().unwrap()));
    }

    /// 设备 SN 尚未写入时载荷里是 20 个空格，此时应回落到 IP 占位身份。
    #[test]
    fn status_frame_with_blank_sn_falls_back() {
        let st = StatusA {
            power_voltage: 10.0,
            ups_voltage: 13.0,
            temperature: 25.0,
            battery_level: 50,
            smart_status: 0,
            sn: String::new(),
        };
        let frame = Frame::new(codec::DEFAULT_ADDRESS, cmd::STATUS_A, st.encode());
        assert_eq!(identity_from_frame(&frame), (None, None));
    }

    #[test]
    fn unrelated_frame_yields_no_identity() {
        let frame = Frame::new(0x01, cmd::VERSION, b"V1.2".to_vec());
        assert_eq!(identity_from_frame(&frame), (None, None));
    }

    #[test]
    fn parses_all_registration_line_formats() {
        assert_eq!(
            parse_registration_line(r#"{"mdvr_id":"MDVR-001","ups_sn":"UPS-9001"}"#).unwrap(),
            ("MDVR-001".to_string(), "UPS-9001".to_string())
        );
        assert_eq!(
            parse_registration_line(r#"{"mdvr_id":"MDVR-001","sn":"UPS-9002"}"#).unwrap(),
            ("MDVR-001".to_string(), "UPS-9002".to_string())
        );
        assert_eq!(
            parse_registration_line("MDVR-001|UPS-9003").unwrap(),
            ("MDVR-001".to_string(), "UPS-9003".to_string())
        );
        assert!(parse_registration_line("垃圾数据").is_err());
        assert!(parse_registration_line("MDVR-001|").is_err());
    }
}
