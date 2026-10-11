//! The pure Up next state machine (DESIGN §32 §5): player transitions, commands, ticks,
//! parental changes and library swaps in; effects out. Unit-tested like `PlayerMachine`.
//! OWNER: Q-core.

use crate::model::Timestamp;
use crate::queue::model::{QueueCommand, QueueSnapshot};

/// What the machine reacts to.
#[derive(Debug, Clone)]
pub enum Input {
    Command(QueueCommand),
    /// The main picture's snapshot changed.
    Player(std::sync::Arc<crate::player::PlayerSnapshot>),
    Tick(Timestamp),
    /// Multi-view turned on / off (Up next waits while it is on).
    MultiView(bool),
}

/// What the engine must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Play this row (exactly one per advance).
    Play(crate::model::QueueId),
    /// Write a state change (one tiny transaction).
    Persist,
    /// Publish a new snapshot.
    Publish,
    Toast(String),
}

#[derive(Debug, Default)]
pub struct QueueMachine {
    snapshot: QueueSnapshot,
}

impl QueueMachine {
    pub fn snapshot(&self) -> &QueueSnapshot {
        &self.snapshot
    }

    /// One step. Placeholder until Q-core: nothing happens.
    pub fn step(&mut self, _input: Input, _now: Timestamp) -> Vec<Effect> {
        Vec::new()
    }
}
