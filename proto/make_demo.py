#!/usr/bin/env python3
"""造模拟"探索一半"的假截图：取某变体某层，测地生长出已探索区，
区外糊上迷雾底色，再加缩放/模糊/JPEG 压缩退化。真实截图到位前的联调用。"""
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402
from synth_eval import geodesic_grow  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FOG = (52, 42, 33)

CASES = [
    ("gm-f0b73c1c", "1f", 350, 1.15),
    ("gm-0c96f737", "2f", 300, 0.85),
    ("gm-9b1c9548", "b1", 260, 1.0),
]


def main():
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    rng = np.random.default_rng(20260827)
    out_dir = os.path.join(ROOT, "build", "demo")
    os.makedirs(out_dir, exist_ok=True)
    for gid, floor, radius, scale in CASES:
        e = next(x for x in game if x["variant"] == gid and x["floor"] == floor)
        img = cv2.imread(os.path.join(ROOT, e["source"]))
        x, y, w, h = e["bbox"]
        crop = img[y:y + h, x:x + w].copy()
        mask = e["mask"]
        ys, xs = np.nonzero(mask)
        i = rng.integers(len(xs))
        region = geodesic_grow(mask, (int(xs[i]), int(ys[i])), radius)
        vis = cv2.dilate(region, np.ones((25, 25), np.uint8))  # 可见范围比结构略宽
        fog = np.full_like(crop, FOG)
        shot = np.where(vis[..., None] > 0, crop, fog)
        # 裁到已探索区附近（模拟游戏只显示探索到的部分）
        rys, rxs = np.nonzero(vis)
        m = 40
        x0, y0 = max(0, rxs.min() - m), max(0, rys.min() - m)
        x1, y1 = min(w, rxs.max() + m), min(h, rys.max() + m)
        shot = shot[y0:y1, x0:x1]
        shot = cv2.resize(shot, None, fx=scale, fy=scale, interpolation=cv2.INTER_AREA)
        shot = cv2.GaussianBlur(shot, (3, 3), 0)
        path = os.path.join(out_dir, f"demo_{gid}_{floor}.jpg")
        cv2.imwrite(path, shot, [cv2.IMWRITE_JPEG_QUALITY, 70])
        print(path, shot.shape)


if __name__ == "__main__":
    main()
