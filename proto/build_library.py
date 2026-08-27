#!/usr/bin/env python3
"""扫描 game-em，提取结构掩码并切分楼层，输出参考库到 build/library/。

同时生成 build/preview/ 下的质检拼图：左=原图+楼层框，右=掩码。
楼层标签先按面积占位（f0/f1/f2...），人工核对后由 floors_labels.json 覆盖。
"""
import glob
import json
import os
import sys

import cv2
import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LIB_DIR = os.path.join(ROOT, "build", "library")
PREV_DIR = os.path.join(ROOT, "build", "preview")
OVERRIDES_PATH = os.path.join(ROOT, "data", "floor_overrides.json")


def preview(img, mask, floors, names, out_path, scale=0.35):
    vis = img.copy()
    for fl, name in zip(floors, names):
        x, y, w, h = fl["bbox"]
        cv2.rectangle(vis, (x, y), (x + w, y + h), (0, 0, 255), 6)
        cv2.putText(vis, name, (x + 8, y + 60), cv2.FONT_HERSHEY_SIMPLEX,
                    2.2, (0, 0, 255), 6)
    vis = cv2.resize(vis, None, fx=scale, fy=scale, interpolation=cv2.INTER_AREA)
    m = cv2.resize(mask, None, fx=scale, fy=scale, interpolation=cv2.INTER_AREA)
    m = cv2.cvtColor(m, cv2.COLOR_GRAY2BGR)
    h = max(vis.shape[0], m.shape[0])
    pad = lambda a: cv2.copyMakeBorder(a, 0, h - a.shape[0], 0, 8, cv2.BORDER_CONSTANT, value=(40, 40, 40))
    cv2.imwrite(out_path, np.hstack([pad(vis), pad(m)]))


def main():
    os.makedirs(LIB_DIR, exist_ok=True)
    os.makedirs(PREV_DIR, exist_ok=True)
    overrides = {}
    if os.path.exists(OVERRIDES_PATH):
        with open(OVERRIDES_PATH) as f:
            overrides = json.load(f)

    files = sorted(glob.glob(os.path.join(ROOT, "game-em", "*.jp*g")))
    variants = []
    for path in files:
        stem = os.path.splitext(os.path.basename(path))[0]
        img = cv2.imread(path)
        mask = emlib.structure_mask(img, "game")
        if stem in overrides:
            floors, names = emlib.split_floors_by_rects(mask, overrides[stem])
        else:
            floors = emlib.split_floors(mask)
            names = emlib.label_floors(floors)
        preview(img, mask, floors, names, os.path.join(PREV_DIR, f"{stem}.jpg"))

        vid = f"gm-{stem[:8]}"
        fl_meta = []
        for fl, name in zip(floors, names):
            mask_name = f"{vid}_{name}.png"
            cv2.imwrite(os.path.join(LIB_DIR, mask_name), fl["mask"])
            fl_meta.append({"floor": name, "bbox": fl["bbox"], "area": fl["area"],
                            "mask": mask_name})
        variants.append({"id": vid, "source": os.path.relpath(path, ROOT), "floors": fl_meta})
        print(f"{stem}: {names} 面积 {[f['area'] for f in floors]}")

    with open(os.path.join(LIB_DIR, "library.json"), "w") as f:
        json.dump({"style": "game", "variants": variants}, f, ensure_ascii=False, indent=1)
    print(f"\n共 {len(variants)} 张图 → {LIB_DIR}")


if __name__ == "__main__":
    main()
