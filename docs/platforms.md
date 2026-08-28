# 各平台实现方案

匹配核心 `em-core` 是纯 Rust、无系统依赖，四个平台共用同一份。
平台差异只集中在**两件事**：怎么拿到画面，怎么把结果显示上去。

| | 拿画面 | 显示 | 状态 |
|---|---|---|---|
| macOS | xcap 抓游戏窗口（需屏幕录制授权） | 透明置顶点击穿透窗（`content_protected` 防自拍） | 已实现并实机验证 |
| Windows | xcap（Windows Graphics Capture / BitBlt） | 同上，底层是 `WS_EX_LAYERED\|WS_EX_TRANSPARENT` | 代码通用，待实机验证 |
| Linux | xcap（X11 可用；Wayland 受限） | 同上（X11 下正常，Wayland 合成器可能不支持置顶穿透） | 代码通用，待实机验证 |
| Android | MediaProjection 投屏授权 | SYSTEM_ALERT_WINDOW 悬浮窗 | 已实现，模拟器验证通过（真机待测） |

## Windows

代码层面**不需要改**：`xcap`、Tauri 的透明/置顶/点击穿透、全局热键都是跨平台的。
几个平台特有的点：

- **防自拍**：`set_content_protected(true)` 在 Windows 上映射到
  `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`，需要 Windows 10 2004+。
  更老的系统上覆盖层会被自己抓进去——`em-core` 里那个「分数 >0.97 告警」就是
  给这种情况兜底的信号，触发时应改用「抓屏前先隐藏覆盖层」的策略。
- **多显示器 / DPI**：抓的是游戏窗口，坐标换算走
  `窗口逻辑坐标 × (物理宽/逻辑宽)`，与 macOS 同一套代码，混合 DPI 也成立。
- **窗口识别**：`GAME_HINTS` 目前含 `dwrg`（进程名 `dwrg.exe`）和 `第五人格`，
  Windows 原生客户端两者都能命中，不必改。
- **SmartScreen**：未签名安装包首次运行会拦，用户需点「更多信息 → 仍要运行」。
  要消除需买代码签名证书（EV 证书才能立即免提示）。
- **反作弊观感**：Windows 端反作弊比 macOS 严格，透明置顶窗**理论上可被检测**。
  因此并排窗口模式始终保留为默认，覆盖模式是用户自选项。

## Android

Android 上没有「窗口」概念可用，所以两件事都得换实现，但 `em-core` 一行不用改。

### 已完成

APK 可构建并在模拟器（android-34 / arm64-v8a）实测通过：

```
[em] 数据包目录：/data/user/0/net.yeah.enceka.embermap/bundle
[em] 自检：参考库 39 个楼层条目
[em] 自检：全库匹配耗时 2.47s，结果 右中门1-2 1f 得分 0.970 —— 正确
```

即掩码运算、FFT 相关、rayon 并行在 ARM Android 上均可用，识别结果与桌面一致。
产物 34MB，含 arm64-v8a 与 x86_64 两份 `libembermap_lib.so`。

随后接入 MediaProjection 取帧与 SYSTEM_ALERT_WINDOW 悬浮窗，在模拟器上
完成端到端验证：投屏授权 → 取帧 → 识别 → 悬浮窗叠加。识别结果与桌面一致
（同一张截图桌面 0.837 / Android 0.838），锁定后持续跟踪每帧约 3.5 秒
（模拟器数字，真机应更快）。悬浮窗经 `dumpsys window` 确认属性正确：

```
ty=APPLICATION_OVERLAY fmt=TRANSLUCENT alpha=0.8
fl=NOT_FOCUSABLE NOT_TOUCHABLE LAYOUT_NO_LIMITS SECURE
```

落地时踩到的四个点：

- **资源读取**：Tauri 的 `resource_dir()` 在 Android 上返回 `asset://` URI，
  `std::fs` 读不了。解法是 `MainActivity.kt` 启动时把 `assets/bundle` 解压到
  `dataDir/bundle`（以 `bundle.json` 大小作版本标记避免重复拷贝），
  Rust 侧从 `app_data_dir()/bundle` 读取。
- **构建工具**：用 `cargo tauri android init` 而非 `npx`，
  否则生成的 Gradle 任务会硬编码调用 `npm`，而 `src-tauri/` 下没有 package.json。
- **前台服务的启动顺序**：Android 14+ 要求先 `startForeground()` 再
  `getMediaProjection()`，否则抛 SecurityException。插件不能在主线程
  `sleep` 等待服务启动——`onStartCommand` 同在主线程，等待会把服务自己饿死。
  正解是把投屏创建整个放进服务内部顺序执行，插件用 Handler 非阻塞轮询就绪。
- **FLAG_SECURE 与自我遮挡**：悬浮窗加 `FLAG_SECURE` 后确实不进投屏画面，
  但它覆盖的区域在抓到的帧里变成黑块，而它盖住的恰是地图——实测表现为识别
  结果在「认出」与「无面板」之间来回震荡。解法是取帧瞬间把悬浮窗设为
  `INVISIBLE`、等一帧新画面再抓、抓完恢复（代价是极短暂的闪烁）。

构建命令：

```bash
export ANDROID_HOME=<sdk> NDK_HOME=<sdk>/ndk/<版本>
cargo tauri android build --target aarch64 --target x86_64 --apk
```

### 待实现

#### 拿画面：MediaProjection

Android 不允许后台静默截屏，唯一合规路径是 **MediaProjection**——
用户主动点授权（系统弹窗「EmberMap 将开始截取您屏幕上显示的内容」），
之后 App 才能拿到屏幕帧。

```
MediaProjectionManager.createScreenCaptureIntent()   // 拉起系统授权弹窗
  → ImageReader（RGBA_8888，降到 ~1080p 省内存）
  → 前台服务（Android 14 起必须声明 mediaProjection 类型前台服务）
  → 每帧 Image → ByteBuffer → 传给 Rust
```

要点：
- 授权是**一次性**的，退出前台服务即失效，重新开局要重新授权——
  这反而是好事，用户始终知道自己在被截屏。
- Android 14+ 必须 `foregroundServiceType="mediaProjection"` 并显示常驻通知。
- 抓的是**整屏**（Android 无法只抓某个 App 的窗口），所以我们的悬浮窗会被
  抓进去 → 必须在**取帧的瞬间把悬浮窗设为不可见**，或给悬浮窗所在的
  `SurfaceView` 加 `setSecure(true)`（FLAG_SECURE 的层不会进入投屏画面）。
  推荐后者：零闪烁，且与桌面端 `content_protected` 语义一致。

#### 显示：SYSTEM_ALERT_WINDOW 悬浮窗

```
Settings.ACTION_MANAGE_OVERLAY_PERMISSION   // 引导用户授予「显示在其他应用上层」
  → WindowManager.addView(overlayView, LayoutParams(
        TYPE_APPLICATION_OVERLAY,
        FLAG_NOT_FOCUSABLE | FLAG_NOT_TOUCHABLE,   // 点击穿透，不挡游戏操作
        PixelFormat.TRANSLUCENT))
```

`FLAG_NOT_TOUCHABLE` 让触摸完全穿透到游戏，这与桌面端
`set_ignore_cursor_events(true)` 是一回事。悬浮窗内容用 Canvas 画
（和现在前端 canvas 的逻辑一一对应：手绘图按亮度转 alpha + 门位圆圈）。

控制入口用一个小的可拖动圆钮（另一个 `TOUCHABLE` 的悬浮窗），
避免为了切换开关而退出游戏。

#### em-core 怎么接进去

两条路，推荐第一条：

1. **Tauri 2 Android**（`cargo tauri android init`）：现有前端 canvas 代码
   几乎能直接复用，Kotlin 侧写一个 Tauri 插件负责 MediaProjection 与悬浮窗，
   Rust 侧 `analyze_with` 原样调用。好处是三端一套代码。
   代价是 Tauri 的 Android 支持比桌面端年轻，悬浮窗要自己写插件。
2. **原生 Kotlin App + Rust 静态库**（cargo-ndk 编译 `aarch64-linux-android`，
   JNI 或 uniffi 桥接）：控制力最强、包最小，但 UI 要重写一遍。

无论哪条，`app/bundle/`（39 张掩码 + 手绘图 + 元数据，约 12 MB）作为
`assets/` 打进 APK，首次启动解压到 `filesDir` 即可被 `load_library` 读取。

#### 性能预算

桌面端一次全库扫描约 0.8-1.5 秒（M 系列多核），模拟器实测 2.47 秒，
真机预计相当或略快。因此：

- 分析分辨率降到长边 ~1200（`Options.target_long_edge`，本来就是参数）；
- 轮询间隔放宽到 1.5-2 秒，锁定后可拉到 3 秒（跟踪只需修正位置与缩放）；
- rayon 线程数限制为大核数量，避免和游戏抢 CPU 导致掉帧。

模拟器实测：首次锁定约 5 秒，跟踪每帧约 3.5 秒。真机预计更快，待验证。

一个反直觉的实测结论：**取帧分辨率不能为了省时间而调低**。把 Kotlin 侧的
取帧上限从 2000 降到 1200 时，多次重采样叠加 JPEG 压缩会把识别分数从 0.84
压到 0.73，直接掉出置信门槛；而「高保真取帧 + 低分辨率分析」
（取帧 2000、`target_long_edge` 1000）既保住 0.84 又省时间。

#### 合规提醒

Android 端的悬浮窗叠在游戏上，观感上最接近「外挂」。本方案不读内存、
不注入、不模拟点击，只处理用户主动授权的投屏画面；但 Android 端的
第三方工具检测更激进，**默认应关闭悬浮窗，只提供「切到 App 内看图」模式**，
悬浮窗作为用户明确开启的选项，并在开启时提示风险。
