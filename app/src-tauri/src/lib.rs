// EmberMap Tauri 壳：抓屏 → em-core 匹配 → 前端 canvas 合成叠加。
//
// 桌面与 Android 共用此库：识别核心与命令是公共的，
// 抓屏（xcap）、覆盖窗、全局热键是桌面独有，用 cfg(desktop) 隔离；
// Android 的投屏取帧与悬浮窗需 Kotlin 插件，尚未接入。

use std::path::PathBuf;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use tauri::{Emitter, Manager, State};

mod tracker;

struct AppState {
    lib: Mutex<Option<em_core::Library>>,
    tracker: Mutex<tracker::Tracker>,
    /// 上一帧的尺寸：变化意味着屏幕旋转或窗口改尺寸，
    /// 此时地图大小随之改变，尺度先验必须作废
    last_frame: Mutex<(usize, usize)>,
    /// 数据包目录：发行版取应用资源目录，开发期取源码树 app/bundle
    bundle_dir: Mutex<PathBuf>,
}

#[derive(Serialize, serde::Deserialize)]
struct DoorOut {
    label: String,
    x: f64,
    y: f64,
}

#[derive(Serialize)]
struct CandidateOut {
    name: String,
    floor: String,
    score: f32,
}

#[derive(Serialize)]
#[serde(tag = "status")]
enum Payload {
    #[serde(rename = "no_panel")]
    NoPanel {
        reason: String,
        /// 实际分析的帧尺寸与缩略图：真机排障全靠它，
        /// 用户截一张图就能看出抓到的到底是什么画面
        frame_w: usize,
        frame_h: usize,
        frame_png: String,
    },
    #[serde(rename = "ok")]
    Ok {
        confident: bool,
        /// acquiring（多帧投票中）| locked（刚锁定）| tracking
        phase: String,
        /// 已累计证据 / 锁定所需证据，供 UI 显示识别进度
        evidence: f32,
        evidence_need: f32,
        /// 本帧与次佳变体的分差：越大越可信（姊妹变体常只差 0.03）
        advantage: f32,
        name: String,
        floor: String,
        score: f32,
        candidates: Vec<CandidateOut>,
        /// 面板裁剪截图 PNG（base64）
        shot_png: String,
        /// 匹配变体该层的手绘图 PNG（base64，桌面前端 canvas 用）
        draw_png: String,
        /// 同一张手绘图在磁盘上的路径（Android 悬浮窗直接读，免 base64 过桥）
        draw_path: String,
        /// 手绘图 → 显示区域坐标的相似变换
        tf: em_core::Transform,
        doors: Vec<DoorOut>,
        /// 整层显示区域（屏幕物理坐标，x/y 可为负——游戏窗口可能部分在屏幕外），
        /// 覆盖窗按它定位定尺寸
        view: [i32; 4],
    },
}

/// 解析数据包目录：Android 取 MainActivity 解压出的数据目录，
/// 桌面优先应用资源目录（发行版），最后回退源码树（开发期）。
fn resolve_bundle_dir(app: &tauri::AppHandle) -> PathBuf {
    // Android 的 resource_dir() 是 asset:// URI，std::fs 读不了；
    // MainActivity.kt 启动时已把 assets/bundle 解压到 dataDir/bundle
    #[cfg(target_os = "android")]
    if let Ok(d) = app.path().app_data_dir() {
        let p = d.join("bundle");
        if p.join("bundle.json").exists() {
            return p;
        }
    }
    if let Ok(p) = app.path().resolve("bundle", tauri::path::BaseDirectory::Resource) {
        if p.join("bundle.json").exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bundle")
}

fn ensure_lib(state: &AppState) -> Result<(), String> {
    let mut g = state.lib.lock().unwrap();
    if g.is_none() {
        let dir = state.bundle_dir.lock().unwrap().clone();
        *g = Some(em_core::load_library(&dir)?);
    }
    Ok(())
}

/// 整帧缩略图（base64 PNG），仅用于排障显示
fn thumbnail(rgb: &[u8], w: usize, h: usize, long_edge: usize) -> Result<String, String> {
    let f = (w.max(h) / long_edge).max(1);
    let (tw, th) = (w / f, h / f);
    let mut small = vec![0u8; tw * th * 3];
    for y in 0..th {
        for x in 0..tw {
            let src = ((y * f) * w + x * f) * 3;
            let dst = (y * tw + x) * 3;
            small[dst..dst + 3].copy_from_slice(&rgb[src..src + 3]);
        }
    }
    png_b64(&small, tw as u32, th as u32)
}

fn png_b64(rgb: &[u8], w: u32, h: u32) -> Result<String, String> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(B64.encode(buf))
}

use image::ImageEncoder;

fn run_analysis(rgb: Vec<u8>, w: usize, h: usize, state: &AppState) -> Result<Payload, String> {
    ensure_lib(state)?;
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    // 始终全库扫描：尺度先验只收窄尺度搜索，不过滤候选，误判才能自我纠正。
    // 帧尺寸变了（旋转/改窗口）：地图随之缩放，沿用旧尺度先验会把搜索窗口带偏，
    // 实测表现为旋转后分数掉到 0.69、分差 0.001，直到超时解锁才恢复。
    {
        let mut lf = state.last_frame.lock().unwrap();
        if *lf != (w, h) {
            if *lf != (0, 0) {
                eprintln!("[em] 帧尺寸 {:?} → {:?}，重置跟踪状态", *lf, (w, h));
                state.tracker.lock().unwrap().reset();
            }
            *lf = (w, h);
        }
    }
    let prior = state.tracker.lock().unwrap().prior_scale_full;
    let mut opts = match prior {
        Some(s) => em_core::Options::track(s),
        None => em_core::Options::acquire(),
    };
    // 移动端算力有限，用更轻的档位：分析分辨率减半、精修分辨率与候选数下调。
    // 注意取帧分辨率不能跟着降——实测取帧缩到 1200 会让重采样与 JPEG 损失
    // 把分数压到门槛下（0.73），而高保真取帧 + 低分辨率分析仍有 0.84。
    // 主要耗时在匹配而非掩码提取，故这里同时收窄精修。
    #[cfg(mobile)]
    {
        opts.target_long_edge = 1000.0;
        opts.match_opts.refine_long_edge = 380;
        opts.match_opts.refine_top = if prior.is_some() { 3 } else { 6 };
    }
    // 自检：投屏管线没跟上屏幕旋转时，抓到的帧会几乎全黑（内容被压到一角）。
    // 单说「未检测到地图面板」会把用户引向错误方向，故单独报出来。
    let dark = rgb.chunks_exact(3).filter(|p| p[0] < 24 && p[1] < 24 && p[2] < 24).count();
    if dark * 100 / (w * h).max(1) >= 92 {
        return Ok(Payload::NoPanel {
            reason: "抓到的画面几乎全黑，投屏可能未跟上屏幕旋转；请关掉再重新授权投屏".into(),
            frame_w: w,
            frame_h: h,
            frame_png: thumbnail(&rgb, w, h, 720)?,
        });
    }
    let t0 = std::time::Instant::now();
    let analysis = em_core::analyze_with(&rgb, w, h, lib, None, &opts);
    let dt = t0.elapsed();
    match analysis {
        em_core::Analysis::NoPanel { reason } => {
            let forgot = state.tracker.lock().unwrap().on_no_panel();
            eprintln!("[em] {dt:?} 无面板{}：{reason}", if forgot { "(证据已清空)" } else { "(暂停)" });
            Ok(Payload::NoPanel {
                reason,
                frame_w: w,
                frame_h: h,
                frame_png: thumbnail(&rgb, w, h, 720)?,
            })
        }
        em_core::Analysis::Matched { panel, candidates, .. } => {
            let flat: Vec<(String, f32)> =
                candidates.iter().map(|c| (c.variant.clone(), c.score)).collect();
            let mut tk = state.tracker.lock().unwrap();
            let dec = tk.update(&flat, em_core::CONFIDENCE_GATE);
            // 采纳变体的最佳楼层；未采纳时用本帧第一名（仅供参考显示）
            let best = match &dec.variant {
                Some(v) => candidates.iter().find(|c| &c.variant == v).unwrap(),
                None => &candidates[0],
            };
            if dec.variant.is_some() {
                tk.prior_scale_full = Some(best.scale_full);
            }
            let (phase, evidence, advantage) = (dec.phase.to_string(), dec.evidence, dec.advantage);
            let confident = dec.variant.is_some();
            drop(tk);
            // 自检：覆盖层本应对抓屏不可见（content_protected）。若它漏进画面，
            // 手绘层会与参考掩码近乎完美匹配，分数异常拉高到 0.97+。
            if best.score > 0.97 {
                eprintln!("[em] 警告：分数 {:.3} 异常高，疑似覆盖层被抓屏捕获", best.score);
            }
            eprintln!(
                "[em] {dt:?} {phase} 证据={evidence:.3} 分差={advantage:.3} 面板={panel:?} \
                 先验={prior:?} | {}",
                candidates
                    .iter()
                    .take(3)
                    .map(|c| format!("{} {} {:.3}", c.name, c.floor, c.score))
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
            let entry = lib
                .entries
                .iter()
                .find(|e| e.variant == best.variant && e.floor == best.floor)
                .unwrap();
            // 显示区域 / 变换 / 门位由 em-core 统一推导，与离线校验工具同源
            let ov = em_core::overlay_geometry(panel, best.transform, entry, w, h);
            let [x, y, pw, ph] = ov.view;
            let mut crop = vec![0u8; pw * ph * 3];
            for row in 0..ph {
                let src = ((y + row) * w + x) * 3;
                crop[row * pw * 3..(row + 1) * pw * 3]
                    .copy_from_slice(&rgb[src..src + pw * 3]);
            }
            let tf = ov.tf;
            let doors: Vec<DoorOut> = ov
                .doors
                .iter()
                .map(|d| DoorOut { label: d.label.clone(), x: d.x, y: d.y })
                .collect();
            let draw_png = B64.encode(
                std::fs::read(&entry.draw_path).map_err(|e| e.to_string())?,
            );
            // EM_DUMP=1：把匹配器看到的面板裁剪图落盘，便于排查误判
            if std::env::var("EM_DUMP").is_ok() {
                let _ = image::save_buffer(
                    "/tmp/em_panel.png", &crop, pw as u32, ph as u32,
                    image::ExtendedColorType::Rgb8,
                );
            }
            Ok(Payload::Ok {
                confident,
                phase,
                evidence,
                evidence_need: tracker::LOCK_ADVANTAGE,
                advantage,
                name: entry.name.clone(),
                floor: entry.floor.clone(),
                score: best.score,
                candidates: candidates
                    .iter()
                    .take(3)
                    .map(|c| CandidateOut { name: c.name.clone(), floor: c.floor.clone(), score: c.score })
                    .collect(),
                shot_png: png_b64(&crop, pw as u32, ph as u32)?,
                draw_png,
                draw_path: entry.draw_path.to_string_lossy().into_owned(),
                tf,
                doors,
                // 交给前端定位覆盖窗的是「整层显示区域」，不是已探索面板
                view: [x as i32, y as i32, pw as i32, ph as i32],
            })
        }
    }
}

/// 游戏窗口标识（Wine 下进程名为 dwrg.exe）
#[cfg(desktop)]
const GAME_HINTS: [&str; 4] = ["第五人格", "identityv", "dwrg", "wine"];

#[cfg(desktop)]
fn rgba_to_rgb(rgba: Vec<u8>, n: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; n * 3];
    for i in 0..n {
        rgb[i * 3] = rgba[i * 4];
        rgb[i * 3 + 1] = rgba[i * 4 + 1];
        rgb[i * 3 + 2] = rgba[i * 4 + 2];
    }
    rgb
}

/// 优先抓游戏窗口本身：与前台无关（我们的窗口挡住也没事），
/// 且覆盖层不可能被自己抓进去。找不到游戏窗口时退回抓主屏。
/// 返回 (rgb, w, h, 该图左上角在屏幕上的物理坐标)。
#[cfg(desktop)]
fn capture_target() -> Result<(Vec<u8>, usize, usize, i32, i32), String> {
    if let Ok(windows) = xcap::Window::all() {
        let game = windows.into_iter().find(|w| {
            let name = format!(
                "{} {}",
                w.app_name().unwrap_or_default(),
                w.title().unwrap_or_default()
            )
            .to_lowercase();
            let big = w.width().unwrap_or(0) > 400 && w.height().unwrap_or(0) > 300;
            let visible = !w.is_minimized().unwrap_or(false);
            big && visible && GAME_HINTS.iter().any(|h| name.contains(h))
        });
        if let Some(win) = game {
            if let Ok(img) = win.capture_image() {
                eprintln!(
                    "[em] 抓取游戏窗口「{} / {}」窗口逻辑={}x{}@{},{} 抓到物理={}x{}",
                    win.app_name().unwrap_or_default(),
                    win.title().unwrap_or_default(),
                    win.width().unwrap_or(0), win.height().unwrap_or(0),
                    win.x().unwrap_or(0), win.y().unwrap_or(0),
                    img.width(), img.height()
                );
                let (w, h) = (img.width() as usize, img.height() as usize);
                // 窗口坐标是逻辑像素，抓到的图是物理像素；按比例换算原点
                let sx = w as f64 / win.width().unwrap_or(w as u32).max(1) as f64;
                let sy = h as f64 / win.height().unwrap_or(h as u32).max(1) as f64;
                let ox = (win.x().unwrap_or(0) as f64 * sx).round() as i32;
                let oy = (win.y().unwrap_or(0) as f64 * sy).round() as i32;
                return Ok((rgba_to_rgb(img.into_raw(), w * h), w, h, ox, oy));
            }
        }
    }
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let mon = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or("没有可用显示器")?;
    let img = mon.capture_image().map_err(|e| {
        format!("抓屏失败：{e}。请在 系统设置→隐私与安全性→屏幕录制 给本应用授权")
    })?;
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok((rgba_to_rgb(img.into_raw(), w * h), w, h, 0, 0))
}

// 注意：重活必须放 spawn_blocking——同步命令跑在主线程上会把 UI 冻住
#[cfg(desktop)]
#[tauri::command]
async fn analyze_screen(app: tauri::AppHandle) -> Result<Payload, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (rgb, w, h, ox, oy) = capture_target()?;
        let mut payload = run_analysis(rgb, w, h, &app.state::<AppState>())?;
        // 换算到屏幕物理坐标供覆盖窗定位；不可钳到 0，游戏窗口可能在屏幕外
        if let Payload::Ok { view, .. } = &mut payload {
            view[0] += ox;
            view[1] += oy;
            eprintln!("[em] 整层显示区域(屏幕物理)={view:?} 窗口原点=({ox},{oy})");
        }
        Ok(payload)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn analyze_file(path: String, app: tauri::AppHandle) -> Result<Payload, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let im = image::open(&path).map_err(|e| e.to_string())?.to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        run_analysis(im.into_raw(), w, h, &app.state::<AppState>())
    })

    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// Android 取帧：经 Kotlin 插件走 MediaProjection
// ---------------------------------------------------------------------------

#[cfg(target_os = "android")]
mod android_capture {
    use serde::Deserialize;
    use tauri::plugin::PluginHandle;

    #[derive(Deserialize)]
    pub struct Granted {
        pub granted: bool,
        pub reason: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct Frame {
        pub path: String,
        /// Kotlin 侧缩放系数（截图 px = 屏幕物理 px × scale）
        pub scale: f64,
    }

    #[derive(Deserialize)]
    pub struct Capturing {
        pub capturing: bool,
    }

    pub fn request(h: &PluginHandle<tauri::Wry>) -> Result<Granted, String> {
        h.run_mobile_plugin("requestCapture", ()).map_err(|e| e.to_string())
    }
    pub fn stop(h: &PluginHandle<tauri::Wry>) -> Result<(), String> {
        h.run_mobile_plugin::<serde_json::Value>("stopCapture", ())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn is_capturing(h: &PluginHandle<tauri::Wry>) -> Result<Capturing, String> {
        h.run_mobile_plugin("isCapturing", ()).map_err(|e| e.to_string())
    }
    pub fn grab(h: &PluginHandle<tauri::Wry>) -> Result<Frame, String> {
        h.run_mobile_plugin("grabFrame", ()).map_err(|e| e.to_string())
    }
}

/// Android：拉起系统投屏授权（用户点「立即开始」后才可取帧）
#[cfg(target_os = "android")]
#[tauri::command]
async fn request_capture(app: tauri::AppHandle) -> Result<bool, String> {
    let h = app.state::<AndroidPlugin>().0.clone();
    let r = android_capture::request(&h)?;
    if !r.granted {
        return Err(r.reason.unwrap_or_else(|| "投屏授权未通过".into()));
    }
    Ok(true)
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn stop_capture(app: tauri::AppHandle) -> Result<(), String> {
    android_capture::stop(&app.state::<AndroidPlugin>().0)
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn capture_active(app: tauri::AppHandle) -> Result<bool, String> {
    Ok(android_capture::is_capturing(&app.state::<AndroidPlugin>().0)?.capturing)
}

/// Android 版 analyze_screen：取帧 → 识别。坐标换算回屏幕物理像素，
/// 供悬浮窗定位（Kotlin 侧按 maxLongEdge 缩过，故需除以 scale）。
#[cfg(target_os = "android")]
#[tauri::command]
async fn analyze_screen(app: tauri::AppHandle) -> Result<Payload, String> {
    let frame = {
        let h = app.state::<AndroidPlugin>().0.clone();
        android_capture::grab(&h)?
    };
    tauri::async_runtime::spawn_blocking(move || {
        let im = image::open(&frame.path).map_err(|e| e.to_string())?.to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        let mut payload = run_analysis(im.into_raw(), w, h, &app.state::<AppState>())?;
        // run_analysis 的输出全在「截图像素」坐标系里，而悬浮窗画在屏幕物理像素上。
        // 只换算 view 是不够的：tf 与门位也在同一坐标系，漏掉它们等于
        // 窗口摆对了、里面的图却按截图尺度画——实测长边 2412 的屏幕被缩到 2000，
        // 叠加层就整体小 17% 并朝窗口左上角偏，表现为「覆盖和缩放都不对」。
        if let Payload::Ok { view, tf, doors, .. } = &mut payload {
            let inv = 1.0 / frame.scale.max(1e-6);
            for v in view.iter_mut() {
                *v = (*v as f64 * inv).round() as i32;
            }
            tf.scale *= inv;
            tf.tx *= inv;
            tf.ty *= inv;
            for d in doors.iter_mut() {
                d.x *= inv;
                d.y *= inv;
            }
            eprintln!(
                "[em] 帧→屏幕 ×{inv:.4} 显示区域={view:?} 手绘尺度={:.4} 平移=({:.1},{:.1})",
                tf.scale, tf.tx, tf.ty
            );
        }
        Ok(payload)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "android")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayArgs {
    draw_path: String,
    scale: f64,
    tx: f64,
    ty: f64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    alpha: f64,
    doors: Vec<DoorOut>,
}

/// Android 悬浮窗：内容由 Kotlin 的 OverlayView 绘制（中文标签用系统字体最省事）
#[cfg(target_os = "android")]
#[tauri::command]
async fn overlay_update(
    app: tauri::AppHandle,
    draw_path: String,
    tf: serde_json::Value,
    doors: Vec<DoorOut>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    alpha: f64,
) -> Result<(), String> {
    let args = OverlayArgs {
        draw_path,
        scale: tf["scale"].as_f64().unwrap_or(1.0),
        tx: tf["tx"].as_f64().unwrap_or(0.0),
        ty: tf["ty"].as_f64().unwrap_or(0.0),
        x: x as i32,
        y: y as i32,
        w: w as i32,
        h: h as i32,
        alpha,
        doors,
    };
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin::<serde_json::Value>("showOverlay", args)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn overlay_hide(app: tauri::AppHandle) -> Result<(), String> {
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin::<serde_json::Value>("hideOverlay", ())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn request_overlay_permission(app: tauri::AppHandle) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    struct Granted { granted: bool }
    let r: Granted = app
        .state::<AndroidPlugin>()
        .0
        .run_mobile_plugin("requestOverlayPermission", ())
        .map_err(|e| e.to_string())?;
    Ok(r.granted)
}

/// 持有 Kotlin 插件句柄
#[cfg(target_os = "android")]
struct AndroidPlugin(tauri::plugin::PluginHandle<tauri::Wry>);

/// 前端据此决定显示哪些功能：Android 尚无悬浮窗
#[tauri::command]
fn capabilities() -> serde_json::Value {
    serde_json::json!({
        // Android 经 MediaProjection 取帧，但需用户先授权
        "screen_capture": true,
        "needs_capture_permission": cfg!(target_os = "android"),
        "overlay": true,
        "hotkeys": cfg!(desktop),
    })
}

/// 清除锁定与投票，下一帧从零开始高质量识别（换局/怀疑锁错时用）
#[tauri::command]
fn reset_lock(state: State<AppState>) {
    state.tracker.lock().unwrap().reset();
}

/// 覆盖窗口：透明/无边框/置顶/点击穿透/防捕获，按面板物理像素坐标摆放。
/// content_protected 使其对抓屏不可见，持续监测不会被自己的叠加污染。
#[cfg(desktop)]
#[tauri::command]
fn overlay_update(
    app: tauri::AppHandle,
    payload: serde_json::Value,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    use tauri::{Emitter, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
    let win = match app.get_webview_window("overlay") {
        Some(w) => w,
        None => {
            let w = WebviewWindowBuilder::new(&app, "overlay", WebviewUrl::App("overlay.html".into()))
                .transparent(true)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .shadow(false)
                .focused(false)
                .visible(false)
                .build()
                .map_err(|e| e.to_string())?;
            w.set_ignore_cursor_events(true).map_err(|e| e.to_string())?;
            w.set_content_protected(true).map_err(|e| e.to_string())?;
            w
        }
    };
    win.set_position(PhysicalPosition::new(x, y)).map_err(|e| e.to_string())?;
    win.set_size(PhysicalSize::new(w, h)).map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    // show 之后再发数据，overlay.html 首次加载时监听器可能尚未就绪，
    // 前端带重试（首帧由 overlay-ready 事件拉取）
    win.emit("overlay-data", payload).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn overlay_hide(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
}


/// 全局热键：即使焦点在游戏里也能用。前端收到 hotkey 事件后执行对应动作。
#[cfg(desktop)]
fn register_hotkeys(app: &tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

    let toggle = Shortcut::new(Some(Modifiers::SHIFT | Modifiers::ALT), Code::KeyM);
    let redo = Shortcut::new(Some(Modifiers::SHIFT | Modifiers::ALT), Code::KeyR);
    let handle = app.clone();
    app.plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(move |_app, sc, ev| {
                if ev.state() != ShortcutState::Pressed {
                    return;
                }
                let action = if sc == &toggle {
                    "toggle_overlay"
                } else if sc == &redo {
                    "reset_lock"
                } else {
                    return;
                };
                let _ = handle.emit("hotkey", action);
            })
            .with_shortcuts([toggle, redo])
            .map_err(|e| e.to_string())?
            .build(),
    )
    .map_err(|e| e.to_string())?;
    eprintln!("[em] 全局热键：⇧⌥M 切换覆盖层，⇧⌥R 重新识别");
    Ok(())
}

/// 从参考库自身合成一个「部分探索」查询并匹配，验证核心可用。
/// 与 em-core/tests/synth_regression.rs 同思路，此处用于目标平台实机自检。
#[cfg(mobile)]
fn self_check(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if let Err(e) = ensure_lib(&state) {
        eprintln!("[em] 自检失败：加载数据包出错：{e}");
        return;
    }
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    eprintln!("[em] 自检：参考库 {} 个楼层条目", lib.entries.len());

    let want = &lib.entries[0];
    let m = &want.mask;
    let sub = m.crop(m.w / 8, m.h / 8, m.w * 3 / 4, m.h * 3 / 4);
    let q = em_core::img::resize_area_binary(&sub, (sub.w * 4) / 5, (sub.h * 4) / 5);
    let masks: Vec<&em_core::img::Gray> = lib.entries.iter().map(|e| &e.mask).collect();
    let t0 = std::time::Instant::now();
    let res = em_core::matcher::match_query(&q, &masks);
    let dt = t0.elapsed();
    let got = &lib.entries[res[0].entry];
    let ok = got.variant == want.variant && got.floor == want.floor;
    eprintln!(
        "[em] 自检：全库匹配耗时 {dt:?}，结果 {} {} 得分 {:.3} —— {}",
        got.name,
        got.floor,
        res[0].score,
        if ok { "正确" } else { "错误！" }
    );
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .manage(AppState {
            lib: Mutex::new(None),
            tracker: Mutex::new(tracker::Tracker::default()),
            last_frame: Mutex::new((0, 0)),
            bundle_dir: Mutex::new(PathBuf::new()),
        })
        .setup(|app| {
            let dir = resolve_bundle_dir(app.handle());
            eprintln!("[em] 数据包目录：{}", dir.display());
            *app.state::<AppState>().bundle_dir.lock().unwrap() = dir;
            #[cfg(desktop)]
            if let Err(e) = register_hotkeys(app.handle()) {
                eprintln!("[em] 全局热键注册失败（不影响其他功能）：{e}");
            }
            // 移动端启动自检：加载参考库并跑一次合成匹配，
            // 验证识别核心（FFT/rayon/掩码）在该架构上确实可用
            #[cfg(mobile)]
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || self_check(&handle));
            }
            Ok(())
        });

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        analyze_screen,
        analyze_file,
        reset_lock,
        capabilities,
        overlay_update,
        overlay_hide
    ]);
    // Android：抓屏与悬浮窗需 Kotlin 插件（MediaProjection / SYSTEM_ALERT_WINDOW），
    // 尚未接入，先只暴露与平台无关的命令
    #[cfg(mobile)]
    let builder = builder
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("emcapture")
                .setup(|app, api: tauri::plugin::PluginApi<tauri::Wry, ()>| {
                    #[cfg(target_os = "android")]
                    {
                        use tauri::Manager;
                        let handle = api.register_android_plugin(
                            "net.yeah.enceka.embermap",
                            "CapturePlugin",
                        )?;
                        app.manage(AndroidPlugin(handle));
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = (app, api);
                    Ok(())
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            analyze_screen,
            analyze_file,
            reset_lock,
            capabilities,
            request_capture,
            stop_capture,
            capture_active,
            request_overlay_permission,
            overlay_update,
            overlay_hide
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("EmberMap 启动失败");
}
