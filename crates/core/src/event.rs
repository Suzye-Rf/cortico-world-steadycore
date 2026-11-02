//! outbox：核心往外发的事件，**只追加不修改**，每条带单调自增的 `seq`。
//!
//! 宿主用 `GET /v1/events?since=<seq>` 拉。用 seq 而不是时间戳，
//! 是因为时间戳会重、会乱序，而 seq 天然可以当游标去重。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 生活层想做点事，等宿主决定要不要叫醒我
    IntentPending,
    /// 某个状态量越线了（憋太久 / 闷太久 / 卡住了）
    StateThreshold,
    /// 一次 tick 跑完了
    TickDone,
    /// 记忆层有急事（P2 之后才会真的出现）
    MemUrgent,
    /// 过了闸门，真该叫一次模型了。带着缩小的开机包（§10.6）
    WakeRequest,
}

impl EventKind {
    /// 落到 JSON / 日志里的名字，跟设计文档 §12.1 对齐
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::IntentPending => "intent.pending",
            EventKind::StateThreshold => "state.threshold",
            EventKind::TickDone => "tick.done",
            EventKind::MemUrgent => "mem.urgent",
            EventKind::WakeRequest => "wake.request",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreEvent {
    pub seq: i64,
    pub ts_ms: i64,
    /// 用 `kind` 这个名字，跟 API 里的事件字段对上
    pub event_type: String,
    pub payload: Value,
}

impl CoreEvent {
    pub fn new(seq: i64, ts_ms: i64, kind: EventKind, payload: Value) -> Self {
        Self {
            seq,
            ts_ms,
            event_type: kind.as_str().to_string(),
            payload,
        }
    }
}
