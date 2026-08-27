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
    /// 全分辨率尺度（= transform.scale），作为下一帧 track 的先验
    pub scale_full: f64,
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

/// 分析参数：首次识别（acquire）求准，锁定后（track）求快。
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// 分析分辨率上限（长边），截图按整数倍盒式降采样到此附近
    pub target_long_edge: f64,
    pub match_opts: matcher::MatchOpts,
    /// 尺度先验，**全分辨率** q→game 尺度（与分析分辨率无关，故可跨帧复用）。
    /// analyze_with 内部乘以当前降采样系数得到 match_opts.prior_scale。
    pub prior_scale_full: Option<f64>,
}

impl Options {
    /// 首次识别：高精修分辨率 + 更多精修候选，盲搜全尺度范围，慢而准。
    pub fn acquire() -> Self {
        Options {
            target_long_edge: 2400.0,
            match_opts: matcher::MatchOpts {
                coarse_long_edge: 200,
                refine_long_edge: 520,
                refine_top: 10,
                prior_scale: None,
            },
            prior_scale_full: None,
        }
    }

    /// 锁定后跟踪：仍扫全库（防误锁自我确认），但尺度只在先验附近搜，
    /// 精修候选减到 5 个 → 约 0.8-0.9s（acquire 为 1.3-1.5s）。
    /// 分析分辨率与 acquire 保持一致，避免两相位间分数漂移导致反复解锁。
    pub fn track(prior_scale_full: f64) -> Self {
        Options {
            match_opts: matcher::MatchOpts { refine_top: 5, ..Self::acquire().match_opts },
            prior_scale_full: Some(prior_scale_full),
            ..Self::acquire()
        }
    }
}

/// 分析一帧截图（RGB8 交错），默认 acquire 质量、全库。
pub fn analyze(rgb: &[u8], w: usize, h: usize, lib: &Library) -> Analysis {
    analyze_with(rgb, w, h, lib, None, &Options::acquire())
}

/// only：只匹配该变体的楼层（13× 提速，须配合周期性全库审计）。
/// 截图先整数倍盒式降采样到 opt.target_long_edge 附近；
/// 返回的 panel 与 transform 均换算回全分辨率坐标。
pub fn analyze_with(
    rgb: &[u8],
    w: usize,
    h: usize,
    lib: &Library,
    only: Option<&str>,
    opt: &Options,
) -> Analysis {
    let f = (w.max(h) as f64 / opt.target_long_edge).round().max(1.0) as usize;
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
        .filter(|(_, e)| only.is_none_or(|v| e.variant == v))
        .map(|(i, _)| i)
        .collect();
    let masks: Vec<&img::Gray> = sel.iter().map(|&i| &lib.entries[i].mask).collect();
    // 先验换算到本帧降采样坐标系（f 随分析分辨率变化，故不能跨帧直接复用 ds 尺度）
    let mopts = matcher::MatchOpts {
        prior_scale: opt.prior_scale_full.map(|s| s * f as f64),
        ..opt.match_opts
    };
    let scores = matcher::match_query_opts(&q, &masks, &mopts);
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
                scale_full: s.transform.scale / f as f64,
            }
        })
        .collect();
    let confident = candidates.first().is_some_and(|c| c.score >= CONFIDENCE_GATE);
    let panel = [x * f, y * f, (pw * f).min(w - x * f), (ph * f).min(h - y * f)];
    Analysis::Matched { panel, confident, candidates }
}
