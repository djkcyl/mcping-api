// SPDX-License-Identifier: AGPL-3.0-or-later
//! mcping-api —— Minecraft 服务器 ping + 原版风格出图的独立 HTTP API。
//!
//! 核心是从 abot 抽出的自包含 [`minecraft`] 集成(SLP/RakNet ping + MOTD/选服界面/数据卡渲染),
//! 这里只加一层薄 HTTP 壳。三个端点:
//! - `GET /ping?host=<地址>[&edition=auto|je|be]` —— 查询结果 JSON。
//! - `GET /image?host=<地址>[&style=screen|card][&edition=…][&name=…]` —— 出一张 PNG。
//! - `GET /list?hosts=a,b,c[&names=A,B,C][&edition=…]` —— 批量并行 ping,出原版列表长图 PNG。
//!
//! `edition` 默认 `auto`:先按 Java(SLP/TCP)查,连不上再换基岩(RakNet/UDP)。监听地址用环境
//! 变量 `BIND`(默认 `0.0.0.0:8686`)。

use axum::Json;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};

// 集成在 lib 里(`src/lib.rs` 的 `pub mod minecraft`),其 pub 项即公开 API,无 dead_code/未用导出告警。
use mcping_api::minecraft::{self, BedrockOptions, CardOptions, ListEntry, PingResult, ScreenOptions};

// ───────────────────────────── ping 取数(自动嗅探) ─────────────────────────────

/// 查询版本。`Auto` = 先 Java、连不上再基岩。
#[derive(Clone, Copy)]
enum Edition {
    Auto,
    Java,
    Bedrock,
}

fn parse_edition(s: Option<&str>) -> Edition {
    match s.unwrap_or("auto").to_ascii_lowercase().as_str() {
        "je" | "java" => Edition::Java,
        "be" | "bedrock" => Edition::Bedrock,
        _ => Edition::Auto,
    }
}

/// 基岩 ping → 折算成统一 [`PingResult`]。
async fn bedrock(addr: &str) -> Result<PingResult, String> {
    minecraft::ping_bedrock(addr, &BedrockOptions::default())
        .await
        .map(|r| minecraft::bedrock::to_ping_result(&r))
        .map_err(|e| e.to_string())
}

/// 按版本取一次结果,返回 `(结果, "java"|"bedrock")`。`Err` 是给调用方看的提示串。
async fn fetch(addr: &str, ed: Edition) -> Result<(PingResult, &'static str), String> {
    match ed {
        Edition::Java => minecraft::ping(addr).await.map(|r| (r, "java")).map_err(|e| e.to_string()),
        Edition::Bedrock => bedrock(addr).await.map(|r| (r, "bedrock")),
        Edition::Auto => match minecraft::ping(addr).await {
            Ok(r) => Ok((r, "java")),
            Err(je) => bedrock(addr).await.map(|r| (r, "bedrock")).map_err(|be| format!("JE: {je} | BE: {be}")),
        },
    }
}

// ───────────────────────────── 端点 ─────────────────────────────

#[derive(Deserialize)]
struct PingParams {
    host: String,
    edition: Option<String>,
}

#[derive(Serialize)]
struct PingResponse {
    host: String,
    port: u16,
    via_srv: bool,
    edition: &'static str,
    online: i64,
    max: i64,
    version: Option<String>,
    protocol: i32,
    latency_ms: Option<u128>,
    motd: String,
    favicon: Option<String>,
    players: Vec<String>,
}

fn to_json(edition: &'static str, r: &PingResult) -> PingResponse {
    let (online, max, players) = match &r.status.players {
        Some(p) => (p.online, p.max, p.sample.iter().map(|pl| pl.name.clone()).collect()),
        None => (0, 0, Vec::new()),
    };
    PingResponse {
        host: r.address.host.clone(),
        port: r.address.port,
        via_srv: r.address.via_srv,
        edition,
        online,
        max,
        version: r.status.version.name.clone(),
        protocol: r.status.version.protocol,
        latency_ms: r.latency.map(|d| d.as_millis()),
        motd: r.status.description.plain(),
        favicon: r.status.favicon.clone(),
        players,
    }
}

async fn ping_handler(Query(p): Query<PingParams>) -> Response {
    match fetch(&p.host, parse_edition(p.edition.as_deref())).await {
        Ok((r, ed)) => Json(to_json(ed, &r)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct ImageParams {
    host: String,
    edition: Option<String>,
    style: Option<String>,
    name: Option<String>,
}

async fn image_handler(Query(p): Query<ImageParams>) -> Response {
    let r = match fetch(&p.host, parse_edition(p.edition.as_deref())).await {
        Ok((r, _)) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
    };
    let title = p.name.clone();
    let png = match p.style.as_deref().unwrap_or("screen") {
        "card" => minecraft::render_server_card_png(&r, &CardOptions { title, ..Default::default() }),
        _ => minecraft::render_select_server_png(&r, &ScreenOptions { title, ..Default::default() }),
    };
    match png {
        Ok(bytes) => png_response(bytes),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ListParams {
    hosts: String,
    names: Option<String>,
    edition: Option<String>,
    /// `screen`(默认,原版列表整屏)或 `card`(每台数据卡竖排)。
    style: Option<String>,
}

async fn list_handler(Query(p): Query<ListParams>) -> Response {
    let hosts: Vec<String> =
        p.hosts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if hosts.is_empty() {
        return (StatusCode::BAD_REQUEST, "no hosts").into_response();
    }
    let names: Vec<String> = p.names.unwrap_or_default().split(',').map(|s| s.trim().to_string()).collect();
    let ed = parse_edition(p.edition.as_deref());

    // 全部并行 ping(每台一个任务);连不上的也进图(unreachable 样式)。
    let tasks: Vec<_> = hosts
        .into_iter()
        .enumerate()
        .map(|(i, host)| {
            let name = names.get(i).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| host.clone());
            tokio::spawn(async move {
                let result = fetch(&host, ed).await.ok().map(|(r, _)| r);
                ListEntry { name, result }
            })
        })
        .collect();
    let mut entries = Vec::with_capacity(tasks.len());
    for t in tasks {
        if let Ok(e) = t.await {
            entries.push(e);
        }
    }

    if p.style.as_deref() == Some("card") {
        // 卡片样式:每台一张数据卡竖排;连不上的没结果、出不了卡,跳过并经 X-Unreachable-Count 头告知。
        let down = entries.iter().filter(|e| e.result.is_none()).count();
        let cards: Vec<_> = entries
            .iter()
            .filter_map(|e| {
                e.result.as_ref().map(|r| {
                    let opts = CardOptions { title: Some(e.name.clone()), ..Default::default() };
                    minecraft::render_server_card(r, &opts)
                })
            })
            .collect();
        if cards.is_empty() {
            return (StatusCode::BAD_GATEWAY, "all hosts unreachable").into_response();
        }
        let stacked = minecraft::render::stack_vertical(&cards, 16, [20, 20, 24, 255]);
        return match minecraft::render::encode_png(&stacked) {
            Ok(bytes) => {
                let mut resp = png_response(bytes);
                if down > 0 {
                    resp.headers_mut().insert("x-unreachable-count", down.to_string().parse().unwrap());
                }
                resp
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // screen 样式(默认):原版列表整屏长图,连不上的也进图(unreachable 样式)。
    match minecraft::render_server_list_png(&entries, &ScreenOptions::default()) {
        Ok(bytes) => png_response(bytes),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn png_response(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")], bytes).into_response()
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>mcping-api</title>
<style>
  :root { color-scheme: dark; }
  body { background:#16161a; color:#d8d8dc; font:15px/1.6 system-ui,"Segoe UI",sans-serif; max-width:880px; margin:0 auto; padding:32px 20px 64px; }
  h1 { font-size:24px; margin:0 0 4px; }
  h2 { font-size:18px; margin:34px 0 8px; border-bottom:1px solid #2c2c34; padding-bottom:6px; }
  .lead { color:#9a9aa4; margin:0 0 8px; }
  code, pre { font-family:ui-monospace,"SF Mono",Consolas,monospace; }
  code { background:#23232b; padding:1px 5px; border-radius:4px; font-size:13px; }
  pre { background:#1d1d23; border:1px solid #2c2c34; border-radius:8px; padding:12px 14px; overflow:auto; font-size:13px; }
  .ep { font-size:15px; }
  .m { color:#55ff55; font-weight:600; }
  table { border-collapse:collapse; width:100%; margin:8px 0; font-size:13.5px; }
  th,td { text-align:left; padding:5px 10px; border-bottom:1px solid #26262e; vertical-align:top; }
  th { color:#9a9aa4; font-weight:600; }
  td:first-child code { background:#2a2a44; }
  a { color:#79b8ff; }
  .try a { display:inline-block; margin:2px 8px 2px 0; }
  .muted { color:#9a9aa4; }
</style>
</head>
<body>
<h1>mcping-api</h1>
<p class="lead">Minecraft 服务器 ping + 原版风格出图的 HTTP API。Java(SLP/TCP)与基岩(RakNet/UDP)皆可,纯 Rust 自包含、无数据库。</p>
<p class="muted">通用参数 <code>edition</code>:<code>auto</code>(默认,先 Java 连不上换基岩)/ <code>je</code> / <code>be</code>。地址形如 <code>host</code>、<code>host:port</code>、<code>1.2.3.4:25565</code>;域名不带端口时走 <code>_minecraft._tcp</code> SRV。</p>

<h2><span class="m">GET</span> /ping <span class="muted">→ JSON</span></h2>
<p class="ep">查一台服务器,返回结构化数据。</p>
<table>
  <tr><th>参数</th><th>说明</th></tr>
  <tr><td><code>host</code></td><td>服务器地址(必填)</td></tr>
  <tr><td><code>edition</code></td><td><code>auto</code> / <code>je</code> / <code>be</code></td></tr>
</table>
<p class="muted">字段:<code>host port via_srv edition online max version protocol latency_ms motd favicon players</code>。</p>
<pre>curl 'http://HOST:8686/ping?host=mc.hypixel.net'</pre>
<p class="try"><strong>试一下:</strong>
  <a href="/ping?host=mc.hypixel.net">/ping?host=mc.hypixel.net</a>
  <a href="/ping?host=2b2t.org">/ping?host=2b2t.org</a>
</p>

<h2><span class="m">GET</span> /image <span class="muted">→ PNG</span></h2>
<p class="ep">出一张图。</p>
<table>
  <tr><th>参数</th><th>说明</th></tr>
  <tr><td><code>host</code></td><td>服务器地址(必填)</td></tr>
  <tr><td><code>style</code></td><td><code>screen</code>(默认,原版 1.16.5 选服整屏)/ <code>card</code>(完整数据卡:图标 + 5 行含延迟·版本·模组·Via + 玩家 sample)</td></tr>
  <tr><td><code>name</code></td><td>覆盖条目展示名(默认用地址)</td></tr>
  <tr><td><code>edition</code></td><td><code>auto</code> / <code>je</code> / <code>be</code></td></tr>
</table>
<pre>curl 'http://HOST:8686/image?host=mc.hypixel.net&name=Hypixel' -o hy.png</pre>
<p class="try"><strong>试一下:</strong>
  <a href="/image?host=mc.hypixel.net&name=Hypixel">screen</a>
  <a href="/image?host=mc.hypixel.net&style=card">card</a>
</p>

<h2><span class="m">GET</span> /list <span class="muted">→ PNG(长图)</span></h2>
<p class="ep">并行 ping 多台,出一张长图(条目多就拉高)。</p>
<table>
  <tr><th>参数</th><th>说明</th></tr>
  <tr><td><code>hosts</code></td><td>逗号分隔的地址列表(必填)</td></tr>
  <tr><td><code>names</code></td><td>逗号分隔的展示名,按位置对应 <code>hosts</code>(可省)</td></tr>
  <tr><td><code>style</code></td><td><code>screen</code>(默认,原版列表样式,连不上的也进图)/ <code>card</code>(每台数据卡竖排,连不上的跳过、经响应头 <code>X-Unreachable-Count</code> 告知数量)</td></tr>
  <tr><td><code>edition</code></td><td><code>auto</code> / <code>je</code> / <code>be</code></td></tr>
</table>
<pre>curl 'http://HOST:8686/list?hosts=mc.hypixel.net,2b2t.org&names=Hypixel,2b2t' -o list.png
curl 'http://HOST:8686/list?hosts=mc.hypixel.net,2b2t.org&style=card'        -o cards.png</pre>
<p class="try"><strong>试一下:</strong>
  <a href="/list?hosts=mc.hypixel.net,2b2t.org&names=Hypixel,2b2t">screen</a>
  <a href="/list?hosts=mc.hypixel.net,2b2t.org&style=card">card</a>
</p>

<p class="muted" style="margin-top:40px">渲染栈与 1.16.5 GUI 贴图/原版字体抽自 <code>abot</code> 的 minecraft 集成,内嵌于二进制。</p>
</body>
</html>
"##;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = Router::new()
        .route("/", get(index))
        .route("/ping", get(ping_handler))
        .route("/image", get(image_handler))
        .route("/list", get(list_handler));

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8686".to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind failed");
    tracing::info!("mcping-api listening on http://{bind}");
    axum::serve(listener, app).await.unwrap();
}
