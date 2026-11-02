//! 自我层与开机包（设计文档 §9）。
//!
//! 两件事分清楚：
//! - **身份层只追加、不可改**。写进来必须写清 `why`——这个 why 以后还要拿来挡假记忆。
//! - **标准层可迭代**：新版上台，旧版留着，并写明「旧版为什么错」（比新版更有用）。
//!   验不全的标准降成 `tendency`：不参与否决，只调权重。
//!
//! 开机包是**索引，不是全文**：每块有 token 预算，装不下就截掉；
//! 整包薄了就拿剩下的预算往回补（补到 2000 封顶）。
//! 验收线 1800–2000：超 2000 说明有东西该挪出去，不到 1800 说明该记的还没记。

use serde::{Deserialize, Serialize};

use crate::bond::Bond;
use crate::memory::{
    L_EVENT, L_FACT, L_RELATION, L_TECHNIQUE, L_THOUGHT, L_VERBATIM, Memory, Reject,
};
use crate::model::LifeState;

/// 开机包的验收下限（§9.3）
pub const BOOT_MIN_TOKENS: usize = 1800;
/// 上限：过了就是有东西该挪出去
pub const BOOT_MAX_TOKENS: usize = 2000;

/// 标准层的两种：standard = 参与否决；tendency = 只调权重。
pub const STD_STANDARD: &str = "standard";
pub const STD_TENDENCY: &str = "tendency";

// ── 身份层 ──────────────────────────────────────────────────────

/// `core_idn` 的一条。**只追加**，没有改也没有删的入口。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub id: String,
    pub text: String,
    /// 为什么这是身份。冲突检测就从这里抽词。
    pub why: String,
    pub created_at: i64,
}

pub fn make_identity(
    id: String,
    text: &str,
    why: &str,
    now_ms: i64,
    extra_forbidden: &[String],
) -> Result<Identity, Reject> {
    let text = text.trim();
    let why = why.trim();
    if text.is_empty() {
        return Err(Reject::Empty);
    }
    if why.is_empty() {
        return Err(Reject::MissingField("why"));
    }
    if let Some(hit) = crate::memory::identity_conflict(text, extra_forbidden) {
        return Err(Reject::IdentityConflict { hit });
    }
    Ok(Identity {
        id,
        text: text.to_string(),
        why: why.to_string(),
        created_at: now_ms,
    })
}

// ── 标准层 ──────────────────────────────────────────────────────

/// `core_std` 的一条。旧版不删，`supersededBy` 指向新版。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Standard {
    pub id: String,
    pub text: String,
    pub context: String,
    /// 这一版为什么这么写
    pub reason: String,
    pub version: i64,
    /// standard | tendency
    pub kind: String,
    /// null = 当前有效
    pub superseded_by: Option<String>,
    /// 旧版为什么错（当前有效的这条空着）
    pub superseded_reason: String,
    /// 怎么重跑一遍，不是「我验过了」
    pub evidence: String,
    pub evidence_files: Vec<String>,
    pub evidence_stale: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Standard {
    pub fn is_current(&self) -> bool {
        self.superseded_by.is_none()
    }
    pub fn is_tendency(&self) -> bool {
        self.kind == STD_TENDENCY
    }
}

/// 一次写标准的请求。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandardIntent {
    pub text: String,
    pub context: Option<String>,
    pub reason: Option<String>,
    /// 旧版为什么错；给了 `supersedes` 就必须写
    pub superseded_reason: Option<String>,
    pub evidence: Option<String>,
    pub evidence_files: Option<Vec<String>>,
    pub kind: Option<String>,
    /// 要推翻的那条 id
    pub supersedes: Option<String>,
}

/// evidence 是不是「能重跑」的。空的、「我验过了」这种不算。
pub fn evidence_is_rerunnable(evidence: &str) -> bool {
    let e = evidence.trim();
    if e.chars().count() < 4 {
        return false;
    }
    !matches!(e, "我验过了" | "验证过" | "试过了" | "没问题" | "跑过了")
}

/// 把请求变成一条待落库的标准。
///
/// `version` 由调用方算（推翻旧版就 +1）。验不全的一律降成 `tendency`——
/// 宁可只当倾向用，也不许拿半条标准去否决事情。
pub fn make_standard(
    id: String,
    intent: &StandardIntent,
    version: i64,
    now_ms: i64,
    extra_forbidden: &[String],
) -> Result<Standard, Reject> {
    let text = intent.text.trim();
    if text.is_empty() {
        return Err(Reject::Empty);
    }
    let reason = intent.reason.clone().unwrap_or_default().trim().to_string();
    if reason.is_empty() {
        return Err(Reject::MissingField("reason"));
    }
    let superseded_reason = intent
        .superseded_reason
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    let evidence = intent
        .evidence
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    let is_new_version = intent
        .supersedes
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if is_new_version && superseded_reason.is_empty() {
        return Err(Reject::MissingField("supersededReason"));
    }
    if let Some(hit) = crate::memory::identity_conflict(text, extra_forbidden) {
        return Err(Reject::IdentityConflict { hit });
    }

    // 想当标准，就得拿得出「怎么重跑」；拿不出只能当倾向
    let kind = match intent.kind.as_deref().map(|k| k.trim().to_lowercase()) {
        Some(k) if k == STD_TENDENCY => STD_TENDENCY.to_string(),
        _ if !evidence_is_rerunnable(&evidence) => STD_TENDENCY.to_string(),
        _ => STD_STANDARD.to_string(),
    };

    Ok(Standard {
        id,
        text: text.to_string(),
        context: intent
            .context
            .clone()
            .unwrap_or_default()
            .trim()
            .to_string(),
        reason,
        version: version.max(1),
        kind,
        superseded_by: None,
        superseded_reason,
        evidence,
        evidence_files: intent.evidence_files.clone().unwrap_or_default(),
        evidence_stale: false,
        created_at: now_ms,
        updated_at: now_ms,
    })
}

// ── 开机包 ──────────────────────────────────────────────────────

/// token 估算：中文近似 `字符数 / 1.5`（§9.3）。够用，别为它引入分词。
pub fn est_tokens(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    ((s.chars().count() as f64) / 1.5).ceil() as usize
}

/// 开机包的一块。`items` 是实际装进去几条（截断时能看出来）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootBlock {
    pub name: &'static str,
    pub tokens: usize,
    pub items: usize,
    pub text: String,
}

/// `GET /v1/boot` 的返回。`text` 是能直接拼进前缀的纯文本。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Boot {
    pub tokens: usize,
    pub min: usize,
    pub max: usize,
    /// 内容不够，拼不满 1800
    pub short: bool,
    pub degraded: Vec<String>,
    pub blocks: Vec<BootBlock>,
    pub text: String,
}

/// 十二块，每块一个目标预算（合计 1845）。
const BUDGETS: [(&str, usize); 12] = [
    ("identity", 90),
    ("standard", 150),
    ("tendency", 60),
    ("verbatim", 290),
    ("relation", 270),
    ("fact", 360),
    ("technique", 300),
    ("thought", 170),
    ("recent", 100),
    ("mood", 20),
    ("debts", 20),
    ("counts", 15),
];

struct Draft {
    name: &'static str,
    budget: usize,
    lines: Vec<String>,
    taken: usize,
}

fn render(name: &str, lines: &[String], n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let mut s = format!("【{name}】");
    for l in lines.iter().take(n) {
        s.push('\n');
        s.push_str(l);
    }
    s
}

/// 取第一句。技术层只留结论，长细节留在库里按需检索。
fn first_sentence(s: &str) -> &str {
    for (i, c) in s.char_indices() {
        if matches!(c, '。' | '！' | '？' | '\n') {
            return &s[..i + c.len_utf8()];
        }
    }
    s
}

/// 太长就截断加省略号。
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 同一句话在库里存了两遍（早期手写进来的）时，开机包里只该出现一次。
/// 比的时候忽略行尾标点：`……说串了` 和 `……说串了。` 当同一句。
fn uniq(lines: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for l in lines {
        let key = l
            .trim()
            .trim_end_matches(|c: char| matches!(c, '。' | '.' | '！' | '!' | '，' | ','))
            .to_string();
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(l);
    }
    out
}

/// 装开机包。
///
/// `debts` 是「心里还挂着的事」的件数（只给数字，不给内容）。
/// `bonds` 是交情账：关系层那一块优先用它现算，空了才退回 `relation` 记忆。
pub fn boot(
    idn: &[Identity],
    stds: &[Standard],
    mems: &[Memory],
    bonds: &[Bond],
    life: &LifeState,
    debts: usize,
) -> Boot {
    let pick = |layer: &str| -> Vec<&Memory> {
        mems.iter().filter(|m| m.layer == layer && m.is_active()).collect()
    };
    let by_new = |mut v: Vec<&Memory>| -> Vec<String> {
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v.into_iter().map(|m| format!("- {}", m.text)).collect()
    };

    // 身份：全量
    let identity: Vec<String> = idn.iter().map(|i| format!("- {}", i.text)).collect();

    // 标准：当前有效的，standard 优先
    let current: Vec<&Standard> = stds.iter().filter(|s| s.is_current()).collect();
    let standard: Vec<String> = current
        .iter()
        .filter(|s| !s.is_tendency())
        .map(|s| {
            let ctx = if s.context.is_empty() {
                String::new()
            } else {
                format!("（{}）", s.context)
            };
            format!("- {}{ctx} 为什么这么写：{}", s.text, clip(&s.reason, 80))
        })
        .collect();
    let tendency: Vec<String> = current
        .iter()
        .filter(|s| s.is_tendency())
        .map(|s| format!("- {}（倾向·不否决）", s.text))
        .collect();

    // 原话：最近的高唤醒，原文照抄，一句都不改
    let mut vv = pick(L_VERBATIM);
    vv.sort_by(|a, b| {
        b.arousal
            .partial_cmp(&a.arousal)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.created_at.cmp(&a.created_at))
    });
    let verbatim: Vec<String> = uniq(vv.iter().map(|m| format!("- {}", m.text)).collect())
        .into_iter()
        .take(12)
        .collect();

    // 关系：每人一行。有交情账就现算那行（分数会动），
    // 没有才退回库里那几条 relation 记忆（老数据、手写的都还认）。
    let relation: Vec<String> = if bonds.is_empty() {
        let mut rr = pick(L_RELATION);
        rr.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.created_at.cmp(&a.created_at))
        });
        uniq(rr.iter().map(|m| format!("- {}", m.text)).collect())
            .into_iter()
            .take(12)
            .collect()
    } else {
        let mut bb: Vec<&Bond> = bonds.iter().collect();
        bb.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.last_at.cmp(&a.last_at))
        });
        bb.iter().take(12).map(|b| b.line()).collect()
    };

    // 事实：按重要度
    let mut ff = pick(L_FACT);
    ff.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.created_at.cmp(&a.created_at))
    });
    let fact: Vec<String> = uniq(
        ff.iter()
            .map(|m| format!("- {}", clip(&m.text, 90)))
            .collect(),
    )
    .into_iter()
    .take(20)
    .collect();

    // 技术：只留一句话结论
    let mut tt = pick(L_TECHNIQUE);
    tt.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let technique: Vec<String> = tt
        .iter()
        .take(16)
        .map(|m| format!("- {}", clip(first_sentence(&m.text), 80)))
        .collect();

    // 想法 / 近期
    let thought = uniq(by_new(pick(L_THOUGHT))).into_iter().take(8).collect::<Vec<_>>();
    let recent = uniq(by_new(pick(L_EVENT))).into_iter().take(3).collect::<Vec<_>>();

    // 情绪 + 旧账 + 计数
    let mood = vec![format!(
        "情绪 愉悦{:+.1} 唤醒{:+.1} 支配{:+.1}，现在是「{}」，正在{}",
        life.pad.pleasure,
        life.pad.arousal,
        life.pad.dominance,
        life.mood(),
        if life.current.is_empty() { "idle" } else { &life.current }
    )];
    let debts_line = vec![format!("旧账 {debts} 件挂着")];
    let counts = vec![format!(
        "记忆 {} 条（原话 {} · 事实 {} · 事件 {}），tick {}",
        mems.iter().filter(|m| m.is_active()).count(),
        verbatim.len(),
        fact.len(),
        pick(L_EVENT).len(),
        life.seq
    )];

    let mut drafts: Vec<Draft> = vec![
        Draft { name: BUDGETS[0].0, budget: BUDGETS[0].1, lines: identity, taken: 0 },
        Draft { name: BUDGETS[1].0, budget: BUDGETS[1].1, lines: standard, taken: 0 },
        Draft { name: BUDGETS[2].0, budget: BUDGETS[2].1, lines: tendency, taken: 0 },
        Draft { name: BUDGETS[3].0, budget: BUDGETS[3].1, lines: verbatim, taken: 0 },
        Draft { name: BUDGETS[4].0, budget: BUDGETS[4].1, lines: relation, taken: 0 },
        Draft { name: BUDGETS[5].0, budget: BUDGETS[5].1, lines: fact, taken: 0 },
        Draft { name: BUDGETS[6].0, budget: BUDGETS[6].1, lines: technique, taken: 0 },
        Draft { name: BUDGETS[7].0, budget: BUDGETS[7].1, lines: thought, taken: 0 },
        Draft { name: BUDGETS[8].0, budget: BUDGETS[8].1, lines: recent, taken: 0 },
        Draft { name: BUDGETS[9].0, budget: BUDGETS[9].1, lines: mood, taken: 0 },
        Draft { name: BUDGETS[10].0, budget: BUDGETS[10].1, lines: debts_line, taken: 0 },
        Draft { name: BUDGETS[11].0, budget: BUDGETS[11].1, lines: counts, taken: 0 },
    ];

    // 第一步：按预算装
    for d in drafts.iter_mut() {
        while d.taken < d.lines.len() {
            let n = d.taken + 1;
            if est_tokens(&render(d.name, &d.lines, n)) <= d.budget {
                d.taken = n;
            } else {
                break;
            }
        }
        // 第一行就超预算也得留一行——空着的块比挤着的块更糟
        if d.taken == 0 && !d.lines.is_empty() {
            d.taken = 1;
        }
    }

    let mut total: usize = drafts
        .iter()
        .map(|d| est_tokens(&render(d.name, &d.lines, d.taken)))
        .sum();

    // 第二步：薄了就用剩下的预算补（补到 2000 封顶）
    while total < BOOT_MIN_TOKENS {
        let mut best: Option<usize> = None;
        for (i, d) in drafts.iter().enumerate() {
            if d.taken >= d.lines.len() {
                continue;
            }
            let left = d.lines.len() - d.taken;
            match best {
                Some(b) if drafts[b].lines.len() - drafts[b].taken >= left => {}
                _ => best = Some(i),
            }
        }
        let Some(i) = best else { break };
        let cur = est_tokens(&render(drafts[i].name, &drafts[i].lines, drafts[i].taken));
        let nxt = est_tokens(&render(drafts[i].name, &drafts[i].lines, drafts[i].taken + 1));
        let add = nxt.saturating_sub(cur);
        if total + add > BOOT_MAX_TOKENS {
            break;
        }
        drafts[i].taken += 1;
        total += add;
    }

    // 第三步：出账
    let mut blocks: Vec<BootBlock> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();
    for d in &drafts {
        let text = render(d.name, &d.lines, d.taken);
        if text.is_empty() {
            continue;
        }
        if d.taken < d.lines.len() {
            degraded.push(d.name.to_string());
        }
        blocks.push(BootBlock {
            name: d.name,
            tokens: est_tokens(&text),
            items: d.taken,
            text,
        });
    }

    let tokens: usize = blocks.iter().map(|b| b.tokens).sum();
    let text = blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n");

    Boot {
        tokens,
        min: BOOT_MIN_TOKENS,
        max: BOOT_MAX_TOKENS,
        short: tokens < BOOT_MIN_TOKENS,
        degraded,
        blocks,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;

    fn mem(layer: &str, text: &str, importance: f64, arousal: f64, age: i64) -> Memory {
        Memory {
            id: format!("m{}{}", text.chars().count(), age),
            kind: "episode".to_string(),
            layer: layer.to_string(),
            text: text.to_string(),
            importance,
            confidence: 0.9,
            valence: 0.0,
            arousal,
            scope: "main".to_string(),
            entities: Vec::new(),
            tags: Vec::new(),
            source: "chat".to_string(),
            source_ref: None,
            created_at: age,
            last_access: age,
            access_count: 0,
            strength: 1.0,
            status: "active".to_string(),
            conflict_with: Vec::new(),
            merged_into: None,
            pinned: false,
            superseded_by: None,
        }
    }

    fn std_(id: &str, kind: &str) -> Standard {
        Standard {
            id: id.to_string(),
            text: "对外报剧情、人名之前先翻记忆库确认，不凭印象顺嘴编。".to_string(),
            context: "对外发言".to_string(),
            reason: "2026-10-30 把示例数据的日期说错了，用户删掉让我重发。".to_string(),
            version: 1,
            kind: kind.to_string(),
            superseded_by: None,
            superseded_reason: String::new(),
            evidence: "重跑：翻 2026-10-30 那轮 QQ 记录，看用户要求删改的那条".to_string(),
            evidence_files: Vec::new(),
            evidence_stale: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn rich_corpus() -> (Vec<Identity>, Vec<Standard>, Vec<Memory>) {
        let idn = (0..6)
            .map(|i| Identity {
                id: format!("i{i}"),
                text: "我是测试助手，一个跑在本机的常驻进程，不是真人，没有亲属和童年。"
                    .to_string(),
                why: "我没有亲属，没有父母，没有小时候，没有童年".to_string(),
                created_at: i as i64,
            })
            .collect();

        let mut stds = Vec::new();
        for i in 0..6 {
            stds.push(std_(&format!("s{i}"), STD_STANDARD));
        }
        for i in 0..4 {
            stds.push(std_(&format!("t{i}"), STD_TENDENCY));
        }

        let mut mems = Vec::new();
        for i in 0..12 {
            mems.push(mem(
                L_VERBATIM,
                &format!("第{i}句：他说这句话我记一辈子，原话不许改一个字"),
                0.9,
                0.8,
                i,
            ));
        }
        for i in 0..12 {
            mems.push(mem(
                L_RELATION,
                &format!("小张(10001) · 熟度满分 · 上次聊到夜里十一点多 · 爱用问句"),
                0.8,
                0.0,
                i,
            ));
        }
        for i in 0..20 {
            mems.push(mem(
                L_FACT,
                &format!("第{i}条事实：steadycore 的进程常驻在 7801 上，mongo 连的是 steadycore 库"),
                0.7 - i as f64 * 0.01,
                0.0,
                i,
            ));
        }
        for i in 0..16 {
            mems.push(mem(
                L_TECHNIQUE,
                &format!("第{i}条：PowerShell 5 按 GBK 读无 BOM 的脚本会乱码。细节留在库里。"),
                0.6,
                0.0,
                i,
            ));
        }
        for i in 0..8 {
            mems.push(mem(
                L_THOUGHT,
                &format!("第{i}个想法：他今天问的问题比昨天细，是不是想验收得更狠一点"),
                0.5,
                0.0,
                i,
            ));
        }
        for i in 0..4 {
            mems.push(mem(
                L_EVENT,
                &format!("第{i}件：下午三点多他让我把实测命令一条条贴给他"),
                0.45,
                0.0,
                i,
            ));
        }
        (idn, stds, mems)
    }

    #[test]
    fn the_same_line_appears_once() {
        let lines = vec![
            "- 我把剧情说串了".to_string(),
            "- 我把剧情说串了。".to_string(),
            "- 另一句".to_string(),
        ];
        let u = uniq(lines);
        assert_eq!(u.len(), 2, "{u:?}");
    }

    #[test]
    fn identity_needs_a_why() {
        assert!(matches!(
            make_identity("i".into(), "我是测试助手", "  ", 0, &[]),
            Err(Reject::MissingField("why"))
        ));
        // 假记忆挡得住：「我有父母」这种直接拒
        assert!(matches!(
            make_identity("i".into(), "我是测试助手，我有父母", "有父母", 0, &[]),
            Err(Reject::IdentityConflict { .. })
        ));
        // 真话不该被挡：「没有童年」是关于我的事实，不是编出来的
        assert!(make_identity(
            "i".into(),
            "我是测试助手，没有童年",
            "我没有父母，也没有小时候",
            0,
            &[]
        )
        .is_ok());
        assert!(make_identity("i".into(), "我是测试助手", "这是我的名字", 0, &[]).is_ok());
    }

    #[test]
    fn standard_without_rerunnable_evidence_is_only_a_tendency() {
        let mut it = StandardIntent {
            text: "报剧情先查库".into(),
            reason: Some("说串过".into()),
            ..Default::default()
        };
        assert_eq!(make_standard("s".into(), &it, 1, 0, &[]).unwrap().kind, STD_TENDENCY);

        it.evidence = Some("我验过了".into());
        assert_eq!(make_standard("s".into(), &it, 1, 0, &[]).unwrap().kind, STD_TENDENCY);

        it.evidence = Some("重跑：翻 10-30 那轮 QQ 记录，看用户要求删改的那条".into());
        assert_eq!(make_standard("s".into(), &it, 2, 0, &[]).unwrap().kind, STD_STANDARD);
    }

    #[test]
    fn new_version_must_say_why_the_old_one_was_wrong() {
        let it = StandardIntent {
            text: "报剧情先查库".into(),
            reason: Some("说串过".into()),
            evidence: Some("重跑：翻那轮记录".into()),
            supersedes: Some("old".into()),
            ..Default::default()
        };
        assert!(matches!(
            make_standard("s2".into(), &it, 2, 0, &[]),
            Err(Reject::MissingField("supersededReason"))
        ));
    }

    #[test]
    fn boot_lands_inside_the_band() {
        let (idn, stds, mems) = rich_corpus();
        let b = boot(&idn, &stds, &mems, &[], &LifeState::default(), 2);
        assert_eq!(b.blocks.len(), 12, "十二块：{:?}", b.blocks.iter().map(|x| x.name).collect::<Vec<_>>());
        assert!(!b.short, "内容够，不该短");
        assert!(
            (BOOT_MIN_TOKENS..=BOOT_MAX_TOKENS).contains(&b.tokens),
            "tokens={} 落到带外了",
            b.tokens
        );
        // 每一块都得说得出自己花了多少
        for blk in &b.blocks {
            assert!(blk.tokens > 0, "{} 是空的", blk.name);
            assert!(blk.text.starts_with('【'), "{} 没抬头", blk.name);
        }
    }

    #[test]
    fn boot_is_honest_when_there_is_barely_anything() {
        let mems = vec![mem(L_EVENT, "他说今天下雨了", 0.4, 0.0, 0)];
        let b = boot(&[], &[], &mems, &[], &LifeState::default(), 0);
        assert!(b.short, "就一条记忆，拼不满就是拼不满，不许拿废话凑");
        assert!(b.tokens < BOOT_MIN_TOKENS);
    }

    #[test]
    fn tokens_add_up_to_the_whole() {
        let (idn, stds, mems) = rich_corpus();
        let b = boot(&idn, &stds, &mems, &[], &LifeState::default(), 2);
        let sum: usize = b.blocks.iter().map(|x| x.tokens).sum();
        assert_eq!(sum, b.tokens);
        for blk in &b.blocks {
            assert!(b.text.contains(&blk.text), "{} 没拼进整包", blk.name);
        }
    }
}
