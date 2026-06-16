//! Status Response 的 JSON 模型 —— 贴近原版 lenient Codec 的容错口径。
//!
//! 对**类型**宽松(乱报的服务器会把 `protocol`/`online`/`max` 发成字符串甚至浮点),对**取值**不清洗
//! (信任上报)。`description` 走 [`Component`] 自定义反序列化(裸串/对象/数组通吃)。模组信息按
//! `forgeData`(FML2 明文 / FML3 打包 `d`)→ `modinfo`(FML1)→ `isModded`(NeoForge)顺序识别。

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::de::{Deserialize, Deserializer};
use serde_json::Value;

use crate::minecraft::component::Component;
use crate::minecraft::forge;

#[derive(Debug, Clone, Default)]
pub struct StatusResponse {
    pub version: Version,
    pub players: Option<Players>,
    pub description: Component,
    pub favicon: Option<String>,
    pub enforces_secure_chat: Option<bool>,
    pub previews_chat: Option<bool>,
    pub prevents_chat_reports: Option<bool>,
    /// NeoForge 标记(`Codec.BOOL.lenientOptionalFieldOf("isModded")`),仅表示「带模组」,无模组明细。
    pub is_modded: Option<bool>,
    /// Forge FML1(1.12 及更早)。
    pub modinfo: Option<Value>,
    /// Forge FML2/3(1.13+)。
    pub forge_data: Option<Value>,
    /// 整合包名 + 版本(Better Compatibility Checker 注入的顶层 `betterStatus`,客户端据此显示整合包
    /// 版本、判定是否匹配)。
    pub modpack: Option<Modpack>,
}

/// 整合包信息(BCC `betterStatus`)。
#[derive(Debug, Clone, Default)]
pub struct Modpack {
    pub name: String,
    pub version: String,
}

impl Modpack {
    fn from_value(v: &Value) -> Option<Modpack> {
        let o = v.as_object()?;
        // BCC 未配置时默认值是 "??"(不是 CHANGE_ME),归一为空。
        let norm = |k: &str| {
            let s = o.get(k).and_then(Value::as_str).unwrap_or_default().trim();
            if s.is_empty() || s == "??" { String::new() } else { s.to_string() }
        };
        let name = norm("name");
        let version = norm("version");
        if name.is_empty() && version.is_empty() {
            None
        } else {
            Some(Modpack { name, version })
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Version {
    /// 自由文本,服务器可写任意内容;协议号对不上时客户端在列表里把它标红。1.20+ 可缺省。
    pub name: Option<String>,
    pub protocol: i32,
    /// 非标准字段 `version.supportedVersions`:ViaVersion 在 `send-supported-versions: true` 时注入,
    /// 列出该服务器(经 Via)能接受的全部客户端协议号。原版没有这个字段,故空数组 = 普通服务器。
    pub supported_versions: Vec<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct Players {
    pub max: i64,
    pub online: i64,
    /// 在线玩家抽样,驱动列表悬浮提示;服务器常拿它塞自定义多行文本,UUID 也常是无效值。
    pub sample: Vec<Player>,
}

#[derive(Debug, Clone, Default)]
pub struct Player {
    pub name: String,
    pub id: String,
}

/// 逐字段从 JSON 值构造,**任何一个字段坏掉(类型不对、缺失)都不影响整体**——贴近原版 lenient
/// Codec 的「绝不因一个怪字段就让整个 ping 失败」原则。
impl<'de> Deserialize<'de> for StatusResponse {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Ok(Self::from_value(&v))
    }
}

impl StatusResponse {
    fn from_value(v: &Value) -> Self {
        let get = |k: &str| v.get(k);
        Self {
            version: get("version").map(Version::from_value).unwrap_or_default(),
            players: get("players").and_then(Players::from_value),
            description: get("description").map(Component::from_json).unwrap_or_default(),
            favicon: get("favicon").and_then(Value::as_str).map(str::to_string),
            enforces_secure_chat: get("enforcesSecureChat").and_then(as_bool_lenient),
            previews_chat: get("previewsChat").and_then(as_bool_lenient),
            prevents_chat_reports: get("preventsChatReports").and_then(as_bool_lenient),
            is_modded: get("isModded").and_then(as_bool_lenient),
            modinfo: get("modinfo").cloned(),
            forge_data: get("forgeData").cloned(),
            // BCC 现代版用 `betterStatus`(camelCase);旧版 Forge 1.20.1 用 `better-status`(连字符)。
            modpack: get("betterStatus").or_else(|| get("better-status")).and_then(Modpack::from_value),
        }
    }
}

impl Version {
    fn from_value(v: &Value) -> Self {
        let clamp_i32 = |n: i64| n.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        Version {
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            protocol: clamp_i32(v.get("protocol").map(as_i64_lenient).unwrap_or(0)),
            // ViaVersion 的 supportedVersions:数组里逐个宽松取整;非数组/缺失 → 空。
            supported_versions: v
                .get("supportedVersions")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|e| clamp_i32(as_i64_lenient(e))).collect())
                .unwrap_or_default(),
        }
    }
}

impl Players {
    /// 只有当 `players` 是对象时才返回 `Some`;sample 不是数组也只当空,绝不让整体解析失败。
    fn from_value(v: &Value) -> Option<Players> {
        let obj = v.as_object()?;
        let sample = obj
            .get("sample")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Player::from_value).collect())
            .unwrap_or_default();
        Some(Players {
            max: obj.get("max").map(as_i64_lenient).unwrap_or(0),
            online: obj.get("online").map(as_i64_lenient).unwrap_or(0),
            sample,
        })
    }
}

impl Player {
    fn from_value(v: &Value) -> Player {
        Player {
            name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
        }
    }
}

/// 模组加载器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModLoader {
    Forge,
    /// `isModded` 标记(多为 NeoForge,但该布尔是通用「带模组」,不确证 NeoForge)。
    Modded,
}

/// 从状态里识别出的模组信息。
#[derive(Debug, Clone)]
pub struct ServerMods {
    pub loader: ModLoader,
    /// (modId, version);可能为空(NeoForge 不报明细,或 `d` 解不出)。
    pub mods: Vec<(String, String)>,
    /// 已知模组数(解出时);`None` = 带模组但数量未知。
    pub mod_count: Option<usize>,
    pub channel_count: Option<usize>,
    pub truncated: bool,
}

impl StatusResponse {
    /// 解析 favicon 为图片字节。宽松处理:按 `base64,` 切(容忍 `image/jpeg` 等 MIME、甚至无前缀)、
    /// 滤掉所有空白/换行(1.13 前的数据 URI 换行变体),解码失败或空给 `None`。返回的字节交给
    /// `image` 解码,故实际尺寸/格式由渲染层判定(原版只认 64×64 PNG,但这里不因此丢弃)。
    pub fn favicon_png(&self) -> Option<Vec<u8>> {
        let data = self.favicon.as_deref()?;
        let b64 = match data.split_once("base64,") {
            Some((_, rest)) => rest,
            None => data, // 没前缀就当整段是 base64
        };
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        B64.decode(cleaned.as_bytes()).ok().filter(|b| !b.is_empty())
    }

    /// 识别模组信息(forgeData → modinfo → isModded)。非模组端给 `None`。
    pub fn mods(&self) -> Option<ServerMods> {
        if let Some(fd) = &self.forge_data {
            return Some(parse_forge_data(fd));
        }
        if let Some(mi) = &self.modinfo
            && mi.get("type").and_then(Value::as_str) == Some("FML")
        {
            let mods = mi
                    .get("modList")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| {
                                let id = m.get("modid").and_then(Value::as_str)?;
                                let ver = m.get("version").and_then(Value::as_str).unwrap_or("");
                                Some((id.to_string(), ver.to_string()))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let n = mods.len();
                return Some(ServerMods {
                    loader: ModLoader::Forge,
                    mods,
                    mod_count: Some(n),
                    channel_count: None,
                    truncated: false,
                });
        }
        if self.is_modded == Some(true) {
            return Some(ServerMods {
                loader: ModLoader::Modded,
                mods: Vec::new(),
                mod_count: None,
                channel_count: None,
                truncated: false,
            });
        }
        None
    }
}

fn parse_forge_data(fd: &Value) -> ServerMods {
    let channels = fd.get("channels").and_then(Value::as_array).map(|a| a.len());
    let truncated = fd.get("truncated").and_then(Value::as_bool).unwrap_or(false);

    // FML2:mods 数组明文 [{modId, modmarker}]
    if let Some(arr) = fd.get("mods").and_then(Value::as_array)
        && !arr.is_empty()
    {
        let mods: Vec<(String, String)> = arr
            .iter()
            .filter_map(|m| {
                let id = m.get("modId").and_then(Value::as_str)?;
                let ver = m.get("modmarker").and_then(Value::as_str).unwrap_or("");
                Some((id.to_string(), ver.to_string()))
            })
            .collect();
        let n = mods.len();
        return ServerMods {
            loader: ModLoader::Forge,
            mods,
            mod_count: Some(n),
            channel_count: channels,
            truncated,
        };
    }

    // FML3:mods 为空,明细打包在 d
    if let Some(d) = fd.get("d").and_then(Value::as_str)
        && let Some(decoded) = forge::decode_forge_d(d)
    {
        let n = decoded.mods.len();
        return ServerMods {
            loader: ModLoader::Forge,
            mods: decoded.mods,
            mod_count: Some(n),
            channel_count: channels.or(Some(decoded.channels)),
            truncated: truncated || decoded.truncated,
        };
    }

    // forgeData 在但取不出明细
    ServerMods {
        loader: ModLoader::Forge,
        mods: Vec::new(),
        mod_count: None,
        channel_count: channels,
        truncated,
    }
}

// ---- 宽松取值:数字 / 字符串 / 浮点 / 布尔 各种乱报都接住,取不到给缺省 ----

/// 数字 / 字符串数字 / 浮点 / null → i64(取不到给 0)。
fn as_i64_lenient(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>().ok().or_else(|| t.parse::<f64>().ok().map(|f| f as i64)).unwrap_or(0)
        }
        _ => 0,
    }
}

/// 布尔 / "true"/"false" 字符串 → bool;其它给 `None`。
fn as_bool_lenient(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| match v.as_str() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    })
}
