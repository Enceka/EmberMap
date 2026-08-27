#!/usr/bin/env python3
"""自动配对 draw-em ↔ game-em 变体。

拿每个 draw 变体的 1f/2f/b1 掩码作查询，匹配 game-em 同楼层的 13 个候选，
三层得分相加，贪心做一对一分配。手绘几何有变形，得分绝对值会偏低，
只要正确对的相对排序稳定即可；结果需人工抽查后写回 data/variants.json。
"""
import json
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def main():
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    dmeta, draw = emlib.load_library(os.path.join(ROOT, "build", "library-draw"))
    dnames = {v["id"]: v.get("name", v["id"]) for v in dmeta["variants"]}

    dids = sorted({e["variant"] for e in draw})
    gids = sorted({e["variant"] for e in game})
    score = np.zeros((len(dids), len(gids)))
    for e in draw:
        cands = [g for g in game if g["floor"] == e["floor"]]
        res = emlib.match_query(e["mask"], cands, refine_top=0)
        for r in res:
            score[dids.index(e["variant"]), gids.index(r["variant"])] += r["score"]

    # 贪心一对一分配（按当前最高分依次锁定）
    s = score.copy()
    pairs = {}
    while len(pairs) < len(dids):
        i, j = np.unravel_index(np.argmax(s), s.shape)
        second = np.partition(score[i], -2)[-2]
        pairs[dids[i]] = (gids[j], score[i, j], score[i, j] - second)
        s[i, :] = s[:, j] = -1

    print(f"{'draw 变体':<14}{'game-em':<16}{'总分':>7}{'与次名分差':>10}")
    for did in dids:
        gid, sc, mg = pairs[did]
        flag = "" if mg > 0.15 else "  ← 分差小，需人工确认"
        print(f"{dnames[did]:<14}{gid:<18}{sc:>7.2f}{mg:>10.2f}{flag}")

    out = {dnames[did]: pairs[did][0] for did in dids}
    with open(os.path.join(ROOT, "build", "pairing_auto.json"), "w") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)


if __name__ == "__main__":
    main()
