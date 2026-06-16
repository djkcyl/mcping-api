//! MOTD 像素级渲染器 —— 严格照原版(26.1.2)服务器列表的渲染逻辑复刻,**不对下发内容做任何
//! 清洗/裁剪**:空格全保留(服务器靠前导空格做伪居中,照样对齐)、样式照原版。
//!
//! 关键度量全部取自反编译的原版源码:
//! - 行高 9,基线 = 文本顶 + 7(ascii ascent)。
//! - MOTD 折行宽 267px(`getContentWidth()=305-4` 再 `-32-2`),最多 2 行,缺省色 0x808080,带阴影。
//! - 阴影色 = 前景 × 0.25(即各通道 >>2),偏移:默认字体 1px、Unihex 0.5px。
//! - 加粗:再描一遍偏移 boldOffset,步进 += boldOffset(默认 1、Unihex 0.5)。
//! - 斜体错切:顶 `1-0.25·up`、底 `1-0.25·down`(GUI 像素),按行线性插值。
//! - 删除线 y+3.5..4.5、下划线 y+8..9(相对文本顶)。
//! - CJK 走 Unihex:oversample 2(半尺寸),步进 = 字宽/2+1。
//!
//! 一切按「GUI 像素」排版,再乘整数 `scale` 到设备像素;Unifont 字形每像素画 scale/2 的块以匹配
//! 半尺寸。附加信息(地址/延迟/人数/版本)放在 MOTD 下方的「第 4 行」,绝不动服务器的两行 MOTD。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use ar_reshaper::ArabicReshaper;
use image::{ImageEncoder, Rgba, RgbaImage};
use unicode_bidi::BidiInfo;

use crate::minecraft::color::Color;
use crate::minecraft::component::{Component, ResolvedStyle, Span};
use crate::minecraft::font::{self, Glyph, GlyphSource};
use crate::minecraft::protocol::PingResult;

/// 原版度量(GUI 像素)。
const LINE_HEIGHT: u32 = 9;
const BASELINE: u32 = 7;
/// 原版服务器列表 MOTD 折行宽:`getContentWidth()(305-4) - 32 - 2`。
pub const MOTD_WIDTH: u32 = 267;
/// MOTD 缺省色 0x808080(原版 -8355712);独立卡(完整数据样式)沿用。
const MOTD_GRAY: [u8; 3] = [128, 128, 128];
/// 选服列表里的灰(人数 / MOTD 缺省 / 扫描点):标准 §7 灰 0xAAAAAA。1.16.5 字节码常量是更暗的
/// 0x808080,但实机参考图用的是 §7 灰,这里照参考图。
const LIST_GRAY: [u8; 3] = [0xAA, 0xAA, 0xAA];

/// 目标客户端版本(决定能否识别 hex 色)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetVersion {
    V1_8_9,
    V1_12_2,
    V1_16_5,
    Latest,
}

impl TargetVersion {
    pub fn protocol(self) -> i32 {
        match self {
            TargetVersion::V1_8_9 => 47,
            TargetVersion::V1_12_2 => 340,
            TargetVersion::V1_16_5 => 754,
            TargetVersion::Latest => 775,
        }
    }
    pub fn supports_hex(self) -> bool {
        matches!(self, TargetVersion::V1_16_5 | TargetVersion::Latest)
    }
}

/// 老客户端遇 hex 色:还原真实行为(丢弃)还是更好看(降到最近命名色)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldColorPolicy {
    Drop,
    Downsample,
}

/// 渲染选项。长度单位为 GUI 像素(再乘 `scale` 到设备像素)。
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// 整数放大倍数(取偶数;Unifont 字形按 scale/2 画)。
    pub scale: u32,
    /// 折行宽(GUI 像素)。MOTD 取 [`MOTD_WIDTH`]。
    pub max_width: u32,
    /// 最多画几行,超出按原版硬截。
    pub max_lines: usize,
    /// 四周留白(GUI 像素)。
    pub padding: u32,
    /// 无 color 时的缺省文字色(原版 MOTD = 0x808080)。
    pub default_color: [u8; 3],
    pub background: Option<[u8; 4]>,
    pub shadow: bool,
    pub target: TargetVersion,
    pub old_color_policy: OldColorPolicy,
    pub obfuscate_seed: u64,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            scale: 4,
            max_width: MOTD_WIDTH,
            max_lines: 2,
            padding: 4,
            default_color: MOTD_GRAY,
            background: Some([20, 20, 24, 255]),
            shadow: true,
            target: TargetVersion::Latest,
            old_color_policy: OldColorPolicy::Downsample,
            obfuscate_seed: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct Placed {
    ch: char, // 源码点(bidi 重排/阿拉伯塑形要用)
    style: ResolvedStyle,
    glyph: Glyph,
    advance: u32, // 设备像素
}

struct Ctx<'a> {
    opts: &'a RenderOptions,
    fg_default: [u8; 3],
}

/// 渲染 MOTD 组件为 RGBA 图(在 `max_width` 宽的区域内左对齐,保留全部空格 → 伪居中照样对齐)。
pub fn render_component(comp: &Component, opts: &RenderOptions) -> RgbaImage {
    render_motd(&comp.to_spans(), opts)
}

/// 渲染样式 span 序列为 RGBA 图。
pub fn render_motd(spans: &[Span], opts: &RenderOptions) -> RgbaImage {
    let s = opts.scale;
    let lines = layout(spans, opts.max_width, opts.max_lines, s);
    let n = lines.len().max(1) as u32;

    // 画布固定为折行宽度,使前导空格的伪居中按原版位置呈现。
    let src_w = opts.padding * 2 + opts.max_width;
    let src_h = opts.padding * 2 + n * LINE_HEIGHT + 3; // +3 容下重音符上探
    let mut img = new_canvas(src_w * s, src_h * s, opts);
    let ctx = Ctx { opts, fg_default: opts.default_color };

    let x0 = (opts.padding * s) as i32;
    for (li, line) in lines.iter().enumerate() {
        let top = ((opts.padding + li as u32 * LINE_HEIGHT) * s) as i32;
        draw_line(&mut img, &ctx, x0, top, line, li);
    }
    img
}

pub fn render_motd_png(spans: &[Span], opts: &RenderOptions) -> Result<Vec<u8>, image::ImageError> {
    encode_png(&render_motd(spans, opts))
}

pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// 把图叠到透明棋盘格底上(透明/半透明区域显示棋盘格,表示该处透明)。返回不透明结果。
/// `cell` = 棋盘格边长(设备像素)。
pub fn composite_over_checker(img: &RgbaImage, cell: u32) -> RgbaImage {
    const LIGHT: [u8; 3] = [60, 60, 64];
    const DARK: [u8; 3] = [42, 42, 46];
    let cell = cell.max(1);
    let (w, h) = (img.width(), img.height());
    let mut out = RgbaImage::new(w.max(1), h.max(1));
    for y in 0..h {
        for x in 0..w {
            let base = if (x / cell + y / cell).is_multiple_of(2) { LIGHT } else { DARK };
            let px = img.get_pixel(x, y).0;
            let a = px[3] as u32;
            let mix = |f: u8, b: u8| ((f as u32 * a + b as u32 * (255 - a)) / 255) as u8;
            out.put_pixel(x, y, Rgba([mix(px[0], base[0]), mix(px[1], base[1]), mix(px[2], base[2]), 255]));
        }
    }
    out
}

fn new_canvas(w: u32, h: u32, opts: &RenderOptions) -> RgbaImage {
    let mut img = RgbaImage::new(w.max(1), h.max(1));
    if let Some(bg) = opts.background {
        for p in img.pixels_mut() {
            *p = Rgba(bg);
        }
    }
    img
}

fn block(glyph: &Glyph, scale: u32) -> u32 {
    if glyph.unifont { (scale / 2).max(1) } else { scale }
}

/// 折行(设备像素):保留全部空格、左对齐;空格处优先断,放不下硬断;`\n` 强制换行;按
/// `max_lines` 硬截。**不去任何空格**。
fn layout(spans: &[Span], max_width: u32, max_lines: usize, scale: u32) -> Vec<Vec<Placed>> {
    let mut lines: Vec<Vec<Placed>> = Vec::new();
    let mut cur: Vec<Placed> = Vec::new();
    let mut cur_w: u32 = 0;
    let mut last_break: Option<usize> = None;
    let max_dev = max_width.saturating_mul(scale);

    for span in spans {
        for ch in span.text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
                last_break = None;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let glyph = glyph_of(ch);
            let bold_extra = if span.style.bold { block(&glyph, scale) } else { 0 };
            let adv = glyph.advance as u32 * scale + bold_extra;

            if cur_w + adv > max_dev && !cur.is_empty() {
                match last_break {
                    Some(bi) if bi < cur.len() => {
                        let rest = cur.split_off(bi);
                        lines.push(std::mem::take(&mut cur));
                        cur_w = rest.iter().map(|p| p.advance).sum();
                        cur = rest;
                    }
                    _ => {
                        lines.push(std::mem::take(&mut cur));
                        cur_w = 0;
                    }
                }
                last_break = None;
            }

            cur.push(Placed { ch, style: span.style, glyph, advance: adv });
            cur_w += adv;
            if ch == ' ' {
                last_break = Some(cur.len());
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.truncate(max_lines.max(1));
    // 折行(逻辑序、按未塑形宽度)之后,逐行做 bidi 重排 —— 与原版 Font.split 的
    // splitLines → getVisualOrder 顺序一致(先折行后逐行重排)。纯 LTR 行原样返回。
    lines.into_iter().map(|line| reorder_bidi(line, scale)).collect()
}

/// 调试/校验用:返回 MOTD 经折行 + bidi 重排 + 阿拉伯塑形后的**每行可见序文本**(不渲染像素)。
/// 可见序即从左到右实际画出的字符顺序,RTL 段已反转、阿拉伯字母已转 presentation form。
pub fn visual_lines(spans: &[Span], max_width: u32, max_lines: usize, scale: u32) -> Vec<String> {
    layout(spans, max_width, max_lines, scale)
        .iter()
        .map(|line| line.iter().map(|p| p.ch).collect())
        .collect()
}

/// 一行的双向(bidi)重排,精确复刻原版 ClientLanguage.getVisualOrder → FormattedBidiReorder:
/// **先**阿拉伯塑形(逻辑序,只做字母连写、不碰数字)**再** bidi(UAX#9,段落基方向 auto-LTR),
/// 按视觉 run 拼接:LTR run 正序、RTL(odd level)run 逆序且逐码点镜像。样式跟着字符走。塑形后按新码点
/// 重取字形、重算宽度。纯 LTR 行直接返回(与原版「只有 odd run 才反转」结果一致)。
fn reorder_bidi(line: Vec<Placed>, scale: u32) -> Vec<Placed> {
    if !line.iter().any(|p| is_rtl_char(p.ch)) {
        return line; // 快路径:无 RTL 码点,顺序不变
    }

    // 1. 阿拉伯塑形:按「同 style 连续段」分段做,保证塑形产物(含合字)归属同一 style。
    //    塑形异常/不可塑形则该段回退原串(对齐原版 ArabicShaping 的 try/catch 静默回退)。
    let reshaper = reshaper();
    let mut shaped: Vec<(char, ResolvedStyle)> = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let style = line[i].style;
        let mut run = String::new();
        let mut j = i;
        while j < line.len() && line[j].style == style {
            run.push(line[j].ch);
            j += 1;
        }
        for c in reshaper.reshape(&run).chars() {
            shaped.push((c, style));
        }
        i = j;
    }

    // 2. bidi:段落基方向传 None = 按首个强字符自动判定、无强字符回退 LTR(对齐 ICU LEVEL_DEFAULT_LTR)。
    let text: String = shaped.iter().map(|(c, _)| *c).collect();
    let bidi = BidiInfo::new(&text, None);
    let Some(para) = bidi.paragraphs.first() else {
        return rebuild(&shaped, scale); // 理论不会:逐行处理时至少一段
    };
    let (levels, runs) = bidi.visual_runs(para, para.range.clone());

    // byte 偏移 → shaped 下标,便于把视觉序字符映回样式。
    let byte_to_idx: HashMap<usize, usize> =
        text.char_indices().enumerate().map(|(idx, (b, _))| (b, idx)).collect();

    // 3. 按视觉顺序拼接每个 run;odd(RTL)level 的 run 逆序 + 逐码点镜像。
    let mut out: Vec<Placed> = Vec::with_capacity(shaped.len());
    for run in runs {
        let rtl = levels[run.start].is_rtl();
        let mut chars: Vec<(usize, char)> =
            text[run.clone()].char_indices().map(|(b, c)| (run.start + b, c)).collect();
        if rtl {
            chars.reverse();
        }
        for (boff, c) in chars {
            let style = byte_to_idx.get(&boff).map(|&k| shaped[k].1).unwrap_or_default();
            let ch = if rtl { mirror(c) } else { c };
            out.push(place(ch, style, scale));
        }
    }
    out
}

/// 进程级共享的阿拉伯塑形器(默认配置 = 上下文连写 + lam-alef 合字,不转数字,对齐 ArabicShaping LETTERS_SHAPE)。
fn reshaper() -> &'static ArabicReshaper {
    static R: OnceLock<ArabicReshaper> = OnceLock::new();
    R.get_or_init(ArabicReshaper::default)
}

/// 由码点+样式造一个 Placed:重取字形、按 bold 重算宽度(与 layout 里一致)。
fn place(ch: char, style: ResolvedStyle, scale: u32) -> Placed {
    let glyph = glyph_of(ch);
    let bold_extra = if style.bold { block(&glyph, scale) } else { 0 };
    Placed { ch, style, glyph, advance: glyph.advance as u32 * scale + bold_extra }
}

fn rebuild(shaped: &[(char, ResolvedStyle)], scale: u32) -> Vec<Placed> {
    shaped.iter().map(|&(c, st)| place(c, st, scale)).collect()
}

/// 强 RTL 码点(希伯来/阿拉伯/叙利亚/它拿/N'Ko/撒玛利亚/阿拉伯扩展与 presentation forms)。
/// 只用于快路径判定:命中才走完整 bidi 流水线,漏判会退化成不重排,故区段取全。
fn is_rtl_char(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F |
        0x0780..=0x07BF | 0x07C0..=0x07FF | 0x0800..=0x083F | 0x0840..=0x085F |
        0x0860..=0x08FF | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

/// 镜像字符(对齐 UCharacter.getMirror,只在 RTL run 上套)。覆盖 MOTD 里常见的成对符号;
/// 其余(罕见数学符号等)不镜像,与原版差异仅限极少见字符。
fn mirror(c: char) -> char {
    match c {
        '(' => ')', ')' => '(',
        '[' => ']', ']' => '[',
        '{' => '}', '}' => '{',
        '<' => '>', '>' => '<',
        '«' => '»', '»' => '«',
        '‹' => '›', '›' => '‹',
        '\u{2264}' => '\u{2265}', '\u{2265}' => '\u{2264}', // ≤ ≥
        _ => c,
    }
}

/// 画一行:先整行阴影、再整行前景(原版 drawString 先描影后描字)。
fn draw_line(img: &mut RgbaImage, ctx: &Ctx, x0: i32, top: i32, line: &[Placed], li: usize) {
    if ctx.opts.shadow {
        let mut pen = x0;
        for (ci, p) in line.iter().enumerate() {
            draw_glyph(img, ctx, pen, top, p, li, ci, true);
            pen += p.advance as i32;
        }
    }
    let mut pen = x0;
    for (ci, p) in line.iter().enumerate() {
        draw_glyph(img, ctx, pen, top, p, li, ci, false);
        pen += p.advance as i32;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    img: &mut RgbaImage,
    ctx: &Ctx,
    pen: i32,
    top: i32,
    p: &Placed,
    li: usize,
    ci: usize,
    is_shadow: bool,
) {
    let s = ctx.opts.scale;
    let color = if is_shadow {
        shadow_of(resolve_fg(&p.style, ctx))
    } else {
        resolve_fg(&p.style, ctx)
    };
    let glyph = if p.style.obfuscated {
        scramble(&p.glyph, salt(ctx.opts.obfuscate_seed, li, ci))
    } else {
        p.glyph
    };
    let blk = block(&glyph, s);
    let baseline = top + (BASELINE * s) as i32;
    let glyph_top = baseline - glyph.ascent as i32 * blk as i32;

    // 阴影偏移(GUI 像素 × scale):默认字体 1、Unihex 0.5。
    let shadow_off = if is_shadow {
        if glyph.unifont { (s / 2).max(1) as i32 } else { s as i32 }
    } else {
        0
    };
    let bold_off = if glyph.unifont { (s / 2).max(1) as i32 } else { s as i32 };

    // 字形像素
    for row in 0..glyph.height as u32 {
        let shear = if p.style.italic { italic_shear(&glyph, row, s) } else { 0 };
        for col in 0..glyph.width as u32 {
            if glyph.pixel(col, row) {
                let gx = pen + (col * blk) as i32 + shear + shadow_off;
                let gy = glyph_top + (row * blk) as i32 + shadow_off;
                fill(img, gx, gy, blk, blk, color);
                if p.style.bold {
                    fill(img, gx + bold_off, gy, blk, blk, color);
                }
            }
        }
    }

    // 装饰线(相对文本顶,跨该字形步进)。删除线 3.5、下划线 8(原版口径),1 GUI 像素粗。
    let span_w = p.advance;
    if p.style.strikethrough {
        let y = top + (BASELINE as i32 * s as i32) - (3 * s as i32 / 2) + shadow_off; // ≈ top+3.5*s
        fill(img, pen + shadow_off, y, span_w, s, color);
    }
    if p.style.underlined {
        let y = top + (8 * s) as i32 + shadow_off;
        fill(img, pen + shadow_off, y, span_w, s, color);
    }
}

/// 原版斜体错切:顶 `1-0.25·up`、底 `1-0.25·down`(GUI 像素),按行线性插值。
fn italic_shear(glyph: &Glyph, row: u32, scale: u32) -> i32 {
    let blk = block(glyph, scale) as f32;
    let s = scale as f32;
    let ascent_gui = glyph.ascent as f32 * blk / s;
    let height_gui = glyph.height as f32 * blk / s;
    let up = BASELINE as f32 - ascent_gui;
    let down = up + height_gui;
    let shear_top = 1.0 - 0.25 * up;
    let shear_bottom = 1.0 - 0.25 * down;
    let t = if glyph.height == 0 { 0.0 } else { row as f32 / glyph.height as f32 };
    let shear_gui = shear_top + (shear_bottom - shear_top) * t;
    (shear_gui * s).round() as i32
}

fn resolve_fg(style: &ResolvedStyle, ctx: &Ctx) -> [u8; 4] {
    match adjust(style.color, ctx.opts) {
        Some(c) => {
            let (r, g, b) = c.rgb();
            [r, g, b, 255]
        }
        None => {
            let [r, g, b] = ctx.fg_default;
            [r, g, b, 255]
        }
    }
}

/// 原版阴影色 = 前景 × 0.25(各通道 >>2),保留 alpha。
fn shadow_of([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [r >> 2, g >> 2, b >> 2, a]
}

/// 按目标版本调整颜色:老目标遇 hex 按策略丢弃或降级。
fn adjust(color: Option<Color>, opts: &RenderOptions) -> Option<Color> {
    match color? {
        Color::Rgb(r, g, b) if !opts.target.supports_hex() => match opts.old_color_policy {
            OldColorPolicy::Drop => None,
            OldColorPolicy::Downsample => Some(Color::Rgb(r, g, b).downsample()),
        },
        c => Some(c),
    }
}

fn fill(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, color: [u8; 4]) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    for dy in 0..h as i32 {
        let yy = y + dy;
        if yy < 0 || yy >= ih {
            continue;
        }
        for dx in 0..w as i32 {
            let xx = x + dx;
            if xx < 0 || xx >= iw {
                continue;
            }
            img.put_pixel(xx as u32, yy as u32, Rgba(color));
        }
    }
}

fn glyph_of(ch: char) -> Glyph {
    font::font().glyph(ch).unwrap_or_else(|| tofu(ch))
}

/// obfuscated:取同字体的随机字形(步进不变,布局稳定)。
fn scramble(orig: &Glyph, salt: u64) -> Glyph {
    const HALF: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789#%&@";
    const FULL: &[char] = &['你', '好', '世', '界', '中', '文', '字', '符', '森', '林', '风', '云'];
    let f = font::font();
    let g = if orig.unifont {
        f.glyph(FULL[(salt % FULL.len() as u64) as usize])
    } else {
        f.glyph(HALF[(salt % HALF.len() as u64) as usize] as char)
    };
    g.unwrap_or(*orig)
}

/// 缺字时的空心豆腐块(默认字体度量)。
fn tofu(_ch: char) -> Glyph {
    let (w, h, ascent) = (6u8, 8u8, 7u8);
    let mut rows = [0u16; 16];
    for (r, row) in rows.iter_mut().take(h as usize).enumerate() {
        let r = r as u32;
        if r == 0 || r as u8 == h - 1 {
            *row = 0b1111_1100_0000_0000;
        } else {
            *row = 0b1000_0100_0000_0000;
        }
    }
    Glyph { advance: w + 1, width: w, height: h, ascent, unifont: false, rows }
}

fn salt(seed: u64, li: usize, ci: usize) -> u64 {
    let mut x = seed
        ^ (li as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (ci as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x
}

// ---- 服务器列表整卡(仿原版条目):favicon + 标题 + 两行 MOTD(忠实)+ 第 4 行附加信息 ----

/// 服务器列表卡样式。
#[derive(Debug, Clone)]
pub struct CardOptions {
    pub scale: u32,
    pub target: TargetVersion,
    pub old_color_policy: OldColorPolicy,
    pub background: [u8; 4],
    /// 标题(空 = 连接地址)。
    pub title: Option<String>,
    /// 把成图叠到透明棋盘格底上:透明 favicon(带 alpha 的 PNG)的镂空处显示棋盘格,而非空洞。
    pub checker: bool,
}

impl Default for CardOptions {
    fn default() -> Self {
        Self {
            scale: 4,
            target: TargetVersion::Latest,
            old_color_policy: OldColorPolicy::Downsample,
            background: [20, 20, 24, 255],
            title: None,
            checker: true,
        }
    }
}

/// 合成一张服务器列表卡(仿原版条目 + 放大图标 + 右侧悬浮窗):
/// - 左侧 favicon 放大到内容区高度(与整卡等高),透明处叠深色棋盘格;
/// - 中列:标题(白)、两行 MOTD(灰 0x808080、折行宽 267、忠实保留空格)、第 4/5 行附加信息;
/// - 右上(限定在 MOTD 列右缘,给悬浮窗让位):人数 `online/max` + 信号格(按延迟取 1..5 格);
/// - 右侧:仿原版悬浮窗(上对齐),渲染最多 5 行 `players.sample`(各行解析 § 码)。无 sample 则不画。
///
/// 卡片高度按**实际内容**取:= max(正文实际占高, 悬浮窗占高),正好包住、底部不留多余空白;没有第 5 行
/// 附加信息时不为它预留空行。图标边长 = 该高度,与整卡等高。
/// 列表条目内边距 / 悬浮窗内边距(GUI 像素)。
const ENTRY_PAD: u32 = 4;
const TT_PAD: u32 = 3;

/// 一个服务器条目的版式(GUI 像素),`entry_layout` 算好,`draw_entry` 据此落笔。整卡与选服整屏
/// 共用,保证「图标 + 5 行 + 右侧 sample 悬浮窗」的渲染口径一致。
struct EntryLayout {
    icon: u32,
    /// 文本列左缘(图标右 + 间隙)。
    text_x: u32,
    /// MOTD 列右缘。
    motd_right: u32,
    /// 悬浮窗左缘。
    tt_x: u32,
    /// 条目高 = `ENTRY_PAD*2 + inner`(标题槽 12 + 4 行)。
    height: u32,
    /// 整宽(含悬浮窗;无 sample 时 = 主体宽)。
    full_w: u32,
    title: String,
    motd: Vec<Span>,
    line4: Vec<Span>,
    line5: Vec<Span>,
    sample_lines: Vec<Vec<Span>>,
    /// 悬浮窗 `(内容宽, 框宽, 框高)`;无 sample 为 `None`。
    tooltip: Option<(u32, u32, u32)>,
}

/// 量好一个条目的版式(不落笔)。固定 5 行高、图标方形等高,口径与原独立卡一致。
fn entry_layout(result: &PingResult, title: Option<&str>, scale: u32) -> EntryLayout {
    let title = title
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}:{}", result.address.host, result.address.port));
    let motd = result.status.description.to_spans();
    let line4 = info_line1(result); // 总有(至少协议号)
    let line5 = info_line2(result); // 可能空

    let sample = result.status.players.as_ref().map(|p| p.sample.as_slice()).unwrap_or(&[]);
    let sample_lines: Vec<Vec<Span>> =
        sample.iter().take(5).map(|pl| Component::text(pl.name.clone()).to_spans()).collect();
    let n = sample_lines.len() as u32;

    let inner = 12 + LINE_HEIGHT * 4;
    let icon = inner;
    let height = ENTRY_PAD * 2 + inner;

    let tooltip = (n > 0).then(|| {
        let content_w = sample_lines
            .iter()
            .map(|l| measure_dev(l, scale).div_ceil(scale))
            .max()
            .unwrap_or(0)
            .clamp(8, 200);
        (content_w, content_w + TT_PAD * 2, (n * LINE_HEIGHT + TT_PAD * 2).min(inner))
    });

    let text_x = ENTRY_PAD + icon + 4;
    let motd_right = text_x + MOTD_WIDTH;
    let tt_x = motd_right + 6;
    let full_w = match tooltip {
        Some((_, box_w, _)) => tt_x + box_w + ENTRY_PAD,
        None => motd_right + ENTRY_PAD,
    };

    EntryLayout {
        icon,
        text_x,
        motd_right,
        tt_x,
        height,
        full_w,
        title,
        motd,
        line4,
        line5,
        sample_lines,
        tooltip,
    }
}

/// 把条目画到 `img` 上,左上角在 GUI 坐标 `(ox, oy)`。图标 / 信号格 / 人数 / 标题 / 两行 MOTD /
/// 第 4·5 行 / 右侧 sample 悬浮窗 —— 与原独立卡同一套落笔逻辑,只是整体平移到 `(ox, oy)`。
fn draw_entry(img: &mut RgbaImage, text: &RenderOptions, ox: u32, oy: u32, lay: &EntryLayout, result: &PingResult) {
    let s = text.scale;
    let dxg = |g: u32| ((ox + g) * s) as i32;
    let dyg = |g: u32| ((oy + g) * s) as i32;

    // 图标(favicon 或占位)。
    match result.status.favicon_png().and_then(|p| image::load_from_memory(&p).ok()) {
        Some(d) => blit_favicon(img, &d.to_rgba8(), (ox + ENTRY_PAD) * s, (oy + ENTRY_PAD) * s, lay.icon * s),
        None => placeholder_icon(img, (ox + ENTRY_PAD) * s, (oy + ENTRY_PAD) * s, lay.icon * s),
    }

    // 右上(对齐 MOTD 列右缘,给悬浮窗让位):信号格 + 人数。
    blit_sprite(img, ping_sprite(result.latency), dxg(lay.motd_right - 15), dyg(ENTRY_PAD), s);
    let count = player_count_spans(result);
    let count_w = measure_dev(&count, s) as i32;
    let count_right = dxg(lay.motd_right - 20);
    draw_right_dev(img, text, MOTD_GRAY, count_right, dyg(ENTRY_PAD + 1), &count);

    // 标题(白),宽度让到人数左侧,放不下截断。
    let title_max_dev = (count_right - count_w) - dxg(lay.text_x) - 4 * s as i32;
    let title_max = (title_max_dev.max(8 * s as i32) as u32) / s;
    let tx = dxg(lay.text_x);
    draw_block_dev(img, text, [255, 255, 255], tx, dyg(ENTRY_PAD + 1), &line_spans(&lay.title), title_max, 1);

    // 两行 MOTD(忠实:保留空格、灰 0x808080、折行宽 267)。
    draw_block_dev(img, text, MOTD_GRAY, tx, dyg(ENTRY_PAD + 12), &lay.motd, MOTD_WIDTH, 2);
    // 第 4 行:延迟 · 版本名 · 协议号。
    draw_block_dev(img, text, [120, 130, 140], tx, dyg(ENTRY_PAD + 12 + LINE_HEIGHT * 2), &lay.line4, MOTD_WIDTH, 1);
    // 第 5 行:整合包 · 模组 · Via · secure chat(空则不画)。
    if !lay.line5.is_empty() {
        draw_block_dev(img, text, [110, 120, 135], tx, dyg(ENTRY_PAD + 12 + LINE_HEIGHT * 3), &lay.line5, MOTD_WIDTH, 1);
    }

    // 右侧 sample 悬浮窗(仿原版 tooltip,上对齐)。
    if let Some((content_w, box_w, box_h)) = lay.tooltip {
        draw_tooltip(img, dxg(lay.tt_x), dyg(ENTRY_PAD), box_w * s, box_h * s, s);
        for (i, ln) in lay.sample_lines.iter().enumerate() {
            let ly = dyg(ENTRY_PAD + TT_PAD) + (i as u32 * LINE_HEIGHT * s) as i32;
            draw_block_dev(img, text, [255, 255, 255], dxg(lay.tt_x + TT_PAD), ly, ln, content_w, 1);
        }
    }
}

pub fn render_server_card(result: &PingResult, opts: &CardOptions) -> RgbaImage {
    let s = opts.scale;
    let text = RenderOptions {
        scale: s,
        max_width: MOTD_WIDTH,
        max_lines: 2,
        padding: 0,
        default_color: MOTD_GRAY,
        background: None,
        shadow: true,
        target: opts.target,
        old_color_policy: opts.old_color_policy,
        obfuscate_seed: 0,
    };
    let lay = entry_layout(result, opts.title.as_deref(), s);
    let bg = RenderOptions { background: Some(opts.background), ..Default::default() };
    let mut img = new_canvas(lay.full_w * s, lay.height * s, &bg);
    draw_entry(&mut img, &text, 0, 0, &lay, result);
    if opts.checker {
        composite_over_checker(&img, (s * 2).max(4))
    } else {
        img
    }
}

/// 仿原版物品/列表悬浮窗:深色底(原版 `0xF0100010`,叠在深色卡底上≈纯黑紫)+ 1px 紫色上下渐变边
/// (原版 `0x505000FF`→`0x5028007F`,按半透明叠深底后的近似色)。坐标/尺寸均为设备像素。
fn draw_tooltip(img: &mut RgbaImage, x: i32, y: i32, w: u32, h: u32, scale: u32) {
    const BG: [u8; 4] = [16, 0, 16, 255];
    const TOP: [u8; 3] = [38, 0, 95];
    const BOT: [u8; 3] = [24, 0, 52];
    fill(img, x, y, w, h, BG);
    let bt = scale.max(1); // 边框 1 GUI 像素厚
    let lerp = |t: f32| {
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
        [mix(TOP[0], BOT[0]), 0, mix(TOP[2], BOT[2]), 255u8]
    };
    fill(img, x, y, w, bt, [TOP[0], TOP[1], TOP[2], 255]); // 上横边
    fill(img, x, y + h as i32 - bt as i32, w, bt, [BOT[0], BOT[1], BOT[2], 255]); // 下横边
    for dy in 0..h as i32 {
        let c = lerp(dy as f32 / h.max(1) as f32); // 左右竖边逐行渐变
        fill(img, x, y + dy, bt, 1, c);
        fill(img, x + w as i32 - bt as i32, y + dy, bt, 1, c);
    }
}

pub fn render_server_card_png(
    result: &PingResult,
    opts: &CardOptions,
) -> Result<Vec<u8>, image::ImageError> {
    encode_png(&render_server_card(result, opts))
}

/// 把多张图竖排合成一张长图。
pub fn stack_vertical(imgs: &[RgbaImage], gap: u32, bg: [u8; 4]) -> RgbaImage {
    let width = imgs.iter().map(|i| i.width()).max().unwrap_or(1).max(1);
    let total_h = imgs.iter().map(|i| i.height()).sum::<u32>()
        + gap * (imgs.len().saturating_sub(1)) as u32;
    let mut out = RgbaImage::from_pixel(width, total_h.max(1), Rgba(bg));
    let mut y = 0u32;
    for im in imgs {
        for (px, py, pixel) in im.enumerate_pixels() {
            out.put_pixel(px, y + py, *pixel);
        }
        y += im.height() + gap;
    }
    out
}

fn line_spans(text: &str) -> Vec<Span> {
    vec![Span { text: text.to_string(), style: ResolvedStyle::default() }]
}

/// 右上角的人数文本 `online/max`(原版直接显示的状态文本)。无玩家信息则空。
fn player_count_spans(result: &PingResult) -> Vec<Span> {
    match &result.status.players {
        Some(p) => line_spans(&format!("{}/{}", p.online, p.max)),
        None => Vec::new(),
    }
}

/// 第 4 行:延迟 ms(原版仅悬浮)· 版本名(原版仅不兼容时显示)· 协议号(原版从不显示)·(旧版标记)。
fn info_line1(result: &PingResult) -> Vec<Span> {
    let mut parts = Vec::new();
    if let Some(d) = result.latency {
        parts.push(format!("{}ms", d.as_millis()));
    }
    if let Some(name) = &result.status.version.name {
        let clean = Component::text(name.clone()).plain();
        let clean = clean.trim();
        if !clean.is_empty() {
            parts.push(clean.to_string());
        }
    }
    parts.push(format!("协议 {}", result.status.version.protocol));
    if result.is_legacy {
        parts.push("旧版 ping".into());
    }
    line_spans(&parts.join("  ·  "))
}

/// 第 5 行:更深的「只有 ping 能拿到」的数据 —— 模组/loader(及数量)· 强制安全聊天 · 免举报(NCR)·
/// 玩家抽样数。都没有则空(不画第 5 行)。
fn info_line2(result: &PingResult) -> Vec<Span> {
    use crate::minecraft::status::ModLoader;
    let st = &result.status;
    let mut parts = Vec::new();

    // 整合包名+版本(BCC betterStatus/better-status)——最值钱的 ping-only 数据,放最前。
    // 未配置的占位已在解析层(归一 "??"→空、双都空→None)滤掉,这里直接用。
    if let Some(mp) = &st.modpack {
        let (name, ver) = (mp.name.trim(), mp.version.trim());
        let tag = match (name.is_empty(), ver.is_empty()) {
            (false, false) => Some(format!("整合包 {name} {ver}")),
            (false, true) => Some(format!("整合包 {name}")),
            (true, false) => Some(format!("整合包 v{ver}")),
            (true, true) => None,
        };
        if let Some(t) = tag {
            parts.push(t);
        }
    }

    if let Some(m) = st.mods() {
        let loader = match m.loader {
            ModLoader::Forge => "Forge",
            ModLoader::Modded => "Modded",
        };
        // 空 modinfo(代理常发的 FML stub)不显示;有数量显数量;带模组但数量未知只显 loader。
        let tag = match m.mod_count {
            Some(0) => None,
            Some(n) => Some(format!("{loader} · {n} 模组{}", if m.truncated { "+" } else { "" })),
            None => Some(loader.to_string()),
        };
        if let Some(t) = tag {
            parts.push(t);
        }
    }
    // ViaVersion supportedVersions(非默认配置才有):服务器经 Via 能接受的客户端协议范围。
    // 原版没有这个字段,出现即说明前面挂了 Via。忠实展示协议号区间 + 个数,不猜版本名。
    let sv = &st.version.supported_versions;
    if !sv.is_empty() {
        let (min, max) = (sv.iter().min().copied().unwrap_or(0), sv.iter().max().copied().unwrap_or(0));
        let range = if min == max { format!("proto {min}") } else { format!("proto {min}–{max}") };
        parts.push(format!("Via 兼容 {} 版 · {range}", sv.len()));
    }
    if st.enforces_secure_chat == Some(true) {
        parts.push("安全聊天".into());
    }
    if st.prevents_chat_reports == Some(true) {
        parts.push("免举报".into());
    }
    // 玩家 sample 不再以「抽样 N」计数出现 —— 改由右侧仿原版悬浮窗渲染实际内容(见 render_server_card)。
    if parts.is_empty() {
        Vec::new()
    } else {
        line_spans(&parts.join("  ·  "))
    }
}

/// 量一行 span 的设备像素宽。
fn measure_dev(spans: &[Span], scale: u32) -> u32 {
    layout(spans, u32::MAX, 1, scale)
        .first()
        .map(|l| l.iter().map(|p| p.advance).sum())
        .unwrap_or(0)
}

/// 在设备坐标处左对齐画若干行 span。
#[allow(clippy::too_many_arguments)]
fn draw_block_dev(
    img: &mut RgbaImage,
    opts: &RenderOptions,
    fg: [u8; 3],
    x_dev: i32,
    top_dev: i32,
    spans: &[Span],
    max_width: u32,
    max_lines: usize,
) {
    let lines = layout(spans, max_width, max_lines, opts.scale);
    let ctx = Ctx { opts, fg_default: fg };
    for (li, line) in lines.iter().enumerate() {
        let top = top_dev + (li as u32 * LINE_HEIGHT * opts.scale) as i32;
        draw_line(img, &ctx, x_dev, top, line, li);
    }
}

/// 在设备坐标处右对齐画一行 span(右边缘 = `right_x_dev`)。
fn draw_right_dev(
    img: &mut RgbaImage,
    opts: &RenderOptions,
    fg: [u8; 3],
    right_x_dev: i32,
    top_dev: i32,
    spans: &[Span],
) {
    let lines = layout(spans, u32::MAX, 1, opts.scale);
    if let Some(line) = lines.first() {
        let w: u32 = line.iter().map(|p| p.advance).sum();
        let ctx = Ctx { opts, fg_default: fg };
        draw_line(img, &ctx, right_x_dev - w as i32, top_dev, line, 0);
    }
}

/// 原版信号格图标(10×8),按延迟取格数;无延迟用 unreachable。
struct PingSprites {
    bars: [RgbaImage; 5],
    unreachable: RgbaImage,
}

fn ping_sprites() -> &'static PingSprites {
    static P: OnceLock<PingSprites> = OnceLock::new();
    P.get_or_init(|| {
        let load = |b: &[u8]| {
            image::load_from_memory(b).map(|d| d.to_rgba8()).unwrap_or_else(|_| RgbaImage::new(10, 8))
        };
        PingSprites {
            bars: [
                load(include_bytes!("../../assets/minecraft/sprites/ping_1.png")),
                load(include_bytes!("../../assets/minecraft/sprites/ping_2.png")),
                load(include_bytes!("../../assets/minecraft/sprites/ping_3.png")),
                load(include_bytes!("../../assets/minecraft/sprites/ping_4.png")),
                load(include_bytes!("../../assets/minecraft/sprites/ping_5.png")),
            ],
            unreachable: load(include_bytes!("../../assets/minecraft/sprites/unreachable.png")),
        }
    })
}

/// 原版阈值:<150→5 格、<300→4、<600→3、<1000→2、否则 1;无延迟→unreachable。
fn ping_sprite(latency: Option<Duration>) -> &'static RgbaImage {
    let p = ping_sprites();
    match latency {
        None => &p.unreachable,
        Some(d) => {
            let ms = d.as_millis();
            let i = if ms < 150 {
                4
            } else if ms < 300 {
                3
            } else if ms < 600 {
                2
            } else if ms < 1000 {
                1
            } else {
                0
            };
            &p.bars[i]
        }
    }
}

/// 把一张精灵图按整数 `scale` 最近邻铺到设备坐标(透明像素跳过)。
fn blit_sprite(img: &mut RgbaImage, sprite: &RgbaImage, ox: i32, oy: i32, scale: u32) {
    for (sx, sy, px) in sprite.enumerate_pixels() {
        if px.0[3] < 8 {
            continue;
        }
        fill(img, ox + (sx * scale) as i32, oy + (sy * scale) as i32, scale, scale, px.0);
    }
}

/// favicon 最近邻缩放到 `size×size` 设备像素铺上。
fn blit_favicon(img: &mut RgbaImage, fav: &RgbaImage, ox: u32, oy: u32, size: u32) {
    let (fw, fh) = (fav.width().max(1), fav.height().max(1));
    for sy in 0..size {
        for sx in 0..size {
            let px = *fav.get_pixel((sx * fw / size).min(fw - 1), (sy * fh / size).min(fh - 1));
            let (xx, yy) = (ox + sx, oy + sy);
            if xx < img.width() && yy < img.height() {
                img.put_pixel(xx, yy, px);
            }
        }
    }
}

fn placeholder_icon(img: &mut RgbaImage, ox: u32, oy: u32, size: u32) {
    for sy in 0..size {
        for sx in 0..size {
            let edge = sx == 0 || sy == 0 || sx == size - 1 || sy == size - 1;
            let c = if edge { [80, 80, 90, 255] } else { [40, 40, 48, 255] };
            let (xx, yy) = (ox + sx, oy + sy);
            if xx < img.width() && yy < img.height() {
                img.put_pixel(xx, yy, Rgba(c));
            }
        }
    }
}

// ============================================================================
// 选服整屏(JoinMultiplayerScreen)复刻 —— 原版 1.21 GUI 度量,贴图取自客户端 jar。
//
// 度量(GUI 像素,皆取自反编译源码):
// - HeaderAndFooterLayout(header=33, footer=60);标题「Select Server」居中于 header 带。
// - 列表行宽 getRowWidth()=305,行内:图标 32²、名字 +35/+1(白)、MOTD +35/(+12+9i)(灰 0x808080)、
//   信号格 10×8 在 right-15、人数右对齐到 right-20。选中行:1px 边框(聚焦=白)+ 黑底(extractSelection)。
// - 分隔线 header_separator/footer_separator(32×2)铺满整宽,列表顶上 2px / 列表底。
// - 背景 menu_background(16×16,已预暗化)按 32px 格平铺(原版 blit 纹理参考尺寸 32,即 2×)。
// - 页脚两排按钮(20 高,排距 4,水平居中):上排 3×100「Join Server·Direct Connection·Add Server」,
//   下排 4×74「Edit·Delete·Refresh·Back」;按钮 widget/button(200×20 nine-slice border 3)。
// ============================================================================

// 1.16.5 MultiplayerScreen 度量(GUI 像素,取自反编译):标题 y=20;列表 top=32、bottom=height-64、
// itemHeight=36;getRowWidth()=305、getRowLeft()=width/2-150;按钮两排 y=height-52 / height-28。
const LIST_TOP: u32 = 32;
const LIST_FOOTER: u32 = 64; // 列表底 = height - 64
const ROW_W: u32 = 305; // getRowWidth() = 220 + 85
const ITEM_H: u32 = 36;

/// 选服整屏的渲染选项。画布尺寸不暴露:高度由 header/footer + 条目高定死、宽度随 sample 增长。
#[derive(Debug, Clone)]
pub struct ScreenOptions {
    pub scale: u32,
    pub target: TargetVersion,
    pub old_color_policy: OldColorPolicy,
    /// 条目标题(空 = 连接地址)。
    pub title: Option<String>,
}

impl Default for ScreenOptions {
    fn default() -> Self {
        Self {
            scale: 4,
            target: TargetVersion::Latest,
            old_color_policy: OldColorPolicy::Downsample,
            title: None,
        }
    }
}

/// 原版 1.16.5 GUI 贴图(运行时解码一次)。`dirt` = options_background(16×16);`button` = widgets.png
/// 的按钮区(0,66,200×20)。
struct GuiTextures {
    dirt: RgbaImage,
    button: RgbaImage,
}

fn gui_textures() -> &'static GuiTextures {
    static G: OnceLock<GuiTextures> = OnceLock::new();
    G.get_or_init(|| {
        let load = |b: &[u8]| {
            image::load_from_memory(b).map(|d| d.to_rgba8()).unwrap_or_else(|_| RgbaImage::new(1, 1))
        };
        GuiTextures {
            dirt: load(include_bytes!("../../assets/minecraft/gui/options_background.png")),
            button: load(include_bytes!("../../assets/minecraft/gui/button_1165.png")),
        }
    })
}

/// src-over 单像素混合(用于半透明分隔线/按钮叠到不透明底上)。
fn put_blend(img: &mut RgbaImage, x: i32, y: i32, px: [u8; 4]) {
    if px[3] == 0 {
        return;
    }
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    if x < 0 || y < 0 || x >= iw || y >= ih {
        return;
    }
    if px[3] == 255 {
        img.put_pixel(x as u32, y as u32, Rgba(px));
        return;
    }
    let dst = img.get_pixel(x as u32, y as u32).0;
    let a = px[3] as u32;
    let inv = 255 - a;
    let mix = |s: u8, d: u8| ((s as u32 * a + d as u32 * inv) / 255) as u8;
    img.put_pixel(x as u32, y as u32, Rgba([mix(px[0], dst[0]), mix(px[1], dst[1]), mix(px[2], dst[2]), 255]));
}

/// 把 16×16 dirt 块按 32px 格(2× 放大、REPEAT)铺到 GUI 矩形 `[gx0,gy0,gw,gh]`,并按 `shift` 右移
/// 变暗(原版顶点色:整屏 0x404040 即 `shift=2`、列表视口 0x202020 即 `shift=3`)。瓦片对齐全局原点,
/// 故整屏与列表两层泥土纹理对齐。输出不透明。
fn fill_dirt_region(img: &mut RgbaImage, gx0: u32, gy0: u32, gw: u32, gh: u32, scale: u32, shift: u8) {
    let tex = &gui_textures().dirt;
    let (tw, th) = (tex.width().max(1), tex.height().max(1));
    for gy in gy0..gy0 + gh {
        let ty = (gy / 2) % th;
        for gx in gx0..gx0 + gw {
            let tx = (gx / 2) % tw;
            let p = tex.get_pixel(tx, ty).0;
            let dark = [p[0] >> shift, p[1] >> shift, p[2] >> shift, 255];
            fill(img, (gx * scale) as i32, (gy * scale) as i32, scale, scale, dark);
        }
    }
}

/// 列表上/下边缘的 4px 黑色渐变阴影(原版 fillGradient):`downward` 时从 `y_edge`(α=255)向下淡出到
/// `y_edge+depth`(α=0);否则从 `y_edge`(α=255)向上淡出。整宽 `gw`,带 alpha 混合。
fn shadow_edge(img: &mut RgbaImage, gw: u32, y_edge: u32, depth: u32, scale: u32, downward: bool) {
    for d in 0..depth {
        let a = (255 - d * 255 / depth) as u8;
        let gy = if downward { y_edge + d } else { y_edge.saturating_sub(1 + d) };
        for gx in 0..gw {
            for sy in 0..scale {
                for sx in 0..scale {
                    put_blend(img, (gx * scale + sx) as i32, (gy * scale + sy) as i32, [0, 0, 0, a]);
                }
            }
        }
    }
}

/// 一颗 1.16.5 按钮:`widgets.png` 按钮(200×20)分左右两半铺到目标宽 `w_gui`(原版 blit 口径:左半取
/// 纹理左 w/2、右半取纹理右 w/2,中段不拉伸)+ 居中白字(带阴影)。`x_gui`/`y_gui` 为 GUI 坐标。
fn draw_button(img: &mut RgbaImage, opts: &RenderOptions, x_gui: u32, y_gui: u32, w_gui: u32, label: &str) {
    let s = opts.scale;
    let tex = &gui_textures().button; // 200×20
    let sw = tex.width().max(1);
    let lh = w_gui / 2; // 左半宽
    let rh = w_gui - lh; // 右半宽(覆盖到底,处理奇数宽)
    let blit_half = |img: &mut RgbaImage, dx0: u32, u0: u32, hw: u32| {
        for gy in 0..20u32 {
            for gx in 0..hw {
                let px = tex.get_pixel((u0 + gx).min(sw - 1), gy).0;
                for sy in 0..s {
                    for sx in 0..s {
                        put_blend(
                            img,
                            ((x_gui + dx0 + gx) * s + sx) as i32,
                            ((y_gui + gy) * s + sy) as i32,
                            px,
                        );
                    }
                }
            }
        }
    };
    blit_half(img, 0, 0, lh); // 左半:纹理 [0, lh)
    blit_half(img, lh, sw - rh, rh); // 右半:纹理 [200-rh, 200)
    let spans = line_spans(label);
    let lw = measure_dev(&spans, s);
    let lx = (x_gui * s) as i32 + ((w_gui * s) as i32 - lw as i32) / 2;
    let ly = (y_gui * s) as i32 + ((20 - 8) / 2 * s) as i32; // 原版按钮文本竖向居中用 8 高
    draw_block_dev(img, opts, [255, 255, 255], lx, ly, &spans, w_gui, 1);
}

/// 复刻原版 **1.16.5**「Play Multiplayer」整屏(度量取自反编译的 1.16.5 客户端):暗泥土背景(列表视口
/// 更暗 + 上下 4px 渐变阴影)+ 标题 + 选中条目(图标 + 名字 + 两行 MOTD + 信号格 + 人数)+ LAN 扫描提示 +
/// 页脚两排按钮。画布固定 427×240 GUI(16:9,= 854×480 窗口在 GUI scale 2 下的逻辑分辨率)。
pub fn render_select_server_screen(result: &PingResult, opts: &ScreenOptions) -> RgbaImage {
    let s = opts.scale.max(1);
    let w_gui = 427u32;
    let h_gui = 240u32;
    let cw = w_gui / 2; // 屏幕水平中线(GUI)

    let text = RenderOptions {
        scale: s,
        max_width: MOTD_WIDTH,
        max_lines: 2,
        padding: 0,
        default_color: LIST_GRAY,
        background: None,
        shadow: true,
        target: opts.target,
        old_color_policy: opts.old_color_policy,
        obfuscate_seed: 0,
    };

    let list_bottom = h_gui - LIST_FOOTER; // = height - 64

    let mut img = RgbaImage::new(w_gui * s, h_gui * s);
    // 整屏泥土 ×0.25(原版 renderDirtBackground,顶点色 0x404040)。
    fill_dirt_region(&mut img, 0, 0, w_gui, h_gui, s, 2);
    // 列表视口泥土更暗 ×0.125(原版顶点色 0x202020),整宽、list_top..list_bottom。
    fill_dirt_region(&mut img, 0, LIST_TOP, w_gui, list_bottom - LIST_TOP, s, 3);
    // 列表上/下 4px 黑色渐变阴影(原版 fillGradient)。
    shadow_edge(&mut img, w_gui, LIST_TOP, 4, s, true);
    shadow_edge(&mut img, w_gui, list_bottom, 4, s, false);

    // 标题「Play Multiplayer」白字居中,y=20(原版 drawCenteredString)。
    let title_spans = line_spans("Play Multiplayer");
    let tw = measure_dev(&title_spans, s);
    draw_block_dev(&mut img, &text, [255, 255, 255], (cw * s) as i32 - tw as i32 / 2, (20 * s) as i32, &title_spans, w_gui, 1);

    // ---- 选中条目(列表顶端,原版 1.16.5 度量)----
    // getRowLeft() = width/2 - getRowWidth()/2 + 2 = cw - 150;条目内容左上即此(无内缩)。
    let row_left = cw - ROW_W / 2 + 2; // = cw - 150
    let entry_top = LIST_TOP + 4; // 首条目顶(列表顶下 4px)
    let row_right = row_left + ROW_W; // 右锚:ping/人数贴 x+width

    // 选中高亮框(原版 extractSelection):聚焦白外框 [cw-152, cw+152]×[top-2, top+34] + 内填黑(内缩 1)。
    let box_l = cw - ROW_W / 2; // = cw - 152
    let box_w = ROW_W - 1; // 304(cw-152..cw+152)
    let box_top = entry_top - 2;
    let box_h = ITEM_H; // 36
    fill(&mut img, (box_l * s) as i32, (box_top * s) as i32, box_w * s, box_h * s, [255, 255, 255, 255]);
    fill(
        &mut img,
        ((box_l + 1) * s) as i32,
        ((box_top + 1) * s) as i32,
        (box_w - 2) * s,
        (box_h - 2) * s,
        [0, 0, 0, 255],
    );

    // 条目内容(图标 + 名字 + 两行 MOTD + 信号格 + 人数)。
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("{}:{}", result.address.host, result.address.port));
    let _ = row_right;
    draw_vanilla_entry(&mut img, &text, row_left, entry_top, &title, Some(result));

    // ---- LAN 扫描提示(原版 lanServer.scanning,居中;下一行扫描指示点)----
    let scan_row_top = entry_top + ITEM_H; // 下一行
    let scan_y = scan_row_top + ITEM_H / 2 - LINE_HEIGHT / 2; // 行内竖向居中
    let scan = line_spans("Scanning for games on your local network");
    let scan_w = measure_dev(&scan, s);
    draw_block_dev(&mut img, &text, [255, 255, 255], (cw * s) as i32 - scan_w as i32 / 2, (scan_y * s) as i32, &scan, w_gui, 1);
    let dots = line_spans("o O o");
    let dots_w = measure_dev(&dots, s);
    draw_block_dev(&mut img, &text, LIST_GRAY, (cw * s) as i32 - dots_w as i32 / 2, ((scan_y + LINE_HEIGHT) * s) as i32, &dots, w_gui, 1);

    draw_footer_buttons(&mut img, &text, cw, h_gui);
    // 透明 favicon 的镂空处叠棋盘格(与完整数据卡口径一致),其余像素本就不透明、棋盘格不显。
    composite_over_checker(&img, (s * 2).max(4))
}

/// 画一条 1.16.5 列表条目(图标 + 名字 + 两行 MOTD + 信号格 + 人数)到 `(row_left, entry_top)`(GUI
/// 坐标)。`result=None` 表示不可达:占位图标 + unreachable 信号格 + 红字「无法连接到服务器」。
fn draw_vanilla_entry(
    img: &mut RgbaImage,
    text: &RenderOptions,
    row_left: u32,
    entry_top: u32,
    name: &str,
    result: Option<&PingResult>,
) {
    let s = text.scale;
    let row_right = row_left + ROW_W;

    // 图标:favicon(可达且有)或占位。
    let fav = result.and_then(|r| r.status.favicon_png()).and_then(|p| image::load_from_memory(&p).ok());
    match fav {
        Some(d) => blit_favicon(img, &d.to_rgba8(), row_left * s, entry_top * s, 32 * s),
        None => placeholder_icon(img, row_left * s, entry_top * s, 32 * s),
    }

    let content_x = row_left + 32 + 3;
    // 信号格:可达按延迟取格,不可达取 unreachable。
    let sprite = ping_sprite(result.and_then(|r| r.latency).filter(|_| result.is_some()));
    let sprite = if result.is_some() { sprite } else { ping_sprite(None) };
    blit_sprite(img, sprite, ((row_right - 15) * s) as i32, (entry_top * s) as i32, s);

    // 人数(仅可达,右缘 x+width-17,灰)。
    let count_right = ((row_right - 17) * s) as i32;
    let count_w = match result {
        Some(r) => {
            let count = player_count_spans(r);
            let w = measure_dev(&count, s) as i32;
            draw_right_dev(img, text, LIST_GRAY, count_right, ((entry_top + 1) * s) as i32, &count);
            w
        }
        None => 0,
    };

    // 名字(白),宽度让到人数左侧。
    let name_max_dev = (count_right - count_w) - (content_x * s) as i32 - 4 * s as i32;
    let name_max = (name_max_dev.max(8 * s as i32) as u32) / s;
    let cx = (content_x * s) as i32;
    draw_block_dev(img, text, [255, 255, 255], cx, ((entry_top + 1) * s) as i32, &line_spans(name), name_max, 1);

    // MOTD(可达:两行忠实灰;不可达:一行红「无法连接到服务器」)。
    match result {
        Some(r) => {
            let motd = r.status.description.to_spans();
            draw_block_dev(img, text, LIST_GRAY, cx, ((entry_top + 12) * s) as i32, &motd, MOTD_WIDTH, 2);
        }
        None => {
            let err = line_spans("无法连接到服务器");
            draw_block_dev(img, text, [255, 85, 85], cx, ((entry_top + 12) * s) as i32, &err, MOTD_WIDTH, 1);
        }
    }
}

/// 页脚两排按钮(原版 1.16.5 逐颗定位,相对屏幕中线 `cw`,贴底 `h_gui`)。
fn draw_footer_buttons(img: &mut RgbaImage, text: &RenderOptions, cw: u32, h_gui: u32) {
    let y_top = h_gui - 52;
    let y_bot = h_gui - 28;
    // (label, x 相对 cw 的偏移, 宽, y)
    let buttons: [(&str, i32, u32, u32); 7] = [
        ("Join Server", -154, 100, y_top),
        ("Direct Connection", -50, 100, y_top),
        ("Add Server", 54, 100, y_top),
        ("Edit", -154, 70, y_bot),
        ("Delete", -74, 70, y_bot),
        ("Refresh", 4, 70, y_bot),
        ("Cancel", 80, 75, y_bot),
    ];
    for (label, dx, bw, by) in buttons {
        draw_button(img, text, (cw as i32 + dx) as u32, by, bw, label);
    }
}

/// 选服整屏(单服)→ PNG。
pub fn render_select_server_png(
    result: &PingResult,
    opts: &ScreenOptions,
) -> Result<Vec<u8>, image::ImageError> {
    encode_png(&render_select_server_screen(result, opts))
}

/// 一条待渲染的列表条目:展示名 + ping 结果(`None` = 不可达)。
pub struct ListEntry {
    pub name: String,
    pub result: Option<PingResult>,
}

/// 复刻 1.16.5「Play Multiplayer」整屏,但列出**多台**服务器(批量 ping 用):条目自上而下堆叠,每条
/// 36px;列表区高度随条目数增长(放不下就出长图),不足一屏时补到 16:9 最小高。无选中高亮、无 LAN 扫描行。
pub fn render_server_list_screen(entries: &[ListEntry], opts: &ScreenOptions) -> RgbaImage {
    let s = opts.scale.max(1);
    let w_gui = 427u32;
    let cw = w_gui / 2;
    let n = entries.len().max(1) as u32;

    let text = RenderOptions {
        scale: s,
        max_width: MOTD_WIDTH,
        max_lines: 2,
        padding: 0,
        default_color: LIST_GRAY,
        background: None,
        shadow: true,
        target: opts.target,
        old_color_policy: opts.old_color_policy,
        obfuscate_seed: 0,
    };

    // 列表区高度:条目堆叠(每条 36)+ 上下各 4 呼吸;不足单屏列表区(240-32-64=144)则补齐,保持 16:9 下限。
    let single_list_h = 240 - LIST_TOP - LIST_FOOTER; // 144
    let list_h = (n * ITEM_H + 8).max(single_list_h);
    let list_bottom = LIST_TOP + list_h;
    let h_gui = LIST_TOP + list_h + LIST_FOOTER;

    let mut img = RgbaImage::new(w_gui * s, h_gui * s);
    fill_dirt_region(&mut img, 0, 0, w_gui, h_gui, s, 2);
    fill_dirt_region(&mut img, 0, LIST_TOP, w_gui, list_bottom - LIST_TOP, s, 3);
    shadow_edge(&mut img, w_gui, LIST_TOP, 4, s, true);
    shadow_edge(&mut img, w_gui, list_bottom, 4, s, false);

    // 标题。
    let title = line_spans("Play Multiplayer");
    let tw = measure_dev(&title, s);
    draw_block_dev(&mut img, &text, [255, 255, 255], (cw * s) as i32 - tw as i32 / 2, (20 * s) as i32, &title, w_gui, 1);

    // 条目堆叠(无选中框,直接画在列表泥土上)。
    let row_left = cw - ROW_W / 2 + 2;
    for (i, e) in entries.iter().enumerate() {
        let entry_top = LIST_TOP + 4 + i as u32 * ITEM_H;
        draw_vanilla_entry(&mut img, &text, row_left, entry_top, &e.name, e.result.as_ref());
    }

    draw_footer_buttons(&mut img, &text, cw, h_gui);
    // 透明 favicon 的镂空处叠棋盘格(与完整数据卡口径一致),其余像素本就不透明、棋盘格不显。
    composite_over_checker(&img, (s * 2).max(4))
}

/// 多服列表整屏 → PNG。
pub fn render_server_list_png(
    entries: &[ListEntry],
    opts: &ScreenOptions,
) -> Result<Vec<u8>, image::ImageError> {
    encode_png(&render_server_list_screen(entries, opts))
}
