//! 流式帧解析器。
//!
//! 4G 透传链路是纯字节流：一次 TCP read 可能包含半帧、多帧、粘包，
//! 也可能因线路抖动出现垃圾字节。解析器负责：
//!   1. 按 0x7E 同步帧边界；
//!   2. 对帧体做**一次**转义解码；
//!   3. 校验 XOR 并产出 [`Frame`]；
//!   4. 遇到坏帧时丢弃一个字节后重新同步，而不是丢弃整段缓冲。

use nom::IResult;
use nom::branch::alt;
use nom::bytes::complete::{tag, take};
use nom::combinator::{map, value};
use nom::multi::many0;
use nom::Parser;

use super::frame::{
    ESC, ESC_ESC, ESC_FLAG, FLAG, Frame, FrameError, MAX_FRAME_BODY,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParserStats {
    pub frames_ok: u64,
    pub checksum_errors: u64,
    pub escape_errors: u64,
    pub malformed: u64,
    pub discarded_bytes: u64,
    pub overflow_events: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseEvent {
    Frame(Frame),
    /// 帧边界完整但内容非法（校验错 / 转义错 / 长度错）。
    Invalid(FrameError),
}

impl ParseEvent {
    pub fn into_frame(self) -> Option<Frame> {
        match self {
            ParseEvent::Frame(f) => Some(f),
            ParseEvent::Invalid(_) => None,
        }
    }
}

/// 增量帧解析器。每个 TCP 连接持有一个实例，**不要跨连接共享**。
#[derive(Debug, Default)]
pub struct FrameParser {
    buf: Vec<u8>,
    pub stats: ParserStats,
}

impl FrameParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(MAX_FRAME_BODY * 2),
            stats: ParserStats::default(),
        }
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// 喂入一段字节，返回本次可产出的所有事件。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ParseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();

        loop {
            // 1. 找起始 0x7E，丢弃前导垃圾
            let Some(start) = self.buf.iter().position(|b| *b == FLAG) else {
                let len = self.buf.len();
                if len > MAX_FRAME_BODY {
                    // 长时间没有帧头，说明链路已错位，丢弃缓冲避免无界增长
                    self.buf.clear();
                    self.stats.discarded_bytes += len as u64;
                    self.stats.overflow_events += 1;
                }
                break;
            };
            if start > 0 {
                self.stats.discarded_bytes += start as u64;
                self.buf.drain(..start);
            }

            // 2. 找结束 0x7E（载荷内的 0x7E 已转义，不会误判）
            let Some(end) = self.buf[1..].iter().position(|b| *b == FLAG).map(|p| p + 1) else {
                if self.buf.len() > MAX_FRAME_BODY {
                    // 帧超长：丢掉这个帧头，继续找下一个
                    self.buf.drain(..1);
                    self.stats.discarded_bytes += 1;
                    self.stats.overflow_events += 1;
                    continue;
                }
                break; // 半帧，等更多数据
            };

            let body_raw: Vec<u8> = self.buf[1..end].to_vec();

            if body_raw.is_empty() {
                // 连续两个 0x7E：只消费前一个，保留后一个作为下一帧的起始标志
                self.buf.drain(..1);
                self.stats.malformed += 1;
                continue;
            }

            match decode_body(&body_raw) {
                Ok(frame) => {
                    self.buf.drain(..=end);
                    self.stats.frames_ok += 1;
                    events.push(ParseEvent::Frame(frame));
                }
                Err(err) => {
                    match &err {
                        FrameError::ChecksumMismatch { .. } => self.stats.checksum_errors += 1,
                        FrameError::BadEscape(_) => self.stats.escape_errors += 1,
                        _ => self.stats.malformed += 1,
                    }
                    // 只丢弃帧体，把帧尾 0x7E 留在缓冲里作为下一次同步的起点。
                    // 截断帧与后续帧共用帧尾时，这样才不会连带吞掉后续帧的帧头。
                    self.buf.drain(..end);
                    events.push(ParseEvent::Invalid(err));
                }
            }
        }

        events
    }
}

/// 用 nom 做帧体解码：先一次性反转义，再按固定字段切分。
fn decode_body(body_raw: &[u8]) -> Result<Frame, FrameError> {
    let (rest, logical) = unescape_nom(body_raw)
        .map_err(|_| FrameError::BadEscape(0))?;
    if !rest.is_empty() {
        return Err(FrameError::BadEscape(body_raw.len() - rest.len()));
    }
    Frame::decode_body(&logical)
}

/// nom 组合子：many0(alt(7D 01 -> 7D, 7D 02 -> 7E, 任意单字节))。
/// 保证每个字节只被消费一次，实现「只解码一次」。
fn unescape_nom(input: &[u8]) -> IResult<&[u8], Vec<u8>> {
    many0(alt((
        value(ESC, tag([ESC, ESC_ESC].as_slice())),
        value(FLAG, tag([ESC, ESC_FLAG].as_slice())),
        map(take(1usize), |b: &[u8]| b[0]),
    )))
    .parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(addr: u8, cmd: u8, payload: &[u8]) -> Vec<u8> {
        Frame::new(addr, cmd, payload.to_vec()).encode()
    }

    #[test]
    fn parses_single_frame() {
        let mut p = FrameParser::new();
        let events = p.feed(&frame(1, 0x81, &[1, 2, 3]));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ParseEvent::Frame(f) if f.address == 1));
        assert_eq!(p.stats.frames_ok, 1);
    }

    #[test]
    fn handles_split_and_sticky_packets() {
        let a = frame(1, 0x81, &[9; 20]);
        let b = frame(2, 0x81, &[7; 5]);
        let mut p = FrameParser::new();
        assert!(p.feed(&a[..3]).is_empty());
        assert!(p.feed(&a[3..10]).is_empty());
        let mut events = p.feed(&a[10..]);
        assert_eq!(events.len(), 1);
        // 粘包：一帧 + 半帧
        events = p.feed(&[b.clone(), b[..4].to_vec()].concat());
        assert_eq!(events.len(), 1);
        events = p.feed(&b[4..]);
        assert_eq!(events.len(), 1);
        assert_eq!(p.stats.frames_ok, 3);
    }

    #[test]
    fn payload_flag_is_not_a_delimiter() {
        let wire = frame(1, 0x91, &[0x7E, 0x7D, 0x7E, 0x00]);
        // 线上帧体只能有两个 0x7E
        assert_eq!(wire.iter().filter(|b| **b == FLAG).count(), 2);
        let mut p = FrameParser::new();
        let events = p.feed(&wire);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ParseEvent::Frame(f) if f.payload == vec![0x7E, 0x7D, 0x7E, 0x00]));
    }

    #[test]
    fn resyncs_after_garbage_and_bad_checksum() {
        let good = frame(3, 0x81, &[4, 5]);
        let mut bad = frame(4, 0x81, &[6, 7]);
        let n = bad.len();
        bad[n - 2] ^= 0x55;

        let mut p = FrameParser::new();
        let mut stream = vec![0x00, 0x11, 0x22];
        stream.extend_from_slice(&bad);
        stream.extend_from_slice(&good);
        let events = p.feed(&stream);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ParseEvent::Invalid(FrameError::ChecksumMismatch { .. })));
        assert!(matches!(&events[1], ParseEvent::Frame(f) if f.address == 3));
        assert!(p.stats.discarded_bytes >= 3);
        assert_eq!(p.stats.checksum_errors, 1);
    }

    #[test]
    fn drops_oversized_frame_and_recovers() {
        let mut p = FrameParser::new();
        // 只有一个起始 0x7E 后跟大量非帧尾数据
        let mut junk = vec![FLAG];
        junk.extend(std::iter::repeat_n(0x33, MAX_FRAME_BODY + 10));
        assert!(p.feed(&junk).is_empty());
        assert!(p.stats.overflow_events >= 1);
        // 之后的正常帧仍能解析
        let events = p.feed(&frame(9, 0x81, &[1]));
        assert_eq!(events.len(), 1);
    }
}
