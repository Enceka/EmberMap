//! 变体判定状态机：首次识别靠多帧投票，锁定后靠粘滞防抖，但**每帧仍全库扫描**，
//! 所以任何误判下一帧即可自我纠正（早期"锁定后只匹配锁定变体"会自我确认，已废弃）。

use std::collections::HashMap;

/// 累计优势达此值即锁定：单帧分差 0.07 立即锁，0.02 需两帧印证。
/// 可以较果断，因为锁定后每帧仍全库扫描，误锁会在 2 帧内自我纠正。
pub const LOCK_ADVANTAGE: f32 = 0.04;
/// 单帧分差低于此值视为「无判别力」，不投票也不锁定。
/// HUD 小地图（约 255×269）上 13 个变体常挤在 0.845-0.857，分差 0.00-0.006，
/// 此时任何锁定都是掷硬币；宁可一直显示识别中，等用户打开大地图或探索更多。
pub const MIN_FRAME_ADVANTAGE: f32 = 0.012;
/// 连续这么多帧检测不到面板才算换局/退出，清空投票；单帧丢失只当暂停，
/// 否则实机里地图帧与丢失帧交替会让证据永远攒不起来
const FORGET_NO_PANEL: u32 = 5;
/// 挑战者需领先锁定变体这么多分、连续这么多帧，才允许切换
const SWITCH_MARGIN: f32 = 0.025;
const SWITCH_FRAMES: u32 = 2;
/// 锁定变体连续这么多帧掉出置信门槛即解锁（换局/地图关闭/用户缩放了地图）
const LOST_FRAMES: u32 = 2;

#[derive(Default)]
pub struct Tracker {
    locked: Option<String>,
    votes: HashMap<String, f32>,
    vote_frames: u32,
    challenger: Option<(String, u32)>,
    lost: u32,
    no_panel: u32,
    /// 全分辨率尺度先验（地图缩放在一局内固定）。存全分辨率值才能跨帧复用，
    /// 因为降采样系数随分析分辨率变化。
    pub prior_scale_full: Option<f64>,
}

pub struct Decision {
    /// 采纳的变体（None = 证据不足，尚在识别中）
    pub variant: Option<String>,
    pub phase: &'static str,
    /// 已累计的证据强度（未锁定时用于向用户显示进度）
    pub evidence: f32,
    /// 本帧最佳变体与次佳变体的分差
    pub advantage: f32,
    /// 本帧的几何不可信：变体仍按粘滞维持显示，但分数已掉出门槛，
    /// 这一帧算出来的位置/尺度不能用。调用方应保持上一帧的叠加不动——
    /// 拿它去摆覆盖窗，用户看到的就是叠加层在屏幕上跳一下。
    pub stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATE: f32 = 0.78;

    fn frame(pairs: &[(&str, f32)]) -> Vec<(String, f32)> {
        pairs.iter().map(|(v, s)| (v.to_string(), *s)).collect()
    }

    #[test]
    fn 强证据单帧锁定() {
        let mut t = Tracker::default();
        let d = t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        assert_eq!(d.variant.as_deref(), Some("A"));
    }

    #[test]
    fn 弱证据需多帧印证() {
        let mut t = Tracker::default();
        let f = frame(&[("A", 0.86), ("B", 0.83)]);
        assert!(t.update(&f, GATE).variant.is_none(), "首帧证据不足不该锁");
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"));
    }

    #[test]
    fn 无判别力的帧永不锁定() {
        // HUD 小地图实况：分数都高但彼此只差 0.005，锁定即掷硬币
        let mut t = Tracker::default();
        let f = frame(&[("A", 0.855), ("B", 0.851), ("C", 0.846)]);
        for _ in 0..20 {
            assert!(t.update(&f, GATE).variant.is_none(), "分差过小不该锁定");
        }
    }

    #[test]
    fn 无判别力帧不污染证据() {
        let mut t = Tracker::default();
        let noise = frame(&[("B", 0.855), ("A", 0.851)]);
        for _ in 0..20 {
            t.update(&noise, GATE);
        }
        // 噪声不该把 B 推过门槛；此后一帧强证据应锁到 A
        let strong = frame(&[("A", 0.88), ("B", 0.80)]);
        assert_eq!(t.update(&strong, GATE).variant.as_deref(), Some("A"));
    }

    #[test]
    fn 分数不过门槛不投票() {
        let mut t = Tracker::default();
        for _ in 0..5 {
            assert!(t.update(&frame(&[("A", 0.6), ("B", 0.4)]), GATE).variant.is_none());
        }
    }

    #[test]
    fn 锁定后误判可自我纠正() {
        let mut t = Tracker::default();
        t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        // B 连续领先超过切换阈值 → 切到 B
        let f = frame(&[("B", 0.88), ("A", 0.80)]);
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"), "首帧仍粘滞");
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("B"), "连续印证后切换");
    }

    #[test]
    fn 微弱领先不触发切换() {
        let mut t = Tracker::default();
        t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        let f = frame(&[("B", 0.85), ("A", 0.84)]); // 仅差 0.01 < SWITCH_MARGIN
        for _ in 0..4 {
            assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"));
        }
    }

    #[test]
    fn 单帧丢失面板不清空证据() {
        let mut t = Tracker::default();
        let f = frame(&[("A", 0.86), ("B", 0.84)]); // 弱证据，单帧不足以锁
        assert!(t.update(&f, GATE).variant.is_none());
        assert!(!t.on_no_panel(), "单帧丢失不该清空");
        // 证据仍在，下一帧地图回来即可锁定
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"));
    }

    #[test]
    fn 持续丢失面板才清空证据() {
        let mut t = Tracker::default();
        let f = frame(&[("A", 0.86), ("B", 0.84)]);
        t.update(&f, GATE);
        for _ in 0..FORGET_NO_PANEL - 1 {
            assert!(!t.on_no_panel());
        }
        assert!(t.on_no_panel(), "连续丢失应清空");
        assert!(t.update(&f, GATE).variant.is_none(), "清空后需重新攒证据");
    }

    #[test]
    fn 连续掉出门槛后解锁() {
        let mut t = Tracker::default();
        t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        let f = frame(&[("A", 0.5), ("B", 0.4)]);
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"), "缓冲期维持显示");
        assert!(t.update(&f, GATE).variant.is_none(), "连续掉分应解锁");
        assert!(t.prior_scale_full.is_none(), "解锁须清尺度先验");
    }

    #[test]
    fn 缓冲期的几何必须标为不可信() {
        // 关掉地图后叠加层「跳几下」的来源：缓冲帧仍报出变体，
        // 调用方若拿它的位置去摆覆盖窗就会甩走。这一帧必须标 stale。
        let mut t = Tracker::default();
        assert!(!t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE).stale);
        let d = t.update(&frame(&[("A", 0.5), ("B", 0.4)]), GATE);
        assert_eq!(d.variant.as_deref(), Some("A"), "缓冲期仍显示");
        assert!(d.stale, "但几何不可信");
    }

    #[test]
    fn 正常跟踪的几何是可信的() {
        let mut t = Tracker::default();
        t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        let d = t.update(&frame(&[("A", 0.85), ("B", 0.78)]), GATE);
        assert_eq!(d.phase, "tracking");
        assert!(!d.stale);
    }
}

impl Tracker {
    pub fn reset(&mut self) {
        *self = Tracker::default();
    }

    /// 本帧没检测到地图面板：只当暂停，连续 FORGET_NO_PANEL 帧才清空证据。
    /// 返回 true 表示已清空（调用方可据此提示"等待地图"）。
    pub fn on_no_panel(&mut self) -> bool {
        self.no_panel += 1;
        if self.no_panel >= FORGET_NO_PANEL {
            self.reset();
            true
        } else {
            false
        }
    }

    /// 输入本帧全库候选（已按分降序），返回该显示哪个变体。
    /// candidates 必须来自全库扫描，否则粘滞逻辑失去纠错能力。
    pub fn update(&mut self, candidates: &[(String, f32)], gate: f32) -> Decision {
        self.no_panel = 0;
        // 每变体取最佳楼层分
        let mut best: HashMap<&str, f32> = HashMap::new();
        for (v, s) in candidates {
            let e = best.entry(v.as_str()).or_insert(f32::MIN);
            *e = e.max(*s);
        }
        let mut ranked: Vec<(&str, f32)> = best.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (leader, leader_score) = (ranked[0].0.to_string(), ranked[0].1);
        let runner = ranked.get(1).map_or(0.0, |r| r.1);
        let advantage = leader_score - runner;

        match self.locked.clone() {
            None => {
                let informative = advantage >= MIN_FRAME_ADVANTAGE;
                if leader_score >= gate && informative {
                    *self.votes.entry(leader.clone()).or_insert(0.0) += advantage;
                    self.vote_frames += 1;
                }
                let ev = self.votes.get(&leader).copied().unwrap_or(0.0);
                let top_vote = self
                    .votes
                    .iter()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(k, v)| (k.clone(), *v));
                // 必须本帧有判别力 + 累计证据够 + 领先者与累计冠军一致，三者齐备才锁
                let lock_now = leader_score >= gate
                    && informative
                    && ev >= LOCK_ADVANTAGE
                    && top_vote.as_ref().map_or(false, |(k, _)| *k == leader);
                if lock_now {
                    let v = top_vote.map(|(k, _)| k).unwrap_or(leader);
                    self.locked = Some(v.clone());
                    self.challenger = None;
                    self.lost = 0;
                    Decision { variant: Some(v), phase: "locked", evidence: ev, advantage, stale: false }
                } else {
                    Decision { variant: None, phase: "acquiring", evidence: ev, advantage, stale: false }
                }
            }
            Some(lock) => {
                let lock_score = ranked
                    .iter()
                    .find(|(v, _)| *v == lock.as_str())
                    .map_or(f32::MIN, |r| r.1);
                if lock_score < gate {
                    self.lost += 1;
                    if self.lost >= LOST_FRAMES {
                        // 解锁并清先验：可能换局，也可能用户缩放了地图导致尺度失效
                        self.reset();
                        return Decision {
                            variant: None, phase: "acquiring", evidence: 0.0, advantage, stale: false,
                        };
                    }
                    // 缓冲期内维持显示，但本帧几何不可信：分数掉出门槛意味着
                    // 匹配已经对不上，拿它的位置去摆覆盖窗就会看到叠加层跳一下
                    return Decision {
                        variant: Some(lock), phase: "tracking", evidence: 0.0, advantage, stale: true,
                    };
                }
                self.lost = 0;
                if leader != lock && leader_score - lock_score >= SWITCH_MARGIN {
                    let n = match &self.challenger {
                        Some((c, n)) if *c == leader => n + 1,
                        _ => 1,
                    };
                    if n >= SWITCH_FRAMES {
                        self.locked = Some(leader.clone());
                        self.challenger = None;
                        return Decision {
                            variant: Some(leader), phase: "locked", evidence: 0.0, advantage, stale: false,
                        };
                    }
                    self.challenger = Some((leader, n));
                } else {
                    self.challenger = None;
                }
                Decision { variant: Some(lock), phase: "tracking", evidence: 0.0, advantage, stale: false }
            }
        }
    }
}
