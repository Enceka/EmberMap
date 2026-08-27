//! 多尺度掩码相关搜索，对齐 proto/emlib.py 的 match_query/_score_at_scale。
//! 相关运算用 2D FFT（rustfft），得分 = 0.6·前景命中率 + 0.4·背景命中率。

use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::{FftDirection, FftPlanner};
use serde::Serialize;

use crate::img::{self, Gray};

pub const MATCH_LONG_EDGE: usize = 160;
pub const REFINE_TOP: usize = 5;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Transform {
    pub scale: f64,
    pub tx: f64,
    pub ty: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EntryScore {
    pub entry: usize,
    pub score: f32,
    pub transform: Transform,
}

fn geomspace(a: f64, b: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| a * (b / a).powf(i as f64 / (n - 1) as f64)).collect()
}

fn next_fast(mut n: usize) -> usize {
    loop {
        let mut m = n;
        for f in [2, 3, 5] {
            while m % f == 0 {
                m /= f;
            }
        }
        if m == 1 {
            return n;
        }
        n += 1;
    }
}

fn fft2(buf: &mut [Complex<f32>], w: usize, h: usize, planner: &mut FftPlanner<f32>, dir: FftDirection) {
    let row_fft = planner.plan_fft(w, dir);
    for row in buf.chunks_exact_mut(w) {
        row_fft.process(row);
    }
    let mut t = vec![Complex::new(0f32, 0f32); w * h];
    for y in 0..h {
        for x in 0..w {
            t[x * h + y] = buf[y * w + x];
        }
    }
    let col_fft = planner.plan_fft(h, dir);
    for col in t.chunks_exact_mut(h) {
        col_fft.process(col);
    }
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = t[x * h + y];
        }
    }
}

/// 相关（cv2.TM_CCORR 语义）：out[y][x] = Σ tmpl·img[y+dy][x+dx]，valid 区域。
/// img 与 tmpl 同时给出可复用同一批 FFT：返回 (corr(img,t1), corr(img,t2))。
fn corr2_valid(
    img_f: &[f32], iw: usize, ih: usize,
    t1: &Gray, t2: &Gray,
    planner: &mut FftPlanner<f32>,
) -> (Vec<f32>, Vec<f32>, usize, usize) {
    let (tw, th) = (t1.w, t1.h);
    let ow = iw - tw + 1;
    let oh = ih - th + 1;
    let fw = next_fast(iw);
    let fh = next_fast(ih);
    let mut fi = vec![Complex::new(0f32, 0f32); fw * fh];
    for y in 0..ih {
        for x in 0..iw {
            fi[y * fw + x] = Complex::new(img_f[y * iw + x], 0.0);
        }
    }
    fft2(&mut fi, fw, fh, planner, FftDirection::Forward);

    let corr_one = |t: &Gray, fi: &[Complex<f32>], planner: &mut FftPlanner<f32>| -> Vec<f32> {
        let mut ft = vec![Complex::new(0f32, 0f32); fw * fh];
        for y in 0..th {
            for x in 0..tw {
                ft[y * fw + x] = Complex::new(t.data[y * tw + x] as f32, 0.0);
            }
        }
        fft2(&mut ft, fw, fh, planner, FftDirection::Forward);
        for (a, b) in ft.iter_mut().zip(fi.iter()) {
            *a = b * a.conj();
        }
        fft2(&mut ft, fw, fh, planner, FftDirection::Inverse);
        let norm = 1.0 / (fw * fh) as f32;
        let mut out = vec![0f32; ow * oh];
        for y in 0..oh {
            for x in 0..ow {
                out[y * ow + x] = ft[y * fw + x].re * norm;
            }
        }
        out
    };
    let c1 = corr_one(t1, &fi, planner);
    let c2 = corr_one(t2, &fi, planner);
    (c1, c2, ow, oh)
}

/// 单尺度平移搜索。返回 (score, dx, dy)，坐标为查询左上角在参考小图中的偏移。
fn score_at_scale(ref_small: &Gray, q_small: &Gray, planner: &mut FftPlanner<f32>) -> Option<(f32, i64, i64)> {
    let (qw, qh) = (q_small.w, q_small.h);
    let (px, py) = (qw / 2, qh / 2);
    let (pw, ph) = (ref_small.w + 2 * px, ref_small.h + 2 * py);
    if pw < qw || ph < qh {
        return None;
    }
    let n_fg = q_small.count_nonzero();
    if n_fg < 30 {
        return None;
    }
    // 已探索区 ≈ 前景膨胀（核7 → r3）；其中非前景像素应落在参考背景上
    let explored = img::dilate(q_small, 3.0);
    let mut qbg = Gray::new(qw, qh);
    for i in 0..qw * qh {
        qbg.data[i] = (explored.data[i] != 0 && q_small.data[i] == 0) as u8;
    }
    let n_bg = qbg.count_nonzero().max(1);

    // 参考四周留出查询一半余量
    let mut ref_pad = vec![0f32; pw * ph];
    for y in 0..ref_small.h {
        for x in 0..ref_small.w {
            ref_pad[(y + py) * pw + (x + px)] = ref_small.data[y * ref_small.w + x] as f32;
        }
    }
    let (c_fg, c_bg, ow, oh) = corr2_valid(&ref_pad, pw, ph, q_small, &qbg, planner);
    // bg 命中 = n_bg − Σ qbg·ref（padding 区 ref=0，即背景）
    let mut best = f32::NEG_INFINITY;
    let mut loc = (0usize, 0usize);
    for y in 0..oh {
        for x in 0..ow {
            let s = 0.6 * c_fg[y * ow + x] / n_fg as f32
                + 0.4 * (n_bg as f32 - c_bg[y * ow + x]) / n_bg as f32;
            if s > best {
                best = s;
                loc = (x, y);
            }
        }
    }
    Some((best, loc.0 as i64 - px as i64, loc.1 as i64 - py as i64))
}

fn resize_long(mask: &Gray, long_edge: usize) -> (Gray, f64) {
    let s = long_edge as f64 / mask.w.max(mask.h) as f64;
    let tw = ((mask.w as f64 * s).round() as usize).max(1);
    let th = ((mask.h as f64 * s).round() as usize).max(1);
    (img::resize_area_binary(mask, tw, th), s)
}

/// 单参考楼层的多尺度搜索。transform：查询原图 px → 参考原图 px。
pub fn match_entry(q_mask: &Gray, ref_mask: &Gray, scales: &[f64], long_edge: usize) -> Option<(f32, Transform)> {
    let (ref_small, ref_s) = resize_long(ref_mask, long_edge);
    let mut planner = FftPlanner::new();
    let mut best: Option<(f32, f64, i64, i64)> = None;
    for &sc in scales {
        let eff = ref_s * sc;
        let tw = ((q_mask.w as f64 * eff).round() as usize).max(1);
        let th = ((q_mask.h as f64 * eff).round() as usize).max(1);
        if tw > ref_small.w * 2 || th > ref_small.h * 2 {
            continue;
        }
        let q_small = img::resize_area_binary(q_mask, tw, th);
        if let Some((s, dx, dy)) = score_at_scale(&ref_small, &q_small, &mut planner) {
            if best.map_or(true, |b| s > b.0) {
                best = Some((s, sc, dx, dy));
            }
        }
    }
    let (score, sc, dx, dy) = best?;
    Some((score, Transform { scale: sc, tx: dx as f64 / ref_s, ty: dy as f64 / ref_s }))
}

/// 匹配参数。acquire（首次识别）求准，track（锁定后）求快。
#[derive(Clone, Copy, Debug)]
pub struct MatchOpts {
    pub coarse_long_edge: usize,
    pub refine_long_edge: usize,
    pub refine_top: usize,
    /// 已知尺度先验（查询降采样坐标系下）：只在其附近搜索，省一个数量级
    pub prior_scale: Option<f64>,
}

impl Default for MatchOpts {
    fn default() -> Self {
        MatchOpts {
            coarse_long_edge: MATCH_LONG_EDGE,
            refine_long_edge: MATCH_LONG_EDGE * 2,
            refine_top: REFINE_TOP,
            prior_scale: None,
        }
    }
}

/// 全库匹配：粗筛（21 档尺度）→ top-N 在更高分辨率细尺度精修重排。
pub fn match_query(q_mask: &Gray, masks: &[&Gray]) -> Vec<EntryScore> {
    match_query_opts(q_mask, masks, &MatchOpts::default())
}

pub fn match_query_opts(q_mask: &Gray, masks: &[&Gray], opt: &MatchOpts) -> Vec<EntryScore> {
    let coarse = match opt.prior_scale {
        // 有先验：地图缩放在一局内固定，只需覆盖面板裁剪抖动带来的微小变化
        Some(s) => geomspace(s * 0.88, s * 1.14, 9),
        None => geomspace(0.5, 2.0, 21),
    };
    let mut results: Vec<EntryScore> = masks
        .par_iter()
        .enumerate()
        .filter_map(|(i, m)| {
            match_entry(q_mask, m, &coarse, opt.coarse_long_edge)
                .map(|(score, transform)| EntryScore { entry: i, score, transform })
        })
        .collect();
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
    let top: Vec<usize> = results.iter().take(opt.refine_top).map(|r| r.entry).collect();
    let refined: Vec<(usize, Option<(f32, Transform)>)> = top
        .par_iter()
        .map(|&e| {
            let base = results.iter().find(|r| r.entry == e).unwrap().transform.scale;
            let fine: Vec<f64> = geomspace(0.94, 1.07, 9).iter().map(|f| base * f).collect();
            (e, match_entry(q_mask, masks[e], &fine, opt.refine_long_edge))
        })
        .collect();
    for (e, r) in refined {
        if let Some((score, tf)) = r {
            let slot = results.iter_mut().find(|x| x.entry == e).unwrap();
            slot.score = score;
            slot.transform = tf;
        }
    }
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
    results
}
