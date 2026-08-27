//! 自包含回归测试：从参考库自身合成「部分探索」查询，匹配必须找回原条目。
//! 不依赖任何外部截图，因此不受素材授权与隐私影响。
//!
//! 守的是坐标系/尺度这一类最容易回归的 bug——历史上出现过 track 阶段
//! 先验尺度与分析分辨率错配、导致匹配整体失效。

use em_core::img::Gray;

fn bundle_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../app/bundle")
}

/// 从参考掩码合成查询：取中部一大块（模拟已探索大半层）并缩放，
/// 再挖若干洞（模拟图标/文字遮挡）。全程确定性，便于复现。
fn synth_query(mask: &Gray, scale: f64) -> Gray {
    let (x0, y0) = (mask.w / 8, mask.h / 8);
    let (cw, ch) = (mask.w * 3 / 4, mask.h * 3 / 4);
    let sub = mask.crop(x0, y0, cw, ch);
    let tw = (cw as f64 * scale).round() as usize;
    let th = (ch as f64 * scale).round() as usize;
    let mut q = em_core::img::resize_area_binary(&sub, tw, th);
    for k in 0..6 {
        let hx = (q.w / 7) * k + q.w / 20;
        let hy = (q.h / 5) * (k % 5) + q.h / 20;
        for y in hy..(hy + q.h / 22).min(q.h) {
            for x in hx..(hx + q.w / 22).min(q.w) {
                q.data[y * q.w + x] = 0;
            }
        }
    }
    q
}

#[test]
fn 合成查询必须匹配回原条目() {
    let dir = bundle_dir();
    if !dir.join("bundle.json").exists() {
        eprintln!("跳过：未找到数据包 {}（未随仓库分发时属正常）", dir.display());
        return;
    }
    let lib = em_core::load_library(&dir).expect("加载数据包");
    assert_eq!(lib.entries.len(), 39, "噩梦难度应为 13 变体 × 3 层");

    let masks: Vec<&Gray> = lib.entries.iter().map(|e| &e.mask).collect();
    // 抽样若干条目（覆盖三种楼层），每个用不同缩放，控制总耗时
    let cases: [(usize, f64); 6] = [(0, 0.6), (1, 0.85), (2, 1.15), (14, 0.7), (27, 0.95), (38, 1.3)];
    for (idx, scale) in cases {
        let want = &lib.entries[idx];
        let q = synth_query(&want.mask, scale);
        let res = em_core::matcher::match_query(&q, &masks);
        let top = &res[0];
        let got = &lib.entries[top.entry];
        assert_eq!(
            (got.variant.as_str(), got.floor.as_str()),
            (want.variant.as_str(), want.floor.as_str()),
            "条目 {idx}（缩放 {scale}）匹配错误：得到 {} {} 得分 {:.3}",
            got.name, got.floor, top.score
        );
        // 尺度必须复原（查询是参考的 scale 倍，故变换尺度应为 1/scale）
        let want_scale = 1.0 / scale;
        let err = (top.transform.scale - want_scale).abs() / want_scale;
        assert!(
            err < 0.12,
            "条目 {idx} 尺度还原偏差过大：期望 {want_scale:.3}，得到 {:.3}",
            top.transform.scale
        );
    }
}
