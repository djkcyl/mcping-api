//! GameSpy4 / UT3 Query 协议(UDP)—— 拿 SLP 拿不到的:**完整在线玩家名单、插件列表、地图、
//! gametype**。需服务端 `enable-query=true`(默认关,所以多数公网服没开)。
//!
//! 流程:① 握手 `FE FD 09 <sessionId:i32>` → 服务器回 `09 <session> <token 字符串>\0`,token 解析成
//! i32。② full stat 请求 `FE FD 00 <session> <token:i32> 00 00 00 00`(末尾 4 字节填充表示要完整数据)
//! → 响应:`00 <session> "splitnum\0\x80\0"(11)` + 若干 `key\0value\0`(空 key 收尾)+ `\x01player_\0\0`
//! + 若干 `name\0`(空收尾)。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::minecraft::protocol::PingError;

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub motd: String,
    pub gametype: String,
    pub version: String,
    /// 形如 `Paper on 1.21.11: PluginA v1.0; PluginB v2.0`。
    pub plugins: String,
    pub map: String,
    pub online: i64,
    pub max: i64,
    pub host_ip: String,
    pub host_port: u16,
    /// 完整在线玩家名(SLP 的 sample 通常只给一部分且可伪造)。
    pub players: Vec<String>,
    /// 全部原始 K/V(留底)。
    pub kv: BTreeMap<String, String>,
    pub latency: Duration,
}

const MAGIC: [u8; 2] = [0xFE, 0xFD];
const SESSION: i32 = 1;

/// 对 `host:port` 做一次 full-stat Query。
pub async fn query(host: &str, port: u16, t: Duration) -> Result<QueryResult, PingError> {
    let start = Instant::now();
    let sock = UdpSocket::bind("0.0.0.0:0").await.map_err(PingError::Io)?;
    sock.connect((host, port)).await.map_err(PingError::Io)?;

    // ① 握手取 challenge token
    let mut hs = Vec::with_capacity(7);
    hs.extend_from_slice(&MAGIC);
    hs.push(0x09);
    hs.extend_from_slice(&SESSION.to_be_bytes());
    sock.send(&hs).await.map_err(PingError::Io)?;

    let mut buf = vec![0u8; 16 * 1024];
    let n = recv(&sock, &mut buf, t).await?;
    if n < 6 || buf[0] != 0x09 {
        return Err(PingError::Protocol("query 握手响应异常".into()));
    }
    let (token_str, _) = read_cstr(&buf[5..n]);
    let token: i32 =
        token_str.trim().parse().map_err(|_| PingError::Protocol("challenge token 非整数".into()))?;

    // ② full stat
    let mut req = Vec::with_capacity(15);
    req.extend_from_slice(&MAGIC);
    req.push(0x00);
    req.extend_from_slice(&SESSION.to_be_bytes());
    req.extend_from_slice(&token.to_be_bytes());
    req.extend_from_slice(&[0, 0, 0, 0]); // 末尾填充 ⇒ full stat
    sock.send(&req).await.map_err(PingError::Io)?;

    let n = recv(&sock, &mut buf, t).await?;
    let latency = start.elapsed();
    parse_full(&buf[..n], latency)
}

async fn recv(sock: &UdpSocket, buf: &mut [u8], t: Duration) -> Result<usize, PingError> {
    timeout(t, sock.recv(buf)).await.map_err(|_| PingError::Timeout)?.map_err(PingError::Io)
}

fn read_cstr(b: &[u8]) -> (String, usize) {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    (String::from_utf8_lossy(&b[..end]).into_owned(), (end + 1).min(b.len() + 1))
}

fn parse_full(resp: &[u8], latency: Duration) -> Result<QueryResult, PingError> {
    // 头:type(1) + session(4) + "splitnum\0\x80\0"(11) = 16 字节
    if resp.len() < 16 || resp[0] != 0x00 {
        return Err(PingError::Protocol("query full-stat 响应异常".into()));
    }
    let body = &resp[16..];

    // K/V 段:key\0value\0 …,空 key 收尾
    let mut kv = BTreeMap::new();
    let mut pos = 0;
    while pos < body.len() {
        let (key, adv) = read_cstr(&body[pos..]);
        pos += adv;
        if key.is_empty() {
            break;
        }
        let (val, adv2) = read_cstr(&body[pos..]);
        pos += adv2;
        kv.insert(key, val);
    }

    let players = parse_players(&body[pos.min(body.len())..]);

    let g = |k: &str| kv.get(k).cloned().unwrap_or_default();
    Ok(QueryResult {
        motd: g("hostname"),
        gametype: g("gametype"),
        version: g("version"),
        plugins: g("plugins"),
        map: g("map"),
        online: g("numplayers").trim().parse().unwrap_or(-1),
        max: g("maxplayers").trim().parse().unwrap_or(-1),
        host_ip: g("hostip"),
        host_port: g("hostport").trim().parse().unwrap_or(0),
        players,
        kv,
        latency,
    })
}

/// 玩家段:`\x01player_\0\0` + `name\0`…(空收尾)。锚定带 0x01 前缀的完整定界,避免 K/V 值里恰好
/// 含 "player_" 时误切。
fn parse_players(rest: &[u8]) -> Vec<String> {
    const MARKER: &[u8] = b"\x01player_\x00";
    let start = rest
        .windows(MARKER.len())
        .position(|w| w == MARKER)
        .map(|i| i + MARKER.len())
        .unwrap_or(rest.len());
    let mut p = &rest[start..];
    if p.first() == Some(&0) {
        p = &p[1..]; // 跳过段头的第二个 \0
    }
    let mut players = Vec::new();
    let mut pos = 0;
    while pos < p.len() {
        let (name, adv) = read_cstr(&p[pos..]);
        pos += adv;
        if name.is_empty() {
            break;
        }
        players.push(name);
    }
    players
}
