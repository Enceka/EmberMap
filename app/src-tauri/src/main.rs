// EmberMap Tauri 壳：抓屏 → em-core 匹配 → 前端 canvas 合成叠加。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use tauri::State;

mod tracker;

struct AppState {
    lib: Mutex<Option<em_core::Library>>,
    tracker: Mutex<tracker::Tracker>,
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
        /// 手绘图 → 面板坐标的相似变换
        tf: em_core::Transform,
        doors: Vec<DoorOut>,
        panel: [usize; 4],
    },
}

fn bundle_dir() -> PathBuf {
    // 开发期：源码树里的 app/bundle；发行版打包进资源目录（M3 处理）
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bundle")
}

fn ensure_lib(state: &AppState) -> Result<(), String> {
    let mut g = state.lib.lock().unwrap();
    if g.is_none() {
        *g = Some(em_core::load_library(&bundle_dir())?);
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
    // 始终全库扫描：有尺度先验时也只要 0.5-0.9s，换来的是误判能自我纠正。
    let prior = state.tracker.lock().unwrap().prior_scale_ds;
    let opts = match prior {
        Some(s) => em_core::Options::track(s),
        None => em_core::Options::acquire(),
    };
    let analysis = em_core::analyze_with(&rgb, w, h, lib, None, &opts);
    match analysis {
        em_core::Analysis::NoPanel { reason } => {
            state.tracker.lock().unwrap().reset();
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
                tk.prior_scale_ds = Some(best.scale_ds);
            }
            let (phase, evidence, advantage) = (dec.phase.to_string(), dec.evidence, dec.advantage);
            let confident = dec.variant.is_some();
            drop(tk);
            let entry = lib
                .entries
                .iter()
                .find(|e| e.variant == best.variant && e.floor == best.floor)
                .unwrap();
            // 面板裁剪图
            let [x, y, pw, ph] = panel;
            let mut crop = vec![0u8; pw * ph * 3];
            for row in 0..ph {
                let src = ((y + row) * w + x) * 3;
                crop[row * pw * 3..(row + 1) * pw * 3]
                    .copy_from_slice(&rgb[src..src + pw * 3]);
            }
            // draw→面板 变换：q = d·(a/b) + (ta−tb)/b
            let a = entry.tf_draw_to_game;
            let b = best.transform;
            let tf = em_core::Transform {
                scale: a.scale / b.scale,
                tx: (a.tx - b.tx) / b.scale,
                ty: (a.ty - b.ty) / b.scale,
            };
            let doors = entry
                .doors
                .iter()
                .map(|d| DoorOut {
                    label: d.label.clone(),
                    x: (d.x - b.tx) / b.scale,
                    y: (d.y - b.ty) / b.scale,
                })
                .collect();
            let draw_png = B64.encode(
                std::fs::read(&entry.draw_path).map_err(|e| e.to_string())?,
            );
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
                panel,
            })
        }
    }
}

// 注意：重活必须放 spawn_blocking——同步命令跑在主线程上会把 UI 冻住
#[tauri::command]
async fn analyze_screen(app: tauri::AppHandle) -> Result<Payload, String> {
    tauri::async_runtime::spawn_blocking(move || {
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
        let rgba = img.into_raw();
        let mut rgb = vec![0u8; w * h * 3];
        for i in 0..w * h {
            rgb[i * 3] = rgba[i * 4];
            rgb[i * 3 + 1] = rgba[i * 4 + 1];
            rgb[i * 3 + 2] = rgba[i * 4 + 2];
        }
        run_analysis(rgb, w, h, &app.state::<AppState>())
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

/// 清除锁定与投票，下一帧从零开始高质量识别（换局/怀疑锁错时用）
#[tauri::command]
fn reset_lock(state: State<AppState>) {
    state.tracker.lock().unwrap().reset();
}

/// 覆盖窗口：透明/无边框/置顶/点击穿透/防捕获，按面板物理像素坐标摆放。
/// content_protected 使其对抓屏不可见，持续监测不会被自己的叠加污染。
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

#[tauri::command]
fn overlay_hide(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
}

use tauri::Manager;

fn main() {
    tauri::Builder::default()
        .manage(AppState { lib: Mutex::new(None), tracker: Mutex::new(tracker::Tracker::default()) })
        .invoke_handler(tauri::generate_handler![
            analyze_screen,
            analyze_file,
            reset_lock,
            overlay_update,
            overlay_hide
        ])
        .run(tauri::generate_context!())
        .expect("EmberMap 启动失败");
}
