//! 变体判定状态机：首次识别靠多帧投票，锁定后靠粘滞防抖，但**每帧仍全库扫描**，
//! 所以任何误判下一帧即可自我纠正（早期"锁定后只匹配锁定变体"会自我确认，已废弃）。

use std::collections::HashMap;

/// 累计优势达此值即锁定：单帧分差 0.07 立即锁，0.033 需两帧印证
pub const LOCK_ADVANTAGE: f32 = 0.05;
/// 投票帧数上限：证据再弱也在此帧数后采纳当前领先者，避免永不锁定
const MAX_VOTE_FRAMES: u32 = 6;
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
    /// 降采样坐标系下的尺度先验（地图缩放在一局内固定）
    pub prior_scale_ds: Option<f64>,
}

pub struct Decision {
    /// 采纳的变体（None = 证据不足，尚在识别中）
    pub variant: Option<String>,
    pub phase: &'static str,
    /// 已累计的证据强度（未锁定时用于向用户显示进度）
    pub evidence: f32,
    /// 本帧最佳变体与次佳变体的分差
    pub advantage: f32,
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
    fn 连续掉出门槛后解锁() {
        let mut t = Tracker::default();
        t.update(&frame(&[("A", 0.84), ("B", 0.77)]), GATE);
        let f = frame(&[("A", 0.5), ("B", 0.4)]);
        assert_eq!(t.update(&f, GATE).variant.as_deref(), Some("A"), "缓冲期维持显示");
        assert!(t.update(&f, GATE).variant.is_none(), "连续掉分应解锁");
        assert!(t.prior_scale_ds.is_none(), "解锁须清尺度先验");
    }
}

impl Tracker {
    pub fn reset(&mut self) {
        *self = Tracker::default();
    }

    /// 输入本帧全库候选（已按分降序），返回该显示哪个变体。
    /// candidates 必须来自全库扫描，否则粘滞逻辑失去纠错能力。
    pub fn update(&mut self, candidates: &[(String, f32)], gate: f32) -> Decision {
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
                if leader_score >= gate {
                    *self.votes.entry(leader.clone()).or_insert(0.0) += advantage;
                    self.vote_frames += 1;
                }
                let ev = self.votes.get(&leader).copied().unwrap_or(0.0);
                let top_vote = self
                    .votes
                    .iter()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(k, v)| (k.clone(), *v));
                let lock_now = leader_score >= gate
                    && ((ev >= LOCK_ADVANTAGE
                        && top_vote.as_ref().map_or(false, |(k, _)| *k == leader))
                        || self.vote_frames >= MAX_VOTE_FRAMES);
                if lock_now {
                    let v = top_vote.map(|(k, _)| k).unwrap_or(leader);
                    self.locked = Some(v.clone());
                    self.challenger = None;
                    self.lost = 0;
                    Decision { variant: Some(v), phase: "locked", evidence: ev, advantage }
                } else {
                    Decision { variant: None, phase: "acquiring", evidence: ev, advantage }
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
                        return Decision { variant: None, phase: "acquiring", evidence: 0.0, advantage };
                    }
                    // 缓冲期内维持显示
                    return Decision { variant: Some(lock), phase: "tracking", evidence: 0.0, advantage };
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
                        return Decision { variant: Some(leader), phase: "locked", evidence: 0.0, advantage };
                    }
                    self.challenger = Some((leader, n));
                } else {
                    self.challenger = None;
                }
                Decision { variant: Some(lock), phase: "tracking", evidence: 0.0, advantage }
            }
        }
    }
}
