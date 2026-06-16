//! Minecraft 服务器 ping(Server List Ping)+ MOTD 渲染 —— 纯 Rust,自包含。
//!
//! 两件交付物:
//! 1. **SLP ping 客户端**([`protocol`]):TCP 连服、跑现代握手状态流程、把状态 JSON 解析成
//!    [`StatusResponse`]、Ping/Pong 测延迟,可选 SRV([`srv`])。状态态不压缩不加密。
//! 2. **MOTD 渲染器**([`render`]):把 `description` 文本组件([`component`])摊平成带样式
//!    span,再像素级忠实栅格成 PNG,或转 ANSI/纯文本。
//!
//! 适配版本 1.8.9(协议 47)/ 1.12.2(340)/ 1.16.5(754)/ 26.1(775)。`description` 到 26.1
//! 仍是 JSON 文本组件(不是 NBT),故无需 NBT 解析。hex 色仅 1.16+ 客户端识别,渲染老目标时按
//! [`render::OldColorPolicy`] 自行降级。
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use mcping_api::minecraft;
//! let r = minecraft::ping("mc.hypixel.net").await?;
//! println!("{}/{} 在线", r.status.players.as_ref().map(|p| p.online).unwrap_or(0),
//!          r.status.players.as_ref().map(|p| p.max).unwrap_or(0));
//! let png = minecraft::render::render_motd_png(&r.status.description.to_spans(), &Default::default())?;
//! std::fs::write("motd.png", png)?;
//! # Ok(()) }
//! ```

pub mod bedrock;
pub mod codec;
pub mod color;
pub mod component;
pub mod font;
pub mod forge;
pub mod legacy;
pub mod protocol;
pub mod query;
pub mod render;
pub mod srv;
pub mod status;
pub mod versions;

pub use bedrock::{BedrockOptions, BedrockResult, BedrockStatus, ping_bedrock};
pub use component::{Component, ResolvedStyle, Span};
pub use protocol::{
    PingError, PingOptions, PingResult, ResolvedAddress, ping, ping_as, ping_resolved, ping_sync,
    ping_with,
};
pub use render::{
    CardOptions, ListEntry, OldColorPolicy, RenderOptions, ScreenOptions, TargetVersion,
    render_motd, render_motd_png, render_select_server_png, render_select_server_screen,
    render_server_card, render_server_card_png, render_server_list_png, render_server_list_screen,
    visual_lines,
};
pub use status::{Player, Players, StatusResponse, Version};

/// 各热门版本的协议号(握手里发的、状态里上报的整数)。
pub mod protocol_version {
    pub const V1_8_9: i32 = 47;
    pub const V1_12_2: i32 = 340;
    pub const V1_16_5: i32 = 754;
    pub const V26_1: i32 = 775;
}
