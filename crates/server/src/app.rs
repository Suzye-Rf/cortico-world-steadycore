//! 进程内的共享状态与那唯一一条「走一步」的路径。
//!
//! 后台定时器和 `POST /v1/life/tick` 走的是**同一个** `do_tick`，
//! 这样手动推一把跟自然到点完全等价，不会出现两套逻辑对不上的情况。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Local;
use steadycore::{
    CoreEvent, EventKind, Intent, IntentDone, IntentReject, LifeState, Produce, TickOutcome,
    WakeBrief,
};
use steadycore_store::Store;
use serde_json::json;
use tracing::{info, warn};

use crate::config::Config;

/// 内存里最多留多少条事件（足够宿主断一下再追上）
pub const EVENT_CAP: usize = 500;

pub struct App {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub life: Mutex<LifeState>,
    pub events: Mutex<VecDeque<CoreEvent>>,
    pub event_seq: AtomicI64,
    pub tick_count: AtomicU64,
    pub last_tick_ms: AtomicI64,
    pub mem_count: AtomicU64,
    pub started_at: Instant,
    pub started_ms: i64,
}

pub fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

impl App {
    pub fn new(cfg: Config, store: Arc<Store>) -> Arc<Self> {
        let now = now_ms();
        Arc::new(Self {
            cfg,
            store,
            life: Mutex::new(LifeState::default()),
            events: Mutex::new(VecDeque::with_capacity(EVENT_CAP)),
            event_seq: AtomicI64::new(0),
            tick_count: AtomicU64::new(0),
            last_tick_ms: AtomicI64::new(0),
            mem_count: AtomicU64::new(0),
            started_at: Instant::now(),
            started_ms: now,
        })
    }

    pub fn uptime_sec(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// 从库里把状态捞回来；捞不到就用默认值，并且把 `last_tick_ms` 补成现在，
    /// 免得第一次 tick 算出一个巨大的间隔。
    pub async fn restore(&self) {
        match self.store.load_life().await {
            Ok(Some(mut s)) => {
                if s.last_tick_ms == 0 {
                    s.last_tick_ms = now_ms();
                }
                s.recent.truncate(steadycore::model::RECENT_CAP);
                let seq = s.seq;
                *self.life.lock().unwrap() = s;
                info!(seq, "从 mongo 恢复了生活状态");
            }
            Ok(None) => {
                let mut s = LifeState::default();
                s.last_tick_ms = now_ms();
                s.current_since_ms = now_ms();
                s.updated_at_ms = now_ms();
                *self.life.lock().unwrap() = s;
                info!("库里没有 core_life，用默认状态起");
            }
            Err(e) => {
                warn!(error = %e, "读 core_life 失败，用默认状态起");
                let mut s = LifeState::default();
                s.last_tick_ms = now_ms();
                *self.life.lock().unwrap() = s;
            }
        }

        // 事件 seq 跨重启接着走。不然宿主手里的游标会错乱，
        // 而且内存里那个「从 1 重来」会被它当成旧事件。
        match self.store.max_event_seq().await {
            Ok(n) if n > 0 => {
                self.event_seq.store(n, Ordering::SeqCst);
                info!(last_seq = n, "从 mongo 接上了事件 seq");
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "读事件 seq 失败，从 0 起"),
        }
    }

    pub fn life_snapshot(&self) -> LifeState {
        self.life.lock().unwrap().clone()
    }

    /// 发一条事件到 outbox + 审计进库。内存里满了就丢最老的（seq 已经推进过，宿主会知道自己晚了）。
    async fn emit(&self, kind: EventKind, payload: serde_json::Value) {
        let seq = self.event_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let ev = CoreEvent::new(seq, now_ms(), kind, payload);
        {
            let mut q = self.events.lock().unwrap();
            if q.len() >= EVENT_CAP {
                q.pop_front();
            }
            q.push_back(ev.clone());
        }
        if let Err(e) = self.store.append_event(&ev).await {
            warn!(error = %e, seq, "事件写库失败（不影响内存里那份）");
        }
    }

    pub fn events_since(&self, since: i64, limit: usize) -> (Vec<CoreEvent>, i64) {
        let q = self.events.lock().unwrap();
        let items: Vec<CoreEvent> = q
            .iter()
            .filter(|e| e.seq > since)
            .take(limit)
            .cloned()
            .collect();
        let next = items.last().map(|e| e.seq).unwrap_or(since);
        (items, next)
    }

    /// 交情账的自然回落：久不联系的人慢慢淡回陌生人。
    ///
    /// 挂在 tick 上——反正每 40 分钟必走一步，不必另开一个定时器。
    /// 只有真掉了分才写回库，免得每 tick 把所有账都重写一遍。
    pub async fn settle_bonds(&self) -> usize {
        let now = now_ms();
        let bonds = match self.store.load_bonds().await {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "读交情账失败，这一轮不结算");
                return 0;
            }
        };
        let mut faded = 0;
        for mut b in bonds {
            if b.decay(now) <= 0.0 {
                continue;
            }
            info!(who = %b.who, score = b.score, "交情淡了一点");
            if let Err(e) = self.store.save_bond(&b).await {
                warn!(error = %e, who = %b.who, "写回交情账失败");
                continue;
            }
            faded += 1;
        }
        faded
    }

    /// 走一步。定时器和 HTTP 用的是同一个入口。
    pub async fn do_tick(&self, force: Option<String>, why: &str) -> TickOutcome {
        let now = now_ms();
        let (outcome, snapshot) = {
            let mut life = self.life.lock().unwrap();
            let mut rng = rand::thread_rng();
            let out = steadycore::step(&mut life, now, force.as_deref(), &mut rng);
            (out, life.clone())
        };

        self.tick_count.fetch_add(1, Ordering::SeqCst);
        self.last_tick_ms.store(now, Ordering::SeqCst);

        info!(
            seq = outcome.seq,
            action = %outcome.action,
            weight = outcome.weight,
            reason = %outcome.reason,
            trigger = why,
            "tick"
        );

        if let Err(e) = self.store.save_life(&snapshot).await {
            warn!(error = %e, "写 core_life 失败");
        }

        // 事件：先报 tick 本身，再报越线
        self.emit(
            EventKind::TickDone,
            json!({
                "seq": outcome.seq,
                "action": outcome.action,
                "reason": outcome.reason,
                "weight": outcome.weight,
                "mood": snapshot.mood(),
                "state": {
                    "energy": snapshot.energy,
                    "sleepy": snapshot.sleepy,
                    "social": snapshot.social,
                    "boredom": snapshot.boredom,
                    "curiosity": snapshot.curiosity,
                    "rest": snapshot.rest,
                },
                "trigger": why,
            }),
        )
        .await;

        for a in &outcome.alerts {
            self.emit(
                EventKind::StateThreshold,
                json!({ "alert": a, "atSeq": outcome.seq, "mood": snapshot.mood() }),
            )
            .await;
        }

        // 内容型动作跑完了 → 开一个待填的槽（§10.5）。字得模型来写，
        // 核心自己编一句「今天想了点什么」那就是假的，所以只开槽不填。
        if steadycore::life::is_content(&outcome.action) {
            let p = Produce::new(&outcome.action, outcome.seq, now);
            if let Err(e) = self.store.insert_produce(&p).await {
                warn!(error = %e, produce = %p.id, "产出槽写库失败");
            }
            self.emit(
                EventKind::IntentPending,
                json!({
                    "action": outcome.action,
                    "atSeq": outcome.seq,
                    "produceId": p.id,
                    "layer": p.layer,
                    "prompt": p.prompt,
                    "note": "开了一个待填的槽，谁有本事填谁来填（POST /v1/produce）",
                }),
            )
            .await;
        }

        // 该不该叫模型（§10.6）。内容型动作那一支上面已经开了槽，本身就是一次唤醒；
        // 这里只管 A（分不出高下 / 打架 / 社交判断）和 B（越线）——同一个 tick 不重复叫。
        let content = steadycore::life::is_content(&outcome.action);
        let decision = if content {
            None
        } else {
            Some(steadycore::decide(&self.life_snapshot(), now))
        };
        let want = matches!(&decision, Some(d) if d.wake);
        // 召回要 await，所以得放在碰锁之前。
        let mems = if want {
            self.wake_mems(&outcome.action).await
        } else {
            Vec::new()
        };

        // ⚠ 锁只活在这个块里，出块就 drop。它和 await 从此不见面，future 才是 Send。
        let (brief, snap): (Option<WakeBrief>, Option<LifeState>) = {
            let mut life = self.life.lock().unwrap();
            if content {
                steadycore::record_wake(&mut life, &outcome.action, now);
                (None, Some(life.clone()))
            } else if want {
                let d = decision.as_ref().expect("want 就意味着有 decision");
                let b = steadycore::wake_brief(&life, d, &mems, now);
                steadycore::record_wake(&mut life, &b.action, now);
                (Some(b), Some(life.clone()))
            } else {
                (None, None)
            }
        };

        if let Some(snap) = snap {
            if let Err(e) = self.store.save_life(&snap).await {
                warn!(error = %e, "写 core_life 失败");
            }
        }

        if let Some(b) = brief {
            info!(
                id = %b.id,
                kind = %b.kind,
                value = b.value,
                action = %b.action,
                "该叫模型了"
            );
            self.emit(
                EventKind::WakeRequest,
                json!({
                    "id": b.id,
                    "kind": b.kind,
                    "reasons": b.reasons,
                    "value": b.value,
                    "action": b.action,
                    "top1": b.top1,
                    "top2": b.top2,
                    "brief": b.text,
                    "note": "该叫一次模型了。看完 brief 回 POST /v1/intent；不回就照规则走。",
                }),
            )
            .await;
        }

        // 交情账顺手结算一下：这一步走完，看谁好久没说话了。
        // 放最后，前面的活一步都不能因为它失败而少做。
        let _ = self.settle_bonds().await;

        outcome
    }

    /// 唤醒时要顺手想起来的那几条（最多 6 条）。按当前动作给一句中文问题去召回——
    /// 拿动作名（`roam` 这种）当查询，在中文记忆里基本什么都命中不了。
    pub async fn wake_mems(&self, action: &str) -> Vec<String> {
        let all = match self.store.load_mems(500).await {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "读记忆失败，这一次唤唤不带记忆");
                return Vec::new();
            }
        };
        if all.is_empty() {
            return Vec::new();
        }
        let q = match action {
            "reflect" => "今天 心里 想",
            "study" => "学到 懂了 明白",
            "note" => "记下来 写下",
            "watch" => "看到 看过",
            "roam" => "出去 走走 外面",
            "tidy" => "收拾",
            "rest" | "nap" | "pause" => "累 困 歇",
            _ => "最近",
        };
        let pleasure = self.life_snapshot().pad.pleasure;
        let hits = steadycore::rank(q, &all, now_ms(), pleasure, 6);
        hits.iter().map(|h| all[h.index].text.clone()).collect()
    }

    /// 有人跟我说话。P3 只算了「我主动说出口」会降 social，这条是另一半。
    pub async fn do_touch(&self) -> (LifeState, steadycore::TouchDone) {
        let now = now_ms();
        let (done, snap) = {
            let mut life = self.life.lock().unwrap();
            let d = steadycore::touch(&mut life, now);
            (d, life.clone())
        };
        if let Err(e) = self.store.save_life(&snap).await {
            warn!(error = %e, "写 core_life 失败");
        }
        info!(social = done.social, "被人搭话了");
        (snap, done)
    }

    /// 拿一个 intent 去执行。拒的时候状态一动不动——宁可没做，也不要做了再说做不了。
    pub async fn do_intent(&self, it: &Intent) -> Result<IntentDone, IntentReject> {
        let now = now_ms();
        let (res, snapshot) = {
            let mut life = self.life.lock().unwrap();
            let r = steadycore::accept_intent(&mut life, it, now);
            (r, life.clone())
        };

        match &res {
            Ok(done) => {
                if let Err(e) = self.store.save_life(&snapshot).await {
                    warn!(error = %e, "写 core_life 失败");
                }
                info!(
                    seq = done.seq,
                    action = %done.action,
                    why = %done.why,
                    say = done.say.is_some(),
                    "intent 执行了"
                );
                self.emit(
                    EventKind::TickDone,
                    json!({
                        "seq": done.seq,
                        "action": done.action,
                        "reason": format!("intent：{}", done.why),
                        "weight": done.weight,
                        "intent": true,
                        "say": done.say,
                        "mood": snapshot.mood(),
                        "trigger": "intent",
                    }),
                )
                .await;
            }
            Err(e) => {
                info!(code = e.code(), message = %e.message(), "intent 被拒");
                self.emit(
                    EventKind::IntentPending,
                    json!({
                        "action": it.action,
                        "rejected": true,
                        "code": e.code(),
                        "message": e.message(),
                    }),
                )
                .await;
            }
        }
        res
    }
}
