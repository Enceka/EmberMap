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

# 左缘竖排鸣谢小字所在列：贯穿整图，不属于任何楼层，扩展裁剪时不计
CREDITS_COL = 80


def display_bboxes(img, floors):
    """把各楼层的掩码 bbox 扩展成「显示用」bbox。

    掩码只认走廊/房间两种色域，金色特殊房间（祭台/酒窖…）、楼层标题、
    路线箭头都不在掩码里——紧贴掩码裁剪会把它们切掉（实测 13 张图的
    地下室全部中招）。这里在各楼层的竖直带内取全部亮内容的包络：
    带边界取相邻楼层掩码 bbox 的中线，保证不会把邻层内容裁进来。

    返回 {floor: [x, y, w, h]}（源图坐标）。
    """
    g = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
    bright = (g > 32).astype(np.uint8)
    bright[:, :CREDITS_COL] = 0
    bright = cv2.morphologyEx(bright, cv2.MORPH_OPEN, np.ones((3, 3), np.uint8))
    H, W = g.shape

    order = sorted(floors, key=lambda f: f["bbox"][1])
    out = {}
    for i, f in enumerate(order):
        x, y, w, h = f["bbox"]
        y0 = 0 if i == 0 else (order[i - 1]["bbox"][1] + order[i - 1]["bbox"][3] + y) // 2
        y1 = H if i == len(order) - 1 else (y + h + order[i + 1]["bbox"][1]) // 2
        band = bright[y0:y1]
        ys, xs = np.nonzero(band)
        if len(xs):
            nx0 = min(x, int(xs.min()))
            nx1 = max(x + w, int(xs.max()) + 1)
            ny0 = min(y, int(ys.min()) + y0)
            ny1 = max(y + h, int(ys.max()) + 1 + y0)
        else:
            nx0, nx1, ny0, ny1 = x, x + w, y, y + h
        # 略留一圈黑边，圆圈标注贴边也画得下
        pad = 6
        nx0 = max(0, nx0 - pad); ny0 = max(y0, ny0 - pad)
        nx1 = min(W, nx1 + pad); ny1 = min(y1, ny1 + pad)
        out[f["floor"]] = [nx0, ny0, nx1 - nx0, ny1 - ny0]
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
        disp = display_bboxes(dimg, dv["floors"])
        floors = []
        for fl in ("1f", "2f", "b1"):
            ge = next(e for e in game if e["variant"] == gid and e["floor"] == fl)
            de = next(e for e in draw if e["variant"] == dv["id"] and e["floor"] == fl)
            dfl = next(f for f in dv["floors"] if f["floor"] == fl)
            tf = emlib.match_query(de["mask"], [ge])[0]["transform"]

            mask_rel = f"masks/{gid}_{fl}.png"
            draw_rel = f"draw/{gid}_{fl}.png"
            cv2.imwrite(os.path.join(OUT, mask_rel), ge["mask"])
            # 显示裁剪比掩码 bbox 大：变换是按掩码局部坐标算的，
            # 裁剪原点左上移了 (dx, dy)，平移量要跟着补
            x, y, w, h = dfl["bbox"]
            cx, cy, cw, ch = disp[fl]
            cv2.imwrite(os.path.join(OUT, draw_rel), dimg[cy:cy + ch, cx:cx + cw])
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
