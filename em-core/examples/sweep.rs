//! 参数扫描：对已知真值的截图，比较不同分辨率/候选数配置下
//! 「正确变体」与「最佳其他变体」的分差（越大越不容易误判）。
//! 用法：cargo run --release --example sweep -- <bundle> <图:期望变体名:期望楼层> ...

use std::time::Instant;

use em_core::{matcher::MatchOpts, Analysis, Options};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let lib = em_core::load_library(std::path::Path::new(&args[0])).expect("bundle");

    let configs: Vec<(&str, Options)> = vec![
        ("半分辨率/旧配置 c160 r320 top5", Options {
            target_long_edge: 1500.0,
            match_opts: MatchOpts { coarse_long_edge: 160, refine_long_edge: 320, refine_top: 5, prior_scale: None },
        }),
        ("半分辨率 c200 r520 top10", Options {
            target_long_edge: 1500.0,
            match_opts: MatchOpts { coarse_long_edge: 200, refine_long_edge: 520, refine_top: 10, prior_scale: None },
        }),
        ("近全分辨率 c200 r520 top10", Options {
            target_long_edge: 2400.0,
            match_opts: MatchOpts { coarse_long_edge: 200, refine_long_edge: 520, refine_top: 10, prior_scale: None },
        }),
        ("跟踪:全库+尺度先验 c200 r520 top10", Options {
            target_long_edge: 1600.0,
            match_opts: MatchOpts { coarse_long_edge: 200, refine_long_edge: 520, refine_top: 10, prior_scale: Some(1.0) },
        }),
        ("跟踪:全库+先验 top5", Options {
            target_long_edge: 1600.0,
            match_opts: MatchOpts { coarse_long_edge: 200, refine_long_edge: 520, refine_top: 5, prior_scale: Some(1.0) },
        }),
    ];

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
