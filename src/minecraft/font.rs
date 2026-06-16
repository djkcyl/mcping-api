//! 字形来源 —— 复刻原版字体栈:默认位图字体(`ascii`/`accented`/`nonlatin_european` + 空格)
//! 命中则用,否则回退 Unifont(CJK/unicode)。
//!
//! 默认字体从 `assets/minecraft/mcfont.bin`(MCF2,由原版 26.1.2 的字体资源烘焙)加载,每个字形
//! 带自己的步进(advance)、位图、ascent、高度 —— 这是原版那套变宽像素字体,空格步进 4px,所以
//! 服务器用空格做的伪居中能照原样对齐。Unifont 从 `unifont_bmp.bin`(16px)加载,去掉左右留白后
//! 作回退;渲染层按半尺寸画并以 ascent 对齐基线,使其与默认字体共线。
//!
//! 度量单位:`advance` 一律是原版 GUI 像素(Unihex 的步进 = 字宽/2+1,原版口径);`width`/
//! `height`/`ascent` 是字形原生像素(默认字体 = GUI 像素;Unifont = 16px 网格)。渲染层用
//! `unifont` 标志决定每像素画多大块(默认 = scale,Unifont = scale/2,即原版 oversample 2 的半尺寸),
//! 从而 advance(×scale)与位图(×block)、基线自动对齐。优先级:默认字体 → Unifont → 豆腐块。

use std::collections::HashMap;
use std::sync::OnceLock;

/// 一个像素字形。度量为字形原生像素(见模块说明)。`rows[r]` 第 15 位 = 最左列。
#[derive(Clone, Copy)]
pub struct Glyph {
    /// 步进宽度(原生像素)。
    pub advance: u8,
    /// 位图宽。
    pub width: u8,
    /// 位图高。
    pub height: u8,
    /// 顶到基线的像素数。
    pub ascent: u8,
    /// 是否来自 Unifont 回退(渲染层据此用半尺寸块、对齐基线)。
    pub unifont: bool,
    pub rows: [u16; 16],
}

impl Glyph {
    #[inline]
    pub fn pixel(&self, col: u32, row: u32) -> bool {
        if col >= self.width as u32 || row >= self.height as u32 {
            return false;
        }
        (self.rows[row as usize] >> (15 - col)) & 1 == 1
    }
}

/// 字形来源:给字符返回字形(取不到给 `None`)。
pub trait GlyphSource {
    fn glyph(&self, ch: char) -> Option<Glyph>;
}

/// 原版字体栈:默认位图字体 + Unifont 回退。
pub struct VanillaFont {
    default: HashMap<char, Glyph>,
    unifont: HashMap<char, Glyph>,
}

impl GlyphSource for VanillaFont {
    fn glyph(&self, ch: char) -> Option<Glyph> {
        self.default.get(&ch).or_else(|| self.unifont.get(&ch)).copied()
    }
}

static MCFONT: &[u8] = include_bytes!("../../assets/minecraft/mcfont.bin");
static UNIFONT: &[u8] = include_bytes!("../../assets/minecraft/unifont_bmp.bin");

/// 进程内只解析一次的全局原版字体。
pub fn font() -> &'static VanillaFont {
    static F: OnceLock<VanillaFont> = OnceLock::new();
    F.get_or_init(|| VanillaFont { default: parse_mcfont(MCFONT), unifont: parse_unifont(UNIFONT) })
}

/// MCF2:magic"MCF2" + u32(LE 字形数) + N×{cp:u32, advance:u8, width:u8, height:u8, ascent:u8,
/// height×u16(LE) 行位图}。坏数据就地停。
fn parse_mcfont(data: &[u8]) -> HashMap<char, Glyph> {
    let mut map = HashMap::new();
    if data.len() < 8 || &data[0..4] != b"MCF2" {
        return map;
    }
    let count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let mut pos = 8;
    for _ in 0..count {
        if pos + 8 > data.len() {
            break;
        }
        let cp = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let advance = data[pos + 4];
        let width = data[pos + 5];
        let height = data[pos + 6];
        let ascent = data[pos + 7];
        pos += 8;
        let h = height as usize;
        if pos + h * 2 > data.len() {
            break;
        }
        let mut rows = [0u16; 16];
        for (r, row) in rows.iter_mut().take(h.min(16)).enumerate() {
            *row = u16::from_le_bytes([data[pos + r * 2], data[pos + r * 2 + 1]]);
        }
        pos += h * 2;
        if let Some(ch) = char::from_u32(cp) {
            map.insert(ch, Glyph { advance, width, height, ascent, unifont: false, rows });
        }
    }
    map
}

/// UFB1:magic"UFB1" + u32(LE) + N×{cp:u16, cell:u8(8|16), bitmap}。去掉左右留白后作回退字形,
/// ascent 取固定 14(原生 16px 网格;半尺寸后 ≈7,与默认字体基线对齐)。
fn parse_unifont(data: &[u8]) -> HashMap<char, Glyph> {
    const UNIFONT_ASCENT: u8 = 14;
    let mut map = HashMap::new();
    if data.len() < 8 || &data[0..4] != b"UFB1" {
        return map;
    }
    let count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
    let mut pos = 8;
    for _ in 0..count {
        if pos + 3 > data.len() {
            break;
        }
        let cp = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let cell = data[pos + 2];
        pos += 3;
        let nbytes = if cell == 8 { 16 } else { 32 };
        if pos + nbytes > data.len() {
            break;
        }
        let mut raw = [0u16; 16];
        for (r, row) in raw.iter_mut().enumerate() {
            *row = if cell == 8 {
                (data[pos + r] as u16) << 8
            } else {
                u16::from_be_bytes([data[pos + 2 * r], data[pos + 2 * r + 1]])
            };
        }
        pos += nbytes;

        // 去左右留白:找最左/最右有像素列,把位图左移到列 0,步进 = 实宽 + 1。
        let (mut left, mut right) = (u32::MAX, -1i32);
        for col in 0..cell as u32 {
            let bit = 15 - col;
            if raw.iter().any(|r| (r >> bit) & 1 == 1) {
                left = left.min(col);
                right = col as i32;
            }
        }
        let (width, advance, rows) = if right < 0 {
            // 整格空(如全角空格):原版口径全角步进 ≈ 8 GUI 像素
            (0u8, 8u8, [0u16; 16])
        } else {
            let w = (right as u32 - left + 1) as u8;
            let mut rows = [0u16; 16];
            for (d, s) in rows.iter_mut().zip(raw.iter()) {
                *d = s << left;
            }
            // 原版 UnihexProvider:advance = 字宽/2 + 1(GUI 像素)
            (w, w / 2 + 1, rows)
        };

        if let Some(ch) = char::from_u32(cp as u32) {
            map.insert(
                ch,
                Glyph { advance, width, height: 16, ascent: UNIFONT_ASCENT, unifont: true, rows },
            );
        }
    }
    map
}
