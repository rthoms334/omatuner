//! Up next types (DESIGN §32 §4). OWNER: Q-core.

use std::sync::Arc;

use crate::model::{ItemRef, QueueId, Timestamp};

/// A row's state (`queue_item.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RowState {
    #[default]
    Pending,
    Current,
    Played,
    Skipped,
    Failed,
}

/// A live programme slot (channel rows): displayed UTC times, catch-up id if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub start: Timestamp,
    pub stop: Timestamp,
    pub catchup_id: Option<String>,
}

/// One stored row.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueEntry {
    pub id: QueueId,
    pub item: ItemRef,
    /// The title's copy plan picks at play time (else this copy).
    pub any_copy: bool,
    pub programme: Option<Slot>,
    pub title: String,
    pub subtitle: Option<String>,
    pub duration_s: Option<f64>,
    pub from_start: bool,
    pub state: RowState,
    pub by_agent: bool,
}

/// One visible row of the snapshot (concealed rows are never in it, nor counted).
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRow {
    pub entry: QueueEntry,
    pub locked: bool,
    /// Expected start (ETA column).
    pub eta: Option<Timestamp>,
    /// Reason shown on a skipped / failed row.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountdownWhy {
    /// The item before ended.
    Ended,
    /// A live slot reached its stop (30 s "Keep watching").
    SlotEnded,
    /// The item before failed.
    Failed,
    /// Empty queue + an episode ended: today's next-episode card, run by the engine.
    NextEpisode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Countdown {
    pub id: Option<QueueId>,
    pub at: Timestamp,
    pub why: CountdownWhy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The next item is locked and the session is locked: "Up next is waiting for your PIN".
    NeedsPin,
    /// A future slot: wait card until it starts.
    NotStarted,
    /// 3 failed items in a row.
    FailedStreak,
}

/// What every consumer reads (≤ 200 small rows).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueueSnapshot {
    pub rows: Arc<[QueueRow]>,
    pub current: Option<QueueId>,
    pub next: Option<QueueId>,
    pub countdown: Option<Countdown>,
    pub blocked: Option<Blocked>,
    pub stop_after: Option<QueueId>,
    /// (end, exact) — "ends ~03:12" / "ends ≥ 03:12".
    pub eta_end: Option<(Timestamp, bool)>,
    /// The user played something else: Up next stepped aside ("Alt+6 to carry on").
    pub interrupted: bool,
    pub failed_streak: u8,
}

impl QueueSnapshot {
    /// Pending visible rows (the sidebar entry shows only while there are some).
    pub fn pending(&self) -> usize {
        self.rows.iter().filter(|r| matches!(r.entry.state, RowState::Pending | RowState::Current)).count()
    }
}

/// Where an add goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    End,
    /// After the current row (Ctrl+Shift+N).
    Next,
    Index(usize),
}

/// Who added it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddedBy {
    App,
    Agent,
}

/// What to add (≤ 50 per add).
#[derive(Debug, Clone, PartialEq)]
pub struct AddTarget {
    pub item: ItemRef,
    pub programme: Option<Slot>,
    pub from_start: bool,
    /// `false` = this copy.
    pub any_copy: bool,
}

/// Commands to the engine (UI, MPRIS, MCP).
#[derive(Debug, Clone, PartialEq)]
pub enum QueueCommand {
    Add {
        targets: Vec<AddTarget>,
        at: Insert,
        by: AddedBy,
    },
    Move {
        ids: Vec<QueueId>,
        to: Insert,
    },
    Remove(Vec<QueueId>),
    Clear {
        played_only: bool,
    },
    PlayAt(QueueId),
    Next,
    Prev,
    /// Skip the countdown.
    Now,
    /// Cancel the countdown (the next row stays pending).
    Cancel,
    StopAfter(Option<QueueId>),
    FromStart(QueueId, bool),
    PinCopy(QueueId, ItemRef),
    /// The PIN was entered (or the user skipped the locked item).
    Proceed {
        unlocked: bool,
    },
    /// Carry on after an interruption.
    Resume,
}
