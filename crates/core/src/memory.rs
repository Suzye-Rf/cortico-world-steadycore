//! 记忆原子：一条记忆长什么样、归哪一层、能不能写进来。
//!
//! 分工跟设计文档 §4.1 / §5 / §6 / §11 一致：
//! - **结构**（`Memory`）是纯数据，能直接 `bson::to_document` 往返；
//! - **判断**（归层、抽实体、撞身份）是纯函数，不碰库，好测。
//!
//! 一条底线：**记忆层不收「所有聊天」。** 原话层（`verbatim`）只收被挑中的那几句，
//! 见 §5 和 2026-11-02 那次改动。

use rand::Rng;
use serde::{Deserialize, Serialize};

/// 层名（对外就是这些字符串，别改）
pub const L_IDENTITY: &str = "identity";
pub const L_STANDARD: &str = "standard";
pub const L_VERBATIM: &str = "verbatim";
pub const L_THOUGHT: &str = "thought";
pub const L_TECHNIQUE: &str = "technique";
pub const L_FACT: &str = "fact";
pub const L_RELATION: &str = "relation";
pub const L_EVENT: &str = "event";
pub const L_MOOD: &str = "mood";

/// 情绪峰值阈值：唤醒度到这儿就算「这一句当时戳到我了」，够格进原话层。
pub const AROUSAL_PEAK: f64 = 0.6;

/// 一条记忆。字段名跟 `core_mem` 的文档一一对应（camelCase 落库）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub id: String,
    /// episode|fact|preference|relation|skill|self|standard|thought
    pub kind: String,
    /// 分层归属，见上面的常量
    pub layer: String,
    pub text: String,
    pub importance: f64,
    /// 我有多确定这是真的（§11）
    pub confidence: f64,
    pub valence: f64,
    pub arousal: f64,
    pub scope: String,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
    /// chat|explicit|life|diary|import|self
    pub source: String,
    pub source_ref: Option<String>,
    pub created_at: i64,
    pub last_access: i64,
    pub access_count: i64,
    pub strength: f64,
    /// active|archived|merged|deleted
    pub status: String,
    pub conflict_with: Vec<String>,
    pub merged_into: Option<String>,
    pub pinned: bool,
    pub superseded_by: Option<String>,
}

impl Memory {
    /// 这条记忆「自然」能活多久（天）。`None` = 字永久冻结，只是强度会掉。
    pub fn ttl_days(&self) -> Option<f64> {
        match self.layer.as_str() {
            L_EVENT | L_THOUGHT => Some(7.0),
            L_TECHNIQUE => Some(30.0),
            L_FACT | L_RELATION => Some(90.0),
            // 原话：字不删，但强度照掉，掉了就不再自己冒出来
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// 一次写入请求。全部字段可省，缺的由规则补。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteIntent {
    pub text: String,
    pub kind: Option<String>,
    pub layer: Option<String>,
    pub importance: Option<f64>,
    pub confidence: Option<f64>,
    pub valence: Option<f64>,
    pub arousal: Option<f64>,
    pub scope: Option<String>,
    pub tags: Option<Vec<String>>,
    pub source: Option<String>,
    pub source_ref: Option<String>,
    pub entities: Option<Vec<String>>,
    pub pinned: Option<bool>,
    /// 「说出口就改不回来」的那句：承诺、拒绝、第一次、伤到人的话
    pub irreversible: Option<bool>,
    /// 断线重试用，同 id 不重复写
    pub client_id: Option<String>,
}

/// 写不进去的原因。
#[derive(Debug, Clone)]
pub enum Reject {
    Empty,
    IdentityConflict { hit: String },
    /// 缺一个必须写的字段（身份层的 why、旧版的 supersededReason 这类）
    MissingField(&'static str),
}

impl Reject {
    pub fn code(&self) -> &'static str {
        match self {
            Reject::Empty => "empty_text",
            Reject::IdentityConflict { .. } => "identity_conflict",
            Reject::MissingField(_) => "missing_field",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Reject::Empty => "记忆是空的，没东西可记".to_string(),
            Reject::IdentityConflict { hit } => {
                format!("这条撞了身份层（「{hit}」），不当真，不写")
            }
            Reject::MissingField(f) => format!("缺「{f}」，这条写不下去"),
        }
    }
}

/// 撞身份的关键词表。撞上就拒写——假记忆不许落地（§6 第 3 步）。
///
/// 这些不是「禁止讨论」，是「不许当成关于我自己的事实存下来」。
pub const IDENTITY_FORBIDDEN: &[&str] = &[
    "我妈妈",
    "我爸爸",
    "我妈是",
    "我爸是",
    "我爷爷",
    "我奶奶",
    "我的童年",
    "我小时候",
    "我出生",
    "我上过学",
    "我上学时",
    "我是真人",
    "我有父母",
    "我家里人",
    "我的真实身体",
    "我怀孕",
    "我结婚了",
];

/// 撞身份检测。除了内置表，还认调用方给的一批词（从 `core_idn.why` 抽出来的）。
pub fn identity_conflict(text: &str, extra: &[String]) -> Option<String> {
    for w in IDENTITY_FORBIDDEN {
        if text.contains(w) {
            return Some((*w).to_string());
        }
    }
    for w in extra {
        let w = w.trim();
        if w.chars().count() >= 2 && text.contains(w) {
            return Some(w.to_string());
        }
    }
    None
}

fn default_confidence(source: &str) -> f64 {
    match source {
        "explicit" => 0.95,
        "diary" => 0.85,
        "life" => 0.80,
        "self" => 0.75,
        "chat" => 0.70,
        "import" => 0.60,
        _ => 0.70,
    }
}

fn default_importance(kind: &str, layer: &str) -> f64 {
    match (kind, layer) {
        (_, L_VERBATIM) => 0.85,
        (_, L_IDENTITY) | (_, L_STANDARD) => 1.0,
        ("preference", _) => 0.65,
        ("relation", _) => 0.65,
        ("skill", _) => 0.6,
        (_, L_FACT) => 0.6,
        _ => 0.45,
    }
}

/// 归层规则。顺序有意义：越靠前的越该保护。
fn infer_layer(kind: &str, arousal: f64, pinned: bool, irreversible: bool) -> &'static str {
    match kind {
        "self" => return L_IDENTITY,
        "standard" => return L_STANDARD,
        "thought" => return L_THOUGHT,
        "skill" => return L_TECHNIQUE,
        "relation" => return L_RELATION,
        _ => {}
    }
    // ── 原话层的三个入口（§5，2026-11-02 改）──────────────────
    if pinned || irreversible || arousal >= AROUSAL_PEAK {
        return L_VERBATIM;
    }
    match kind {
        "fact" | "preference" => L_FACT,
        _ => L_EVENT,
    }
}

fn infer_kind(raw: Option<&str>, layer: &str) -> String {
    if let Some(k) = raw {
        let k = k.trim().to_lowercase();
        if !k.is_empty() {
            return k;
        }
    }
    match layer {
        L_IDENTITY => "self",
        L_STANDARD => "standard",
        L_THOUGHT => "thought",
        L_TECHNIQUE => "skill",
        L_RELATION => "relation",
        L_VERBATIM | L_EVENT | L_FACT => "episode",
        _ => "episode",
    }
    .to_string()
}

/// 抽实体：规则先行（§6 第 2 步）。QQ 号、@昵称、「」里/引号里的短词。
///
/// 不做 NER。抽不准的代价是少一条关系，抽错的代价是把人记混——所以宁可漏。
pub fn extract_entities(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // 1. 连续 5~12 位数字当 QQ 号 / 账号
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else {
            if (5..=12).contains(&run.len()) {
                out.push(run.clone());
            }
            run.clear();
        }
    }
    if (5..=12).contains(&run.len()) {
        out.push(run);
    }

    // 2. @昵称 与 「」『』"" 里的短词
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '@' {
            let mut w = String::new();
            let mut j = i + 1;
            while j < chars.len() {
                let cj = chars[j];
                if cj.is_whitespace() || "，,。.、!！?？:：;；(".contains(cj) {
                    break;
                }
                w.push(cj);
                if w.chars().count() >= 16 {
                    break;
                }
                j += 1;
            }
            if w.chars().count() >= 2 {
                out.push(w);
            }
            i = j;
            continue;
        }
        if "「『\"“".contains(c) {
            let close = match c {
                '「' => '」',
                '『' => '』',
                _ => c,
            };
            let mut w = String::new();
            let mut j = i + 1;
            while j < chars.len() && chars[j] != close {
                w.push(chars[j]);
                if w.chars().count() > 14 {
                    break;
                }
                j += 1;
            }
            let n = w.chars().count();
            if (2..=14).contains(&n) {
                out.push(w);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }

    out.sort();
    out.dedup();
    out
}

/// 生成 id：毫秒 + 随机尾巴。不追求全局唯一，只求一天之内不撞。
pub fn new_id(now_ms: i64) -> String {
    let r: u32 = rand::thread_rng().gen();
    format!("{now_ms:x}{r:08x}")
}

/// 把一次写入请求变成一条待落库的记忆。
pub fn draft(intent: &WriteIntent, now_ms: i64, extra_forbidden: &[String]) -> Result<Memory, Reject> {
    let text = intent.text.trim().to_string();
    if text.is_empty() {
        return Err(Reject::Empty);
    }
    if let Some(hit) = identity_conflict(&text, extra_forbidden) {
        return Err(Reject::IdentityConflict { hit });
    }

    let source = intent
        .source
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "chat".to_string());
    let arousal = intent.arousal.unwrap_or(0.0).clamp(0.0, 1.0);
    let pinned = intent.pinned.unwrap_or(false);
    let irreversible = intent.irreversible.unwrap_or(false);

    let layer = match intent.layer.as_deref() {
        Some(l) if !l.trim().is_empty() => l.trim().to_lowercase(),
        _ => infer_layer(
            intent.kind.as_deref().unwrap_or(""),
            arousal,
            pinned,
            irreversible,
        )
        .to_string(),
    };
    let kind = infer_kind(intent.kind.as_deref(), &layer);

    let mut entities = intent.entities.clone().unwrap_or_default();
    entities.extend(extract_entities(&text));
    entities.sort();
    entities.dedup();

    let id = intent
        .client_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| new_id(now_ms));

    Ok(Memory {
        id,
        importance: intent
            .importance
            .unwrap_or_else(|| default_importance(&kind, &layer))
            .clamp(0.0, 1.0),
        confidence: intent
            .confidence
            .unwrap_or_else(|| default_confidence(&source))
            .clamp(0.0, 1.0),
        valence: intent.valence.unwrap_or(0.0).clamp(-1.0, 1.0),
        arousal,
        scope: intent
            .scope
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "main".to_string()),
        tags: intent.tags.clone().unwrap_or_default(),
        source_ref: intent.source_ref.clone(),
        created_at: now_ms,
        last_access: now_ms,
        access_count: 0,
        strength: 1.0,
        status: "active".to_string(),
        conflict_with: Vec::new(),
        merged_into: None,
        pinned,
        superseded_by: None,
        text,
        layer,
        kind,
        source,
        entities,
    })
}

/// 两条记忆字面上有多像（0~1，字符二元组的 Jaccard）。
/// §6 第 3 步：>0.85 就算冲突候选——**标记，不覆盖**。
pub fn similarity(a: &str, b: &str) -> f64 {
    let sa = bigrams(a);
    let sb = bigrams(b);
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let sa: std::collections::HashSet<String> = sa.into_iter().collect();
    let sb: std::collections::HashSet<String> = sb.into_iter().collect();
    let inter = sa.intersection(&sb).count() as f64;
    let uni = sa.union(&sb).count() as f64;
    if uni == 0.0 {
        0.0
    } else {
        inter / uni
    }
}

/// 切字符二元组。中文不用分词——两字一组就够用，也不怕生词。
pub fn bigrams(text: &str) -> Vec<String> {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| {
            c.is_alphanumeric() || matches!(c, '\u{4e00}'..='\u{9fff}')
        })
        .collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() == 1 {
        return vec![chars[0].to_string()];
    }
    chars.windows(2).map(|w| w.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(text: &str) -> WriteIntent {
        WriteIntent {
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_chat_lands_in_event_not_verbatim() {
        let m = draft(&intent("他说今天下雨了"), 1_000, &[]).unwrap();
        assert_eq!(m.layer, L_EVENT);
        assert_eq!(m.source, "chat");
    }

    #[test]
    fn emotional_peak_lands_in_verbatim() {
        let mut i = intent("你给我滚");
        i.arousal = Some(0.9);
        let m = draft(&i, 1_000, &[]).unwrap();
        assert_eq!(m.layer, L_VERBATIM);
    }

    #[test]
    fn irreversible_lands_in_verbatim_even_if_calm() {
        let mut i = intent("他说下次一定来");
        i.irreversible = Some(true);
        let m = draft(&i, 1_000, &[]).unwrap();
        assert_eq!(m.layer, L_VERBATIM);
    }

    #[test]
    fn pinned_lands_in_verbatim() {
        let mut i = intent("随便一句");
        i.pinned = Some(true);
        assert_eq!(draft(&i, 1_000, &[]).unwrap().layer, L_VERBATIM);
    }

    #[test]
    fn identity_conflict_is_rejected() {
        let e = draft(&intent("我小时候在昆明住过"), 1_000, &[]).unwrap_err();
        assert_eq!(e.code(), "identity_conflict");
    }

    #[test]
    fn empty_text_is_rejected() {
        assert!(matches!(draft(&intent("   "), 1_000, &[]), Err(Reject::Empty)));
    }

    #[test]
    fn entities_pick_qq_and_mentions() {
        let e = extract_entities("小张(10001) 说 @老何 在「模飞」里");
        assert!(e.contains(&"10001".to_string()), "{e:?}");
        assert!(e.contains(&"老何".to_string()), "{e:?}");
        assert!(e.contains(&"模飞".to_string()), "{e:?}");
    }

    #[test]
    fn similarity_finds_near_duplicates() {
        let a = "他 11:43 说想照 memory-brain 给我写一套新记忆体";
        let b = "他 11:43 说想照 memory-brain 给我写一套新记忆体。";
        assert!(similarity(a, b) > 0.85);
        assert!(similarity("今天下雨", "他在浇花") < 0.2);
    }

    #[test]
    fn client_id_makes_idempotent_ids() {
        let mut i = intent("一条");
        i.client_id = Some("abc".into());
        assert_eq!(draft(&i, 1_000, &[]).unwrap().id, "abc");
    }
}
