//! 协议层集成测试：转义、校验、流式分片、坏帧重同步。

use mdvr_ups::protocol::frame::{Frame, FrameError, unescape};
use mdvr_ups::protocol::{FrameParser, ParseEvent, StatusA, Version, cmd, codec};

fn encode(addr: u8, cmd: u8, payload: &[u8]) -> Vec<u8> {
    Frame::new(addr, cmd, payload.to_vec()).encode()
}

#[test]
fn frame_roundtrip_with_all_byte_values() {
    let payload: Vec<u8> = (0..=255u8).collect();
    let frame = Frame::new(0x05, 0x91, payload.clone());
    let wire = frame.encode();
    assert_eq!(wire[0], 0x7E);
    assert_eq!(*wire.last().unwrap(), 0x7E);

    let body = unescape(&wire[1..wire.len() - 1]).unwrap();
    let decoded = Frame::decode_body(&body).unwrap();
    assert_eq!(decoded.payload, payload);
    assert_eq!(decoded.command, 0x91);
}

#[test]
fn split_stream_at_every_boundary() {
    let payload: Vec<u8> = vec![0x7E, 0x7D, 0x00, 0xFF, 0x7E, 0x11];
    let wire = encode(1, 0x81, &payload);
    for split in 0..=wire.len() {
        let mut p = FrameParser::new();
        let mut events = p.feed(&wire[..split]);
        events.extend(p.feed(&wire[split..]));
        assert_eq!(events.len(), 1, "split at {split}");
        assert!(
            matches!(&events[0], ParseEvent::Frame(f) if f.payload == payload),
            "split at {split}"
        );
    }
}

#[test]
fn multiple_frames_in_one_read() {
    let mut stream = Vec::new();
    for i in 0..20u8 {
        stream.extend_from_slice(&encode(i, 0x81, &[i; 8]));
    }
    let mut p = FrameParser::new();
    let events = p.feed(&stream);
    assert_eq!(events.len(), 20);
    assert_eq!(p.stats.frames_ok, 20);
    assert_eq!(p.buffered(), 0);
}

#[test]
fn recovers_from_truncated_frame() {
    let good = encode(7, 0x81, &[1, 2, 3]);
    let mut stream = vec![0x7E, 0x10, 0x01]; // 半帧被截断
    stream.extend_from_slice(&good);
    let mut p = FrameParser::new();
    let events = p.feed(&stream);
    // 截断帧与后续帧共用帧尾会导致第一帧校验失败，随后重新同步成功
    let ok = events
        .iter()
        .filter(|e| matches!(e, ParseEvent::Frame(_)))
        .count();
    assert!(ok >= 1, "至少应解析出一帧，实际 {ok}");
}

#[test]
fn checksum_covers_logical_bytes() {
    // 校验应基于反转义后的字节计算
    let frame = Frame::new(0x02, 0x90, vec![0x7E, 0x7D]);
    let wire = frame.encode();
    let body = unescape(&wire[1..wire.len() - 1]).unwrap();
    let decoded = Frame::decode_body(&body).unwrap();
    assert_eq!(decoded, frame);
}

#[test]
fn status_a_and_ota_codec() {
    // 'a' 实时状态帧：3 个 2 字节大端采样值 + 电量 + 健康 + 20 字节 SN
    let st = StatusA {
        power_voltage: 12.34,
        ups_voltage: 13.05,
        temperature: -5.5,
        battery_level: 100,
        smart_status: 1,
        sn: "UPS-9001".into(),
    };
    let payload = st.encode();
    assert_eq!(payload.len(), codec::STATUS_A_LEN);
    assert_eq!(StatusA::parse(&payload).unwrap(), st);

    // 用命令字 'a' 走完整帧
    let wire = encode(codec::DEFAULT_ADDRESS, cmd::STATUS_A, &payload);
    let mut p = FrameParser::new();
    let events = p.feed(&wire);
    assert_eq!(events.len(), 1);
    let ParseEvent::Frame(f) = &events[0] else {
        panic!("expected frame");
    };
    assert_eq!(f.command, b'a');
    assert_eq!(StatusA::parse(&f.payload).unwrap(), st);

    // 'V' 版本帧：7 字节 ASCII
    let v = Version {
        major: 2,
        minor: 0,
        patch: 1,
    };
    assert_eq!(v.encode(), b"V020001");
    assert_eq!(Version::parse(&v.encode()).unwrap(), v);

    // OTA：Size / Number / FrameID 均为 2 字节大端
    let begin = codec::ota_begin_payload(8192, 128);
    let parsed = codec::parse_ota_begin(&begin).unwrap();
    assert_eq!(parsed.size, 8192);
    assert_eq!(parsed.packets, 128);

    let chunk = vec![0x7E; codec::OTA_CHUNK_SIZE];
    let payload = codec::ota_data_payload(9, &chunk);
    let (id, data) = codec::parse_ota_data(&payload).unwrap();
    assert_eq!(id, 9);
    assert_eq!(data.len(), codec::OTA_CHUNK_SIZE);

    // 'D' 应答：FrameID + ack
    assert_eq!(codec::parse_data_ack(&[0x00, 0x09, 0x31]), Some((9, 0x31)));
}

#[test]
fn rejects_frame_without_end_flag() {
    let mut p = FrameParser::new();
    let wire = encode(1, 0x81, &[1, 2, 3]);
    assert!(p.feed(&wire[..wire.len() - 1]).is_empty());
    let events = p.feed(&wire[wire.len() - 1..]);
    assert_eq!(events.len(), 1);
}

#[test]
fn bad_checksum_is_reported_not_panicking() {
    let mut wire = encode(1, 0x81, &[9, 9, 9]);
    let n = wire.len();
    wire[n - 2] ^= 0x01;
    let mut p = FrameParser::new();
    let events = p.feed(&wire);
    assert!(matches!(
        events.first(),
        Some(ParseEvent::Invalid(FrameError::ChecksumMismatch { .. }))
    ));
}
