//! Firing while Tuner is closed (B6 hooks 3–4): transient `systemd-run --user` timers that run
//! `tuner --reminder <id>`. Units are described "Tuner reminder" and carry only the id —
//! `systemctl --user list-timers` would otherwise bypass the spoiler guard and the "no titles
//! at INFO" rule; Tuner looks the reminder up and guards it at fire time
//! ([`super::closed`]). OWNER: remind. The units themselves: `platform::systemd`.
//!
//! The running scheduler keeps the units in step with the pending reminders (every reload), so
//! they are armed whenever Tuner quits or crashes. A unit that fires while Tuner runs hands
//! `--reminder <id>` to it, which only re-reads its reminders (the row fires once: a fired row
//! is no longer pending).

use crate::model::{Reminder, ReminderId, Timestamp};

/// Arm only reminders that fire within this window…
pub const ARM_WINDOW_S: i64 = 48 * 3600;
/// …and at most this many units.
pub const MAX_UNITS: usize = 30;
/// The unit description (nothing else describes it).
pub const UNIT_DESCRIPTION: &str = "Tuner reminder";
/// A switch's unit starts Tuner this much before its countdown, so the window (or pop-out) is
/// up when the 10 s Cancel toast shows.
pub const LAUNCH_EARLY_S: i64 = 30;
/// Unit names start with this (one per reminder id).
pub const UNIT_PREFIX: &str = "tuner-reminder-";

/// The transient unit's name for reminder `id`.
pub fn unit_name(id: ReminderId) -> String {
    format!("{UNIT_PREFIX}{}", id.0)
}

/// The reminder id of one of our unit names (`tuner-reminder-7.timer` → 7).
pub fn unit_id(unit: &str) -> Option<ReminderId> {
    let rest = unit.strip_prefix(UNIT_PREFIX)?;
    let digits = rest.strip_suffix(".timer").or_else(|| rest.strip_suffix(".service")).unwrap_or(rest);
    digits.parse().ok().filter(|n: &i64| *n > 0).map(ReminderId)
}

/// The command a unit runs: this Tuner (an install outside the user manager's `PATH` too).
pub fn command(id: ReminderId) -> [String; 3] {
    [exe(), "--reminder".to_owned(), id.0.to_string()]
}

/// The running Tuner's executable, else (replaced by an upgrade, unreadable) Tuner by name.
pub fn exe() -> String {
    std::env::current_exe()
        .ok()
        .filter(|p| p.is_file())
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| crate::APP_ID.to_owned())
}

/// When `r` fires in a running Tuner (`lead_default_s` for rows without their own lead;
/// switches at their countdown, `SWITCH_WARN_S` before start + offset).
pub fn fire_at(r: &Reminder, lead_default_s: i64) -> Timestamp {
    if r.switches() { super::switch_at(r) - Reminder::SWITCH_WARN_S } else { super::notify_at(r, lead_default_s) }
}

/// When `r`'s unit elapses: at its fire time, a switch [`LAUNCH_EARLY_S`] earlier.
pub fn unit_at(r: &Reminder, lead_default_s: i64) -> Timestamp {
    fire_at(r, lead_default_s) - if r.switches() { LAUNCH_EARLY_S } else { 0 }
}

/// The reminders to arm now and when their units elapse: pending, elapsing in `(now, now +
/// ARM_WINDOW_S]`, soonest first, at most [`MAX_UNITS`]. Re-planned whenever the scheduler
/// reloads (every change, every Following evaluation, at least once a minute).
pub fn plan(pending: &[Reminder], now: Timestamp, lead_default_s: i64) -> Vec<(ReminderId, Timestamp)> {
    let mut due: Vec<(Timestamp, ReminderId)> = pending
        .iter()
        .filter(|r| r.state == crate::model::ReminderState::Pending)
        .map(|r| (unit_at(r, lead_default_s), r.id))
        .filter(|(at, _)| *at > now && *at <= now + ARM_WINDOW_S)
        .collect();
    due.sort();
    due.into_iter().take(MAX_UNITS).map(|(at, id)| (id, at)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, ReminderMode, SourceId};

    fn r(id: i64, start: Timestamp) -> Reminder {
        let mut r = Reminder::notify(
            ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(format!("k{id}")) },
            "Secret Channel".into(),
            "Secret Final".into(),
            start,
            start + 3600,
            120,
        );
        r.id = ReminderId(id);
        r
    }

    /// The arming window and the unit cap hold whatever Following made; units never name the
    /// programme.
    #[test]
    fn arms_the_soonest_thirty_within_two_days_and_names_nothing() {
        let now = 1_000_000;
        let mut pending: Vec<Reminder> = (1..=40).map(|i| r(i, now + 600 + i * 3600)).collect();
        pending.push(r(100, now + ARM_WINDOW_S + 7200)); // too far
        pending.push(r(101, now - 60)); // already due: the running app fires it
        let armed = plan(&pending, now, 120);
        assert_eq!(armed.len(), MAX_UNITS);
        assert_eq!(armed[0], (ReminderId(1), now + 600 + 3600 - 120));
        assert!(!armed.iter().any(|(id, _)| [ReminderId(100), ReminderId(101)].contains(id)));
        let cmd = command(ReminderId(7));
        assert_eq!(cmd[1..], ["--reminder", "7"]);
        assert!(std::path::Path::new(&cmd[0]).is_file(), "this executable: {cmd:?}");
        assert!(!unit_name(ReminderId(7)).contains("Secret") && !UNIT_DESCRIPTION.contains("Secret"));
    }

    /// A switch's unit starts Tuner early enough for the countdown; an offset moves both.
    #[test]
    fn switch_units_elapse_before_the_countdown() {
        let mut s = r(3, 10_000);
        s.mode = ReminderMode::Switch;
        s.offset_s = 300;
        assert_eq!(fire_at(&s, 120), 10_300 - Reminder::SWITCH_WARN_S);
        assert_eq!(unit_at(&s, 120), 10_300 - Reminder::SWITCH_WARN_S - LAUNCH_EARLY_S);
        assert_eq!(unit_at(&r(4, 10_000), 120), 10_000 - 120);
    }

    #[test]
    fn unit_names_round_trip() {
        assert_eq!(unit_id(&format!("{}.timer", unit_name(ReminderId(42)))), Some(ReminderId(42)));
        assert_eq!(unit_id("tuner-reminder-9.service"), Some(ReminderId(9)));
        assert_eq!(unit_id("tuner-reminder-x.timer"), None);
        assert_eq!(unit_id("other-7.timer"), None);
    }
}
