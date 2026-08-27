#!/usr/bin/env python3
"""叠加显示：识别截图 → 把配对的手绘完整楼层图对齐叠加到截图上。

链路：截图 → 结构掩码 → 匹配 game 库得 q→game 变换
     → 取配对 draw 变体同层，匹配得 draw→game 变换
     → 复合出 draw→截图 变换，把手绘图（亮度作 alpha）叠上去
     → 门位（game 坐标）映射回截图标注。

用法：overlay.py 截图.png [--crop x,y,w,h] [--out 输出.png]
"""
import argparse
import json
import os
import sys

import cv2
import numpy as np
from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

FONT_CANDIDATES = ["/System/Library/Fonts/PingFang.ttc",
                   "/System/Library/Fonts/Hiragino Sans GB.ttc"]


def load_font(size):
    for p in FONT_CANDIDATES:
        if os.path.exists(p):
            return ImageFont.truetype(p, size)
    return ImageFont.load_default()


def load_all():
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    dmeta, draw = emlib.load_library(os.path.join(ROOT, "build", "library-draw"))
    with open(os.path.join(ROOT, "data", "variants.json")) as f:
        variants = json.load(f)
    with open(os.path.join(ROOT, "data", "doors_game.json")) as f:
        doors = json.load(f)
    # gm id → (draw 变体 id, 变体名)
    name_by_stem = variants["draw"]
    dv_by_name = {}
    for v in dmeta["variants"]:
        stem = os.path.splitext(os.path.basename(v["source"]))[0]
        dv_by_name[name_by_stem[stem]] = v
    gm2draw = {gid: dv_by_name[name] for name, gid in variants["pairing"].items()}
    return game, draw, gm2draw, doors


def compose(shot, result, game, draw, gm2draw, doors, alpha=0.72):
    """把手绘完整楼层叠加到截图，返回 BGR 图。"""
    gid, floor = result["variant"], result["floor"]
    tf_qg = result["transform"]           # q→game: g = q·b + tb
    b, tb = tf_qg["scale"], np.array([tf_qg["tx"], tf_qg["ty"]])

    dv = gm2draw[gid]
    dfl = next(f for f in dv["floors"] if f["floor"] == floor)
    dmask = next(e["mask"] for e in draw
                 if e["variant"] == dv["id"] and e["floor"] == floor)
    gentry = [e for e in game if e["variant"] == gid and e["floor"] == floor]
    tf_dg = emlib.match_query(dmask, gentry)[0]["transform"]  # d→game: g = d·a + ta
    a, ta = tf_dg["scale"], np.array([tf_dg["tx"], tf_dg["ty"]])

    # draw→截图：q = d·(a/b) + (ta−tb)/b
    s = a / b
    t = (ta - tb) / b
    dimg = cv2.imread(os.path.join(ROOT, dv["source"]))
    x, y, w, h = dfl["bbox"]
    dcrop = dimg[y:y + h, x:x + w]
    M = np.float32([[s, 0, t[0]], [0, s, t[1]]])
    H, W = shot.shape[:2]
    warped = cv2.warpAffine(dcrop, M, (W, H), flags=cv2.INTER_LINEAR,
                            borderValue=(0, 0, 0))

    # 手绘图黑底：用亮度当 alpha，结构/文字/路线可见，底色透明
    gray = cv2.cvtColor(warped, cv2.COLOR_BGR2GRAY).astype(np.float32)
    a_map = np.clip(gray / 70.0, 0, 1)[..., None] * alpha
    out = (shot.astype(np.float32) * (1 - a_map) + warped.astype(np.float32) * a_map)
    out = out.astype(np.uint8)

    # 门位标注（game 坐标 → 截图坐标）+ 标题，用 PIL 画中文
    pil = Image.fromarray(cv2.cvtColor(out, cv2.COLOR_BGR2RGB))
    dr = ImageDraw.Draw(pil)
    font = load_font(max(16, int(H * 0.028)))
    for door in doors.get(gid, {}).get("doors", []):
        if door["floor"] != floor:
            continue
        q = (np.array([door["x"], door["y"]]) - tb) / b
        qx, qy = float(q[0]), float(q[1])
        r = max(8, int(H * 0.012))
        dr.ellipse([qx - r, qy - r, qx + r, qy + r], outline=(255, 80, 80), width=4)
        dr.text((qx + r + 4, qy - r), door["label"], fill=(255, 120, 120),
                font=font, stroke_width=2, stroke_fill=(0, 0, 0))
    name = doors.get(gid, {}).get("name", gid)
    fl_cn = {"1f": "一楼", "2f": "二楼", "b1": "地下室"}[floor]
    dr.text((12, 8), f"{name}·{fl_cn}  置信 {result['score']:.2f}",
            fill=(120, 255, 120), font=load_font(max(20, int(H * 0.035))),
            stroke_width=2, stroke_fill=(0, 0, 0))
    return cv2.cvtColor(np.array(pil), cv2.COLOR_RGB2BGR)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("shot")
    ap.add_argument("--crop", help="x,y,w,h 地图面板区域")
    ap.add_argument("--out")
    args = ap.parse_args()

    shot = cv2.imread(args.shot)
    if shot is None:
        sys.exit(f"读不了图：{args.shot}")
    if args.crop:
        x, y, w, h = map(int, args.crop.split(","))
        shot = shot[y:y + h, x:x + w]

    game, draw, gm2draw, doors = load_all()
    q_mask = emlib.structure_mask(shot, "game")
    if cv2.countNonZero(q_mask) < 3000:
        sys.exit("查询掩码太小，检查颜色阈值或 --crop")
    results = emlib.match_query(q_mask, game)
    r = results[0]
    print(f"匹配：{r['variant']} {r['floor']}  score={r['score']:.3f}"
          f"（次名 {results[1]['variant']} {results[1]['floor']} {results[1]['score']:.3f}）")

    out = compose(shot, r, game, draw, gm2draw, doors)
    out_path = args.out or os.path.join(
        ROOT, "build", "match_out",
        os.path.splitext(os.path.basename(args.shot))[0] + "_overlay.png")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    cv2.imwrite(out_path, out)
    print(f"叠加图 → {out_path}")


if __name__ == "__main__":
    main()
