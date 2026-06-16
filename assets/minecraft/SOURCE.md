# Minecraft 渲染字体资源

`mcfont.bin` — 原版默认字体(变宽像素字体),从 Minecraft Java 26.1.2 客户端 jar 的字体资源烘焙。

- 来源:官方客户端 jar 内 `assets/minecraft/font/`(`include/default.json` 的三张位图表
  `ascii.png` 8×8 ascent7、`accented.png` 9×12 ascent10、`nonlatin_european.png` 8×8 ascent7)
  + `include/space.json`(空格步进 4)。按 default.json 的字符表与 first-wins 优先级合并。
- 度量取自反编译的原版源码(`BitmapProvider`):每字形步进 = 最右非透明列+1 再 +1。
- 用途:MOTD 渲染器的拉丁/默认字形(原版那套像素字体),空格步进 4 → 服务器的空格伪居中照样对齐。
- 许可:Mojang 美术资源,随客户端;本仓仅作渲染复刻用途内嵌。

格式(小端):
  magic   = "MCF2" (4 字节)
  count   = u32     (字形数 N)
  records = N × { cp:u32, advance:u8, width:u8, height:u8, ascent:u8, bitmap:height×u16(LE) }
  位图每行 u16,bit15 = 最左像素;records 按 cp 升序。

---

`unifont_bmp.bin` — GNU Unifont 17.0.04 的 BMP(平面 0)位图,从官方 `.hex` 转出的紧凑二进制。

- 上游:https://unifoundry.com/pub/unifont/unifont-17.0.04/font-builds/unifont-17.0.04.hex.gz
- 许可:SIL OFL 1.1 / GNU GPLv2+ with font exception(随上游)
- 用途:MOTD 渲染器的 CJK/unicode 字形回退,等价于 MC 的 unicode 字体。

格式(小端):
  magic   = "UFB1" (4 字节)
  count   = u32     (字形数 N)
  records = N × { cp:u16, cell_width:u8(8|16), bitmap:[u8; cell_width*2] }
  位图按行优先,每行 MSB 为最左像素;共 16 行。records 按 cp 升序。

---

---

`gui/` —— 1.16.5「Play Multiplayer」选服界面复刻用的原版 GUI 贴图,取自 **Minecraft Java 1.16.5** 客户端 jar
(`piston-data.mojang.com` 官方下载,sha1 37fd3c90…)的 `assets/minecraft/textures/gui/`:

- `options_background.png`(16×16)—— 菜单泥土背景:整屏按原版 `renderDirtBackground` 平铺 32px 格(2×)乘
  0.25(0x404040),列表视口再乘 0.125(0x202020)更暗。
- `button_1165.png`(从 `widgets.png` 裁出按钮区 0,66,200×20)—— 页脚按钮,按原版左右两半铺到目标宽。
- 度量(标题 y=20、列表 top=32/itemHeight=36/getRowWidth=305、按钮逐颗定位、Cancel 宽 75 等)取自反编译的
  1.16.5 `MultiplayerScreen`/`MultiplayerServerListWidget`。信号格沿用 `sprites/ping_*.png`(各版本观感一致)。
- 许可:Mojang 美术资源,随客户端;本仓仅作渲染复刻用途内嵌。
