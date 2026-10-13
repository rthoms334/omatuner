//! Firing while Tuner is closed (B6 hooks 3–4): transient `systemd-run --user` timers that run
//! `tuner --reminder <id>`. Units are described "Tuner reminder" and carry only the id —
//! `systemctl --user list-timers` would otherwise bypass the spoiler guard and the "no titles
//! at INFO" rule; Tuner looks the reminder up and guards it at fire time
//! ([`super::closed`]). OWNER: remind. The units themselves: `platform::systemd`.
//!
//! The running scheduler keeps the units in step with the pending reminders (every reload), so
//! they are armed whenever Tuner quits or crashes. A unit that fires while Tuner runs hands
//! `--reminder <id>` to it, which only re-reads its reminders (the row fires once: a fired row
//! is no longer pending). Transient units don't outlive the user manager: `tuner --rearm`
//! ([`rearm_at_login`], the packaged `tuner-rearm.service` at every login) and a headless
//! `tuner --refresh` ([`rearm`]) arm them again with Tuner closed, from [`closed_plan`] — what
//! a Tuner started then would arm, and a unit right away for what it would fire late (DESIGN
//! §36.2).

use std::sync::Arc;

use crate::error::Result;
use crate::model::{Reminder, ReminderId, Timestamp};
use crate::platform::systemd::{ArmStatus, ArmSync, Armer};

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

/// After a headless `tuner --refresh` (no scheduler runs): put the units in step with the
/// pending reminders, so the ones Following just made or moved fire while Tuner stays closed.
/// Blocking (`systemctl`); errors are logged.
pub fn rearm(conn: &rusqlite::Connection, cfg: &crate::config::Config, paths: &crate::paths::AppPaths) {
    if !super::ENABLED || !cfg.reminders.when_closed {
        return;
    }
    let Some(armer) = crate::platform::systemd::SystemdArmer::for_profile(paths) else { return };
    match closed_plan(conn, cfg, crate::model::now()) {
        Ok(want) => ArmSync::new(Arc::new(armer)).sync(&want),
        Err(e) => tracing::warn!("reminders: re-arming after the refresh: {e}"),
    }
}

/// `tuner --rearm`, run at every login by the packaged `tuner-rearm.service` (DESIGN §36.2):
/// transient units are gone after a reboot or logout, and nothing else re-arms them until Tuner
/// next starts. Arms what a Tuner started now would arm or fire late ([`closed_plan`]),
/// offline, without a window, then returns what arming did (`Unavailable`: systemd refused).
/// Blocking. `Off` (nothing done) with the setting off, for a `--profile-dir` profile, without
/// `systemd-run`, and without a database yet (Tuner never started: nothing is made). The caller holds the instance lock (a
/// running Tuner arms itself). Session variables: see `platform::systemd::SystemdArmer`.
pub fn rearm_at_login(paths: &crate::paths::AppPaths, cfg: &crate::config::Config) -> Result<ArmStatus> {
    if !super::ENABLED || !cfg.reminders.when_closed {
        return Ok(ArmStatus::Off);
    }
    let Some(armer) = crate::platform::systemd::SystemdArmer::for_profile(paths) else { return Ok(ArmStatus::Off) };
    rearm_profile(paths, cfg, Arc::new(armer), crate::model::now())
}

/// [`rearm_at_login`] with its armer. A fresh [`ArmSync`] learns what is armed first, so a
/// second run (a re-login while the user manager stayed up) re-arms the same units, no more.
fn rearm_profile(paths: &crate::paths::AppPaths, cfg: &crate::config::Config, armer: Arc<dyn Armer>, now: Timestamp) -> Result<ArmStatus> {
    let db = paths.db_file();
    if !db.exists() {
        return Ok(ArmStatus::Off);
    }
    let store = crate::store::Store::open(&db, None)?;
    let want = store.reader().and_then(|conn| closed_plan(&conn, cfg, now));
    store.close(std::time::Duration::from_secs(3));
    let mut sync = ArmSync::new(armer);
    sync.sync(&want?);
    Ok(sync.status().clone())
}

/// What a Tuner started at `now` arms: [`plan`] over its scheduler's pending reminders — a
/// Following rule's wait while Following is off, explicit channels' while parental control
/// hides them (the session counts as locked, as in [`super::closed`]) — and, first, a unit
/// [`LATE_UNIT_S`] from now for each one it would fire late on starting (its time passed while
/// nothing ran, e.g. during the reboot: [`fires_late`]), which nothing else fires with Tuner
/// closed.
pub fn closed_plan(conn: &rusqlite::Connection, cfg: &crate::config::Config, now: Timestamp) -> Result<Vec<(ReminderId, Timestamp)>> {
    let mut pending = crate::store::user::pending_reminders(conn, Timestamp::MIN)?;
    if !cfg.follow.enabled {
        pending.retain(|r| r.rule_id.is_none());
    }
    if cfg.parental.hides_explicit(!cfg.parental.pin_active()) {
        pending.retain(|r| !crate::store::live::item_explicit(conn, &r.channel).unwrap_or(true));
    }
    let lead = i64::from(cfg.reminders.lead_min) * 60;
    let late = pending.iter().filter(|r| fires_late(r, now, lead, cfg.reminders.enabled)).map(|r| (r.id, now + LATE_UNIT_S));
    Ok(late.chain(plan(&pending, now, lead)).take(MAX_UNITS).collect())
}

/// A late unit (see [`closed_plan`]) elapses this soon.
pub const LATE_UNIT_S: i64 = 5;

/// `r`'s unit time has passed but a Tuner starting at `now` still fires it (the scheduler's
/// rules, which `closed::fire` repeats): a note while its programme airs (with notes on), a
/// switch up to `LATE_SWITCH_S` after its time.
fn fires_late(r: &Reminder, now: Timestamp, lead_default_s: i64, notes: bool) -> bool {
    r.state == crate::model::ReminderState::Pending
        && unit_at(r, lead_default_s) <= now
        && super::effective_stop(r) > now
        && if r.switches() { now <= super::switch_at(r) + crate::platform::notify::LATE_SWITCH_S } else { notes }
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

    /// `tuner --rearm` at login arms what the running scheduler arms
    /// (`platform::notify`'s `scheduler_keeps_the_units_in_step`: same rows, same units), plus
    /// a unit right away for what a starting Tuner would fire late (a note still airing, a
    /// switch within `LATE_SWITCH_S`; not one past those), drops a unit left from before, and a
    /// second login re-arms the same set. Before Tuner ever started there is no database:
    /// nothing is armed, nothing is made.
    #[test]
    fn login_rearm_arms_what_the_running_app_arms() {
        use crate::platform::systemd::tests::FakeArmer;
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::rooted(dir.path());
        let cfg = crate::config::Config::default();
        let fake = Arc::new(FakeArmer::default());
        let now = crate::model::now();
        assert_eq!(rearm_profile(&paths, &cfg, fake.clone(), now).unwrap(), ArmStatus::Off);
        assert!(fake.calls.lock().unwrap().is_empty() && !paths.db_file().exists(), "no profile: nothing touched");

        paths.ensure().unwrap();
        let store = crate::store::Store::open(&paths.db_file(), None).unwrap();
        store
            .writer()
            .blocking(move |c| {
                c.execute(
                    "INSERT INTO source(id, name, kind, url, created_at, generation) VALUES (1, 'S', 'm3u-url', 'http://127.0.0.1/p.m3u', 0, 1)",
                    [],
                )?;
                let sw = |id, start| {
                    let mut s = r(id, start);
                    s.mode = ReminderMode::Switch;
                    s
                };
                let late = crate::platform::notify::LATE_SWITCH_S;
                // 4, 5: due during the reboot, still fired late; 6, 7: too late
                for row in [
                    r(1, now + 3600),
                    sw(2, now + 7200),
                    r(3, now + ARM_WINDOW_S + 7200),
                    r(4, now - 60),
                    sw(5, now - 300),
                    sw(6, now - late - 60),
                    r(7, now - 7200),
                ] {
                    crate::store::user::add_reminder(c, &row)?;
                }
                Ok(())
            })
            .unwrap();
        assert!(store.close(std::time::Duration::from_secs(3)));
        fake.units.lock().unwrap().insert(ReminderId(99), now + 60); // a reminder deleted since
        let want = [
            (ReminderId(1), now + 3600 - 120),
            (ReminderId(2), now + 7200 - Reminder::SWITCH_WARN_S - LAUNCH_EARLY_S),
            (ReminderId(4), now + LATE_UNIT_S),
            (ReminderId(5), now + LATE_UNIT_S),
        ];
        for _login in 0..2 {
            assert_eq!(rearm_profile(&paths, &cfg, fake.clone(), now).unwrap(), ArmStatus::Armed(4));
            assert_eq!(*fake.units.lock().unwrap(), want.into_iter().collect());
        }
    }

    #[test]
    fn unit_names_round_trip() {
        assert_eq!(unit_id(&format!("{}.timer", unit_name(ReminderId(42)))), Some(ReminderId(42)));
        assert_eq!(unit_id("tuner-reminder-9.service"), Some(ReminderId(9)));
        assert_eq!(unit_id("tuner-reminder-x.timer"), None);
        assert_eq!(unit_id("other-7.timer"), None);
    }
}
