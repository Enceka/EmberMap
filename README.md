# EmberMap

第五人格「加页手记」模式（噩梦难度）的地图识别/覆盖辅助工具：
截图游戏内地图（部分已探索）→ 识别是 13 个变体中的哪张、哪一层 → 求相似变换 → 对齐叠加完整手绘图与门位。

## 目录

- `draw-em/` — 手绘示意图素材（作者：小红薯@撒娇怪），13 变体 × 3 层，含 `doors.json` 门位标注
- `game-em/` — 游戏内实拍拼图素材（作者：王清心，免费发布禁止出售），与游戏内渲染同风格
- `data/` — 项目自有元数据：变体命名与配对、楼层切分人工覆盖、门位（game 坐标系）
- `proto/` — Python 原型（掩码提取、楼层切分、匹配、评测、实机监测），算法准绳
- `em-core/` — 纯 Rust 匹配核心（无 OpenCV），语义与 proto/emlib.py 对齐，双实现校验
- `app/` — Tauri 2 应用（`app/bundle/` 为数据包，`proto/build_bundle.py` 生成）
- `test/` — 真实游戏截图测试集
- `build/` — Python 生成物（参考库、质检图、评测输出），不入库

## 应用

```bash
cd app/src-tauri && cargo run --release        # 开发运行
cd app && npx @tauri-apps/cli build            # 打发行包（dmg/nsis/deb/AppImage）
```

首次运行需授予屏幕录制权限（macOS：系统设置 → 隐私与安全性 → 屏幕录制）。

功能：抓游戏窗口自动识别 / 整层地图叠加（覆盖模式为透明点击穿透窗，
不挡操作、对抓屏隐形）/ 并排窗口模式 / 窗口置顶 / 叠加透明度 /
全局热键 **⇧⌥M** 切换覆盖层、**⇧⌥R** 重新识别。

识别策略：首次识别慢而准（全库多尺度，多帧投票攒够证据才锁定，证据不足
宁可持续显示「识别中」）；锁定后仍每帧全库扫描，锁只作显示粘滞，
所以误判能在两帧内自我纠正。详见 `app/src-tauri/src/tracker.rs`。

排障：应用把每帧决策写到 stderr（耗时/相位/证据/分差/候选）；
`EM_DUMP=1` 额外把匹配器所见的裁剪图落到 `/tmp/em_panel.png`。

各平台实现方案（含 Windows 注意事项与 Android 悬浮窗方案）见
[docs/platforms.md](docs/platforms.md)。

一致性校验（Rust vs Python，两张真实截图）：

```bash
cd em-core && cargo run --release --example match_file -- ../app/bundle ../test/*.png
```

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
