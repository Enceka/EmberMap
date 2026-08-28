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
    /// 数据包目录：发行版取应用资源目录，开发期取源码树 app/bundle
    bundle_dir: Mutex<PathBuf>,
}

#[derive(Serialize)]
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
    NoPanel { reason: String },
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
        /// 匹配变体该层的手绘图 PNG（base64）
        draw_png: String,
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
    let prior = state.tracker.lock().unwrap().prior_scale_full;
    let opts = match prior {
        Some(s) => em_core::Options::track(s),
        None => em_core::Options::acquire(),
    };
    let t0 = std::time::Instant::now();
    let analysis = em_core::analyze_with(&rgb, w, h, lib, None, &opts);
    let dt = t0.elapsed();
    match analysis {
        em_core::Analysis::NoPanel { reason } => {
            let forgot = state.tracker.lock().unwrap().on_no_panel();
            eprintln!("[em] {dt:?} 无面板{}：{reason}", if forgot { "(证据已清空)" } else { "(暂停)" });
            Ok(Payload::NoPanel { reason })
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
            // 显示区域 = 整层参考图投影回画面，而非仅已探索区域——
            // 工具的价值就在于显示还没探索的部分。裁到画面边界内。
            let [px0, py0, pw0, ph0] = panel;
            let b0 = best.transform; // q(面板局部 px) → 参考 px：ref = q·s + t
            let proj = |rx: f64, ry: f64| {
                ((rx - b0.tx) / b0.scale + px0 as f64, (ry - b0.ty) / b0.scale + py0 as f64)
            };
            let (vx0, vy0) = proj(0.0, 0.0);
            let (vx1, vy1) = proj(entry.mask.w as f64, entry.mask.h as f64);
            let x = vx0.floor().max(0.0) as usize;
            let y = vy0.floor().max(0.0) as usize;
            let pw = (vx1.ceil().min(w as f64) as usize).saturating_sub(x).max(1);
            let ph = (vy1.ceil().min(h as f64) as usize).saturating_sub(y).max(1);
            let mut crop = vec![0u8; pw * ph * 3];
            for row in 0..ph {
                let src = ((y + row) * w + x) * 3;
                crop[row * pw * 3..(row + 1) * pw * 3]
                    .copy_from_slice(&rgb[src..src + pw * 3]);
            }
            // 面板局部坐标 → 显示区域局部坐标的平移量
            let (sx, sy) = (px0 as f64 - x as f64, py0 as f64 - y as f64);
            let _ = (pw0, ph0);
            // draw→面板 变换：q = d·(a/b) + (ta−tb)/b
            let a = entry.tf_draw_to_game;
            let b = best.transform;
            let tf = em_core::Transform {
                scale: a.scale / b.scale,
                tx: (a.tx - b.tx) / b.scale + sx,
                ty: (a.ty - b.ty) / b.scale + sy,
            };
            let doors = entry
                .doors
                .iter()
                .map(|d| DoorOut {
                    label: d.label.clone(),
                    x: (d.x - b.tx) / b.scale + sx,
                    y: (d.y - b.ty) / b.scale + sy,
                })
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

/// 前端据此决定显示哪些功能：Android 尚无抓屏与悬浮窗
#[tauri::command]
fn capabilities() -> serde_json::Value {
    serde_json::json!({
        "screen_capture": cfg!(desktop),
        "overlay": cfg!(desktop),
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
    let builder = builder.invoke_handler(tauri::generate_handler![
        analyze_file,
        reset_lock,
        capabilities
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("EmberMap 启动失败");
}
