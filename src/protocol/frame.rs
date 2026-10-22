//! UPS 串口协议帧编解码。
//!
//! 线上帧格式（经 4G 透传后字节完全不变）：
//!
//! ```text
//! 0x7E | 设备类型(0x10) | 地址码 | 命令字 | 数据正文 ... | XOR 校验 | 0x7E
//! ```
//!
//! 转义规则（只作用于两个 0x7E 之间的内容）：
//!   * 0x7E -> 0x7D 0x02
//!   * 0x7D -> 0x7D 0x01
//!
//! 校验规则：设备类型、地址码、命令字、数据正文（均为**转义前**的逻辑字节）逐字节 XOR。

use std::fmt;

pub const FLAG: u8 = 0x7E;
pub const ESC: u8 = 0x7D;
pub const ESC_FLAG: u8 = 0x02;
pub const ESC_ESC: u8 = 0x01;

/// UPS 设备类型码，见协议文档。
pub const DEVICE_TYPE_UPS: u8 = 0x10;

/// 单帧最大逻辑长度（防止异常数据无限堆积内存）。
pub const MAX_FRAME_BODY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub device_type: u8,
    pub address: u8,
    pub command: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame too short: {0} bytes")]
    TooShort(usize),
    #[error("checksum mismatch: expected 0x{expected:02X}, got 0x{actual:02X}")]
    ChecksumMismatch { expected: u8, actual: u8 },
    #[error("invalid escape sequence at offset {0}")]
    BadEscape(usize),
    #[error("frame body exceeds {MAX_FRAME_BODY} bytes")]
    TooLong,
    #[error("unexpected device type 0x{0:02X}")]
    UnexpectedDeviceType(u8),
}

impl Frame {
    pub fn new(address: u8, command: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            device_type: DEVICE_TYPE_UPS,
            address,
            command,
            payload: payload.into(),
        }
    }

    /// 逻辑字节（未转义）的校验值。
    pub fn checksum(&self) -> u8 {
        let mut acc = self.device_type ^ self.address ^ self.command;
        for b in &self.payload {
            acc ^= b;
        }
        acc
    }

    /// 编码为完整线上帧（含首尾 0x7E 与转义）。
    pub fn encode(&self) -> Vec<u8> {
        let mut logical = Vec::with_capacity(self.payload.len() + 4);
        logical.push(self.device_type);
        logical.push(self.address);
        logical.push(self.command);
        logical.extend_from_slice(&self.payload);
        let checksum = xor_checksum(&logical);
        logical.push(checksum);

        let mut out = Vec::with_capacity(logical.len() + 2);
        out.push(FLAG);
        escape_into(&mut out, &logical);
        out.push(FLAG);
        out
    }

    /// 从**已转义解码**的帧体（设备类型..校验位）构造帧。
    pub fn decode_body(body: &[u8]) -> Result<Self, FrameError> {
        if body.len() < 4 {
            return Err(FrameError::TooShort(body.len()));
        }
        if body.len() > MAX_FRAME_BODY {
            return Err(FrameError::TooLong);
        }
        let (logical, checksum) = body.split_at(body.len() - 1);
        let expected = xor_checksum(logical);
        if expected != checksum[0] {
            return Err(FrameError::ChecksumMismatch {
                expected,
                actual: checksum[0],
            });
        }
        Ok(Self {
            device_type: logical[0],
            address: logical[1],
            command: logical[2],
            payload: logical[3..].to_vec(),
        })
    }

    /// 仅接受 UPS 设备类型的帧（接入层使用）。
    pub fn require_ups(self) -> Result<Self, FrameError> {
        if self.device_type != DEVICE_TYPE_UPS {
            return Err(FrameError::UnexpectedDeviceType(self.device_type));
        }
        Ok(self)
    }

    /// 应答帧：命令字沿用原命令，载荷为 1 字节 ack（`0x30` / `0x31` / `0x32`）。
    pub fn ack(address: u8, command: u8, code: u8) -> Self {
        Self::new(address, command, vec![code])
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Frame{{type=0x{:02X}, addr=0x{:02X}, cmd=0x{:02X}, len={}}}",
            self.device_type,
            self.address,
            self.command,
            self.payload.len()
        )
    }
}

/// 对逻辑字节求 XOR。
pub fn xor_checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc ^ b)
}

/// 转义写入：0x7E -> 7D 02，0x7D -> 7D 01。
pub fn escape_into(out: &mut Vec<u8>, data: &[u8]) {
    for &b in data {
        match b {
            FLAG => out.extend_from_slice(&[ESC, ESC_FLAG]),
            ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
            _ => out.push(b),
        }
    }
}

/// 转义解码。严格实现「只解码一次」，且拒绝非法转义序列。
pub fn unescape(data: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b != ESC {
            out.push(b);
            i += 1;
            continue;
        }
        let next = *data.get(i + 1).ok_or(FrameError::BadEscape(i))?;
        match next {
            ESC_FLAG => out.push(FLAG),
            ESC_ESC => out.push(ESC),
            _ => return Err(FrameError::BadEscape(i)),
        }
        i += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_xor_of_logical_bytes() {
        let f = Frame::new(0x01, 0x81, vec![0x10, 0x20]);
        assert_eq!(f.checksum(), 0x10 ^ 0x01 ^ 0x81 ^ 0x10 ^ 0x20);
    }

    #[test]
    fn encode_wraps_and_escapes() {
        let f = Frame::new(0x02, 0x91, vec![FLAG, ESC, 0x00]);
        let wire = f.encode();
        assert_eq!(wire.first(), Some(&FLAG));
        assert_eq!(wire.last(), Some(&FLAG));
        // 载荷中的 0x7E / 0x7D 必须被转义，不能出现在帧体里
        assert_eq!(wire.iter().filter(|b| **b == FLAG).count(), 2);
        assert!(wire.windows(2).any(|w| w == [ESC, ESC_FLAG]));
        assert!(wire.windows(2).any(|w| w == [ESC, ESC_ESC]));

        let body = unescape(&wire[1..wire.len() - 1]).unwrap();
        assert_eq!(Frame::decode_body(&body).unwrap(), f);
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut wire = Frame::new(0x01, 0x81, vec![1, 2, 3]).encode();
        let n = wire.len();
        wire[n - 2] ^= 0xFF;
        let body = unescape(&wire[1..n - 1]).unwrap();
        assert!(matches!(
            Frame::decode_body(&body),
            Err(FrameError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_invalid_escape() {
        assert!(matches!(
            unescape(&[0x7D, 0x03]),
            Err(FrameError::BadEscape(0))
        ));
        assert!(matches!(unescape(&[0x7D]), Err(FrameError::BadEscape(0))));
    }

    #[test]
    fn unescape_only_once() {
        // 7D 01 -> 7D，结果中的 7D 不会再次参与解码
        assert_eq!(unescape(&[ESC, ESC_ESC]).unwrap(), vec![ESC]);
        // 7D 02 7D 02 -> 7E 7E
        assert_eq!(
            unescape(&[ESC, ESC_FLAG, ESC, ESC_FLAG]).unwrap(),
            vec![FLAG, FLAG]
        );
    }
}
