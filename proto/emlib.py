#!/usr/bin/env python3
"""EmberMap 原型核心库：结构掩码提取、楼层切分、参考库读写、匹配。

坐标约定：所有 bbox 均为 (x, y, w, h)，源图像素坐标。
掩码约定：uint8，255=可行走结构（走廊∪房间），0=其他。
"""
import json
import os

import cv2
import numpy as np

# ---------------------------------------------------------------------------
# 结构掩码提取
# ---------------------------------------------------------------------------

# game-em / 游戏内地图渲染的 HSV 区间（OpenCV H∈[0,180]）
GAME_CORRIDOR = ((95, 25, 75), (130, 115, 190))   # 蓝灰走廊
GAME_ROOM = ((5, 25, 80), (32, 115, 185))         # 棕色房间（含地板纹理）

# draw-em 手绘图的 HSV 区间
DRAW_CORRIDOR = ((95, 15, 90), (130, 90, 200))    # 蓝灰走廊
DRAW_ROOM = ((0, 20, 90), (32, 90, 200))          # 棕灰房间
DRAW_GOLD = ((15, 120, 120), (30, 255, 255))      # 金色高亮房间（祭台/缪斯房）


def _in_range(hsv, lo_hi):
    lo, hi = lo_hi
    return cv2.inRange(hsv, np.array(lo, np.uint8), np.array(hi, np.uint8))


def structure_mask(img_bgr, style):
    """提取可行走结构掩码。style: 'game' | 'draw'。"""
    return structure_mask_parts(img_bgr, style)[0]


def structure_mask_parts(img_bgr, style):
    """同 structure_mask，另返回「房间」子掩码（棕色地板）。

    去噪策略：先中值滤波压掉细水印笔画和 JPEG 噪点，再按颜色分割，
    然后开运算去孤立小块、闭运算补上被文字/路线遮挡的洞。

    房间占比用于区分真地图与误检：手机上地图界面半透明，透出的 3D 场景
    天空落进走廊色域、地面落进房间色域，会形成比真地图更大的连通块，
    但那些块几乎是纯单色（实测房间占比 0.0-0.2%），
    而真地图必然走廊与房间混合（实测 23%）。
    """
    img = cv2.medianBlur(img_bgr, 5)
    hsv = cv2.cvtColor(img, cv2.COLOR_BGR2HSV)
    room_raw = None
    if style == "game":
        room_raw = _in_range(hsv, GAME_ROOM)
        mask = _in_range(hsv, GAME_CORRIDOR) | room_raw
    elif style == "draw":
        mask = _in_range(hsv, DRAW_CORRIDOR) | _in_range(hsv, DRAW_ROOM) | _in_range(hsv, DRAW_GOLD)
    else:
        raise ValueError(style)
    k3 = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (3, 3))
    k9 = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (9, 9))
    mask = cv2.morphologyEx(mask, cv2.MORPH_OPEN, k3)
    mask = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, k9)
    # 去掉面积过小的连通块（残余水印/图例碎片）
    n, labels, stats, _ = cv2.connectedComponentsWithStats(mask, 8)
    keep = np.zeros(n, bool)
    for i in range(1, n):
        keep[i] = stats[i, cv2.CC_STAT_AREA] >= 400
    mask = np.where(keep[labels], 255, 0).astype(np.uint8)
    room = np.zeros_like(mask) if room_raw is None else ((mask > 0) & (room_raw > 0)) * np.uint8(255)
    return mask, room


# ---------------------------------------------------------------------------
# 楼层切分：一张拼图里含多个楼层，按空间聚类拆开
# ---------------------------------------------------------------------------

def split_floors(mask, k=3, erode=15, min_core=5000, attach=60, ds=4):
    """把掩码切成 k 个楼层，返回按面积降序的 [{bbox, area, mask}]。

    水印笔画细（<15px）、真实走廊宽（>20px），所以：
      1. 腐蚀出"楼层核心"——细水印桥/文字笔画被打断消失；
      2. 核心连通块按像素间距单链接聚类到 k 簇；若最小簇面积 < min_core
         （不可能是楼层，多为水印残团），整簇剔除后重聚；
      3. 原掩码像素按最近核心归属分配；离一切核心超过 attach 的像素
         （游离水印尾巴）直接丢弃。
    """
    ker = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (erode, erode))
    core = cv2.erode(mask, ker)
    n, labels, stats, _ = cv2.connectedComponentsWithStats(core, 8)
    small = labels[::ds, ::ds]
    comp_ids = [i for i in range(1, n) if stats[i, cv2.CC_STAT_AREA] >= 200]
    if len(comp_ids) < k:
        return []

    # 成对最小像素距离（在 1/ds 分辨率上算，够准）。
    # 单链接聚类合并时距离取行最小值，无需重算。
    m = len(comp_ids)
    dist = np.full((m, m), np.inf)
    for a in range(m):
        blob = (small == comp_ids[a]).astype(np.uint8)
        dt = cv2.distanceTransform(1 - blob, cv2.DIST_L2, 3) * ds
        for b in range(m):
            if b != a:
                pts = small == comp_ids[b]
                if pts.any():
                    dist[a, b] = dist[b, a] = min(dist[a, b], float(dt[pts].min()))

    while True:
        clusters = [{i} for i in range(m)]
        d = dist.copy()
        active = list(range(m))
        while len(active) > k:
            sub = d[np.ix_(active, active)]
            ai, aj = np.unravel_index(np.argmin(sub), sub.shape)
            i, j = active[ai], active[aj]
            clusters[i] |= clusters[j]
            d[i, :] = d[:, i] = np.minimum(d[i, :], d[j, :])
            d[i, i] = np.inf
            active.remove(j)
        out = [{"ids": [comp_ids[x] for x in clusters[i]]} for i in active]
        for c in out:
            c["core_area"] = int(sum(stats[cid, cv2.CC_STAT_AREA] for cid in c["ids"]))
        smallest = min(out, key=lambda c: c["core_area"])
        if smallest["core_area"] >= min_core or len(active) <= 1:
            break
        drop = {x for x in range(m) if comp_ids[x] in smallest["ids"]}
        keep = [x for x in range(m) if x not in drop]
        comp_ids = [comp_ids[x] for x in keep]
        dist = dist[np.ix_(keep, keep)]
        m = len(comp_ids)
        if m < k:
            return []

    # 像素分配：每簇一张距离场，argmin 归属，过远丢弃
    dts = []
    for c in out:
        blob = np.isin(labels, c["ids"]).astype(np.uint8)
        dts.append(cv2.distanceTransform(1 - blob, cv2.DIST_L2, 3))
    dts = np.stack(dts)
    owner = np.argmin(dts, axis=0)
    dmin = np.min(dts, axis=0)
    valid = (mask > 0) & (dmin <= attach)

    floors = []
    for ci in range(len(out)):
        sel = valid & (owner == ci)
        if not sel.any():
            continue
        ys, xs = np.nonzero(sel)
        bbox = [int(xs.min()), int(ys.min()),
                int(xs.max() - xs.min() + 1), int(ys.max() - ys.min() + 1)]
        fm = np.where(crop(sel, bbox), 255, 0).astype(np.uint8)
        floors.append({"bbox": bbox, "area": int(sel.sum()), "mask": fm})
    floors.sort(key=lambda f: -f["area"])
    return floors


def split_floors_by_rects(mask, rect_map, min_frag=400):
    """人工覆盖切分：rect_map = {楼层名: [[x,y,w,h], ...]}，像素按矩形归属。

    用于距离聚类原理性失效的个例（楼层过近/同层分离过远）。
    矩形外的掩码像素丢弃；先去掉过小碎块。
    """
    n, labels, stats, _ = cv2.connectedComponentsWithStats(mask, 8)
    ok = np.zeros(n, bool)
    for i in range(1, n):
        ok[i] = stats[i, cv2.CC_STAT_AREA] >= min_frag
    clean = ok[labels]
    floors, names = [], []
    for name, rects in rect_map.items():
        sel = np.zeros(mask.shape, bool)
        for x, y, w, h in rects:
            sel[y:y + h, x:x + w] = True
        sel &= clean & (mask > 0)
        if not sel.any():
            continue
        ys, xs = np.nonzero(sel)
        bbox = [int(xs.min()), int(ys.min()),
                int(xs.max() - xs.min() + 1), int(ys.max() - ys.min() + 1)]
        floors.append({"bbox": bbox, "area": int(sel.sum()),
                       "mask": np.where(crop(sel, bbox), 255, 0).astype(np.uint8)})
        names.append(name)
    order = sorted(range(len(floors)), key=lambda i: -floors[i]["area"])
    return [floors[i] for i in order], [names[i] for i in order]


def label_floors(floors):
    """按面积/位置规律命名楼层：最小=地下室 b1；其余两个，上方=一楼 1f，下方=二楼 2f。

    game-em 全部 13 张拼图布局一致（一楼最大在上、二楼在下、地下室最小居中偏右）。
    如有例外，用 data/floor_labels.json 人工覆盖。
    """
    if len(floors) != 3:
        return [f"f{i}" for i in range(len(floors))]
    by_area = sorted(range(3), key=lambda i: floors[i]["area"])
    names = [None] * 3
    names[by_area[0]] = "b1"
    rest = by_area[1:]
    rest.sort(key=lambda i: floors[i]["bbox"][1] + floors[i]["bbox"][3] / 2)
    names[rest[0]] = "1f"
    names[rest[1]] = "2f"
    return names


def crop(arr, bbox):
    x, y, w, h = bbox
    return arr[y:y + h, x:x + w]


#: 判定为地图所需的最低房间像素占比。真地图实测 23%，
#: 半透明界面透出的 3D 场景 0.0-0.2%；取 0.04 给开局只露一角的房间留余量。
MIN_ROOM_FRAC = 0.04


def find_map_region(mask, room=None, close=75, margin=30, min_area=8000):
    """从整帧结构掩码里自动定位地图面板区域，返回 bbox 或 None。

    地图结构是画面里最大的一团彼此邻近的掩码块；UI 图标/文字虽然
    色相相同，但零散且远离地图。大核闭运算把邻近块并成整团后，
    优先取「走廊与房间混合」的团（见 structure_mask_parts），
    都不达标时再退回取掩码像素最多的一团，bbox 外扩 margin。
    """
    ker = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (close, close))
    merged = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, ker)
    n, labels, stats, _ = cv2.connectedComponentsWithStats(merged, 8)
    best, best_area = None, 0
    best_mixed, best_mixed_area = None, 0
    for i in range(1, n):
        x, y, w, h = stats[i, cv2.CC_STAT_LEFT], stats[i, cv2.CC_STAT_TOP], \
            stats[i, cv2.CC_STAT_WIDTH], stats[i, cv2.CC_STAT_HEIGHT]
        sel = labels[y:y + h, x:x + w] == i
        area = int(np.count_nonzero(mask[y:y + h, x:x + w][sel]))
        if area > best_area:
            best, best_area = (x, y, w, h), area
        if room is not None and area >= min_area:
            r = int(np.count_nonzero(room[y:y + h, x:x + w][sel]))
            if r >= area * MIN_ROOM_FRAC and area > best_mixed_area:
                best_mixed, best_mixed_area = (x, y, w, h), area
    if best_mixed is not None:
        best, best_area = best_mixed, best_mixed_area
    if best is None or best_area < min_area:
        return None
    x, y, w, h = best
    H, W = mask.shape
    x0, y0 = max(0, x - margin), max(0, y - margin)
    return [x0, y0, min(W, x + w + margin) - x0, min(H, y + h + margin) - y0]


# ---------------------------------------------------------------------------
# 参考库
# ---------------------------------------------------------------------------

def load_library(lib_dir):
    """读参考库：library.json + 各楼层掩码 PNG。返回 (meta, entries)。

    entries: [{variant, floor, mask, meta...}]，mask 为源图分辨率下的裁剪掩码。
    """
    with open(os.path.join(lib_dir, "library.json")) as f:
        meta = json.load(f)
    entries = []
    for v in meta["variants"]:
        for fl in v["floors"]:
            mask = cv2.imread(os.path.join(lib_dir, fl["mask"]), cv2.IMREAD_GRAYSCALE)
            entries.append({"variant": v["id"], "floor": fl["floor"],
                            "source": v["source"], "bbox": fl["bbox"], "mask": mask})
    return meta, entries


# ---------------------------------------------------------------------------
# 匹配：多尺度掩码相关
# ---------------------------------------------------------------------------

MATCH_LONG_EDGE = 160   # 粗筛时参考掩码降采样后的长边
SCALES = np.geomspace(0.5, 2.0, 21)  # 查询相对参考的尺度搜索范围（步长 ~7%）


def _resize_long(mask, long_edge):
    h, w = mask.shape
    s = long_edge / max(h, w)
    out = cv2.resize(mask, (max(1, round(w * s)), max(1, round(h * s))),
                     interpolation=cv2.INTER_AREA)
    return (out > 127).astype(np.uint8), s


def _score_at_scale(ref_small, q_small):
    """在单一尺度下平移搜索，返回 (score, (dx, dy))。

    得分 = 0.6·前景命中率 + 0.4·背景命中率：
      前景命中率 = 查询可行走像素落在参考可行走上的比例；
      背景命中率 = 查询"已探索但不可行走"像素落在参考背景上的比例。
    背景项用来惩罚"哪儿都是可行走"的糊脸匹配。
    """
    qh, qw = q_small.shape
    rh, rw = ref_small.shape
    # 参考图四周留出查询一半大小的余量，容忍查询略微出界
    py, px = qh // 2, qw // 2
    ref_pad = cv2.copyMakeBorder(ref_small, py, py, px, px, cv2.BORDER_CONSTANT, value=0)
    if ref_pad.shape[0] < qh or ref_pad.shape[1] < qw:
        return -1.0, (0, 0)
    q_fg = q_small
    n_fg = int(q_fg.sum())
    if n_fg < 30:
        return -1.0, (0, 0)
    # 已探索区域 ≈ 可行走的膨胀；其中的非可行走像素应落在参考背景上
    k = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (7, 7))
    explored = cv2.dilate(q_fg, k)
    q_bg = (explored > 0) & (q_fg == 0)
    n_bg = max(1, int(q_bg.sum()))

    ref_f = ref_pad.astype(np.float32)
    hit_fg = cv2.matchTemplate(ref_f, q_fg.astype(np.float32), cv2.TM_CCORR)
    hit_bg = cv2.matchTemplate(1.0 - ref_f, q_bg.astype(np.float32), cv2.TM_CCORR)
    score_map = 0.6 * hit_fg / n_fg + 0.4 * hit_bg / n_bg
    _, best, _, loc = cv2.minMaxLoc(score_map)
    return float(best), (loc[0] - px, loc[1] - py)


def _match_entry(q_mask, ref_mask, scales, long_edge):
    """单个参考楼层的多尺度搜索。返回 (score, transform) 或 (−1, None)。

    transform：查询原图 px → 参考裁剪原图 px 的相似变换，ref_xy = q_xy·s + t。
    """
    ref_small, ref_s = _resize_long(ref_mask, long_edge)
    qh, qw = q_mask.shape
    best, best_sc, best_off = -1.0, None, None
    for sc in scales:
        eff = ref_s * sc
        tw, th = max(1, round(qw * eff)), max(1, round(qh * eff))
        if tw > ref_small.shape[1] * 2 or th > ref_small.shape[0] * 2:
            continue
        q_small = (cv2.resize(q_mask, (tw, th), interpolation=cv2.INTER_AREA) > 127).astype(np.uint8)
        s, off = _score_at_scale(ref_small, q_small)
        if s > best:
            best, best_sc, best_off = s, sc, off
    if best_sc is None:
        return -1.0, None
    return best, {"scale": float(best_sc),
                  "tx": best_off[0] / ref_s, "ty": best_off[1] / ref_s}


def match_query(q_mask, entries, scales=SCALES, long_edge=MATCH_LONG_EDGE,
                refine_top=5):
    """查询掩码 vs 参考库全体：低分辨率粗筛全库，前 refine_top 名在
    2 倍分辨率、更细尺度步长下精修后重排。返回按得分降序的结果列表。
    """
    results = []
    for e in entries:
        score, tf = _match_entry(q_mask, e["mask"], scales, long_edge)
        if tf is None:
            continue
        results.append({"variant": e["variant"], "floor": e["floor"], "score": score,
                        "bbox": e["bbox"], "source": e["source"],
                        "transform": tf, "mask_shape": e["mask"].shape,
                        "_mask": e["mask"]})
    results.sort(key=lambda r: -r["score"])
    for r in results[:refine_top]:
        fine = r["transform"]["scale"] * np.geomspace(0.92, 1.09, 9)
        score, tf = _match_entry(q_mask, r["_mask"], fine, long_edge * 2)
        if tf is not None:
            r["score"], r["transform"] = score, tf
    results.sort(key=lambda r: -r["score"])
    for r in results:
        del r["_mask"]
    return results
