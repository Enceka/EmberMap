//! 基础图像类型与像素级操作：中值滤波、HSV、距离变换形态学、连通域、面积重采样。
//! 语义与 proto/emlib.py（OpenCV 实现）逐项对齐；核形状为欧氏圆盘，
//! 与 cv2 椭圆核有 ≤1px 边界差异，对下游降采样相关匹配无影响。

// 图像代码里同一个下标要同时索引多个缓冲区（源/目标/标签），
// 显式 for x in 0..w 比迭代器组合更贴近坐标语义，故豁免该 lint。
#![allow(clippy::needless_range_loop)]

use rayon::prelude::*;

/// 单通道图，data 长度 = w*h，行主序。
#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Gray {
    pub fn new(w: usize, h: usize) -> Self {
        Gray { w, h, data: vec![0; w * h] }
    }
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.w + x]
    }
    pub fn count_nonzero(&self) -> usize {
        self.data.iter().filter(|&&v| v != 0).count()
    }
    pub fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Gray {
        let mut out = Gray::new(w, h);
        for r in 0..h {
            let src = (y + r) * self.w + x;
            out.data[r * w..(r + 1) * w].copy_from_slice(&self.data[src..src + w]);
        }
        out
    }
}

/// RGB8 整数倍盒式降采样（均值），用于把高分屏截图降到参考库像素密度附近。
pub fn downscale_rgb(rgb: &[u8], w: usize, h: usize, f: usize) -> (Vec<u8>, usize, usize) {
    if f <= 1 {
        return (rgb.to_vec(), w, h);
    }
    let (tw, th) = (w / f, h / f);
    let mut out = vec![0u8; tw * th * 3];
    out.par_chunks_mut(tw * 3).enumerate().for_each(|(ty, row)| {
        for tx in 0..tw {
            let mut acc = [0u32; 3];
            for dy in 0..f {
                let base = ((ty * f + dy) * w + tx * f) * 3;
                for dx in 0..f {
                    for c in 0..3 {
                        acc[c] += rgb[base + dx * 3 + c] as u32;
                    }
                }
            }
            let n = (f * f) as u32;
            for c in 0..3 {
                row[tx * 3 + c] = (acc[c] / n) as u8;
            }
        }
    });
    (out, tw, th)
}

/// 裁剪并整数倍盒式降采样（均值）。用于「先定位面板、再按全分辨率提取该区域」：
/// 地图在屏幕上占比小时，若沿用整帧降采样后的掩码，剩下的像素不足以判别。
pub fn crop_downscale_rgb(
    rgb: &[u8], w: usize,
    x: usize, y: usize, cw: usize, ch: usize, f: usize,
) -> (Vec<u8>, usize, usize) {
    let (tw, th) = ((cw / f).max(1), (ch / f).max(1));
    let mut out = vec![0u8; tw * th * 3];
    out.par_chunks_mut(tw * 3).enumerate().for_each(|(ty, row)| {
        for tx in 0..tw {
            let mut acc = [0u32; 3];
            for dy in 0..f {
                let base = ((y + ty * f + dy) * w + x + tx * f) * 3;
                for dx in 0..f {
                    for c in 0..3 {
                        acc[c] += rgb[base + dx * 3 + c] as u32;
                    }
                }
            }
            let n = (f * f) as u32;
            for c in 0..3 {
                row[tx * 3 + c] = (acc[c] / n) as u8;
            }
        }
    });
    (out, tw, th)
}

/// RGB8 交错格式的 5×5 逐通道中值滤波（边界复制），对齐 cv2.medianBlur(img, 5)。
pub fn median5_rgb(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let mut win = [0u8; 25];
        for x in 0..w {
            for c in 0..3 {
                let mut k = 0;
                for dy in -2i64..=2 {
                    let sy = (y as i64 + dy).clamp(0, h as i64 - 1) as usize;
                    for dx in -2i64..=2 {
                        let sx = (x as i64 + dx).clamp(0, w as i64 - 1) as usize;
                        win[k] = rgb[(sy * w + sx) * 3 + c];
                        k += 1;
                    }
                }
                win.sort_unstable();
                row[x * 3 + c] = win[12];
            }
        }
    });
    out
}

/// OpenCV 约定的 RGB→HSV（H∈[0,180)，S,V∈[0,255]）。
#[inline]
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let v = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = v - mn;
    if v == 0 || d == 0 {
        return (0, 0, v as u8); // 全黑或无饱和度：色相无意义，S 记 0
    }
    let s = (d * 255 + v / 2) / v;
    let h = if v == r {
        30 * (g - b) / d
    } else if v == g {
        60 + 30 * (b - r) / d
    } else {
        120 + 30 * (r - g) / d
    };
    let h = if h < 0 { h + 180 } else { h };
    (h as u8, s as u8, v as u8)
}

pub type HsvRange = ((u8, u8, u8), (u8, u8, u8));

/// 双区间（走廊∪房间）颜色分割，输出 0/1 掩码。
pub fn in_ranges(rgb: &[u8], w: usize, h: usize, ranges: &[HsvRange]) -> Gray {
    let mut out = Gray::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let (hh, ss, vv) = rgb_to_hsv(rgb[i], rgb[i + 1], rgb[i + 2]);
            for &(lo, hi) in ranges {
                if hh >= lo.0 && hh <= hi.0 && ss >= lo.1 && ss <= hi.1 && vv >= lo.2 && vv <= hi.2 {
                    row[x] = 1;
                    break;
                }
            }
        }
    });
    out
}

/// 精确欧氏距离变换的平方（Felzenszwalb & Huttenlocher），到值为 0 像素的距离。
/// mask 中非零像素距离为 0…… 注意：这里求"到前景的距离"，前景=非零。
pub fn edt_sq(fg: &Gray) -> Vec<f32> {
    const INF: f32 = 1e18;
    let (w, h) = (fg.w, fg.h);
    let mut d = vec![0f32; w * h];
    for i in 0..w * h {
        d[i] = if fg.data[i] != 0 { 0.0 } else { INF };
    }
    // 列向 1D DT
    let mut cols: Vec<f32> = vec![0.0; w * h];
    cols.par_chunks_mut(h).enumerate().for_each(|(x, out)| {
        let mut f = vec![0f32; h];
        for y in 0..h {
            f[y] = d[y * w + x];
        }
        dt1d(&f, out);
    });
    // 行向 1D DT（cols 是按列转置存储的：cols[x*h+y]）
    let mut res = vec![0f32; w * h];
    res.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let f: Vec<f32> = (0..w).map(|x| cols[x * h + y]).collect();
        dt1d(&f, out);
    });
    res
}

fn dt1d(f: &[f32], out: &mut [f32]) {
    let n = f.len();
    let mut v = vec![0usize; n];
    let mut z = vec![0f32; n + 1];
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        loop {
            let s = ((f[q] + (q * q) as f32) - (f[v[k]] + (v[k] * v[k]) as f32))
                / (2.0 * q as f32 - 2.0 * v[k] as f32);
            if s <= z[k] {
                if k == 0 {
                    break;
                }
                k -= 1;
            } else {
                k += 1;
                v[k] = q;
                z[k] = s;
                z[k + 1] = f32::INFINITY;
                break;
            }
        }
    }
    k = 0;
    for q in 0..n {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let dq = q as f32 - v[k] as f32;
        out[q] = dq * dq + f[v[k]];
    }
}

/// 圆盘半径 r 的膨胀：到前景距离 ≤ r。
pub fn dilate(m: &Gray, r: f32) -> Gray {
    let d = edt_sq(m);
    let r2 = r * r + 1e-3;
    let mut out = Gray::new(m.w, m.h);
    out.data.par_iter_mut().zip(d.par_iter()).for_each(|(o, &v)| {
        *o = (v <= r2) as u8;
    });
    out
}

/// 圆盘半径 r 的腐蚀：到背景距离 > r。
pub fn erode(m: &Gray, r: f32) -> Gray {
    let inv = Gray { w: m.w, h: m.h, data: m.data.iter().map(|&v| (v == 0) as u8).collect() };
    let d = edt_sq(&inv);
    let r2 = r * r + 1e-3;
    let mut out = Gray::new(m.w, m.h);
    out.data.par_iter_mut().zip(d.par_iter()).for_each(|(o, &v)| {
        *o = (v > r2) as u8;
    });
    out
}

pub fn open(m: &Gray, r: f32) -> Gray {
    dilate(&erode(m, r), r)
}

pub fn close(m: &Gray, r: f32) -> Gray {
    erode(&dilate(m, r), r)
}

pub struct Component {
    pub bbox: [usize; 4], // x, y, w, h
    pub area: usize,
}

/// 8 连通连通域标记。返回 (labels, 分量表)，label 0 = 背景，分量下标 = label-1。
pub fn connected_components(m: &Gray) -> (Vec<u32>, Vec<Component>) {
    let (w, h) = (m.w, m.h);
    let mut labels = vec![0u32; w * h];
    let mut comps: Vec<Component> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..w * h {
        if m.data[start] == 0 || labels[start] != 0 {
            continue;
        }
        let id = comps.len() as u32 + 1;
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        let mut area = 0usize;
        labels[start] = id;
        stack.push(start);
        while let Some(p) = stack.pop() {
            let (px, py) = (p % w, p / w);
            area += 1;
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
            let ylo = py.saturating_sub(1);
            let yhi = (py + 1).min(h - 1);
            let xlo = px.saturating_sub(1);
            let xhi = (px + 1).min(w - 1);
            for ny in ylo..=yhi {
                for nx in xlo..=xhi {
                    let q = ny * w + nx;
                    if m.data[q] != 0 && labels[q] == 0 {
                        labels[q] = id;
                        stack.push(q);
                    }
                }
            }
        }
        comps.push(Component { bbox: [x0, y0, x1 - x0 + 1, y1 - y0 + 1], area });
    }
    (labels, comps)
}

/// 面积平均重采样（对齐 cv2 INTER_AREA 缩小），输入 0/1，输出按覆盖率 >0.5 二值化。
pub fn resize_area_binary(m: &Gray, tw: usize, th: usize) -> Gray {
    let mut out = Gray::new(tw, th);
    let sx = m.w as f64 / tw as f64;
    let sy = m.h as f64 / th as f64;
    out.data.par_chunks_mut(tw).enumerate().for_each(|(ty, row)| {
        let y0 = ty as f64 * sy;
        let y1 = (ty as f64 + 1.0) * sy;
        for tx in 0..tw {
            let x0 = tx as f64 * sx;
            let x1 = (tx as f64 + 1.0) * sx;
            let mut acc = 0f64;
            let mut area = 0f64;
            let iy0 = y0.floor() as usize;
            let iy1 = (y1.ceil() as usize).min(m.h);
            let ix0 = x0.floor() as usize;
            let ix1 = (x1.ceil() as usize).min(m.w);
            for yy in iy0..iy1 {
                let wy = (y1.min(yy as f64 + 1.0) - y0.max(yy as f64)).max(0.0);
                for xx in ix0..ix1 {
                    let wx = (x1.min(xx as f64 + 1.0) - x0.max(xx as f64)).max(0.0);
                    area += wx * wy;
                    if m.data[yy * m.w + xx] != 0 {
                        acc += wx * wy;
                    }
                }
            }
            row[tx] = (area > 0.0 && acc / area > 0.5) as u8;
        }
    });
    out
}
