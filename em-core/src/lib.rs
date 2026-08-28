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

/// 首选候选达到此分即直接采信，不再探测其余候选
const PROBE_TRUST: f32 = 0.75;

/// 一个候选区域「有多像库里的地图」：低分辨率 + 抽样库的粗匹配。
/// 真地图对某张参考总能拿到 0.8+，3D 场景对任何参考都只有 0.6 上下。
fn probe_score(rgb: &[u8], w: usize, h: usize, f: usize, c: [usize; 4], lib: &Library) -> f32 {
    let masks: Vec<&img::Gray> = lib.entries.iter().step_by(2).map(|e| &e.mask).collect();
    let opts = matcher::MatchOpts {
        coarse_long_edge: 140,
        refine_long_edge: 140,
        refine_top: 0,
        prior_scale: None,
    };
    let [cx, cy, cw0, ch0] = c;
    let (fx, fy) = (cx * f, cy * f);
    let fw = (cw0 * f).min(w - fx);
    let fh = (ch0 * f).min(h - fy);
    let g = ((fw.max(fh) as f64 / 500.0).round() as usize).max(1);
    let (pc, cw, ch) = img::crop_downscale_rgb(rgb, w, h, fx, fy, fw, fh, g);
    let (q, _) = mask::structure_mask_parts(&pc, cw, ch, (400 / (g * g)).max(100));
    if q.count_nonzero() < 500 {
        return 0.0;
    }
    matcher::match_query_opts(&q, &masks, &opts)
        .first()
        .map(|r| r.score)
        .unwrap_or(0.0)
}

/// 从多个候选区域里挑「最像地图」的那个
fn probe_best_candidate(
    rgb: &[u8],
    w: usize,
    h: usize,
    f: usize,
    cands: &[[usize; 4]],
    lib: &Library,
) -> Option<[usize; 4]> {
    cands
        .iter()
        .map(|&c| (c, probe_score(rgb, w, h, f, c, lib)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c)
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
    let (m, room) = mask::structure_mask_parts(&ds, dw, dh, (400 / f2).max(100));
    let cands = mask::find_map_candidates(&m, &room, 37.0 / f as f32, 30 / f, (8000 / f2).max(1500), 4);
    if cands.is_empty() {
        return Analysis::NoPanel { reason: "未检测到地图面板".into() };
    }
    // 面板不会贴屏幕边（UI 有边距）；3D 场景误检几乎都贴边或铺满全屏。
    // 下限 120：开局只探索出生点附近时地图确实很小（真机实测 177×162）。
    let ok: Vec<[usize; 4]> = cands
        .iter()
        .copied()
        .filter(|&[x, y, pw, ph]| {
            pw >= 120 / f && ph >= 120 / f && x > 2 && y > 2
                && x + pw < dw - 2 && y + ph < dh - 2 && pw * ph <= dw * dh * 7 / 10
        })
        .collect();
    let Some(&first) = ok.first() else {
        let [x, y, pw, ph] = cands[0];
        return Analysis::NoPanel {
            reason: format!("检测区域不像地图面板（{}×{}@{},{}）", pw * f, ph * f, x * f, y * f),
        };
    };
    // 首选是「有房间的最大团」，绝大多数情况就是对的。
    // 只有当它自己都不像地图时（真机上地图缩到最小、半透明界面透出的 3D 场景
    // 形成更大的混合团），才逐个探测其余候选——避免探测把本来正确的选择带偏。
    let [x, y, pw, ph] = if ok.len() == 1 {
        first
    } else if probe_score(rgb, w, h, f, first, lib) >= PROBE_TRUST {
        first
    } else {
        probe_best_candidate(rgb, w, h, f, &ok, lib).unwrap_or(first)
    };
    // 第二段：按全分辨率重新提取面板区域的掩码。
    // 只用整帧降采样后的掩码会让「地图在屏幕上很小」时判别力崩掉——
    // 真机把游戏地图缩到最小时，降采样后面板只剩几十像素，13 个变体分数并列。
    // 面板本身再按需降到长边 ~900（超过这个分辨率对匹配无增益，只增耗时）。
    let (fx, fy) = (x * f, y * f);
    let fw = (pw * f).min(w - fx);
    let fh = (ph * f).min(h - fy);
    let g = ((fw.max(fh) as f64 / 900.0).round() as usize).max(1);
    let (pc, cw, ch) = img::crop_downscale_rgb(rgb, w, h, fx, fy, fw, fh, g);
    let g2 = g * g;
    let (q, _) = mask::structure_mask_parts(&pc, cw, ch, (400 / g2).max(100));
    if q.count_nonzero() < (5000 / g2).max(1000) {
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
    // 先验是全分辨率尺度，换算到面板裁剪坐标系（该坐标系比全分辨率小 g 倍）
    let mopts = matcher::MatchOpts {
        prior_scale: opt.prior_scale_full.map(|s| s * g as f64),
        ..opt.match_opts
    };
    let scores = matcher::match_query_opts(&q, &masks, &mopts);
    let candidates: Vec<Candidate> = scores
        .iter()
        .map(|s| {
            let e = &lib.entries[sel[s.entry]];
            // 变换的查询坐标系是「面板裁剪 / g」，换算成全分辨率面板局部坐标
            let tf = Transform {
                scale: s.transform.scale / g as f64,
                tx: s.transform.tx,
                ty: s.transform.ty,
            };
            Candidate {
                variant: e.variant.clone(),
                name: e.name.clone(),
                floor: e.floor.clone(),
                score: s.score,
                transform: tf,
                scale_full: tf.scale,
            }
        })
        .collect();
    let confident = candidates.first().is_some_and(|c| c.score >= CONFIDENCE_GATE);
    let panel = [fx, fy, fw, fh];
    Analysis::Matched { panel, confident, candidates }
}
