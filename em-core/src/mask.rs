//! 结构掩码提取与地图面板定位，对齐 proto/emlib.py 的 game 风格路径。

use crate::img::{self, Gray, HsvRange};

/// 判定为地图所需的最低房间像素占比（百分比）。
/// 实测：真地图 23%，半透明界面透出的 3D 场景 0.0-0.2%。
/// 取 4% 是为了给「开局只探索到一段走廊、房间刚露一角」留余量。
const MIN_ROOM_PERCENT: usize = 4;

/// 游戏内地图渲染的 HSV 区间（OpenCV H∈[0,180]），与 emlib.GAME_* 一致。
pub const GAME_RANGES: [HsvRange; 2] = [
    ((95, 25, 75), (130, 115, 190)),  // 蓝灰走廊
    ((5, 25, 80), (32, 115, 185)),    // 棕色房间
];

/// 提取可行走结构掩码（0/1）：中值滤波 → 颜色分割 → 开1 → 闭4 → 去小块。
/// min_area 为最小连通块面积（全分辨率语义 400，降采样时按面积比例缩）。
pub fn structure_mask(rgb: &[u8], w: usize, h: usize, min_area: usize) -> Gray {
    structure_mask_parts(rgb, w, h, min_area).0
}

/// 同 structure_mask，另外返回「房间」子掩码（棕色地板）。
/// 房间占比是区分真地图与误检的关键：手机上地图界面半透明，
/// 透出的 3D 场景天空落进走廊色域、地面落进房间色域，
/// 会形成比真地图更大的连通块；但那些块几乎是纯单色
/// （实测房间占比 0.0-0.2%），而真地图必然走廊与房间混合（实测 23%）。
pub fn structure_mask_parts(rgb: &[u8], w: usize, h: usize, min_area: usize) -> (Gray, Gray) {
    let blurred = img::median5_rgb(rgb, w, h);
    let corridor = img::in_ranges(&blurred, w, h, &GAME_RANGES[..1]);
    let room = img::in_ranges(&blurred, w, h, &GAME_RANGES[1..]);
    let mut raw = Gray::new(w, h);
    for i in 0..w * h {
        raw.data[i] = (corridor.data[i] != 0 || room.data[i] != 0) as u8;
    }
    let m = img::open(&raw, 1.0);
    let mut m = img::close(&m, 4.0);
    let (labels, comps) = img::connected_components(&m);
    let keep: Vec<bool> = comps.iter().map(|c| c.area >= min_area).collect();
    for (px, &l) in m.data.iter_mut().zip(labels.iter()) {
        if l != 0 && !keep[(l - 1) as usize] {
            *px = 0;
        }
    }
    let mut room_out = Gray::new(w, h);
    for i in 0..w * h {
        room_out.data[i] = (m.data[i] != 0 && room.data[i] != 0) as u8;
    }
    (m, room_out)
}

/// 自动定位地图面板：大半径闭运算并块后取掩码像素最多的一团，bbox 外扩 margin。
/// 对齐 emlib.find_map_region；全分辨率语义 close_r=37, margin=30, min_area=8000。
pub fn find_map_region(
    mask: &Gray,
    room: &Gray,
    close_r: f32,
    margin: usize,
    min_area: usize,
) -> Option<[usize; 4]> {
    let merged = img::close(mask, close_r);
    let (labels, comps) = img::connected_components(&merged);
    if comps.is_empty() {
        return None;
    }
    let mut counts = vec![0usize; comps.len()];
    let mut rooms = vec![0usize; comps.len()];
    for (i, &l) in labels.iter().enumerate() {
        if l != 0 && mask.data[i] != 0 {
            counts[(l - 1) as usize] += 1;
            if room.data[i] != 0 {
                rooms[(l - 1) as usize] += 1;
            }
        }
    }
    // 优先取「走廊与房间混合」的团：真地图必有棕色房间，而半透明界面下
    // 透出的 3D 场景是纯单色。混合团都不达标时再退回取最大团。
    let mixed = |i: usize| counts[i] >= min_area && rooms[i] * 100 >= counts[i] * MIN_ROOM_PERCENT;
    let best = (0..counts.len())
        .filter(|&i| mixed(i))
        .max_by_key(|&i| counts[i])
        .or_else(|| (0..counts.len()).max_by_key(|&i| counts[i]))?;
    let best_area = counts[best];
    if best_area < min_area {
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
    let rx = x0.saturating_sub(margin);
    let ry = y0.saturating_sub(margin);
    Some([rx, ry, (x1 + margin + 1).min(w) - rx, (y1 + margin + 1).min(h) - ry])
}
