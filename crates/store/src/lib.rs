//! 存取层：只跟 mongodb 打交道，不放业务规则。
//!
//! P0 的范围很窄——`core_life` 一个文档 + 事件审计。检索、衰减、合并留到 P1。
//!
//! 一条硬约定：**mongo 挂了核心不许死。** 所有方法在连接不可用时就地返回，
//! 由 `/v1/health` 如实报 `mongo: "down"`，进程继续活着。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use steadycore::{Bond, CoreEvent, Identity, LifeState, Memory, Produce, Standard};
use mongodb::bson::{doc, Document};
use mongodb::options::{FindOneOptions, FindOptions, IndexOptions, ReplaceOptions};
use mongodb::IndexModel;
use mongodb::{Client, Database};
use serde_json::{json, Value};
use tracing::{info, warn};

pub const COL_LIFE: &str = "core_life";
pub const COL_EVENTS: &str = "core_events";
pub const COL_MEM: &str = "core_mem";
pub const COL_IDN: &str = "core_idn";
pub const COL_STD: &str = "core_std";
pub const COL_PRODUCE: &str = "core_produce";
pub const COL_BOND: &str = "core_bond";

pub struct Store {
    client: Option<Client>,
    db_name: String,
    up: AtomicBool,
    last_error: std::sync::Mutex<Option<String>>,
}

impl Store {
    /// 连不上不算错——只是 `up = false`，整个进程照常起来。
    pub async fn connect(uri: &str, db_name: &str) -> Self {
        let uri = with_selection_timeout(uri);
        let s = match tokio::time::timeout(Duration::from_secs(6), Client::with_uri_str(&uri)).await
        {
            Ok(Ok(c)) => {
                let st = Store {
                    client: Some(c),
                    db_name: db_name.to_string(),
                    up: AtomicBool::new(false),
                    last_error: std::sync::Mutex::new(None),
                };
                if let Err(e) = st.ping().await {
                    st.set_down(&format!("{e:#}"));
                }
                st
            }
            Ok(Err(e)) => {
                warn!(error = %e, "mongo uri 解析失败，核心将以无库模式运行");
                Store::null(db_name, format!("{e:#}"))
            }
            Err(_) => {
                warn!("mongo 连接超时，核心将以无库模式运行");
                Store::null(db_name, "connect timeout".to_string())
            }
        };

        if s.up() {
            info!(db = %s.db_name, "mongo 已连上");
        }
        s
    }

    fn null(db_name: &str, err: String) -> Self {
        Store {
            client: None,
            db_name: db_name.to_string(),
            up: AtomicBool::new(false),
            last_error: std::sync::Mutex::new(Some(err)),
        }
    }

    pub fn up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn set_down(&self, err: &str) {
        self.up.store(false, Ordering::Relaxed);
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(err.to_string());
        }
    }

    fn set_up(&self) {
        self.up.store(true, Ordering::Relaxed);
        if let Ok(mut g) = self.last_error.lock() {
            *g = None;
        }
    }

    fn db(&self) -> Option<Database> {
        self.client.as_ref().map(|c| c.database(&self.db_name))
    }

    /// 探活。失败就把自己标成 down，下次写之前会再探一次。
    pub async fn ping(&self) -> Result<()> {
        let Some(client) = self.client.as_ref() else {
            anyhow::bail!("没有 client");
        };
        let r = tokio::time::timeout(
            Duration::from_secs(3),
            client.database("admin").run_command(doc! { "ping": 1 }, None),
        )
        .await;

        match r {
            Ok(Ok(_)) => {
                self.set_up();
                Ok(())
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                self.set_down(&msg);
                Err(e).context("ping 失败")
            }
            Err(_) => {
                self.set_down("ping timeout");
                anyhow::bail!("ping 超时")
            }
        }
    }

    // ── core_life ─────────────────────────────────────────────────

    /// 覆盖写 `_id = "life"` 那一条。用 `replace_one + upsert`，
    /// 语义就是「整个状态是个整体，别拆成 $set 一片散沙」。
    pub async fn save_life(&self, state: &LifeState) -> Result<()> {
        let Some(db) = self.db() else { return Ok(()) };
        if !self.up() {
            return Ok(());
        }

        let mut d = bson::to_document(state).context("LifeState 序列化失败")?;
        d.insert("_id", "life");
        d.insert("mood", state.mood());
        d.insert("_version", state.version);

        let coll = db.collection::<Document>(COL_LIFE);
        let opts = ReplaceOptions::builder().upsert(true).build();
        match coll.replace_one(doc! { "_id": "life" }, d, opts).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = format!("{e:#}");
                self.set_down(&msg);
                Err(e).context("写 core_life 失败")
            }
        }
    }

    pub async fn load_life(&self) -> Result<Option<LifeState>> {
        let Some(db) = self.db() else { return Ok(None) };
        if !self.up() {
            return Ok(None);
        }
        let coll = db.collection::<Document>(COL_LIFE);
        let found = coll.find_one(doc! { "_id": "life" }, None).await?;
        match found {
            Some(mut d) => {
                d.remove("_id");
                d.remove("mood");
                d.remove("_version");
                Ok(Some(bson::from_document::<LifeState>(d)?))
            }
            None => Ok(None),
        }
    }

    // ── core_bond ─────────────────────────────────────────────────

    /// 交情账：`_id` 就是那个人的稳定键（QQ 号 / 昵称），一个键一份账。
    pub async fn save_bond(&self, b: &Bond) -> Result<()> {
        let Some(db) = self.db() else { return Ok(()) };
        if !self.up() {
            return Ok(());
        }
        let mut d = bson::to_document(b).context("Bond 序列化失败")?;
        d.insert("_id", b.who.clone());
        let coll = db.collection::<Document>(COL_BOND);
        let opts = ReplaceOptions::builder().upsert(true).build();
        match coll.replace_one(doc! { "_id": &b.who }, d, opts).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = format!("{e:#}");
                self.set_down(&msg);
                Err(e).context("写 core_bond 失败")
            }
        }
    }

    /// 全部交情账，分高的在前。
    pub async fn load_bonds(&self) -> Result<Vec<Bond>> {
        let Some(db) = self.db() else { return Ok(Vec::new()) };
        if !self.up() {
            return Ok(Vec::new());
        }
        let opts = FindOptions::builder().sort(doc! { "score": -1 }).build();
        let mut cur = db
            .collection::<Document>(COL_BOND)
            .find(doc! {}, opts)
            .await?;
        let mut out = Vec::new();
        while cur.advance().await? {
            let mut d = cur.deserialize_current()?;
            d.remove("_id");
            out.push(bson::from_document::<Bond>(d)?);
        }
        Ok(out)
    }

    pub async fn load_bond(&self, who: &str) -> Result<Option<Bond>> {
        let Some(db) = self.db() else { return Ok(None) };
        if !self.up() {
            return Ok(None);
        }
        let found = db
            .collection::<Document>(COL_BOND)
            .find_one(doc! { "_id": who }, None)
            .await?;
        match found {
            Some(mut d) => {
                d.remove("_id");
                Ok(Some(bson::from_document::<Bond>(d)?))
            }
            None => Ok(None),
        }
    }

    // ── 事件审计 ──────────────────────────────────────────────────

    /// P0 里事件只用于留痕：真正对外供的是内存里那个环形缓冲。
    ///
    /// `_id` 交给 mongo 自己生成（ObjectId），**不要拿 seq 当 `_id`**——
    /// 重启后如果 seq 从 1 重来，直接撞 `E11000 duplicate key`。
    /// seq 只当一个普通字段，靠 `max_event_seq()` 在启动时接上。
    pub async fn append_event(&self, ev: &CoreEvent) -> Result<()> {
        let Some(db) = self.db() else { return Ok(()) };
        if !self.up() {
            return Ok(());
        }
        let d = bson::to_document(ev)?;
        db.collection::<Document>(COL_EVENTS).insert_one(d, None).await?;
        Ok(())
    }

    /// 库里最后一条事件的 seq；启动时用它把内存里的计数器接上。
    pub async fn max_event_seq(&self) -> Result<i64> {
        let Some(db) = self.db() else { return Ok(0) };
        if !self.up() {
            return Ok(0);
        }
        let opts = FindOneOptions::builder()
            .sort(doc! { "seq": -1 })
            .projection(doc! { "seq": 1 })
            .build();
        let found = db
            .collection::<Document>(COL_EVENTS)
            .find_one(doc! {}, opts)
            .await?;
        Ok(found
            .and_then(|d| {
                d.get_i64("seq")
                    .ok()
                    .or_else(|| d.get_i32("seq").ok().map(|v| v as i64))
                    .or_else(|| d.get_f64("seq").ok().map(|v| v as i64))
            })
            .unwrap_or(0))
    }

    // ── 统计 ──────────────────────────────────────────────────────

    pub async fn count_mem(&self) -> u64 {
        let Some(db) = self.db() else { return 0 };
        if !self.up() {
            return 0;
        }
        match db
            .collection::<Document>(COL_MEM)
            .count_documents(doc! {}, None)
            .await
        {
            Ok(n) => n,
            Err(_) => 0,
        }
    }

    // ── 记忆（P1）──────────────────────────────────────────────

    /// 建索引。只在 `status/layer`、`scope/createdAt`、`strength` 上，
    /// 不建 text 索引——§7.1 说中文 FTS 要额外分析器，这个量级用不上。
    pub async fn ensure_indexes(&self) {
        let Some(db) = self.db() else { return };
        if !self.up() {
            return;
        }
        for (name, keys) in [
            ("mem_status_layer", doc! { "status": 1, "layer": 1 }),
            ("mem_scope_created", doc! { "scope": 1, "createdAt": -1 }),
            ("mem_strength", doc! { "strength": 1 }),
            ("mem_entities", doc! { "entities": 1 }),
        ] {
            let model = IndexModel::builder()
                .keys(keys)
                .options(IndexOptions::builder().name(name.to_string()).build())
                .build();
            if let Err(e) = db.collection::<Document>(COL_MEM).create_index(model, None).await {
                warn!(error = %e, index = name, "建索引失败（不挡事，扫描慢点而已）");
            }
        }
    }

    pub async fn insert_mem(&self, m: &Memory) -> Result<()> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let mut d = bson::to_document(m).context("Memory 序列化失败")?;
        d.insert("_id", m.id.clone());
        db.collection::<Document>(COL_MEM)
            .insert_one(d, None)
            .await
            .context("写 core_mem 失败")?;
        Ok(())
    }

    /// 取所有活着的记忆。P1 不做增量索引——几千条整个读进来，打分在内存里跑。
    pub async fn load_mems(&self, limit: i64) -> Result<Vec<Memory>> {
        let Some(db) = self.db() else { return Ok(Vec::new()) };
        if !self.up() {
            return Ok(Vec::new());
        }
        let opts = FindOptions::builder()
            .sort(doc! { "createdAt": -1 })
            .limit(limit)
            .build();
        let mut cur = db
            .collection::<Document>(COL_MEM)
            .find(doc! { "status": "active" }, opts)
            .await?;
        let mut out = Vec::new();
        while cur.advance().await? {
            let mut d = cur.deserialize_current()?;
            d.remove("_id");
            match bson::from_document::<Memory>(d) {
                Ok(m) => out.push(m),
                Err(e) => warn!(error = %e, "core_mem 里有一条读不出来，跳过"),
            }
        }
        Ok(out)
    }

    /// 召回之后记账：被想起一次，强度涨一点、lastAccess 往前推。
    pub async fn touch_mems(&self, ids: &[String], now_ms: i64) -> Result<()> {
        let Some(db) = self.db() else { return Ok(()) };
        if !self.up() || ids.is_empty() {
            return Ok(());
        }
        let coll = db.collection::<Document>(COL_MEM);
        for id in ids {
            let r = coll
                .update_one(
                    doc! { "_id": id },
                    doc! {
                        "$inc": { "accessCount": 1i64 },
                        "$set": { "lastAccess": now_ms },
                        "$mul": { "strength": 1.05f64 },
                    },
                    None,
                )
                .await;
            if let Err(e) = r {
                warn!(error = %e, id = %id, "更新记忆的访问计数失败");
            }
        }
        Ok(())
    }

    pub async fn set_pin(&self, id: &str, pinned: bool) -> Result<bool> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let r = db
            .collection::<Document>(COL_MEM)
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "pinned": pinned } },
                None,
            )
            .await?;
        Ok(r.modified_count > 0)
    }

    /// 删。`hard=false` 只是标 archived（还能翻出来），`hard=true` 才真删。
    pub async fn drop_mem(&self, id: &str, hard: bool) -> Result<bool> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let coll = db.collection::<Document>(COL_MEM);
        if hard {
            let r = coll.delete_one(doc! { "_id": id }, None).await?;
            Ok(r.deleted_count > 0)
        } else {
            let r = coll
                .update_one(
                    doc! { "_id": id },
                    doc! { "$set": { "status": "archived" } },
                    None,
                )
                .await?;
            Ok(r.modified_count > 0)
        }
    }

    /// 身份层的 `why` 片段，拿来做撞身份检测的关键词。
    pub async fn identity_whys(&self) -> Vec<String> {
        let Some(db) = self.db() else { return Vec::new() };
        if !self.up() {
            return Vec::new();
        }
        let mut cur = match db.collection::<Document>(COL_IDN).find(doc! {}, None).await {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Ok(true) = cur.advance().await {
            if let Ok(d) = cur.deserialize_current() {
                if let Ok(w) = d.get_str("why") {
                    for seg in w.split(['、', '，', ',', '。', '；', ';']) {
                        let seg = seg.trim();
                        if seg.chars().count() >= 2 {
                            out.push(seg.to_string());
                        }
                    }
                }
            }
        }
        out
    }

    /// 各层的条数、最老最新、待合并（§12 的 /v1/stats）
    pub async fn mem_stats(&self) -> Value {
        let Some(db) = self.db() else {
            return json!({ "mongo": "down" });
        };
        if !self.up() {
            return json!({ "mongo": "down" });
        }
        let coll = db.collection::<Document>(COL_MEM);
        let total = coll.count_documents(doc! {}, None).await.unwrap_or(0);
        let active = coll
            .count_documents(doc! { "status": "active" }, None)
            .await
            .unwrap_or(0);

        let mut layers = serde_json::Map::new();
        for l in [
            "identity", "standard", "verbatim", "thought", "technique", "fact", "relation",
            "event", "mood",
        ] {
            let n = coll
                .count_documents(doc! { "status": "active", "layer": l }, None)
                .await
                .unwrap_or(0);
            if n > 0 {
                layers.insert(l.to_string(), json!(n));
            }
        }

        let oldest = coll
            .find_one(
                doc! { "status": "active" },
                FindOneOptions::builder()
                    .sort(doc! { "createdAt": 1 })
                    .projection(doc! { "createdAt": 1, "text": 1 })
                    .build(),
            )
            .await
            .ok()
            .flatten();
        let newest = coll
            .find_one(
                doc! { "status": "active" },
                FindOneOptions::builder()
                    .sort(doc! { "createdAt": -1 })
                    .projection(doc! { "createdAt": 1, "text": 1 })
                    .build(),
            )
            .await
            .ok()
            .flatten();
        let weakest = coll
            .find_one(
                doc! { "status": "active" },
                FindOneOptions::builder()
                    .sort(doc! { "strength": 1 })
                    .projection(doc! { "strength": 1, "text": 1 })
                    .build(),
            )
            .await
            .ok()
            .flatten();

        json!({
            "mongo": "up",
            "total": total,
            "active": active,
            "layers": layers,
            "oldest": oldest.map(|d| json!({ "at": d.get("createdAt"), "text": d.get("text") })),
            "newest": newest.map(|d| json!({ "at": d.get("createdAt"), "text": d.get("text") })),
            "weakest": weakest.map(|d| json!({ "strength": d.get("strength"), "text": d.get("text") })),
        })
    }
    // ── 自我层（P2）──────────────────────────────────────────────

    /// 身份层只追加。`insert` 是唯一入口——没有 update，也没有 delete。
    pub async fn insert_identity(&self, i: &Identity) -> Result<()> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let mut d = bson::to_document(i).context("Identity 序列化失败")?;
        d.insert("_id", i.id.clone());
        db.collection::<Document>(COL_IDN)
            .insert_one(d, None)
            .await
            .context("写 core_idn 失败")?;
        Ok(())
    }

    pub async fn load_identities(&self) -> Vec<Identity> {
        let Some(db) = self.db() else { return Vec::new() };
        if !self.up() {
            return Vec::new();
        }
        let opts = FindOptions::builder()
            .sort(doc! { "createdAt": 1 })
            .build();
        let mut cur = match db.collection::<Document>(COL_IDN).find(doc! {}, opts).await {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Ok(true) = cur.advance().await {
            if let Ok(mut d) = cur.deserialize_current() {
                d.remove("_id");
                match bson::from_document::<Identity>(d) {
                    Ok(i) => out.push(i),
                    Err(e) => warn!(error = %e, "core_idn 里有一条读不出来，跳过"),
                }
            }
        }
        out
    }

    pub async fn insert_standard(&self, s: &Standard) -> Result<()> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let mut d = bson::to_document(s).context("Standard 序列化失败")?;
        d.insert("_id", s.id.clone());
        db.collection::<Document>(COL_STD)
            .insert_one(d, None)
            .await
            .context("写 core_std 失败")?;
        Ok(())
    }

    /// 旧版不删：标 `supersededBy` 并写清「旧版为什么错」。
    pub async fn supersede_standard(&self, old_id: &str, new_id: &str, reason: &str) -> Result<bool> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let r = db
            .collection::<Document>(COL_STD)
            .update_one(
                doc! { "_id": old_id },
                doc! { "$set": { "supersededBy": new_id, "supersededReason": reason } },
                None,
            )
            .await?;
        Ok(r.modified_count > 0)
    }

    pub async fn load_standards(&self) -> Vec<Standard> {
        let Some(db) = self.db() else { return Vec::new() };
        if !self.up() {
            return Vec::new();
        }
        let opts = FindOptions::builder()
            .sort(doc! { "createdAt": 1 })
            .build();
        let mut cur = match db.collection::<Document>(COL_STD).find(doc! {}, opts).await {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        while let Ok(true) = cur.advance().await {
            if let Ok(mut d) = cur.deserialize_current() {
                d.remove("_id");
                // 老文档里可能是 null，反序列化会炸，先补成空串
                if matches!(d.get("supersededReason"), Some(bson::Bson::Null)) {
                    d.insert("supersededReason", "");
                }
                match bson::from_document::<Standard>(d) {
                    Ok(s) => out.push(s),
                    Err(e) => warn!(error = %e, "core_std 里有一条读不出来，跳过"),
                }
            }
        }
        out
    }

    // ── 产出槽（P3，设计文档 §10.5）────────────────────────────

    /// 开一个待填的槽。同一个 id 重写就当覆盖（重试安全）。
    pub async fn insert_produce(&self, p: &Produce) -> Result<()> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let mut d = bson::to_document(p).context("Produce 序列化失败")?;
        d.insert("_id", p.id.clone());
        db.collection::<Document>(COL_PRODUCE)
            .replace_one(
                doc! { "_id": &p.id },
                d,
                ReplaceOptions::builder().upsert(true).build(),
            )
            .await
            .context("写 core_produce 失败")?;
        Ok(())
    }

    /// 还没人填的槽，老的排在前面。
    pub async fn pending_produces(&self, limit: i64) -> Result<Vec<Produce>> {
        let Some(db) = self.db() else { return Ok(Vec::new()) };
        if !self.up() {
            return Ok(Vec::new());
        }
        let opts = FindOptions::builder()
            .sort(doc! { "askedAtMs": 1 })
            .limit(limit)
            .build();
        let mut cur = db
            .collection::<Document>(COL_PRODUCE)
            .find(doc! { "status": "pending" }, opts)
            .await?;
        let mut out = Vec::new();
        while cur.advance().await? {
            let mut d = cur.deserialize_current()?;
            d.remove("_id");
            match bson::from_document::<Produce>(d) {
                Ok(p) => out.push(p),
                Err(e) => warn!(error = %e, "core_produce 里有一条读不出来，跳过"),
            }
        }
        Ok(out)
    }

    /// 填完了 / 放掉了。`status` 是 `done` 或 `dropped`。
    pub async fn finish_produce(
        &self,
        id: &str,
        status: &str,
        text: &str,
        mem_id: &str,
        now_ms: i64,
    ) -> Result<bool> {
        let Some(db) = self.db() else {
            anyhow::bail!("没有库")
        };
        let r = db
            .collection::<Document>(COL_PRODUCE)
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$set": {
                        "status": status,
                        "text": text,
                        "memId": mem_id,
                        "doneAtMs": now_ms,
                    }
                },
                None,
            )
            .await
            .context("更新 core_produce 失败")?;
        Ok(r.modified_count > 0)
    }

    // ── 备份：把整库倒成 JSONL ──────────────────────────────────────────────

    /// 每个集合导成一个 `<collection>.jsonl`，一行一个文档（`_id` 带 `$oid` 之类照原样）。
    ///
    /// 为什么不走 `mongodump`：那玩意属于 MongoDB Database Tools，跟 mongod 是两个包，
    /// 这台机器上没装。为了备份去让人装一个额外的工具不划算——我本来就连着库、
    /// 手里就是 `Document`，自己倒一遍就好。
    ///
    /// **倒的是整个库，不是硬编码的几个集合**：以后加集合不用回来改这里。
    /// 返回每个集合的 `(名字, 条数)`。
    pub async fn dump_all(&self, dir: &std::path::Path) -> Result<Vec<(String, u64)>> {
        use std::io::Write;

        let Some(db) = self.db() else {
            anyhow::bail!("mongo 没连上，导不了");
        };
        std::fs::create_dir_all(dir)
            .with_context(|| format!("建不出备份目录 {}", dir.display()))?;

        let mut names = db.list_collection_names(None).await.context("列集合失败")?;
        names.sort();

        let mut out = Vec::new();
        for name in names {
            // 先写临时文件再改名：中途挂了也不会留下半份看起来像好的备份
            let tmp = dir.join(format!("{name}.jsonl.tmp"));
            let path = dir.join(format!("{name}.jsonl"));
            let mut f = std::io::BufWriter::new(
                std::fs::File::create(&tmp).with_context(|| format!("写不了 {}", tmp.display()))?,
            );

            let mut cur = db
                .collection::<Document>(&name)
                .find(doc! {}, None)
                .await
                .with_context(|| format!("读集合 {name} 失败"))?;

            let mut n: u64 = 0;
            while cur.advance().await? {
                let d = cur.deserialize_current()?;
                serde_json::to_writer(&mut f, &d).context("文档转 json 失败")?;
                f.write_all(b"\n")?;
                n += 1;
            }
            f.flush()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
            out.push((name, n));
        }

        // 一份小的清单，回头人在看目录时不用猜这堆文件是什么时候的
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let meta = serde_json::json!({
            "db": self.db_name,
            "atMs": at_ms,
            "collections": out.iter().map(|(n, c)| (n.clone(), *c)).collect::<std::collections::BTreeMap<_, _>>(),
            "total": out.iter().map(|(_, c)| *c).sum::<u64>(),
        });
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta)?,
        )?;

        Ok(out)
    }
}

/// 给 uri 塞一个短的选择超时，不然 mongod 没起来时会干等 30 秒。
///
/// 坑：mongodb 2.x 的解析器要求 options 前面必须有 `/`，
/// `mongodb://h:27017?serverSelectionTimeoutMS=2000` 会报
/// "Missing delimiting slash between hosts and options"。
/// 所以先把「host:port」和「/path」「?opts」理清楚再拼。
fn with_selection_timeout(uri: &str) -> String {
    if uri.contains("serverSelectionTimeoutMS") {
        return uri.to_string();
    }

    let (head, query) = match uri.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (uri, None),
    };

    // scheme:// 之后必须已经有 `/`（也就是 host:port 后面那段），否则补一个
    let has_path = head
        .split_once("://")
        .map(|(_, rest)| rest.contains('/'))
        .unwrap_or(false);
    let head = if has_path {
        head.to_string()
    } else {
        format!("{head}/")
    };

    match query {
        Some(q) if !q.is_empty() => format!("{head}?{q}&serverSelectionTimeoutMS=2000"),
        _ => format!("{head}?serverSelectionTimeoutMS=2000"),
    }
}

#[cfg(test)]
mod tests {
    use super::with_selection_timeout;

    #[test]
    fn uri_gets_a_slash_before_options() {
        assert_eq!(
            with_selection_timeout("mongodb://127.0.0.1:27017"),
            "mongodb://127.0.0.1:27017/?serverSelectionTimeoutMS=2000"
        );
    }

    #[test]
    fn uri_with_db_path_keeps_it() {
        assert_eq!(
            with_selection_timeout("mongodb://127.0.0.1:27017/steadycore"),
            "mongodb://127.0.0.1:27017/steadycore?serverSelectionTimeoutMS=2000"
        );
    }

    #[test]
    fn uri_with_existing_query_appends() {
        assert_eq!(
            with_selection_timeout("mongodb://127.0.0.1:27017/?authSource=admin"),
            "mongodb://127.0.0.1:27017/?authSource=admin&serverSelectionTimeoutMS=2000"
        );
    }

    #[test]
    fn uri_already_carrying_timeout_is_left_alone() {
        let u = "mongodb://127.0.0.1:27017/?serverSelectionTimeoutMS=8000";
        assert_eq!(with_selection_timeout(u), u);
    }
}
