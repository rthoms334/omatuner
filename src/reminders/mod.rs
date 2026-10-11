//! The reminder / fire engine (B6; MCP pass 2): the ONLY "fire at a time" mechanism — your own
//! reminders and automatic switches, Following's reminders (`rule_id`), news flashes, the sleep
//! timer. Batch base wrote the hooks; OWNERS: remind (engine: this module, the scheduler in
//! `platform::notify`, systemd arming, `store::user` reminders, migration 0008) and rui (the
//! Settings ▸ Reminders page, the sleep-timer menu, toasts, the pop-out countdown).
//!
//! Hooks the base fixed (the batch plan §3):
//! 1. `store::user::remove_reminder(conn, id, why)` is the only delete path.
//! 2. [`fire_policy`] decides at fire time how a reminder fires (B1 through `playback::route`).
//! 3. systemd units carry only `tuner --reminder <id>`, described "Tuner reminder"
//!    ([`arming`]): no titles, channels or refs in `systemctl --user list-timers`.
//! 4. One arming window for all reminders: ≤ 48 h ahead, ≤ 30 units ([`arming::plan`]).
//! 5. Editing a rule reminder's lead or mode detaches it (`store::follow::on_reminder_edited`).
//! 6. The sleep timer counts `after_items` (queue items while Up next drives playback, else
//!    episodes) and knows `end_of_queue`; "stop" stops every picture ([`sleep`]).

pub mod arming;
pub mod sleep;

use crate::guard::Guard;
use crate::model::{Reminder, Timestamp};
use crate::playback::{Kind, PlayIntent, PlaySource, Route, RouteState};

/// The remind track flips this when pass 2 (modes, arming, sleep timer, pop-out launch) is
/// complete; the existing scheduler runs either way.
pub const ENABLED: bool = false;

/// What the engine knows at fire time.
#[derive(Debug, Clone)]
pub struct FireCtx {
    pub route: RouteState,
    /// The target channel is locked and the session is locked.
    pub target_locked: bool,
    pub guard: Guard,
    pub now: Timestamp,
}

/// Why a switch became a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyWhy {
    /// A plain notification reminder.
    Reminder,
    /// A Following / news-flash switch would interrupt a film, episode or catch-up.
    WouldInterrupt,
    /// The target needs the PIN: "needs your PIN".
    Locked,
    /// The router refused (its message).
    Refused(&'static str),
}

/// How a due reminder fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireAs {
    /// Switch (with the 10 s Cancel toast before) where the router says.
    Switch(Route),
    Notify(NotifyWhy),
}

/// Decide how `r` fires now (B6 hook 2). Notification reminders notify; switches go through
/// the router (a rule's switch never interrupts a film or episode unless
/// `follow.interrupt_vod`; in multi-view switches go to the silent picture); a locked target
/// of a rule's switch becomes "needs your PIN" (the user's own switch asks for the PIN on
/// screen, as today).
pub fn fire_policy(r: &Reminder, ctx: &FireCtx) -> FireAs {
    if !r.switches() {
        return FireAs::Notify(NotifyWhy::Reminder);
    }
    if r.rule_id.is_some() && ctx.target_locked {
        return FireAs::Notify(NotifyWhy::Locked);
    }
    let intent = PlayIntent { source: PlaySource::Reminder { id: r.id, rule: r.rule_id }, kind: Kind::Live, live_channel: None };
    match crate::playback::route(&intent, &ctx.route, &ctx.guard, ctx.now) {
        r @ Route::Tile { .. } => FireAs::Switch(r),
        Route::Downgrade(_) | Route::Shield => FireAs::Notify(NotifyWhy::WouldInterrupt),
        Route::Refuse(why) => FireAs::Notify(NotifyWhy::Refused(why)),
    }
}

/// What the engine tells the app (`AppEvent::Reminder`).
#[derive(Debug, Clone, PartialEq)]
pub enum ReminderEvent {
    /// The sleep timer fires at `at`: a toast with Cancel ([`crate::config::ReminderConfig::sleep_warn_s`] before).
    SleepWarn { at: Timestamp },
    /// The sleep timer fired: stop (every picture) or pause.
    SleepNow { action: sleep::SleepAction },
    /// A news flash ended: close the pop-out and put back what was playing.
    FlashEnded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FollowRuleId, ItemKey, ItemKind, ItemRef, ReminderId, ReminderMode, SourceId};
    use crate::playback::TilesState;

    fn reminder(mode: ReminderMode, rule: Option<i64>) -> Reminder {
        let mut r = Reminder::notify(
            ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("xt:live:1".into()) },
            "BBC One".into(),
            "News".into(),
            100,
            200,
            120,
        );
        r.id = ReminderId(5);
        r.mode = mode;
        r.rule_id = rule.map(FollowRuleId);
        r
    }

    fn ctx(main: Option<Kind>, locked: bool) -> FireCtx {
        FireCtx { route: RouteState { main_kind: main, ..Default::default() }, target_locked: locked, guard: Guard::default(), now: 100 }
    }

    /// The user's decisions (2026-10-11): their own switches interrupt (with Cancel), a
    /// Following rule's never interrupts a film or episode and never opens a locked channel.
    #[test]
    fn rule_switches_never_interrupt_a_film_own_switches_do() {
        let film = ctx(Some(Kind::Vod), false);
        assert_eq!(fire_policy(&reminder(ReminderMode::Notify, None), &film), FireAs::Notify(NotifyWhy::Reminder));
        assert!(matches!(fire_policy(&reminder(ReminderMode::Switch, None), &film), FireAs::Switch(Route::Tile { .. })));
        assert_eq!(fire_policy(&reminder(ReminderMode::Switch, Some(3)), &film), FireAs::Notify(NotifyWhy::WouldInterrupt));
        assert!(matches!(fire_policy(&reminder(ReminderMode::Switch, Some(3)), &ctx(Some(Kind::Live), false)), FireAs::Switch(_)));
        assert_eq!(fire_policy(&reminder(ReminderMode::Switch, Some(3)), &ctx(None, true)), FireAs::Notify(NotifyWhy::Locked));
        // multi-view: to the silent picture, taking the sound
        let mut mv = ctx(Some(Kind::Vod), false);
        mv.route.tiles = TilesState { on: true, ..Default::default() };
        assert_eq!(
            fire_policy(&reminder(ReminderMode::Switch, Some(3)), &mv),
            FireAs::Switch(Route::Tile { tile: crate::model::TileId::B, take_sound: true, interrupts_queue: false })
        );
    }
}
