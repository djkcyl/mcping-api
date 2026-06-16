//! 解码 Forge `forgeData.d` 打包块(FML3/优化编码),取出模组列表。
//!
//! 编码两层(对照 Forge `ServerStatusPing.encodeOptimized`),解码即逆这两层。
//!
//! 字节层 `toBuf`:bool(truncated) + short(模组数) + 每模组「varint(频道数<<1|ignoreServerOnly) + utf(modId) + (utf(version) 仅当未置该位) + 每频道 utf/utf/bool」;之后 varint(非模组频道数) + 每个 utf/utf/bool。
//!
//! 字符层 `encodeOptimized`:字节按每字符 15 位塞进 UTF-16(每字符 0..0x7FFF,避代理区);头两字符为字节长度 `len&0x7FFF`、`(len>>15)&0x7FFF`。
//!
//! 尽力而为:任何一步出错返回 `None`,绝不 panic。

use crate::minecraft::codec;

#[derive(Debug, Clone)]
pub struct ForgeMods {
    /// (modId, version);version 可能为空(ignoreServerOnly 的模组不带版本)。
    pub mods: Vec<(String, String)>,
    /// 频道总数(各模组频道 + 非模组频道)。
    pub channels: usize,
    pub truncated: bool,
}

/// 解码 `forgeData.d`。
pub fn decode_forge_d(d: &str) -> Option<ForgeMods> {
    let bytes = unpack15(d)?;
    parse_buf(&bytes)
}

/// 逆 15 位/字符打包,还原原始字节。
fn unpack15(d: &str) -> Option<Vec<u8>> {
    let chars: Vec<u32> = d.chars().map(|c| c as u32 & 0x7FFF).collect();
    if chars.len() < 2 {
        return None;
    }
    let byte_len = chars[0] as usize | ((chars[1] as usize) << 15);
    if byte_len > 8 * 1024 * 1024 {
        return None;
    }
    let mut out = Vec::with_capacity(byte_len.min(1 << 20));
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &c in &chars[2..] {
        if out.len() >= byte_len {
            break; // 读满即停,避免 bits 无界增长导致 `c << bits` 在 debug 下溢出 panic
        }
        buffer |= c << bits;
        bits += 15;
        while bits >= 8 && out.len() < byte_len {
            out.push((buffer & 0xFF) as u8);
            buffer >>= 8;
            bits -= 8;
        }
    }
    Some(out)
}

/// 解析 `toBuf` 字节布局。频道 `version` 的线缆类型随 Forge 世代变:1.18/1.19 是 UTF 串,
/// 1.20.x / 1.21+ 改成 VarInt。两种都试,取「无错且正好读到缓冲末尾」的那条(都没读满就取模组更多的)。
fn parse_buf(bytes: &[u8]) -> Option<ForgeMods> {
    let as_varint = parse_with(bytes, true);
    if matches!(&as_varint, Some((_, true))) {
        return as_varint.map(|(m, _)| m);
    }
    let as_utf = parse_with(bytes, false);
    if matches!(&as_utf, Some((_, true))) {
        return as_utf.map(|(m, _)| m);
    }
    match (as_varint, as_utf) {
        (Some((a, _)), Some((b, _))) => Some(if a.mods.len() >= b.mods.len() { a } else { b }),
        (Some((a, _)), None) => Some(a),
        (None, other) => other.map(|(m, _)| m),
    }
}

/// 读一个频道 version:VarInt(新版)或 UTF 串(旧版)。
fn read_chan_version(r: &mut codec::Reader, varint: bool) -> std::io::Result<()> {
    if varint {
        r.read_varint().map(|_| ())
    } else {
        r.read_string(32767).map(|_| ())
    }
}

/// 按指定的频道 version 类型解析一遍;返回 (结果, `full`=有没有干净地读到缓冲末尾)。
///
/// **容错而非全有全无**:坏频道 version 类型会在某个 mod 的频道段把流读错位,但已读到的 mod
/// (id+version)是有价值的;真机 d 还可能因 15 位打包丢掉末字节,导致结尾那个「非模组频道数」
/// 读到 EOF。这些情形都只「停下、保留已得」,绝不丢弃整份结果。选型交给 [`parse_buf`]:正确的
/// version 类型会干净读满(`full=true`),错的会提前错位(`full=false`),据此取优。
fn parse_with(bytes: &[u8], chan_ver_varint: bool) -> Option<(ForgeMods, bool)> {
    let mut r = codec::Reader::new(bytes);
    let truncated = r.read_u8().ok()? != 0;
    let mod_count = (r.read_i16().ok()? as u16) as usize; // 原版 readUnsignedShort
    if mod_count > 8192 {
        return None;
    }
    let mut mods = Vec::with_capacity(mod_count.min(1024));
    let mut channels = 0usize;
    let mut clean = true; // 全程无读取错误才算干净

    'mods: for _ in 0..mod_count {
        let Ok(flag) = r.read_varint() else {
            clean = false;
            break;
        };
        let chan = (flag >> 1).max(0) as usize;
        let ignore_server_only = (flag & 1) != 0;
        let Ok(mod_id) = r.read_string(32767) else {
            clean = false;
            break;
        };
        // 非 ignoreServerOnly 才带 version
        let version = if ignore_server_only {
            String::new()
        } else {
            match r.read_string(32767) {
                Ok(v) => v,
                Err(_) => {
                    clean = false;
                    break;
                }
            }
        };
        mods.push((mod_id, version)); // 先记下这个 mod,频道读错也不丢它
        for _ in 0..chan.min(4096) {
            if r.read_string(32767).is_err() // 频道 path
                || read_chan_version(&mut r, chan_ver_varint).is_err() // version(VarInt / UTF)
                || r.read_u8().is_err()
            // required
            {
                clean = false;
                break 'mods;
            }
            channels += 1;
        }
    }

    // 非模组频道(尾部)。15 位打包可能丢了这个计数字节,读到 EOF 属正常,停即可。
    if clean && let Ok(extra) = r.read_varint() {
        for _ in 0..(extra.max(0) as usize).min(8192) {
            if r.read_string(32767).is_err()
                || read_chan_version(&mut r, chan_ver_varint).is_err()
                || r.read_u8().is_err()
            {
                break;
            }
            channels += 1;
        }
    }

    let full = clean && r.remaining() == 0;
    Some((ForgeMods { mods, channels, truncated }, full))
}
