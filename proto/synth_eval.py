#!/usr/bin/env python3
"""从参考库合成"部分探索"查询，交叉匹配全库，测识别准确率与对齐误差。

查询合成流程（模拟真实使用场景）：
  1. 取某变体某楼层掩码，在结构上随机取种子点（开局=门附近某点）；
  2. 沿结构测地生长（迭代掩码内膨胀），半径对应探索程度；
  3. 随机相似变换（缩放 0.6–1.5 + 平移入画布，无旋转）；
  4. 退化：随机开/闭运算、随机洞（图标/标注遮挡）、随机噪声斑。

注意：这只验证 13 个变体间的可判别性与对齐精度；
真实截图的渲染风格差异要等实机截图测试集来验证。
"""
import os
import sys
import time

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LIB_DIR = os.path.join(ROOT, "build", "library")
OUT_DIR = os.path.join(ROOT, "build", "eval")

RADII = {"early": 150, "mid": 350, "full": 100000}
SEEDS_PER_CASE = 3


def geodesic_grow(mask, seed, radius):
    region = np.zeros_like(mask)
    cv2.circle(region, seed, 6, 255, -1)
    region &= mask
    k = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (7, 7))  # 每轮半径 +3px
    prev = 0
    for _ in range(max(1, radius // 3)):
        region = cv2.dilate(region, k) & mask
        n = cv2.countNonZero(region)
        if n == prev:
            break
        prev = n
    return region


def make_query(floor_mask, rng, radius):
    """返回 (查询画布, 真值变换 q→ref: (s, tx, ty)) 或 None。"""
    ys, xs = np.nonzero(floor_mask)
    i = rng.integers(len(xs))
    region = geodesic_grow(floor_mask, (int(xs[i]), int(ys[i])), radius)
    if cv2.countNonZero(region) < 3000:
        return None
    rys, rxs = np.nonzero(region)
    cx, cy = int(rxs.min()), int(rys.min())
    part = region[cy:rys.max() + 1, cx:rxs.max() + 1]

    sigma = float(rng.uniform(0.6, 1.5))
    part = cv2.resize(part, None, fx=sigma, fy=sigma, interpolation=cv2.INTER_AREA)
    part = np.where(part > 127, 255, 0).astype(np.uint8)
    mx, my = int(rng.integers(10, 80)), int(rng.integers(10, 80))
    canvas = cv2.copyMakeBorder(part, my, int(rng.integers(10, 80)),
                                mx, int(rng.integers(10, 80)),
                                cv2.BORDER_CONSTANT, value=0)

    # 退化
    h, w = canvas.shape
    for _ in range(int(rng.integers(3, 9))):     # 遮挡洞（图标/文字/玩家标记）
        hw, hh = int(rng.integers(8, 50)), int(rng.integers(8, 50))
        x, y = int(rng.integers(0, max(1, w - hw))), int(rng.integers(0, max(1, h - hh)))
        canvas[y:y + hh, x:x + hw] = 0
    for _ in range(int(rng.integers(1, 5))):     # 噪声斑（UI 误检）
        cv2.circle(canvas, (int(rng.integers(0, w)), int(rng.integers(0, h))),
                   int(rng.integers(3, 12)), 255, -1)
    op = rng.integers(0, 3)
    if op == 1:
        canvas = cv2.morphologyEx(canvas, cv2.MORPH_OPEN, np.ones((3, 3), np.uint8))
    elif op == 2:
        canvas = cv2.morphologyEx(canvas, cv2.MORPH_CLOSE, np.ones((5, 5), np.uint8))

    truth = {"scale": 1.0 / sigma, "tx": cx - mx / sigma, "ty": cy - my / sigma}
    return canvas, truth


def align_err(qshape, est, truth):
    h, w = qshape
    pts = np.array([[0, 0], [w, 0], [0, h], [w, h]], float)
    a = pts * est["scale"] + [est["tx"], est["ty"]]
    b = pts * truth["scale"] + [truth["tx"], truth["ty"]]
    return float(np.mean(np.linalg.norm(a - b, axis=1)))


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    _, entries = emlib.load_library(LIB_DIR)
    rng = np.random.default_rng(42)
    stats = {r: {"n": 0, "top1": 0, "top2": 0, "errs": [], "margins": []} for r in RADII}
    fails = []
    t0 = time.time()
    nq = 0
    for e in entries:
        for rname, radius in RADII.items():
            for _ in range(SEEDS_PER_CASE):
                q = make_query(e["mask"], rng, radius)
                if q is None:
                    continue
                canvas, truth = q
                res = emlib.match_query(canvas, entries)
                nq += 1
                st = stats[rname]
                st["n"] += 1
                got = (res[0]["variant"], res[0]["floor"])
                want = (e["variant"], e["floor"])
                if got == want:
                    st["top1"] += 1
                    st["errs"].append(align_err(canvas.shape, res[0]["transform"], truth))
                    other = next((r for r in res[1:] if r["variant"] != e["variant"]), None)
                    if other:
                        st["margins"].append(res[0]["score"] - other["score"])
                else:
                    fails.append((want, got, rname, res[0]["score"], canvas))
                if got == want or want in [(r["variant"], r["floor"]) for r in res[:2]]:
                    st["top2"] += 1
    dt = time.time() - t0

    print(f"\n查询总数 {nq}，总耗时 {dt:.0f}s，单次匹配 {dt / max(nq,1):.2f}s\n")
    print(f"{'探索程度':<8}{'n':>5}{'top1':>8}{'top2':>8}{'对齐误差中位(px)':>18}{'分差中位':>10}")
    for rname, st in stats.items():
        if st["n"] == 0:
            continue
        err = np.median(st["errs"]) if st["errs"] else float("nan")
        mg = np.median(st["margins"]) if st["margins"] else float("nan")
        print(f"{rname:<10}{st['n']:>5}{st['top1'] / st['n']:>8.1%}"
              f"{st['top2'] / st['n']:>8.1%}{err:>16.1f}{mg:>12.3f}")

    for i, (want, got, rname, score, canvas) in enumerate(fails[:12]):
        cv2.imwrite(os.path.join(OUT_DIR, f"fail{i}_{rname}_{want[0]}-{want[1]}_as_{got[0]}-{got[1]}.png"), canvas)
    if fails:
        print(f"\n{len(fails)} 个失败样例，前 12 个查询掩码已存 {OUT_DIR}")


if __name__ == "__main__":
    main()
