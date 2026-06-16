//! SLP ping 客户端 —— 现代握手流程,同步(std)与异步(tokio)各一条,共用 [`codec`]。
//!
//! 流程:握手(next=1)→ Status Request(0x00)→ 读 Status Response(0x00,VarInt 长前缀的
//! JSON 串)→ 可选 Ping(0x01,Long)/ Pong 测延迟。状态态不压缩不加密,所以收发都是裸帧。
//! 异步路径会先查 SRV(域名且未显式给端口时);同步路径从简,只直连。

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as TokioStream;
use tokio::time::timeout;

use crate::minecraft::codec;
use crate::minecraft::component::Component;
use crate::minecraft::legacy;
use crate::minecraft::srv;
use crate::minecraft::status::{Modpack, Players, StatusResponse, Version};

/// 单帧上限,挡住乱报长度的服务器(状态 JSON + favicon 远不到这个量级)。注意帧长前缀本身
/// 还另受 21 位(3 字节 VarInt)限制,见 [`read_frame_len`] —— 与原版 `Varint21FrameDecoder` 一致。
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// 状态 JSON 串上限(原版 `ClientboundStatusResponsePacket` 按 `Short.MAX` 量级,这里留足到 256 KiB)。
const STATUS_JSON_MAX: usize = 256 * 1024;
/// Pong 帧上限。标准 pong 仅 9 字节,但 Nyf's Modpack Version Check 会在尾部追加整合包版本 + IP
/// 两个字符串,故放宽到 1 KiB 以容下。
const PONG_FRAME_MAX: usize = 1024;
/// ping 载荷:服务器须原样回显。
const PING_PAYLOAD: i64 = 0x0123_4567_89AB_CDEFu64 as i64;
/// MC 默认端口。
pub const DEFAULT_PORT: u16 = 25565;

/// ping 选项。
#[derive(Debug, Clone)]
pub struct PingOptions {
    /// 握手里发的协议号。-1 = 「只是 ping」(惯例);严格的服务器更认真实号。
    pub protocol_version: i32,
    /// 连接 / 每步收发的超时。
    pub timeout: Duration,
    /// 域名且未显式给端口时是否查 `_minecraft._tcp` SRV(仅异步路径)。
    pub use_srv: bool,
    /// 是否再做一轮 Ping/Pong 测延迟。
    pub measure_latency: bool,
    /// 现代 ping 失败时是否回退旧版(0xFE)ping(仅异步路径)。
    pub allow_legacy: bool,
}

impl Default for PingOptions {
    fn default() -> Self {
        Self {
            protocol_version: -1,
            timeout: Duration::from_secs(5),
            use_srv: true,
            measure_latency: true,
            allow_legacy: true,
        }
    }
}

/// 实际落地的连接地址。
#[derive(Debug, Clone)]
pub struct ResolvedAddress {
    pub host: String,
    pub port: u16,
    pub via_srv: bool,
}

/// ping 结果。
#[derive(Debug, Clone)]
pub struct PingResult {
    /// 往返延迟(关了测延迟或测失败为 `None`)。
    pub latency: Option<Duration>,
    pub status: StatusResponse,
    /// 原始状态 JSON(留着排查 / 透传;旧版 ping 无 JSON,为空)。
    pub raw_json: String,
    pub address: ResolvedAddress,
    /// 是否走的旧版(pre-1.7,0xFE)ping。
    pub is_legacy: bool,
}

#[derive(Debug)]
pub enum PingError {
    Address(String),
    Io(std::io::Error),
    Timeout,
    Protocol(String),
    Json(serde_json::Error),
}

impl std::fmt::Display for PingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PingError::Address(s) => write!(f, "地址无法解析: {s}"),
            PingError::Io(e) => write!(f, "网络错误: {e}"),
            PingError::Timeout => write!(f, "连接超时"),
            PingError::Protocol(s) => write!(f, "协议错误: {s}"),
            PingError::Json(e) => write!(f, "状态 JSON 解析失败: {e}"),
        }
    }
}

impl std::error::Error for PingError {}

impl From<std::io::Error> for PingError {
    fn from(e: std::io::Error) -> Self {
        PingError::Io(e)
    }
}
impl From<serde_json::Error> for PingError {
    fn from(e: serde_json::Error) -> Self {
        PingError::Json(e)
    }
}

/// 用默认选项 ping。`address` 形如 `host`、`host:port`、`1.2.3.4`、`[::1]:25565`。
pub async fn ping(address: &str) -> Result<PingResult, PingError> {
    ping_with(address, &PingOptions::default()).await
}

/// 异步 ping(tokio)。先现代 ping;失败且允许时回退旧版(0xFE)ping。
pub async fn ping_with(address: &str, opts: &PingOptions) -> Result<PingResult, PingError> {
    let (host_in, port_in, explicit) = parse_address(address)?;
    let (chost, cport, hhost, via_srv) = resolve_target(&host_in, explicit, port_in, opts).await;
    let addr = ResolvedAddress { host: hhost.clone(), port: cport, via_srv };

    match ping_modern(&chost, cport, &hhost, opts, addr.clone()).await {
        Ok(r) => Ok(r),
        Err(modern_err) if opts.allow_legacy => {
            match legacy::ping_legacy(&chost, cport, opts.timeout).await {
                Ok((ls, latency)) => Ok(from_legacy(ls, latency, addr)),
                Err(_) => Err(modern_err), // 旧版也不行就回报现代的错
            }
        }
        Err(e) => Err(e),
    }
}

/// 以指定版本的协议去 ping。`version` 接受 `"1.8"`/`"1.12.2"`/`"1.16"`/`"26.1"` 这类版本名,或直接给协议号
/// 字符串(如 `"47"`)。认不出的版本退回 -1(「只是 ping」)。握手字节随版本不变,只是换了那个协议号。
pub async fn ping_as(address: &str, version: &str, opts: &PingOptions) -> Result<PingResult, PingError> {
    let mut o = opts.clone();
    o.protocol_version = crate::minecraft::versions::protocol_for(version).unwrap_or(-1);
    ping_with(address, &o).await
}

/// 两段式 ping:先按 `opts` 探一次;若对端是 ViaProxy 这类「对未知客户端(含默认 -1)回自造占位 status」
/// 的独立代理,就换一个它**认得**的协议号再探,以穿透到后端真实状态。普通服务器与挂 Via 的服务器一次即得。
///
/// 选哪个协议号回探很关键:ViaProxy 的占位包**不携带**任何「要求的版本/区间」(它在连后端之前就回了
/// `{name:ViaProxy, protocol:-1}`),所以拿不到服务端要求的版本。而 ViaProxy 注册的是「最老→自身构建版本」的
/// 连续区间且会翻译任意已注册版本到后端,故回探只要用它认得的号即可——**绝不**无脑用我们的最新版(更新的协议号
/// 旧 ViaProxy 没注册,会和 -1 一样被踢)。策略:占位包若竟带 `supportedVersions` 就用其最大值;否则按
/// [`PROXY_PROBE_LADDER`](crate::minecraft::versions::PROXY_PROBE_LADDER) 从较新稳定版往老试,命中(非占位)即返回。
pub async fn ping_resolved(address: &str, opts: &PingOptions) -> Result<PingResult, PingError> {
    use crate::minecraft::versions;

    let first = ping_with(address, opts).await?;
    if !is_proxy_placeholder(&first.status) {
        return Ok(first);
    }

    // 候选协议号:占位若带 supportedVersions 取其最大(理论上 ViaProxy 占位不带,但带了就优先用),其后接阶梯。
    let mut candidates: Vec<i32> = Vec::new();
    if let Some(&m) = first.status.version.supported_versions.iter().max() {
        candidates.push(m);
    }
    candidates.extend(versions::PROXY_PROBE_LADDER.iter().copied());
    candidates.dedup();

    for proto in candidates {
        let mut o = opts.clone();
        o.protocol_version = proto;
        if let Ok(r) = ping_with(address, &o).await
            && !is_proxy_placeholder(&r.status)
        {
            return Ok(r); // 穿透成功,拿到后端真实 status
        }
    }
    Ok(first) // 都没穿透:如实返回占位包,不臆造
}

/// ViaProxy 对未注册协议(含我们默认 -1)的握手会回自造占位 status:`version.name == "ViaProxy"` 且
/// protocol < 0(占位恒为 -1)、0/0 在线、MOTD 是「not supported」。真实服务器即便 MOTD 里叫 ViaProxy,
/// 其 protocol 也是正常号,故加 protocol<0 收紧,避免误判。
fn is_proxy_placeholder(status: &StatusResponse) -> bool {
    status.version.name.as_deref() == Some("ViaProxy") && status.version.protocol < 0
}

async fn ping_modern(
    chost: &str,
    cport: u16,
    hhost: &str,
    opts: &PingOptions,
    addr: ResolvedAddress,
) -> Result<PingResult, PingError> {
    let connect = timeout(opts.timeout, TokioStream::connect((chost, cport)));
    let stream = connect.await.map_err(|_| PingError::Timeout)?.map_err(PingError::Io)?;

    let fut = do_ping_async(stream, hhost, cport, opts);
    let (latency, raw_json, nyf) = timeout(opts.timeout, fut).await.map_err(|_| PingError::Timeout)??;

    let mut status = parse_status(&raw_json)?;
    apply_nyf_modpack(&mut status, nyf);
    Ok(PingResult { latency, status, raw_json, address: addr, is_legacy: false })
}

/// 把旧版 ping 结果映射成统一的 [`PingResult`](现代 [`StatusResponse`] 形状)。
fn from_legacy(ls: legacy::LegacyStatus, latency: Duration, addr: ResolvedAddress) -> PingResult {
    let players = if ls.online >= 0 || ls.max >= 0 {
        Some(Players { max: ls.max, online: ls.online, sample: Vec::new() })
    } else {
        None
    };
    let status = StatusResponse {
        version: Version {
            name: (!ls.version.is_empty()).then_some(ls.version),
            protocol: ls.protocol,
            supported_versions: Vec::new(),
        },
        players,
        description: Component::text(ls.motd),
        favicon: None,
        enforces_secure_chat: None,
        previews_chat: None,
        prevents_chat_reports: None,
        is_modded: None,
        modinfo: None,
        forge_data: None,
        modpack: None,
    };
    PingResult { latency: Some(latency), status, raw_json: String::new(), address: addr, is_legacy: true }
}

/// Nyf's Modpack Version Check 把整合包版本塞在 Pong 包尾(不在状态 JSON 里)。仅当状态里没有
/// BCC 整合包信息时,才用它补一个「只有版本、无名」的 [`Modpack`]。
fn apply_nyf_modpack(status: &mut StatusResponse, nyf_version: Option<String>) {
    if status.modpack.is_none()
        && let Some(version) = nyf_version.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    {
        status.modpack = Some(Modpack { name: String::new(), version });
    }
}

/// 同步 ping(std,不查 SRV)。给不跑 tokio 的场合用。
pub fn ping_sync(address: &str, opts: &PingOptions) -> Result<PingResult, PingError> {
    let (host, port, _explicit) = parse_address(address)?;

    let target = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| PingError::Address(format!("{host} 无法解析出地址")))?;
    let mut stream = TcpStream::connect_timeout(&target, opts.timeout)?;
    stream.set_read_timeout(Some(opts.timeout))?;
    stream.set_write_timeout(Some(opts.timeout))?;

    let (latency, raw_json, nyf) = do_ping_sync(&mut stream, &host, port, opts)?;
    let mut status: StatusResponse = parse_status(&raw_json)?;
    apply_nyf_modpack(&mut status, nyf);
    Ok(PingResult {
        latency,
        status,
        raw_json,
        address: ResolvedAddress { host, port, via_srv: false },
        is_legacy: false,
    })
}

fn parse_status(raw: &str) -> Result<StatusResponse, PingError> {
    let mut de = serde_json::Deserializer::from_str(raw);
    StatusResponse::deserialize(&mut de).map_err(PingError::Json)
}

async fn do_ping_async(
    mut s: TokioStream,
    host: &str,
    port: u16,
    opts: &PingOptions,
) -> Result<(Option<Duration>, String, Option<String>), PingError> {
    let mut hello = codec::handshake(opts.protocol_version, host, port, 1);
    hello.extend_from_slice(&codec::status_request());
    s.write_all(&hello).await.map_err(PingError::Io)?;
    s.flush().await.map_err(PingError::Io)?;

    let frame = read_frame_async(&mut s, MAX_FRAME).await?;
    let json = parse_status_frame(&frame)?;

    let (latency, nyf) = if opts.measure_latency {
        match measure_latency_async(&mut s).await {
            Ok((d, n)) => (Some(d), n),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    Ok((latency, json, nyf))
}

fn do_ping_sync(
    s: &mut TcpStream,
    host: &str,
    port: u16,
    opts: &PingOptions,
) -> Result<(Option<Duration>, String, Option<String>), PingError> {
    let mut hello = codec::handshake(opts.protocol_version, host, port, 1);
    hello.extend_from_slice(&codec::status_request());
    s.write_all(&hello)?;
    s.flush()?;

    let frame = read_frame_sync(s, MAX_FRAME)?;
    let json = parse_status_frame(&frame)?;

    let (latency, nyf) = if opts.measure_latency {
        match measure_latency_sync(s) {
            Ok((d, n)) => (Some(d), n),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    Ok((latency, json, nyf))
}

/// 校验 Status Response 帧(包 ID 须为 0x00),取出 JSON 串。
fn parse_status_frame(frame: &[u8]) -> Result<String, PingError> {
    let mut r = codec::Reader::new(frame);
    let id = r.read_varint()?;
    if id != 0x00 {
        return Err(PingError::Protocol(format!("状态响应包 ID 应为 0,得到 {id}")));
    }
    let json = r.read_string(STATUS_JSON_MAX)?;
    Ok(json)
}

/// 测一轮 Ping/Pong 延迟,并捎回 Nyf's 在 Pong 尾部追加的整合包版本(若有)。
async fn measure_latency_async(s: &mut TokioStream) -> Result<(Duration, Option<String>), PingError> {
    let start = Instant::now();
    s.write_all(&codec::ping_request(PING_PAYLOAD)).await.map_err(PingError::Io)?;
    s.flush().await.map_err(PingError::Io)?;
    let frame = read_frame_async(s, PONG_FRAME_MAX).await?;
    let elapsed = start.elapsed();
    let nyf = verify_pong(&frame)?;
    Ok((elapsed, nyf))
}

fn measure_latency_sync(s: &mut TcpStream) -> Result<(Duration, Option<String>), PingError> {
    let start = Instant::now();
    s.write_all(&codec::ping_request(PING_PAYLOAD))?;
    s.flush()?;
    let frame = read_frame_sync(s, PONG_FRAME_MAX)?;
    let elapsed = start.elapsed();
    let nyf = verify_pong(&frame)?;
    Ok((elapsed, nyf))
}

/// 校验 Pong(包 ID 0x01 + 原样回显的 Long),并取 Nyf's Modpack Version Check 在标准载荷之后
/// 追加的整合包版本字符串(若有)。返回 `Some(version)` 仅当尾部确有非空串。
fn verify_pong(frame: &[u8]) -> Result<Option<String>, PingError> {
    let mut r = codec::Reader::new(frame);
    let id = r.read_varint()?;
    if id != 0x01 {
        return Err(PingError::Protocol(format!("pong 包 ID 应为 1,得到 {id}")));
    }
    let echoed = r.read_i64()?;
    if echoed != PING_PAYLOAD {
        return Err(PingError::Protocol("pong 载荷不匹配".into()));
    }
    // Nyf's:Long 之后追加 writeUtf(modpackVersion) + writeUtf(serverIP),取第一段当版本。
    let nyf = if r.remaining() > 0 {
        r.read_string(256).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };
    Ok(nyf)
}

async fn read_frame_async(
    s: &mut TokioStream,
    max: usize,
) -> Result<Vec<u8>, PingError> {
    let len = read_varint_async(s).await?;
    let len = usize::try_from(len).map_err(|_| PingError::Protocol("帧长为负".into()))?;
    if len == 0 || len > max {
        return Err(PingError::Protocol(format!("帧长越界: {len}")));
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf).await.map_err(PingError::Io)?;
    Ok(buf)
}

fn read_frame_sync(s: &mut TcpStream, max: usize) -> Result<Vec<u8>, PingError> {
    let len = read_frame_len_sync(s)?;
    let len = usize::try_from(len).map_err(|_| PingError::Protocol("帧长为负".into()))?;
    if len == 0 || len > max {
        return Err(PingError::Protocol(format!("帧长越界: {len}")));
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// 帧长前缀:原版 `Varint21FrameDecoder` 只允许最多 3 字节(21 位)。
const FRAME_LEN_MAX_BYTES: usize = 3;

async fn read_varint_async(s: &mut TokioStream) -> Result<i32, PingError> {
    let mut val: i32 = 0;
    for i in 0..FRAME_LEN_MAX_BYTES {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).await.map_err(PingError::Io)?;
        val |= ((b[0] & 0x7F) as i32) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(val);
        }
    }
    Err(PingError::Protocol("帧长 VarInt 超过 21 位".into()))
}

fn read_frame_len_sync(s: &mut TcpStream) -> Result<i32, PingError> {
    let mut val: i32 = 0;
    for i in 0..FRAME_LEN_MAX_BYTES {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).map_err(PingError::Io)?;
        val |= ((b[0] & 0x7F) as i32) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(val);
        }
    }
    Err(PingError::Protocol("帧长 VarInt 超过 21 位".into()))
}

/// 解析地址成 `(host, port, 是否显式给了端口)`。支持 `host`、`host:port`、`[ipv6]`、`[ipv6]:port`、裸 IPv6。
fn parse_address(addr: &str) -> Result<(String, u16, bool), PingError> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err(PingError::Address("地址为空".into()));
    }
    if let Some(rest) = addr.strip_prefix('[') {
        let (ip, tail) =
            rest.split_once(']').ok_or_else(|| PingError::Address("IPv6 缺 ]".into()))?;
        if let Some(p) = tail.strip_prefix(':') {
            let port = p.parse().map_err(|_| PingError::Address("端口非法".into()))?;
            return Ok((ip.to_string(), port, true));
        }
        return Ok((ip.to_string(), DEFAULT_PORT, false));
    }
    if addr.matches(':').count() == 1 {
        let (h, p) = addr.split_once(':').unwrap();
        let port = p.parse().map_err(|_| PingError::Address("端口非法".into()))?;
        return Ok((h.to_string(), port, true));
    }
    // 无冒号(域名 / IPv4)或多冒号(裸 IPv6):都视作没给端口
    Ok((addr.to_string(), DEFAULT_PORT, false))
}

/// 异步路径的目标解析:符合条件就查 SRV,命中则整体重定向并把 target 填进握手地址。
/// 返回 `(连接 host, 连接 port, 握手 host, 是否走了 SRV)`。
async fn resolve_target(
    host: &str,
    explicit_port: bool,
    port: u16,
    opts: &PingOptions,
) -> (String, u16, String, bool) {
    if opts.use_srv
        && !explicit_port
        && host.parse::<IpAddr>().is_err()
        && let Some((target, srv_port)) = srv::resolve_srv(host, opts.timeout).await
    {
        return (target.clone(), srv_port, target, true);
    }
    (host.to_string(), port, host.to_string(), false)
}
