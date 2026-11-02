//! 检索与打分：为什么想起这一条。
//!
//! 设计文档 §7 的融合打分，全部是纯函数——喂进来一堆记忆和一句问话，
//! 出排好序的候选。库有多大不影响这里：几千条的量级，内存里算完再取前 k 条。
//!
//! **一条硬要求：每一条结果都要说得出「为什么是它」。**
//! 说不清为什么想起这件事，就等于没有记忆。

use crate::memory::{extract_entities, Memory};

/// 召回窗口：先按关键词命中的条数上限（防止最坏情况全库打分）。
pub const CANDIDATE_CAP: usize = 300;

/// 打分各项权重（§7.2）
pub const W_KW: f64 = 0.42;
pub const W_GRAPH: f64 = 0.18;
pub const W_IMPORTANCE: f64 = 0.16;
pub const W_FRESH: f64 = 0.12;
pub const W_MOOD: f64 = 0.12;

/// 新鲜度半衰：14 天掉到 1/e
pub const FRESH_DAYS: f64 = 14.0;

/// 一条命中。
#[derive(Debug, Clone)]
pub struct Hit {
    /// 在传入切片里的下标
    pub index: usize,
    pub score: f64,
    /// 人话解释，给 `why` 字段用
    pub why: String,
    pub matched_terms: Vec<String>,
    pub shared_entities: Vec<String>,
}

/// 过滤条件。全部可空，空了就是不过滤。
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub scope: Option<String>,
    pub layer: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub min_confidence: Option<f64>,
}

impl Filter {
    pub fn keep(&self, m: &Memory) -> bool {
        if let Some(s) = &self.scope {
            if &m.scope != s {
                return false;
            }
        }
        if let Some(l) = &self.layer {
            if &m.layer != l {
                return false;
            }
        }
        if let Some(t) = self.since_ms {
            if m.created_at < t {
                return false;
            }
        }
        if let Some(t) = self.until_ms {
            if m.created_at > t {
                return false;
            }
        }
        if let Some(c) = self.min_confidence {
            if m.confidence < c {
                return false;
            }
        }
        true
    }
}

/// 打分。`pleasure` 是当前情绪的愉悦度（-1~1），只用来给「同号」的旧事加一点。
///
/// 返回按分数从高到低排好、长度不超过 `k` 的结果。
pub fn rank(
    query: &str,
    mems: &[Memory],
    now_ms: i64,
    pleasure: f64,
    k: usize,
) -> Vec<Hit> {
    let q = query.trim();
    if q.is_empty() || mems.is_empty() || k == 0 {
        return Vec::new();
    }

    let mut q_terms = crate::memory::bigrams(q);
    q_terms.sort();
    q_terms.dedup();
    let q_entities = extract_entities(q);

    // ── 1. df 现算：每个查询二元组在库里出现过几条 ──────────────
    let n = mems.len() as f64;
    let df: Vec<f64> = q_terms
        .iter()
        .map(|t| mems.iter().filter(|m| m.text.contains(t.as_str())).count() as f64)
        .collect();
    let idf: Vec<f64> = df
        .iter()
        .map(|&d| (1.0 + (n - d + 0.5) / (d + 0.5)).ln())
        .collect();
    let total_idf: f64 = idf.iter().sum();

    let mut hits: Vec<Hit> = Vec::with_capacity(mems.len().min(CANDIDATE_CAP));

    for (i, m) in mems.iter().enumerate() {
        let mut idf_hit = 0.0;
        let mut matched: Vec<String> = Vec::new();
        for (j, t) in q_terms.iter().enumerate() {
            if m.text.contains(t.as_str()) {
                idf_hit += idf[j];
                if matched.len() < 6 {
                    matched.push(t.clone());
                }
            }
        }

        // 共享实体：查询里抽到的实体，或者干脆整句问话提到过某个实体名
        let shared: Vec<String> = m
            .entities
            .iter()
            .filter(|e| q_entities.iter().any(|x| x == *e) || q.contains(e.as_str()))
            .cloned()
            .collect();

        // 一路都没命中的不叫「想起」——不进候选，免得靠重要度蒙进来
        if idf_hit == 0.0 && shared.is_empty() {
            continue;
        }

        let kw = if total_idf > 0.0 {
            (idf_hit / total_idf).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let graph = ((shared.len() as f64) / 2.0).min(1.0);
        let age_days = ((now_ms - m.created_at).max(0) as f64) / 86_400_000.0;
        let fresh = (-age_days / FRESH_DAYS).exp();
        let mood = if m.valence * pleasure > 0.0 {
            m.valence.abs()
        } else {
            0.0
        };

        let base = W_KW * kw
            + W_GRAPH * graph
            + W_IMPORTANCE * m.importance
            + W_FRESH * fresh
            + W_MOOD * mood;
        let score = base * m.strength * (0.5 + 0.5 * m.confidence);

        hits.push(Hit {
            index: i,
            score,
            why: why_string(&matched, &shared, m.importance, age_days),
            matched_terms: matched,
            shared_entities: shared,
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // 同分时新的在前，别让老账压新的
            .then_with(|| mems[b.index].created_at.cmp(&mems[a.index].created_at))
    });
    hits.truncate(k);
    hits
}

fn why_string(matched: &[String], shared: &[String], importance: f64, age_days: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !matched.is_empty() {
        parts.push(format!("命中词 {}", matched.join("/")));
    }
    if !shared.is_empty() {
        parts.push(format!("共享实体 {}", shared.join("、")));
    }
    parts.push(format!("重要度 {importance:.2}"));
    parts.push(format!("{age_days:.0} 天前"));
    parts.join("；")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{draft, WriteIntent};

    fn mem(text: &str, importance: f64, at: i64) -> Memory {
        let m = draft(
            &WriteIntent {
                text: text.to_string(),
                importance: Some(importance),
                ..Default::default()
            },
            at,
            &[],
        )
        .unwrap();
        m
    }

    #[test]
    fn exact_topic_beats_unrelated_high_importance() {
        let now = 1_000_000_000_000;
        let mems = vec![
            mem("他说下周要交 steadycore 的 P1 记忆层", 0.5, now),
            mem("很重要的一件事：明天要交周报", 1.0, now),
        ];
        let hits = rank("P1 记忆层做到哪了", &mems, now, 0.0, 5);
        assert_eq!(hits[0].index, 0);
        assert!(hits[0].why.contains("命中词"));
    }

    #[test]
    fn no_keyword_no_hit() {
        let now = 0;
        let mems = vec![mem("今天下雨了", 1.0, now)];
        assert!(rank("咖啡机又坏了", &mems, now, 0.0, 5).is_empty());
    }

    #[test]
    fn older_loses_to_newer_on_tie() {
        let now = 100 * 86_400_000;
        let mems = vec![
            mem("他说要交稿", 0.6, now - 60 * 86_400_000),
            mem("他说要交稿", 0.6, now),
        ];
        let hits = rank("交稿", &mems, now, 0.0, 5);
        assert_eq!(hits[0].index, 1);
    }

    #[test]
    fn entity_mention_recalls_without_topic_words() {
        let now = 0;
        let mems = vec![mem("10001 今天很累", 0.5, now)];
        let hits = rank("10001", &mems, now, 0.0, 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].why.contains("共享实体") || hits[0].why.contains("命中词"));
    }

    #[test]
    fn confidence_shrinks_score() {
        let now = 0;
        let mut a = mem("他说想写新记忆体", 0.6, now);
        let mut b = mem("他说想写新记忆体", 0.6, now);
        a.confidence = 1.0;
        b.confidence = 0.3;
        let hits = rank("新记忆体", &[a, b], now, 0.0, 2);
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn filter_by_scope_and_layer() {
        let now = 0;
        let mut a = mem("群里聊逃离那个游戏", 0.5, now);
        a.scope = "qq群".into();
        let b = mem("用户说那个游戏别玩了", 0.5, now);
        let f = Filter {
            scope: Some("qq群".into()),
            ..Default::default()
        };
        let kept: Vec<&Memory> = [&a, &b].into_iter().filter(|m| f.keep(m)).collect();
        assert_eq!(kept.len(), 1);
    }
}
