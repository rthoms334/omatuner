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

/// What the app knows when a news flash is due (the reminder scheduler asks
/// `store::follow::flash_of` first, then [`decide`]).
#[derive(Debug, Clone, Default)]
pub struct FlashCtx {
    /// Local minute of the day now (quiet hours).
    pub minute_of_day: u16,
    /// Another app is fullscreen ([`other_app_fullscreen`]; only asked when
    /// `news_flash.skip_when_fullscreen`).
    pub other_fullscreen: bool,
    /// What the main picture plays (`None`: nothing).
    pub main_kind: Option<crate::playback::Kind>,
    /// Multi-view is on: an automatic switch goes to the silent picture, never interrupting.
    pub multiview: bool,
    /// The channels playing now (every picture).
    pub playing: Vec<crate::model::ItemRef>,
}

/// What a due news flash does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashDecision {
    /// Pop up in the pop-out (no focus taken) with the countdown toast; close at `ends_at`
    /// (`None`: with the programme) and put back what was playing.
    Pop { ends_at: Option<Timestamp>, volume: FlashVolume },
    /// A notification with Watch instead (a film or episode is playing: never interrupted).
    Notify(FlashSkip),
    /// Nothing at all (quiet hours, another app fullscreen, the bulletin already on).
    Skip(FlashSkip),
}

/// Decide a due news flash for `channel` (and its `copies`: the same bulletin elsewhere)
/// starting at `start` (the user's decisions, 2026-10-11: automatic things never interrupt a
/// film or episode; with multi-view on they go to the silent picture). Pure.
pub fn decide(
    spec: &FlashSpec,
    cfg: &crate::config::NewsFlashConfig,
    ctx: &FlashCtx,
    channel: &crate::model::ItemRef,
    copies: &[crate::model::ItemRef],
    start: Timestamp,
) -> FlashDecision {
    if ctx.playing.iter().any(|p| p == channel || copies.contains(p)) {
        return FlashDecision::Skip(FlashSkip::AlreadyWatching);
    }
    if in_quiet_hours(cfg, ctx.minute_of_day) {
        return FlashDecision::Skip(FlashSkip::QuietHours);
    }
    if cfg.skip_when_fullscreen && ctx.other_fullscreen {
        return FlashDecision::Skip(FlashSkip::Fullscreen);
    }
    if !ctx.multiview && matches!(ctx.main_kind, Some(crate::playback::Kind::Vod | crate::playback::Kind::Catchup)) {
        return FlashDecision::Notify(FlashSkip::WouldInterrupt);
    }
    FlashDecision::Pop { ends_at: ends_at(spec, start), volume: spec.volume }
}

/// Where the flash plays on accounts allowing one stream each (both of the user's): a copy on a
/// playlist that isn't streaming now, so whatever plays keeps playing; else `channel` itself —
/// it briefly takes over that playlist's stream and the previous playback is restored after.
/// Returns (the channel, takes over).
pub fn pick<'a>(
    channel: &'a crate::model::ItemRef,
    copies: &'a [crate::model::ItemRef],
    busy: &[crate::model::SourceId],
) -> (&'a crate::model::ItemRef, bool) {
    std::iter::once(channel).chain(copies).find(|c| !busy.contains(&c.source)).map_or((channel, true), |c| (c, false))
}

/// Another app's window is fullscreen (Hyprland's active window; not ours). Blocking: one
/// `hyprctl` run — call it from the scheduler's worker, never the UI thread. Outside Hyprland:
/// false.
pub fn other_app_fullscreen() -> bool {
    if !crate::platform::hypr::is_hyprland() {
        return false;
    }
    match std::process::Command::new("hyprctl").args(["-j", "activewindow"]).output() {
        Ok(out) if out.status.success() => fullscreen_in(&out.stdout, std::process::id()),
        _ => false,
    }
}

/// `hyprctl -j activewindow` says a window other than process `own_pid`'s is fullscreen
/// (`fullscreen` is a number in Hyprland ≥ 0.42 — 0 none, 1 maximised, 2 fullscreen — and a
/// bool before; maximised doesn't count).
pub fn fullscreen_in(json: &[u8], own_pid: u32) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else { return false };
    let full = match v.get("fullscreen") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().is_some_and(|n| n >= 2),
        _ => false,
    };
    full && v.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(own_pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};
    use crate::playback::Kind;

    fn ch(src: i64, key: &str) -> ItemRef {
        ItemRef { source: SourceId(src), kind: ItemKind::Channel, key: ItemKey(key.into()) }
    }

    /// The user's rules for a news flash: never over a film or episode (a notification
    /// instead), never in quiet hours or over another app's fullscreen, nothing when the
    /// bulletin is already on; headlines-only closes after N minutes.
    #[test]
    fn decide_honours_the_no_interrupt_rule() {
        let spec = FlashSpec::default();
        let cfg = crate::config::NewsFlashConfig {
            quiet_from_min: Some(22 * 60),
            quiet_to_min: Some(7 * 60),
            skip_when_fullscreen: true,
            headlines_min: 5,
        };
        let (bbc, bbc2) = (ch(1, "bbc1"), ch(2, "bbc1-hd"));
        let at = |minute, main: Option<Kind>| FlashCtx { minute_of_day: minute, main_kind: main, ..Default::default() };
        let pop = FlashDecision::Pop { ends_at: Some(1000 + 300), volume: FlashVolume::Normal };
        assert_eq!(decide(&spec, &cfg, &at(13 * 60, None), &bbc, &[], 1000), pop);
        assert_eq!(decide(&spec, &cfg, &at(13 * 60, Some(Kind::Live)), &bbc, &[], 1000), pop, "live TV is put back after");
        assert_eq!(decide(&spec, &cfg, &at(13 * 60, Some(Kind::Vod)), &bbc, &[], 1000), FlashDecision::Notify(FlashSkip::WouldInterrupt));
        assert_eq!(decide(&spec, &cfg, &at(13 * 60, Some(Kind::Catchup)), &bbc, &[], 1000), FlashDecision::Notify(FlashSkip::WouldInterrupt));
        let mv = FlashCtx { multiview: true, ..at(13 * 60, Some(Kind::Vod)) };
        assert_eq!(decide(&spec, &cfg, &mv, &bbc, &[], 1000), pop, "multi-view: the silent picture");
        assert_eq!(decide(&spec, &cfg, &at(23 * 60, None), &bbc, &[], 1000), FlashDecision::Skip(FlashSkip::QuietHours));
        assert_eq!(decide(&spec, &cfg, &at(6 * 60 + 59, None), &bbc, &[], 1000), FlashDecision::Skip(FlashSkip::QuietHours));
        let full = FlashCtx { other_fullscreen: true, ..at(13 * 60, None) };
        assert_eq!(decide(&spec, &cfg, &full, &bbc, &[], 1000), FlashDecision::Skip(FlashSkip::Fullscreen));
        let watching = FlashCtx { playing: vec![bbc2.clone()], ..at(13 * 60, Some(Kind::Live)) };
        assert_eq!(decide(&spec, &cfg, &watching, &bbc, std::slice::from_ref(&bbc2), 1000), FlashDecision::Skip(FlashSkip::AlreadyWatching));
        let whole = FlashSpec { length: FlashLength::Whole, volume: FlashVolume::Mute };
        assert_eq!(decide(&whole, &cfg, &at(13 * 60, None), &bbc, &[], 1000), FlashDecision::Pop { ends_at: None, volume: FlashVolume::Mute });

        // one connection per account: the other playlist's copy, else take over and restore
        assert_eq!(pick(&bbc, std::slice::from_ref(&bbc2), &[SourceId(1)]), (&bbc2, false));
        assert_eq!(pick(&bbc, std::slice::from_ref(&bbc2), &[]), (&bbc, false));
        assert_eq!(pick(&bbc, std::slice::from_ref(&bbc2), &[SourceId(1), SourceId(2)]), (&bbc, true));
    }

    #[test]
    fn hyprland_fullscreen() {
        assert!(fullscreen_in(br#"{"fullscreen": 2, "pid": 7}"#, 1));
        assert!(!fullscreen_in(br#"{"fullscreen": 2, "pid": 1}"#, 1), "our own window");
        assert!(!fullscreen_in(br#"{"fullscreen": 1, "pid": 7}"#, 1), "maximised");
        assert!(fullscreen_in(br#"{"fullscreen": true, "pid": 7}"#, 1));
        assert!(!fullscreen_in(b"{}", 1) && !fullscreen_in(b"no json", 1));
    }
}
