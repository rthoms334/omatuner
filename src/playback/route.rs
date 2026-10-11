//! The playback router (B1): one pure function from a play intent and the current state to a
//! decision. Callers: `UiCtx::play`, the reminder scheduler (through `reminders::fire_policy`),
//! the MCP `play` / `control` handlers, IPC, the queue engine and the MPRIS router.
//!
//! | Source | Single view, main idle or live | Main plays a film, episode or catch-up | Multi-view on |
//! |---|---|---|---|
//! | User pick | main (the queue steps aside) | main (the queue steps aside) | focused picture |
//! | User zap | main, unless the spoiler shield applies | main | focused picture, shield checked |
//! | Ctrl+M watch alongside | opens multi-view (B) | refused for a film / episode | the other picture |
//! | Up next advance | main | main | waits |
//! | Own reminder auto-switch | main, 10 s Cancel | main, Cancel; the queue steps aside | silent picture, takes the sound |
//! | Following / news flash switch | main, Cancel | a notification with Watch (unless `follow.interrupt_vod`) | silent picture, takes the sound |
//! | MCP play, IPC `--play`, notification Watch | main | main; the queue steps aside | silent picture, takes the sound |
//!
//! The parental gate is NOT here: the PIN and the explicit gate stay in the play path, and the
//! router never unlocks anything. The shield applies to `UserZap` only.

use crate::guard::Guard;
use crate::model::{ChannelId, FollowRuleId, ReminderId, TileId, Timestamp};

/// Where a play request comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaySource {
    /// Enter, play buttons, a click.
    User,
    /// ↑↓, digits, MPRIS Next on live.
    UserZap,
    /// Ctrl+M: watch alongside (multi-view).
    Alongside,
    /// Up next advancing.
    Queue,
    /// A reminder's automatic switch; `rule` = made by a Following rule (news flash included).
    Reminder { id: ReminderId, rule: Option<FollowRuleId> },
    /// MCP `play`.
    Agent,
    /// `tuner --play`.
    Ipc,
    /// A notification's Watch button.
    Notification,
}

/// What would play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Live,
    Catchup,
    /// A movie or an episode.
    Vod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayIntent {
    pub source: PlaySource,
    pub kind: Kind,
    pub live_channel: Option<ChannelId>,
}

/// The pictures as the UI last published them (`AppShared::tiles`, a `watch`): tokio callers
/// route with it while the window is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TilesState {
    /// Multi-view is on (two pictures).
    pub on: bool,
    pub focus: TileId,
    pub audible: TileId,
    /// What each picture plays (`None` = idle), by `TileId.0`.
    pub kinds: [Option<Kind>; 2],
}

impl TilesState {
    /// The picture that isn't audible (outside plays go there in multi-view).
    pub fn silent(&self) -> TileId {
        self.audible.other()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RouteState {
    pub tiles: TilesState,
    /// What the main picture plays (single view: the only one).
    pub main_kind: Option<Kind>,
    /// Up next drives the main picture.
    pub queue_running: bool,
    /// `follow.interrupt_vod`.
    pub follow_interrupt_vod: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyReason {
    /// It would interrupt a film, an episode or catch-up.
    WouldInterrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downgrade {
    /// Don't switch: a notification with Watch instead.
    Notify { reason: NotifyReason },
}

/// The decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Play in `tile`. `take_sound`: move the sound there (multi-view); `interrupts_queue`: Up
    /// next steps aside ("Up next paused · Alt+6").
    Tile {
        tile: TileId,
        take_sound: bool,
        interrupts_queue: bool,
    },
    Downgrade(Downgrade),
    /// Refused, with the toast / MCP message.
    Refuse(&'static str),
    /// The spoiler shield: don't tune; show the shield card.
    Shield,
}

/// Refusal texts (also MCP messages).
pub const MULTIVIEW_LIVE_ONLY: &str = "Multi-view is for live TV and catch-up";
pub const QUEUE_WAITS: &str = "Up next waits while multi-view is on";

/// Route one play intent. Pure; `guard` and `now` only for the zap shield.
pub fn route(intent: &PlayIntent, st: &RouteState, guard: &Guard, now: Timestamp) -> Route {
    let multi = st.tiles.on;
    let main = |interrupts_queue: bool| Route::Tile { tile: TileId::A, take_sound: false, interrupts_queue };
    let main_busy_vod = matches!(st.main_kind, Some(Kind::Vod | Kind::Catchup));
    let steps_aside = st.queue_running;
    match intent.source {
        PlaySource::User => {
            if multi {
                Route::Tile { tile: st.tiles.focus, take_sound: false, interrupts_queue: false }
            } else {
                main(steps_aside)
            }
        }
        PlaySource::UserZap => {
            if let Some(ch) = intent.live_channel
                && guard.shield(ch, now).is_some()
            {
                return Route::Shield;
            }
            if multi { Route::Tile { tile: st.tiles.focus, take_sound: false, interrupts_queue: false } } else { main(steps_aside) }
        }
        PlaySource::Alongside => {
            if intent.kind == Kind::Vod || (!multi && st.main_kind == Some(Kind::Vod)) {
                Route::Refuse(MULTIVIEW_LIVE_ONLY)
            } else if multi {
                Route::Tile { tile: st.tiles.focus.other(), take_sound: false, interrupts_queue: false }
            } else if st.main_kind.is_none() {
                // nothing plays: it simply plays
                main(steps_aside)
            } else {
                Route::Tile { tile: TileId::B, take_sound: false, interrupts_queue: false }
            }
        }
        PlaySource::Queue => {
            if multi {
                Route::Refuse(QUEUE_WAITS)
            } else {
                main(false)
            }
        }
        PlaySource::Reminder { rule: Some(_), .. } => {
            if multi {
                Route::Tile { tile: st.tiles.silent(), take_sound: true, interrupts_queue: false }
            } else if main_busy_vod && !st.follow_interrupt_vod {
                Route::Downgrade(Downgrade::Notify { reason: NotifyReason::WouldInterrupt })
            } else {
                main(steps_aside)
            }
        }
        PlaySource::Reminder { rule: None, .. } | PlaySource::Agent | PlaySource::Ipc | PlaySource::Notification => {
            if multi {
                Route::Tile { tile: st.tiles.silent(), take_sound: true, interrupts_queue: false }
            } else {
                main(steps_aside)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = 1_800_000_000;

    fn intent(source: PlaySource, kind: Kind) -> PlayIntent {
        PlayIntent { source, kind, live_channel: Some(ChannelId(7)) }
    }

    fn single(main: Option<Kind>, queue: bool) -> RouteState {
        RouteState { main_kind: main, queue_running: queue, ..Default::default() }
    }

    fn multi() -> RouteState {
        RouteState {
            tiles: TilesState { on: true, focus: TileId::B, audible: TileId::A, kinds: [Some(Kind::Live), Some(Kind::Live)] },
            main_kind: Some(Kind::Live),
            ..Default::default()
        }
    }

    const MAIN: Route = Route::Tile { tile: TileId::A, take_sound: false, interrupts_queue: false };
    const MAIN_STEPS_ASIDE: Route = Route::Tile { tile: TileId::A, take_sound: false, interrupts_queue: true };
    const SILENT_TAKES_SOUND: Route = Route::Tile { tile: TileId::B, take_sound: true, interrupts_queue: false };

    /// The decision table of the batch plan (B1), row by row.
    #[test]
    fn the_decision_table() {
        let g = Guard::default();
        let r = |i: PlaySource, k: Kind, st: &RouteState| route(&intent(i, k), st, &g, NOW);
        let own = PlaySource::Reminder { id: ReminderId(1), rule: None };
        let rule = PlaySource::Reminder { id: ReminderId(2), rule: Some(FollowRuleId(3)) };

        // a user pick: main in single view (Up next steps aside), the focused picture in multi-view
        assert_eq!(r(PlaySource::User, Kind::Vod, &single(Some(Kind::Vod), true)), MAIN_STEPS_ASIDE);
        assert_eq!(r(PlaySource::User, Kind::Live, &single(None, false)), MAIN);
        assert_eq!(r(PlaySource::User, Kind::Live, &multi()), Route::Tile { tile: TileId::B, take_sound: false, interrupts_queue: false });
        // zaps: main / the focused picture (the identity guard never shields)
        assert_eq!(r(PlaySource::UserZap, Kind::Live, &single(Some(Kind::Live), false)), MAIN);
        assert_eq!(r(PlaySource::UserZap, Kind::Live, &multi()), Route::Tile { tile: TileId::B, take_sound: false, interrupts_queue: false });
        // Ctrl+M: opens B next to live or catch-up, refused next to a film and for one
        assert_eq!(
            r(PlaySource::Alongside, Kind::Live, &single(Some(Kind::Live), false)),
            Route::Tile { tile: TileId::B, take_sound: false, interrupts_queue: false }
        );
        assert_eq!(
            r(PlaySource::Alongside, Kind::Live, &single(Some(Kind::Catchup), false)),
            Route::Tile { tile: TileId::B, take_sound: false, interrupts_queue: false }
        );
        assert_eq!(r(PlaySource::Alongside, Kind::Live, &single(Some(Kind::Vod), false)), Route::Refuse(MULTIVIEW_LIVE_ONLY));
        assert_eq!(r(PlaySource::Alongside, Kind::Vod, &single(None, false)), Route::Refuse(MULTIVIEW_LIVE_ONLY));
        assert_eq!(r(PlaySource::Alongside, Kind::Live, &single(None, false)), MAIN, "nothing plays: it simply plays");
        assert_eq!(r(PlaySource::Alongside, Kind::Live, &multi()), Route::Tile { tile: TileId::A, take_sound: false, interrupts_queue: false });
        // Up next: main, waits in multi-view
        assert_eq!(r(PlaySource::Queue, Kind::Vod, &single(Some(Kind::Vod), true)), MAIN);
        assert_eq!(r(PlaySource::Queue, Kind::Vod, &multi()), Route::Refuse(QUEUE_WAITS));
        // the user's own auto-switch interrupts anything (with its Cancel), the queue steps aside
        assert_eq!(r(own, Kind::Live, &single(Some(Kind::Vod), true)), MAIN_STEPS_ASIDE);
        assert_eq!(r(own, Kind::Live, &single(Some(Kind::Live), false)), MAIN);
        assert_eq!(r(own, Kind::Live, &multi()), SILENT_TAKES_SOUND);
        // a Following rule (or news flash) never interrupts a film, an episode or catch-up…
        for busy in [Kind::Vod, Kind::Catchup] {
            assert_eq!(
                r(rule, Kind::Live, &single(Some(busy), false)),
                Route::Downgrade(Downgrade::Notify { reason: NotifyReason::WouldInterrupt }),
                "{busy:?}"
            );
        }
        // …unless follow.interrupt_vod; live TV it switches
        assert_eq!(r(rule, Kind::Live, &RouteState { follow_interrupt_vod: true, ..single(Some(Kind::Vod), true) }), MAIN_STEPS_ASIDE);
        assert_eq!(r(rule, Kind::Live, &single(Some(Kind::Live), false)), MAIN);
        assert_eq!(r(rule, Kind::Live, &single(None, false)), MAIN);
        assert_eq!(r(rule, Kind::Live, &multi()), SILENT_TAKES_SOUND);
        // MCP, IPC and a notification's Watch: main (the queue steps aside) / the silent picture
        for s in [PlaySource::Agent, PlaySource::Ipc, PlaySource::Notification] {
            assert_eq!(r(s, Kind::Vod, &single(Some(Kind::Vod), true)), MAIN_STEPS_ASIDE, "{s:?}");
            assert_eq!(r(s, Kind::Live, &multi()), SILENT_TAKES_SOUND, "{s:?}");
        }
    }

    /// The silent picture is the one without sound, wherever the focus is.
    #[test]
    fn outside_plays_take_the_silent_picture_not_the_focused_one() {
        let g = Guard::default();
        let st = RouteState { tiles: TilesState { on: true, focus: TileId::A, audible: TileId::B, kinds: [None; 2] }, ..Default::default() };
        assert_eq!(
            route(&intent(PlaySource::Agent, Kind::Live), &st, &g, NOW),
            Route::Tile { tile: TileId::A, take_sound: true, interrupts_queue: false }
        );
    }
}
