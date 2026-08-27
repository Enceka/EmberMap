//! 结构掩码提取与地图面板定位，对齐 proto/emlib.py 的 game 风格路径。

use crate::img::{self, Gray, HsvRange};

/// 游戏内地图渲染的 HSV 区间（OpenCV H∈[0,180]），与 emlib.GAME_* 一致。
pub const GAME_RANGES: [HsvRange; 2] = [
    ((95, 25, 75), (130, 115, 190)),  // 蓝灰走廊
    ((5, 25, 80), (32, 115, 185)),    // 棕色房间
];

/// 提取可行走结构掩码（0/1）：中值滤波 → 颜色分割 → 开1 → 闭4 → 去小块。
pub fn structure_mask(rgb: &[u8], w: usize, h: usize) -> Gray {
    let blurred = img::median5_rgb(rgb, w, h);
    let m = img::in_ranges(&blurred, w, h, &GAME_RANGES);
    let m = img::open(&m, 1.0);
    let mut m = img::close(&m, 4.0);
    let (labels, comps) = img::connected_components(&m);
    let keep: Vec<bool> = comps.iter().map(|c| c.area >= 400).collect();
    for (px, &l) in m.data.iter_mut().zip(labels.iter()) {
        if l != 0 && !keep[(l - 1) as usize] {
            *px = 0;
        }
    }
    m
}

/// 自动定位地图面板：大半径闭运算并块后取掩码像素最多的一团，bbox 外扩 margin。
/// 对齐 emlib.find_map_region(close=75 → r=37, margin=30, min_area=8000)。
pub fn find_map_region(mask: &Gray) -> Option<[usize; 4]> {
    let merged = img::close(mask, 37.0);
    let (labels, comps) = img::connected_components(&merged);
    if comps.is_empty() {
        return None;
    }
    let mut counts = vec![0usize; comps.len()];
    for (i, &l) in labels.iter().enumerate() {
        if l != 0 && mask.data[i] != 0 {
            counts[(l - 1) as usize] += 1;
        }
    }
    let (best, &best_area) = counts.iter().enumerate().max_by_key(|(_, &c)| c)?;
    if best_area < 8000 {
        return None;
    }
    // bbox 收缩到该团内真实掩码像素
    let (w, h) = (mask.w, mask.h);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if mask.data[i] != 0 && labels[i] == best as u32 + 1 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    const M: usize = 30;
    let rx = x0.saturating_sub(M);
    let ry = y0.saturating_sub(M);
    Some([rx, ry, (x1 + M + 1).min(w) - rx, (y1 + M + 1).min(h) - ry])
}
