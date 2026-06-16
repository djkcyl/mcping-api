// SPDX-License-Identifier: AGPL-3.0-or-later
//! mcping —— Minecraft 服务器 ping + 原版风格出图,自包含(从 abot 的
//! `src/integrations/minecraft/` 整体抽出)。
//!
//! 作为**库** crate 暴露 [`minecraft`] 模块(SLP/RakNet ping + MOTD / 1.16.5 选服界面 / 数据卡渲染);
//! `src/main.rs` 是基于它的 HTTP API 二进制。把集成放在库里(而非 bin 内的私有 `mod`),其 `pub` 项即
//! 真正的公开 API,Rust 不再对「本 bin 没用到的导出」报 dead_code / unused_imports。

pub mod minecraft;
