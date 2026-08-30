#!/usr/bin/env python3
"""扫描 draw-em 手绘图，提取结构掩码并切分楼层，输出到 build/library-draw/。

draw-em 布局：地下室(上) / 一楼(中) / 二楼(下)，label_floors 的规则同样适用
（地下室最小；一楼在二楼上方）。质检拼图输出到 build/preview-draw/。
"""
import glob
import json
import os
import sys

import cv2

sys.path.insert(0, os.path.dirname(__file__))
import emlib  # noqa: E402
from build_library import preview  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LIB_DIR = os.path.join(ROOT, "build", "library-draw")
PREV_DIR = os.path.join(ROOT, "build", "preview-draw")
OVERRIDES_PATH = os.path.join(ROOT, "data", "floor_overrides.json")


def main():
    os.makedirs(LIB_DIR, exist_ok=True)
    os.makedirs(PREV_DIR, exist_ok=True)
    overrides = {}
    if os.path.exists(OVERRIDES_PATH):
        with open(OVERRIDES_PATH) as f:
            overrides = json.load(f)
    with open(os.path.join(ROOT, "data", "variants.json")) as f:
        names = json.load(f)["draw"]

    variants = []
    for path in sorted(glob.glob(os.path.join(ROOT, "draw-em", "*.jpeg"))):
        stem = os.path.splitext(os.path.basename(path))[0]
        img = cv2.imread(path)
        mask = emlib.structure_mask(img, "draw")
        if stem in overrides:
            floors, fnames = emlib.split_floors_by_rects(mask, overrides[stem])
        else:
            floors = emlib.split_floors(mask)
            fnames = emlib.label_floors(floors)
        preview(img, mask, floors, fnames, os.path.join(PREV_DIR, f"{stem}.jpg"))

        vid = f"dw-{stem[:8]}"
        fl_meta = []
        for fl, name in zip(floors, fnames):
            mask_name = f"{vid}_{name}.png"
            cv2.imwrite(os.path.join(LIB_DIR, mask_name), fl["mask"])
            fl_meta.append({"floor": name, "bbox": fl["bbox"], "area": fl["area"],
                            "mask": mask_name})
        variants.append({"id": vid, "source": os.path.relpath(path, ROOT),
                         "name": names.get(stem, stem), "floors": fl_meta})
        print(f"{names.get(stem, stem)}: {fnames} 面积 {[f['area'] for f in floors]}")

    with open(os.path.join(LIB_DIR, "library.json"), "w") as f:
        json.dump({"style": "draw", "variants": variants}, f, ensure_ascii=False, indent=1)
    print(f"\n共 {len(variants)} 张图 → {LIB_DIR}")


if __name__ == "__main__":
    main()
