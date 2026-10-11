//! News flash (big-features additions): a Following preset tied to a news programme ("BBC News
//! at One"), matched each day from the guide. At broadcast time it plays in the pinned pop-out
//! without taking focus (countdown toast: Cancel / Open full), for the whole programme or the
//! first N minutes, then closes and puts back what was playing. OWNER: F-AB (engine) / F-C (UI).
//!
//! Never interrupts a film or episode being watched (a notification with Watch instead), never
//! in quiet hours, never while another app is fullscreen, nothing when that news channel is
//! already playing. On a one-connection account the flash takes the other playlist's copy, or
//! briefly takes over and restores.

use serde::{Deserialize, Serialize};

use crate::model::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "t", content = "min")]
pub enum FlashLength {
    Whole,
    /// The first N minutes, then close.
    Headlines(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FlashVolume {
    #[default]
    Normal,
    /// Lower what plays under it.
    Duck,
    /// "Silent flash" (subtitles when available).
    Mute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashSpec {
    pub length: FlashLength,
    pub volume: FlashVolume,
}

impl Default for FlashSpec {
    fn default() -> Self {
        Self { length: FlashLength::Headlines(5), volume: FlashVolume::Normal }
    }
}

/// Why a flash doesn't pop up now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashSkip {
    QuietHours,
    /// Another app is fullscreen (Hyprland).
    Fullscreen,
    /// A film or episode is playing: a notification with Watch instead.
    WouldInterrupt,
    /// That news channel is already playing.
    AlreadyWatching,
}

/// Quiet hours at local minute `minute_of_day` (`from > to` wraps midnight). Pure.
pub fn in_quiet_hours(cfg: &crate::config::NewsFlashConfig, minute_of_day: u16) -> bool {
    match (cfg.quiet_from_min, cfg.quiet_to_min) {
        (Some(from), Some(to)) if from <= to => (from..to).contains(&minute_of_day),
        (Some(from), Some(to)) => minute_of_day >= from || minute_of_day < to,
        _ => false,
    }
}

/// When a running flash ends (`None`: with the programme).
pub fn ends_at(spec: &FlashSpec, start: Timestamp) -> Option<Timestamp> {
    match spec.length {
        FlashLength::Whole => None,
        FlashLength::Headlines(min) => Some(start + i64::from(min) * 60),
    }
}
