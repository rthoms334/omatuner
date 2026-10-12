//! `tuner --reminder <id>` with no Tuner running: a systemd timer elapsed while Tuner was
//! closed ([`super::arming`]). OWNER: remind; `main.rs` (integrator) calls it once it owns the
//! instance lock and before it starts the window:
//!
//! ```text
//! match closed::fire(&store, &cfg, &guard, id, now)? {
//!     Exit           → exit 0
//!     StartApp{..}   → start Tuner as usual (the scheduler fires it; `popout` = in the pop-out,
//!                      without taking focus)
//!     Notify(note)   → release the instance lock and the store, then closed::notify_and_wait(&note)
//! }
//! ```
//!
//! A notification reminder never opens a window: the note goes out from this short-lived
//! process (the fallback when nothing should start), and its Watch launches `tuner --play
//! <ref>`. A switch starts Tuner (its countdown and the switch happen there). Parental control
//! counts the session as locked: explicit channels' reminders wait (silently, still pending) and
//! a Following switch to a locked channel becomes a "needs your PIN" note. Missed ones are
//! dismissed silently, like in a running Tuner.

use crate::config::Config;
use crate::error::Result;
use crate::guard::Guard;
use crate::model::{OpenIn, Reminder, ReminderId, ReminderState, Timestamp};
use crate::store::Store;

/// What the caller does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedOutcome {
    /// Nothing to show (gone, fired, not due, missed, hidden).
    Exit,
    /// Start Tuner: its scheduler fires the switch.
    StartApp { popout: bool },
    /// Show this note from here (no window).
    Notify(ClosedNote),
}

/// A note shown by the closed path; Watch runs `tuner --play <play>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedNote {
    pub summary: String,
    pub body: String,
    /// `<source>:<channel key>` for `--play`.
    pub play: String,
}

/// A unit that elapses this much before its time (clock change) is ignored: the next start
/// re-arms it.
const EARLY_SLACK_S: i64 = 120;

/// Decide, and record (`Fired` / `Dismissed`), what reminder `id` does now.
pub fn fire(store: &Store, cfg: &Config, guard: &Guard, id: i64, now: Timestamp) -> Result<ClosedOutcome> {
    let id = ReminderId(id);
    let conn = store.reader()?;
    let Some(r) = crate::store::user::reminder(&conn, id)? else { return Ok(ClosedOutcome::Exit) };
    if r.state != ReminderState::Pending {
        return Ok(ClosedOutcome::Exit);
    }
    // the session is locked while Tuner is closed (no PIN: unlocked)
    let unlocked = !cfg.parental.pin_active();
    if cfg.parental.hides_explicit(unlocked) && crate::store::live::item_explicit(&conn, &r.channel).unwrap_or(true) {
        return Ok(ClosedOutcome::Exit); // waits, untouched (DESIGN §20.3)
    }
    let set = |state| store.writer().blocking(move |c| crate::store::user::set_reminder_state(c, id, state));
    let stop = super::effective_stop(&r);
    if r.switches() {
        let at = super::switch_at(&r);
        if stop <= now || now > at + crate::platform::notify::LATE_SWITCH_S {
            set(ReminderState::Dismissed)?;
            return Ok(ClosedOutcome::Exit);
        }
        if at - now > super::arming::LAUNCH_EARLY_S + Reminder::SWITCH_WARN_S + EARLY_SLACK_S {
            return Ok(ClosedOutcome::Exit);
        }
        let ctx = super::FireCtx { target_locked: !unlocked && crate::store::user::channel_locked(&conn, &r.channel)?, ..super::FireCtx::idle(now) };
        return Ok(match super::fire_policy(&r, &ctx) {
            super::FireAs::Switch(_) => ClosedOutcome::StartApp { popout: super::open_in(&r, cfg.reminders.open_in) == OpenIn::PopOut },
            super::FireAs::Notify(why) => {
                set(ReminderState::Fired)?;
                ClosedOutcome::Notify(note(&r, guard, now, Some(why)))
            }
            super::FireAs::Skip(_) => {
                set(ReminderState::Fired)?;
                ClosedOutcome::Exit
            }
        });
    }
    if !cfg.reminders.enabled {
        return Ok(ClosedOutcome::Exit);
    }
    if stop <= now {
        set(ReminderState::Dismissed)?;
        return Ok(ClosedOutcome::Exit);
    }
    if super::notify_at(&r, i64::from(cfg.reminders.lead_min) * 60) - now > EARLY_SLACK_S {
        return Ok(ClosedOutcome::Exit);
    }
    set(ReminderState::Fired)?;
    Ok(ClosedOutcome::Notify(note(&r, guard, now, None)))
}

fn note(r: &Reminder, guard: &Guard, now: Timestamp, why: Option<super::NotifyWhy>) -> ClosedNote {
    let (summary, body) = super::note_text(r, now, guard, why);
    ClosedNote { summary, body, play: format!("{}:{}", r.channel.source.0, r.channel.key.0) }
}

/// Show `note` with Watch and wait for it (blocking, at most the notification's action wait);
/// Watch launches `tuner --play <ref>`. Call it after releasing the instance lock: a Tuner
/// started meanwhile must not wait for this process.
pub fn notify_and_wait(note: &ClosedNote) {
    if crate::platform::notify::show_and_wait_watch(&note.summary, &note.body) {
        let spawned = std::process::Command::new(crate::APP_ID)
            .args(["--play", &note.play])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(e) = spawned {
            tracing::warn!("reminder Watch: couldn't start Tuner: {e}");
        }
    }
}
