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

/// 叠加几何：整层显示区域，以及手绘图与门位到该区域局部坐标的映射。
///
/// 全部处于「帧像素」坐标系——即传给 analyze_with 的那张图的像素。
/// 若上层对帧做过缩放（Android 取帧按长边封顶），view / tf / doors
/// 必须乘同一个系数一起换算，只换算其中之一就会「窗口摆对了、图画歪了」。
#[derive(Serialize, Clone)]
pub struct Overlay {
    /// 显示区域 [x, y, w, h]，已裁到帧内
    pub view: [usize; 4],
    /// 手绘图 px → 显示区域局部 px
    pub tf: Transform,
    /// 门位，显示区域局部 px
    pub doors: Vec<bundle::Door>,
}

/// 由匹配结果推出叠加几何。
/// q_to_ref 是 Candidate::transform（面板局部 px → 参考 px：ref = q·s + t）。
///
/// 显示区域取「整层参考图投影回画面」而非仅已探索面板——工具的价值就在于
/// 显示还没探索的部分；越界部分裁到画面内，叠加层照常按局部坐标绘制。
pub fn overlay_geometry(
    panel: [usize; 4],
    q_to_ref: Transform,
    entry: &bundle::Entry,
    frame_w: usize,
    frame_h: usize,
) -> Overlay {
    let [px0, py0, _, _] = panel;
    let b = q_to_ref;
    let proj = |rx: f64, ry: f64| {
        ((rx - b.tx) / b.scale + px0 as f64, (ry - b.ty) / b.scale + py0 as f64)
    };
    let (vx0, vy0) = proj(0.0, 0.0);
    let (vx1, vy1) = proj(entry.mask.w as f64, entry.mask.h as f64);
    let x = vx0.floor().max(0.0) as usize;
    let y = vy0.floor().max(0.0) as usize;
    let w = (vx1.ceil().clamp(0.0, frame_w as f64) as usize).saturating_sub(x).max(1);
    let h = (vy1.ceil().clamp(0.0, frame_h as f64) as usize).saturating_sub(y).max(1);
    // 面板局部 → 显示区域局部的平移量（显示区域被裁到画面内，故未必等于 0）
    let (sx, sy) = (px0 as f64 - x as f64, py0 as f64 - y as f64);
    // draw→面板：ref = d·a.scale + a.t 且 ref = q·b.scale + b.t
    //         ⇒ q = d·(a.scale/b.scale) + (a.t − b.t)/b.scale
    let a = entry.tf_draw_to_game;
    Overlay {
        view: [x, y, w, h],
        tf: Transform {
            scale: a.scale / b.scale,
            tx: (a.tx - b.tx) / b.scale + sx,
            ty: (a.ty - b.ty) / b.scale + sy,
        },
        doors: entry
            .doors
            .iter()
            .map(|d| bundle::Door {
                label: d.label.clone(),
                x: (d.x - b.tx) / b.scale + sx,
                y: (d.y - b.ty) / b.scale + sy,
            })
            .collect(),
    }
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

/// 低于此分就直接报「没有地图」，而不是报出一个 0.6 的候选名——
/// 在游戏主界面上，「当前最像 XX 0.62」看着就像认错了。
///
/// 必须小于 CONFIDENCE_GATE：这样它只可能改变提示文案，
/// 不可能压掉一次本来能成立的锁定。真地图实测 ≥0.84，界面/场景误检 ≤0.75。
pub const NOT_A_MAP: f32 = 0.76;

/// 匹配范围限定。用户手动锁定后只跟这些参考比：
/// 既快十几倍，也让主界面之类的画面不可能再被认成别的地图。
#[derive(Default, Clone, Copy, Debug)]
pub struct Pin<'a> {
    pub variant: Option<&'a str>,
    pub floor: Option<&'a str>,
}

impl Pin<'_> {
    pub fn is_set(&self) -> bool {
        self.variant.is_some() || self.floor.is_some()
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
    let (pc, cw, ch) = img::crop_downscale_rgb(rgb, w, fx, fy, fw, fh, g);
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
    analyze_with(rgb, w, h, lib, Pin::default(), &Options::acquire())
}

/// pin：把匹配范围收窄到指定变体/楼层（用户手动锁定时用）。
/// 截图先整数倍盒式降采样到 opt.target_long_edge 附近；
/// 返回的 panel 与 transform 均换算回全分辨率坐标。
pub fn analyze_with(
    rgb: &[u8],
    w: usize,
    h: usize,
    lib: &Library,
    pin: Pin,
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
    let trust_first = ok.len() == 1 || probe_score(rgb, w, h, f, first, lib) >= PROBE_TRUST;
    let [x, y, pw, ph] = if trust_first {
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
    let (pc, cw, ch) = img::crop_downscale_rgb(rgb, w, fx, fy, fw, fh, g);
    let g2 = g * g;
    // 这里不再复核房间占比。看似合理，实测会误杀：游戏地图缩到最小时
    // 整张图只剩走廊色，棕色房间一个像素都不剩（实测 75446 px 的面板里只有 9 px），
    // 而这恰恰是最需要本工具的场景。房间占比只用在第一段挑候选团时。
    let (q, _) = mask::structure_mask_parts(&pc, cw, ch, (400 / g2).max(100));
    if q.count_nonzero() < (5000 / g2).max(1000) {
        return Analysis::NoPanel { reason: "地图区域太小".into() };
    }
    let sel: Vec<usize> = lib
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            pin.variant.is_none_or(|v| e.variant == v) && pin.floor.is_none_or(|f| e.floor == f)
        })
        .map(|(i, _)| i)
        .collect();
    if sel.is_empty() {
        return Analysis::NoPanel { reason: "锁定的地图/楼层不在参考库里".into() };
    }
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
    // 分数低到这个地步，「最像的是某某地图」这句话本身就是误导——直接说没有地图。
    // 手动锁定时不设这道闸：范围是用户自己指定的，分数低就低，如实报出来。
    if let Some(c) = candidates.first().filter(|c| c.score < NOT_A_MAP) {
        return Analysis::NoPanel {
            reason: if pin.is_set() {
                // 锁定时候选就这几张，说「不像地图」会误导——真正的问题是选错了范围
                format!("画面与锁定的范围对不上（{} {} 仅 {:.2}）", c.name, c.floor, c.score)
            } else {
                format!("检测到的区域不像地图（最像 {} {:.2}）", c.name, c.score)
            },
        };
    }
    let confident = candidates.first().is_some_and(|c| c.score >= CONFIDENCE_GATE);
    let panel = [fx, fy, fw, fh];
    Analysis::Matched { panel, confident, candidates }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mask_w: usize, mask_h: usize) -> bundle::Entry {
        bundle::Entry {
            variant: "t".into(),
            name: "测试".into(),
            floor: "1f".into(),
            mask: img::Gray::new(mask_w, mask_h),
            draw_path: std::path::PathBuf::new(),
            // 手绘 → 参考：ref = d·2 + (10,20)
            tf_draw_to_game: bundle::TfJson { scale: 2.0, tx: 10.0, ty: 20.0 },
            doors: vec![bundle::Door { label: "门".into(), x: 110.0, y: 60.0 }],
        }
    }

    /// 叠加几何必须与匹配变换自洽：同一个点无论走「手绘→显示区域」还是
    /// 「手绘→参考→画面」，落点必须一致。真机叠加错位就是从这里跑偏的。
    #[test]
    fn 叠加几何与匹配变换自洽() {
        let e = entry(200, 100);
        // 面板局部 → 参考：ref = q·0.5 + (30,40)
        let b = Transform { scale: 0.5, tx: 30.0, ty: 40.0 };
        let ov = overlay_geometry([500, 300, 50, 40], b, &e, 1000, 800);
        // 整层投影：参考 (0,0) 与 (200,100) 的画面落点
        assert_eq!(ov.view, [440, 220, 400, 200]);
        assert!((ov.tf.scale - 4.0).abs() < 1e-9);

        // 手绘原点：经显示区域 → 画面
        let via_view = (ov.tf.tx + ov.view[0] as f64, ov.tf.ty + ov.view[1] as f64);
        // 手绘原点：经参考 → 画面
        let a = e.tf_draw_to_game;
        let via_ref = ((a.tx - b.tx) / b.scale + 500.0, (a.ty - b.ty) / b.scale + 300.0);
        assert!((via_view.0 - via_ref.0).abs() < 1e-9);
        assert!((via_view.1 - via_ref.1).abs() < 1e-9);

        // 门位同理：参考 (110,60) → 画面 (660,340) → 显示区域局部 (220,120)
        assert!((ov.doors[0].x - 220.0).abs() < 1e-9);
        assert!((ov.doors[0].y - 120.0).abs() < 1e-9);
    }

    /// 整层超出画面时显示区域被裁，但 tf 必须跟着补偿，叠加内容不能因此位移。
    #[test]
    fn 显示区域裁到画面内后变换仍对齐() {
        let e = entry(200, 100);
        let b = Transform { scale: 0.5, tx: 30.0, ty: 40.0 };
        let full = overlay_geometry([500, 300, 50, 40], b, &e, 1000, 800);
        // 画面缩到 600×360：右下被裁，左上顶点不变
        let cut = overlay_geometry([500, 300, 50, 40], b, &e, 600, 360);
        assert_eq!(cut.view[0], full.view[0]);
        assert_eq!(cut.view[1], full.view[1]);
        assert_eq!([cut.view[2], cut.view[3]], [600 - 440, 360 - 220]);
        // 原点未变 ⇒ 局部坐标下的变换与门位应当逐字相同
        assert!((cut.tf.tx - full.tf.tx).abs() < 1e-9);
        assert!((cut.tf.ty - full.tf.ty).abs() < 1e-9);
        assert!((cut.doors[0].x - full.doors[0].x).abs() < 1e-9);
    }
}
