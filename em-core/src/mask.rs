//! 结构掩码提取与地图面板定位，对齐 proto/emlib.py 的 game 风格路径。

use crate::img::{self, Gray, HsvRange};

/// 判定为地图所需的最低房间像素占比（百分比）。
/// 实测：真地图 23%，半透明界面透出的 3D 场景 0.0-0.2%。
/// 取 4% 是为了给「开局只探索到一段走廊、房间刚露一角」留余量。
pub const MIN_ROOM_PERCENT: usize = 4;

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
/// 找出所有像地图面板的候选团（按面积降序，最多 max_n 个）。
///
/// 只给候选而不直接定夺：真机上地图缩到最小时，半透明界面下透出的 3D 场景
/// 会形成比地图更大的「混合团」，取最大者必然选错；改由调用方用匹配分数决定。
pub fn find_map_candidates(
    mask: &Gray,
    room: &Gray,
    close_r: f32,
    margin: usize,
    min_area: usize,
    max_n: usize,
) -> Vec<[usize; 4]> {
    let merged = img::close(mask, close_r);
    let (labels, comps) = img::connected_components(&merged);
    if comps.is_empty() {
        return Vec::new();
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
    // 真地图必有棕色房间；纯单色团（3D 场景）先排到后面
    let mut idx: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] >= min_area).collect();
    idx.sort_by_key(|&i| {
        let mixed = rooms[i] * 100 >= counts[i] * MIN_ROOM_PERCENT;
        (!mixed, std::cmp::Reverse(counts[i]))
    });
    idx.truncate(max_n);

    let (w, h) = (mask.w, mask.h);
    idx.into_iter()
        .map(|best| {
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
            [rx, ry, (x1 + margin + 1).min(w) - rx, (y1 + margin + 1).min(h) - ry]
        })
        .collect()
}
