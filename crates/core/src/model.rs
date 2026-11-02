//! 记忆体与生活层的核心数据模型。
//!
//! 约定：所有时间都是 **epoch 毫秒(i64)**，所有数值都是 **f64**。
//! 这两条不是随便定的——bson 没有 f32、u64 有溢出坑，统一之后
//! `bson::to_document(&state)` 才能无损往返。

use serde::{Deserialize, Serialize};

/// 核心的版本号（各 crate 共用 workspace 里的版本）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 情绪三维（PAD 模型）。范围都是 -1..=1。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub struct Pad {
    /// 愉悦
    pub pleasure: f64,
    /// 唤醒度
    pub arousal: f64,
    /// 支配感
    pub dominance: f64,
}

impl Pad {
    pub fn clamp(&mut self) {
        self.pleasure = self.pleasure.clamp(-1.0, 1.0);
        self.arousal = self.arousal.clamp(-1.0, 1.0);
        self.dominance = self.dominance.clamp(-1.0, 1.0);
    }

    /// 每 tick 朝 0 回收一点（设计文档说 45 分钟半衰期，P0 先用按 tick 的近似）。
    pub fn decay(&mut self, factor: f64) {
        self.pleasure *= factor;
        self.arousal *= factor;
        self.dominance *= factor;
        self.clamp();
    }
}

/// 两道闸门的记账（设计文档 §10.4 话痨检测、§10.6 日预算）。
///
/// 单独一个结构，是因为它**按天清零**——跟别的状态量的生命周期不一样。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Gates {
    /// 记账用的是哪一天（本地日期的 `num_days_from_ce`）。换日就清零。
    pub day: i64,
    /// 今天叫了几次模型
    pub wakes: i32,
    /// 今天主动说了几次话
    pub says: i32,
    pub last_wake_ms: i64,
    /// 上次是为了哪个动作叫的（同动作 24 小时内不重复叫）
    pub last_wake_action: String,
    pub last_say_ms: i64,
}

impl Gates {
    /// 换日了就清零。`day` 传 `life::day_of(now_ms)`。
    pub fn roll(&mut self, day: i64) {
        if self.day != day {
            self.day = day;
            self.wakes = 0;
            self.says = 0;
        }
    }

    /// 距离上次主动说话多久了（毫秒）。从没说过 → `None`。
    pub fn since_last_say(&self, now_ms: i64) -> Option<i64> {
        if self.last_say_ms <= 0 {
            None
        } else {
            Some((now_ms - self.last_say_ms).max(0))
        }
    }
}

/// 一条动作记录（`core_life.recent` 里滚动保留最后 N 条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub seq: i64,
    pub ts_ms: i64,
    pub action: String,
    pub reason: String,
    /// 抽中它的那个权重，方便事后看「当时是不是勉强」
    pub weight: f64,
}

/// 生活状态：整个核心只有一个文档，`_id = "life"`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifeState {
    /// 乐观锁版本号，每次 tick +1
    pub version: i64,
    /// tick 序号
    pub seq: i64,
    pub updated_at_ms: i64,

    // ── 六个驱动量，0..=1 ─────────────────────────
    pub energy: f64,
    pub sleepy: f64,
    pub social: f64,
    pub boredom: f64,
    pub curiosity: f64,
    pub rest: f64,

    pub pad: Pad,

    /// 当前正在做的动作
    pub current: String,
    pub current_since_ms: i64,
    pub last_tick_ms: i64,

    /// 闸门记账（P3 加的，老文档里没有就用默认值）
    #[serde(default)]
    pub gates: Gates,

    pub recent: Vec<ActionRecord>,
}

/// 最近动作只保留这么多条。
pub const RECENT_CAP: usize = 50;

impl Default for LifeState {
    fn default() -> Self {
        Self {
            version: 0,
            seq: 0,
            updated_at_ms: 0,
            energy: 0.72,
            sleepy: 0.30,
            social: 0.35,
            boredom: 0.40,
            curiosity: 0.55,
            rest: 0.20,
            pad: Pad::default(),
            current: "idle".to_string(),
            current_since_ms: 0,
            last_tick_ms: 0,
            gates: Gates::default(),
            recent: Vec::new(),
        }
    }
}

impl LifeState {
    /// 设计文档 §10.1：`mood = f(energy, pad.pleasure)`
    pub fn mood(&self) -> &'static str {
        if self.energy < 0.35 {
            "困/懒"
        } else if self.energy > 0.75 && self.pad.pleasure > 0.2 {
            "精神"
        } else {
            "平常"
        }
    }

    /// 把六个量都夹回 0..=1，防止噪声漂出去。
    pub fn clamp(&mut self) {
        self.energy = self.energy.clamp(0.0, 1.0);
        self.sleepy = self.sleepy.clamp(0.0, 1.0);
        self.social = self.social.clamp(0.0, 1.0);
        self.boredom = self.boredom.clamp(0.0, 1.0);
        self.curiosity = self.curiosity.clamp(0.0, 1.0);
        self.rest = self.rest.clamp(0.0, 1.0);
        self.pad.clamp();
    }

    /// 某个动作上次是什么时候（毫秒），没有就 None。
    pub fn last_action_ms(&self, action: &str) -> Option<i64> {
        self.recent
            .iter()
            .rev()
            .find(|r| r.action == action)
            .map(|r| r.ts_ms)
    }

    /// 最近 `window` 次里某个动作出现了几次（死磕检测用）。
    pub fn count_recent(&self, action: &str, window: usize) -> usize {
        let start = self.recent.len().saturating_sub(window);
        self.recent[start..]
            .iter()
            .filter(|r| r.action == action)
            .count()
    }

    /// 最近 24 小时里出现过几种动作（§10.6 B 类「闷太久」的判据）。
    pub fn distinct_actions_since(&self, since_ms: i64) -> usize {
        let mut seen: Vec<&str> = Vec::new();
        for r in self.recent.iter().filter(|r| r.ts_ms >= since_ms) {
            if !seen.contains(&r.action.as_str()) {
                seen.push(&r.action);
            }
        }
        seen.len()
    }

    /// 换日就清闸门记账。每个 tick 开头、以及任何读闸门之前都要先调它。
    pub fn roll_day(&mut self, day: i64) {
        self.gates.roll(day);
    }
}

/// 内容型动作跑完之后开的一个「待填」的槽（设计文档 §10.5 产出回流）。
///
/// 为什么不在动作里直接把字写死：写字的得是模型（§10.6 C 类），
/// 核心自己编一句「今天想了点什么」那就是假的了。所以先开槽，
/// 谁有本事填谁来填；没人填就一直是 pending，**如实挂着，不假装有产出**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Produce {
    pub id: String,
    /// 哪个动作开的槽（reflect / study / note / watch / roam）
    pub action: String,
    /// 对应 `core_life.seq`，方便对到那一步
    pub seq: i64,
    pub asked_at_ms: i64,
    /// pending / done / dropped
    pub status: String,
    /// 填进来之后落成哪一层：thought / technique / event
    pub layer: String,
    /// 给填写的人看的一句话提示
    pub prompt: String,
    /// 填进来的字
    pub text: String,
    /// 落库之后那条记忆的 id
    pub mem_id: String,
    pub done_at_ms: i64,
}

impl Produce {
    /// 一个动作开的槽该落哪一层。想出来的归想法，学到手的归技巧，其它归事件。
    pub fn layer_for(action: &str) -> &'static str {
        match action {
            "reflect" | "study" => "thought",
            "note" => "technique",
            _ => "event",
        }
    }

    pub fn prompt_for(action: &str) -> String {
        match action {
            "reflect" => "刚发完呆，想到什么就写一句".to_string(),
            "study" => "刚看完点东西，学到的写一句".to_string(),
            "note" => "把手上这点零碎整理成一句话".to_string(),
            "watch" => "刚才看的东西里，有一句值得记下来".to_string(),
            "roam" => "走了走，路上看见什么、心里起了一句什么".to_string(),
            other => format!("「{other}」干完了，本该留下一句什么"),
        }
    }

    pub fn new(action: &str, seq: i64, now_ms: i64) -> Self {
        Self {
            id: format!("p{seq}-{now_ms}"),
            action: action.to_string(),
            seq,
            asked_at_ms: now_ms,
            status: "pending".to_string(),
            layer: Self::layer_for(action).to_string(),
            prompt: Self::prompt_for(action),
            text: String::new(),
            mem_id: String::new(),
            done_at_ms: 0,
        }
    }
}
