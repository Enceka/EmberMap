// EmberMap Tauri 壳：抓屏 → em-core 匹配 → 前端 canvas 合成叠加。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use tauri::State;

struct AppState {
    lib: Mutex<Option<em_core::Library>>,
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
    match em_core::analyze(&rgb, w, h, lib) {
        em_core::Analysis::NoPanel { reason } => Ok(Payload::NoPanel { reason }),
        em_core::Analysis::Matched { panel, confident, candidates } => {
            let best = &candidates[0];
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

#[tauri::command]
fn analyze_screen(state: State<AppState>) -> Result<Payload, String> {
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
    run_analysis(rgb, w, h, &state)
}

#[tauri::command]
fn analyze_file(path: String, state: State<AppState>) -> Result<Payload, String> {
    let im = image::open(&path).map_err(|e| e.to_string())?.to_rgb8();
    let (w, h) = (im.width() as usize, im.height() as usize);
    run_analysis(im.into_raw(), w, h, &state)
}

fn main() {
    tauri::Builder::default()
        .manage(AppState { lib: Mutex::new(None) })
        .invoke_handler(tauri::generate_handler![analyze_screen, analyze_file])
        .run(tauri::generate_context!())
        .expect("EmberMap 启动失败");
}
