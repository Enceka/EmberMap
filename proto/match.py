#!/usr/bin/env python3
"""匹配一张游戏内地图截图：识别变体+楼层，并可视化对齐结果。

用法：
  match.py 截图.png [--crop x,y,w,h] [--debug]

  --crop   只处理截图中的地图面板区域（游戏 UI 位置固定时先手动量一次）
  --debug  同时保存提取的查询掩码，便于对真实截图校颜色阈值

输出：终端打印 top-5；build/match_out/ 下保存对齐可视化
（截图上叠加匹配到的参考楼层轮廓，绿色）。
"""
import argparse
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "build", "match_out")


def visualize(shot, result, out_path):
    """把匹配到的参考楼层结构轮廓按逆变换画回截图。"""
    tf = result["transform"]
    s, tx, ty = tf["scale"], tf["tx"], tf["ty"]
    _, entries = emlib.load_library(os.path.join(ROOT, "build", "library"))
    ref = next(e["mask"] for e in entries
               if e["variant"] == result["variant"] and e["floor"] == result["floor"])
    contours, _ = cv2.findContours(ref, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
    vis = shot.copy()
    for c in contours:
        pts = (c.reshape(-1, 2).astype(np.float64) - [tx, ty]) / s
        cv2.polylines(vis, [pts.astype(np.int32)], True, (0, 255, 0), 2)
    label = f"{result['variant']} {result['floor']}  score={result['score']:.3f}"
    cv2.putText(vis, label, (10, 30), cv2.FONT_HERSHEY_SIMPLEX, 0.9, (0, 255, 0), 2)
    cv2.imwrite(out_path, vis)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("shot")
    ap.add_argument("--crop", help="x,y,w,h 地图面板区域")
    ap.add_argument("--debug", action="store_true")
    args = ap.parse_args()

    os.makedirs(OUT_DIR, exist_ok=True)
    img = cv2.imread(args.shot)
    if img is None:
        sys.exit(f"读不了图：{args.shot}")
    if args.crop:
        x, y, w, h = map(int, args.crop.split(","))
        img = img[y:y + h, x:x + w]

    q_mask = emlib.structure_mask(img, "game")
    stem = os.path.splitext(os.path.basename(args.shot))[0]
    if args.debug:
        cv2.imwrite(os.path.join(OUT_DIR, f"{stem}_mask.png"), q_mask)
    n = cv2.countNonZero(q_mask)
    if n < 3000:
        sys.exit(f"查询掩码只有 {n} 像素——多半是颜色阈值不适配这张截图，"
                 f"用 --debug 看掩码，或检查 --crop 区域")

    _, entries = emlib.load_library(os.path.join(ROOT, "build", "library"))
    results = emlib.match_query(q_mask, entries)
    print(f"{'变体':<14}{'楼层':<5}{'得分':>7}")
    for r in results[:5]:
        print(f"{r['variant']:<14}{r['floor']:<5}{r['score']:>7.3f}")

    out = os.path.join(OUT_DIR, f"{stem}_match.png")
    visualize(img, results[0], out)
    print(f"\n可视化 → {out}")


if __name__ == "__main__":
    main()
