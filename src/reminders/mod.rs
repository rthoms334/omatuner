//! The reminder / fire engine (B6; MCP pass 2): the ONLY "fire at a time" mechanism — your own
//! reminders and automatic switches, Following's reminders (`rule_id`), news flashes, the sleep
//! timer. OWNERS: remind (engine: this module, the scheduler in `platform::notify`, systemd
//! arming in `platform::systemd`, `store::user` reminders, migration 0008) and rui (the
//! Settings ▸ Reminders page, the sleep-timer menu, toasts, the pop-out countdown).
//!
//! Hooks (the batch plan §3):
//! 1. `store::user::remove_reminder(conn, id, why)` is the only delete path.
//! 2. [`fire_policy`] decides at fire time how a reminder fires (B1 through `playback::route`).
//! 3. systemd units carry only `tuner --reminder <id>`, described "Tuner reminder"
//!    ([`arming`]): no titles, channels or refs in `systemctl --user list-timers`.
//! 4. One arming window for all reminders: ≤ 48 h ahead, ≤ 30 units ([`arming::plan`]).
//! 5. Editing a rule reminder's lead or mode detaches it (`store::user::edit_reminder` calls
//!    `store::follow::on_reminder_edited`).
//! 6. The sleep timer counts `after_items` (queue items while Up next drives playback, else
//!    episodes) and knows `end_of_queue`; "stop" stops every picture ([`sleep`]).
//!
//! Times. A reminder's *target* is `start + offset_s` (the offset skips the build-up). A
//! notification goes out `lead` before the target ([`notify_at`]); a switch shows its Cancel
//! countdown [`Reminder::SWITCH_WARN_S`] before the target and switches at it ([`switch_at`]).
//! Late: a notification still goes out while the programme airs ("Started 5 min ago"); a
//! switch still happens up to `platform::notify::LATE_SWITCH_S` late and says how late it is
//! ([`Late`]) so the app can start it from the beginning on catch-up.

pub mod arming;
pub mod closed;
pub mod sleep;
pub mod ui;

use crate::follow::flash::FlashSkip;
use crate::guard::Guard;
use crate::model::{FollowRuleId, ItemRef, OpenIn, Reminder, ReminderId, Timestamp};
use crate::playback::{Kind, PlayIntent, PlaySource, Route, RouteState};

/// The remind track's pass-2 switch (richer `AppEvent::Reminder` events instead of the
/// `PlatformEvent::Switch*` pair, pop-out launch, systemd arming). Flipped once rui's
/// `AppEvent::Reminder` arm handles [`ReminderEvent::SwitchSoon`] / [`ReminderEvent::SwitchNow`]:
/// before that the scheduler keeps sending the pass-1 events the app handles today.
pub const ENABLED: bool = false;

/// Programmes without a usable stop time are assumed to last this long.
pub const DEFAULT_DURATION_S: i64 = 3600;
/// A switch this late (or later) says so ([`Late`]): the app may start it from the beginning.
pub const LATE_NOTE_S: i64 = 60;

/// When the reminder is about: the programme's start plus its offset.
pub fn target_at(r: &Reminder) -> Timestamp {
    r.start.saturating_add(r.offset_s)
}

/// The lead of a notification (its own, else `lead_default_s`; the old switch marker counts as
/// none).
pub fn lead_of(r: &Reminder, lead_default_s: i64) -> i64 {
    if r.lead_s > 0 { r.lead_s } else { lead_default_s.max(0) }
}

/// When a notification reminder goes out.
pub fn notify_at(r: &Reminder, lead_default_s: i64) -> Timestamp {
    target_at(r) - lead_of(r, lead_default_s)
}

/// When an automatic switch switches (its countdown starts `SWITCH_WARN_S` earlier).
pub fn switch_at(r: &Reminder) -> Timestamp {
    target_at(r)
}

/// The programme's end used for "missed" (rows without a sane stop get a default length).
pub fn effective_stop(r: &Reminder) -> Timestamp {
    if r.stop > r.start { r.stop } else { r.start + DEFAULT_DURATION_S }
}

/// The note a reminder shows: "‹title›" / "Starts in 2 min on ‹channel›" ("Starting now",
/// "Started 5 min ago"); a switch that became a note says why when it matters. The title has
/// passed the spoiler guard (`Guard::outside_title`); nothing here goes to the log.
pub fn note_text(r: &Reminder, now: Timestamp, guard: &Guard, why: Option<NotifyWhy>) -> (String, String) {
    let until = target_at(r) - now;
    let when = if until >= 30 {
        format!("Starts in {} min", (until + 59) / 60)
    } else if until > -60 {
        "Starting now".to_owned()
    } else {
        format!("Started {} min ago", -until / 60)
    };
    let title = r.programme_title.trim();
    let title = if title.is_empty() { "Reminder".to_owned() } else { guard.outside_title(title, now).into_owned() };
    let channel = r.channel_name.trim();
    let body = match why {
        Some(NotifyWhy::Locked) => format!("{when} on {channel} · needs your PIN"),
        _ => format!("{when} on {channel}"),
    };
    (title, body)
}

/// What the engine knows at fire time.
#[derive(Debug, Clone)]
pub struct FireCtx {
    pub route: RouteState,
    /// The target channel is locked and the session is locked.
    pub target_locked: bool,
    /// The main picture already plays the target channel.
    pub target_playing: bool,
    /// A news flash's own reasons not to pop up (quiet hours, another app fullscreen), from
    /// Following's flash check; `None` for every other reminder.
    pub flash_skip: Option<FlashSkip>,
    pub guard: Guard,
    pub now: Timestamp,
}

impl FireCtx {
    /// Nothing plays, nothing is locked (a fire while Tuner was closed).
    pub fn idle(now: Timestamp) -> Self {
        Self { route: RouteState::default(), target_locked: false, target_playing: false, flash_skip: None, guard: Guard::default(), now }
    }
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

/// Why a due reminder does nothing at all (it is marked fired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipWhy {
    /// The switch's channel already plays.
    AlreadyPlaying,
    /// A news flash in quiet hours / while another app is fullscreen.
    Quiet(FlashSkip),
}

/// How a due reminder fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireAs {
    /// Switch (with the 10 s Cancel toast before) where the router says.
    Switch(Route),
    Notify(NotifyWhy),
    Skip(SkipWhy),
}

/// Decide how `r` fires now (B6 hook 2). Notification reminders notify; switches go through
/// the router (a rule's switch never interrupts a film or episode unless
/// `follow.interrupt_vod`; in multi-view switches go to the silent picture); a locked target
/// of a rule's switch becomes "needs your PIN" (the user's own switch asks for the PIN on
/// screen, as today); a switch to what already plays, and a news flash in quiet hours, do
/// nothing.
pub fn fire_policy(r: &Reminder, ctx: &FireCtx) -> FireAs {
    if !r.switches() {
        return FireAs::Notify(NotifyWhy::Reminder);
    }
    if ctx.target_playing && !ctx.route.tiles.on {
        return FireAs::Skip(SkipWhy::AlreadyPlaying);
    }
    match ctx.flash_skip {
        Some(s @ (FlashSkip::QuietHours | FlashSkip::Fullscreen)) => return FireAs::Skip(SkipWhy::Quiet(s)),
        Some(FlashSkip::AlreadyWatching) => return FireAs::Skip(SkipWhy::AlreadyPlaying),
        Some(FlashSkip::WouldInterrupt) => return FireAs::Notify(NotifyWhy::WouldInterrupt),
        None => {}
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

/// Where scheduled playback opens: the reminder's own choice, else the setting.
pub fn open_in(r: &Reminder, default: OpenIn) -> OpenIn {
    r.open_in.unwrap_or(default)
}

/// A switch that happens late (Tuner was closed or asleep at its time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Late {
    pub by_s: i64,
    /// Where it would have started (programme start + offset)…
    pub from: Timestamp,
    /// …and the programme's end.
    pub stop: Timestamp,
    /// The channel has catch-up: the app may play `from`–`stop` from the beginning.
    pub catchup: bool,
}

/// What the engine tells the app (`AppEvent::Reminder`).
#[derive(Debug, Clone, PartialEq)]
pub enum ReminderEvent {
    /// An automatic switch is `SWITCH_WARN_S` away: the countdown toast "Switching to
    /// ‹channel› in 10 s · ‹title›" with Cancel (`Action::CancelSwitch`) and, when it opens in
    /// the pop-out, "Open full" — drawn in the pop-out then. `title` is guarded already.
    SwitchSoon { reminder: ReminderId, channel: ItemRef, channel_name: String, title: String, at: Timestamp, open_in: OpenIn, route: Route },
    /// Switch now (unless cancelled): play `channel` where `route` says, opened per `open_in`
    /// (pop-out: pinned, never taking focus; full window: the scheduler raised it already;
    /// where I am: as it is). `rule`: made by a Following rule (news flash included).
    SwitchNow { reminder: ReminderId, channel: ItemRef, open_in: OpenIn, route: Route, rule: Option<FollowRuleId>, late: Option<Late> },
    /// A switch became a notification (shown by the scheduler) or did nothing; for the
    /// Settings list and a toast when the window shows.
    NotSwitched { reminder: ReminderId, why: NotSwitched },
    /// The sleep timer fires at `at`: a toast with Cancel ([`crate::config::ReminderConfig::sleep_warn_s`] before).
    SleepWarn { at: Timestamp },
    /// The sleep timer fired: stop (every picture) or pause. The engine already sent it to the
    /// pictures it knows; the app does the rest (tile B, the pop-out, the OSD).
    SleepNow { action: sleep::SleepAction },
    /// The sleep timer was set, changed or cancelled (`None`) — Settings, the OSD chip.
    SleepChanged(Option<sleep::SleepStatus>),
    /// A news flash ended: close the pop-out and put back what was playing.
    FlashEnded,
}

/// Why a due switch didn't switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSwitched {
    Notified(NotifyWhy),
    Skipped(SkipWhy),
}

// ------------------------------------------------------------------ clashes

/// How two pending reminders clash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClashKind {
    /// Both switch while the other's programme runs: the later switch takes the picture.
    Switches,
    /// The programmes overlap (watch one, catch up the other).
    Overlap,
}

/// Two pending reminders whose programmes overlap; `a` starts first (ties: the lower id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Clash {
    pub a: ReminderId,
    pub b: ReminderId,
    pub kind: ClashKind,
}

/// The time a reminder covers: from its target to the programme's end.
fn window(r: &Reminder) -> (Timestamp, Timestamp) {
    let from = target_at(r);
    (from, effective_stop(r).max(from + 1))
}

fn clash_of(a: &Reminder, b: &Reminder) -> Option<ClashKind> {
    if a.id == b.id || a.channel == b.channel && a.start == b.start {
        return None;
    }
    let ((a0, a1), (b0, b1)) = (window(a), window(b));
    (a0 < b1 && b0 < a1).then_some(if a.switches() && b.switches() { ClashKind::Switches } else { ClashKind::Overlap })
}

/// Every clash among `rs` (pending reminders), soonest first. Sweep over start times:
/// O(n log n + clashes).
pub fn clashes(rs: &[Reminder]) -> Vec<Clash> {
    let mut by_start: Vec<&Reminder> = rs.iter().filter(|r| r.state == crate::model::ReminderState::Pending).collect();
    by_start.sort_by_key(|r| (window(r).0, r.id));
    let mut out = Vec::new();
    for (i, a) in by_start.iter().enumerate() {
        let end = window(a).1;
        for b in by_start[i + 1..].iter().take_while(|b| window(b).0 < end) {
            if let Some(kind) = clash_of(a, b) {
                out.push(Clash { a: a.id, b: b.id, kind });
            }
        }
    }
    out
}

/// The pending reminders among `rs` that `new` would clash with (before adding it; the MCP
/// `reminders add` reply and the guide's "clashes with …" note).
pub fn clashes_with<'a>(new: &Reminder, rs: &'a [Reminder]) -> Vec<(&'a Reminder, ClashKind)> {
    let mut out: Vec<_> =
        rs.iter().filter(|r| r.state == crate::model::ReminderState::Pending).filter_map(|r| clash_of(new, r).map(|k| (r, k))).collect();
    out.sort_by_key(|(r, _)| (window(r).0, r.id));
    out
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
        FireCtx { route: RouteState { main_kind: main, ..Default::default() }, target_locked: locked, ..FireCtx::idle(100) }
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

    /// A switch to the channel already on does nothing; a news flash keeps quiet in quiet hours
    /// or while another app is fullscreen, and becomes a notification rather than interrupt.
    #[test]
    fn switches_skip_what_already_plays_and_flashes_keep_quiet() {
        let mut on = ctx(Some(Kind::Live), false);
        on.target_playing = true;
        assert_eq!(fire_policy(&reminder(ReminderMode::Switch, None), &on), FireAs::Skip(SkipWhy::AlreadyPlaying));
        assert_eq!(fire_policy(&reminder(ReminderMode::Notify, None), &on), FireAs::Notify(NotifyWhy::Reminder), "notes still go out");
        let flash = |skip| FireCtx { flash_skip: Some(skip), ..ctx(None, false) };
        let r = reminder(ReminderMode::Switch, Some(9));
        assert_eq!(fire_policy(&r, &flash(FlashSkip::QuietHours)), FireAs::Skip(SkipWhy::Quiet(FlashSkip::QuietHours)));
        assert_eq!(fire_policy(&r, &flash(FlashSkip::Fullscreen)), FireAs::Skip(SkipWhy::Quiet(FlashSkip::Fullscreen)));
        assert_eq!(fire_policy(&r, &flash(FlashSkip::WouldInterrupt)), FireAs::Notify(NotifyWhy::WouldInterrupt));
    }

    /// Lead and offset: a note goes out `lead` before start + offset ("tell me when the last
    /// 10 laps start"); a switch switches at start + offset; rows without a lead (or with the
    /// old switch marker) take the default.
    #[test]
    fn fire_times_follow_lead_and_offset() {
        let mut r = reminder(ReminderMode::Notify, None);
        r.offset_s = 600;
        assert_eq!((target_at(&r), notify_at(&r, 300)), (700, 580));
        r.lead_s = Reminder::SWITCH_LEAD;
        assert_eq!(notify_at(&r, 300), 400);
        r.mode = ReminderMode::Switch;
        assert_eq!(switch_at(&r), 700);
        assert_eq!(arming::fire_at(&r, 300), 700 - Reminder::SWITCH_WARN_S);
    }

    fn at(id: i64, ch: &str, start: Timestamp, stop: Timestamp, mode: ReminderMode) -> Reminder {
        let mut r = Reminder::notify(
            ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey(ch.into()) },
            ch.into(),
            format!("P{id}"),
            start,
            stop,
            120,
        );
        r.id = ReminderId(id);
        r.mode = mode;
        r
    }

    /// "Both semis are at 8, which one?": overlapping programmes clash; two switches clash
    /// harder (only one picture); back-to-back programmes and the same programme don't.
    #[test]
    fn clashes_find_overlaps_and_switch_pairs() {
        use ReminderMode::{Notify, Switch};
        let rs = vec![
            at(1, "a", 1000, 2000, Notify),
            at(2, "b", 1500, 2500, Notify), // overlaps 1
            at(3, "c", 2000, 3000, Switch), // back-to-back with 1, overlaps 2
            at(4, "d", 2900, 3600, Switch), // overlaps 3: two switches
            at(5, "e", 5000, 4000, Notify), // no stop: assumed an hour, alone
        ];
        let got = clashes(&rs);
        assert_eq!(
            got,
            vec![
                Clash { a: ReminderId(1), b: ReminderId(2), kind: ClashKind::Overlap },
                Clash { a: ReminderId(2), b: ReminderId(3), kind: ClashKind::Overlap },
                Clash { a: ReminderId(3), b: ReminderId(4), kind: ClashKind::Switches },
            ]
        );
        let new = at(0, "z", 1900, 2100, Switch);
        let with: Vec<_> = clashes_with(&new, &rs).into_iter().map(|(r, k)| (r.id.0, k)).collect();
        assert_eq!(with, vec![(1, ClashKind::Overlap), (2, ClashKind::Overlap), (3, ClashKind::Switches)]);
        // an offset narrows the window (1300–1400 here): it ends before 2 starts
        let mut late = at(9, "a2", 1000, 1400, Notify);
        late.offset_s = 300;
        assert!(clashes_with(&late, &rs[1..]).is_empty());
    }
}
