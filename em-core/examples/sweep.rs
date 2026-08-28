//! 参数扫描：对已知真值的截图，比较不同分辨率/候选数配置下
//! 「正确变体」与「最佳其他变体」的分差（越大越不容易误判）。
//! 用法：cargo run --release --example sweep -- <bundle> <图:期望变体名:期望楼层> ...
//!
//! 截图需自备（本仓库不分发游戏截图）。CI 的自动回归改用
//! tests/synth_regression.rs，不依赖外部图片。

use std::time::Instant;

use em_core::{matcher::MatchOpts, Analysis, Options};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let lib = em_core::load_library(std::path::Path::new(&args[0])).expect("bundle");

    // 命令行给 --prior <全分辨率尺度> 时额外测跟踪配置
    let prior: Option<f64> = std::env::var("EM_PRIOR").ok().and_then(|s| s.parse().ok());
    let mk = |label: &'static str, tle: f64, c: usize, r: usize, top: usize, p: Option<f64>| {
        (label, Options {
            target_long_edge: tle,
            match_opts: MatchOpts { coarse_long_edge: c, refine_long_edge: r, refine_top: top, prior_scale: None },
            prior_scale_full: p,
        })
    };
    let mut configs: Vec<(&str, Options)> = vec![
        mk("acquire 桌面 tle2400 c200 r520 top10", 2400.0, 200, 520, 10, None),
        // 移动端当前档位：先由 Kotlin 缩到 2000，再按 tle1000 分析
        mk("mobile 现档 tle1000 c200 r380 top6", 1000.0, 200, 380, 6, None),
        mk("mobile 候选 tle1600 c200 r380 top6", 1600.0, 200, 380, 6, None),
        mk("mobile 候选 tle2000 c200 r520 top8", 2000.0, 200, 520, 8, None),
    ];
    if let Some(p) = prior {
        configs.push(mk("track tle2400 top5 +先验", 2400.0, 200, 520, 5, Some(p)));
        configs.push(mk("track tle1600 top5 +先验", 1600.0, 200, 520, 5, Some(p)));
    }

    for spec in &args[1..] {
        let parts: Vec<&str> = spec.split(':').collect();
        let (path, want_name, want_floor) = (parts[0], parts[1], parts[2]);
        let im = image::open(path).expect("读图").to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        println!("\n=== {path}  真值 {want_name} {want_floor}");
        for (label, opt) in &configs {
            let t = Instant::now();
            let res = em_core::analyze_with(im.as_raw(), w, h, &lib, None, opt);
            let dt = t.elapsed();
            match res {
                Analysis::NoPanel { reason } => println!("  {label:32} 无面板：{reason}"),
                Analysis::Matched { candidates, .. } => {
                    let best = &candidates[0];
                    let ok = best.name == want_name && best.floor == want_floor;
                    // 正确变体最高分 vs 最佳其他变体
                    let mine = candidates.iter().filter(|c| c.name == want_name)
                        .map(|c| c.score).fold(f32::MIN, f32::max);
                    let other = candidates.iter().filter(|c| c.name != want_name)
                        .map(|c| c.score).fold(f32::MIN, f32::max);
                    println!(
                        "  {label:32} {} top1={} {} ({:.3})  真值分={:.3} 他变体最高={:.3} 分差={:+.3}  {:?}",
                        if ok { "✓" } else { "✗" },
                        best.name, best.floor, best.score, mine, other, mine - other, dt
                    );
                }
            }
        }
    }
}
