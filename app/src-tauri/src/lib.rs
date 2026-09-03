// EmberMap Tauri 壳：抓屏 → em-core 匹配 → 前端 canvas 合成叠加。
//
// 桌面与 Android 共用此库：识别核心与命令是公共的，
// 抓屏（xcap）、覆盖窗、全局热键是桌面独有，用 cfg(desktop) 隔离；
// Android 的投屏取帧与悬浮窗需 Kotlin 插件，尚未接入。

use std::path::PathBuf;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Serialize;
use tauri::{Emitter, Manager, State};

mod tracker;

struct AppState {
    lib: Mutex<Option<em_core::Library>>,
    tracker: Mutex<tracker::Tracker>,
    /// 上一帧的尺寸：变化意味着屏幕旋转或窗口改尺寸，
    /// 此时地图大小随之改变，尺度先验必须作废
    last_frame: Mutex<(usize, usize)>,
    /// 数据包目录：发行版取应用资源目录，开发期取源码树 app/bundle
    bundle_dir: Mutex<PathBuf>,
    /// 用户手动锁定的变体/楼层。锁定后只跟这些参考比对，
    /// 主界面之类的画面就不可能再被认成别的地图，顺带快十几倍。
    pin: Mutex<PinState>,
    /// 上一次被采信的面板位置，用来拦「叠加层突然跳到别处」
    geom: Mutex<GeomGuard>,
}

/// 叠加层几何连续性守卫。
///
/// 地图关闭（或场景切换）的那一两帧，面板检测可能落到画面里完全另一处，
/// 分数却还勉强过得了门槛——那一帧会把覆盖窗整个甩走，用户看到的就是
/// 「关掉地图后叠加层在屏幕上跳几下」。游戏里的地图面板不会瞬移：
/// 平移、缩放都是连续的，前后两帧必然大幅交叠。
#[derive(Default)]
struct GeomGuard {
    /// 上一次采信的面板（帧像素）
    panel: Option<[usize; 4]>,
    /// 连续被拦下的帧数，到 GEOM_MAX_REJECT 就承认地图真的挪了这么远
    rejects: u32,
}

/// 新面板与上次采信的面板，交叠须占较小者的这个百分比
const GEOM_MIN_OVERLAP: usize = 20;
/// 连续拦这么多帧就放行，免得地图真挪远了从此再也跟不上
const GEOM_MAX_REJECT: u32 = 2;

/// 锁定变体后，是否采信本帧。
///
/// 锁定绕开了跟踪器的多帧投票——候选里只剩这一个变体，跨变体比分差无从谈起。
/// 但跟踪器同时还挡着另一类东西：**没有判别力的帧**。关掉大地图后 HUD 角落
/// 还留着个小地图，它太小，各参考分数挤在 0.845-0.857（实测分差 0.00-0.006），
/// 分数过得了门槛却毫无判别力。不锁定时 MIN_FRAME_ADVANTAGE 把它挡住了，
/// 锁定后这道闸不能跟着一起丢，否则就是「锁定了、地图关掉了、叠加层还在」。
///
/// 锁定后改用同一变体各楼层之间的分差：真地图会明确选中某一层，
/// 实测领先次佳楼层 0.137 与 0.264，留足了余量。
fn pin_confident(scores: &[f32], gate: f32) -> bool {
    let Some(&top) = scores.first() else { return false };
    if top < gate {
        return false;
    }
    // 连楼层也锁死时只剩一个候选，无从比较，只能看分数
    scores.get(1).is_none_or(|&next| top - next >= tracker::MIN_FRAME_ADVANTAGE)
}

/// 两个矩形的交叠面积占较小者的百分比
fn overlap_pct(a: [usize; 4], b: [usize; 4]) -> usize {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    if x1 <= x0 || y1 <= y0 {
        return 0;
    }
    let smaller = (a[2] * a[3]).min(b[2] * b[3]).max(1);
    (x1 - x0) * (y1 - y0) * 100 / smaller
}

/// 手动锁定状态。两项都为 None = 全自动。
#[derive(Default, Clone, Serialize, serde::Deserialize)]
struct PinState {
    /// 变体 id（bundle.json 里的 gm-xxxx），None = 自动判定
    variant: Option<String>,
    /// 楼层 id（b1 / 1f / 2f），None = 自动判定
    floor: Option<String>,
    /// 变体的中文名，仅供界面回显
    name: Option<String>,
}

#[derive(Serialize, serde::Deserialize)]
struct DoorOut {
    label: String,
    x: f64,
    y: f64,
}

#[derive(Serialize)]
struct CandidateOut {
    variant: String,
    name: String,
    floor: String,
    score: f32,
}

/// 分数高到这个地步只可能是「叠加层漏进了抓屏」——手绘层与参考掩码本就同源，
/// 会近乎完美自匹配。真地图实测 0.84-0.86，留足余量。
const SELF_CAPTURE_GATE: f32 = 0.97;

#[derive(Serialize)]
#[serde(tag = "status")]
enum Payload {
    /// 本轮不出结果。**不移动**叠加层——一动一动正是用户看到的跳。
    ///
    /// hold 决定它算不算「叠加层仍然有效」的凭据，这个区分是必须的：
    /// - true：画面与上一帧逐像素没变（Android 廉价采样），那上一帧的叠加当然还成立，
    ///   可以无限期保持，稳态零闪烁全靠它。
    /// - false：这一帧没能验证（疑似拍到叠加层自己）。不能拿它当凭据——
    ///   否则一旦持续出现，叠加层就会永远赖在屏幕上，地图早关了也不消失。
    ///   前端据此让保活计时继续走，超时就收起来。
    #[serde(rename = "skip")]
    Skip { reason: String, hold: bool },
    #[serde(rename = "no_panel")]
    NoPanel {
        reason: String,
        /// 实际分析的帧尺寸与缩略图：真机排障全靠它，
        /// 用户截一张图就能看出抓到的到底是什么画面
        frame_w: usize,
        frame_h: usize,
        frame_png: String,
    },
    #[serde(rename = "ok")]
    Ok {
        confident: bool,
        /// acquiring（多帧投票中）| locked（刚锁定）| tracking
        phase: String,
        /// 已累计证据 / 锁定所需证据，供 UI 显示识别进度
        evidence: f32,
        evidence_need: f32,
        /// 本帧与次佳变体的分差：越大越可信（姊妹变体常只差 0.03）
        advantage: f32,
        /// 变体 id，前端「锁定」按钮据此下钉
        variant: String,
        name: String,
        floor: String,
        score: f32,
        candidates: Vec<CandidateOut>,
        /// 面板裁剪截图 PNG（base64）
        shot_png: String,
        /// 匹配变体该层的手绘图 PNG（base64，桌面前端 canvas 用）
        draw_png: String,
        /// 同一张手绘图在磁盘上的路径（Android 悬浮窗直接读，免 base64 过桥）
        draw_path: String,
        /// 手绘图 → 显示区域坐标的相似变换
        tf: em_core::Transform,
        doors: Vec<DoorOut>,
        /// 整层显示区域（屏幕物理坐标，x/y 可为负——游戏窗口可能部分在屏幕外），
        /// 覆盖窗按它定位定尺寸
        view: [i32; 4],
    },
}

/// 解析数据包目录：Android 取 MainActivity 解压出的数据目录，
/// 桌面优先应用资源目录（发行版），最后回退源码树（开发期）。
fn resolve_bundle_dir(app: &tauri::AppHandle) -> PathBuf {
    // Android 的 resource_dir() 是 asset:// URI，std::fs 读不了；
    // MainActivity.kt 启动时已把 assets/bundle 解压到 dataDir/bundle
    #[cfg(target_os = "android")]
    if let Ok(d) = app.path().app_data_dir() {
        let p = d.join("bundle");
        if p.join("bundle.json").exists() {
            return p;
        }
    }
    if let Ok(p) = app.path().resolve("bundle", tauri::path::BaseDirectory::Resource) {
        if p.join("bundle.json").exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bundle")
}

fn ensure_lib(state: &AppState) -> Result<(), String> {
    let mut g = state.lib.lock().unwrap();
    if g.is_none() {
        let dir = state.bundle_dir.lock().unwrap().clone();
        *g = Some(em_core::load_library(&dir)?);
    }
    Ok(())
}

/// 整帧缩略图（base64 PNG），仅用于排障显示
fn thumbnail(rgb: &[u8], w: usize, h: usize, long_edge: usize) -> Result<String, String> {
    let f = (w.max(h) / long_edge).max(1);
    let (tw, th) = (w / f, h / f);
    let mut small = vec![0u8; tw * th * 3];
    for y in 0..th {
        for x in 0..tw {
            let src = ((y * f) * w + x * f) * 3;
            let dst = (y * tw + x) * 3;
            small[dst..dst + 3].copy_from_slice(&rgb[src..src + 3]);
        }
    }
    png_b64(&small, tw as u32, th as u32)
}

fn png_b64(rgb: &[u8], w: u32, h: u32) -> Result<String, String> {
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(B64.encode(buf))
}

use image::ImageEncoder;

fn run_analysis(rgb: Vec<u8>, w: usize, h: usize, state: &AppState) -> Result<Payload, String> {
    ensure_lib(state)?;
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    // 始终全库扫描：尺度先验只收窄尺度搜索，不过滤候选，误判才能自我纠正。
    // 帧尺寸变了（旋转/改窗口）：地图随之缩放，沿用旧尺度先验会把搜索窗口带偏，
    // 实测表现为旋转后分数掉到 0.69、分差 0.001，直到超时解锁才恢复。
    {
        let mut lf = state.last_frame.lock().unwrap();
        if *lf != (w, h) {
            if *lf != (0, 0) {
                eprintln!("[em] 帧尺寸 {:?} → {:?}，重置跟踪状态", *lf, (w, h));
                state.tracker.lock().unwrap().reset();
            }
            *lf = (w, h);
        }
    }
    let prior = state.tracker.lock().unwrap().prior_scale_full;
    // 只有下面的 cfg(mobile) 分支会改它，桌面编译时看起来是多余的 mut
    #[allow(unused_mut)]
    let mut opts = match prior {
        Some(s) => em_core::Options::track(s),
        None => em_core::Options::acquire(),
    };
    // 移动端算力有限，用更轻的档位：分析分辨率减半、精修分辨率与候选数下调。
    // 注意取帧分辨率不能跟着降——实测取帧缩到 1200 会让重采样与 JPEG 损失
    // 把分数压到门槛下（0.73），而高保真取帧 + 低分辨率分析仍有 0.84。
    // 主要耗时在匹配而非掩码提取，故这里同时收窄精修。
    #[cfg(mobile)]
    {
        opts.target_long_edge = 1000.0;
        opts.match_opts.refine_long_edge = 380;
        opts.match_opts.refine_top = if prior.is_some() { 3 } else { 6 };
    }
    // 自检：投屏管线没跟上屏幕旋转时，抓到的帧会几乎全黑（内容被压到一角）。
    // 单说「未检测到地图面板」会把用户引向错误方向，故单独报出来。
    let dark = rgb.chunks_exact(3).filter(|p| p[0] < 24 && p[1] < 24 && p[2] < 24).count();
    if dark * 100 / (w * h).max(1) >= 92 {
        return Ok(Payload::NoPanel {
            reason: "抓到的画面几乎全黑，投屏可能未跟上屏幕旋转；请关掉再重新授权投屏".into(),
            frame_w: w,
            frame_h: h,
            frame_png: thumbnail(&rgb, w, h, 720)?,
        });
    }
    let pin_state = state.pin.lock().unwrap().clone();
    let pin = em_core::Pin {
        variant: pin_state.variant.as_deref(),
        floor: pin_state.floor.as_deref(),
    };
    let t0 = std::time::Instant::now();
    let analysis = em_core::analyze_with(&rgb, w, h, lib, pin, &opts);
    let dt = t0.elapsed();
    match analysis {
        em_core::Analysis::NoPanel { reason } => {
            let forgot = state.tracker.lock().unwrap().on_no_panel();
            *state.geom.lock().unwrap() = GeomGuard::default();
            eprintln!("[em] {dt:?} 无面板{}：{reason}", if forgot { "(证据已清空)" } else { "(暂停)" });
            Ok(Payload::NoPanel {
                reason,
                frame_w: w,
                frame_h: h,
                frame_png: thumbnail(&rgb, w, h, 720)?,
            })
        }
        em_core::Analysis::Matched { panel, candidates, .. } => {
            // 自检要赶在喂跟踪器之前：污染帧的尺度先验会把后续几帧一起带偏。
            // 跳过一帧的代价可以忽略，前端保持上一帧的叠加，用户毫无察觉。
            if candidates.first().is_some_and(|c| c.score > SELF_CAPTURE_GATE) {
                let c = &candidates[0];
                eprintln!(
                    "[em] {dt:?} 警告：分数 {:.3} 异常高，疑似把叠加层自己拍了进来（{} {}）。\
                     丢弃该帧；若持续出现，前端会按保活超时收起叠加层",
                    c.score, c.name, c.floor
                );
                return Ok(Payload::Skip {
                    reason: format!("疑似抓到叠加层自身（{:.3}）", c.score),
                    hold: false,
                });
            }
            // 钉了变体就绕开投票/粘滞：候选里只剩这一个变体，「与次佳变体比分差」
            // 无从谈起，多帧印证也没有意义——用户已经替它作了保。
            let (best_i, confident, phase, evidence, advantage) = if pin_state.variant.is_some() {
                let scores: Vec<f32> = candidates.iter().map(|c| c.score).collect();
                let ok = pin_confident(&scores, em_core::CONFIDENCE_GATE);
                let adv = scores.first().copied().unwrap_or(0.0)
                    - scores.get(1).copied().unwrap_or(0.0);
                let mut tk = state.tracker.lock().unwrap();
                tk.prior_scale_full = ok.then_some(candidates[0].scale_full);
                (0usize, ok, "pinned".to_string(), 0.0, adv)
            } else {
                let flat: Vec<(String, f32)> =
                    candidates.iter().map(|c| (c.variant.clone(), c.score)).collect();
                let mut tk = state.tracker.lock().unwrap();
                let dec = tk.update(&flat, em_core::CONFIDENCE_GATE);
                // 采纳变体的最佳楼层；未采纳时用本帧第一名（仅供参考显示）
                let i = match &dec.variant {
                    Some(v) => candidates.iter().position(|c| &c.variant == v).unwrap(),
                    None => 0,
                };
                if dec.variant.is_some() {
                    tk.prior_scale_full = Some(candidates[i].scale_full);
                }
                // 粘滞缓冲帧：变体还报得出，但分数已掉出门槛、几何不可信。
                // 降为「不置信」而不是 Skip——不置信只是不移动叠加层，
                // 同时照常计入丢失，连续两帧就收起来。若在这里 Skip，
                // 地图关掉后叠加层会一直赖在屏幕上（上一版正是这么错的）。
                if dec.stale {
                    eprintln!("[em] {dt:?} 分数掉出门槛，本帧几何不可信，按未识别处理");
                }
                let ok = dec.variant.is_some() && !dec.stale;
                (i, ok, dec.phase.to_string(), dec.evidence, dec.advantage)
            };
            let best = &candidates[best_i];
            // 几何连续性：面板不会瞬移。与上次采信的面板交叠太少就先不动叠加层，
            // 连续两帧都对不上才承认地图真挪了这么远。地图关闭那一两帧的误检
            // 往往落在画面别处，正是这道闸要拦的。
            let mut confident = confident;
            {
                let mut gg = state.geom.lock().unwrap();
                if !confident {
                    *gg = GeomGuard::default();
                } else if let Some(prev) = gg.panel {
                    let ov = overlap_pct(prev, panel);
                    if ov < GEOM_MIN_OVERLAP && gg.rejects < GEOM_MAX_REJECT {
                        gg.rejects += 1;
                        eprintln!(
                            "[em] {dt:?} 面板从 {prev:?} 跳到 {panel:?}（交叠 {ov}%），\
                             本帧按未识别处理（第 {} 次）",
                            gg.rejects
                        );
                        // 同 stale：降为不置信而非 Skip。不移动叠加层，但照常计入丢失，
                        // 免得误检持续时叠加层永远收不起来。
                        confident = false;
                    } else {
                        gg.rejects = 0;
                        gg.panel = Some(panel);
                    }
                } else {
                    gg.panel = Some(panel);
                }
            }
            // 一帧对全库毫无判别力时（13 个变体挤在 0.806-0.810 里，实测就是这样），
            // 报出「当前最像 XX 0.81」在用户看来就是认错了地图。这种帧本来也锁不了
            // （tracker 要求 MIN_FRAME_ADVANTAGE），如实说没认出来更好。
            if !confident
                && pin_state.variant.is_none()
                && advantage < tracker::MIN_FRAME_ADVANTAGE
            {
                eprintln!("[em] {dt:?} 本帧无判别力（分差 {advantage:.3}），按无地图处理");
                state.tracker.lock().unwrap().on_no_panel();
                return Ok(Payload::NoPanel {
                    // 探索得太少时，Y 形路口一段走廊哪张图都对得上，自动识别无从下手。
                    // 与其反复「识别中」，不如指条明路：多探索些，或直接手动选地图。
                    reason: format!(
                        "探索出来的部分太少，还认不出是哪张（各变体只差 {advantage:.3}）；\
                         多探索一点，或用上方「锁定地图」手动选"
                    ),
                    frame_w: w,
                    frame_h: h,
                    frame_png: thumbnail(&rgb, w, h, 720)?,
                });
            }
            eprintln!(
                "[em] {dt:?} {phase} 证据={evidence:.3} 分差={advantage:.3} 面板={panel:?} \
                 先验={prior:?} | {}",
                candidates
                    .iter()
                    .take(3)
                    .map(|c| format!("{} {} {:.3}", c.name, c.floor, c.score))
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
            let entry = lib
                .entries
                .iter()
                .find(|e| e.variant == best.variant && e.floor == best.floor)
                .unwrap();
            // 显示区域 / 变换 / 门位由 em-core 统一推导，与离线校验工具同源
            let ov = em_core::overlay_geometry(panel, best.transform, entry, w, h);
            let [x, y, pw, ph] = ov.view;
            let mut crop = vec![0u8; pw * ph * 3];
            for row in 0..ph {
                let src = ((y + row) * w + x) * 3;
                crop[row * pw * 3..(row + 1) * pw * 3]
                    .copy_from_slice(&rgb[src..src + pw * 3]);
            }
            let tf = ov.tf;
            let doors: Vec<DoorOut> = ov
                .doors
                .iter()
                .map(|d| DoorOut { label: d.label.clone(), x: d.x, y: d.y })
                .collect();
            let draw_png = B64.encode(
                std::fs::read(&entry.draw_path).map_err(|e| e.to_string())?,
            );
            // EM_DUMP=1：把匹配器看到的面板裁剪图落盘，便于排查误判
            if std::env::var("EM_DUMP").is_ok() {
                let _ = image::save_buffer(
                    "/tmp/em_panel.png", &crop, pw as u32, ph as u32,
                    image::ExtendedColorType::Rgb8,
                );
            }
            Ok(Payload::Ok {
                confident,
                phase,
                evidence,
                evidence_need: tracker::LOCK_ADVANTAGE,
                advantage,
                variant: entry.variant.clone(),
                name: entry.name.clone(),
                floor: entry.floor.clone(),
                score: best.score,
                candidates: candidates
                    .iter()
                    .take(3)
                    .map(|c| CandidateOut {
                        variant: c.variant.clone(),
                        name: c.name.clone(),
                        floor: c.floor.clone(),
                        score: c.score,
                    })
                    .collect(),
                shot_png: png_b64(&crop, pw as u32, ph as u32)?,
                draw_png,
                draw_path: entry.draw_path.to_string_lossy().into_owned(),
                tf,
                doors,
                // 交给前端定位覆盖窗的是「整层显示区域」，不是已探索面板
                view: [x as i32, y as i32, pw as i32, ph as i32],
            })
        }
    }
}

/// 游戏窗口标识（Wine 下进程名为 dwrg.exe）
#[cfg(desktop)]
const GAME_HINTS: [&str; 4] = ["第五人格", "identityv", "dwrg", "wine"];

#[cfg(desktop)]
fn rgba_to_rgb(rgba: Vec<u8>, n: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; n * 3];
    for i in 0..n {
        rgb[i * 3] = rgba[i * 4];
        rgb[i * 3 + 1] = rgba[i * 4 + 1];
        rgb[i * 3 + 2] = rgba[i * 4 + 2];
    }
    rgb
}

/// 优先抓游戏窗口本身：与前台无关（我们的窗口挡住也没事），
/// 且覆盖层不可能被自己抓进去。找不到游戏窗口时退回抓主屏。
/// 返回 (rgb, w, h, 该图左上角在屏幕上的物理坐标)。
#[cfg(desktop)]
fn capture_target() -> Result<(Vec<u8>, usize, usize, i32, i32), String> {
    // EM_FORCE_MONITOR=1：跳过找游戏窗口，直接抓主屏。
    // 排障用——把一张游戏截图摆到屏幕上就能复现整条链路，不必真的开着游戏。
    if std::env::var("EM_FORCE_MONITOR").is_ok() {
        eprintln!("[em] EM_FORCE_MONITOR=1，直接抓主屏");
    } else if let Ok(windows) = xcap::Window::all() {
        let game = windows.into_iter().find(|w| {
            let name = format!(
                "{} {}",
                w.app_name().unwrap_or_default(),
                w.title().unwrap_or_default()
            )
            .to_lowercase();
            let big = w.width().unwrap_or(0) > 400 && w.height().unwrap_or(0) > 300;
            let visible = !w.is_minimized().unwrap_or(false);
            big && visible && GAME_HINTS.iter().any(|h| name.contains(h))
        });
        if let Some(win) = game {
            if let Ok(img) = win.capture_image() {
                eprintln!(
                    "[em] 抓取游戏窗口「{} / {}」窗口逻辑={}x{}@{},{} 抓到物理={}x{}",
                    win.app_name().unwrap_or_default(),
                    win.title().unwrap_or_default(),
                    win.width().unwrap_or(0), win.height().unwrap_or(0),
                    win.x().unwrap_or(0), win.y().unwrap_or(0),
                    img.width(), img.height()
                );
                let (w, h) = (img.width() as usize, img.height() as usize);
                // 窗口坐标是逻辑像素，抓到的图是物理像素；按比例换算原点
                let sx = w as f64 / win.width().unwrap_or(w as u32).max(1) as f64;
                let sy = h as f64 / win.height().unwrap_or(h as u32).max(1) as f64;
                let ox = (win.x().unwrap_or(0) as f64 * sx).round() as i32;
                let oy = (win.y().unwrap_or(0) as f64 * sy).round() as i32;
                return Ok((rgba_to_rgb(img.into_raw(), w * h), w, h, ox, oy));
            }
        }
    }
    // 找不到游戏窗口只能退回抓主屏。抓窗口时叠加层不可能进画面（CGWindowList
    // 只合成那一个窗口）；抓主屏就全指望 content_protected 把它挡在外面了，
    // 一旦挡不住就会自己识别自己，故单独记一笔便于排障。
    eprintln!("[em] 未找到游戏窗口，退回抓主屏");
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let mon = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or("没有可用显示器")?;
    let img = mon.capture_image().map_err(|e| {
        format!("抓屏失败：{e}。请在 系统设置→隐私与安全性→屏幕录制 给本应用授权")
    })?;
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok((rgba_to_rgb(img.into_raw(), w * h), w, h, 0, 0))
}

// 注意：重活必须放 spawn_blocking——同步命令跑在主线程上会把 UI 冻住
#[cfg(desktop)]
#[tauri::command]
async fn analyze_screen(app: tauri::AppHandle, force: bool) -> Result<Payload, String> {
    let _ = force; // 桌面端覆盖窗对抓屏不可见，无需跳帧策略
    tauri::async_runtime::spawn_blocking(move || {
        let (rgb, w, h, ox, oy) = capture_target()?;
        let mut payload = run_analysis(rgb, w, h, &app.state::<AppState>())?;
        // 换算到屏幕物理坐标供覆盖窗定位；不可钳到 0，游戏窗口可能在屏幕外
        if let Payload::Ok { view, .. } = &mut payload {
            view[0] += ox;
            view[1] += oy;
            eprintln!("[em] 整层显示区域(屏幕物理)={view:?} 窗口原点=({ox},{oy})");
        }
        Ok(payload)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn analyze_file(path: String, app: tauri::AppHandle) -> Result<Payload, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let im = image::open(&path).map_err(|e| e.to_string())?.to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        run_analysis(im.into_raw(), w, h, &app.state::<AppState>())
    })

    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// Android 取帧：经 Kotlin 插件走 MediaProjection
// ---------------------------------------------------------------------------

#[cfg(target_os = "android")]
mod android_capture {
    use serde::Deserialize;
    use tauri::plugin::PluginHandle;

    #[derive(Deserialize)]
    pub struct Granted {
        pub granted: bool,
        pub reason: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct Frame {
        pub path: String,
        /// Kotlin 侧缩放系数（截图 px = 屏幕物理 px × scale）
        pub scale: f64,
    }

    #[derive(Deserialize)]
    pub struct Capturing {
        pub capturing: bool,
    }

    #[derive(Deserialize)]
    pub struct Peek {
        pub changed: bool,
        /// 采样点里「明显变了」的占比，调阈值时看它
        pub ratio: f64,
    }

    pub fn request(h: &PluginHandle<tauri::Wry>) -> Result<Granted, String> {
        h.run_mobile_plugin("requestCapture", ()).map_err(|e| e.to_string())
    }
    pub fn stop(h: &PluginHandle<tauri::Wry>) -> Result<(), String> {
        h.run_mobile_plugin::<serde_json::Value>("stopCapture", ())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn is_capturing(h: &PluginHandle<tauri::Wry>) -> Result<Capturing, String> {
        h.run_mobile_plugin("isCapturing", ()).map_err(|e| e.to_string())
    }
    pub fn grab(h: &PluginHandle<tauri::Wry>) -> Result<Frame, String> {
        h.run_mobile_plugin("grabFrame", ()).map_err(|e| e.to_string())
    }
    pub fn peek(h: &PluginHandle<tauri::Wry>) -> Result<Peek, String> {
        h.run_mobile_plugin("peekChanged", ()).map_err(|e| e.to_string())
    }
}

/// Android：拉起系统投屏授权（用户点「立即开始」后才可取帧）
#[cfg(target_os = "android")]
#[tauri::command]
async fn request_capture(app: tauri::AppHandle) -> Result<bool, String> {
    let h = app.state::<AndroidPlugin>().0.clone();
    let r = android_capture::request(&h)?;
    if !r.granted {
        return Err(r.reason.unwrap_or_else(|| "投屏授权未通过".into()));
    }
    Ok(true)
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn stop_capture(app: tauri::AppHandle) -> Result<(), String> {
    android_capture::stop(&app.state::<AndroidPlugin>().0)
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn capture_active(app: tauri::AppHandle) -> Result<bool, String> {
    Ok(android_capture::is_capturing(&app.state::<AndroidPlugin>().0)?.capturing)
}

/// Android 版 analyze_screen：取帧 → 识别。坐标换算回屏幕物理像素，
/// 供悬浮窗定位（Kotlin 侧按 maxLongEdge 缩过，故需除以 scale）。
#[cfg(target_os = "android")]
#[tauri::command]
async fn analyze_screen(app: tauri::AppHandle, force: bool) -> Result<Payload, String> {
    let frame = {
        let h = app.state::<AndroidPlugin>().0.clone();
        // 先廉价探一下画面动没动：取干净帧要瞬时隐藏悬浮窗，那一下用户看得见。
        // 地图开着不动时（正是在看叠加层的时刻）没有任何重算的必要。
        // force：用户手动点「抓屏匹配」，那就必须真的抓一次，否则按钮像坏了。
        let peek = if force { None } else { Some(android_capture::peek(&h)?) };
        if peek.as_ref().is_some_and(|p| !p.changed) {
            let peek = peek.unwrap();
            return Ok(Payload::Skip {
                reason: format!("画面未变（差异 {:.2}%）", peek.ratio * 100.0),
                // 画面逐像素没变 ⇒ 上一帧的叠加当然还成立，可以无限期保持
                hold: true,
            });
        }
        android_capture::grab(&h)?
    };
    tauri::async_runtime::spawn_blocking(move || {
        let im = image::open(&frame.path).map_err(|e| e.to_string())?.to_rgb8();
        let (w, h) = (im.width() as usize, im.height() as usize);
        let mut payload = run_analysis(im.into_raw(), w, h, &app.state::<AppState>())?;
        // run_analysis 的输出全在「截图像素」坐标系里，而悬浮窗画在屏幕物理像素上。
        // 只换算 view 是不够的：tf 与门位也在同一坐标系，漏掉它们等于
        // 窗口摆对了、里面的图却按截图尺度画——实测长边 2412 的屏幕被缩到 2000，
        // 叠加层就整体小 17% 并朝窗口左上角偏，表现为「覆盖和缩放都不对」。
        if let Payload::Ok { view, tf, doors, .. } = &mut payload {
            let inv = 1.0 / frame.scale.max(1e-6);
            for v in view.iter_mut() {
                *v = (*v as f64 * inv).round() as i32;
            }
            tf.scale *= inv;
            tf.tx *= inv;
            tf.ty *= inv;
            for d in doors.iter_mut() {
                d.x *= inv;
                d.y *= inv;
            }
            eprintln!(
                "[em] 帧→屏幕 ×{inv:.4} 显示区域={view:?} 手绘尺度={:.4} 平移=({:.1},{:.1})",
                tf.scale, tf.tx, tf.ty
            );
        }
        Ok(payload)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "android")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlayArgs {
    draw_path: String,
    scale: f64,
    tx: f64,
    ty: f64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    alpha: f64,
    doors: Vec<DoorOut>,
}

/// Android 悬浮窗：内容由 Kotlin 的 OverlayView 绘制（中文标签用系统字体最省事）
#[cfg(target_os = "android")]
#[tauri::command]
async fn overlay_update(
    app: tauri::AppHandle,
    draw_path: String,
    tf: serde_json::Value,
    doors: Vec<DoorOut>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    alpha: f64,
) -> Result<(), String> {
    let args = OverlayArgs {
        draw_path,
        scale: tf["scale"].as_f64().unwrap_or(1.0),
        tx: tf["tx"].as_f64().unwrap_or(0.0),
        ty: tf["ty"].as_f64().unwrap_or(0.0),
        x: x as i32,
        y: y as i32,
        w: w as i32,
        h: h as i32,
        alpha,
        doors,
    };
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin::<serde_json::Value>("showOverlay", args)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Android 控制悬浮窗的一次性全量状态。手机上没有键盘，
/// 桌面端那套全局热键在这里等价于这个窗口上的几个按钮。
#[cfg(target_os = "android")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlArgs {
    show: bool,
    status: String,
    overlay_on: bool,
    map_on: bool,
    busy: bool,
    floor: Option<String>,
    map_path: Option<String>,
    map_doors: Vec<DoorOut>,
}

/// 一次带上全部状态而不是拆成几个命令：控制条上的按钮态、状态行、
/// 整层图必须同时刷新，分开发容易出现「按钮亮了但图还没换」的中间态。
#[cfg(target_os = "android")]
#[tauri::command]
async fn control_update(app: tauri::AppHandle, args: ControlArgs) -> Result<(), String> {
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin::<serde_json::Value>("showControl", args)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取回控制悬浮窗上攒下的按钮事件。
/// 走轮询而不是推送：Kotlin 的 trigger 需要 JS 侧 addPluginListener，
/// 而那条命令被 Tauri 的 ACL 拦下（应用内联插件没有权限清单可放行）。
#[cfg(target_os = "android")]
#[tauri::command]
async fn poll_control(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin("pollControl", ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn overlay_hide(app: tauri::AppHandle) -> Result<(), String> {
    app.state::<AndroidPlugin>()
        .0
        .run_mobile_plugin::<serde_json::Value>("hideOverlay", ())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn request_overlay_permission(app: tauri::AppHandle) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    struct Granted { granted: bool }
    let r: Granted = app
        .state::<AndroidPlugin>()
        .0
        .run_mobile_plugin("requestOverlayPermission", ())
        .map_err(|e| e.to_string())?;
    Ok(r.granted)
}

/// 持有 Kotlin 插件句柄
#[cfg(target_os = "android")]
struct AndroidPlugin(tauri::plugin::PluginHandle<tauri::Wry>);

/// 前端据此决定显示哪些功能，并回显版本号——
/// 测试构建的版本号带时间戳（0.1.0-nightly.YYYYMMDDHHMM），
/// 界面上显示出来才分得清手里装的到底是哪一次编出来的。
#[tauri::command]
fn capabilities(app: tauri::AppHandle) -> serde_json::Value {
    serde_json::json!({
        "version": app.package_info().version.to_string(),
        // Android 经 MediaProjection 取帧，但需用户先授权
        "screen_capture": true,
        "needs_capture_permission": cfg!(target_os = "android"),
        "overlay": true,
        "hotkeys": cfg!(desktop),
    })
}

/// 前端把关键判断点打到 stderr，与后端日志汇到同一条时间线上。
/// WebView 的 console 在发行版里看不见，排障时缺这一段就只能靠猜。
#[tauri::command]
fn log_line(msg: String) {
    eprintln!("[em/ui] {msg}");
}

/// 清除锁定与投票，下一帧从零开始高质量识别（换局/怀疑锁错时用）
#[tauri::command]
fn reset_lock(state: State<AppState>) {
    state.tracker.lock().unwrap().reset();
}

/// 手动锁定变体/楼层。传 null 即解除对应那一项。
///
/// 锁定后匹配只在这个范围里做：主界面之类的画面不可能再被认成别的地图，
/// 而且候选从 39 条降到 3 条（或 1 条），单帧耗时也随之下来。
#[tauri::command]
fn set_pin(
    state: State<AppState>,
    variant: Option<String>,
    floor: Option<String>,
    name: Option<String>,
) -> PinState {
    let p = PinState { variant, floor, name };
    eprintln!("[em] 手动锁定：变体={:?} 楼层={:?}", p.variant, p.floor);
    // 范围变了，旧的投票与尺度先验都不再适用
    state.tracker.lock().unwrap().reset();
    *state.pin.lock().unwrap() = p.clone();
    p
}

#[tauri::command]
fn get_pin(state: State<AppState>) -> PinState {
    state.pin.lock().unwrap().clone()
}

/// 单层完整手绘图，供「看整层」使用。
/// 与叠加无关：不需要对准游戏画面，就是把这一层原样摊开给用户看，
/// 解决「地图放大了看不到其它区域」和「想先看看另一层长什么样」。
#[derive(Serialize)]
struct FloorMapOut {
    variant: String,
    name: String,
    floor: String,
    draw_png: String,
    /// 同一张图在磁盘上的路径：Android 控制悬浮窗由 Kotlin 直接读，免 base64 过桥
    draw_path: String,
    /// 门位，已换算到手绘图自身的像素坐标
    doors: Vec<DoorOut>,
    w: u32,
    h: u32,
}

#[tauri::command]
fn floor_map(state: State<AppState>, variant: String, floor: String) -> Result<FloorMapOut, String> {
    ensure_lib(&state)?;
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    let e = lib
        .entries
        .iter()
        .find(|e| e.variant == variant && e.floor == floor)
        .ok_or_else(|| format!("参考库里没有 {variant} 的 {floor}"))?;
    let bytes = std::fs::read(&e.draw_path).map_err(|x| x.to_string())?;
    let (w, h) = image::image_dimensions(&e.draw_path).map_err(|x| x.to_string())?;
    // 门位存的是参考图坐标；这里要的是手绘图自身坐标，反解 tf_draw_to_game：
    // ref = d·a.scale + a.t  ⇒  d = (ref − a.t)/a.scale
    let a = e.tf_draw_to_game;
    let doors = e
        .doors
        .iter()
        .map(|d| DoorOut {
            label: d.label.clone(),
            x: (d.x - a.tx) / a.scale,
            y: (d.y - a.ty) / a.scale,
        })
        .collect();
    Ok(FloorMapOut {
        variant,
        name: e.name.clone(),
        floor,
        draw_png: B64.encode(bytes),
        draw_path: e.draw_path.to_string_lossy().into_owned(),
        doors,
        w,
        h,
    })
}

/// 参考库里的全部变体与楼层，供界面做手动选择
#[tauri::command]
fn list_maps(state: State<AppState>) -> Result<serde_json::Value, String> {
    ensure_lib(&state)?;
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for e in &lib.entries {
        if !out.iter().any(|v| v["variant"] == e.variant.as_str()) {
            out.push(serde_json::json!({ "variant": e.variant, "name": e.name }));
        }
    }
    Ok(serde_json::json!(out))
}

/// 覆盖窗口：透明/无边框/置顶/点击穿透/防捕获，按面板物理像素坐标摆放。
/// content_protected 使其对抓屏不可见，持续监测不会被自己的叠加污染。
#[cfg(desktop)]
#[tauri::command]
fn overlay_update(
    app: tauri::AppHandle,
    payload: serde_json::Value,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    use tauri::{Emitter, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};
    let win = match app.get_webview_window("overlay") {
        Some(w) => w,
        None => {
            let w = WebviewWindowBuilder::new(&app, "overlay", WebviewUrl::App("overlay.html".into()))
                .transparent(true)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .shadow(false)
                .focused(false)
                .visible(false)
                .build()
                .map_err(|e| {
                    eprintln!("[em] 覆盖窗创建失败：{e}");
                    e.to_string()
                })?;
            if let Err(e) = w.set_ignore_cursor_events(true) {
                eprintln!("[em] 覆盖窗设置点击穿透失败（不影响显示）：{e}");
            }
            if let Err(e) = w.set_content_protected(true) {
                eprintln!("[em] 覆盖窗设置防抓屏失败（不影响显示）：{e}");
            }
            eprintln!("[em] 覆盖窗已创建");
            w
        }
    };
    win.set_position(PhysicalPosition::new(x, y)).map_err(|e| e.to_string())?;
    win.set_size(PhysicalSize::new(w, h)).map_err(|e| e.to_string())?;
    win.show().map_err(|e| e.to_string())?;
    eprintln!("[em] 覆盖窗已摆放 {w}x{h}@{x},{y} 可见={:?}", win.is_visible());
    // show 之后再发数据，overlay.html 首次加载时监听器可能尚未就绪，
    // 前端带重试（首帧由 overlay-ready 事件拉取）
    win.emit("overlay-data", payload).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn overlay_hide(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
}


/// 全局热键设置。字符串用 tauri 的加速键写法：修饰键在前、键码在后，
/// 例如 "Shift+Alt+KeyM"。键码名与浏览器 KeyboardEvent.code 一致，
/// 前端因此可以直接把用户按下的组合拼成这个字符串。
///
/// serde(default)：老的 hotkeys.json 里没有后加的字段，逐字段回落到默认值，
/// 而不是整份配置解析失败、把用户已经改好的键一并丢掉。
#[cfg(desktop)]
#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(default)]
struct Hotkeys {
    toggle_overlay: String,
    reset_lock: String,
    capture_now: String,
}

#[cfg(desktop)]
impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            toggle_overlay: "Shift+Alt+KeyM".into(),
            reset_lock: "Shift+Alt+KeyR".into(),
            capture_now: "Shift+Alt+KeyC".into(),
        }
    }
}

#[cfg(desktop)]
fn hotkeys_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("hotkeys.json"))
}

#[cfg(desktop)]
fn load_hotkeys(app: &tauri::AppHandle) -> Hotkeys {
    hotkeys_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 全局热键：即使焦点在游戏里也能用。前端收到 hotkey 事件后执行对应动作。
/// 每次改键都整体重注册——先全解绑再绑新的，避免旧组合残留。
#[cfg(desktop)]
fn apply_hotkeys(app: &tauri::AppHandle, hk: &Hotkeys) -> Result<(), String> {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    let parse = |s: &str, what: &str| {
        Shortcut::from_str(s).map_err(|e| format!("{what}「{s}」无法识别：{e}"))
    };
    let toggle = parse(&hk.toggle_overlay, "开关叠加层的热键")?;
    let redo = parse(&hk.reset_lock, "重新识别的热键")?;
    let shoot = parse(&hk.capture_now, "抓屏匹配的热键")?;
    if toggle == redo || toggle == shoot || redo == shoot {
        return Err("三个热键不能有重复的组合".into());
    }

    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let handle = app.clone();
    gs.on_shortcuts([toggle, redo, shoot], move |_app, sc, ev| {
        if ev.state() != ShortcutState::Pressed {
            return;
        }
        let action = if sc == &toggle {
            "toggle_overlay"
        } else if sc == &redo {
            "reset_lock"
        } else if sc == &shoot {
            "capture_now"
        } else {
            return;
        };
        let _ = handle.emit("hotkey", action);
    })
    .map_err(|e| format!("注册热键失败（可能已被别的程序占用）：{e}"))?;
    eprintln!(
        "[em] 全局热键：{} 开关叠加层，{} 重新识别，{} 抓屏匹配",
        hk.toggle_overlay, hk.reset_lock, hk.capture_now
    );
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn get_hotkeys(app: tauri::AppHandle) -> Hotkeys {
    load_hotkeys(&app)
}

/// 改键：先试着注册，成功了才落盘——注册失败时旧的仍然有效，不会两头落空
#[cfg(desktop)]
#[tauri::command]
fn set_hotkeys(
    app: tauri::AppHandle,
    toggle_overlay: String,
    reset_lock: String,
    capture_now: String,
) -> Result<Hotkeys, String> {
    let hk = Hotkeys { toggle_overlay, reset_lock, capture_now };
    if let Err(e) = apply_hotkeys(&app, &hk) {
        // 回滚到原来的设置，别让用户既丢了新键也丢了旧键
        let _ = apply_hotkeys(&app, &load_hotkeys(&app));
        return Err(e);
    }
    if let Some(p) = hotkeys_path(&app) {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&hk).unwrap_or_default());
    }
    Ok(hk)
}

/// 从参考库自身合成一个「部分探索」查询并匹配，验证核心可用。
/// 与 em-core/tests/synth_regression.rs 同思路，此处用于目标平台实机自检。
#[cfg(mobile)]
fn self_check(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if let Err(e) = ensure_lib(&state) {
        eprintln!("[em] 自检失败：加载数据包出错：{e}");
        return;
    }
    let g = state.lib.lock().unwrap();
    let lib = g.as_ref().unwrap();
    eprintln!("[em] 自检：参考库 {} 个楼层条目", lib.entries.len());

    let want = &lib.entries[0];
    let m = &want.mask;
    let sub = m.crop(m.w / 8, m.h / 8, m.w * 3 / 4, m.h * 3 / 4);
    let q = em_core::img::resize_area_binary(&sub, (sub.w * 4) / 5, (sub.h * 4) / 5);
    let masks: Vec<&em_core::img::Gray> = lib.entries.iter().map(|e| &e.mask).collect();
    let t0 = std::time::Instant::now();
    let res = em_core::matcher::match_query(&q, &masks);
    let dt = t0.elapsed();
    let got = &lib.entries[res[0].entry];
    let ok = got.variant == want.variant && got.floor == want.floor;
    eprintln!(
        "[em] 自检：全库匹配耗时 {dt:?}，结果 {} {} 得分 {:.3} —— {}",
        got.name,
        got.floor,
        res[0].score,
        if ok { "正确" } else { "错误！" }
    );
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .manage(AppState {
            lib: Mutex::new(None),
            tracker: Mutex::new(tracker::Tracker::default()),
            last_frame: Mutex::new((0, 0)),
            bundle_dir: Mutex::new(PathBuf::new()),
            pin: Mutex::new(PinState::default()),
            geom: Mutex::new(GeomGuard::default()),
        })
        .setup(|app| {
            // 插件先装上，apply_hotkeys 才能通过 global_shortcut() 动态改键
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_global_shortcut::Builder::new().build())?;
            let dir = resolve_bundle_dir(app.handle());
            eprintln!("[em] 数据包目录：{}", dir.display());
            *app.state::<AppState>().bundle_dir.lock().unwrap() = dir;
            #[cfg(desktop)]
            {
                let h = app.handle();
                let hk = load_hotkeys(h);
                if let Err(e) = apply_hotkeys(h, &hk) {
                    eprintln!("[em] 全局热键注册失败（不影响其他功能）：{e}");
                }
            }
            // 移动端启动自检：加载参考库并跑一次合成匹配，
            // 验证识别核心（FFT/rayon/掩码）在该架构上确实可用
            #[cfg(mobile)]
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || self_check(&handle));
            }
            Ok(())
        });

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        analyze_screen,
        analyze_file,
        reset_lock,
        log_line,
        set_pin,
        get_pin,
        list_maps,
        floor_map,
        capabilities,
        get_hotkeys,
        set_hotkeys,
        overlay_update,
        overlay_hide
    ]);
    // Android：抓屏与悬浮窗需 Kotlin 插件（MediaProjection / SYSTEM_ALERT_WINDOW），
    // 尚未接入，先只暴露与平台无关的命令
    #[cfg(mobile)]
    let builder = builder
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("emcapture")
                .setup(|app, api: tauri::plugin::PluginApi<tauri::Wry, ()>| {
                    #[cfg(target_os = "android")]
                    {
                        use tauri::Manager;
                        let handle = api.register_android_plugin(
                            "net.yeah.enceka.embermap",
                            "CapturePlugin",
                        )?;
                        app.manage(AndroidPlugin(handle));
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = (app, api);
                    Ok(())
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            analyze_screen,
            analyze_file,
            reset_lock,
            log_line,
            set_pin,
            get_pin,
            list_maps,
            floor_map,
            capabilities,
            request_capture,
            stop_capture,
            capture_active,
            request_overlay_permission,
            overlay_update,
            overlay_hide,
            control_update,
            poll_control
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("EmberMap 启动失败");
}

#[cfg(test)]
mod geom_tests {
    use super::overlap_pct;

    #[test]
    fn 面板平移缩放仍算连续() {
        let a = [100, 100, 200, 300];
        // 小幅平移：交叠远超门槛
        assert!(overlap_pct(a, [120, 110, 200, 300]) > 70);
        // 缩放：小的整个落在大的里面，按较小者算就是满的
        assert_eq!(overlap_pct(a, [150, 150, 80, 100]), 100);
        assert_eq!(overlap_pct([150, 150, 80, 100], a), 100);
    }

    #[test]
    fn 面板瞬移到别处交叠为零() {
        let a = [100, 100, 200, 300];
        assert_eq!(overlap_pct(a, [900, 800, 200, 300]), 0);
        // 只擦到一角也远低于 20% 门槛
        assert!(overlap_pct(a, [290, 390, 200, 300]) < 20);
    }
}

/// 端到端回归：地图从画面上消失后，必须在两帧内让前端能收起叠加层。
///
/// 这条链路曾经断过一次——「几何不可信」的帧一律返回 Skip，而 Skip 的语义是
/// 「保持现状、不计入丢失」，于是叠加层永远收不起来，地图关了还盖在游戏上。
/// 单元测试都是绿的，因为没人从 run_analysis 这一层验证过整条链路。
#[cfg(test)]
mod e2e_tests {
    use super::*;

    /// 游戏地图的走廊色（HSV 落在 GAME_RANGES 的蓝灰区间）
    const CORRIDOR: [u8; 3] = [110, 120, 140];
    /// 既不是走廊也不是房间、又不至于被判成「几乎全黑」的背景色
    const BG: [u8; 3] = [60, 90, 60];

    fn state() -> AppState {
        AppState {
            lib: Mutex::new(None),
            tracker: Mutex::new(tracker::Tracker::default()),
            last_frame: Mutex::new((0, 0)),
            bundle_dir: Mutex::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bundle")),
            pin: Mutex::new(PinState::default()),
            geom: Mutex::new(GeomGuard::default()),
        }
    }

    fn fill(fw: usize, fh: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; fw * fh * 3];
        for p in rgb.chunks_exact_mut(3) {
            p.copy_from_slice(&BG);
        }
        rgb
    }

    /// 合成一帧「屏幕上开着地图」：取参考掩码的左上一块（模拟只探索了一部分，
    /// 整张会自匹配到 1.0 反被自拍闸门拦下），按走廊色画到背景上，避开画面边缘。
    fn frame_with_map(st: &AppState, fw: usize, fh: usize, at: (usize, usize)) -> Vec<u8> {
        frame_with_entry(st, 0, fw, fh, at)
    }

    fn frame_with_entry(
        st: &AppState, entry: usize, fw: usize, fh: usize, at: (usize, usize),
    ) -> Vec<u8> {
        ensure_lib(st).unwrap();
        let g = st.lib.lock().unwrap();
        let m = &g.as_ref().unwrap().entries[entry].mask;
        let (cw, ch) = (m.w * 7 / 10, m.h * 7 / 10);
        let mut rgb = fill(fw, fh);
        for y in 0..ch {
            for x in 0..cw {
                if m.at(x, y) == 0 {
                    continue;
                }
                let (px, py) = (at.0 + x, at.1 + y);
                if px < fw && py < fh {
                    let i = (py * fw + px) * 3;
                    rgb[i..i + 3].copy_from_slice(&CORRIDOR);
                }
            }
        }
        rgb
    }

    #[test]
    fn 地图消失后前端必须能收起叠加层() {
        let st = state();
        let (fw, fh) = (1400, 900);
        let map = frame_with_map(&st, fw, fh, (240, 130));

        let mut confident = false;
        for i in 0..4 {
            match run_analysis(map.clone(), fw, fh, &st).unwrap() {
                Payload::Ok { confident: c, .. } => confident |= c,
                // 桌面路径不存在「画面没变」的廉价采样，Skip 只可能来自自拍误判；
                // 合成帧不该触发，真触发了说明闸门定得太松
                Payload::Skip { hold, reason } => panic!("第 {i} 帧意外 Skip(hold={hold})：{reason}"),
                Payload::NoPanel { reason, .. } => panic!("第 {i} 帧没认出合成地图：{reason}"),
            }
        }
        assert!(confident, "连续四帧同一张地图应当锁定");

        // 地图关掉。前端只有在 NoPanel 或 Ok{confident:false} 时才会收起叠加层；
        // Skip 会让它原地不动，持续出现就是「叠加层赖着不走」。
        let blank = fill(fw, fh);
        for i in 0..2 {
            match run_analysis(blank.clone(), fw, fh, &st).unwrap() {
                Payload::NoPanel { .. } => return,
                Payload::Ok { confident, .. } => {
                    assert!(!confident, "地图已消失，第 {i} 帧不该仍报置信");
                }
                Payload::Skip { hold, reason } => {
                    panic!("地图已消失却返回 Skip(hold={hold})，叠加层会收不起来：{reason}")
                }
            }
        }
    }

    /// 换成另一张地图的那一帧：面板还在、分数却掉出门槛，跟踪器进入粘滞缓冲。
    /// 这正是当初返回 Skip 的那条路径——Skip 意味着叠加层原地不动且不计入丢失，
    /// 持续出现就再也收不起来。这一帧必须报成「不置信」，让前端照常倒计时。
    #[test]
    fn 换成另一张地图时不得返回_Skip() {
        let st = state();
        let (fw, fh) = (1400, 900);
        let a = frame_with_map(&st, fw, fh, (240, 130));
        for _ in 0..4 {
            run_analysis(a.clone(), fw, fh, &st).unwrap();
        }
        // 换一个变体的地图上来
        let other = {
            ensure_lib(&st).unwrap();
            let g = st.lib.lock().unwrap();
            let lib = g.as_ref().unwrap();
            let v0 = lib.entries[0].variant.clone();
            lib.entries.iter().position(|e| e.variant != v0).unwrap()
        };
        let b = frame_with_entry(&st, other, fw, fh, (240, 130));
        for i in 0..3 {
            match run_analysis(b.clone(), fw, fh, &st).unwrap() {
                Payload::Skip { hold, reason } => {
                    panic!("第 {i} 帧返回 Skip(hold={hold})，叠加层会赖着不走：{reason}")
                }
                _ => {}
            }
        }
    }

    /// 手动锁定变体后，地图消失同样要能收起——锁定绕开了跟踪器，是另一条分支
    #[test]
    fn 锁定变体后地图消失也要能收起() {
        let st = state();
        let (fw, fh) = (1400, 900);
        {
            ensure_lib(&st).unwrap();
            let g = st.lib.lock().unwrap();
            let e = &g.as_ref().unwrap().entries[0];
            *st.pin.lock().unwrap() = PinState {
                variant: Some(e.variant.clone()),
                floor: None,
                name: Some(e.name.clone()),
            };
        }
        let map = frame_with_map(&st, fw, fh, (240, 130));
        match run_analysis(map, fw, fh, &st).unwrap() {
            Payload::Ok { confident, .. } => assert!(confident, "锁定后单帧过门槛即置信"),
            other => panic!("锁定后应当直接认出：{}", serde_json::to_string(&other).unwrap()),
        }
        let blank = fill(fw, fh);
        match run_analysis(blank, fw, fh, &st).unwrap() {
            Payload::NoPanel { .. } => {}
            Payload::Ok { confident, .. } => assert!(!confident, "地图已消失不该仍报置信"),
            Payload::Skip { hold, reason } => panic!("返回 Skip(hold={hold})：{reason}"),
        }
    }
}

#[cfg(test)]
mod pin_gate_tests {
    use super::pin_confident;

    const GATE: f32 = 0.78;

    #[test]
    fn 真地图各楼层分得开就采信() {
        // 实测：shot-20260827 锁定左中门1-1 → 1f 0.859 / 2f 0.722 / b1 0.595
        assert!(pin_confident(&[0.859, 0.722, 0.595], GATE));
        // shot-20260826 锁定右中门1-2
        assert!(pin_confident(&[0.844, 0.580, 0.573], GATE));
    }

    #[test]
    fn 无判别力的帧不采信() {
        // 关掉大地图后 HUD 角落的小地图：分数都高，彼此却几乎并列。
        // 不锁定时靠跟踪器挡住，锁定后靠这里挡住，否则叠加层会一直挂着。
        assert!(!pin_confident(&[0.855, 0.851, 0.846], GATE));
        assert!(!pin_confident(&[0.850, 0.850, 0.849], GATE));
    }

    #[test]
    fn 分数不过门槛不采信() {
        assert!(!pin_confident(&[0.70, 0.50, 0.40], GATE));
        assert!(!pin_confident(&[], GATE));
    }

    #[test]
    fn 连楼层也锁死时只看分数() {
        assert!(pin_confident(&[0.85], GATE));
        assert!(!pin_confident(&[0.70], GATE));
    }
}
