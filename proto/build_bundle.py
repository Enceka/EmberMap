#!/usr/bin/env python3
"""生成应用数据包 app/bundle/：匹配掩码 + 手绘楼层裁剪图 + 元数据。

draw→game 变换在此预计算（Python 匹配器），运行时应用只做 截图→game 匹配。
"""
import json
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "app", "bundle")

TITLE_BOX = (240, 70)   # 左上角地图名的右下角（各图一致）
CREDIT_X = 110          # 左缘竖排鸣谢小字的右界
PAD = 6                 # 裁剪留的黑边，圆圈标注贴边也画得下


def floor_layers(img, floors, entries):
    """把手绘整图拆成各楼层的显示图层，返回 {floor: (bbox, 图)}。

    不能按矩形切：三层在图上是斜着错开摆的，谁的外接矩形都盖到别人身上
    （右中门2 的地下室与一楼之间干脆是一条斜分隔线）。矩形切要么切掉自己
    的内容，要么把隔壁楼层的一角带进来，二选一，没有中间地带。

    所以改成按内容归属切：整图的亮像素分连通块，每块判给一个楼层，裁剪框
    取本层内容的包络，框内不属于本层的像素抹黑。黑=透明（叠加层用 screen
    混合，整层视图也是黑底），所以「不规则」的图层用一张矩形位图就能表达。

    掩码只认走廊/房间两种色域；金色特殊房间、楼层标题、路线箭头、房间名
    都不在掩码里，靠这里的归属判定才跟着走。
    """
    H, W = img.shape[:2]
    g = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
    bright = (g > 32).astype(np.uint8)
    names = [f["floor"] for f in floors]

    # 各层结构掩码铺回整图坐标
    full = {}
    for f in floors:
        x, y, w, h = f["bbox"]
        e = next(e for e in entries if e["floor"] == f["floor"])
        m = np.zeros((H, W), np.uint8)
        m[y:y + h, x:x + w] = (e["mask"] > 0)
        full[f["floor"]] = m
    any_struct = np.zeros((H, W), np.uint8)
    for m in full.values():
        any_struct |= m
    struct_d = cv2.dilate(any_struct, np.ones((7, 7), np.uint8))
    nearest_mask = np.stack([
        cv2.distanceTransform((1 - full[n]).astype(np.uint8), cv2.DIST_L2, 3) for n in names
    ]).argmin(0)

    # 1) 不属于任何楼层的三类东西：楼层分隔线、左上角地图名、左缘竖排鸣谢。
    #    只在「不贴结构」的块里认，贴着结构的一律留（宁可多留不可切掉）
    n, lab, st, _ = cv2.connectedComponentsWithStats(bright, connectivity=8)
    drop = np.zeros((H, W), np.uint8)
    for i in range(1, n):
        x, y, w, h, a = st[i]
        comp = lab[y:y + h, x:x + w] == i
        if struct_d[y:y + h, x:x + w][comp].any():
            continue
        long_side = max(w, h)
        if ((long_side >= 100 and a / long_side <= 6)                    # 细长 = 分隔线
                or (x + w <= TITLE_BOX[0] and y + h <= TITLE_BOX[1])     # 地图名
                or (x + w <= CREDIT_X and long_side <= 60)):             # 鸣谢小字
            drop[y:y + h, x:x + w][comp] = 1
    bright2 = bright & (1 - drop)

    # 2) 邻近内容成组后整组归属（房间名紧贴房间、箭头连着路线，逐块判会散架）。
    #    先判碰得到结构的组：按组内像素最近的楼层掩码投票
    grouped = cv2.dilate(bright2, cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (13, 13)))
    n2, lab2, st2, _ = cv2.connectedComponentsWithStats(grouped, connectivity=8)
    assign = np.full((H, W), -1, np.int32)
    floating = []
    for i in range(1, n2):
        x, y, w, h, _ = st2[i]
        comp = (lab2[y:y + h, x:x + w] == i) & (bright2[y:y + h, x:x + w] > 0)
        if not comp.any():
            continue
        if struct_d[y:y + h, x:x + w][comp].any():
            votes = np.bincount(nearest_mask[y:y + h, x:x + w][comp], minlength=len(names))
            assign[y:y + h, x:x + w][comp] = votes.argmax()
        else:
            floating.append((x, y, w, h, comp))
    # 3) 悬空的组（楼层标题、脱开的箭头）按最近的「已分派内容」投票——
    #    掩码可能缺角（HSV 没认全的走廊），已分派内容才是完整的视觉结构
    nearest_content = np.stack([
        cv2.distanceTransform((assign != k).astype(np.uint8), cv2.DIST_L2, 3)
        for k in range(len(names))
    ]).argmin(0)
    for x, y, w, h, comp in floating:
        votes = np.bincount(nearest_content[y:y + h, x:x + w][comp], minlength=len(names))
        assign[y:y + h, x:x + w][comp] = votes.argmax()

    out = {}
    for k, f in enumerate(floors):
        fl = f["floor"]
        # 并上本层掩码：掩码里有些像素偏暗（抗锯齿边）没进 bright，
        # 不并进来会在结构边缘啃出缺口
        mine = (assign == k).astype(np.uint8) | full[fl]
        # 楼层切分是按距离聚类的，偶尔把隔壁楼层的一条边划给本层。只留与本层
        # 已分派内容相连的那一团：真结构必然连着自己，误划来的碎片没有本层
        # 内容与之相连，在这里丢掉（它在隔壁层的图层里还在，不会凭空消失）
        _, ll = cv2.connectedComponents(cv2.dilate(mine, np.ones((3, 3), np.uint8)))
        keep_labels = set(np.unique(ll[assign == k])) - {0}
        mine = (np.isin(ll, list(keep_labels)) & (mine > 0)).astype(np.uint8)

        ys, xs = np.nonzero(mine)
        x0, x1 = max(0, int(xs.min()) - PAD), min(W, int(xs.max()) + 1 + PAD)
        y0, y1 = max(0, int(ys.min()) - PAD), min(H, int(ys.max()) + 1 + PAD)
        # 外扩几像素再抹黑，保住内容边缘的抗锯齿像素与 JPEG 振铃
        keep = cv2.dilate(mine, cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (9, 9)))
        crop = img[y0:y1, x0:x1].copy()
        crop[keep[y0:y1, x0:x1] == 0] = 0
        out[fl] = ([x0, y0, x1 - x0, y1 - y0], crop)

    # 对账：每个亮像素要么归了某层，要么被判为分隔线/图名/鸣谢，不允许有漏网的
    covered = np.zeros((H, W), np.uint8)
    for k in range(len(names)):
        covered |= (assign == k).astype(np.uint8)
    orphan = int((bright & (1 - covered) & (1 - drop)).sum())
    assert orphan == 0, f"有 {orphan} 个亮像素没归属"
    return out


def main():
    os.makedirs(os.path.join(OUT, "masks"), exist_ok=True)
    os.makedirs(os.path.join(OUT, "draw"), exist_ok=True)
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    dmeta, draw = emlib.load_library(os.path.join(ROOT, "build", "library-draw"))
    with open(os.path.join(ROOT, "data", "variants.json")) as f:
        variants = json.load(f)
    with open(os.path.join(ROOT, "data", "doors_game.json")) as f:
        doors = json.load(f)

    name_by_stem = variants["draw"]
    dv_by_name = {}
    for v in dmeta["variants"]:
        stem = os.path.splitext(os.path.basename(v["source"]))[0]
        dv_by_name[name_by_stem[stem]] = v

    out_variants = []
    for name, gid in variants["pairing"].items():
        dv = dv_by_name[name]
        dimg = cv2.imread(os.path.join(ROOT, dv["source"]))
        layers = floor_layers(dimg, dv["floors"], [e for e in draw if e["variant"] == dv["id"]])
        floors = []
        for fl in ("1f", "2f", "b1"):
            ge = next(e for e in game if e["variant"] == gid and e["floor"] == fl)
            de = next(e for e in draw if e["variant"] == dv["id"] and e["floor"] == fl)
            dfl = next(f for f in dv["floors"] if f["floor"] == fl)
            tf = emlib.match_query(de["mask"], [ge])[0]["transform"]

            mask_rel = f"masks/{gid}_{fl}.png"
            draw_rel = f"draw/{gid}_{fl}.png"
            cv2.imwrite(os.path.join(OUT, mask_rel), ge["mask"])
            # 显示图层的裁剪框与掩码 bbox 不同：变换是按掩码局部坐标算的，
            # 裁剪原点移了 (dx, dy)，平移量要跟着补
            x, y, w, h = dfl["bbox"]
            (cx, cy, cw, ch), crop = layers[fl]
            cv2.imwrite(os.path.join(OUT, draw_rel), crop)
            tf["tx"] -= (x - cx) * tf["scale"]
            tf["ty"] -= (y - cy) * tf["scale"]
            floors.append({
                "floor": fl,
                "mask": mask_rel,
                "draw": draw_rel,
                "tf_draw_to_game": {k: round(v, 5) for k, v in tf.items()},
                "doors": [{"label": d["label"], "x": d["x"], "y": d["y"]}
                          for d in doors.get(gid, {}).get("doors", [])
                          if d["floor"] == fl],
            })
            print(f"{name} {fl}: tf={floors[-1]['tf_draw_to_game']} 门×{len(floors[-1]['doors'])}")
        out_variants.append({"id": gid, "name": name, "floors": floors})

    with open(os.path.join(OUT, "bundle.json"), "w") as f:
        json.dump({"version": 1, "variants": out_variants}, f, ensure_ascii=False, indent=1)
    print(f"\n→ {OUT}")


if __name__ == "__main__":
    main()
