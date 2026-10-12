//! Up next types (DESIGN §32 §4). OWNER: Q-core (display fields with Q-ui).
//!
//! Order of play: the first *visible* pending row in list order (concealed rows are never
//! counted, a "Skip for now" slot waits until it is picked again). The UI's "Next" section is
//! exactly that order; played, skipped and failed rows stay in place, drawn under "Played".

use std::sync::Arc;

use crate::model::{EpisodeId, ItemRef, QueueId, Timestamp};

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

impl RowState {
    pub fn as_str(self) -> &'static str {
        match self {
            RowState::Pending => "pending",
            RowState::Current => "current",
            RowState::Played => "played",
            RowState::Skipped => "skipped",
            RowState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => RowState::Pending,
            "current" => RowState::Current,
            "played" => RowState::Played,
            "skipped" => RowState::Skipped,
            "failed" => RowState::Failed,
            _ => return None,
        })
    }
    /// Done with: drawn under "Played (n)", removed by "Remove played", pruned after 7 days.
    pub fn is_done(self) -> bool {
        matches!(self, RowState::Played | RowState::Skipped | RowState::Failed)
    }
}

/// A live programme slot (channel rows): displayed (shifted) UTC times, catch-up id if any.
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
    /// Its item vanished in a refresh (`store::user::HOLDERS`): "Unavailable", skipped.
    pub orphaned: bool,
}

/// One visible row of the snapshot (concealed rows are never in it, nor counted).
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRow {
    pub entry: QueueEntry,
    /// Needs the PIN now (dimmed while the session is locked).
    pub locked: bool,
    /// Expected start (ETA column), pending and current rows.
    pub eta: Option<Timestamp>,
    /// Reason shown on a skipped / failed / unavailable row.
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
    /// Empty queue + an episode ended: today's next-episode card, run by the engine
    /// ([`QueueSnapshot::next_episode`] names it).
    NextEpisode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Countdown {
    /// The row that plays when it runs out (`None` for [`CountdownWhy::NextEpisode`]).
    pub id: Option<QueueId>,
    /// When it plays; `None` = `[queue] ask`: the card waits for Play now.
    pub at: Option<Timestamp>,
    pub why: CountdownWhy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// The next item is locked and the session is locked: "Up next is waiting for your PIN".
    NeedsPin,
    /// A future slot: wait card until it starts ("Skip for now · n").
    NotStarted,
    /// 3 failed items in a row: "Up next paused: 3 items failed in a row".
    FailedStreak,
    /// Multi-view is on: "Up next waits while multi-view is on" (B1).
    MultiView,
}

/// Why Up next stopped by itself (the card after the last item).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedWhy {
    /// "That was Up next." (with the next episode offered, no countdown).
    EndOfQueue,
    /// "Stopped after Goldfinger, as asked".
    StopAfter(QueueId),
}

/// The episode after the one that ended (the next-episode countdown, or the offer on the
/// "That was Up next." card).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextEpisode {
    pub id: EpisodeId,
    /// "S02E06 · Episode title" (unguarded: the UI passes it through the spoiler guard).
    pub label: String,
    /// The series' name as the library shows it (the guard's subject).
    pub series: String,
    pub season: u32,
    pub episode: u32,
}

/// What every consumer reads (≤ 200 small rows).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueueSnapshot {
    pub rows: Arc<[QueueRow]>,
    pub current: Option<QueueId>,
    pub next: Option<QueueId>,
    pub countdown: Option<Countdown>,
    pub blocked: Option<Blocked>,
    /// The row [`QueueSnapshot::blocked`] is about (the locked or not-yet-started one).
    pub blocked_id: Option<QueueId>,
    pub stop_after: Option<QueueId>,
    /// (end, exact) — "ends ~03:12" / "ends ≥ 03:12".
    pub eta_end: Option<(Timestamp, bool)>,
    /// The user played something else (or stopped): Up next stepped aside ("Alt+6 to carry on").
    pub interrupted: bool,
    pub failed_streak: u8,
    /// Up next drives the main picture right now (`origin == Queue` on one of its items):
    /// MPRIS Next/Previous go to the queue, `RouteState::queue_running`.
    pub driving: bool,
    /// The card after the last item ("That was Up next.").
    pub finished: Option<FinishedWhy>,
    /// The next-episode countdown's episode, or the one offered on the finished card.
    pub next_episode: Option<NextEpisode>,
}

impl QueueSnapshot {
    /// Pending visible rows (the sidebar entry shows only while there are some).
    pub fn pending(&self) -> usize {
        self.rows.iter().filter(|r| matches!(r.entry.state, RowState::Pending | RowState::Current)).count()
    }

    pub fn row(&self, id: QueueId) -> Option<&QueueRow> {
        self.rows.iter().find(|r| r.entry.id == id)
    }

    /// Up next is between two of its items: the card after one (a countdown to its next row, a
    /// wait for the PIN, a future slot, the failure pause). It hasn't run out or stepped aside;
    /// the sleep timer's "end of Up next" must not fire here.
    pub fn between_items(&self) -> bool {
        self.countdown.as_ref().is_some_and(|c| c.why != CountdownWhy::NextEpisode) || self.blocked.is_some()
    }

    /// "3 of 7": the place of `id` among the visible rows that aren't skipped or failed.
    pub fn place(&self, id: QueueId) -> Option<(usize, usize)> {
        let counted: Vec<&QueueRow> = self.rows.iter().filter(|r| !matches!(r.entry.state, RowState::Skipped | RowState::Failed)).collect();
        Some((counted.iter().position(|r| r.entry.id == id)? + 1, counted.len()))
    }
}

/// Where an add or a move goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    End,
    /// Before the first pending row: it plays next (Ctrl+Shift+N).
    Next,
    /// Index in the whole list (0 = top).
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
    /// A movie, an episode, a series (its next episode) or a channel with `programme`.
    pub item: ItemRef,
    pub programme: Option<Slot>,
    pub from_start: bool,
    /// `false` = this copy.
    pub any_copy: bool,
}

/// Why a target wasn't added (the MCP `refused[].error`, the app's toast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Over MCP only: "locked: needs your PIN in Tuner".
    Locked,
    /// Missing, or explicit while hidden (indistinguishable on purpose).
    NotFound,
    /// Already pending.
    Duplicate,
    /// 200 rows, or more than 50 in one add.
    Full,
    /// A bare channel: "Live channels join Up next by programme; this one has no guide".
    NoGuide,
}

impl Refusal {
    /// The MCP error text.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Locked => "locked: needs your PIN in Tuner",
            Refusal::NotFound => "not_found",
            Refusal::Duplicate => "duplicate",
            Refusal::Full => "full",
            Refusal::NoGuide => "no_guide",
        }
    }
}

/// What a play request ended in (MCP `play`, `Proceed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayReply {
    Playing(QueueId),
    /// Locked while the session is: Tuner shows the PIN card (`waiting_for_pin: true`).
    WaitingForPin(QueueId),
    /// Multi-view is on, or a future slot: the card says so.
    Waiting(QueueId, Blocked),
    /// Nothing to play.
    Nothing,
}

/// The engine's answer to [`QueueHandle::request`](crate::queue::QueueHandle::request).
#[derive(Debug, Clone, PartialEq)]
pub enum QueueReply {
    Added {
        added: Vec<(QueueId, ItemRef)>,
        refused: Vec<(ItemRef, Refusal)>,
    },
    /// Rows removed (remove, clear).
    Removed(usize),
    Play(PlayReply),
    Done,
    /// The store failed (logged).
    Error(String),
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
    /// These rows, in this order, take the places they hold now among the list (the others
    /// stay where they are): "Shuffle the rest" in one transaction.
    Reorder(Vec<QueueId>),
    Remove(Vec<QueueId>),
    /// "Remove played" / "Clear Up next…": the rows the user can see (concealed rows stay).
    Clear {
        played_only: bool,
    },
    PlayAt(QueueId),
    /// The Resume prompt's answer on a row (Enter in the list): `resume` = "Resume from …"
    /// (whatever the row's "from the start" says), else "Start over". UI only.
    PlayAtFrom {
        id: QueueId,
        resume: bool,
    },
    /// `n`, MPRIS Next: skip to the next row (a locked or not-started one is skipped / left).
    Next,
    Prev,
    /// Skip the countdown.
    Now,
    /// Cancel the countdown or the card (the next row stays pending).
    Cancel,
    StopAfter(Option<QueueId>),
    FromStart(QueueId, bool),
    PinCopy(QueueId, ItemRef),
    /// The PIN card: `unlocked` = the PIN was entered (the UI publishes the unlock through
    /// `AppShared::set_unlocked_at` first — the engine re-reads the parental state and never
    /// unlocks anything itself); `false` = skip the locked item. UI only, never MCP.
    Proceed {
        unlocked: bool,
    },
    /// Carry on after an interruption ("Resume Up next").
    Resume,
}
