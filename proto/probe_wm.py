#!/usr/bin/env python3
"""列出掩码中的小连通块（疑似水印残渣）及其 HSV 统计，与最大块（真实结构）对比。"""
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402


def stat(hsv, sel, tag):
    px = hsv[sel]
    line = f"  {tag:28s} n={len(px):7d}  "
    for name, ch in zip("HSV", px.T):
        q = np.percentile(ch, [10, 50, 90]).astype(int)
        line += f"{name}[{q[0]:3d},{q[1]:3d},{q[2]:3d}] "
    print(line)


for path in sys.argv[1:]:
    img = cv2.imread(path)
    mask = emlib.structure_mask(img, "game")
    hsv = cv2.cvtColor(cv2.medianBlur(img, 5), cv2.COLOR_BGR2HSV)
    n, labels, stats, _ = cv2.connectedComponentsWithStats(mask, 8)
    order = np.argsort(-stats[1:, cv2.CC_STAT_AREA]) + 1
    print(f"== {os.path.basename(path)}: {n - 1} 个连通块 ==")
    stat(hsv, labels == order[0], "最大块(真实结构)")
    for i in order:
        a = stats[i, cv2.CC_STAT_AREA]
        if a < 8000:
            x, y, w, h = stats[i, cv2.CC_STAT_LEFT], stats[i, cv2.CC_STAT_TOP], \
                stats[i, cv2.CC_STAT_WIDTH], stats[i, cv2.CC_STAT_HEIGHT]
            stat(hsv, labels == i, f"小块 a={a} @({x},{y},{w},{h})")
