//! MOTD 配色 —— 16 个命名色(§0–§f)、`#RRGGBB` 十六进制色(1.16+),以及给老客户端
//! 用的「降到最近命名色」。
//!
//! 阴影色:命名色用表里的定值(MC 对 gold 有特例 `#3E2A00`,其余是各通道 ÷4);任意 hex
//! 的阴影按各通道 `>>2` 现算。文本默认 RGB 取前景,阴影偏暗一档,贴合 MC 文字的 1px 投影。

/// 一个命名色:§ 码字符、名字、前景 RGB、投影 RGB。
#[derive(Clone, Copy, Debug)]
pub struct NamedColor {
    pub code: char,
    pub name: &'static str,
    pub rgb: (u8, u8, u8),
    pub shadow: (u8, u8, u8),
}

/// 16 个命名色,顺序即 §0..§f / 索引 0..15。
pub const NAMED: [NamedColor; 16] = [
    NamedColor { code: '0', name: "black", rgb: (0, 0, 0), shadow: (0, 0, 0) },
    NamedColor { code: '1', name: "dark_blue", rgb: (0, 0, 170), shadow: (0, 0, 42) },
    NamedColor { code: '2', name: "dark_green", rgb: (0, 170, 0), shadow: (0, 42, 0) },
    NamedColor { code: '3', name: "dark_aqua", rgb: (0, 170, 170), shadow: (0, 42, 42) },
    NamedColor { code: '4', name: "dark_red", rgb: (170, 0, 0), shadow: (42, 0, 0) },
    NamedColor { code: '5', name: "dark_purple", rgb: (170, 0, 170), shadow: (42, 0, 42) },
    NamedColor { code: '6', name: "gold", rgb: (255, 170, 0), shadow: (62, 42, 0) },
    NamedColor { code: '7', name: "gray", rgb: (170, 170, 170), shadow: (42, 42, 42) },
    NamedColor { code: '8', name: "dark_gray", rgb: (85, 85, 85), shadow: (21, 21, 21) },
    NamedColor { code: '9', name: "blue", rgb: (85, 85, 255), shadow: (21, 21, 63) },
    NamedColor { code: 'a', name: "green", rgb: (85, 255, 85), shadow: (21, 63, 21) },
    NamedColor { code: 'b', name: "aqua", rgb: (85, 255, 255), shadow: (21, 63, 63) },
    NamedColor { code: 'c', name: "red", rgb: (255, 85, 85), shadow: (63, 21, 21) },
    NamedColor { code: 'd', name: "light_purple", rgb: (255, 85, 255), shadow: (63, 21, 63) },
    NamedColor { code: 'e', name: "yellow", rgb: (255, 255, 85), shadow: (63, 63, 21) },
    NamedColor { code: 'f', name: "white", rgb: (255, 255, 255), shadow: (63, 63, 63) },
];

/// 解析出的颜色:命名色(指向 [`NAMED`])或真 24 位 RGB。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Named(usize),
    Rgb(u8, u8, u8),
}

impl Color {
    /// 前景 RGB。
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Color::Named(i) => NAMED[i].rgb,
            Color::Rgb(r, g, b) => (r, g, b),
        }
    }

    /// 投影 RGB:命名色取表中定值,hex 按各通道 `>>2`。
    pub fn shadow_rgb(self) -> (u8, u8, u8) {
        match self {
            Color::Named(i) => NAMED[i].shadow,
            Color::Rgb(r, g, b) => (r >> 2, g >> 2, b >> 2),
        }
    }

    /// 命名色名(hex 给 `None`)。
    pub fn name(self) -> Option<&'static str> {
        match self {
            Color::Named(i) => Some(NAMED[i].name),
            Color::Rgb(..) => None,
        }
    }

    /// 老客户端(<1.16)无法识别 hex,会直接丢弃。要在老目标上仍出彩,得自己先降到最近命名色。
    /// 命名色原样返回。
    pub fn downsample(self) -> Color {
        match self {
            Color::Named(_) => self,
            Color::Rgb(r, g, b) => Color::Named(nearest_named((r, g, b))),
        }
    }
}

/// 按 § 码字符取命名色索引(`'0'..'9'`、`'a'..'f'`,大小写不限)。
pub fn index_by_code(c: char) -> Option<usize> {
    let c = c.to_ascii_lowercase();
    NAMED.iter().position(|n| n.code == c)
}

/// 按名字取命名色(`"gold"`、`"dark_blue"`…)。
pub fn by_name(name: &str) -> Option<Color> {
    NAMED.iter().position(|n| n.name == name).map(Color::Named)
}

/// 解析 `color` 字段:`#RRGGBB` → RGB;否则按命名色查;都不中给 `None`(与老客户端的「不认就
/// 丢弃」一致 —— 由渲染层决定丢弃还是降级)。
pub fn parse(value: &str) -> Option<Color> {
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex).map(|(r, g, b)| Color::Rgb(r, g, b));
    }
    by_name(value)
}

/// 解析十六进制为 RGB。对齐原版 `TextColor.parseColor` 的 `#` 分支:用 `Integer.parseInt(s,16)`,
/// 故接受 1–6 位(不足按高位补零,`#abc` → `0x000abc`),≤6 位即保证值 ≤ `0xFFFFFF`。
/// (BungeeCord `§x` 这条路径仍恒为 6 位,见 component。)
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    if !(1..=6).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

/// 在 16 命名色里找欧氏距离最近的(给 hex 降级用)。
pub fn nearest_named((r, g, b): (u8, u8, u8)) -> usize {
    let mut best = 0usize;
    let mut best_d = u32::MAX;
    for (i, n) in NAMED.iter().enumerate() {
        let (nr, ng, nb) = n.rgb;
        let dr = r as i32 - nr as i32;
        let dg = g as i32 - ng as i32;
        let db = b as i32 - nb as i32;
        let d = (dr * dr + dg * dg + db * db) as u32;
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}
