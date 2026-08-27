# EmberMap

第五人格「加页手记」模式（噩梦难度）的地图识别/覆盖辅助工具。当前为 **算法原型阶段**：
用户截图游戏内地图（部分已探索）→ 识别是 13 个变体中的哪张、哪一层 → 求相似变换 → 对齐显示完整地图。

## 目录

- `draw-em/` — 手绘示意图素材（作者：小红薯@撒娇怪），13 变体 × 3 层，含 `doors.json` 门位标注
- `game-em/` — 游戏内实拍拼图素材（作者：王清心，免费发布禁止出售），与游戏内渲染同风格
- `data/` — 项目自有元数据：变体命名与配对、楼层切分人工覆盖、门位（game 坐标系）
- `proto/` — Python 原型（掩码提取、楼层切分、匹配、评测）
- `build/` — 生成物（参考库、质检图、评测输出），不入库

> 素材为他人作品，仅限本地开发使用；公开分发前必须取得作者授权或改用自绘底图。

## 原型用法

```bash
python3 -m venv .venv && .venv/bin/pip install opencv-python-headless numpy pillow

.venv/bin/python proto/build_library.py   # game-em → build/library/（匹配参考库，39 楼层掩码）
.venv/bin/python proto/build_draw.py      # draw-em → build/library-draw/（展示层）
.venv/bin/python proto/pair.py            # 自动配对 draw↔game 变体
.venv/bin/python proto/map_doors.py       # 门位坐标映射到 game 坐标系
.venv/bin/python proto/synth_eval.py      # 合成"部分探索"查询做交叉评测
.venv/bin/python proto/match.py 截图.png --debug   # 匹配一张真实截图
```

## 算法

1. **结构掩码**：HSV 颜色分割提取可行走结构（走廊+房间），滤除水印/攻略线/文字；
2. **楼层切分**：腐蚀出楼层核心打断细水印桥 → 像素距离单链接聚类到 3 簇 → 像素按最近核心归属；
3. **匹配**：查询掩码对 39 个楼层做多尺度平移搜索（相似变换，无旋转），
   得分 = 0.6×前景命中率 + 0.4×背景命中率；粗筛全库后 top-5 精修重排。

## 合成评测（2026-08-27，350 条查询）

| 探索程度 | top-1 | top-2 | 对齐误差中位 |
|---|---|---|---|
| 开局（测地半径 150px） | 99.1% | 99.1% | 1.7px |
| 中期（350px） | 100% | 100% | 2.1px |
| 完整楼层 | 100% | 100% | 2.6px |

单次匹配约 2s（未优化 Python）。**待办**：真实游戏截图测试集（不同机型/分辨率/探索程度），
用于验证渲染风格差异并校准 `--debug` 掩码颜色阈值。
