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

/// 分析一帧截图（RGB8 交错）。
pub fn analyze(rgb: &[u8], w: usize, h: usize, lib: &Library) -> Analysis {
    let m = mask::structure_mask(rgb, w, h);
    let Some(panel) = mask::find_map_region(&m) else {
        return Analysis::NoPanel { reason: "未检测到地图面板".into() };
    };
    let [x, y, pw, ph] = panel;
    // 面板不会贴屏幕边（UI 有边距）；3D 场景误检几乎都贴边或铺满全屏
    if pw < 180 || ph < 180 || x <= 2 || y <= 2 || x + pw >= w - 2 || y + ph >= h - 2
        || pw * ph > w * h * 7 / 10
    {
        return Analysis::NoPanel { reason: format!("检测区域不像地图面板（{pw}×{ph}@{x},{y}）") };
    }
    let q = m.crop(x, y, pw, ph);
    if q.count_nonzero() < 5000 {
        return Analysis::NoPanel { reason: "地图区域太小".into() };
    }
    let masks: Vec<&img::Gray> = lib.entries.iter().map(|e| &e.mask).collect();
    let scores = matcher::match_query(&q, &masks);
    let candidates: Vec<Candidate> = scores
        .iter()
        .map(|s| {
            let e = &lib.entries[s.entry];
            Candidate {
                variant: e.variant.clone(),
                name: e.name.clone(),
                floor: e.floor.clone(),
                score: s.score,
                transform: s.transform,
            }
        })
        .collect();
    let confident = candidates.first().map_or(false, |c| c.score >= CONFIDENCE_GATE);
    Analysis::Matched { panel, confident, candidates }
}
