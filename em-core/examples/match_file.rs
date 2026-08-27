//! 一致性校验 CLI：em-core 匹配一张截图，与 Python 原型输出对比。
//! 用法：cargo run --release --example match_file -- <bundle目录> <截图...>

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法: match_file <bundle目录> <截图...>");
        std::process::exit(1);
    }
    let t0 = Instant::now();
    let lib = em_core::load_library(std::path::Path::new(&args[0])).expect("加载 bundle 失败");
    println!("库 {} 条目，加载 {:?}", lib.entries.len(), t0.elapsed());

    for path in &args[1..] {
        let im = image::open(path).expect("读图失败").to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        let t = Instant::now();
        let res = em_core::analyze(im.as_raw(), w, h, &lib);
        println!("\n== {path}  {w}×{h}  耗时 {:?}", t.elapsed());
        match res {
            em_core::Analysis::NoPanel { reason } => println!("  {reason}"),
            em_core::Analysis::Matched { panel, confident, candidates } => {
                println!("  面板@{:?}  过门槛={confident}", panel);
                for c in candidates.iter().take(5) {
                    println!(
                        "  {} {}  score={:.3}  tf=(s={:.3}, tx={:.1}, ty={:.1})",
                        c.name, c.floor, c.score,
                        c.transform.scale, c.transform.tx, c.transform.ty
                    );
                }
            }
        }
    }
}
