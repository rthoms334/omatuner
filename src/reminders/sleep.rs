//! The in-app sleep timer (skill plan; B6 hook 6): it outlives the Claude session because it
//! runs in Tuner (on tokio, so it works while the window is hidden). OWNER: remind (engine) /
//! rui (menu, toast, OSD countdown).
//!
//! * At a time / after N minutes: [`SleepWhen::At`].
//! * When the programme ends: live → the guide's end of the programme on now (the caller
//!   passes it, [`live_programme_stop`] looks it up); a film, episode or catch-up → when it
//!   ends, i.e. after one item.
//! * After N items: queue items while Up next drives playback, else films / episodes. The item
//!   playing when it is set counts as the first. An item counts when it ends, or when the next
//!   starts within [`END_SLACK_S`] of its end (auto-next at the credits); stopping or picking
//!   something else mid-way doesn't count. Live channels are never items.
//! * At the end of Up next: when the queue runs out.
//!
//! [`SleepRun`] is the pure part (tests drive it with [`Watch`] values); [`SleepService`]
//! feeds it the main picture's and Up next's snapshots, warns `warn_s` before (a toast with
//! Cancel), then stops every picture it was given (or pauses) and tells the app, which stops
//! the rest (tile B, the pop-out).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, watch};

use crate::model::{MediaRef, QueueId, Timestamp};
use crate::msg::{AppEvent, EventBus};
use crate::player::{PlayerCommand, PlayerHandle, PlayerSnapshot, PlayerState};
use crate::queue::QueueSnapshot;

/// The timer is set at most this far ahead.
pub const MAX_AHEAD_S: i64 = 24 * 3600;
/// At most this many items.
pub const MAX_ITEMS: u32 = 50;
/// An item that gives way to the next this close to its end has finished (credits).
pub const END_SLACK_S: f64 = 90.0;

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

/// A request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepTimer {
    pub when: SleepWhen,
    pub action: SleepAction,
    /// Warning toast this long before (`reminders.sleep_warn_s`).
    pub warn_s: u32,
    pub set_at: Timestamp,
}

/// Why a timer can't be set (the text is the toast / MCP message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepError {
    NothingPlaying,
    /// End of a live programme without a guide entry for now.
    NoGuide,
    NoQueue,
    InPast,
    TooFar,
    NoItems,
}

impl SleepError {
    pub fn message(self) -> &'static str {
        match self {
            SleepError::NothingPlaying => "Nothing is playing",
            SleepError::NoGuide => "This channel's guide doesn't say when the programme ends; pick a time instead",
            SleepError::NoQueue => "Up next isn't playing",
            SleepError::InPast => "That time has passed",
            SleepError::TooFar => "The sleep timer goes at most 24 hours ahead",
            SleepError::NoItems => "Pick 1 to 50 items",
        }
    }
}

impl std::fmt::Display for SleepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// What the main picture and Up next do, as the timer sees it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Watch {
    pub now: Timestamp,
    /// What plays (`None`: nothing).
    pub media: Option<MediaRef>,
    pub live: bool,
    /// Seconds left of a film, episode or catch-up, when known.
    pub left_s: Option<f64>,
    /// It played to the end.
    pub ended: bool,
    /// Up next drives the main picture: its current row.
    pub queue_current: Option<QueueId>,
    /// Up next is between two of its items (the card after one, a PIN wait, a future slot):
    /// it hasn't run out, and the item that ended is already counted.
    pub queue_between: bool,
    /// Up next's rows after the current one.
    pub queue_left: usize,
    /// Up next's estimated end.
    pub queue_eta: Option<Timestamp>,
}

impl Watch {
    /// From the main picture's and Up next's snapshots.
    pub fn of(p: &PlayerSnapshot, q: &QueueSnapshot, now: Timestamp) -> Self {
        let playing = !matches!(p.state, PlayerState::Idle);
        let media = p.playable.as_ref().filter(|_| playing).map(|pl| pl.media.clone());
        let live = p.playable.as_ref().is_some_and(|pl| pl.kind.is_live());
        let left_s = match (p.duration, p.position) {
            (Some(d), Some(pos)) if d > 0.0 => Some((d - pos).max(0.0)),
            _ => None,
        };
        // The engine's own word (origin Queue on one of the row's refs; a zap away is not driving).
        let driving = q.driving && q.current.is_some();
        Self {
            now,
            media,
            live,
            left_s,
            ended: p.state == PlayerState::Ended,
            queue_current: q.current.filter(|_| driving),
            queue_between: q.between_items(),
            queue_left: if driving { q.pending().saturating_sub(1) } else { 0 },
            queue_eta: q.eta_end.map(|(t, _)| t).filter(|_| driving),
        }
    }

    /// The item being counted now: the queue row while Up next drives, else a film / episode /
    /// catch-up (never live).
    fn item(&self) -> Option<Item> {
        match (&self.queue_current, &self.media) {
            (Some(q), _) => Some(Item::Queue(*q)),
            // the row that ended still shows under the card: not a new item
            (None, _) if self.queue_between => None,
            (None, Some(m)) if !self.live => Some(Item::Media(m.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Queue(QueueId),
    Media(MediaRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deadline {
    At(Timestamp),
    Items { left: u32 },
    QueueEnd,
}

/// What the timer says now (Settings, the OSD chip, MCP `sleep_timer status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepStatus {
    pub when: SleepWhen,
    pub action: SleepAction,
    /// When it fires, when known (an estimate for items and the queue).
    pub fires_at: Option<Timestamp>,
    /// Items still to finish (after_items, end of a film).
    pub items_left: Option<u32>,
}

/// What to do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepStep {
    Warn { at: Timestamp },
    Fire(SleepAction),
}

/// A set timer.
#[derive(Debug, Clone, PartialEq)]
pub struct SleepRun {
    timer: SleepTimer,
    deadline: Deadline,
    /// The item being counted and the last seconds-left seen of it.
    counting: Option<(Item, Option<f64>)>,
    /// An item counted as finished that still shows ended: not counted again.
    spent: Option<Item>,
    /// Up next was seen driving (end of queue).
    queue_seen: bool,
    warned: bool,
}

impl SleepRun {
    /// Set `timer` against what plays now. `programme_stop`: the end of the live programme on
    /// now (end of programme on live).
    pub fn arm(timer: SleepTimer, w: &Watch, programme_stop: Option<Timestamp>) -> Result<Self, SleepError> {
        let deadline = match timer.when {
            SleepWhen::At(t) if t <= w.now => return Err(SleepError::InPast),
            SleepWhen::At(t) if t > w.now + MAX_AHEAD_S => return Err(SleepError::TooFar),
            SleepWhen::At(t) => Deadline::At(t),
            SleepWhen::EndOfProgramme => match (&w.media, w.live) {
                (None, _) => return Err(SleepError::NothingPlaying),
                (Some(_), true) => match programme_stop {
                    Some(t) if t > w.now => Deadline::At(t),
                    _ => return Err(SleepError::NoGuide),
                },
                // a film / episode / catch-up: when this one ends (not Up next's row)
                (Some(_), false) => Deadline::Items { left: 1 },
            },
            SleepWhen::AfterItems(n) if n == 0 || n > MAX_ITEMS => return Err(SleepError::NoItems),
            SleepWhen::AfterItems(_) if w.item().is_none() => return Err(SleepError::NothingPlaying),
            SleepWhen::AfterItems(n) => Deadline::Items { left: n },
            SleepWhen::EndOfQueue if w.queue_current.is_none() && !w.queue_between => return Err(SleepError::NoQueue),
            SleepWhen::EndOfQueue => Deadline::QueueEnd,
        };
        let counting = match timer.when {
            // the film itself, even when Up next plays it
            SleepWhen::EndOfProgramme => w.media.clone().map(Item::Media),
            _ => w.item(),
        }
        .map(|i| (i, w.left_s));
        Ok(Self { timer, deadline, counting, spent: None, queue_seen: w.queue_current.is_some() || w.queue_between, warned: false })
    }

    pub fn timer(&self) -> &SleepTimer {
        &self.timer
    }

    /// Look at what plays now; `Some` when it is time to warn or fire (each happens once; after
    /// `Fire` the run is done).
    pub fn step(&mut self, w: &Watch) -> Option<SleepStep> {
        let by_item = matches!(self.timer.when, SleepWhen::EndOfProgramme);
        match &mut self.deadline {
            Deadline::At(t) => {
                if w.now >= *t {
                    return Some(SleepStep::Fire(self.timer.action));
                }
            }
            Deadline::Items { left } => {
                let now_item = if by_item { w.media.clone().filter(|_| !w.live).map(Item::Media) } else { w.item() };
                let finished = match &self.counting {
                    // played to the end
                    Some((i, _)) if w.ended && now_item.as_ref() == Some(i) => true,
                    // gave way to the next at the credits
                    Some((i, last_left)) if now_item.as_ref() != Some(i) => last_left.is_some_and(|l| l <= END_SLACK_S),
                    _ => false,
                };
                let changed = self.counting.as_ref().map(|(i, _)| i) != now_item.as_ref();
                if finished {
                    *left = left.saturating_sub(1);
                    if *left == 0 {
                        return Some(SleepStep::Fire(self.timer.action));
                    }
                    // an ended item stays on screen: start counting the next one when it starts
                    if changed {
                        self.counting = now_item.map(|i| (i, w.left_s));
                    } else {
                        self.spent = now_item;
                        self.counting = None;
                    }
                } else if changed {
                    // picked something else mid-way, or the next started: count that one (not the
                    // finished one still showing ended; played again, it counts afresh)
                    if now_item != self.spent || !w.ended {
                        self.spent = None;
                        self.counting = now_item.map(|i| (i, w.left_s));
                    }
                } else if let Some((_, last)) = &mut self.counting {
                    *last = w.left_s;
                }
            }
            Deadline::QueueEnd => {
                self.queue_seen |= w.queue_current.is_some() || w.queue_between;
                let last_ended = w.queue_left == 0 && w.queue_current.is_some() && w.ended;
                // between two items (the card after one) it hasn't run out
                if self.queue_seen && ((w.queue_current.is_none() && !w.queue_between) || last_ended) {
                    return Some(SleepStep::Fire(self.timer.action));
                }
            }
        }
        if !self.warned
            && let Some(at) = self.fires_at(w)
            && at - w.now <= i64::from(self.timer.warn_s)
        {
            self.warned = true;
            return Some(SleepStep::Warn { at });
        }
        None
    }

    /// When it fires: exact for a time, an estimate when the last item (or Up next's last
    /// row) is on and its length is known.
    pub fn fires_at(&self, w: &Watch) -> Option<Timestamp> {
        let left_s = || w.left_s.map(|l| w.now + l.ceil() as i64);
        match self.deadline {
            Deadline::At(t) => Some(t),
            Deadline::Items { left: 1 } if self.counting.is_some() => left_s(),
            Deadline::Items { .. } => None,
            Deadline::QueueEnd if w.queue_left == 0 && w.queue_current.is_some() => left_s(),
            Deadline::QueueEnd => w.queue_eta,
        }
    }

    pub fn status(&self, w: &Watch) -> SleepStatus {
        SleepStatus {
            when: self.timer.when,
            action: self.timer.action,
            fires_at: self.fires_at(w),
            items_left: match self.deadline {
                Deadline::Items { left } => Some(left),
                _ => None,
            },
        }
    }
}

/// The end of the live programme on `channel` now (end of programme on live), from the guide.
/// Blocking (one indexed query).
pub fn live_programme_stop(
    conn: &rusqlite::Connection,
    lib: &crate::store::library::Library,
    channel: crate::model::ChannelId,
    now: Timestamp,
) -> crate::error::Result<Option<Timestamp>> {
    let Some(c) = lib.live.get(channel) else { return Ok(None) };
    let Some(epg) = c.epg_channel_id else { return Ok(None) };
    let nn = crate::store::epg::now_next(conn, &[(epg, c.tvg_shift_min)], now)?;
    Ok(nn.get(&(epg, c.tvg_shift_min)).and_then(|n| n.now.as_ref()).map(|p| p.stop))
}

// ------------------------------------------------------------------ the service

/// Stops or pauses pictures (every picture the engine knows; the app gets `SleepNow` too).
pub type Actuator = Arc<dyn Fn(SleepAction) + Send + Sync>;

/// An actuator over player handles (tile A at least; MV-1 passes both pictures).
pub fn player_actuator(handles: Vec<PlayerHandle>) -> Actuator {
    Arc::new(move |a| {
        for h in &handles {
            h.send(match a {
                SleepAction::Stop => PlayerCommand::Stop,
                SleepAction::Pause => PlayerCommand::SetPause(true),
            });
        }
    })
}

#[derive(Default)]
struct State {
    run: Option<SleepRun>,
}

/// Sets, reads and cancels the timer from anywhere (UI thread, MCP handler); cheap to clone.
#[derive(Clone)]
pub struct SleepHandle {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    bus: EventBus,
    /// What plays now, for setting and reading the timer between the runner's looks.
    player: watch::Receiver<Arc<PlayerSnapshot>>,
    queue: watch::Receiver<Arc<QueueSnapshot>>,
}

impl std::fmt::Debug for SleepHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SleepHandle")
    }
}

impl SleepHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn watch_now(&self) -> Watch {
        Watch::of(&self.player.borrow(), &self.queue.borrow(), crate::model::now())
    }

    /// Set (or replace) the timer. `programme_stop`: see [`SleepRun::arm`].
    pub fn set(&self, when: SleepWhen, action: SleepAction, warn_s: u32, programme_stop: Option<Timestamp>) -> Result<SleepStatus, SleepError> {
        let w = self.watch_now();
        let run = SleepRun::arm(SleepTimer { when, action, warn_s, set_at: w.now }, &w, programme_stop)?;
        let status = run.status(&w);
        self.lock().run = Some(run);
        tracing::info!("sleep timer set: {when:?}, then {action:?}");
        self.bus.send(AppEvent::Reminder(super::ReminderEvent::SleepChanged(Some(status))));
        self.wake.notify_one();
        Ok(status)
    }

    /// Cancel; false when none was set.
    pub fn cancel(&self) -> bool {
        let had = self.lock().run.take().is_some();
        if had {
            tracing::info!("sleep timer cancelled");
            self.bus.send(AppEvent::Reminder(super::ReminderEvent::SleepChanged(None)));
            self.wake.notify_one();
        }
        had
    }

    pub fn status(&self) -> Option<SleepStatus> {
        let w = self.watch_now();
        self.lock().run.as_ref().map(|r| r.status(&w))
    }
}

/// The runner (one per app).
pub struct SleepService;

impl SleepService {
    /// Start on `rt`, following the main picture (`player`) and Up next (`queue`).
    pub fn start(
        rt: &tokio::runtime::Handle,
        player: watch::Receiver<Arc<PlayerSnapshot>>,
        queue: watch::Receiver<Arc<QueueSnapshot>>,
        actuator: Actuator,
        bus: EventBus,
    ) -> SleepHandle {
        let handle = SleepHandle { state: Arc::default(), wake: Arc::new(Notify::new()), bus, player: player.clone(), queue: queue.clone() };
        rt.spawn(run(handle.clone(), player, queue, actuator));
        handle
    }
}

async fn run(h: SleepHandle, mut player: watch::Receiver<Arc<PlayerSnapshot>>, mut queue: watch::Receiver<Arc<QueueSnapshot>>, act: Actuator) {
    // a closed channel (that picture / Up next is gone) keeps its last value; the clock goes on
    let (mut player_open, mut queue_open) = (true, true);
    loop {
        let w = Watch::of(&player.borrow_and_update(), &queue.borrow_and_update(), crate::model::now());
        let (step, armed) = {
            let mut st = h.lock();
            let step = st.run.as_mut().and_then(|r| r.step(&w));
            if matches!(step, Some(SleepStep::Fire(_))) {
                st.run = None;
            }
            (step, st.run.is_some())
        };
        match step {
            Some(SleepStep::Warn { at }) => h.bus.send(AppEvent::Reminder(super::ReminderEvent::SleepWarn { at })),
            Some(SleepStep::Fire(action)) => {
                tracing::info!("sleep timer: {action:?}");
                act(action);
                h.bus.send(AppEvent::Reminder(super::ReminderEvent::SleepNow { action }));
                h.bus.send(AppEvent::Reminder(super::ReminderEvent::SleepChanged(None)));
            }
            None => {}
        }
        // a timer looks every second (the wall clock; suspend/resume and clock changes); idle,
        // it waits for a set
        let tick = if armed { Duration::from_secs(1) } else { Duration::from_secs(3600) };
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
            _ = h.wake.notified() => {}
            r = player.changed(), if armed && player_open => player_open = r.is_ok(),
            r = queue.changed(), if armed && queue_open => queue_open = r.is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpisodeId, MovieId};

    const T0: Timestamp = 1_000_000;

    fn timer(when: SleepWhen) -> SleepTimer {
        SleepTimer { when, action: SleepAction::Stop, warn_s: 120, set_at: T0 }
    }

    fn ep(n: i64, left: f64, now: Timestamp) -> Watch {
        Watch { now, media: Some(MediaRef::Episode(EpisodeId(n))), left_s: Some(left), ..Default::default() }
    }

    fn live(now: Timestamp) -> Watch {
        Watch { now, media: Some(MediaRef::Channel(crate::model::ChannelId(4))), live: true, ..Default::default() }
    }

    /// "Stop at 11": warns `warn_s` before (once), fires at the time; nonsense times refuse.
    #[test]
    fn at_a_time_warns_then_fires() {
        let mut run = SleepRun::arm(timer(SleepWhen::At(T0 + 600)), &live(T0), None).unwrap();
        assert_eq!(run.step(&live(T0 + 100)), None);
        assert_eq!(run.step(&live(T0 + 480)), Some(SleepStep::Warn { at: T0 + 600 }));
        assert_eq!(run.step(&live(T0 + 500)), None, "warns once");
        assert_eq!(run.step(&live(T0 + 600)), Some(SleepStep::Fire(SleepAction::Stop)));
        assert_eq!(SleepRun::arm(timer(SleepWhen::At(T0)), &live(T0), None), Err(SleepError::InPast));
        assert_eq!(SleepRun::arm(timer(SleepWhen::At(T0 + MAX_AHEAD_S + 1)), &live(T0), None), Err(SleepError::TooFar));
    }

    /// End of programme: live uses the guide's end; a film / episode its own end; nothing on
    /// (or no guide) refuses.
    #[test]
    fn end_of_programme_live_and_vod() {
        let run = SleepRun::arm(timer(SleepWhen::EndOfProgramme), &live(T0), Some(T0 + 1500)).unwrap();
        assert_eq!(run.status(&live(T0)).fires_at, Some(T0 + 1500));
        assert_eq!(SleepRun::arm(timer(SleepWhen::EndOfProgramme), &live(T0), None), Err(SleepError::NoGuide));
        assert_eq!(SleepRun::arm(timer(SleepWhen::EndOfProgramme), &Watch { now: T0, ..Default::default() }, None), Err(SleepError::NothingPlaying));
        let film = |left: f64, now, ended| Watch { now, media: Some(MediaRef::Movie(MovieId(7))), left_s: Some(left), ended, ..Default::default() };
        let mut run = SleepRun::arm(timer(SleepWhen::EndOfProgramme), &film(1000.0, T0, false), None).unwrap();
        assert_eq!(run.status(&film(1000.0, T0, false)).fires_at, Some(T0 + 1000));
        assert_eq!(run.step(&film(500.0, T0 + 500, false)), None);
        assert_eq!(run.step(&film(100.0, T0 + 900, false)), Some(SleepStep::Warn { at: T0 + 1000 }));
        assert_eq!(run.step(&film(0.0, T0 + 1000, true)), Some(SleepStep::Fire(SleepAction::Stop)));
    }

    /// After 2 episodes: the one on counts first; skipping to another mid-way doesn't count;
    /// auto-next at the credits does; the warning comes near the end of the last one.
    #[test]
    fn after_items_counts_finished_episodes() {
        let mut run = SleepRun::arm(timer(SleepWhen::AfterItems(2)), &ep(1, 1200.0, T0), None).unwrap();
        assert_eq!(run.step(&ep(1, 900.0, T0 + 300)), None);
        // the user picks episode 3 mid-way through 1: 1 doesn't count, 3 is counted from now
        assert_eq!(run.step(&ep(3, 1500.0, T0 + 310)), None);
        assert_eq!(run.status(&ep(3, 1500.0, T0 + 310)).items_left, Some(2));
        assert_eq!(run.step(&ep(3, 60.0, T0 + 1750)), None);
        // auto-next at the credits: 3 counts
        assert_eq!(run.step(&ep(4, 1300.0, T0 + 1760)), None);
        assert_eq!(run.status(&ep(4, 1300.0, T0 + 1760)).items_left, Some(1));
        assert_eq!(run.step(&ep(4, 110.0, T0 + 2950)), Some(SleepStep::Warn { at: T0 + 3060 }));
        let ended = Watch { ended: true, ..ep(4, 0.0, T0 + 3060) };
        assert_eq!(run.step(&ended), Some(SleepStep::Fire(SleepAction::Stop)));
        // an ended episode that stays on screen (no auto-next) counts once, however long it shows
        let mut run = SleepRun::arm(timer(SleepWhen::AfterItems(2)), &ep(1, 1200.0, T0), None).unwrap();
        for t in 0..3 {
            assert_eq!(run.step(&Watch { ended: true, ..ep(1, 0.0, T0 + 1200 + t) }), None, "tick {t}");
        }
        assert_eq!(run.step(&ep(2, 1300.0, T0 + 1300)), None);
        assert_eq!(run.status(&ep(2, 1300.0, T0 + 1300)).items_left, Some(1));
        assert_eq!(run.step(&ep(2, 110.0, T0 + 2490)), Some(SleepStep::Warn { at: T0 + 2600 }));
        assert_eq!(run.step(&Watch { ended: true, ..ep(2, 0.0, T0 + 2600) }), Some(SleepStep::Fire(SleepAction::Stop)));
        // live is never an item
        assert_eq!(SleepRun::arm(timer(SleepWhen::AfterItems(1)), &live(T0), None), Err(SleepError::NothingPlaying));
        assert_eq!(SleepRun::arm(timer(SleepWhen::AfterItems(0)), &ep(1, 10.0, T0), None), Err(SleepError::NoItems));
    }

    fn queue(row: Option<i64>, left_rows: usize, media: i64, left: f64, now: Timestamp) -> Watch {
        Watch { queue_current: row.map(QueueId), queue_left: left_rows, ..ep(media, left, now) }
    }

    /// While Up next drives playback items are its rows, and the card between
    /// two rows (the engine no longer drives; the ended row still shows) neither counts again nor
    /// ends Up next; end of queue fires when the last row ends or the queue stops.
    #[test]
    fn queue_rows_and_end_of_queue() {
        let between = |media: i64, now| Watch { ended: true, queue_between: true, ..ep(media, 0.0, now) };
        let mut run = SleepRun::arm(timer(SleepWhen::AfterItems(2)), &queue(Some(1), 2, 10, 50.0, T0), None).unwrap();
        assert_eq!(run.step(&queue(Some(1), 2, 10, 0.0, T0 + 50)), None);
        for t in 0..3 {
            assert_eq!(run.step(&between(10, T0 + 51 + t)), None, "the card, tick {t}");
        }
        assert_eq!(run.status(&between(10, T0 + 54)).items_left, Some(1));
        // the next row starts, and ends
        assert_eq!(run.step(&queue(Some(2), 1, 11, 600.0, T0 + 60)), None);
        assert_eq!(run.step(&Watch { ended: true, ..queue(Some(2), 1, 11, 0.0, T0 + 660) }), Some(SleepStep::Fire(SleepAction::Stop)));

        assert_eq!(SleepRun::arm(timer(SleepWhen::EndOfQueue), &ep(1, 10.0, T0), None), Err(SleepError::NoQueue));
        let mut run = SleepRun::arm(timer(SleepWhen::EndOfQueue), &queue(Some(1), 1, 10, 900.0, T0), None).unwrap();
        for t in 0..3 {
            assert_eq!(run.step(&between(10, T0 + 900 + t)), None, "the card, tick {t}");
        }
        assert_eq!(run.step(&queue(Some(2), 0, 11, 1000.0, T0 + 910)), None);
        assert_eq!(run.status(&queue(Some(2), 0, 11, 1000.0, T0 + 910)).fires_at, Some(T0 + 1910));
        assert_eq!(run.step(&queue(Some(2), 0, 11, 100.0, T0 + 1810)), Some(SleepStep::Warn { at: T0 + 1910 }));
        let done = Watch { ended: true, ..queue(Some(2), 0, 11, 0.0, T0 + 1910) };
        assert_eq!(run.step(&done), Some(SleepStep::Fire(SleepAction::Stop)));
        // armed on the card between two rows
        assert!(SleepRun::arm(timer(SleepWhen::EndOfQueue), &between(10, T0), None).is_ok());
        // the user clears Up next: it has run out
        let mut run = SleepRun::arm(timer(SleepWhen::EndOfQueue), &queue(Some(1), 3, 10, 900.0, T0), None).unwrap();
        assert_eq!(run.step(&ep(10, 800.0, T0 + 100)), Some(SleepStep::Fire(SleepAction::Stop)));
    }

    /// Watch reads the snapshots: Up next counts only while it drives the main picture.
    #[test]
    fn watch_follows_the_queue_only_while_it_drives() {
        use crate::queue::{QueueEntry, QueueRow, RowState};
        let mut p = PlayerSnapshot { state: PlayerState::Playing, position: Some(100.0), duration: Some(1000.0), ..Default::default() };
        let row = |id: i64, state| QueueRow {
            entry: QueueEntry {
                id: QueueId(id),
                item: crate::model::ItemRef {
                    source: crate::model::SourceId(1),
                    kind: crate::model::ItemKind::Episode,
                    key: crate::model::ItemKey(format!("e{id}")),
                },
                any_copy: false,
                programme: None,
                title: String::new(),
                subtitle: None,
                duration_s: None,
                from_start: false,
                state,
                by_agent: false,
                orphaned: false,
            },
            locked: false,
            eta: None,
            note: None,
        };
        let q = QueueSnapshot {
            rows: vec![row(1, RowState::Current), row(2, RowState::Pending)].into(),
            current: Some(QueueId(1)),
            ..Default::default()
        };
        let w = Watch::of(&p, &q, T0);
        assert_eq!((w.queue_current, w.left_s), (None, Some(900.0)), "the user plays something else");
        p.origin = Some(crate::model::PlayOrigin::Queue);
        let q = QueueSnapshot { driving: true, ..q };
        let w = Watch::of(&p, &q, T0);
        assert_eq!((w.queue_current, w.queue_left), (Some(QueueId(1)), 1));
    }

    /// The service: set → warn → fire through the actuator and the bus; cancel stops it.
    #[test]
    fn service_fires_through_the_actuator() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (_ptx, prx) = watch::channel(Arc::new(PlayerSnapshot::default()));
        let (_qtx, qrx) = watch::channel(Arc::new(QueueSnapshot::default()));
        let (bus, events) = EventBus::new(None);
        let fired = Arc::new(Mutex::new(Vec::new()));
        let f = fired.clone();
        let h = SleepService::start(rt.handle(), prx, qrx, Arc::new(move |a| f.lock().unwrap().push(a)), bus);
        assert_eq!(h.set(SleepWhen::EndOfProgramme, SleepAction::Pause, 120, None), Err(SleepError::NothingPlaying));
        let at = crate::model::now() + 2;
        let st = h.set(SleepWhen::At(at), SleepAction::Pause, 120, None).unwrap();
        assert_eq!(st.fires_at, Some(at));
        let mut seen = Vec::new();
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while fired.lock().unwrap().is_empty() && std::time::Instant::now() < end {
            if let Ok(AppEvent::Reminder(e)) = events.recv_timeout(Duration::from_millis(100)) {
                seen.push(e);
            }
        }
        assert_eq!(*fired.lock().unwrap(), [SleepAction::Pause]);
        while let Ok(AppEvent::Reminder(e)) = events.recv_timeout(Duration::from_millis(200)) {
            seen.push(e);
        }
        use super::super::ReminderEvent as E;
        assert!(seen.contains(&E::SleepWarn { at }), "{seen:?}");
        assert!(seen.contains(&E::SleepNow { action: SleepAction::Pause }), "{seen:?}");
        assert_eq!(h.status(), None);
        assert!(!h.cancel());
        h.set(SleepWhen::At(crate::model::now() + 3600), SleepAction::Stop, 120, None).unwrap();
        assert!(h.cancel() && h.status().is_none());
    }
}
