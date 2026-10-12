//! Up next, the play queue (DESIGN §32). Batch base wrote the contract; OWNERS:
//! * Q-core (engine): `model.rs` (with Q-ui for display fields), `machine.rs`, `service.rs`,
//!   `store::queue`, migration 0010;
//! * Q-ui: `queue/ui.rs` (new: add flows, series picker, mini chip, strip), the Up next screen,
//!   the player card and `u` panel.
//!
//! ```text
//!  UI thread ──QueueCommand──▶ queue engine (tokio) ──PlayerCommand::PlayCopy──▶ main picture
//!  MPRIS / MCP ─────────────▶   QueueMachine (pure)  ◀── Tiles::main_watch() ──┘
//!          ◀──── AppShared::subscribe_queue (watch<Arc<QueueSnapshot>>) ────
//! ```
//! Up next drives "the main picture" (the only one in single view) and waits while multi-view
//! is on (B1). It never fires while Tuner is closed and never auto-plays after a restart.

pub mod machine;
pub mod model;
pub mod service;
pub mod ui;

pub use model::{
    AddTarget, AddedBy, Blocked, Countdown, CountdownWhy, FinishedWhy, Insert, NextEpisode, PlayReply, QueueCommand, QueueEntry, QueueReply,
    QueueRow, QueueSnapshot, Refusal, RowState, Slot,
};
pub use service::{EngineDeps, PlayerPort, QueueHandle};

/// Q-ui flips this in its last commit (the engine runs, the UI shows Up next).
pub const ENABLED: bool = true;
