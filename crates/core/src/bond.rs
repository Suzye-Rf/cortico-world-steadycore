//! 关系层：一个人跟我有多熟，分数怎么长、怎么掉。
//!
//! 为什么单独一层：`relation` 记忆存的是「跟这个人有关的一件事」，会淡；
//! 交情分数不一样——它得跨事件累加、按天封顶、久不联系自己回落。
//! 早先这几样全塞在一句话里（「交情 55 分，朋友档，聊过 1 句」），
//! 那行字一旦写进库里就再也不会动，所以单独落一个 `core_bond`，一行字由它现算。
//!
//! 规则（都在这儿，别的地方不许再写一份）：
//! - 没见过面 = 50 分；除用户外最高 95；
//! - 聊一句 +1，分过 70 之后 +0.5（越熟越慢），一天最多 +6；
//! - 被人拿话戳了之类的硬扣，走 `bump` 的负数，不受日封顶；
//! - 超过 30 天不说话，每多一天 -0.1，最低回到 50；
//! - `pinned` 的人（用户）不掉、能到 100。

use serde::{Deserialize, Serialize};

pub use crate::life::day_of;

/// 陌生人的起点。没见过面的人不该低于它。
pub const BASE: f64 = 50.0;
/// 除用户以外能到的最热。
pub const CAP: f64 = 95.0;
/// 用户那一格。
pub const OWNER_CAP: f64 = 100.0;
/// 一天最多长这么多，免得一口气聊两小时就把交情刷满。
pub const DAY_CAP: f64 = 6.0;
/// 超过这么多天没说话，开始往回掉。
pub const IDLE_DAYS: f64 = 30.0;
/// 掉多快：一天 0.1 分（十天一分）。
pub const DECAY_PER_DAY: f64 = 0.1;
/// 热起来之后每句只涨这么多。
pub const CLOSE_STEP: f64 = 0.5;

const DAY_MS: i64 = 86_400_000;

/// 一个人的交情账。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bond {
    /// 稳定键：QQ 号 / 昵称。改名字不该丢分，所以键不能是显示名。
    pub who: String,
    /// 怎么称呼这人
    pub name: String,
    pub score: f64,
    /// 聊过几句
    pub talks: i64,
    pub first_at: i64,
    pub last_at: i64,
    /// 永远不淡的人
    pub pinned: bool,
    /// 一句备注：这人是谁、哪件事还记着
    pub note: String,
    /// 今天哪一天、今天已经长了多少（按天封顶用）
    pub day: i64,
    pub day_gain: f64,
    /// 衰减算到哪一刻了（重复调用不重复扣）
    pub decay_at: i64,
    pub updated_at: i64,
}

impl Bond {
    pub fn new(who: &str, name: &str, now: i64) -> Self {
        let name = if name.trim().is_empty() { who } else { name.trim() };
        Self {
            who: who.trim().to_string(),
            name: name.to_string(),
            score: BASE,
            talks: 0,
            first_at: now,
            last_at: now,
            pinned: false,
            note: String::new(),
            day: day_of(now),
            day_gain: 0.0,
            decay_at: now,
            updated_at: now,
        }
    }

    pub fn cap(&self) -> f64 {
        if self.pinned {
            OWNER_CAP
        } else {
            CAP
        }
    }

    /// 档位：给开机包和工具回执用的一句话。
    pub fn tier(&self) -> &'static str {
        match self.score {
            s if s < 45.0 => "生面孔",
            s if s < 55.0 => "眼熟",
            s if s < 70.0 => "朋友档",
            s if s < 85.0 => "熟人",
            _ => "老相识",
        }
    }

    /// 聊过一句。返回这次涨了多少分。
    pub fn talk(&mut self, now: i64) -> f64 {
        let d = day_of(now);
        if d != self.day {
            self.day = d;
            self.day_gain = 0.0;
        }
        self.talks += 1;
        self.last_at = now;
        let step = if self.score >= 70.0 { CLOSE_STEP } else { 1.0 };
        // 三道闸：单句的量、今天还剩多少、离上限还差多少
        let room_day = (DAY_CAP - self.day_gain).max(0.0);
        let room_top = (self.cap() - self.score).max(0.0);
        let gain = step.min(room_day).min(room_top);
        self.score += gain;
        self.day_gain += gain;
        self.updated_at = now;
        gain
    }

    /// 手动加 / 扣。扣分不受日封顶，加分受总分上限。
    pub fn bump(&mut self, delta: f64, now: i64) -> f64 {
        let before = self.score;
        self.score = (self.score + delta).clamp(0.0, self.cap());
        self.updated_at = now;
        self.score - before
    }

    /// 久不联系慢慢回落。只算跨过的整天，tick 反复调也不会重复扣。
    pub fn decay(&mut self, now: i64) -> f64 {
        if self.pinned {
            self.decay_at = now;
            return 0.0;
        }
        let start = self.last_at + (IDLE_DAYS * DAY_MS as f64) as i64;
        let from = self.decay_at.max(start);
        if now <= from {
            return 0.0;
        }
        let days = ((now - from) as f64 / DAY_MS as f64).floor();
        if days < 1.0 {
            return 0.0;
        }
        let before = self.score;
        self.score = (self.score - DECAY_PER_DAY * days).max(BASE).min(before);
        self.decay_at = now;
        self.updated_at = now;
        before - self.score
    }

    pub fn set_note(&mut self, note: &str, now: i64) {
        let note = note.trim();
        if note.is_empty() {
            return;
        }
        self.note = note.to_string();
        self.updated_at = now;
    }

    /// 开机包关系层的那一行。
    ///
    /// `pinned` 的人（用户）不写句数：那格是「聊过几句」，对他写着只会难看。
    pub fn line(&self) -> String {
        let mut s = format!(
            "- {}：交情 {:.0} 分，{}",
            self.name.trim(),
            self.score,
            self.tier()
        );
        if !self.pinned {
            s.push_str(&format!("，聊过 {} 句", self.talks));
        }
        if !self.note.trim().is_empty() {
            s.push_str("，");
            s.push_str(self.note.trim());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const D: i64 = DAY_MS;

    #[test]
    fn 新人从五十分起() {
        let b = Bond::new("123", "小明", 0);
        assert_eq!(b.score, BASE);
        assert_eq!(b.tier(), "眼熟");
    }

    #[test]
    fn 一天涨不过六分() {
        let mut b = Bond::new("123", "小明", 0);
        // 二十分钟里说二十句：同一本地日内，涨到 6 分就该停
        for i in 0..20 {
            b.talk(i * 60_000);
        }
        assert!((b.score - (BASE + DAY_CAP)).abs() < 1e-9, "score={}", b.score);
        assert_eq!(b.talks, 20);
        // 换一天又能长
        b.talk(D);
        assert!(b.score > BASE + DAY_CAP);
    }

    #[test]
    fn 越熟越慢且封顶() {
        let mut b = Bond::new("123", "小明", 0);
        b.score = 70.0;
        b.day_gain = 0.0;
        let g = b.talk(0);
        assert!((g - CLOSE_STEP).abs() < 1e-9);
        b.day_gain = 0.0;
        b.score = CAP;
        assert_eq!(b.talk(0), 0.0);
        assert_eq!(b.score, CAP);
    }

    #[test]
    fn 用户不封顶也不掉() {
        let mut b = Bond::new("owner", "用户", 0);
        b.pinned = true;
        b.score = 100.0;
        assert_eq!(b.talk(0), 0.0);
        let before = b.score;
        assert_eq!(b.decay(1000 * D), 0.0);
        assert_eq!(b.score, before);
    }

    #[test]
    fn 三十天之后才掉而且不重复掉() {
        let mut b = Bond::new("123", "小明", 0);
        b.score = 70.0;
        assert_eq!(b.decay(20 * D), 0.0, "还没到三十天");
        // 跑到第 40 天：只该算 10 天
        let lost = b.decay(40 * D);
        assert!((lost - 1.0).abs() < 1e-6, "lost={lost}");
        // 同一刻再调一次，不该再扣
        assert_eq!(b.decay(40 * D), 0.0);
        assert!((b.score - 69.0).abs() < 1e-6);
    }

    #[test]
    fn 掉不到陌生人以下() {
        let mut b = Bond::new("123", "小明", 0);
        b.score = BASE + 0.5;
        b.decay(365 * D);
        assert!((b.score - BASE).abs() < 1e-9);
    }

    #[test]
    fn 硬扣不受日封顶() {
        let mut b = Bond::new("123", "小明", 0);
        b.day_gain = DAY_CAP;
        let d = b.bump(-3.0, 0);
        assert!((d + 3.0).abs() < 1e-9);
        assert!((b.score - (BASE - 3.0)).abs() < 1e-9);
    }

    #[test]
    fn 一行字带着名档句数() {
        let mut b = Bond::new("755009004", "老何", 0);
        b.score = 62.0;
        b.talks = 8;
        b.set_note("4412 群，爱问技术", 0);
        assert_eq!(b.line(), "- 老何：交情 62 分，朋友档，聊过 8 句，4412 群，爱问技术");
    }

    #[test]
    fn 用户那行不写句数() {
        let mut b = Bond::new("10001", "用户", 0);
        b.pinned = true;
        b.score = 100.0;
        b.set_note("张三，就这一个", 0);
        assert_eq!(b.line(), "- 用户：交情 100 分，老相识，张三，就这一个");
    }
}
