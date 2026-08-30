# 各平台实现方案

匹配核心 `em-core` 是纯 Rust、无系统依赖，四个平台共用同一份。
平台差异只集中在**两件事**：怎么拿到画面，怎么把结果显示上去。

| | 拿画面 | 显示 | 状态 |
|---|---|---|---|
| macOS | xcap 抓游戏窗口（需屏幕录制授权） | 透明置顶点击穿透窗（`content_protected` 防自拍） | 已实现并实机验证 |
| Windows | xcap（Windows Graphics Capture / BitBlt） | 同上，底层是 `WS_EX_LAYERED\|WS_EX_TRANSPARENT` | 代码通用，待实机验证 |
| Linux | xcap（X11 可用；Wayland 受限） | 同上（X11 下正常，Wayland 合成器可能不支持置顶穿透） | 代码通用，待实机验证 |
| Android | MediaProjection 投屏授权（前台服务） | SYSTEM_ALERT_WINDOW 悬浮窗（叠加层 + 可触摸控制条） | 已实现，模拟器端到端验证通过 |

## 桌面（macOS / Windows / Linux）

Tauri 2 一套代码三端跑，差异点只有抓屏与覆盖窗的底层映射。

- **拿画面**：优先抓游戏窗口（`GAME_HINTS`：第五人格 / identityv / dwrg / wine，
  按窗口标题与进程名匹配），抓不到退回主屏。与前台无关（自己的窗口挡住也没事），
  抓窗口时覆盖层不可能进画面；退回抓主屏就全指望 `content_protected` 挡住，
  挡不住会自己识别自己，日志里单独记一笔便于排障。
  `EM_FORCE_MONITOR=1` 可跳过找窗口直接抓主屏——把一张游戏截图摆到屏幕上
  就能复现整条链路，不必真的开着游戏。
- **显示**：透明/无边框/置顶/点击穿透（`set_ignore_cursor_events`）/
  防捕获（`set_content_protected`）的覆盖窗加载 `overlay.html`，
  按「整层显示区域」（屏幕物理像素）定位定尺寸，先 show 再发数据
  （`overlay-ready` 事件兜底首帧）。应用内的画布（识别预览与「看整层」）
  支持缩放：Ctrl/⌘+滚轮、触屏双指捏合、按钮、双击切换，25%–600% 记 localStorage。
- **诊断**：前端的关键判断（命中、叠加层为何未推送等）经 `log_line` 命令
  与后端日志汇入同一条 stderr 时间线；发行版里 WebView 的 console 看不见，
  排障全靠它。全局热键（默认 ⇧⌥C 抓屏、⇧⌥M 开关叠加层、⇧⌥R 重识别）
  可在界面上改，存 `hotkeys.json`。

### Windows 注意事项

代码层面**不需要改**：`xcap`、Tauri 的透明/置顶/点击穿透、全局热键都是跨平台的。
几个平台特有的点：

- **防自拍**：`set_content_protected(true)` 在 Windows 上映射到
  `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`，需要 Windows 10 2004+。
  更老的系统上覆盖层会被自己抓进去——`em-core` 里那个「分数 >0.97 告警」就是
  给这种情况兜底的信号，触发时应改用「抓屏前先隐藏覆盖层」的策略
  （Android 端正是这么做的，见下）。
- **多显示器 / DPI**：抓的是游戏窗口，坐标换算走
  `窗口逻辑坐标 × (物理宽/逻辑宽)`，与 macOS 同一套代码，混合 DPI 也成立。
- **窗口识别**：`GAME_HINTS` 目前含 `dwrg`（进程名 `dwrg.exe`）和 `第五人格`，
  Windows 原生客户端两者都能命中，不必改。
- **SmartScreen**：未签名安装包首次运行会拦，用户需点「更多信息 → 仍要运行」。
  要消除需买代码签名证书（EV 证书才能立即免提示）。
- **反作弊观感**：Windows 端反作弊比 macOS 严格，透明置顶窗**理论上可被检测**。
  因此并排窗口模式始终保留为默认，覆盖模式是用户自选项。

## Android

Android 上没有「窗口」概念可用，两件事都得换实现，但 `em-core` 一行不用改。
APK 可构建（arm64-v8a + x86_64，约 34 MB），模拟器（android-34 / arm64-v8a）
端到端验证通过：投屏授权 → 取帧 → 识别 → 悬浮窗叠加，识别结果与桌面一致
（同一张截图桌面 0.837 / Android 0.838）。启动自检在 ARM 上跑一次合成匹配，
确认掩码运算、FFT、rayon 均可用：

```
[em] 数据包目录：/data/user/0/net.yeah.enceka.embermap/bundle
[em] 自检：参考库 39 个楼层条目
[em] 自检：全库匹配耗时 2.47s，结果 右中门1-2 1f 得分 0.970 —— 正确
```

### 拿画面：MediaProjection + CaptureService

`CaptureService`（`foregroundServiceType="mediaProjection"` 前台服务）持有
整条管线，插件 `CapturePlugin`（`emcapture`）只负责拉授权与转发命令：

- **启动顺序**：Android 14+ 必须先 `startForeground()` 再
  `getMediaProjection()`，否则 SecurityException。插件不能在主线程 sleep 等
  服务启动（`onStartCommand` 同在主线程，会饿死自己），投屏创建整个放进服务内
  顺序执行，插件用 Handler 非阻塞轮询就绪。
- **方形缓冲区，一次建成永不改动**：`ImageReader` 边长 = 屏幕长边的正方形。
  旋转时 `resize()+setSurface()` 的方案被实测否决——镜像仍按旧尺寸绘制，
  内容被压进缓冲区一角；重建 `VirtualDisplay` 又被系统禁止（同一投影令牌
  只能建一次，SecurityException）。方形缓冲区在两种朝向下都按 1:1 镜像
  （只是留黑边），不损失分辨率。
- **黑边还是拉伸，不猜，检测**：方形缓冲区里横屏内容有两种呈现方式——
  留黑边（内容居中）或拉伸填满（纵向拉长 2.2 倍），匹配器只允许等比缩放，
  后者不纠正必然认错。按屏幕尺寸算出「若留黑边则内容应在的矩形」，
  检查矩形外是否确实全黑：是则裁掉黑边，否则按拉伸处理整体还原比例。
- **廉价跳帧**：取一帧干净画面必须先藏悬浮窗（用户看得见闪烁），而地图开着
  不动时根本无需重算。于是先用 48×48 灰度指纹（直接读 ByteBuffer，不建
  Bitmap）探一下画面动没动：没变整轮 skip，稳态零闪烁；连续 12 轮
  「没变」强制重算一次，防阈值偏钝让叠加层跟不动。
- **取帧瞬间藏悬浮窗**：拿到原始像素立刻恢复，裁剪/缩放/JPEG/写盘
  （合计几百毫秒）全在内存里做。**不用 `FLAG_SECURE`**——它能让悬浮窗
  不进投屏画面，但用户也没法截图核对叠加位置了；藏窗的闪烁由「只在该藏的
  60ms 里藏」压到最低。
- **取帧分辨率不能为省时间而降**：上限从 2000 降到 1200 时，多次重采样
  叠加 JPEG 压缩把识别分数从 0.84 压到 0.73，直接掉出置信门槛；
  「高保真取帧 2000 + 低分辨率分析（`target_long_edge` 1000）」两头都占。
- **走文件不走桥**：整帧 RGB base64 过 JSON 桥代价过高，存成 JPEG
  （200-400 KB）交路径，Rust 侧 `image` 解码——识别本就对 JPEG 不敏感
  （参考库素材本身就是 JPEG）。

移动端 `Options` 相应收窄：`target_long_edge` 1000、精修长边 380、
精修候选 3-6。实测单帧 1-2 秒（模拟器 3-3.6 秒）。

### 显示：SYSTEM_ALERT_WINDOW 悬浮窗 ×2

悬浮窗必须分**两个窗口**，因为触摸语义相反，一个窗口做不到
「一部分穿透一部分不穿透」：

- **叠加层**（`OverlayView`）：`FLAG_NOT_FOCUSABLE | FLAG_NOT_TOUCHABLE`，
  触摸完全穿透到游戏（等价桌面端 `set_ignore_cursor_events`）。
  内容 Kotlin Canvas 绘制——手绘图黑底按亮度转 alpha、门位圆圈用系统字体画
  中文。几何（view/tf/门位）由 Rust 侧 `overlay_geometry` 推导，
  Android 按「截图 px = 屏幕 px × scale」整体换算，漏掉 tf 与门位的换算
  等于「窗口摆对了、里面的图按截图尺度画」。
- **控制条**（`ControlView`）：`NOT_FOCUSABLE`（不抢焦点）但不
  `NOT_TOUCHABLE`——要收下自己范围内的触摸。按钮：抓屏匹配（连拍至多 6 帧
  直到锁定）、叠加层开关、整层开关、重识、✕；状态行显示识别进度。
  **默认不显示**：是否打开由用户在应用内勾选「控制悬浮窗」（存 localStorage），
  ✕ 只收控制条本身，不连坐叠加层。
  - 拖动全程钳在屏内，拖不丢；拖到屏幕边缘松手**收起成小柄**，点小柄展开。
  - 「整层」卡片先等比适配（宽 ≤ 屏 2/3、高 ≤ 半屏），可双指捏合放大
    （≤8×）、拖动查看、双击回适配。
  - 按钮事件进队列由前端 `poll_control` 轮询取走（250ms 一次，只在投屏期间
    转）——推送路径 `Plugin.trigger` 需要 JS 侧 `addPluginListener`，
    被应用内联插件没有权限清单的 ACL 拦下。
- **窗口失效自愈**：`WindowManager` 的窗口被系统摘除后旧引用不能再
  `updateViewLayout`，检测到 `isAttachedToWindow == false` 就摘掉重建；
  悬浮窗路径的异常捕获收严为 `Throwable`，异常不上抛成进程崩溃。
- **通知栏兜底**：投屏服务的常驻通知带「开关叠加层」「停止投屏」两个动作，
  供控制条被收起或被挡住时使用；动作排进同一队列，前端不必分辨来源。

触摸穿透之外，横屏时还有挖孔区的坑：`LayoutParams` 不声明
`layoutInDisplayCutoutMode = ALWAYS` 的话窗口会被系统整体推开，偏移可达
上百像素，叠加层就对不准。

### 资源与构建

Tauri 的 `resource_dir()` 在 Android 上返回 `asset://` URI，`std::fs`
读不了。`MainActivity` 启动时把 `assets/bundle` 解压到 `dataDir/bundle`
（以 `bundle.json` 大小作版本标记避免重复拷贝），Rust 侧从
`app_data_dir()/bundle` 读。用 `cargo tauri android init` 而非 `npx`
初始化，否则 Gradle 任务硬编码调用 `npm`，而 `src-tauri/` 下没有
package.json。

```bash
export ANDROID_HOME=<sdk> NDK_HOME=<sdk>/ndk/<版本>
cargo tauri android build --target aarch64 --target x86_64 --apk
```

### 合规

Android 端的悬浮窗叠在游戏上，观感上最接近「外挂」。本方案不读内存、
不注入、不模拟点击，只处理用户主动授权的投屏画面。因此**悬浮窗默认关闭**——
叠加层与控制条都由用户明确开启（这正是现在的实现），自动监测在手机上
也默认关闭，识别由用户按需触发。
