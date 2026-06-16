//! 现代 SLP 线缆编解码 —— 不碰 I/O,只在 `Vec<u8>` 与 `&[u8]` 上读写。
//!
//! 把字节构造/解析与网络收发分开:同步(std)和异步(tokio)两条 ping 路径共用这里,
//! 全部逻辑也就能脱网验证。状态态(Status)不压缩、不加密,所以包帧永远是裸格式:
//! `VarInt 长度`(= 包 ID + 数据的字节数)→ `VarInt 包 ID` → 数据。
//!
//! VarInt/VarLong 是 7 位一组、低位组在前、最高位为续接位的变长整数,**按补码**存负数
//! (不是 zig-zag),所以负数恒占满长(VarInt 5 字节 / VarLong 10 字节)。除 VarInt/VarLong
//! 外的多字节数值一律大端。

use std::io;

const SEGMENT: u8 = 0x7F;
const CONTINUE: u8 = 0x80;

/// VarInt 最多 5 字节,VarLong 最多 10 字节;超出即判损坏。
pub const MAX_VARINT_BYTES: usize = 5;
pub const MAX_VARLONG_BYTES: usize = 10;

/// 写一个 VarInt。`value as u32` 是关键:对无符号做逻辑右移,负数才会按补码占满 5 字节
/// (对有符号算术右移会死循环 / 编错)。
pub fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut u = value as u32;
    loop {
        let mut b = (u & SEGMENT as u32) as u8;
        u >>= 7;
        if u != 0 {
            b |= CONTINUE;
        }
        buf.push(b);
        if u == 0 {
            break;
        }
    }
}

/// 写一个 VarLong(同 VarInt,宽到 64 位、上限 10 字节)。
pub fn write_varlong(buf: &mut Vec<u8>, value: i64) {
    let mut u = value as u64;
    loop {
        let mut b = (u & SEGMENT as u64) as u8;
        u >>= 7;
        if u != 0 {
            b |= CONTINUE;
        }
        buf.push(b);
        if u == 0 {
            break;
        }
    }
}

/// 写 String:VarInt 字节长前缀 + 标准 UTF-8(非 Java 改良 UTF-8)。
pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as i32);
    buf.extend_from_slice(s.as_bytes());
}

/// 写无符号短整数(端口),大端 2 字节。
pub fn write_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// 写 Long(ping 载荷),大端 8 字节。
pub fn write_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// 把一个包载荷裹成线缆帧:`VarInt(len) + VarInt(id) + payload`,`len` 为后两段字节数。
pub fn packet(id: i32, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(payload.len() + 1);
    write_varint(&mut inner, id);
    inner.extend_from_slice(payload);

    let mut out = Vec::with_capacity(inner.len() + MAX_VARINT_BYTES);
    write_varint(&mut out, inner.len() as i32);
    out.extend_from_slice(&inner);
    out
}

/// Handshake(包 ID 0x00,Handshaking 态):协议号 + 服务器地址 + 端口 + 下一状态。
/// `next` 用 1 进 Status 态。协议号惯例发 -1 表示「只是 ping」,严格的服务器更认真实号。
pub fn handshake(protocol: i32, host: &str, port: u16, next: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, protocol);
    write_string(&mut p, host);
    write_u16(&mut p, port);
    write_varint(&mut p, next);
    packet(0x00, &p)
}

/// Status Request(包 ID 0x00,无字段)。
pub fn status_request() -> Vec<u8> {
    packet(0x00, &[])
}

/// Ping Request(包 ID 0x01,Long 载荷);服务器回 Pong 原样回显该 Long。
pub fn ping_request(payload: i64) -> Vec<u8> {
    let mut p = Vec::new();
    write_i64(&mut p, payload);
    packet(0x01, &p)
}

/// 在一段已收齐的字节上顺序取值的游标 —— 用来解析单个包体。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| bad("长度溢出"))?;
        let slice = self.buf.get(self.pos..end).ok_or_else(|| bad("包体读越界"))?;
        self.pos = end;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// 还剩多少字节未读。
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn read_varint(&mut self) -> io::Result<i32> {
        let mut val: i32 = 0;
        for i in 0..MAX_VARINT_BYTES {
            let b = self.read_u8()?;
            val |= ((b & SEGMENT) as i32) << (7 * i);
            if b & CONTINUE == 0 {
                return Ok(val);
            }
        }
        Err(bad("VarInt 过长"))
    }

    pub fn read_i16(&mut self) -> io::Result<i16> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes(bytes.try_into().unwrap()))
    }

    pub fn read_i64(&mut self) -> io::Result<i64> {
        let bytes = self.take(8)?;
        Ok(i64::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// 读 String:VarInt 字节长 + UTF-8。长度做上界保护,避免坏服务器索要天量内存。
    pub fn read_string(&mut self, max_bytes: usize) -> io::Result<String> {
        let len = self.read_varint()?;
        let len = usize::try_from(len).map_err(|_| bad("字符串长为负"))?;
        if len > max_bytes {
            return Err(bad("字符串超长"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| bad("字符串非合法 UTF-8"))
    }
}

pub(crate) fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
