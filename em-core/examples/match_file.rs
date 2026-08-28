//! 一致性校验 CLI：em-core 匹配一张截图，与 Python 原型输出对比。
//! 用法：cargo run --release --example match_file -- [--overlay 输出目录] <bundle目录> <截图...>
//!
//! --overlay 会把「截图 + 手绘层 + 门位」的合成图落盘：叠加错位是几何问题，
//! 光看分数看不出来，而真机往返一次要重打包，离线合成能把排查压到一次运行。

// 同 img.rs：像素级代码里显式下标比迭代器组合更贴近坐标语义
#![allow(clippy::needless_range_loop)]

use std::time::Instant;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut overlay_dir: Option<String> = None;
    if let Some(i) = args.iter().position(|a| a == "--overlay") {
        if i + 1 >= args.len() {
            eprintln!("--overlay 需要一个输出目录");
            std::process::exit(1);
        }
        overlay_dir = Some(args.remove(i + 1));
        args.remove(i);
    }
    if args.len() < 2 {
        eprintln!("用法: match_file [--overlay 输出目录] <bundle目录> <截图...>");
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
                let Some(dir) = &overlay_dir else { continue };
                let best = &candidates[0];
                let entry = lib
                    .entries
                    .iter()
                    .find(|e| e.variant == best.variant && e.floor == best.floor)
                    .unwrap();
                let ov = em_core::overlay_geometry(panel, best.transform, entry, w, h);
                println!(
                    "  显示区域={:?}  手绘尺度={:.4}  平移=({:.1},{:.1})",
                    ov.view, ov.tf.scale, ov.tf.tx, ov.tf.ty
                );
                let name = std::path::Path::new(path).file_stem().unwrap().to_string_lossy();
                let out = std::path::Path::new(dir).join(format!("{name}.overlay.png"));
                std::fs::create_dir_all(dir).expect("建目录失败");
                compose(&im, &ov, entry, panel, best.transform).save(&out).expect("写图失败");
                println!("  合成图 → {}", out.display());
            }
        }
    }
}

/// 把手绘层按 tf 叠到截图的显示区域上（黑底按亮度转 alpha，与前端 screen 混合等价），
/// 再画门位圆圈。生成的图应当与真机悬浮窗看到的一模一样。
///
/// 另叠一层绿色：参考掩码按 q_to_ref 投影回来。它只依赖匹配结果，
/// 手绘层还多依赖 tf_draw_to_game——两层分开画，错位是哪一环一眼可辨。
fn compose(
    frame: &image::RgbImage,
    ov: &em_core::Overlay,
    entry: &em_core::bundle::Entry,
    panel: [usize; 4],
    q_to_ref: em_core::Transform,
) -> image::RgbImage {
    let [vx, vy, vw, vh] = ov.view;
    let mut out = image::RgbImage::new(vw as u32, vh as u32);
    for y in 0..vh {
        for x in 0..vw {
            out.put_pixel(x as u32, y as u32, *frame.get_pixel((vx + x) as u32, (vy + y) as u32));
        }
    }
    // 绿层：显示区域局部 → 面板局部 → 参考掩码
    let (sx, sy) = (panel[0] as f64 - vx as f64, panel[1] as f64 - vy as f64);
    for y in 0..vh {
        for x in 0..vw {
            let rx = (x as f64 - sx) * q_to_ref.scale + q_to_ref.tx;
            let ry = (y as f64 - sy) * q_to_ref.scale + q_to_ref.ty;
            if rx < 0.0 || ry < 0.0 || rx >= entry.mask.w as f64 || ry >= entry.mask.h as f64 {
                continue;
            }
            // 只描边：填充会把底图糊住，看不出对没对准
            let (rxi, ryi) = (rx as usize, ry as usize);
            let v = entry.mask.at(rxi, ryi);
            let edge = [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)].iter().any(|&(dx, dy)| {
                let (nx, ny) = (rxi as i64 + dx * 2, ryi as i64 + dy * 2);
                nx < 0 || ny < 0 || nx >= entry.mask.w as i64 || ny >= entry.mask.h as i64
                    || entry.mask.at(nx as usize, ny as usize) != v
            });
            if edge {
                out.put_pixel(x as u32, y as u32, image::Rgb([40, 255, 80]));
            }
        }
    }
    let draw = image::open(&entry.draw_path).expect("读手绘图失败").to_rgb8();
    let (dw, dh) = (draw.width() as f64, draw.height() as f64);
    let (s, tx, ty) = (ov.tf.scale, ov.tf.tx, ov.tf.ty);
    for y in 0..vh {
        for x in 0..vw {
            // 显示区域局部 → 手绘图坐标（最近邻足够，这里只看几何对不对）
            let dx = (x as f64 - tx) / s;
            let dy = (y as f64 - ty) / s;
            if dx < 0.0 || dy < 0.0 || dx >= dw || dy >= dh {
                continue;
            }
            let p = draw.get_pixel(dx as u32, dy as u32).0;
            let luma = 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
            let a = (luma / 70.0).min(1.0) * 0.45;
            let q = out.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                q.0[c] = (q.0[c] as f64 * (1.0 - a) + p[c] as f64 * a).round() as u8;
            }
        }
    }
    for d in &ov.doors {
        let r = (vh as f64 * 0.014).max(8.0);
        for k in 0..360 {
            let t = k as f64 * std::f64::consts::PI / 180.0;
            let (px, py) = (d.x + r * t.cos(), d.y + r * t.sin());
            if px >= 0.0 && py >= 0.0 && (px as usize) < vw && (py as usize) < vh {
                out.put_pixel(px as u32, py as u32, image::Rgb([255, 80, 80]));
            }
        }
    }
    out
}
