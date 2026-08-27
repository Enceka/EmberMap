//! 应用数据包（app/bundle/）加载：bundle.json + 参考掩码 PNG。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::img::Gray;

#[derive(Deserialize)]
struct BundleJson {
    version: u32,
    variants: Vec<VariantJson>,
}

#[derive(Deserialize)]
struct VariantJson {
    id: String,
    name: String,
    floors: Vec<FloorJson>,
}

#[derive(Deserialize)]
struct FloorJson {
    floor: String,
    mask: String,
    draw: String,
    tf_draw_to_game: TfJson,
    doors: Vec<Door>,
}

#[derive(Deserialize, Serialize, Clone, Copy)]
pub struct TfJson {
    pub scale: f64,
    pub tx: f64,
    pub ty: f64,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Door {
    pub label: String,
    pub x: f64,
    pub y: f64,
}

pub struct Entry {
    pub variant: String,
    pub name: String,
    pub floor: String,
    pub mask: Gray,
    pub draw_path: PathBuf,
    pub tf_draw_to_game: TfJson,
    pub doors: Vec<Door>,
}

pub struct Library {
    pub entries: Vec<Entry>,
}

pub fn load_png_mask(path: &Path) -> Result<Gray, String> {
    let im = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?.to_luma8();
    let (w, h) = (im.width() as usize, im.height() as usize);
    Ok(Gray { w, h, data: im.into_raw().iter().map(|&v| (v > 127) as u8).collect() })
}

pub fn load_library(dir: &Path) -> Result<Library, String> {
    let text = std::fs::read_to_string(dir.join("bundle.json"))
        .map_err(|e| format!("bundle.json: {e}"))?;
    let bj: BundleJson = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if bj.version != 1 {
        return Err(format!("不支持的 bundle 版本 {}", bj.version));
    }
    let mut entries = Vec::new();
    for v in bj.variants {
        for f in v.floors {
            entries.push(Entry {
                variant: v.id.clone(),
                name: v.name.clone(),
                floor: f.floor,
                mask: load_png_mask(&dir.join(&f.mask))?,
                draw_path: dir.join(&f.draw),
                tf_draw_to_game: f.tf_draw_to_game,
                doors: f.doors,
            });
        }
    }
    Ok(Library { entries })
}
