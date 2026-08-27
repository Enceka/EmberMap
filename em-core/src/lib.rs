//! EmberMap 匹配核心。运行时链路：
//! 截图 RGB → 结构掩码 → 自动定位地图面板 → 裁剪 → 全库多尺度匹配。
//! 语义对齐 proto/emlib.py + proto/live.py（Python 原型为准绳）。

pub mod bundle;
pub mod img;
pub mod mask;
pub mod matcher;

use serde::Serialize;

pub use bundle::{load_library, Library};
pub use matcher::Transform;

/// 实测校准的置信门槛：真地图命中 ≥0.81，3D 场景误检 ≤0.75。
pub const CONFIDENCE_GATE: f32 = 0.78;

#[derive(Serialize, Clone)]
pub struct Candidate {
    pub variant: String,
    pub name: String,
    pub floor: String,
    pub score: f32,
    pub transform: Transform,
}

#[derive(Serialize)]
pub enum Analysis {
    /// 没找到像地图面板的区域（还没打开地图）
    NoPanel { reason: String },
    /// 找到了面板并完成匹配；confident = 首名过置信门槛
    Matched {
        panel: [usize; 4],
        confident: bool,
        candidates: Vec<Candidate>,
    },
}

/// 分析一帧截图（RGB8 交错）。等价 analyze_opts(.., None)。
pub fn analyze(rgb: &[u8], w: usize, h: usize, lib: &Library) -> Analysis {
    analyze_opts(rgb, w, h, lib, None)
}

/// only：只匹配该变体的楼层（已锁定变体时 13× 提速）；调用方在
/// 得分掉出门槛时应重新全库扫描。
/// 高分屏截图先盒式降采样到长边 ~1500（接近参考库像素密度），
/// 返回的 panel 与 transform 均换算回全分辨率坐标。
pub fn analyze_opts(rgb: &[u8], w: usize, h: usize, lib: &Library, only: Option<&str>) -> Analysis {
    let f = (w.max(h) as f64 / 1500.0).round().max(1.0) as usize;
    let (ds, dw, dh) = img::downscale_rgb(rgb, w, h, f);
    let f2 = f * f;
    let m = mask::structure_mask(&ds, dw, dh, (400 / f2).max(100));
    let Some(panel_ds) = mask::find_map_region(&m, 37.0 / f as f32, 30 / f, (8000 / f2).max(1500))
    else {
        return Analysis::NoPanel { reason: "未检测到地图面板".into() };
    };
    let [x, y, pw, ph] = panel_ds;
    // 面板不会贴屏幕边（UI 有边距）；3D 场景误检几乎都贴边或铺满全屏
    if pw < 180 / f || ph < 180 / f || x <= 2 || y <= 2
        || x + pw >= dw - 2 || y + ph >= dh - 2 || pw * ph > dw * dh * 7 / 10
    {
        return Analysis::NoPanel {
            reason: format!("检测区域不像地图面板（{}×{}@{},{}）", pw * f, ph * f, x * f, y * f),
        };
    }
    let q = m.crop(x, y, pw, ph);
    if q.count_nonzero() < (5000 / f2).max(1000) {
        return Analysis::NoPanel { reason: "地图区域太小".into() };
    }
    let sel: Vec<usize> = lib
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| only.map_or(true, |v| e.variant == v))
        .map(|(i, _)| i)
        .collect();
    let masks: Vec<&img::Gray> = sel.iter().map(|&i| &lib.entries[i].mask).collect();
    let scores = matcher::match_query(&q, &masks);
    let candidates: Vec<Candidate> = scores
        .iter()
        .map(|s| {
            let e = &lib.entries[sel[s.entry]];
            Candidate {
                variant: e.variant.clone(),
                name: e.name.clone(),
                floor: e.floor.clone(),
                score: s.score,
                // q(降采样面板 px)→game 换算成 q(全分辨率面板 px)→game
                transform: Transform {
                    scale: s.transform.scale / f as f64,
                    tx: s.transform.tx,
                    ty: s.transform.ty,
                },
            }
        })
        .collect();
    let confident = candidates.first().map_or(false, |c| c.score >= CONFIDENCE_GATE);
    let panel = [x * f, y * f, (pw * f).min(w - x * f), (ph * f).min(h - y * f)];
    Analysis::Matched { panel, confident, candidates }
}
