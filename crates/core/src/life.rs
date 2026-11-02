//! 生活状态机（设计文档 §10）。
//!
//! 一个 tick = 演化状态 → 算候选 → 打分 → 抽样 → 施加反馈 → 记账。
//! **全程纯函数、不碰 IO、不调大模型**——要不要叫模型由 `decide()` 判断，
//! 真叫出去、真产出什么是 server 那一层的事。
//!
//! P0 只做了干跑；P3 补上三样：
//! 1. [`decide`]：这一 tick 该不该唤醒大模型（§10.6 的三类判据 + 四道闸门）；
//! 2. [`accept_intent`]：模型吐回来的 intent 拿去做，**做不做得到另说**（§10.6 末）；
//! 3. [`simulate`]：虚拟时间快进，用来干跑 24 小时看动作单不单调。

use std::collections::BTreeMap;

use chrono::{Datelike, Local, TimeZone, Timelike};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::model::{ActionRecord, LifeState, RECENT_CAP};

/// 动作全集（设计文档 §10.3）。顺序即并列时的优先顺序。
pub const ACTIONS: [&str; 11] = [
    "idle", "rest", "nap", "reflect", "study", "read", "watch", "roam", "note", "tidy", "pause",
];

/// 内容型动作：P3 之后这些动作要叫模型写东西，P0 只记一句「干跑没产出」。
pub const CONTENT_ACTIONS: [&str; 5] = ["reflect", "study", "note", "watch", "roam"];

pub fn is_content(action: &str) -> bool {
    CONTENT_ACTIONS.contains(&action)
}

// ── 闸门的几个数（§10.6「闸门」、§10.4「话痨检测」） ──────────────
/// 一天最多叫模型几次。超了就只剩规则层跑。
pub const WAKE_DAILY_MAX: i32 = 24;
/// 主动说话一天最多几次。
pub const SAY_DAILY_MAX: i32 = 6;
/// 两次主动说话之间至少隔多久。
pub const SAY_MIN_GAP_MS: i64 = 90 * 60 * 1000;
/// 同一动作 24 小时内不重复叫模型。
pub const WAKE_SAME_ACTION_MS: i64 = 24 * 3600 * 1000;
/// 离上次叫不到这么久，就把新理由**合并**进上一次，不另起一 call。
pub const WAKE_MERGE_MS: i64 = 10 * 60 * 1000;
/// 收益门槛：值不值低于这个数，直接抽，不问模型。
pub const VALUE_FLOOR: f64 = 0.30;
/// 一句主动的话说出口，`social` 落下去多少。
pub const SAY_RELIEF: f64 = 0.45;

/// 本地日期的序号，用来判断「换日了没有」。取不到就退化成 0。
pub fn day_of(now_ms: i64) -> i64 {
    Local
        .timestamp_millis_opt(now_ms)
        .single()
        .map(|t| t.date_naive().num_days_from_ce() as i64)
        .unwrap_or(0)
}

/// 一次 tick 的结果，直接拿去回 API / 写日志。
#[derive(Debug, Clone)]
pub struct TickOutcome {
    pub seq: i64,
    pub action: String,
    pub reason: String,
    pub weight: f64,
    /// 所有候选和它们的权重，从高到低。用来解释「为什么没选那个」
    pub scores: Vec<(String, f64)>,
    /// 这一 tick 有没有触发越线（憋太久 / 闷太久 / 卡住了）
    pub alerts: Vec<String>,
}

/// 冷却表：动作 → 最小间隔毫秒。`0` 表示没有冷却。
fn cooldown_ms(action: &str) -> i64 {
    match action {
        "rest" => 30 * 60 * 1000,
        "read" => 2 * 3600 * 1000,
        "watch" => 3 * 3600 * 1000,
        "roam" => 2 * 3600 * 1000,
        "note" => 1 * 3600 * 1000,
        "reflect" => 24 * 3600 * 1000,
        "tidy" => 24 * 3600 * 1000,
        _ => 0,
    }
}

fn hour_of(now_ms: i64) -> u32 {
    Local
        .timestamp_millis_opt(now_ms)
        .single()
        .map(|t| t.hour())
        .unwrap_or(12)
}

/// 某个动作此刻值多少分。`None` = 不满足前置条件，进不了候选集。
fn score(action: &str, s: &LifeState, now_ms: i64) -> Option<f64> {
    // 冷却没过就不进候选集（pause / idle 没有冷却，永远在）
    let cd = cooldown_ms(action);
    if cd > 0 {
        if let Some(last) = s.last_action_ms(action) {
            if now_ms - last < cd {
                return None;
            }
        }
    }

    let hour = hour_of(now_ms);
    let v = match action {
        "idle" => 0.30 + s.boredom * 0.60,
        "rest" => {
            if s.energy < 0.60 {
                0.35 + (0.60 - s.energy) * 1.50
            } else {
                return None;
            }
        }
        "nap" => {
            let lunch = (12..14).contains(&hour);
            if s.energy < 0.35 || lunch {
                0.50 + (0.35 - s.energy).max(0.0) * 1.20
            } else {
                return None;
            }
        }
        "reflect" => 0.25 + s.boredom * 0.40,
        "study" => {
            if s.curiosity > 0.50 && s.energy > 0.40 {
                0.30 + s.curiosity * 0.80
            } else {
                return None;
            }
        }
        "read" => 0.25 + s.curiosity * 0.50,
        "watch" => 0.20 + s.boredom * 0.30 + s.curiosity * 0.30,
        "roam" => 0.25 + s.boredom * 0.60,
        "note" => 0.15 + s.curiosity * 0.20,
        "tidy" => {
            if s.boredom > 0.50 {
                0.20 + s.boredom * 0.20
            } else {
                return None;
            }
        }
        "pause" => 0.15,
        _ => return None,
    };

    // 困到不行的时候，什么都不想干 —— 除了睡
    let mut v = v;
    if s.energy < 0.25 && !matches!(action, "nap" | "rest") {
        v *= 0.25;
    }

    // 死磕检测（§10.4）：最近 8 次里同一个动作出现 3 次 → 狠狠压权重
    if s.count_recent(action, 8) >= 3 {
        v *= 0.25;
    }

    Some(v.max(0.0001))
}

/// 施加动作反馈（§10.3「反馈」一列）。P0 只有状态变化，没有产出物。
fn apply_feedback(action: &str, s: &mut LifeState) {
    match action {
        "idle" => s.boredom += 0.05,
        "rest" => {
            s.energy += 0.15;
            s.rest -= 0.20;
            s.sleepy -= 0.05;
        }
        "nap" => {
            s.energy += 0.30;
            s.sleepy = 0.0;
            s.rest -= 0.30;
        }
        "reflect" => {
            s.boredom -= 0.08;
            s.energy -= 0.03;
        }
        "study" => {
            s.curiosity -= 0.20;
            s.energy -= 0.08;
        }
        "read" => {
            s.curiosity -= 0.15;
            s.energy -= 0.03;
        }
        "watch" => {
            s.curiosity -= 0.20;
            s.boredom -= 0.15;
        }
        "roam" => {
            s.boredom -= 0.10;
            s.energy -= 0.02;
        }
        "note" => {
            s.boredom -= 0.05;
            s.energy -= 0.03;
        }
        "tidy" => {
            s.boredom -= 0.15;
            s.energy -= 0.02;
        }
        "pause" => s.boredom += 0.03,
        _ => {}
    }
}

/// 从候选集里按权重抽一个。`r` 是 0..1 的随机数（抽样的核心，单独拆出来好测）。
fn pick_weighted(cands: &[(String, f64)], r: f64) -> usize {
    let total: f64 = cands.iter().map(|(_, w)| *w).sum();
    if total <= 0.0 {
        return 0;
    }
    let mut acc = 0.0;
    let target = r * total;
    for (i, (_, w)) in cands.iter().enumerate() {
        acc += *w;
        if target <= acc {
            return i;
        }
    }
    cands.len() - 1
}

/// 走一步。`force` 有值时强制做那个动作（调试 / 我手动推一把时用）。
pub fn step<R: Rng + ?Sized>(
    s: &mut LifeState,
    now_ms: i64,
    force: Option<&str>,
    rng: &mut R,
) -> TickOutcome {
    // `force` 只认动作表里的名字。`?force=1` 这种「只想推一步」的写法当没给，
    // 否则会凭空造出一个叫 "1" 的动作。
    let force = force.filter(|f| ACTIONS.contains(f));

    let mut alerts: Vec<String> = Vec::new();

    s.roll_day(day_of(now_ms));

    // ── 1. 自然演化 + 噪声（§10.2 不确定性之一） ─────────────────
    s.energy -= 0.005;
    s.sleepy += 0.004;
    s.social += 0.030;
    s.boredom += 0.030;
    s.curiosity += 0.015;
    s.rest += 0.010;

    let noise = |rng: &mut R| -> f64 { rng.gen_range(-0.03..0.03) };
    s.energy += noise(rng);
    s.social += noise(rng);
    s.boredom += noise(rng);
    s.curiosity += noise(rng);

    // 情绪往回收（45 分钟半衰期，按 tick 近似）
    let tick_gap_min = if s.last_tick_ms > 0 {
        ((now_ms - s.last_tick_ms) as f64 / 60000.0).max(0.0)
    } else {
        40.0
    };
    let half = 0.5f64.powf(tick_gap_min / 45.0);
    s.pad.decay(half);

    s.clamp();

    // ── 2. 越线检测（§10.6 B 类：憋太久 / 闷太久 / 卡住了） ─────
    alerts.extend(threshold_alerts(s, now_ms));

    // ── 3. 候选 + 打分 ───────────────────────────────────────────
    let mut cands: Vec<(String, f64)> = Vec::new();
    for a in ACTIONS {
        if let Some(w) = score(a, s, now_ms) {
            cands.push((a.to_string(), w));
        }
    }
    // 兜底：候选为空（理论上 pause/idle 永远在，这里只是防一手）
    if cands.is_empty() {
        cands.push(("pause".to_string(), 0.1));
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let all_scores = cands.clone();
    let chosen = if let Some(f) = force {
        cands
            .iter()
            .position(|(a, _)| a == f)
            .map(|i| cands[i].clone())
            .unwrap_or_else(|| (f.to_string(), 0.0))
    } else if rng.gen_range(0.0..1.0) < 0.02 && cands.len() > 2 {
        // 从「次优可选集合」里随机挑一个（§10.2 之二）
        let i = rng.gen_range(1..cands.len());
        cands[i].clone()
    } else {
        let r = rng.gen_range(0.0..1.0);
        let i = pick_weighted(&cands, r);
        cands[i].clone()
    };

    let (action, weight) = chosen;

    // ── 5. 施加反馈 + 记账 ──────────────────────────────────────
    apply_feedback(&action, s);
    s.clamp();

    let reason = if let Some(f) = force {
        format!("外部强制：{f}")
    } else {
        let top = all_scores.first().map(|(a, _)| a.clone()).unwrap_or_default();
        let top_w = all_scores.first().map(|(_, w)| *w).unwrap_or(0.0);
        if top == action {
            format!("加权抽样命中最高分（{action} {weight:.3}）")
        } else {
            // 这不是「故意跑偏」——加权抽样本来就常常不选第一名，
            // 写清楚是哪个更高，别让人以为出了 bug。
            format!("加权抽样（选 {action} {weight:.3}；更高的是 {top} {top_w:.3}）")
        }
    };

    record_action(s, &action, &reason, weight, now_ms);

    TickOutcome {
        seq: s.seq,
        action,
        reason,
        weight,
        scores: all_scores,
        alerts,
    }
}

/// 记一笔动作。规则抽中的和 intent 进来的走同一条路——不然后面查日志会对不上。
fn record_action(s: &mut LifeState, action: &str, reason: &str, weight: f64, now_ms: i64) {
    s.seq += 1;
    s.version += 1;
    s.updated_at_ms = now_ms;
    s.last_tick_ms = now_ms;
    s.current = action.to_string();
    s.current_since_ms = now_ms;

    s.recent.push(ActionRecord {
        seq: s.seq,
        ts_ms: now_ms,
        action: action.to_string(),
        reason: reason.to_string(),
        weight,
    });
    if s.recent.len() > RECENT_CAP {
        let overflow = s.recent.len() - RECENT_CAP;
        s.recent.drain(0..overflow);
    }
}

/// §10.6 B 类的三条越线判据。`step` 和 `decide` 共用，免得两处算出不一样的结论。
fn threshold_alerts(s: &LifeState, now_ms: i64) -> Vec<String> {
    let mut alerts: Vec<String> = Vec::new();

    // 憋太久：social 顶到 0.85 以上
    if s.social > 0.85 {
        alerts.push("social_high".to_string());
    }

    // 闷太久：无聊到顶，而且 24 小时里没干过几种不同的事
    let day_ago = now_ms - 24 * 3600 * 1000;
    if s.boredom > 0.80 && s.distinct_actions_since(day_ago) < 3 {
        alerts.push("boredom_high".to_string());
    }

    // 卡住了：连续 6 次 pause，或连续 8 次同一个动作
    if s.count_recent("pause", 6) >= 6 {
        alerts.push("stuck".to_string());
    }
    if let Some(last) = s.recent.last() {
        let same = last.action.clone();
        if s.count_recent(&same, 8) >= 8 {
            alerts.push("stuck_same_action".to_string());
        }
    }

    alerts
}

/// 六个小时没主动说过话（从没说过也算）。
fn muted_6h(s: &LifeState, now_ms: i64) -> bool {
    s.gates
        .since_last_say(now_ms)
        .map(|d| d > 6 * 3600 * 1000)
        .unwrap_or(true)
}

/// 按当前状态算一遍候选集，从高到低。
fn candidate_scores(s: &LifeState, now_ms: i64) -> Vec<(String, f64)> {
    let mut cands: Vec<(String, f64)> = ACTIONS
        .iter()
        .filter_map(|a| score(a, s, now_ms).map(|w| (a.to_string(), w)))
        .collect();
    if cands.is_empty() {
        cands.push(("pause".to_string(), 0.1));
    }
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    cands
}

// ══════════════════════════════════════════════════════════════
//  §10.6 要不要叫模型
// ══════════════════════════════════════════════════════════════

/// 这一 tick 该不该唤醒大模型，以及为什么。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeDecision {
    /// 最终结论：叫 / 不叫
    pub wake: bool,
    /// 主类：`A`（决策）/ `B`（越线反常）/ `C`（要内容）/ 空串（没理由）
    pub kind: String,
    /// 三类理由，一条一条写清，别只说「该叫了」
    pub reasons: Vec<String>,
    /// 值不值（收益门槛），低于 [`VALUE_FLOOR`] 就不叫
    pub value: f64,
    pub top1: Option<(String, f64)>,
    pub top2: Option<(String, f64)>,
    /// 被哪道闸门挡下的。`wake = false` 而这块不空，就是「该叫但不该叫」
    pub blocked_by: Vec<String>,
}

/// 这一 tick 值不值得叫一次模型（§10.6）。
///
/// 纯函数：只看状态，不改它。叫不叫、叫了记不记账，是调用方的事。
pub fn decide(s: &LifeState, now_ms: i64) -> WakeDecision {
    let cands = candidate_scores(s, now_ms);
    let top1 = cands.first().cloned();
    let top2 = cands.get(1).cloned();

    let mut a: Vec<String> = Vec::new();
    let mut b: Vec<String> = Vec::new();
    let mut c: Vec<String> = Vec::new();

    // ── A 类：决策 ────────────────────────────────────────────
    if let (Some((_, w1)), Some((_, w2))) = (&top1, &top2) {
        if *w1 < 0.45 {
            a.push(format!("分不出高下：最高分只有 {w1:.2}"));
        }
        if w1 - w2 < 0.08 {
            a.push(format!("前两名只差 {:.3}（{w1:.2} / {w2:.2}）", w1 - w2));
        }
    }
    // 该做的事互相打架
    if s.social > 0.60 && s.energy < 0.40 {
        a.push(format!(
            "想说话（{:.2}）可精力只剩 {:.2}——去说还是先歇着，规则加不出这个权",
            s.social, s.energy
        ));
    }
    if s.social > 0.60 {
        a.push(format!(
            "想说话了（social {:.2}）：要不要打扰人是个价值判断",
            s.social
        ));
    }

    // ── B 类：越线 / 反常 ─────────────────────────────────────
    for alert in threshold_alerts(s, now_ms) {
        match alert.as_str() {
            "social_high" => b.push(format!(
                "憋太久：social {:.2}{}",
                s.social,
                if muted_6h(s, now_ms) {
                    "，而且六个小时没主动说过话"
                } else {
                    ""
                }
            )),
            "boredom_high" => b.push(format!(
                "闷太久：boredom {:.2}，24 小时里只干过 {} 种事",
                s.boredom,
                s.distinct_actions_since(now_ms - 24 * 3600 * 1000)
            )),
            "stuck" => b.push("卡住了：连续 6 次 pause".to_string()),
            "stuck_same_action" => b.push("卡住了：连续 8 次同一个动作".to_string()),
            other => b.push(other.to_string()),
        }
    }

    // ── C 类：要内容 ──────────────────────────────────────────
    if let Some((act, _)) = &top1 {
        if is_content(act) {
            c.push(format!(
                "要内容：接下来这个动作（{act}）本该产出一段字，规则写不出来"
            ));
        }
    }

    // ── 值不值 ────────────────────────────────────────────────
    let mut value: f64 = 0.0;
    if !a.is_empty() {
        let w1 = top1.as_ref().map(|(_, w)| *w).unwrap_or(0.0);
        value = value.max((1.05 - w1).clamp(0.0, 1.0));
    }
    if !b.is_empty() {
        let over = (s.social - 0.85).max(s.boredom - 0.80).max(0.0) / 0.15;
        value = value.max(0.50 + over * 0.40);
    }
    if !c.is_empty() {
        value = value.max(0.60);
    }

    // ── 闸门（防止变成烧钱定时器） ─────────────────────────────
    let mut blocked: Vec<String> = Vec::new();

    if s.gates.wakes >= WAKE_DAILY_MAX {
        blocked.push(format!(
            "今天的唤醒额度用完了（{}/{}）",
            s.gates.wakes, WAKE_DAILY_MAX
        ));
    }

    // B 类问的是「我最近怎么了」，不针对某个动作，所以不吃「同动作」那道闸
    let main_action = if b.is_empty() {
        top1.as_ref().map(|(x, _)| x.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    if !main_action.is_empty()
        && s.gates.last_wake_action == main_action
        && now_ms - s.gates.last_wake_ms < WAKE_SAME_ACTION_MS
    {
        blocked.push(format!(
            "「{main_action}」24 小时内刚问过（上次 {:.1} 小时前）",
            (now_ms - s.gates.last_wake_ms) as f64 / 3_600_000.0
        ));
    }
    if s.gates.last_wake_ms > 0 && now_ms - s.gates.last_wake_ms < WAKE_MERGE_MS {
        blocked.push(format!(
            "刚叫过（{:.0} 分钟前），新的理由合并进上一次",
            (now_ms - s.gates.last_wake_ms) as f64 / 60_000.0
        ));
    }

    if (!a.is_empty() || !b.is_empty() || !c.is_empty()) && value < VALUE_FLOOR {
        blocked.push(format!("值不值只有 {value:.2}，低于门槛 {VALUE_FLOOR:.2}"));
    }

    let kind = if !b.is_empty() {
        "B"
    } else if !a.is_empty() {
        "A"
    } else if !c.is_empty() {
        "C"
    } else {
        ""
    }
    .to_string();

    let has_reason = !(a.is_empty() && b.is_empty() && c.is_empty());
    let mut reasons = Vec::new();
    reasons.extend(a);
    reasons.extend(b);
    reasons.extend(c);

    WakeDecision {
        wake: has_reason && blocked.is_empty(),
        kind,
        reasons,
        value,
        top1,
        top2,
        blocked_by: blocked,
    }
}

// ══════════════════════════════════════════════════════════════
//  intent：模型说想做，不等于真做了
// ══════════════════════════════════════════════════════════════

/// 模型（或我）吐回来的「想做」。§10.6：模型输出的不是「做了」。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Intent {
    pub action: String,
    #[serde(default)]
    pub why: Option<String>,
    /// 想主动说的话。过话痨闸门才算数。
    #[serde(default)]
    pub say: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

/// intent 被拒的五种理由。**拒绝也要说清是哪一档**——只回个 false 等于没说。
#[derive(Debug, Clone)]
pub enum IntentReject {
    Unknown(String),
    Precondition(String),
    Cooldown(String),
    Stubborn(String),
    TalkTooMuch(String),
}

impl IntentReject {
    pub fn code(&self) -> &'static str {
        match self {
            IntentReject::Unknown(_) => "unknown_action",
            IntentReject::Precondition(_) => "precondition_failed",
            IntentReject::Cooldown(_) => "in_cooldown",
            IntentReject::Stubborn(_) => "stubborn_topic",
            IntentReject::TalkTooMuch(_) => "talk_too_much",
        }
    }

    pub fn message(&self) -> String {
        match self {
            IntentReject::Unknown(a) => format!("没有「{a}」这个动作"),
            IntentReject::Precondition(m)
            | IntentReject::Cooldown(m)
            | IntentReject::Stubborn(m)
            | IntentReject::TalkTooMuch(m) => m.clone(),
        }
    }
}

/// intent 被接受之后，实际做了什么。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentDone {
    pub seq: i64,
    pub action: String,
    pub why: String,
    pub say: Option<String>,
    /// 记下来的权重，方便跟规则抽的一对比
    pub weight: f64,
}

/// 拿一个 intent 去执行。做不到就拒，理由写清。
pub fn accept_intent(
    s: &mut LifeState,
    it: &Intent,
    now_ms: i64,
) -> Result<IntentDone, IntentReject> {
    let action = it.action.trim().to_string();
    if !ACTIONS.contains(&action.as_str()) {
        return Err(IntentReject::Unknown(action));
    }
    s.roll_day(day_of(now_ms));

    // 前置条件 / 冷却：直接问 `score()`，不另写一套，免得两边结论不一样
    let weight = match score(&action, s, now_ms) {
        Some(w) => w,
        None => {
            let cd = cooldown_ms(&action);
            if cd > 0 {
                if let Some(last) = s.last_action_ms(&action) {
                    let left = cd - (now_ms - last);
                    if left > 0 {
                        return Err(IntentReject::Cooldown(format!(
                            "「{action}」还在冷却里，还差 {:.0} 分钟",
                            left as f64 / 60_000.0
                        )));
                    }
                }
            }
            return Err(IntentReject::Precondition(format!(
                "「{action}」此刻的前置条件不满足（看 /v1/life 的状态量）"
            )));
        }
    };

    // 死磕：最近 8 次里同一个动作已经 3 次，别再来
    let hits = s.count_recent(&action, 8);
    if hits >= 3 {
        return Err(IntentReject::Stubborn(format!(
            "最近 8 次里「{action}」已经出现过 {hits} 次，换个事儿干"
        )));
    }

    // 话痨闸门
    let say = it
        .say
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());
    if say.is_some() {
        if s.gates.says >= SAY_DAILY_MAX {
            return Err(IntentReject::TalkTooMuch(format!(
                "今天已经主动说过 {} 次，够了",
                s.gates.says
            )));
        }
        if let Some(gap) = s.gates.since_last_say(now_ms) {
            if gap < SAY_MIN_GAP_MS {
                return Err(IntentReject::TalkTooMuch(format!(
                    "距上次主动说话才 {:.0} 分钟，还得再等 {:.0} 分钟",
                    gap as f64 / 60_000.0,
                    (SAY_MIN_GAP_MS - gap) as f64 / 60_000.0
                )));
            }
        }
    }

    let why = it
        .why
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("没写为什么就做了")
        .to_string();

    apply_feedback(&action, s);
    s.clamp();

    if say.is_some() {
        s.gates.says += 1;
        s.gates.last_say_ms = now_ms;
        // 话说出口就算真的连上了，想说话的那股劲泄掉一大截。
        // 不泄的话 social 只涨不落，会一整天顶着 1.00 喊「憋太久」。
        s.social -= SAY_RELIEF;
        s.clamp();
    }

    record_action(s, &action, &format!("intent：{why}"), weight, now_ms);

    Ok(IntentDone {
        seq: s.seq,
        action,
        why,
        say,
        weight,
    })
}

/// 记一次唤醒（过了闸门、真叫出去了才记）。
pub fn record_wake(s: &mut LifeState, action: &str, now_ms: i64) {
    s.roll_day(day_of(now_ms));
    s.gates.wakes += 1;
    s.gates.last_wake_ms = now_ms;
    s.gates.last_wake_action = action.to_string();
}

// ══════════════════════════════════════════════════════════════
//  唤醒：递给模型的那一小口（§10.6「喂给模型什么」）
// ══════════════════════════════════════════════════════════════

/// 一次唤醒递给模型的东西。**不是整个记忆库**——目标 800 token 以内。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeBrief {
    /// 唤醒编号。宿主拿它去重，模型回 intent 时可以带上
    pub id: String,
    pub kind: String,
    pub reasons: Vec<String>,
    pub value: f64,
    /// 规则本来会抽中的那个动作。模型不回 intent 就照这个走。
    pub action: String,
    pub top1: Option<(String, f64)>,
    pub top2: Option<(String, f64)>,
    /// 拼好的那段话，直接塞进提示词
    pub text: String,
}

/// 把「为什么叫你 + 现在什么样 + 最近干了什么 + 顺手想起来的事」拼成一小段字。
///
/// 纯函数：`mems` 由调用方从库里召回后塞进来（最多 6 条），核心不在这里碰 IO。
pub fn wake_brief(s: &LifeState, d: &WakeDecision, mems: &[String], now_ms: i64) -> WakeBrief {
    let mut t = String::new();

    t.push_str(&format!(
        "【为什么叫你】{} 类唤醒\n",
        if d.kind.is_empty() { "?" } else { d.kind.as_str() }
    ));
    for r in &d.reasons {
        t.push_str(&format!("· {r}\n"));
    }
    t.push_str(&format!("【值不值】{:.2}\n", d.value));
    if let Some((a, w1)) = &d.top1 {
        t.push_str(&format!("【规则本来想】{a} {w1:.2}"));
        if let Some((b, w2)) = &d.top2 {
            t.push_str(&format!(" ／ {b} {w2:.2}"));
        }
        t.push('\n');
    }
    t.push_str(&format!(
        "【此刻】心情 {} ｜ 精力 {:.2} 困 {:.2} 想说话 {:.2} 闷 {:.2} 好奇 {:.2} 歇够 {:.2}\n",
        s.mood(),
        s.energy,
        s.sleepy,
        s.social,
        s.boredom,
        s.curiosity,
        s.rest
    ));

    let n = s.recent.len();
    let tail = &s.recent[n.saturating_sub(5)..];
    if !tail.is_empty() {
        let line: Vec<String> = tail
            .iter()
            .map(|r| format!("{} {}", ago(now_ms - r.ts_ms), r.action))
            .collect();
        t.push_str(&format!("【最近】{}\n", line.join(" · ")));
    }

    if !mems.is_empty() {
        t.push_str("【顺手想起来的】\n");
        for m in mems.iter().take(6) {
            t.push_str(&format!("· {m}\n"));
        }
    }

    t.push_str(&format!(
        "【怎么回】回一个 intent：{{\"action\":\"…\",\"why\":\"为什么\",\"say\":\"想说的话（能不写就不写）\"}}\n\
         动作只能从这张表里挑：{}\n\
         不想动就什么都别回，规则层照走。\n",
        ACTIONS.join(" / ")
    ));

    let action = d
        .top1
        .as_ref()
        .map(|(a, _)| a.clone())
        .unwrap_or_else(|| "idle".to_string());

    WakeBrief {
        id: format!("w-{}-{now_ms}", s.seq),
        kind: d.kind.clone(),
        reasons: d.reasons.clone(),
        value: d.value,
        action,
        top1: d.top1.clone(),
        top2: d.top2.clone(),
        text: t,
    }
}

/// 多久以前，说人话。
fn ago(ms: i64) -> String {
    let ms = ms.max(0);
    let m = ms / 60_000;
    if m < 1 {
        "刚才".to_string()
    } else if m < 60 {
        format!("{m} 分钟前")
    } else if m < 60 * 24 {
        format!("{} 小时前", m / 60)
    } else {
        format!("{} 天前", m / (60 * 24))
    }
}

// ══════════════════════════════════════════════════════════════
//  被人搭话
// ══════════════════════════════════════════════════════════════

/// 一句搭话能把「想说话」那股劲泄掉多少。
pub const TOUCH_RELIEF: f64 = 0.35;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TouchDone {
    pub social: f64,
    pub boredom: f64,
    pub curiosity: f64,
    /// 本来绷着的那口「憋太久」，泄完之后还响不响
    pub still_lonely: bool,
}

/// 有人跟我说话。
///
/// P3 只让「我主动说出口」降 `social`，于是有人陪我聊了一整晚，
/// 核心还以为我憋了一整天、隔一会儿就喊一次「憋太久」。这条是补上的另一半。
///
/// 不进 `recent`：被搭话不是我干的活，混进动作统计会把「动作种类」「死磕」都算歪。
pub fn touch(s: &mut LifeState, now_ms: i64) -> TouchDone {
    s.roll_day(day_of(now_ms));
    s.social -= TOUCH_RELIEF;
    s.boredom -= 0.12;
    s.curiosity += 0.08;
    s.energy += 0.01;
    s.clamp();
    TouchDone {
        social: s.social,
        boredom: s.boredom,
        curiosity: s.curiosity,
        still_lonely: s.social > 0.85,
    }
}


// ══════════════════════════════════════════════════════════════
//  干跑
// ══════════════════════════════════════════════════════════════

/// 干跑的结果。**不是**「模拟出来的未来」，是「按现在这套规则推 24 小时会长什么样」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimReport {
    pub hours: f64,
    pub ticks: usize,
    /// 每个动作跑了几次
    pub counts: BTreeMap<String, usize>,
    /// 出现了几种不同的动作
    pub distinct: usize,
    /// 发呆 / 歇着的 tick 数（idle + pause + rest + nap）
    pub quiet_ticks: usize,
    /// 内容型动作的 tick 数（这些本该有产出）
    pub content_ticks: usize,
    /// 越线的次数，按种类
    pub alerts: BTreeMap<String, usize>,
    /// 同一个动作最长连着来了几次
    pub longest_run: usize,
    /// 末尾几个动作，肉眼看看像不像活的
    pub tail: Vec<String>,
}

/// 从当前状态快进 `hours` 小时。tick 间隔按 §10.2 的 `40 ± 15` 分钟随机。
///
/// 传进来的 `s` **不会被改**——它只是起点。
pub fn simulate<R: Rng + ?Sized>(
    s: &LifeState,
    start_ms: i64,
    hours: f64,
    rng: &mut R,
) -> SimReport {
    let mut cur = s.clone();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut alerts: BTreeMap<String, usize> = BTreeMap::new();
    let mut ticks = 0usize;
    let mut quiet = 0usize;
    let mut content = 0usize;
    let mut longest_run = 0usize;
    let mut run = 0usize;
    let mut tail: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;

    let end = start_ms + (hours * 3_600_000.0) as i64;
    let mut t = start_ms;
    while t < end {
        let out = step(&mut cur, t, None, rng);
        ticks += 1;
        *counts.entry(out.action.clone()).or_insert(0) += 1;
        if matches!(out.action.as_str(), "idle" | "pause" | "rest" | "nap") {
            quiet += 1;
        }
        if is_content(&out.action) {
            content += 1;
        }
        for a in &out.alerts {
            *alerts.entry(a.clone()).or_insert(0) += 1;
        }
        if prev.as_deref() == Some(out.action.as_str()) {
            run += 1;
            longest_run = longest_run.max(run);
        } else {
            run = 1;
        }
        prev = Some(out.action.clone());

        tail.push(out.action);
        if tail.len() > 12 {
            tail.remove(0);
        }

        let gap: f64 = (40.0 + rng.gen_range(-15.0..15.0)) * 60_000.0;
        t += gap.max(60_000.0) as i64;
    }

    SimReport {
        hours,
        ticks,
        distinct: counts.len(),
        counts,
        quiet_ticks: quiet,
        content_ticks: content,
        alerts,
        longest_run,
        tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn t0() -> i64 {
        1_700_000_000_000
    }

    #[test]
    fn tick_moves_state() {
        let mut s = LifeState::default();
        let mut rng = StdRng::seed_from_u64(42);
        let before = (s.seq, s.boredom);
        for i in 0..10 {
            step(&mut s, 1_700_000_000_000 + i * 60_000, None, &mut rng);
        }
        assert_eq!(s.seq, before.0 + 10);
        assert!(s.recent.len() == 10);
        // 十个 tick 之后状态一定不一样了
        assert!(s.energy != 0.72 || s.boredom != before.1 || s.curiosity != 0.55);
    }

    #[test]
    fn same_seed_same_path() {
        let mut a = LifeState::default();
        let mut b = LifeState::default();
        let mut ra = StdRng::seed_from_u64(7);
        let mut rb = StdRng::seed_from_u64(7);
        for i in 0..5 {
            let t = 1_700_000_000_000 + i * 60_000;
            assert_eq!(
                step(&mut a, t, None, &mut ra).action,
                step(&mut b, t, None, &mut rb).action
            );
        }
    }

    #[test]
    fn forced_action_is_respected() {
        let mut s = LifeState::default();
        let mut rng = StdRng::seed_from_u64(1);
        let out = step(&mut s, 1_700_000_000_000, Some("nap"), &mut rng);
        assert_eq!(out.action, "nap");
        assert_eq!(s.sleepy, 0.0);
    }

    #[test]
    fn cooldown_blocks_candidate() {
        let mut s = LifeState::default();
        s.energy = 0.2;
        let mut rng = StdRng::seed_from_u64(3);
        step(&mut s, 1_700_000_000_000, Some("nap"), &mut rng);
        // 刚睡完，冷却期内 nap 不该再进候选
        let mut s2 = s.clone();
        let mut rng2 = StdRng::seed_from_u64(3);
        let out = step(&mut s2, 1_700_000_000_000 + 60_000, None, &mut rng2);
        assert_ne!(out.action, "nap");
    }

    #[test]
    fn recent_is_capped() {
        let mut s = LifeState::default();
        let mut rng = StdRng::seed_from_u64(9);
        for i in 0..80 {
            step(&mut s, 1_700_000_000_000 + i * 60_000, None, &mut rng);
        }
        assert_eq!(s.recent.len(), RECENT_CAP);
    }

    // ── P3 ──────────────────────────────────────────────────

    #[test]
    fn gates_clear_on_a_new_day() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.gates.wakes = 5;
        s.gates.says = 3;
        // 隔一天
        s.roll_day(day_of(t + 24 * 3600 * 1000));
        assert_eq!(s.gates.wakes, 0);
        assert_eq!(s.gates.says, 0);
    }

    #[test]
    fn decide_wakes_when_energized_and_content() {
        // 精力足、好奇高 → study 会进候选且分数最高，C 类该叫
        let s = LifeState::default();
        let d = decide(&s, t0());
        assert!(d.value > 0.0, "{d:?}");
    }

    #[test]
    fn decide_respects_daily_budget() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.gates.wakes = WAKE_DAILY_MAX;
        let d = decide(&s, t);
        assert!(!d.wake, "{d:?}");
        assert!(
            d.blocked_by.iter().any(|b| b.contains("额度")),
            "{:?}",
            d.blocked_by
        );
    }

    #[test]
    fn decide_merges_a_call_just_made() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        record_wake(&mut s, "study", t);
        // 一分钟后又有理由：该合并，不另起一次
        let d = decide(&s, t + 60_000);
        assert!(!d.wake, "{d:?}");
        assert!(
            d.blocked_by.iter().any(|b| b.contains("合并")),
            "{:?}",
            d.blocked_by
        );
    }

    #[test]
    fn intent_is_rejected_when_precondition_fails() {
        let mut s = LifeState::default();
        s.energy = 0.9;
        let it = Intent {
            action: "rest".into(),
            ..Default::default()
        };
        match accept_intent(&mut s, &it, t0()) {
            Err(e) => assert_eq!(e.code(), "precondition_failed"),
            Ok(_) => panic!("精力这么足还想休息，应该被拒"),
        }
    }

    #[test]
    fn intent_rejects_unknown_action() {
        let mut s = LifeState::default();
        let it = Intent {
            action: "sleep_in".into(),
            ..Default::default()
        };
        match accept_intent(&mut s, &it, t0()) {
            Err(e) => assert_eq!(e.code(), "unknown_action"),
            Ok(_) => panic!("动作表里没有 sleep_in"),
        }
    }

    #[test]
    fn talking_too_soon_is_refused() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.gates.last_say_ms = t - 10 * 60_000;

        let it = Intent {
            action: "idle".into(),
            why: Some("想搭句话".into()),
            say: Some("在干嘛呀".into()),
            ..Default::default()
        };
        match accept_intent(&mut s, &it, t) {
            Err(e) => assert_eq!(e.code(), "talk_too_much"),
            Ok(_) => panic!("十分钟内说两次，该被拦住"),
        }
    }

    #[test]
    fn saying_out_loud_is_counted() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        let it = Intent {
            action: "pause".into(),
            why: Some("想搭句话".into()),
            say: Some("在干嘛呀".into()),
            ..Default::default()
        };
        let done = accept_intent(&mut s, &it, t).expect("第一次说话该放行");
        assert_eq!(done.say.as_deref(), Some("在干嘛呀"));
        assert_eq!(s.gates.says, 1);
        assert_eq!(s.gates.last_say_ms, t);
    }

    #[test]
    fn saying_out_loud_relieves_social() {
        // social 平时只涨，说出口才落。不然会一整天顶着 1.00 喊「憋太久」。
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.social = 0.90;
        let it = Intent {
            action: "pause".into(),
            why: Some("憋半天了".into()),
            say: Some("在干嘛呀".into()),
            ..Default::default()
        };
        accept_intent(&mut s, &it, t).expect("该放行");
        assert!((s.social - (0.90 - SAY_RELIEF)).abs() < 1e-9, "social 该落下去");
    }

    #[test]
    fn a_day_of_dry_run_is_not_monotonous() {
        let s = LifeState::default();
        let mut rng = StdRng::seed_from_u64(2026);
        let rep = simulate(&s, t0(), 24.0, &mut rng);
        // 24 小时按 40±15 分钟一次 → 20~60 个 tick 之间
        assert!(rep.ticks >= 20 && rep.ticks <= 60, "{rep:?}");
        assert!(rep.distinct >= 4, "一天只有 {} 种动作，太单调：{:?}", rep.distinct, rep.counts);
        assert!(rep.longest_run <= 8, "同一个动作连着来了 {} 次", rep.longest_run);
        assert!(rep.quiet_ticks > 0, "一次都没歇过不合理");
    }

    #[test]
    fn simulate_does_not_touch_the_input() {
        let s = LifeState::default();
        let before = s.seq;
        let mut rng = StdRng::seed_from_u64(11);
        let _ = simulate(&s, t0(), 6.0, &mut rng);
        assert_eq!(s.seq, before);
    }

    #[test]
    fn wake_brief_is_a_small_handful() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.social = 0.95;
        s.energy = 0.80;
        s.boredom = 0.20;
        let d = decide(&s, t);
        assert!(d.wake, "憋成这样该叫：{:?}", d.blocked_by);
        let b = wake_brief(&s, &d, &["她昨天说想去看海".to_string()], t);
        assert!(b.text.contains("B 类唤醒"), "{}", b.text);
        assert!(b.text.contains("憋太久"), "{}", b.text);
        assert!(b.text.contains("想说话"), "{}", b.text);
        assert!(b.text.contains("她昨天说想去看海"), "顺手想起来的不该丢");
        assert!(b.text.contains("idle / rest"), "动作表得在里面，不然模型编一个就白叫了");
        // 是「一小口」不是整个记忆库
        assert!(b.text.chars().count() < 900, "{} 字，太长了", b.text.chars().count());
        assert!(b.id.starts_with('w'), "{}", b.id);
    }

    #[test]
    fn wake_brief_caps_mems_at_six() {
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.social = 0.95;
        let d = decide(&s, t);
        let mems: Vec<String> = (0..20).map(|i| format!("记忆 {i}")).collect();
        let b = wake_brief(&s, &d, &mems, t);
        assert_eq!(b.text.matches("· 记忆").count(), 6, "只带 6 条");
    }

    #[test]
    fn touch_relieves_social_without_going_negative() {
        // P3 只让「我主动说出口」降 social，被人搭话这条一直没有。
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        s.social = 1.00;
        s.boredom = 0.50;
        let d = touch(&mut s, t);
        assert!((s.social - (1.00 - TOUCH_RELIEF)).abs() < 1e-9);
        assert!(d.still_lonely == false, "泄完就不该再喊憋太久");
        for _ in 0..10 {
            touch(&mut s, t);
        }
        assert!(s.social >= 0.0, "social 不能跑到负数");
        assert!(s.boredom >= 0.0);
        assert!(s.curiosity <= 1.0);
    }

    #[test]
    fn touch_does_not_pollute_the_action_log() {
        // 被搭话不是我干的活，混进 recent 会把「动作种类」「死磕」都算歪。
        let mut s = LifeState::default();
        let t = t0();
        s.roll_day(day_of(t));
        let n = s.recent.len();
        touch(&mut s, t);
        assert_eq!(s.recent.len(), n);
    }
}
