//! Firing while Tuner is closed (B6 hooks 3–4): transient `systemd-run --user` timers that run
//! `tuner --reminder <id>`. Units are described "Tuner reminder" and carry only the id —
//! `systemctl --user list-timers` would otherwise bypass the spoiler guard and the "no titles
//! at INFO" rule; Tuner looks the reminder up and guards it at fire time. OWNER: remind.

use crate::model::{Reminder, ReminderId, Timestamp};

/// Arm only reminders that fire within this window…
pub const ARM_WINDOW_S: i64 = 48 * 3600;
/// …and at most this many units.
pub const MAX_UNITS: usize = 30;
/// The unit description (nothing else describes it).
pub const UNIT_DESCRIPTION: &str = "Tuner reminder";

/// The transient unit's name for reminder `id`.
pub fn unit_name(id: ReminderId) -> String {
    format!("tuner-reminder-{}", id.0)
}

/// The command a unit runs.
pub fn command(id: ReminderId) -> [String; 3] {
    [crate::APP_ID.to_owned(), "--reminder".to_owned(), id.0.to_string()]
}

/// When `r` fires (`lead_default_s` for rows without their own lead; switches at their
/// start + offset).
pub fn fire_at(r: &Reminder, lead_default_s: i64) -> Timestamp {
    if r.switches() { r.start + r.offset_s - Reminder::SWITCH_WARN_S } else { r.start - if r.lead_s > 0 { r.lead_s } else { lead_default_s.max(0) } }
}

/// The reminders to arm now: pending, firing in `(now, now + ARM_WINDOW_S]`, soonest first,
/// at most [`MAX_UNITS`]. Re-planned after each Following evaluation and on the daily tick.
pub fn plan(pending: &[Reminder], now: Timestamp, lead_default_s: i64) -> Vec<ReminderId> {
    let mut due: Vec<(Timestamp, ReminderId)> = pending
        .iter()
        .filter(|r| r.state == crate::model::ReminderState::Pending)
        .map(|r| (fire_at(r, lead_default_s), r.id))
        .filter(|(at, _)| *at > now && *at <= now + ARM_WINDOW_S)
        .collect();
    due.sort();
    due.into_iter().take(MAX_UNITS).map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};

    /// The arming window and the unit cap hold whatever Following made; units never name the
    /// programme.
    #[test]
    fn arms_the_soonest_thirty_within_two_days_and_names_nothing() {
        let r = |id: i64, start: Timestamp| {
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
        };
        let now = 1_000_000;
        let mut pending: Vec<Reminder> = (0..40).map(|i| r(i, now + 600 + i * 3600)).collect();
        pending.push(r(100, now + ARM_WINDOW_S + 7200)); // too far
        pending.push(r(101, now - 60)); // already due: the running app fires it
        let armed = plan(&pending, now, 120);
        assert_eq!(armed.len(), MAX_UNITS);
        assert_eq!(armed[0], ReminderId(0));
        assert!(!armed.contains(&ReminderId(100)) && !armed.contains(&ReminderId(101)));
        let cmd = command(ReminderId(7)).join(" ");
        assert_eq!(cmd, "tuner --reminder 7");
        assert!(!unit_name(ReminderId(7)).contains("Secret") && !UNIT_DESCRIPTION.contains("Secret"));
    }
}
