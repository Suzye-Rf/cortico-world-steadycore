//! P1 验收：recall@5。评估集在 `recall_eval.json`，不在这个文件里。
//!
//! 为什么把它搬出去：评估集是要**跟着日子长**的——又发生一件真事，就往 json 里加一条；
//! 哪天我漏了一件事，也把那种问法留下来，那是它没接住的地方。放在 Rust 里加一条要改代码、
//! 要记得下标，加着加着就没人加了。
//!
//! 现在这个测试干两件事：
//! 1. **及格线**：rate ≥ `minRate`（0.8）。
//! 2. **不许退步**：漏的条数 ≤ `maxMisses`。这一条才是"掉了就红"——分数掉下去这件事，
//!    本来不会有人发现，直到某天它连我是谁都想不起来。
//!
//! 纯内存跑，不连库——检索的分数只跟记忆内容和问法有关。

use std::collections::BTreeMap;

use steadycore::memory::{draft, WriteIntent};
use steadycore::recall::rank;
use steadycore::Memory;
use serde::Deserialize;

const DAY: i64 = 86_400_000;

/// `include_str!` 而不是运行时读文件：测试不依赖工作目录在哪，
/// 但改 json 仍然只要 `cargo test` 一次，不用动 Rust。
const EVAL_JSON: &str = include_str!("recall_eval.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Eval {
    now_ms: i64,
    top_k: usize,
    min_rate: f64,
    max_misses: usize,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    text: String,
    days_ago: i64,
    /// 同一件事可以有几种问法。问法比事多没关系——多的那种就是顺便测泛化。
    ask: Vec<String>,
}

fn eval() -> Eval {
    serde_json::from_str(EVAL_JSON).expect("recall_eval.json 读不了，看看是不是 JSON 写坏了")
}

fn build(cases: &[Case], now: i64) -> Vec<Memory> {
    cases
        .iter()
        .map(|c| {
            draft(
                &WriteIntent {
                    text: c.text.clone(),
                    ..Default::default()
                },
                now - c.days_ago * DAY,
                &[],
            )
            .unwrap()
        })
        .collect()
}

#[test]
fn recall_at_k_is_good_enough_and_does_not_slip() {
    let e = eval();
    assert!(!e.cases.is_empty(), "评估集是空的");
    let mems = build(&e.cases, e.now_ms);

    let mut total = 0usize;
    let mut misses: Vec<String> = Vec::new();
    // 每条 case 的最好名次：1 = 第一名就是它，None = 没进前 K。
    // 摆出来是为了让"第几名"看得见——只报一个 0.95 看不出快掉下去了。
    let mut ranks: BTreeMap<usize, Option<usize>> = BTreeMap::new();

    for (want, case) in e.cases.iter().enumerate() {
        for q in &case.ask {
            total += 1;
            let hits = rank(q, &mems, e.now_ms, 0.0, e.top_k);
            let got: Vec<usize> = hits.iter().map(|h| h.index).collect();
            let place = got.iter().position(|&i| i == want).map(|p| p + 1);
            let best = ranks.entry(want).or_insert(place);
            *best = match (*best, place) {
                (None, _) => None,       // 已经漏过一次，这个 case 就记着"漏过"
                (Some(_), None) => None, // 这次漏了
                (Some(a), Some(b)) => Some(a.min(b)),
            };
            if place.is_none() {
                misses.push(format!(
                    "问「{q}」想要 #{}「{}」，拿到 {got:?}",
                    want, case.text
                ));
            }
        }
    }

    let miss = misses.len();
    let rate = (total - miss) as f64 / total as f64;

    println!(
        "recall@{} = {:.2}（{}/{} 命中，漏 {}，线：rate ≥ {:.2} 且漏 ≤ {}）",
        e.top_k,
        rate,
        total - miss,
        total,
        miss,
        e.min_rate,
        e.max_misses
    );
    let spread: Vec<String> = ranks
        .iter()
        .map(|(i, r)| match r {
            Some(1) => format!("#{i} 第一"),
            Some(n) => format!("#{i} 第{n}"),
            None => format!("#{i} 没进前 K"),
        })
        .collect();
    println!("每条的位置：{}", spread.join(" / "));

    assert!(rate >= e.min_rate, "掉到及格线下面了。漏的：\n{}", misses.join("\n"));
    assert!(
        miss <= e.max_misses,
        "没到及格线以下，但比上次退步了（漏 {miss} > {}）。退步的：\n{}",
        e.max_misses,
        misses.join("\n")
    );
}

#[test]
fn every_hit_says_why() {
    let e = eval();
    let mems = build(&e.cases, e.now_ms);
    for case in &e.cases {
        for q in &case.ask {
            let hits = rank(q, &mems, e.now_ms, 0.0, e.top_k);
            assert!(!hits.is_empty(), "问「{q}」一条都没命中，连理由都没得给");
            for h in hits {
                assert!(!h.why.trim().is_empty(), "问「{q}」有一条没给理由");
                assert!(h.score > 0.0);
            }
        }
    }
}

#[test]
fn empty_query_returns_nothing() {
    let e = eval();
    let mems = build(&e.cases, e.now_ms);
    assert!(rank("   ", &mems, e.now_ms, 0.0, e.top_k).is_empty());
}
