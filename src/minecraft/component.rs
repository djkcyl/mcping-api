//! 文本组件(raw JSON text)解析 → 样式 span 中间表示。
//!
//! SLP 的 `description` 永远是 JSON 文本组件(到 26.1 仍如此,不是 NBT)。它有三种形态:裸
//! 字符串、单个对象、组件数组(首元素为根、其余进 `extra`)。两套上色机制并存:结构化的
//! `color`/布尔样式字段(子继承父),以及正文里内嵌的 `§` 码。
//!
//! [`Component::to_spans`] 把整棵树摊平成 [`Span`] 序列 —— 每段携带「解析完毕、无 Option」的
//! [`ResolvedStyle`]。这就是对外的「结构化样式 span」交付物,也是渲染器的输入。
//!
//! `§` 码语义按 MC 的 legacy 口径:颜色码(§0–§f / §x+六位)会**清空**当前所有样式标志再上色;
//! `§r` 整体复位;k/l/m/n/o 叠加样式。`§` 状态在单个文本节点内左→右贯穿;跨节点各自从自身继承
//! 样式起算(父节点 § 码渗进子节点这种边角不模拟,MOTD 几乎不出现)。

use std::iter::Peekable;
use std::str::Chars;

use serde::de::{Deserialize, Deserializer};

use crate::minecraft::color::{self, Color};

/// 组件上的样式(JSON 形态,字段可缺省 → 继承父)。
#[derive(Clone, Default, Debug)]
pub struct Style {
    pub color: Option<Color>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underlined: Option<bool>,
    pub strikethrough: Option<bool>,
    pub obfuscated: Option<bool>,
    pub font: Option<String>,
}

/// 一个文本组件节点。
#[derive(Clone, Default, Debug)]
pub struct Component {
    pub text: String,
    pub translate: Option<String>,
    pub with: Vec<Component>,
    pub fallback: Option<String>,
    pub extra: Vec<Component>,
    pub style: Style,
}

/// 摊平后的样式 —— 全部定下来,渲染器直接用。`color = None` 表示用渲染器的默认文字色。
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ResolvedStyle {
    pub color: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub strikethrough: bool,
    pub obfuscated: bool,
}

/// 一段同样式的文本。渲染器/ANSI/纯文本三种后端都吃它。
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: ResolvedStyle,
}

impl Component {
    /// 仅含文本的便捷构造。
    pub fn text(s: impl Into<String>) -> Self {
        Component { text: s.into(), ..Default::default() }
    }

    /// 摊平成 span 序列(真彩色;hex 不在此降级,留给渲染层按目标版本决定)。相邻同样式段会合并。
    pub fn to_spans(&self) -> Vec<Span> {
        let mut out = Vec::new();
        flatten(self, ResolvedStyle::default(), &mut out);
        merge_adjacent(&mut out);
        out
    }

    /// 去样式的纯文本(日志 / 退化展示用)。
    pub fn plain(&self) -> String {
        self.to_spans().into_iter().map(|s| s.text).collect()
    }

    /// 从已解析的 JSON 值构造(裸串/数字/布尔/数组/对象都接住,绝不报错)。
    pub fn from_json(v: &serde_json::Value) -> Self {
        from_value(v)
    }
}

/// 自定义反序列化:接住裸字符串 / 数字 / 布尔 / 数组 / 对象五种形态,坏结构尽量降级而非报错。
impl<'de> Deserialize<'de> for Component {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(from_value(&v))
    }
}

fn from_value(v: &serde_json::Value) -> Component {
    use serde_json::Value;
    match v {
        Value::String(s) => Component::text(s.clone()),
        Value::Bool(b) => Component::text(b.to_string()),
        Value::Number(n) => Component::text(n.to_string()),
        Value::Array(arr) => {
            let mut it = arr.iter();
            let mut root = it.next().map(from_value).unwrap_or_default();
            for child in it {
                root.extra.push(from_value(child));
            }
            root
        }
        Value::Object(obj) => {
            let get_str = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
            let arr = |k: &str| {
                obj.get(k)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(from_value).collect())
                    .unwrap_or_default()
            };
            Component {
                text: get_str("text").unwrap_or_default(),
                translate: get_str("translate"),
                with: arr("with"),
                fallback: get_str("fallback"),
                extra: arr("extra"),
                style: parse_style(obj),
            }
        }
        Value::Null => Component::default(),
    }
}

fn parse_style(obj: &serde_json::Map<String, serde_json::Value>) -> Style {
    let as_bool = |k: &str| {
        obj.get(k).and_then(|v| {
            v.as_bool().or_else(|| match v.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
        })
    };
    Style {
        color: obj.get("color").and_then(serde_json::Value::as_str).and_then(color::parse),
        bold: as_bool("bold"),
        italic: as_bool("italic"),
        underlined: as_bool("underlined"),
        strikethrough: as_bool("strikethrough"),
        obfuscated: as_bool("obfuscated"),
        font: obj.get("font").and_then(serde_json::Value::as_str).map(str::to_string),
    }
}

fn resolve(inherited: ResolvedStyle, s: &Style) -> ResolvedStyle {
    ResolvedStyle {
        color: s.color.or(inherited.color),
        bold: s.bold.unwrap_or(inherited.bold),
        italic: s.italic.unwrap_or(inherited.italic),
        underlined: s.underlined.unwrap_or(inherited.underlined),
        strikethrough: s.strikethrough.unwrap_or(inherited.strikethrough),
        obfuscated: s.obfuscated.unwrap_or(inherited.obfuscated),
    }
}

fn flatten(comp: &Component, inherited: ResolvedStyle, out: &mut Vec<Span>) {
    let current = resolve(inherited, &comp.style);

    // 内容文本:优先 text;否则 translate 取 fallback / 键名,并用 with 参数填 %s / %N$s
    //(MOTD 罕用 translate,且无 lang 文件无法真正本地化;keybind/score/selector/nbt 需客户端或世界
    // 上下文,服务器在状态里给不了,故按空处理——与原版在列表里的表现一致)。
    let content: String = if !comp.text.is_empty() {
        comp.text.clone()
    } else if let Some(t) = &comp.translate {
        let template = comp.fallback.as_deref().unwrap_or(t.as_str());
        apply_with(template, &comp.with)
    } else {
        String::new()
    };
    if !content.is_empty() {
        expand_section(&content, current, out);
    }

    for child in &comp.extra {
        flatten(child, current, out);
    }
}

/// 把 translate 模板里的 `%s`(顺序)/ `%N$s`(按位)用 with 参数的纯文本替换。`%%` → `%`。
fn apply_with(template: &str, with: &[Component]) -> String {
    if with.is_empty() || !template.contains('%') {
        return template.to_string();
    }
    let args: Vec<String> = with.iter().map(Component::plain).collect();
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    let mut seq = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('s') => {
                chars.next();
                if let Some(a) = args.get(seq) {
                    out.push_str(a);
                }
                seq += 1;
            }
            Some(d) if d.is_ascii_digit() => {
                let mut num = String::new();
                while let Some(&d2) = chars.peek() {
                    if d2.is_ascii_digit() {
                        num.push(d2);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&'$') {
                    chars.next();
                    if chars.peek() == Some(&'s') {
                        chars.next();
                    }
                }
                if let Ok(idx) = num.parse::<usize>()
                    && let Some(a) = idx.checked_sub(1).and_then(|i| args.get(i))
                {
                    out.push_str(a);
                }
            }
            _ => out.push('%'),
        }
    }
    out
}

/// 在一段正文里展开内嵌 `§` 码,产出若干带样式 span。
fn expand_section(text: &str, base: ResolvedStyle, out: &mut Vec<Span>) {
    let mut cur = base;
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '§' {
            if let Some(code) = chars.next() {
                push_span(out, &mut buf, cur);
                apply_code(&mut cur, code, &mut chars, base);
            }
            // 末尾孤零零的 § 直接丢弃
        } else {
            buf.push(c);
        }
    }
    push_span(out, &mut buf, cur);
}

fn push_span(out: &mut Vec<Span>, buf: &mut String, style: ResolvedStyle) {
    if !buf.is_empty() {
        out.push(Span { text: std::mem::take(buf), style });
    }
}

fn apply_code(cur: &mut ResolvedStyle, code: char, chars: &mut Peekable<Chars>, base: ResolvedStyle) {
    let lower = code.to_ascii_lowercase();
    if lower == 'x' {
        // BungeeCord 风格 §x§R§R§G§G§B§B:再吃 6 组「§ + 十六进制位」。
        let mut probe = chars.clone();
        let mut hex = String::new();
        let mut ok = true;
        for _ in 0..6 {
            match (probe.next(), probe.next()) {
                (Some('§'), Some(h)) if h.is_ascii_hexdigit() => hex.push(h),
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            for _ in 0..12 {
                chars.next();
            }
            if let Some((r, g, b)) = color::parse_hex(&hex) {
                *cur = with_color(Color::Rgb(r, g, b));
            }
        }
        return;
    }
    if let Some(idx) = color::index_by_code(lower) {
        *cur = with_color(Color::Named(idx)); // 颜色码清空样式标志
        return;
    }
    match lower {
        'k' => cur.obfuscated = true,
        'l' => cur.bold = true,
        'm' => cur.strikethrough = true,
        'n' => cur.underlined = true,
        'o' => cur.italic = true,
        // §g = Minecoin Gold(Bedrock 专有色,Java MOTD 不用此码,故全局加无副作用)。其余 Bedrock
        // 材质色(§h–§w,非连续且 §m/§n 与 Java 样式码冲突)实战 MOTD 几乎不出现,留作未知码忽略。
        'g' => *cur = with_color(Color::Rgb(0xDD, 0xD6, 0x05)),
        // §r 复位到本节点解析后的样式(原版 StringDecomposer 用 selfStyle,非全空)
        'r' => *cur = base,
        _ => {} // 未知码忽略
    }
}

fn with_color(c: Color) -> ResolvedStyle {
    ResolvedStyle { color: Some(c), ..Default::default() }
}

fn merge_adjacent(spans: &mut Vec<Span>) {
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for sp in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if last.style == sp.style => last.text.push_str(&sp.text),
            _ => merged.push(sp),
        }
    }
    *spans = merged;
}

/// 把 span 序列转成 24 位 ANSI 彩色字符串(终端 / 日志查看用)。obfuscated 段原样出字。
pub fn to_ansi(spans: &[Span]) -> String {
    let mut s = String::new();
    for sp in spans {
        let mut codes: Vec<String> = Vec::new();
        if let Some(c) = sp.style.color {
            let (r, g, b) = c.rgb();
            codes.push(format!("38;2;{r};{g};{b}"));
        }
        if sp.style.bold {
            codes.push("1".into());
        }
        if sp.style.italic {
            codes.push("3".into());
        }
        if sp.style.underlined {
            codes.push("4".into());
        }
        if sp.style.strikethrough {
            codes.push("9".into());
        }
        if !codes.is_empty() {
            s.push_str(&format!("\x1b[{}m", codes.join(";")));
        }
        s.push_str(&sp.text);
        s.push_str("\x1b[0m");
    }
    s
}
