//! 旧版(pre-1.7)SLP 客户端 —— 照原版 `LegacyServerPinger` 复刻 1.6 形式的 `0xFE 0x01 0xFA` ping。
//!
//! 请求:`FE 01 FA` + 旧式串 `"MC|PingHost"` + `u16(载荷字节数)` + `载荷{ 协议字节 0x7F + 旧式串(host) +
//! i32(port,大端) }`。旧式串 = `u16(字符数,大端) + UTF-16BE`。
//!
//! 响应:`0xFF` + 旧式串。串以 `§1\0` 开头时按 NUL 分隔取 `protocol\0version\0motd\0online\0max`;
//! 否则按 `§` 分隔的古老格式 `motd§online§max`。现代服务器仍带 `LegacyQueryHandler`,也会用 §1 格式作答。

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as TokioStream;
use tokio::time::timeout;

use crate::minecraft::protocol::PingError;

/// 旧版 ping 解析结果。
#[derive(Debug, Clone)]
pub struct LegacyStatus {
    pub protocol: i32,
    pub version: String,
    pub motd: String,
    pub online: i64,
    pub max: i64,
}

fn write_legacy_string(buf: &mut Vec<u8>, s: &str) {
    let utf16: Vec<u16> = s.encode_utf16().collect();
    buf.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
    for u in utf16 {
        buf.extend_from_slice(&u.to_be_bytes());
    }
}

fn build_request(host: &str, port: u16) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0xFE, 0x01, 0xFA]);
    write_legacy_string(&mut p, "MC|PingHost");

    let mut payload = Vec::new();
    payload.push(0x7F); // FAKE_PROTOCOL_VERSION = 127
    write_legacy_string(&mut payload, host);
    payload.extend_from_slice(&(port as i32).to_be_bytes());

    p.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    p.extend_from_slice(&payload);
    p
}

/// 对 `host:port` 发旧版 ping,返回解析结果与往返耗时。
pub async fn ping_legacy(
    host: &str,
    port: u16,
    t: Duration,
) -> Result<(LegacyStatus, Duration), PingError> {
    let stream = timeout(t, TokioStream::connect((host, port)))
        .await
        .map_err(|_| PingError::Timeout)?
        .map_err(PingError::Io)?;
    timeout(t, exchange(stream, host, port)).await.map_err(|_| PingError::Timeout)?
}

async fn exchange(
    mut s: TokioStream,
    host: &str,
    port: u16,
) -> Result<(LegacyStatus, Duration), PingError> {
    let start = Instant::now();
    s.write_all(&build_request(host, port)).await.map_err(PingError::Io)?;
    s.flush().await.map_err(PingError::Io)?;

    let first = s.read_u8().await.map_err(PingError::Io)?;
    if first != 0xFF {
        return Err(PingError::Protocol(format!("旧版响应包 ID 应为 0xFF,得到 {first:#x}")));
    }
    let char_count = s.read_u16().await.map_err(PingError::Io)? as usize; // 大端
    if char_count > 64 * 1024 {
        return Err(PingError::Protocol("旧版响应过长".into()));
    }
    let mut bytes = vec![0u8; char_count * 2];
    s.read_exact(&mut bytes).await.map_err(PingError::Io)?;
    let elapsed = start.elapsed();

    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16_lossy(&units);
    Ok((parse_legacy(&text)?, elapsed))
}

fn parse_legacy(text: &str) -> Result<LegacyStatus, PingError> {
    // 1.4+ 的 §1 格式:§1\0protocol\0version\0motd\0online\0max
    if let Some(rest) = text.strip_prefix("\u{00a7}1\u{0000}") {
        let parts: Vec<&str> = rest.split('\u{0000}').collect();
        if parts.len() >= 5 {
            return Ok(LegacyStatus {
                protocol: parts[0].trim().parse().unwrap_or(-1),
                version: parts[1].to_string(),
                motd: parts[2].to_string(),
                online: parts[3].trim().parse().unwrap_or(-1),
                max: parts[4].trim().parse().unwrap_or(-1),
            });
        }
    }
    // 古老格式:motd§online§max(motd 可能含 §,故末两段为人数)
    let parts: Vec<&str> = text.split('\u{00a7}').collect();
    if parts.len() >= 3 {
        let n = parts.len();
        return Ok(LegacyStatus {
            protocol: -1,
            version: String::new(),
            motd: parts[..n - 2].join("\u{00a7}"),
            online: parts[n - 2].trim().parse().unwrap_or(-1),
            max: parts[n - 1].trim().parse().unwrap_or(-1),
        });
    }
    Err(PingError::Protocol("旧版响应无法解析".into()))
}
