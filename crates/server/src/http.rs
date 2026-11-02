//! HTTP 层：薄。只做参数解析 + 调 `App`，业务逻辑一律不写在这儿。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::app::App;
use crate::config::fmt_ms;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/v1/health", get(health))
        .route("/v1/life", get(get_life))
        .route("/v1/life/tick", get(tick).post(tick))
        .route("/v1/life/log", get(get_log))
        .route("/v1/life/set", post(set_life))
        .route("/v1/events", get(get_events))
        .route("/v1/remember", post(remember))
        .route("/v1/recall", post(recall))
        .route("/v1/forget", post(forget))
        .route("/v1/pin", post(pin))
        .route("/v1/conflict", post(conflict))
        .route("/v1/stats", get(stats))
        .route("/v1/identity", post(write_identity).get(list_identity))
        .route("/v1/standard", post(write_standard).get(list_standard))
        .route("/v1/boot", get(boot_pack))
        .route("/v1/bond", get(list_bond).post(write_bond))
        .route("/v1/life/decide", get(decide))
        .route("/v1/life/touch", post(touch_life))
        .route("/v1/life/wake", get(wake_preview))
        .route("/v1/life/simulate", get(simulate_life))
        .route("/v1/intent", post(intent))
        .route("/v1/produce", get(list_produce).post(write_produce))
        .route("/v1/backup", post(backup))
        .with_state(app)
}

/// 整库倒一份到 `data/backup/<stamp>/`。手动触发；定时是外面的事（任务计划）。
async fn backup(State(app): State<Arc<App>>) -> Json<Value> {
    let dir = crate::config::data_dir()
        .join("backup")
        .join(crate::config::stamp());
    match app.store.dump_all(&dir).await {
        Ok(cols) => {
            let total: u64 = cols.iter().map(|(_, c)| *c).sum();
            Json(json!({
                "ok": true,
                "dir": dir.to_string_lossy(),
                "total": total,
                "collections": cols
                    .iter()
                    .map(|(n, c)| json!({ "name": n, "docs": c }))
                    .collect::<Vec<_>>(),
            }))
        }
        Err(e) => bad("backup_failed", &format!("{e:#}")),
    }
}

async fn root() -> Json<Value> {
    Json(json!({
        "ok": true,
        "name": "steadycore",
        "version": steadycore::model::VERSION,
        "stage": "P4",
        "docs": "/v1/health · /v1/life · /v1/life/tick · /v1/life/log · /v1/life/set · /v1/life/decide · /v1/life/simulate · /v1/intent · /v1/produce · /v1/events · /v1/remember · /v1/recall · /v1/forget · /v1/pin · /v1/conflict · /v1/stats · /v1/identity · /v1/standard · /v1/boot · /v1/bond",
    }))
}

/// 探活。mongo 断了这里会**如实**报 down，并且顺手再试一次。
async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    if !app.store.up() {
        let _ = app.store.ping().await;
    }
    let mem = app.store.count_mem().await;
    app.mem_count.store(mem, Ordering::Relaxed);

    let life = app.life_snapshot();
    let last = app.last_tick_ms.load(Ordering::Relaxed);

    Json(json!({
        "ok": true,
        "name": "steadycore",
        "pid": std::process::id(),
        "version": steadycore::model::VERSION,
        "stage": "P4",
        "uptimeSec": app.uptime_sec(),
        "startedAt": fmt_ms(app.started_ms),
        "mongo": if app.store.up() { "up" } else { "down" },
        "mongoError": app.store.last_error(),
        "memCount": mem,
        "tickCount": app.tick_count.load(Ordering::Relaxed),
        "lastTickAt": if last > 0 { Value::String(fmt_ms(last)) } else { Value::Null },
        "life": {
            "seq": life.seq,
            "version": life.version,
            "current": life.current,
            "mood": life.mood(),
            "updatedAt": if life.updated_at_ms > 0 { Value::String(fmt_ms(life.updated_at_ms)) } else { Value::Null },
        },
        "events": {
            "lastSeq": app.event_seq.load(Ordering::Relaxed),
            "buffered": app.events.lock().unwrap().len(),
        },
        "config": {
            "tickIntervalSec": app.cfg.tick_interval_sec,
            "tickJitterSec": app.cfg.tick_jitter_sec,
            "db": app.cfg.db,
        },
    }))
}

async fn get_life(State(app): State<Arc<App>>) -> Json<Value> {
    let s = app.life_snapshot();
    Json(json!({ "ok": true, "state": s, "mood": s.mood() }))
}

/// 手动推一步。`?force=nap` 可以指定动作（调试用）。
/// 走的是和定时器完全相同的 `do_tick`。
async fn tick(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let force = q.get("force").filter(|s| !s.is_empty()).cloned();
    let out = app.do_tick(force, "manual").await;
    let s = app.life_snapshot();

    Json(json!({
        "ok": true,
        "seq": out.seq,
        "action": out.action,
        "reason": out.reason,
        "weight": out.weight,
        "scores": out.scores.iter().map(|(a, w)| json!({ "action": a, "weight": w })).collect::<Vec<_>>(),
        "alerts": out.alerts,
        "mood": s.mood(),
        "state": {
            "energy": s.energy,
            "sleepy": s.sleepy,
            "social": s.social,
            "boredom": s.boredom,
            "curiosity": s.curiosity,
            "rest": s.rest,
        },
    }))
}

async fn get_log(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let n: usize = q.get("n").and_then(|s| s.parse().ok()).unwrap_or(20);
    let s = app.life_snapshot();
    let start = s.recent.len().saturating_sub(n);
    let items: Vec<Value> = s.recent[start..]
        .iter()
        .map(|r| {
            json!({
                "seq": r.seq,
                "at": fmt_ms(r.ts_ms),
                "action": r.action,
                "reason": r.reason,
                "weight": r.weight,
            })
        })
        .collect();
    Json(json!({ "ok": true, "total": s.recent.len(), "items": items }))
}

/// 手动改状态量（调试 / 我情绪上来时推一把）。只认六个量和 pad。
async fn set_life(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return Json(json!({ "ok": false, "error": { "code": "bad_body", "message": "要一个 JSON body" } }));
    };

    let mut changed: Vec<String> = Vec::new();
    {
        let mut s = app.life.lock().unwrap();
        for key in ["energy", "sleepy", "social", "boredom", "curiosity", "rest"] {
            if let Some(n) = v.get(key).and_then(|x| x.as_f64()) {
                let val = n.clamp(0.0, 1.0);
                match key {
                    "energy" => s.energy = val,
                    "sleepy" => s.sleepy = val,
                    "social" => s.social = val,
                    "boredom" => s.boredom = val,
                    "curiosity" => s.curiosity = val,
                    "rest" => s.rest = val,
                    _ => {}
                }
                changed.push(key.to_string());
            }
        }
        if let Some(pad) = v.get("pad") {
            for key in ["pleasure", "arousal", "dominance"] {
                if let Some(n) = pad.get(key).and_then(|x| x.as_f64()) {
                    let val = n.clamp(-1.0, 1.0);
                    match key {
                        "pleasure" => s.pad.pleasure = val,
                        "arousal" => s.pad.arousal = val,
                        "dominance" => s.pad.dominance = val,
                        _ => {}
                    }
                    changed.push(format!("pad.{key}"));
                }
            }
        }
        if let Some(a) = v.get("activity").and_then(|x| x.as_str()) {
            s.current = a.to_string();
            changed.push("current".to_string());
        }
        s.clamp();
        s.version += 1;
        s.updated_at_ms = crate::app::now_ms();
    }

    let s = app.life_snapshot();
    if let Err(e) = app.store.save_life(&s).await {
        tracing::warn!(error = %e, "set 之后写库失败");
    }

    Json(json!({ "ok": true, "changed": changed, "state": s }))
}

/// outbox 拉取。`since` 是上次拿到的 `nextSeq`，只回比它大的。
async fn get_events(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let since: i64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(500);
    let (items, next) = app.events_since(since, limit);
    Json(json!({ "ok": true, "items": items, "nextSeq": next }))
}

// ── 记忆（P1）──────────────────────────────────────────────────

/// 存一条。撞身份 → 拒写；跟已有记忆高度相似 → 标 `conflictWith`，**不覆盖**。
async fn remember(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let intent: steadycore::WriteIntent = match serde_json::from_value(v) {
        Ok(i) => i,
        Err(e) => return bad("bad_body", &format!("字段不对：{e}")),
    };

    let whys = app.store.identity_whys().await;
    let mut m = match steadycore::draft(&intent, crate::app::now_ms(), &whys) {
        Ok(m) => m,
        Err(r) => return bad(r.code(), &r.message()),
    };

    // 冲突检测：同 scope 下相似度 > 0.85 的，标上，两条都留着
    let mut conflicts: Vec<Value> = Vec::new();
    if let Ok(existing) = app.store.load_mems(5000).await {
        for old in existing.iter().filter(|o| o.scope == m.scope) {
            let sim = steadycore::memory::similarity(&m.text, &old.text);
            if sim > 0.85 {
                m.conflict_with.push(old.id.clone());
                conflicts.push(json!({ "id": old.id, "text": old.text, "sim": sim }));
            }
        }
    }

    if let Err(e) = app.store.insert_mem(&m).await {
        return bad("store_failed", &format!("写不进去：{e:#}"));
    }

    Json(json!({
        "ok": true,
        "id": m.id,
        "layer": m.layer,
        "kind": m.kind,
        "confidence": m.confidence,
        "importance": m.importance,
        "entities": m.entities,
        "conflict": conflicts,
    }))
}

/// 想一件事。每条结果都带 `why`——说不清怎么想起来的，等于没想起来。
async fn recall(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let query = v.get("query").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if query.trim().is_empty() {
        return bad("empty_query", "要找什么");
    }
    let k = v.get("k").and_then(|x| x.as_u64()).unwrap_or(5).clamp(1, 50) as usize;

    let filter = steadycore::Filter {
        scope: v.get("scope").and_then(|x| x.as_str()).map(|s| s.to_string()),
        layer: v.get("layer").and_then(|x| x.as_str()).map(|s| s.to_string()),
        since_ms: v.get("since").and_then(|x| x.as_str()).and_then(parse_ts),
        until_ms: v.get("until").and_then(|x| x.as_str()).and_then(parse_ts),
        min_confidence: v.get("minConfidence").and_then(|x| x.as_f64()),
    };

    let all = match app.store.load_mems(5000).await {
        Ok(a) => a,
        Err(e) => return bad("store_failed", &format!("读不出来：{e:#}")),
    };
    let pool: Vec<steadycore::Memory> = all.into_iter().filter(|m| filter.keep(m)).collect();

    let pleasure = app.life_snapshot().pad.pleasure;
    let now = crate::app::now_ms();
    let hits = steadycore::rank(&query, &pool, now, pleasure, k);

    let items: Vec<Value> = hits
        .iter()
        .map(|h| {
            let m = &pool[h.index];
            json!({
                "id": m.id,
                "text": m.text,
                "layer": m.layer,
                "kind": m.kind,
                "score": (h.score * 10_000.0).round() / 10_000.0,
                "confidence": m.confidence,
                "importance": m.importance,
                "why": h.why,
                "createdAt": fmt_ms(m.created_at),
                "conflictWith": m.conflict_with,
                "pinned": m.pinned,
            })
        })
        .collect();

    let ids: Vec<String> = hits.iter().map(|h| pool[h.index].id.clone()).collect();
    if let Err(e) = app.store.touch_mems(&ids, now).await {
        tracing::warn!(error = %e, "记访问次数失败");
    }

    Json(json!({ "ok": true, "query": query, "pool": pool.len(), "items": items }))
}

/// 忘掉一条。`hard=false` 只是归档，还能翻出来。
async fn forget(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let hard = v.get("hard").and_then(|x| x.as_bool()).unwrap_or(false);

    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
        return match app.store.drop_mem(id, hard).await {
            Ok(true) => Json(json!({ "ok": true, "dropped": [id] })),
            Ok(false) => bad("not_found", "没有这条"),
            Err(e) => bad("store_failed", &format!("{e:#}")),
        };
    }

    // 按问话忘：只删分数最高的那一条，别一口气清一片
    let Some(query) = v.get("query").and_then(|x| x.as_str()) else {
        return bad("bad_body", "给 id 或 query");
    };
    let all = app.store.load_mems(5000).await.unwrap_or_default();
    let hits = steadycore::rank(query, &all, crate::app::now_ms(), 0.0, 1);
    let Some(h) = hits.first() else {
        return bad("not_found", "没想起相关的");
    };
    let id = all[h.index].id.clone();
    match app.store.drop_mem(&id, hard).await {
        Ok(_) => Json(json!({ "ok": true, "dropped": [id], "text": all[h.index].text })),
        Err(e) => bad("store_failed", &format!("{e:#}")),
    }
}

async fn pin(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
        return bad("bad_body", "给 id");
    };
    let want = v.get("pinned").and_then(|x| x.as_bool()).unwrap_or(true);
    match app.store.set_pin(id, want).await {
        Ok(true) => Json(json!({ "ok": true, "id": id, "pinned": want })),
        Ok(false) => bad("not_found", "没有这条"),
        Err(e) => bad("store_failed", &format!("{e:#}")),
    }
}

/// 这句话撞不撞身份层。写之前先问一句，比写完被拒好。
async fn conflict(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
    let whys = app.store.identity_whys().await;
    match steadycore::memory::identity_conflict(text, &whys) {
        Some(hit) => Json(json!({ "ok": true, "conflict": true, "hit": hit })),
        None => Json(json!({ "ok": true, "conflict": false })),
    }
}

async fn stats(State(app): State<Arc<App>>) -> Json<Value> {
    let mut v = app.store.mem_stats().await;
    if let Some(o) = v.as_object_mut() {
        o.insert("ok".into(), json!(true));
    }
    Json(v)
}

// ── 自我层与开机包（P2）────────────────────────────────────────

/// 身份层：只追加。`why` 必填——它是以后挡假记忆的钥匙。
async fn write_identity(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
    let why = v.get("why").and_then(|x| x.as_str()).unwrap_or("");
    let whys = app.store.identity_whys().await;
    let id = steadycore::memory::new_id(crate::app::now_ms());
    match steadycore::make_identity(id.clone(), text, why, crate::app::now_ms(), &whys) {
        Ok(i) => match app.store.insert_identity(&i).await {
            Ok(_) => Json(json!({ "ok": true, "id": i.id, "text": i.text, "why": i.why })),
            Err(e) => bad("store_failed", &format!("写不进去：{e:#}")),
        },
        Err(r) => bad(r.code(), &r.message()),
    }
}

async fn list_identity(State(app): State<Arc<App>>) -> Json<Value> {
    let items = app.store.load_identities().await;
    Json(json!({ "ok": true, "count": items.len(), "items": items }))
}

/// 标准层：可迭代。带 `supersedes` 就是新版上台，旧版留着并写明为什么被推翻。
async fn write_standard(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let intent: steadycore::StandardIntent = match serde_json::from_value(v) {
        Ok(i) => i,
        Err(e) => return bad("bad_body", &format!("字段不对：{e}")),
    };

    let existing = app.store.load_standards().await;
    let supersedes = intent
        .supersedes
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (version, old_id) = match &supersedes {
        Some(id) => match existing.iter().find(|s| &s.id == id) {
            Some(old) => (old.version + 1, Some(old.id.clone())),
            None => return bad("not_found", "要推翻的那条标准不存在"),
        },
        None => (1, None),
    };

    let whys = app.store.identity_whys().await;
    let new_id = steadycore::memory::new_id(crate::app::now_ms());
    let std_ = match steadycore::make_standard(new_id.clone(), &intent, version, crate::app::now_ms(), &whys) {
        Ok(s) => s,
        Err(r) => return bad(r.code(), &r.message()),
    };

    if let Err(e) = app.store.insert_standard(&std_).await {
        return bad("store_failed", &format!("写不进去：{e:#}"));
    }
    let mut superseded: Option<String> = None;
    if let Some(old) = old_id {
        match app
            .store
            .supersede_standard(&old, &new_id, &std_.superseded_reason)
            .await
        {
            Ok(true) => superseded = Some(old),
            Ok(false) => return bad("store_failed", "新版写进去了，旧版没标上——去库里看一眼"),
            Err(e) => return bad("store_failed", &format!("旧版没标上：{e:#}")),
        }
    }

    Json(json!({
        "ok": true,
        "id": std_.id,
        "version": std_.version,
        "kind": std_.kind,
        "superseded": superseded,
        "note": if std_.is_tendency() {
            "没给出「怎么重跑」，所以只当倾向用，不参与否决"
        } else {
            "当前有效"
        },
    }))
}

async fn list_standard(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let all = app.store.load_standards().await;
    let only_current = !matches!(q.get("all").map(|s| s.as_str()), Some("1") | Some("true"));
    let items: Vec<_> = all
        .iter()
        .filter(|s| !only_current || s.is_current())
        .cloned()
        .collect();
    Json(json!({ "ok": true, "count": items.len(), "items": items }))
}

/// 开机包：十二块，1800–2000 token。超了截断、薄了补，账都在 `blocks` 里。
async fn boot_pack(State(app): State<Arc<App>>) -> Json<Value> {
    let idn = app.store.load_identities().await;
    let stds = app.store.load_standards().await;
    let mems = app.store.load_mems(5000).await.unwrap_or_default();
    let bonds = app.store.load_bonds().await.unwrap_or_default();
    let life = app.life_snapshot();
    let b = steadycore::boot(&idn, &stds, &mems, &bonds, &life, 0);
    let mut v = serde_json::to_value(&b).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("ok".into(), json!(true));
    }
    Json(v)
}

// ── 关系层：交情账 ───────────────────────────────────────────────

/// 全部交情账，分高的在前。
async fn list_bond(State(app): State<Arc<App>>) -> Json<Value> {
    let items = app.store.load_bonds().await.unwrap_or_default();
    let lines: Vec<String> = items.iter().map(|b| b.line()).collect();
    Json(json!({
        "ok": true,
        "count": items.len(),
        "items": items,
        "lines": lines,
    }))
}

/// 记一笔交情。
///
/// 三种用法，一次只干一样：
/// - 默认（`talk` 不写成 false）：聊过一句，`+1`，一天最多 +6；
/// - 给 `delta`：手动加 / 扣（扣分不受日封顶，负数就是「这人惹我了」）；
/// - 给 `note`：写 / 改一句备注，分不动。
///
/// `who` 是稳定键（QQ 号或昵称），`name` 是显示名；`pinned` 只给用户用。
async fn write_bond(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body：{who, name?, delta?, note?, pinned?}");
    };
    let who = v.get("who").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if who.is_empty() {
        return bad("bad_body", "who 不能空——拿 QQ 号当键，改昵称不该丢分");
    }
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
    let note = v.get("note").and_then(|x| x.as_str());
    let delta = v.get("delta").and_then(|x| x.as_f64());
    let pinned = v.get("pinned").and_then(|x| x.as_bool());
    // 迁移老账用：直接把「聊过几句」放到这个数（不是加），平时别给
    let talks = v.get("talks").and_then(|x| x.as_i64());
    let talk = v.get("talk").and_then(|x| x.as_bool()).unwrap_or(delta.is_none());

    let now = crate::app::now_ms();
    let mut b = match app.store.load_bond(&who).await {
        Ok(Some(b)) => b,
        Ok(None) => steadycore::Bond::new(&who, name, now),
        Err(e) => return bad("store", &format!("读 core_bond 失败：{e:#}")),
    };
    if !name.trim().is_empty() {
        b.name = name.trim().to_string();
    }
    if let Some(p) = pinned {
        b.pinned = p;
    }

    if let Some(n) = talks {
        b.talks = n.max(0);
        b.updated_at = now;
    }
    let gain = if let Some(d) = delta {
        b.bump(d, now)
    } else if talk {
        b.talk(now)
    } else {
        0.0
    };
    if let Some(n) = note {
        b.set_note(n, now);
    }

    if let Err(e) = app.store.save_bond(&b).await {
        return bad("store", &format!("写 core_bond 失败：{e:#}"));
    }
    Json(json!({
        "ok": true,
        "gain": (gain * 10.0).round() / 10.0,
        "bond": b,
        "line": b.line(),
    }))
}

// ── 小工具 ─────────────────────────────────────────────────────

/// 这一 tick 该不该叫模型（§10.6）。**只看不记账**——真叫了才 `POST /v1/intent`。
async fn decide(State(app): State<Arc<App>>) -> Json<Value> {
    let now = crate::app::now_ms();
    let mut s = app.life_snapshot();
    s.roll_day(steadycore::life::day_of(now));
    let d = steadycore::decide(&s, now);
    Json(json!({
        "ok": true,
        "decision": d,
        "gates": s.gates,
        "limits": {
            "wakeDailyMax": steadycore::life::WAKE_DAILY_MAX,
            "sayDailyMax": steadycore::life::SAY_DAILY_MAX,
            "sayMinGapMin": steadycore::life::SAY_MIN_GAP_MS / 60_000,
            "valueFloor": steadycore::life::VALUE_FLOOR,
        },
    }))
}

/// 现在要是叫一次模型，会给它看什么。**只看不发**——真叫由 tick 里 `wake.request` 那条走。
async fn wake_preview(State(app): State<Arc<App>>) -> Json<Value> {
    let now = crate::app::now_ms();
    let mut s = app.life_snapshot();
    s.roll_day(steadycore::life::day_of(now));
    let d = steadycore::decide(&s, now);
    let mems = app.wake_mems(&s.current).await;
    let b = steadycore::wake_brief(&s, &d, &mems, now);
    Json(json!({ "ok": true, "wake": d.wake, "brief": b }))
}

/// 有人跟我说话：`social` 泄一截。P3 只让「我主动说出口」降，这条是另一半。
async fn touch_life(State(app): State<Arc<App>>) -> Json<Value> {
    let (state, done) = app.do_touch().await;
    Json(json!({
        "ok": true,
        "done": done,
        "state": {
            "social": state.social,
            "boredom": state.boredom,
            "curiosity": state.curiosity,
            "mood": state.mood(),
        },
    }))
}

/// 干跑：从现在这个状态快进 `hours` 小时。只算，不改真状态。
async fn simulate_life(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let hours: f64 = q
        .get("hours")
        .and_then(|s| s.parse().ok())
        .filter(|h: &f64| *h > 0.0 && *h <= 24.0 * 30.0)
        .unwrap_or(24.0);
    let now = crate::app::now_ms();
    let s = app.life_snapshot();
    let mut rng = rand::thread_rng();
    let rep = steadycore::simulate(&s, now, hours, &mut rng);
    Json(json!({ "ok": true, "report": rep }))
}

/// 模型（或我）说想做点什么。**能不能做另说**——拒的时候状态一动不动。
async fn intent(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let it: steadycore::Intent = match serde_json::from_value(v) {
        Ok(i) => i,
        Err(e) => return bad("bad_intent", &format!("intent 解析不了：{e}")),
    };
    match app.do_intent(&it).await {
        Ok(done) => Json(json!({ "ok": true, "done": done })),
        Err(e) => bad(e.code(), &e.message()),
    }
}

/// 还没人填的产出槽。
async fn list_produce(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let n: i64 = q.get("n").and_then(|s| s.parse().ok()).unwrap_or(20).min(200);
    match app.store.pending_produces(n).await {
        Ok(items) => Json(json!({ "ok": true, "count": items.len(), "items": items })),
        Err(e) => bad("store_failed", &format!("读 core_produce 失败：{e:#}")),
    }
}

/// 往槽里填字 → 落成一条 `source:"life"` 的记忆。`{"id": "...", "drop": true}` 就是放掉。
async fn write_produce(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> Json<Value> {
    let Some(Json(v)) = body else {
        return bad("bad_body", "要一个 JSON body");
    };
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return bad("missing_field", "缺「id」，得说清填的是哪个槽");
    }
    let now = crate::app::now_ms();

    if v.get("drop").and_then(|x| x.as_bool()).unwrap_or(false) {
        return match app.store.finish_produce(&id, "dropped", "", "", now).await {
            Ok(true) => Json(json!({ "ok": true, "id": id, "status": "dropped" })),
            Ok(false) => bad("not_found", &format!("没有 id 为 {id} 的槽")),
            Err(e) => bad("store_failed", &format!("{e:#}")),
        };
    }

    let text = v
        .get("text")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return bad("empty_text", "要填「text」；空的不算产出，想放掉就传 drop:true");
    }

    let pending = app.store.pending_produces(500).await.unwrap_or_default();
    let Some(p) = pending.into_iter().find(|p| p.id == id) else {
        return bad("not_found", &format!("没有 id 为 {id} 的待填槽（也许已经填过或放掉了）"));
    };

    let whys = app.store.identity_whys().await;
    let draft = steadycore::WriteIntent {
        text: text.clone(),
        layer: Some(p.layer.clone()),
        source: Some("life".to_string()),
        source_ref: Some(p.id.clone()),
        scope: Some("life".to_string()),
        importance: Some(0.5),
        ..Default::default()
    };
    let m = match steadycore::draft(&draft, now, &whys) {
        Ok(m) => m,
        Err(r) => return bad(r.code(), &r.message()),
    };
    if let Err(e) = app.store.insert_mem(&m).await {
        return bad("store_failed", &format!("写不进去：{e:#}"));
    }
    match app.store.finish_produce(&id, "done", &text, &m.id, now).await {
        Ok(_) => Json(json!({
            "ok": true,
            "id": id,
            "status": "done",
            "memId": m.id,
            "layer": m.layer,
            "text": m.text,
        })),
        Err(e) => bad("store_failed", &format!("记忆写了，但槽没标上：{e:#}")),
    }
}

fn bad(code: &str, msg: &str) -> Json<Value> {
    Json(json!({ "ok": false, "error": { "code": code, "message": msg } }))
}

/// 收 `2026-11-01` / `2026-11-01 13:00` / `2026-11-01T13:00:00` 三种写法。
fn parse_ts(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?;
        return Local.from_local_datetime(&dt).single().map(|x| x.timestamp_millis());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return Local.from_local_datetime(&dt).single().map(|x| x.timestamp_millis());
        }
    }
    s.parse::<i64>().ok()
}
