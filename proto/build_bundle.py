#!/usr/bin/env python3
"""生成应用数据包 app/bundle/：匹配掩码 + 手绘楼层裁剪图 + 元数据。

draw→game 变换在此预计算（Python 匹配器），运行时应用只做 截图→game 匹配。
"""
import json
import os
import sys

import cv2

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "app", "bundle")


def main():
    os.makedirs(os.path.join(OUT, "masks"), exist_ok=True)
    os.makedirs(os.path.join(OUT, "draw"), exist_ok=True)
    _, game = emlib.load_library(os.path.join(ROOT, "build", "library"))
    dmeta, draw = emlib.load_library(os.path.join(ROOT, "build", "library-draw"))
    with open(os.path.join(ROOT, "data", "variants.json")) as f:
        variants = json.load(f)
    with open(os.path.join(ROOT, "data", "doors_game.json")) as f:
        doors = json.load(f)

    name_by_stem = variants["draw"]
    dv_by_name = {}
    for v in dmeta["variants"]:
        stem = os.path.splitext(os.path.basename(v["source"]))[0]
        dv_by_name[name_by_stem[stem]] = v

    out_variants = []
    for name, gid in variants["pairing"].items():
        dv = dv_by_name[name]
        dimg = cv2.imread(os.path.join(ROOT, dv["source"]))
        floors = []
        for fl in ("1f", "2f", "b1"):
            ge = next(e for e in game if e["variant"] == gid and e["floor"] == fl)
            de = next(e for e in draw if e["variant"] == dv["id"] and e["floor"] == fl)
            dfl = next(f for f in dv["floors"] if f["floor"] == fl)
            tf = emlib.match_query(de["mask"], [ge])[0]["transform"]

            mask_rel = f"masks/{gid}_{fl}.png"
            draw_rel = f"draw/{gid}_{fl}.png"
            cv2.imwrite(os.path.join(OUT, mask_rel), ge["mask"])
            x, y, w, h = dfl["bbox"]
            cv2.imwrite(os.path.join(OUT, draw_rel), dimg[y:y + h, x:x + w])
            floors.append({
                "floor": fl,
                "mask": mask_rel,
                "draw": draw_rel,
                "tf_draw_to_game": {k: round(v, 5) for k, v in tf.items()},
                "doors": [{"label": d["label"], "x": d["x"], "y": d["y"]}
                          for d in doors.get(gid, {}).get("doors", [])
                          if d["floor"] == fl],
            })
            print(f"{name} {fl}: tf={floors[-1]['tf_draw_to_game']} 门×{len(floors[-1]['doors'])}")
        out_variants.append({"id": gid, "name": name, "floors": floors})

    with open(os.path.join(OUT, "bundle.json"), "w") as f:
        json.dump({"version": 1, "variants": out_variants}, f, ensure_ascii=False, indent=1)
    print(f"\n→ {OUT}")


if __name__ == "__main__":
    main()
