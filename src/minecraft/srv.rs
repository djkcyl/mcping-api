//! `_minecraft._tcp` SRV 解析 —— 手搓的极简 DNS over UDP,不引解析器依赖。
//!
//! MC 客户端连域名时先查 `_minecraft._tcp.<host>` 的 SRV:命中即整体重定向(连记录里的
//! target:port,并在握手的地址字段填 target)。这里只做 UDP 单包查询、取优先级最低的一条;
//! 全程尽力而为,任何失败都返回 `None`,由上层退回直连。

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

/// 查 `_minecraft._tcp.<host>` 的 SRV,返回 `(target, port)`。
pub async fn resolve_srv(host: &str, query_timeout: Duration) -> Option<(String, u16)> {
    let server = nameserver();
    let qname = format!("_minecraft._tcp.{host}");
    let query = build_query(&qname);

    let bind: SocketAddr =
        if server.is_ipv6() { "[::]:0".parse().ok()? } else { "0.0.0.0:0".parse().ok()? };
    let sock = UdpSocket::bind(bind).await.ok()?;
    sock.connect(SocketAddr::new(server, 53)).await.ok()?;
    sock.send(&query).await.ok()?;

    let mut buf = [0u8; 512];
    let n = timeout(query_timeout, sock.recv(&mut buf)).await.ok()?.ok()?;
    parse_srv(&buf[..n])
}

/// 取 `/etc/resolv.conf` 第一个 `nameserver`,取不到退到 1.1.1.1。
fn nameserver() -> IpAddr {
    if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in content.lines() {
            if let Some(rest) = line.trim().strip_prefix("nameserver")
                && let Ok(ip) = rest.trim().parse::<IpAddr>()
            {
                return ip;
            }
        }
    }
    IpAddr::from([1, 1, 1, 1])
}

fn build_query(qname: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(qname.len() + 18);
    q.extend_from_slice(&[0x12, 0x34]); // 事务 ID(任意)
    q.extend_from_slice(&[0x01, 0x00]); // flags:RD=1
    q.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    q.extend_from_slice(&[0x00, 0x00]); // ANCOUNT
    q.extend_from_slice(&[0x00, 0x00]); // NSCOUNT
    q.extend_from_slice(&[0x00, 0x00]); // ARCOUNT
    for label in qname.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            continue;
        }
        q.push(bytes.len() as u8);
        q.extend_from_slice(bytes);
    }
    q.push(0); // 根标签
    q.extend_from_slice(&[0x00, 0x21]); // QTYPE=SRV(33)
    q.extend_from_slice(&[0x00, 0x01]); // QCLASS=IN
    q
}

fn parse_srv(msg: &[u8]) -> Option<(String, u16)> {
    if msg.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let an = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    if an == 0 {
        return None;
    }
    let mut pos = 12;
    for _ in 0..qd {
        pos = skip_name(msg, pos)?;
        pos = pos.checked_add(4)?; // QTYPE + QCLASS
    }

    let mut best: Option<(u16, String, u16)> = None; // (priority, target, port)
    for _ in 0..an {
        pos = skip_name(msg, pos)?;
        if pos + 10 > msg.len() {
            break;
        }
        let rtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
        let rdlen = u16::from_be_bytes([msg[pos + 8], msg[pos + 9]]) as usize;
        let rdata = pos + 10;
        if rdata + rdlen > msg.len() {
            break;
        }
        if rtype == 33 && rdlen >= 6 {
            let priority = u16::from_be_bytes([msg[rdata], msg[rdata + 1]]);
            let port = u16::from_be_bytes([msg[rdata + 4], msg[rdata + 5]]);
            if let Some((target, _)) = read_name(msg, rdata + 6)
                && !target.is_empty()
                && best.as_ref().is_none_or(|(bp, ..)| priority < *bp)
            {
                best = Some((priority, target, port));
            }
        }
        pos = rdata + rdlen;
    }
    best.map(|(_, t, p)| (t, p))
}

/// 读 DNS 名(处理 0xC0 压缩指针),返回 `(名字, 顶层游标后位置)`。
fn read_name(msg: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut pos = start;
    let mut next_after: Option<usize> = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos)?;
        if len & 0xC0 == 0xC0 {
            let b2 = *msg.get(pos + 1)?;
            let ptr = (((len & 0x3F) as usize) << 8) | b2 as usize;
            next_after.get_or_insert(pos + 2);
            pos = ptr;
            jumps += 1;
            if jumps > 64 {
                return None; // 防指针成环
            }
            continue;
        }
        if len == 0 {
            pos += 1;
            break;
        }
        let s = pos + 1;
        let e = s + len as usize;
        if e > msg.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&msg[s..e]).into_owned());
        pos = e;
    }
    Some((labels.join("."), next_after.unwrap_or(pos)))
}

fn skip_name(msg: &[u8], start: usize) -> Option<usize> {
    read_name(msg, start).map(|(_, next)| next)
}
