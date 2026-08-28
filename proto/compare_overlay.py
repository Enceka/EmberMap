#!/usr/bin/env python3
"""对比两种叠加层来源：draw-em（手绘）vs game-em（实拍，即匹配参考本身）。

用途是回答「能不能只留一套素材」：匹配层必须是 game-em（手绘几何变形会误判），
那叠加层能否也用 game-em，从而彻底去掉 draw-em？
输出 build/preview/_overlay_source_compare.jpg 供目视评估。
"""
import json
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402
import overlay as ov  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def blend(base, layer, alpha=0.75, thresh=70.0):
    """按亮度把 layer 叠到 base 上（黑底透明），与应用中的 screen 混合一致。"""
    gray = cv2.cvtColor(layer, cv2.COLOR_BGR2GRAY).astype(np.float32)
    a = np.clip(gray / thresh, 0, 1)[..., None] * alpha
    return (base.astype(np.float32) * (1 - a) + layer.astype(np.float32) * a).astype(np.uint8)


def main(shot_path):
    game, draw, gm2draw, doors = ov.load_all()
    with open(os.path.join(ROOT, "build", "library", "library.json")) as f:
        gmeta = json.load(f)

    shot = cv2.imread(shot_path)
    mask, room = emlib.structure_mask_parts(shot, "game")
    bbox = emlib.find_map_region(mask, room)
    x, y, w, h = bbox
    q_mask = mask[y:y + h, x:x + w]
    r = emlib.match_query(q_mask, game)[0]
    gid, floor = r["variant"], r["floor"]
    b, tb = r["transform"]["scale"], np.array([r["transform"]["tx"], r["transform"]["ty"]])
    print(f"匹配：{gid} {floor} {r['score']:.3f}")

    # 整层显示区域 = 参考图投影回画面（与应用一致）
    ge = next(e for e in game if e["variant"] == gid and e["floor"] == floor)
    rh, rw = ge["mask"].shape
    proj = lambda rx, ry: ((rx - tb[0]) / b + x, (ry - tb[1]) / b + y)
    vx0, vy0 = proj(0, 0)
    vx1, vy1 = proj(rw, rh)
    X, Y = int(max(0, vx0)), int(max(0, vy0))
    W = int(min(shot.shape[1], vx1)) - X
    H = int(min(shot.shape[0], vy1)) - Y
    base = shot[Y:Y + H, X:X + W]
    sx, sy = x - X, y - Y

    # A：draw-em 叠加（现方案，需 draw→game 预配准变换）
    dv = gm2draw[gid]
    dfl = next(f for f in dv["floors"] if f["floor"] == floor)
    dimg = cv2.imread(os.path.join(ROOT, dv["source"]))
    dx, dy, dw, dh = dfl["bbox"]
    dcrop = dimg[dy:dy + dh, dx:dx + dw]
    dmask = next(e["mask"] for e in draw if e["variant"] == dv["id"] and e["floor"] == floor)
    tfa = emlib.match_query(dmask, [ge])[0]["transform"]
    s = tfa["scale"] / b
    t = (np.array([tfa["tx"], tfa["ty"]]) - tb) / b + [sx, sy]
    warped_draw = cv2.warpAffine(dcrop, np.float32([[s, 0, t[0]], [0, s, t[1]]]), (W, H))
    a = blend(base, warped_draw)

    # B：game-em 叠加（匹配参考本身，变换即 q→game 的逆，无需预配准）
    gimg = cv2.imread(os.path.join(ROOT, ge["source"]))
    gx, gy, gw, gh = ge["bbox"]
    gcrop = gimg[gy:gy + gh, gx:gx + gw]
    s2 = 1.0 / b
    t2 = (-tb) / b + [sx, sy]
    warped_game = cv2.warpAffine(gcrop, np.float32([[s2, 0, t2[0]], [0, s2, t2[1]]]), (W, H))
    bb = blend(base, warped_game)

    lab = lambda im, txt: cv2.putText(im.copy(), txt, (10, 30),
                                      cv2.FONT_HERSHEY_SIMPLEX, 1.0, (0, 255, 0), 2)
    out = np.hstack([lab(base, "raw"), lab(a, "A: draw-em"), lab(bb, "B: game-em")])
    scale = min(1.0, 1500 / out.shape[1])
    out = cv2.resize(out, None, fx=scale, fy=scale, interpolation=cv2.INTER_AREA)
    p = os.path.join(ROOT, "build", "preview", "_overlay_source_compare.jpg")
    cv2.imwrite(p, out, [cv2.IMWRITE_JPEG_QUALITY, 88])
    print(f"→ {p}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "test", "shot-20260826-2219.png"))
