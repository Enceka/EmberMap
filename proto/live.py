#!/usr/bin/env python3
"""实机测试：抓屏 → 自动定位地图面板 → 匹配 → 叠加图。

用法：
  live.py                 抓一次屏，处理，结果存 build/live/
  live.py --watch 3       每 3 秒抓一次，检测到地图就更新 build/live/latest.png
                          （用 Preview 打开它，文件变了会自动刷新）
  live.py --file 截图.png  处理已有截图（联调用，不抓屏）
  live.py --open          处理完用 Preview 打开结果

需要给终端宿主 App 授予"屏幕录制"权限（系统设置 → 隐私与安全性）。
"""
import argparse
import os
import subprocess
import sys
import time

import cv2

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402
import overlay as ov  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LIVE_DIR = os.path.join(ROOT, "build", "live")


def capture(path):
    r = subprocess.run(["screencapture", "-x", path], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"抓屏失败：{r.stderr.strip()}\n"
                 f"请在 系统设置→隐私与安全性→屏幕录制 里给终端宿主 App 授权后重试")
    return cv2.imread(path)


def process(img, ctx, out_path):
    """返回 (状态字符串, 是否成功)。成功时叠加图写到 out_path。"""
    game, draw, gm2draw, doors = ctx
    mask, room = emlib.structure_mask_parts(img, "game")
    bbox = emlib.find_map_region(mask, room)
    if bbox is None:
        return "未检测到地图面板", False
    # 探索形状任意，不检查长宽比；但地图面板不会贴屏幕边（UI 有边距），
    # 3D 场景误检几乎都贴边或铺满全屏
    x, y, w, h = bbox
    H, W = mask.shape
    if (w < 180 or h < 180 or x <= 2 or y <= 2
            or x + w >= W - 2 or y + h >= H - 2 or w * h > 0.7 * W * H):
        return f"检测区域不像地图面板（{w}×{h}@{x},{y}，还没打开地图？）", False
    q_mask = mask[y:y + h, x:x + w]
    if cv2.countNonZero(q_mask) < 5000:
        return "地图区域太小（还没打开地图？）", False
    results = emlib.match_query(q_mask, game)
    r, r2 = results[0], results[1]
    name = doors.get(r["variant"], {}).get("name", r["variant"])
    line = (f"{name} {r['floor']}  score={r['score']:.3f} "
            f"(次名 {r2['variant']} {r2['floor']} {r2['score']:.3f})  面板@{bbox}")
    # 实测：真地图命中 ≥0.81，3D 场景误检 ≤0.75，取 0.78 为门槛
    if r["score"] < 0.78:
        return f"低置信，忽略：{line}", False
    out = ov.compose(img[y:y + h, x:x + w], r, game, draw, gm2draw, doors)
    cv2.imwrite(out_path, out)
    return line, True


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file", help="处理已有截图而不抓屏")
    ap.add_argument("--watch", type=float, help="轮询间隔秒数，持续监测")
    ap.add_argument("--open", action="store_true", help="完成后用 Preview 打开结果")
    args = ap.parse_args()

    os.makedirs(LIVE_DIR, exist_ok=True)
    ctx = ov.load_all()
    latest = os.path.join(LIVE_DIR, "latest.png")

    if args.file:
        img = cv2.imread(args.file)
        if img is None:
            sys.exit(f"读不了图：{args.file}")
        msg, ok = process(img, ctx, latest)
        print(msg + (f"\n叠加图 → {latest}" if ok else ""))
        if ok and args.open:
            subprocess.run(["open", latest])
        return

    if args.watch:
        print(f"每 {args.watch}s 抓屏监测中，Ctrl-C 退出；结果实时写入 {latest}")
        last = None
        while True:
            img = capture(os.path.join(LIVE_DIR, "frame.png"))
            msg, ok = process(img, ctx, latest)
            if msg != last:
                print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)
                last = msg
            time.sleep(args.watch)

    img = capture(os.path.join(LIVE_DIR, "frame.png"))
    msg, ok = process(img, ctx, latest)
    print(msg + (f"\n叠加图 → {latest}" if ok else ""))
    if ok and args.open:
        subprocess.run(["open", latest])


if __name__ == "__main__":
    main()
