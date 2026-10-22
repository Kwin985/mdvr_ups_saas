//! UPS 串口协议编解码，依据 `docs/UPS串口通讯协议（RS485）v1.4.doc`。
//!
//! 帧格式（线上字节，经 4G 透传后不变）：
//!
//! ```text
//! 0x7E | 设备类型 | 地址码 | 命令字 | 数据正文 ... | 校验码 | 0x7E
//! ```
//!
//! * 设备类型：`0x01` 多功能手麦、`0x02` LED 面板、`0x10` UPS。
//! * 地址码：RS485 设备 `0-247`，未配置时默认 `0x00`。
//! * 命令字：**ASCII 字母**（`a`/`V`/`r`/`u`/`b`/`d`/`f`/`N`/`S`/`D`/`F`）。
//! * 校验码：校验位之前所有字节（设备类型..数据正文）的异或值。
//!   文档的转义示例把开始符也算进去了，而**实机抓包（`a` 帧）不含开始符**，
//!   本实现以实机为准，见 `docs/protocol.md`。
//!
//! 转义规则（只作用于两个 0x7E 之间，且只解码一次）：
//!   * 0x7E -> 0x7D 0x02
//!   * 0x7D -> 0x7D 0x01

use std::fmt;

/// 地址码默认值：文档「RS485 设备有效且可配置（0-247），其他默认 00」。
pub const DEFAULT_ADDRESS: u8 = 0x00;

// ---------------------------------------------------------------- 命令字

/// 命令字：真实协议里是 ASCII 字母，收发方向由上下文区分。
pub mod cmd {
    // ---- 主机 -> UPS ----
    /// 请求版本信息（载荷空）
    pub const QUERY_VERSION: u8 = b'r';
    /// 重启系统（载荷空）
    pub const RESTART: u8 = b'u';
    /// 串口参数设置（2 字节大端位域）
    pub const SET_BAUDRATE: u8 = b'b';
    /// 设备地址设置（1 字节，0-247）
    pub const SET_ADDRESS: u8 = b'd';
    /// 数据推送频率（2 字节大端，单位秒，1-3600）
    pub const SET_REPORT_INTERVAL: u8 = b'f';
    /// UPS SN 设置（20 字节 ASCII）
    pub const SET_SN: u8 = b'N';

    // ---- UPS -> 主机 ----
    /// 实时状态信息（28 字节载荷）
    pub const STATUS_A: u8 = b'a';
    /// 版本信息（7 字节 ASCII，如 `V010101`）
    pub const VERSION: u8 = b'V';

    // ---- OTA（ISP 升级），双向同命令字 ----
    /// 开始升级：主机发 `Size + Number`，UPS 回 1 字节 ack
    pub const OTA_BEGIN: u8 = b'S';
    /// 升级数据：主机发 `FrameID + 数据包`，UPS 回 `FrameID + ack`
    pub const OTA_DATA: u8 = b'D';
    /// 结束升级：主机发空载荷，UPS 回 1 字节 ack
    pub const OTA_END: u8 = b'F';

    // ---- 平台自定义（TCP 注册，非串口协议）----
    /// MDVR -> 平台：注册帧
    pub const REGISTER: u8 = 0x80;
}

/// 应答码（文档 3.4 节）。
pub mod ack {
    /// 成功 / 继续
    pub const CONTINUE: u8 = 0x30;
    /// 校验和错误：重发当前记录
    pub const RETRY: u8 = 0x31;
    /// 校验正确但写 Flash 失败：从开始升级命令整轮重来
    pub const FLASH_FAIL: u8 = 0x32;
}

pub fn ack_name(code: u8) -> &'static str {
    match code {
        ack::CONTINUE => "OK(0x30)",
        ack::RETRY => "RETRY(0x31)",
        ack::FLASH_FAIL => "FLASH_FAIL(0x32)",
        _ => "UNKNOWN",
    }
}

/// 命令字的中文名（日志 / 审计用）。
pub fn command_name(cmd: u8) -> &'static str {
    match cmd {
        cmd::STATUS_A => "实时状态信息(a)",
        cmd::VERSION => "版本信息(V)",
        cmd::QUERY_VERSION => "请求版本(r)",
        cmd::RESTART => "重启(u)",
        cmd::SET_BAUDRATE => "串口参数(b)",
        cmd::SET_ADDRESS => "设备地址(d)",
        cmd::SET_REPORT_INTERVAL => "推送频率(f)",
        cmd::SET_SN => "SN 设置(N)",
        cmd::OTA_BEGIN => "OTA 开始(S)",
        cmd::OTA_DATA => "OTA 数据(D)",
        cmd::OTA_END => "OTA 结束(F)",
        cmd::REGISTER => "注册",
        _ => "未知命令",
    }
}

// ---------------------------------------------------------------- 错误

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("payload too short: {0} bytes")]
    TooShort(usize),
    #[error("invalid payload")]
    InvalidPayload,
    #[error("invalid utf8 in payload")]
    InvalidUtf8,
    #[error("invalid version payload")]
    InvalidVersion,
}

// ---------------------------------------------------------------- 注册（平台自定义）

/// MDVR -> 平台注册载荷：`MDVR_ID|UPS_SN`（UTF-8）。
///
/// 这是平台自定义的 TCP 注册约定，不属于 UPS 串口协议；串口侧没有注册命令。
pub fn parse_registration(payload: &[u8]) -> Result<(String, String), CodecError> {
    let text = std::str::from_utf8(payload).map_err(|_| CodecError::InvalidUtf8)?;
    let (mdvr, sn) = text.split_once('|').ok_or(CodecError::InvalidPayload)?;
    let (mdvr, sn) = (mdvr.trim(), sn.trim());
    if mdvr.is_empty() || sn.is_empty() {
        return Err(CodecError::InvalidPayload);
    }
    Ok((mdvr.to_string(), sn.to_string()))
}

pub fn registration_payload(mdvr_id: &str, sn: &str) -> Vec<u8> {
    format!("{mdvr_id}|{sn}").into_bytes()
}

// ---------------------------------------------------------------- 'a' 实时状态

/// `a` 帧载荷长度：PWR(2) + VOL(2) + TEM(2) + BLEVEL(1) + SMART(1) + SN(20)。
pub const STATUS_A_LEN: usize = 28;
/// `a` 帧 SN 字段长度。
pub const STATUS_A_SN_LEN: usize = 20;
/// 采样值换算除数：物理量 = 原始值 / 100。
///
/// 文档只写「BIT15~8 / BIT7~0 对应采样值」未标单位，按现场惯例取 0.01 倍率：
/// PWR 1007 -> 10.07 V、VOL 1305 -> 13.05 V、TEM 4200 -> 42.00 ℃。
/// 用除法而不是乘 0.01，避免 1305 * 0.01 = 13.049999 这类浮点误差。
pub const STATUS_A_DIVISOR: f32 = 100.0;

/// UPS 实时状态信息（`a` 帧）。
#[derive(Debug, Clone, PartialEq)]
pub struct StatusA {
    /// PWR：电源电压（V）
    pub power_voltage: f32,
    /// VOL：UPS 电压（V）
    pub ups_voltage: f32,
    /// TEM：当前温度（℃）
    pub temperature: f32,
    /// BLEVEL：剩余电量（%）
    pub battery_level: u8,
    /// SMART：健康状态 0 良好 / 1 一般 / 2 差 / 3 差（建议更换）
    pub smart_status: u8,
    /// SN：20 字节 UPS 唯一识别码，未写入时为一串空格
    pub sn: String,
}

impl StatusA {
    pub fn parse(payload: &[u8]) -> Result<Self, CodecError> {
        if payload.len() < STATUS_A_LEN {
            return Err(CodecError::TooShort(payload.len()));
        }
        let sample =
            |i: usize| u16::from_be_bytes([payload[i], payload[i + 1]]) as f32 / STATUS_A_DIVISOR;
        // 温度允许零下，按 i16 解释同一组 BIT15~0
        let temp = i16::from_be_bytes([payload[4], payload[5]]) as f32 / STATUS_A_DIVISOR;
        let sn_raw = &payload[8..8 + STATUS_A_SN_LEN];
        Ok(Self {
            power_voltage: sample(0),
            ups_voltage: sample(2),
            temperature: temp,
            battery_level: payload[6].min(100),
            smart_status: payload[7],
            sn: String::from_utf8_lossy(sn_raw).trim().to_string(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATUS_A_LEN);
        let word = |v: f32| ((v * STATUS_A_DIVISOR).round() as u16).to_be_bytes();
        let signed = |v: f32| ((v * STATUS_A_DIVISOR).round() as i16).to_be_bytes();
        out.extend_from_slice(&word(self.power_voltage));
        out.extend_from_slice(&word(self.ups_voltage));
        out.extend_from_slice(&signed(self.temperature));
        out.push(self.battery_level.min(100));
        out.push(self.smart_status);
        let mut sn = [0x20u8; STATUS_A_SN_LEN];
        for (slot, byte) in sn
            .iter_mut()
            .zip(self.sn.as_bytes().iter().take(STATUS_A_SN_LEN))
        {
            *slot = *byte;
        }
        out.extend_from_slice(&sn);
        out
    }

    pub fn smart_label(&self) -> &'static str {
        smart_label(self.smart_status)
    }
}

/// 健康状态文案（文档：0-良好，1-一般，2-差，3-差（建议更换））。
pub fn smart_label(smart: u8) -> &'static str {
    match smart {
        0 => "良好",
        1 => "一般",
        2 => "差",
        _ => "差（建议更换）",
    }
}

// ---------------------------------------------------------------- 'V' 版本信息

/// `V` 帧载荷：7 字节 ASCII，例如 `V010101`（MCU 版本 V1.1.1）。
pub const VERSION_PAYLOAD_LEN: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl Version {
    pub fn parse(payload: &[u8]) -> Result<Self, CodecError> {
        if payload.len() < VERSION_PAYLOAD_LEN {
            return Err(CodecError::TooShort(payload.len()));
        }
        let text = std::str::from_utf8(&payload[..VERSION_PAYLOAD_LEN])
            .map_err(|_| CodecError::InvalidUtf8)?
            .trim();
        let digits = text
            .strip_prefix('V')
            .or_else(|| text.strip_prefix('v'))
            .unwrap_or(text);
        if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CodecError::InvalidVersion);
        }
        let part = |i: usize| digits[i..i + 2].parse::<u8>().unwrap_or(0);
        Ok(Self {
            major: part(0),
            minor: part(2),
            patch: part(4),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        format!("V{:02}{:02}{:02}", self.major, self.minor, self.patch).into_bytes()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// ---------------------------------------------------------------- 设置类命令

/// 波特率档位表（文档 3.3 节 `para` bit0-3）。
pub const BAUD_LEVELS: [(u8, u32); 10] = [
    (0, 600),
    (1, 1200),
    (2, 2400),
    (3, 4800),
    (4, 9600),
    (5, 19200),
    (6, 38400),
    (7, 57600),
    (8, 115200),
    (9, 28800),
];

pub fn baud_name(index: u8) -> &'static str {
    match index {
        0 => "600",
        1 => "1200",
        2 => "2400",
        3 => "4800",
        4 => "9600",
        5 => "19200",
        6 => "38400",
        7 => "57600",
        8 => "115200",
        9 => "28800",
        _ => "未知档位",
    }
}

/// `b` 命令的 `para`（2 字节大端）：bit0-3 波特率档位，bit4-6 校验格式，bit7 停止位。
pub fn baud_para(index: u8) -> [u8; 2] {
    ((index & 0x0F) as u16).to_be_bytes()
}

/// `f` 命令的推送频率（2 字节大端，秒）。
pub fn interval_para(secs: u16) -> [u8; 2] {
    secs.to_be_bytes()
}

// ---------------------------------------------------------------- OTA 载荷

/// 每包数据长度（文档 3.4 节：64 字节，最后一包按实际长度）。
pub const OTA_CHUNK_SIZE: usize = 64;

/// 升级包大小上限：文档规定 `Size` 为 2 字节。
pub const OTA_MAX_SIZE: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaBegin {
    pub size: u16,
    pub packets: u16,
}

/// `S` 命令载荷：Size(2B BE) + Number(2B BE)。
pub fn ota_begin_payload(size: u16, packets: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&packets.to_be_bytes());
    out
}

pub fn parse_ota_begin(payload: &[u8]) -> Result<OtaBegin, CodecError> {
    if payload.len() < 4 {
        return Err(CodecError::TooShort(payload.len()));
    }
    Ok(OtaBegin {
        size: u16::from_be_bytes([payload[0], payload[1]]),
        packets: u16::from_be_bytes([payload[2], payload[3]]),
    })
}

/// `D` 命令载荷：FrameID(2B BE) + 数据包。
pub fn ota_data_payload(frame_id: u16, chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + chunk.len());
    out.extend_from_slice(&frame_id.to_be_bytes());
    out.extend_from_slice(chunk);
    out
}

pub fn parse_ota_data(payload: &[u8]) -> Result<(u16, &[u8]), CodecError> {
    if payload.len() < 2 {
        return Err(CodecError::TooShort(payload.len()));
    }
    let id = u16::from_be_bytes([payload[0], payload[1]]);
    Ok((id, &payload[2..]))
}

/// `F` 命令载荷：空。
pub fn ota_end_payload() -> Vec<u8> {
    Vec::new()
}

/// 设置类 / 升级应答的 ack 字节（载荷首字节）。
pub fn parse_ack(payload: &[u8]) -> Option<u8> {
    payload.first().copied()
}

/// `D` 应答载荷：FrameID(2B BE) + ack。
pub fn parse_data_ack(payload: &[u8]) -> Option<(u16, u8)> {
    if payload.len() < 3 {
        return None;
    }
    Some((u16::from_be_bytes([payload[0], payload[1]]), payload[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_status_a_frame() {
        use crate::protocol::frame::Frame;
        use crate::protocol::{FrameParser, ParseEvent};

        // 现场抓包：UPS -> 主机，实时状态信息 'a'
        let mut wire = vec![
            0x7Eu8, 0x10, 0x00, cmd::STATUS_A, 0x03, 0xEF, 0x05, 0x19, 0x10, 0x68, 0x10, 0x00,
        ];
        wire.extend(std::iter::repeat_n(0x20u8, 20));
        wire.extend([0xE9, 0x7E]);

        // 真实解析器：无转义、校验通过、address=0x00 不被拒绝
        let mut parser = FrameParser::new();
        let events = parser.feed(&wire);
        assert_eq!(events.len(), 1);
        let ParseEvent::Frame(f) = &events[0] else {
            panic!("expected frame");
        };
        f.clone().require_ups().unwrap();
        assert_eq!(f.address, 0x00);
        assert_eq!(f.command, cmd::STATUS_A);
        assert_eq!(f.checksum(), 0xE9);
        assert_eq!(f.payload.len(), STATUS_A_LEN);

        let st = StatusA::parse(&f.payload).unwrap();
        assert_eq!(st.power_voltage, 10.07);
        assert_eq!(st.ups_voltage, 13.05);
        assert_eq!(st.temperature, 42.00);
        assert_eq!(st.battery_level, 16);
        assert_eq!(st.smart_status, 0);
        assert_eq!(st.smart_label(), "良好");
        assert_eq!(st.sn, "", "20 个空格表示 SN 未写入");

        // 帧体也可独立解码
        let body = crate::protocol::frame::unescape(&wire[1..wire.len() - 1]).unwrap();
        assert_eq!(Frame::decode_body(&body).unwrap(), *f);
    }

    #[test]
    fn status_a_roundtrip_keeps_sn_field() {
        let st = StatusA {
            power_voltage: 12.34,
            ups_voltage: 13.05,
            temperature: -5.5,
            battery_level: 87,
            smart_status: 1,
            sn: "UPS2024000001".into(),
        };
        let bytes = st.encode();
        assert_eq!(bytes.len(), STATUS_A_LEN);
        assert_eq!(&bytes[8..28], b"UPS2024000001       ");
        assert_eq!(StatusA::parse(&bytes).unwrap(), st);
        assert!(matches!(
            StatusA::parse(&bytes[..10]),
            Err(CodecError::TooShort(_))
        ));
    }

    #[test]
    fn version_roundtrip() {
        let v = Version::parse(b"V010101").unwrap();
        assert_eq!(v.to_string(), "1.1.1");
        assert_eq!(v.encode(), b"V010101");
        assert!(matches!(
            Version::parse(b"V01"),
            Err(CodecError::TooShort(_))
        ));
        assert!(matches!(
            Version::parse(b"V0101X1"),
            Err(CodecError::InvalidVersion)
        ));
    }

    #[test]
    fn ota_payloads_are_big_endian() {
        let begin = ota_begin_payload(8192, 128);
        assert_eq!(begin, vec![0x20, 0x00, 0x00, 0x80]);
        let parsed = parse_ota_begin(&begin).unwrap();
        assert_eq!(
            parsed,
            OtaBegin {
                size: 8192,
                packets: 128
            }
        );

        let chunk = [0x7E, 0x7D, 0x00, 0xFF];
        let data = ota_data_payload(300, &chunk);
        assert_eq!(&data[..2], &[0x01, 0x2C]);
        let (id, body) = parse_ota_data(&data).unwrap();
        assert_eq!(id, 300);
        assert_eq!(body, chunk);

        assert_eq!(ota_end_payload(), Vec::<u8>::new());
        assert_eq!(parse_ack(&[0x30]), Some(0x30));
        assert_eq!(parse_data_ack(&[0x01, 0x2C, 0x31]), Some((300, 0x31)));
        assert_eq!(parse_data_ack(&[0x01]), None);
    }

    #[test]
    fn baud_and_interval_params() {
        assert_eq!(baud_para(4), [0x00, 0x04]);
        assert_eq!(baud_name(4), "9600");
        assert_eq!(baud_name(8), "115200");
        assert_eq!(interval_para(60), [0x00, 0x3C]);
    }
}
