//! 本地 MDVR / UPS 模拟器，用于端到端联调。
//!
//! 按 `docs/UPS串口通讯协议（RS485）v1.4.doc` 收发：周期上报 `a` 实时状态帧，
//! 响应 `r`/`b`/`d`/`f`/`N` 与 OTA 的 `S`/`D`/`F`（应答沿用原命令字 + ack）。
//!
//! ```text
//! cargo run --bin simulator --release
//! ```
//!
//! 环境变量：
//!   SIM_ADDR            平台接入地址，默认 127.0.0.1:9100
//!   SIM_MDVR            MDVR_ID，默认 MDVR-001
//!   SIM_SN              UPS_SN，默认 UPS-9001
//!   SIM_INTERVAL        状态上报间隔秒，默认 2
//!   SIM_OTA_RETRY_EVERY 每 N 包模拟一次 0x31 校验错，0 表示关闭
//!   SIM_OTA_FLASH_FAIL_AT 第 N 包模拟 0x32 Flash 失败，0 表示关闭

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};

use mdvr_ups::protocol::{Frame, FrameParser, ParseEvent, StatusA, Version, ack, cmd, codec};

struct OtaSim {
    packets: u16,
    received: HashSet<u16>,
    /// 已经模拟过一次校验错的包，重发时必须放行，否则重试永远失败
    retried: HashSet<u16>,
}

struct SimState {
    battery: u8,
    temperature: f32,
    /// PWR 电源电压（12V 车载系统）
    power_voltage: f32,
    /// VOL UPS 电压
    ups_voltage: f32,
    /// `a` 帧里的 20 字节 SN 字段
    sn: String,
    version: Version,
    ota: Option<OtaSim>,
    /// 0x32 只模拟一次，重来后必须放行，否则永远无法验证恢复路径
    flash_failed: bool,
}

impl SimState {
    fn new(sn: &str) -> Self {
        Self {
            battery: 92,
            temperature: 28.5,
            power_voltage: 12.4,
            ups_voltage: 13.1,
            sn: sn.to_string(),
            version: Version {
                major: 1,
                minor: 1,
                patch: 1,
            },
            ota: None,
            flash_failed: false,
        }
    }

    fn tick(&mut self) {
        let mut rng = rand::thread_rng();
        self.battery = (self.battery as i16 + rng.gen_range(-1..=1)).clamp(20, 100) as u8;
        self.temperature = (self.temperature + rng.gen_range(-0.2..0.2)).clamp(15.0, 55.0);
        self.power_voltage = (self.power_voltage + rng.gen_range(-0.15..0.15)).clamp(10.5, 14.6);
        self.ups_voltage = (self.power_voltage + rng.gen_range(0.5..1.2)).clamp(11.0, 14.8);
    }

    fn status(&self) -> StatusA {
        StatusA {
            power_voltage: self.power_voltage,
            ups_voltage: self.ups_voltage,
            temperature: self.temperature,
            battery_level: self.battery,
            // 文档：0 良好 / 1 一般 / 2 差 / 3 差（建议更换）
            smart_status: match self.battery {
                0..=29 => 2,
                30..=49 => 1,
                _ => 0,
            },
            sn: self.sn.clone(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "simulator=info".into()),
        )
        .init();

    let addr = env("SIM_ADDR", "127.0.0.1:9100");
    let mdvr = env("SIM_MDVR", "MDVR-001");
    let sn = env("SIM_SN", "UPS-9001");
    let interval: u64 = env("SIM_INTERVAL", "2").parse().unwrap_or(2);
    let retry_every: u32 = env("SIM_OTA_RETRY_EVERY", "0").parse().unwrap_or(0);
    let flash_fail_at: u16 = env("SIM_OTA_FLASH_FAIL_AT", "0").parse().unwrap_or(0);

    let stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("连接平台 {addr} 失败"))?;
    let (mut reader, mut writer) = stream.into_split();

    let hello = format!("{{\"mdvr_id\":\"{mdvr}\",\"ups_sn\":\"{sn}\"}}\n");
    writer.write_all(hello.as_bytes()).await?;
    tracing::info!(%addr, %mdvr, %sn, "已注册到平台");

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(Mutex::new(SimState::new(&sn)));

    // 下行处理
    {
        let state = state.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut parser = FrameParser::new();
            let mut buf = [0u8; 2048];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for event in parser.feed(&buf[..n]) {
                    if let ParseEvent::Frame(frame) = event {
                        handle_downlink(frame, &state, &tx, retry_every, flash_fail_at).await;
                    }
                }
            }
            tracing::warn!("平台连接已断开");
        });
    }

    // 周期上报
    let mut ticker = tokio::time::interval(Duration::from_secs(interval.max(1)));
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let bytes = {
                    let mut s = state.lock().await;
                    s.tick();
                    Frame::new(codec::DEFAULT_ADDRESS, cmd::STATUS_A, s.status().encode()).encode()
                };
                if tx.send(bytes).await.is_err() {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("模拟器退出");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_downlink(
    frame: Frame,
    state: &Arc<Mutex<SimState>>,
    tx: &mpsc::Sender<Vec<u8>>,
    retry_every: u32,
    flash_fail_at: u16,
) {
    let addr = frame.address;
    match frame.command {
        // r：请求版本信息，回 V 帧
        cmd::QUERY_VERSION => {
            let v = state.lock().await.version;
            tracing::info!(version = %v, "收到版本查询");
            send(tx, Frame::new(addr, cmd::VERSION, v.encode())).await;
        }
        // u：重启（文档没有应答帧）
        cmd::RESTART => {
            tracing::info!("收到重启指令");
        }
        cmd::SET_BAUDRATE => {
            let para = frame
                .payload
                .get(..2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .unwrap_or(0);
            tracing::info!(para, baud = codec::baud_name((para & 0x0F) as u8), "收到串口参数设置");
            send(tx, Frame::ack(addr, cmd::SET_BAUDRATE, ack::CONTINUE)).await;
        }
        cmd::SET_ADDRESS => {
            tracing::info!(addr = ?frame.payload, "收到设备地址设置");
            send(tx, Frame::ack(addr, cmd::SET_ADDRESS, ack::CONTINUE)).await;
        }
        cmd::SET_REPORT_INTERVAL => {
            let secs = frame
                .payload
                .get(..2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .unwrap_or(0);
            tracing::info!(secs, "收到推送频率设置");
            send(tx, Frame::ack(addr, cmd::SET_REPORT_INTERVAL, ack::CONTINUE)).await;
        }
        cmd::SET_SN => {
            let sn = String::from_utf8_lossy(&frame.payload).trim().to_string();
            tracing::info!(sn = %sn, "收到 SN 设置");
            if !sn.is_empty() {
                state.lock().await.sn = sn;
            }
            send(tx, Frame::ack(addr, cmd::SET_SN, ack::CONTINUE)).await;
        }
        cmd::OTA_BEGIN => match codec::parse_ota_begin(&frame.payload) {
            Ok(begin) => {
                let mut s = state.lock().await;
                s.ota = Some(OtaSim {
                    packets: begin.packets,
                    received: HashSet::new(),
                    retried: HashSet::new(),
                });
                tracing::info!(size = begin.size, packets = begin.packets, "开始接收固件");
                drop(s);
                send(tx, Frame::ack(addr, cmd::OTA_BEGIN, ack::CONTINUE)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "OTA 开始载荷非法");
                send(tx, Frame::ack(addr, cmd::OTA_BEGIN, ack::RETRY)).await;
            }
        },
        cmd::OTA_DATA => {
            let Ok((frame_id, data)) = codec::parse_ota_data(&frame.payload) else {
                send(tx, Frame::ack(addr, cmd::OTA_DATA, ack::RETRY)).await;
                return;
            };
            let mut s = state.lock().await;
            if flash_fail_at > 0 && frame_id == flash_fail_at && !s.flash_failed {
                tracing::warn!(frame_id, "模拟 Flash 写入失败 0x32");
                s.flash_failed = true;
                s.ota = None;
                drop(s);
                send(tx, data_ack(addr, frame_id, ack::FLASH_FAIL)).await;
                return;
            }
            let simulate_retry = retry_every > 0
                && frame_id % retry_every as u16 == 0
                && s.ota
                    .as_ref()
                    .map(|o| !o.received.contains(&frame_id) && !o.retried.contains(&frame_id))
                    .unwrap_or(false);
            if simulate_retry {
                tracing::warn!(frame_id, "模拟校验错 0x31");
                if let Some(ota) = s.ota.as_mut() {
                    ota.retried.insert(frame_id);
                }
                drop(s);
                send(tx, data_ack(addr, frame_id, ack::RETRY)).await;
                return;
            }
            if let Some(ota) = s.ota.as_mut() {
                ota.received.insert(frame_id);
                if frame_id % 50 == 0 {
                    tracing::info!(
                        frame_id,
                        received = ota.received.len(),
                        total = ota.packets,
                        bytes = data.len(),
                        "接收进度"
                    );
                }
            }
            drop(s);
            send(tx, data_ack(addr, frame_id, ack::CONTINUE)).await;
        }
        cmd::OTA_END => {
            let mut s = state.lock().await;
            let ok = s
                .ota
                .as_ref()
                .map(|o| o.received.len() as u16 >= o.packets)
                .unwrap_or(false);
            if ok {
                if let Some(ota) = s.ota.take() {
                    tracing::info!(packets = ota.packets, "固件接收完成，写入 Flash");
                }
                s.version.patch = s.version.patch.wrapping_add(1);
                tracing::info!(version = %s.version, "升级完成");
                drop(s);
                send(tx, Frame::ack(addr, cmd::OTA_END, ack::CONTINUE)).await;
            } else {
                drop(s);
                send(tx, Frame::ack(addr, cmd::OTA_END, ack::RETRY)).await;
            }
        }
        cmd::REGISTER => {}
        other => tracing::debug!(
            cmd = format_args!("0x{other:02X}"),
            name = codec::command_name(other),
            "忽略未知命令"
        ),
    }
}

/// `D` 应答：FrameID(2B BE) + ack。
fn data_ack(addr: u8, frame_id: u16, code: u8) -> Frame {
    Frame::new(addr, cmd::OTA_DATA, codec::ota_data_payload(frame_id, &[code]))
}

async fn send(tx: &mpsc::Sender<Vec<u8>>, frame: Frame) {
    let _ = tx.send(frame.encode()).await;
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
