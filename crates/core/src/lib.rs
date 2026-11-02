//! 领域层：纯数据 + 纯函数，不碰 IO，方便单测。

pub mod bond;
pub mod event;
pub mod life;
pub mod memory;
pub mod model;
pub mod recall;
pub mod selfview;

pub use bond::Bond;
pub use event::{CoreEvent, EventKind};
pub use life::{
    accept_intent, decide, record_wake, simulate, step, touch, wake_brief, Intent, IntentDone,
    IntentReject, SimReport, TickOutcome, TouchDone, WakeBrief, WakeDecision, ACTIONS,
};
pub use memory::{draft, Memory, Reject, WriteIntent};
pub use model::{ActionRecord, LifeState, Pad, Produce};
pub use recall::{rank, Filter, Hit};
pub use selfview::{
    boot, make_identity, make_standard, Boot, BootBlock, Identity, Standard, StandardIntent,
};
