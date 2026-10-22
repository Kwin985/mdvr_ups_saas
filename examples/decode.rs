//! 临时调试工具：把一串十六进制字节喂给真实的 FrameParser，打印解析结果。
//!
//! cargo run --example decode -- "7E 10 00 ..."

use mdvr_ups::protocol::frame::{unescape, xor_checksum};
use mdvr_ups::protocol::{codec, FrameParser, ParseEvent};

fn main() {
    let arg = std::env::args().nth(1).expect("usage: decode \"7E 10 ..\"");
    let bytes: Vec<u8> = arg
        .split_whitespace()
        .map(|t| u8::from_str_radix(t.trim(), 16).expect("bad hex"))
        .collect();

    println!("输入 {} 字节: {:02X?}", bytes.len(), bytes);

    // 帧体（两个 0x7E 之间）
    if bytes.first() == Some(&0x7E) && bytes.last() == Some(&0x7E) && bytes.len() > 2 {
        let body = &bytes[1..bytes.len() - 1];
        println!("帧体 {} 字节: {:02X?}", body.len(), body);
        match unescape(body) {
            Ok(logical) => {
                println!("反转义后 {} 字节: {:02X?}", logical.len(), logical);
                let expect = xor_checksum(&logical[..logical.len() - 1]);
                let actual = logical[logical.len() - 1];
                println!(
                    "校验: 计算值 0x{expect:02X} / 帧内值 0x{actual:02X} => {}",
                    if expect == actual { "一致" } else { "不一致" }
                );
                println!("字段: type=0x{:02X} addr=0x{:02X} cmd=0x{:02X} payload_len={}",
                    logical[0], logical[1], logical[2], logical.len() - 4);
                println!("payload: {:02X?}", &logical[3..logical.len() - 1]);
            }
            Err(e) => println!("反转义失败: {e}"),
        }
    }

    let mut parser = FrameParser::new();
    for event in parser.feed(&bytes) {
        match event {
            ParseEvent::Frame(f) => {
                println!("FrameParser => {f}");
                println!(
                    "  type=0x{:02X} addr=0x{:02X} cmd=0x{:02X} ({}) payload={} 字节",
                    f.device_type,
                    f.address,
                    f.command,
                    codec::command_name(f.command),
                    f.payload.len()
                );
                println!("  payload: {:02X?}", f.payload);
                match f.command {
                    codec::cmd::STATUS_A => {
                        println!("  状态: {:?}", codec::StatusA::parse(&f.payload));
                    }
                    codec::cmd::VERSION => {
                        println!("  版本: {:?}", codec::Version::parse(&f.payload));
                    }
                    codec::cmd::OTA_DATA => {
                        println!("  OTA 数据: {:?}", codec::parse_data_ack(&f.payload));
                    }
                    codec::cmd::OTA_BEGIN | codec::cmd::OTA_END => {
                        if let Some(code) = codec::parse_ack(&f.payload) {
                            println!("  升级应答: {}", codec::ack_name(code));
                        }
                    }
                    _ => {}
                }
            }
            ParseEvent::Invalid(e) => println!("FrameParser => Invalid: {e}"),
        }
    }
}
