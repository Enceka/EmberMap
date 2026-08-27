#!/usr/bin/env python3
"""把 draw-em/doors.json 的门坐标映射到 game-em 楼层坐标系。

doors.json 坐标在 draw-em 全图坐标系：正门在一楼、阳台门在二楼。
流程：全图坐标 − 楼层 bbox → draw 楼层坐标 → (匹配求相似变换) → game 楼层坐标。
输出 data/doors_game.json 和可视化 build/preview/_doors_check.jpg。
"""
import json
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOOR_FLOOR = {"正门": "1f", "阳台门": "2f"}


def main():
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    dmeta, draw = emlib.load_library(os.path.join(ROOT, "build", "library-draw"))
    with open(os.path.join(ROOT, "draw-em", "doors.json")) as f:
        doors = json.load(f)
    with open(os.path.join(ROOT, "data", "variants.json")) as f:
        variants = json.load(f)
    dname_by_stem = variants["draw"]
    pairing = variants["pairing"]

    out = {}
    tiles = []
    for dv in dmeta["variants"]:
        stem = os.path.splitext(os.path.basename(dv["source"]))[0]
        name = dname_by_stem[stem]
        gid = pairing[name]
        out[gid] = {"name": name, "doors": []}
        for door in doors.get(stem, []):
            fl = DOOR_FLOOR[door["label"]]
            dfl = next(f for f in dv["floors"] if f["floor"] == fl)
            dmask = next(e["mask"] for e in draw
                         if e["variant"] == dv["id"] and e["floor"] == fl)
            gentry = [e for e in game if e["variant"] == gid and e["floor"] == fl]
            tf = emlib.match_query(dmask, gentry)[0]["transform"]
            qx, qy = door["x"] - dfl["bbox"][0], door["y"] - dfl["bbox"][1]
            gx, gy = qx * tf["scale"] + tf["tx"], qy * tf["scale"] + tf["ty"]
            out[gid]["doors"].append({"label": door["label"], "floor": fl,
                                      "x": round(gx, 1), "y": round(gy, 1)})
            vis = cv2.cvtColor(gentry[0]["mask"], cv2.COLOR_GRAY2BGR)
            cv2.circle(vis, (int(gx), int(gy)), 14, (0, 0, 255), 4)
            vis = cv2.resize(vis, None, fx=0.28, fy=0.28, interpolation=cv2.INTER_AREA)
            cv2.putText(vis, f"{name} {door['label']}", (4, 16),
                        cv2.FONT_HERSHEY_SIMPLEX, 0.45, (0, 255, 255), 1)
            tiles.append(vis)

    with open(os.path.join(ROOT, "data", "doors_game.json"), "w") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)

    h = max(t.shape[0] for t in tiles)
    w = max(t.shape[1] for t in tiles)
    tiles = [cv2.copyMakeBorder(t, 0, h - t.shape[0], 0, w - t.shape[1] + 4,
                                cv2.BORDER_CONSTANT, value=(30, 30, 30)) for t in tiles]
    rows = [np.hstack(tiles[i:i + 5]) for i in range(0, len(tiles), 5)]
    ww = max(r.shape[1] for r in rows)
    rows = [cv2.copyMakeBorder(r, 0, 4, 0, ww - r.shape[1],
                               cv2.BORDER_CONSTANT, value=(30, 30, 30)) for r in rows]
    cv2.imwrite(os.path.join(ROOT, "build", "preview", "_doors_check.jpg"),
                np.vstack(rows), [cv2.IMWRITE_JPEG_QUALITY, 85])
    print(f"{sum(len(v['doors']) for v in out.values())} 个门 → data/doors_game.json")


if __name__ == "__main__":
    main()
