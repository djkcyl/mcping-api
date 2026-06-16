//! Bedrock(基岩版)服务器 ping —— RakNet Unconnected Ping/Pong(UDP,默认端口 19132)。
//!
//! 与 Java 版 SLP(TCP,见 [`protocol`](super::protocol))**完全两套**:一来一回 UDP 报文,
//! 先于任何 RakNet 连接握手,无需握手即可问到状态。响应里是一串**分号分隔**的 MOTD,字段数随版本/
//! 实现浮动(实测 The Hive 9 段、CubeCraft 10 段、官方 BDS 12 段),故全程按索引宽松取、缺段给缺省、
//! 绝不 panic(信任上报,不清洗)。所有整数大端;MOTD 长度前缀是大端 u16 的**字节数**(非字符数)。
//!
//! 报文(逐字节,已对 go-raknet / RakLib / CloudburstMC 源码核实):
//! - Unconnected Ping(33 字节):`0x01` + ping_time(i64 BE)+ MAGIC(16)+ client GUID(i64 BE)。
//! - Unconnected Pong:`0x1C` + ping_time(回显)+ server GUID(i64 BE)+ MAGIC(16)+ u16 BE 长度 + MOTD。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::minecraft::component::Component;
use crate::minecraft::protocol::{PingError, PingResult, ResolvedAddress};
use crate::minecraft::status::{Players, StatusResponse, Version};

/// RakNet OFFLINE MESSAGE MAGIC(固定 16 字节)。
const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];
const ID_UNCONNECTED_PING: u8 = 0x01;
const ID_UNCONNECTED_PONG: u8 = 0x1c;
/// Bedrock 默认端口。
pub const DEFAULT_PORT: u16 = 19132;
/// 任意固定 client GUID —— 服务器只原样回显 ping_time,不校验 GUID。
const CLIENT_GUID: i64 = 0x0123_4567_89AB_CDEF;

/// Bedrock ping 选项。
#[derive(Debug, Clone)]
pub struct BedrockOptions {
    /// 每次尝试的收包超时。
    pub timeout: Duration,
    /// 总尝试次数(含首发)。UDP 会丢包,首包常丢,故默认重发一次。
    pub attempts: u8,
}

impl Default for BedrockOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(3), attempts: 2 }
    }
}

/// Bedrock 状态(分号串解析后的结构;字段一律信任上报、不清洗)。索引对应分号串里的位置。
#[derive(Debug, Clone, Default)]
pub struct BedrockStatus {
    /// [0] 版本类型,`MCPE`(基岩/移动)或 `MCEE`(教育版),也可能是代理自报的非标准串。
    pub edition: String,
    /// [1] MOTD 第一行(含 `§` 颜色码)。
    pub motd_line1: String,
    /// [2] 协议号(**Bedrock 自成体系**,与 Java 的 47/340 那套无关)。解不出给 -1。
    pub protocol: i32,
    /// [3] 版本名,如 `1.21.90`;部分实现只给 `1` 这种,原样保留。
    pub version_name: String,
    /// [4] 在线人数。
    pub online: i64,
    /// [5] 最大人数。
    pub max: i64,
    /// [6] 服务器 GUID(十进制串;代理常给与二进制 GUID 不一致的值,仅留底)。
    pub server_guid: String,
    /// [7] MOTD 第二行 / 世界名 —— 代理常塞品牌名而非真实世界名,故不叫 level_name。
    pub motd_line2: String,
    /// [8] 游戏模式名,如 `Survival`。
    pub gamemode: String,
    /// [9] 游戏模式数字(可选)。
    pub gamemode_id: Option<i32>,
    /// [10] IPv4 端口(可选)。
    pub port_v4: Option<u16>,
    /// [11] IPv6 端口(可选)。
    pub port_v6: Option<u16>,
    /// 本地测得的往返延迟(用本机时钟,不依赖回显的 ping_time)。
    pub latency: Duration,
    /// 原始分号串(留底排查)。
    pub raw_motd: String,
}

/// Bedrock ping 结果。
#[derive(Debug, Clone)]
pub struct BedrockResult {
    pub status: BedrockStatus,
    pub address: ResolvedAddress,
}

/// 用默认选项 ping。`address` 形如 `host`、`host:port`、`1.2.3.4`、`[::1]:19132`;不带端口默认 19132。
pub async fn ping(address: &str) -> Result<BedrockResult, PingError> {
    ping_bedrock(address, &BedrockOptions::default()).await
}

/// 对 Bedrock 服务器做一次 RakNet unconnected ping。UDP 丢包按 `opts.attempts` 重发,全失败报超时。
pub async fn ping_bedrock(address: &str, opts: &BedrockOptions) -> Result<BedrockResult, PingError> {
    let (host, port) = parse_addr(address);
    let sock = UdpSocket::bind("0.0.0.0:0").await.map_err(PingError::Io)?;
    sock.connect((host.as_str(), port)).await.map_err(PingError::Io)?;

    let addr = ResolvedAddress { host, port, via_srv: false };
    let mut buf = vec![0u8; 4096]; // 容得下较长的 MOTD(理论上限 65535,实战远小于此)
    let attempts = opts.attempts.max(1);

    for i in 0..attempts {
        let start = Instant::now();
        sock.send(&build_ping(now_millis())).await.map_err(PingError::Io)?;
        match timeout(opts.timeout, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                if let Some(motd) = parse_pong(&buf[..n]) {
                    let mut status = parse_motd(&motd);
                    status.latency = start.elapsed();
                    return Ok(BedrockResult { status, address: addr });
                }
                // 收到但不是合法 pong(magic 不符/截断):落到下一次重发
            }
            Ok(Err(e)) => return Err(PingError::Io(e)),
            Err(_) if i + 1 < attempts => continue, // 超时,重发
            Err(_) => return Err(PingError::Timeout),
        }
    }
    Err(PingError::Timeout)
}

/// 构造 33 字节 Unconnected Ping。
fn build_ping(ping_time: i64) -> [u8; 33] {
    let mut b = [0u8; 33];
    b[0] = ID_UNCONNECTED_PING;
    b[1..9].copy_from_slice(&ping_time.to_be_bytes());
    b[9..25].copy_from_slice(&MAGIC);
    b[25..33].copy_from_slice(&CLIENT_GUID.to_be_bytes());
    b
}

/// 校验 Unconnected Pong 并取出 MOTD 串。包 ID 须 0x1C、MAGIC 须匹配;声明长度若超过实收则截到实收。
fn parse_pong(d: &[u8]) -> Option<String> {
    if d.len() < 35 || d[0] != ID_UNCONNECTED_PONG || d[17..33] != MAGIC {
        return None;
    }
    let slen = u16::from_be_bytes([d[33], d[34]]) as usize;
    let end = (35 + slen).min(d.len());
    Some(String::from_utf8_lossy(&d[35..end]).into_owned())
}

/// 把分号串解析成 [`BedrockStatus`]。按索引取,缺段/空段给缺省,数值解析失败不 panic。
fn parse_motd(s: &str) -> BedrockStatus {
    let f: Vec<&str> = s.split(';').collect();
    let get = |i: usize| f.get(i).map(|x| x.trim()).unwrap_or("");
    let as_i64 = |x: &str| x.parse::<i64>().unwrap_or(0);
    BedrockStatus {
        edition: get(0).to_string(),
        motd_line1: get(1).to_string(),
        protocol: get(2).parse().unwrap_or(-1),
        version_name: get(3).to_string(),
        online: as_i64(get(4)),
        max: as_i64(get(5)),
        server_guid: get(6).to_string(),
        motd_line2: get(7).to_string(),
        gamemode: get(8).to_string(),
        gamemode_id: get(9).parse().ok(),
        port_v4: get(10).parse().ok(),
        port_v6: get(11).parse().ok(),
        latency: Duration::ZERO,
        raw_motd: s.to_string(),
    }
}

/// 把 Bedrock 状态映射成统一 [`PingResult`],复用 Java 版的整卡渲染。Bedrock ping **没有** favicon /
/// 玩家抽样 / 模组,这些字段恒缺省(渲染时走占位图标、无悬浮窗)。版本名前缀 `Bedrock` 并带上游戏模式,
/// 让附加信息行一眼看出是基岩服;协议号原样透传(Bedrock 体系)。
pub fn to_ping_result(r: &BedrockResult) -> PingResult {
    let s = &r.status;
    let motd = if s.motd_line2.is_empty() {
        s.motd_line1.clone()
    } else {
        format!("{}\n{}", s.motd_line1, s.motd_line2)
    };
    let mut name = format!("Bedrock {}", s.version_name).trim().to_string();
    if !s.gamemode.is_empty() {
        name = format!("{name} · {}", s.gamemode);
    }
    let status = StatusResponse {
        version: Version { name: Some(name), protocol: s.protocol, supported_versions: Vec::new() },
        players: Some(Players { max: s.max, online: s.online, sample: Vec::new() }),
        description: Component::text(motd),
        favicon: None,
        enforces_secure_chat: None,
        previews_chat: None,
        prevents_chat_reports: None,
        is_modded: None,
        modinfo: None,
        forge_data: None,
        modpack: None,
    };
    PingResult {
        latency: Some(s.latency),
        status,
        raw_json: String::new(),
        address: r.address.clone(),
        is_legacy: false,
    }
}

/// 解析 `host` / `host:port` / `[ipv6]` / `[ipv6]:port`,缺端口给 [`DEFAULT_PORT`]。
fn parse_addr(addr: &str) -> (String, u16) {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once("]:") {
            return (h.to_string(), p.parse().unwrap_or(DEFAULT_PORT));
        }
        if let Some(h) = rest.strip_suffix(']') {
            return (h.to_string(), DEFAULT_PORT);
        }
    }
    match addr.rsplit_once(':') {
        // 仅当冒号左侧无冒号时才当作 host:port(避免把裸 IPv6 的末段当端口)
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().unwrap_or(DEFAULT_PORT)),
        _ => (addr.to_string(), DEFAULT_PORT),
    }
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
