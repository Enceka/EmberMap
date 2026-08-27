#!/usr/bin/env python3
"""对样例图做 k-means 聚类，打印主要颜色（BGR/HSV/占比），用于确定分割阈值。"""
import sys

import cv2
import numpy as np


def probe(path, k=12):
    img = cv2.imread(path)
    small = cv2.resize(img, None, fx=0.25, fy=0.25, interpolation=cv2.INTER_AREA)
    pixels = small.reshape(-1, 3).astype(np.float32)
    criteria = (cv2.TERM_CRITERIA_EPS + cv2.TERM_CRITERIA_MAX_ITER, 30, 1.0)
    _, labels, centers = cv2.kmeans(pixels, k, None, criteria, 3, cv2.KMEANS_PP_CENTERS)
    counts = np.bincount(labels.ravel(), minlength=k)
    order = np.argsort(-counts)
    print(f"== {path} ==")
    for i in order:
        b, g, r = centers[i]
        hsv = cv2.cvtColor(np.uint8([[[b, g, r]]]), cv2.COLOR_BGR2HSV)[0, 0]
        pct = 100.0 * counts[i] / counts.sum()
        print(f"  {pct:5.1f}%  BGR=({b:3.0f},{g:3.0f},{r:3.0f})  HSV=({hsv[0]:3d},{hsv[1]:3d},{hsv[2]:3d})")


for p in sys.argv[1:]:
    probe(p)
