//! The in-app sleep timer (skill plan; B6 hook 6): it outlives the Claude session because it
//! runs in Tuner. OWNER: remind (engine) / rui (menu, toast, OSD countdown).

use crate::model::Timestamp;

/// When it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepWhen {
    At(Timestamp),
    /// When the programme / film playing now ends.
    EndOfProgramme,
    /// After `n` items: queue items while Up next drives playback, else episodes.
    AfterItems(u32),
    /// When Up next runs out.
    EndOfQueue,
}

/// What it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SleepAction {
    /// Stop every picture.
    #[default]
    Stop,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepTimer {
    pub when: SleepWhen,
    pub action: SleepAction,
    /// Warning toast this long before (`reminders.sleep_warn_s`).
    pub warn_s: u32,
    pub set_at: Timestamp,
}
