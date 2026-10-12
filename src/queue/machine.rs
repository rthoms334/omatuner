//! The pure Up next state machine (DESIGN §32 §5): player transitions, commands, ticks,
//! parental / explicit facts, multi-view and the stored rows in; effects out. Unit-tested like
//! `PlayerMachine`; `queue::service` runs it on tokio and performs the effects. OWNER: Q-core.
//!
//! * **Ownership.** Up next drives the main picture while its snapshot says `origin == Queue`
//!   on one of the refs the current row was sent with (its copies). Anything else that loads
//!   there (a pick, a zap to another channel, a reminder, an agent) makes Up next step aside: the
//!   current row keeps its state, "Up next paused · Alt+6 to carry on". Stop (`Idle`) never
//!   advances.
//! * **Advancing.** `Ended` of the current row → played, then the countdown (or the stop:
//!   stop-after, the end of Up next). The sleep timer is `reminders::sleep`'s: it reads this
//!   snapshot ([`QueueSnapshot::between_items`]) and stops the pictures itself. The next row is the first visible,
//!   available, pending row in list order; locked ones wait for the PIN (never skipped
//!   silently), concealed ones are invisible and passed over, and stay pending.
//! * **Live slots.** Airing → live (from the start through catch-up when the row asks and the
//!   archive has it), advanced at the programme's stop with a 30 s "Keep watching" card; ended →
//!   catch-up inside the window, else skipped "Expired"; future → a wait card until it starts.
//!   Times are wall clock: a suspend that jumps past a stop or a start is caught on the next tick.
//! * **Failures.** A row that fails after the player's copy fallbacks is marked failed and the
//!   countdown moves on; three in a row pause Up next.
//! * **Empty queue.** An episode that ends while Up next doesn't drive playback gets today's
//!   next-episode countdown, run here so it also works in the mini player, the pop-out and a
//!   hidden window.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::{EpisodeId, ItemRef, MediaRef, PlayOrigin, PlayableKind, QueueId, Timestamp};
use crate::player::{PlayerSnapshot, PlayerState};
use crate::queue::model::{
    AddTarget, AddedBy, Blocked, Countdown, CountdownWhy, FinishedWhy, Insert, NextEpisode, QueueCommand, QueueEntry, QueueRow, QueueSnapshot,
    RowState,
};

/// The "Keep watching" card at the end of a live slot.
pub const SLOT_END_CARD_S: i64 = 30;
/// Failed rows in a row that pause Up next.
pub const MAX_FAILED_STREAK: u8 = 3;
/// The ETA follows the playing item's position in steps this long (fewer snapshot updates).
const ETA_STEP_S: f64 = 30.0;

pub const PAUSED_TOAST: &str = "Up next paused · Alt+6 to carry on";
pub const STREAK_TOAST: &str = "Up next paused: 3 items failed in a row; provider trouble?";

/// What the engine knows about a row now (library snapshot + parental state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowFacts {
    /// Explicit while hidden: absent everywhere, never played, left pending.
    pub concealed: bool,
    /// Locked and the session is locked.
    pub needs_pin: bool,
    /// Not in the catalogue now (orphaned, gone, hidden category): passed over, "Unavailable".
    pub unavailable: bool,
    /// A slot's channel's catch-up days (0 = none).
    pub catchup_days: u32,
}

/// How a row plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// A movie or an episode.
    Vod,
    /// A live slot, live.
    Live,
    /// A slot from the archive (ended, or "from the start").
    Catchup,
}

/// The play-time resolution of a [`Effect::Play`].
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// Sent to the main picture; `items` = every ref that counts as this row (its copies).
    Sent { items: Vec<ItemRef> },
    /// Locked while the session is (facts were stale).
    NeedsPin,
    /// B1 refused: multi-view is on.
    Waits,
    /// Nothing to play any more: passed over, "Unavailable".
    Gone,
    /// Explicit while hidden: passed over, left pending.
    Concealed,
    /// Reading it failed.
    Failed(String),
}

/// What the machine reacts to.
#[derive(Debug, Clone)]
pub enum Input {
    Command(QueueCommand),
    /// The main picture's snapshot changed.
    Player(Arc<PlayerSnapshot>),
    /// Timers: countdowns, slot starts and stops (wall clock).
    Tick,
    /// Multi-view turned on / off (Up next waits while it is on).
    MultiView(bool),
    /// The stored rows (start, after a write, after an outside change).
    Rows(Vec<QueueEntry>),
    /// Fresh facts per row (a library swap, a parental change, new rows).
    Facts(HashMap<QueueId, RowFacts>),
    Resolved {
        id: QueueId,
        outcome: Resolved,
    },
    /// The answer to [`Effect::LookupNextEpisode`].
    NextEpisode(Option<NextEpisode>),
}

/// A write to `queue_item` (one small transaction each).
#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    State(QueueId, RowState),
    Move {
        ids: Vec<QueueId>,
        to: Insert,
    },
    /// [`QueueCommand::Reorder`].
    Order(Vec<QueueId>),
    /// Rows to delete (a removal, "Remove played", "Clear Up next…": never a concealed row).
    Remove(Vec<QueueId>),
    FromStart(QueueId, bool),
    PinCopy(QueueId, ItemRef),
}

/// What the engine must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Resolve this row at play time (copy plan, catch-up, PIN, B1) and play it on the main
    /// picture: exactly one per advance. Answered with [`Input::Resolved`].
    Play {
        id: QueueId,
        how: How,
        resume: bool,
    },
    /// The next-episode countdown ran out.
    PlayEpisode {
        id: EpisodeId,
        origin: PlayOrigin,
    },
    /// Find the episode after this one; answered with [`Input::NextEpisode`].
    LookupNextEpisode(EpisodeId),
    /// Look the rows' display fields up and add them; answered with [`Input::Rows`].
    Add {
        targets: Vec<AddTarget>,
        at: Insert,
        by: AddedBy,
    },
    Write(Write),
    Toast(&'static str),
    /// "Up next is waiting for your PIN" (a notification while the window is hidden).
    NotifyPin(QueueId),
}

/// The settings it runs with (`[queue]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineConfig {
    /// 0 = at once.
    pub countdown_s: u32,
    /// No countdown: the card waits for Play now.
    pub ask: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self { countdown_s: 10, ask: false }
    }
}

impl From<&crate::config::QueueConfig> for MachineConfig {
    fn from(c: &crate::config::QueueConfig) -> Self {
        Self { countdown_s: c.countdown_s, ask: c.ask }
    }
}

/// What the last player snapshot looked like (transitions are what count).
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    generation: u64,
    state: std::mem::Discriminant<PlayerState>,
    item: Option<ItemRef>,
    origin: Option<PlayOrigin>,
}

/// Where the next-episode lookup's answer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Countdown,
    /// The "That was Up next." card offers it, without a countdown.
    Offer,
}

#[derive(Debug, Default)]
pub struct QueueMachine {
    cfg: MachineConfig,
    rows: Vec<QueueEntry>,
    facts: HashMap<QueueId, RowFacts>,
    /// Session-only reasons on skipped / failed rows.
    notes: HashMap<QueueId, &'static str>,
    /// Future slots the user skipped for now: passed over until picked again.
    deferred: HashSet<QueueId>,
    /// The refs that count as the current row's while it plays.
    items: Vec<ItemRef>,
    /// The main picture plays the current row.
    owned: bool,
    /// A [`Effect::Play`] waiting for its [`Input::Resolved`].
    resolving: Option<(QueueId, How)>,
    /// A slot playing live advances at this stop.
    live_until: Option<Timestamp>,
    countdown: Option<Countdown>,
    blocked: Option<(Blocked, Option<QueueId>)>,
    stop_after: Option<QueueId>,
    interrupted: bool,
    failed_streak: u8,
    multiview: bool,
    finished: Option<FinishedWhy>,
    next_episode: Option<NextEpisode>,
    lookup: Option<Lookup>,
    last: Option<Seen>,
    /// Origin of the last foreign playback (the next-episode card keeps it).
    origin: Option<PlayOrigin>,
    /// Position of the playing row (ETA), in [`ETA_STEP_S`] steps.
    position: Option<f64>,
    snapshot: QueueSnapshot,
}

impl QueueMachine {
    pub fn new(cfg: MachineConfig) -> Self {
        Self { cfg, ..Default::default() }
    }

    pub fn set_config(&mut self, cfg: MachineConfig) {
        self.cfg = cfg;
    }

    pub fn snapshot(&self) -> &QueueSnapshot {
        &self.snapshot
    }

    pub fn rows(&self) -> &[QueueEntry] {
        &self.rows
    }

    /// When the engine must tick next (a countdown, a slot's stop or start), if ever.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let countdown = self.countdown.as_ref().and_then(|c| c.at);
        let stop = self.live_until.filter(|_| self.owned && self.countdown.is_none());
        let start = match self.blocked {
            Some((Blocked::NotStarted, Some(id))) => self.row(id).and_then(|r| r.programme.as_ref()).map(|s| s.start),
            _ => None,
        };
        [countdown, stop, start].into_iter().flatten().min()
    }

    /// One step.
    pub fn step(&mut self, input: Input, now: Timestamp) -> Vec<Effect> {
        let mut fx = Vec::new();
        match input {
            Input::Command(c) => self.command(c, now, &mut fx),
            Input::Player(s) => {
                if !self.player(&s, now, &mut fx) {
                    return fx; // a position update inside the ETA step: nothing to rebuild
                }
            }
            Input::Tick => self.tick(now, &mut fx),
            Input::MultiView(on) => {
                self.multiview = on;
                if let (false, Some((Blocked::MultiView, Some(id)))) = (on, self.blocked) {
                    self.advance(Some(id), None, now, &mut fx);
                }
            }
            Input::Rows(rows) => self.set_rows(rows),
            Input::Facts(f) => {
                self.facts = f;
                self.refit();
            }
            Input::Resolved { id, outcome } => self.resolved(id, outcome, now, &mut fx),
            Input::NextEpisode(card) => match self.lookup.take() {
                Some(Lookup::Countdown) => {
                    if let Some(card) = card {
                        self.next_episode = Some(card);
                        self.start_countdown(CountdownWhy::NextEpisode, None, now, &mut fx);
                    }
                }
                Some(Lookup::Offer) => self.next_episode = card,
                None => {}
            },
        }
        self.rebuild(now);
        fx
    }

    // ---------------------------------------------------------------- rows

    fn row(&self, id: QueueId) -> Option<&QueueEntry> {
        self.rows.iter().find(|r| r.id == id)
    }

    fn current(&self) -> Option<QueueId> {
        self.rows.iter().find(|r| r.state == RowState::Current).map(|r| r.id)
    }

    /// Rows without facts yet count as concealed (fails closed at startup).
    fn visible(&self, id: QueueId) -> bool {
        self.facts.get(&id).is_some_and(|f| !f.concealed)
    }

    fn playable(&self, r: &QueueEntry) -> bool {
        r.state == RowState::Pending
            && !self.deferred.contains(&r.id)
            && !r.orphaned
            && self.facts.get(&r.id).is_some_and(|f| !f.concealed && !f.unavailable)
    }

    /// The row that plays next: the first visible, available, pending row in list order.
    fn next_row(&self) -> Option<QueueId> {
        self.rows.iter().find(|r| self.playable(r)).map(|r| r.id)
    }

    fn set_state(&mut self, id: QueueId, state: RowState, fx: &mut Vec<Effect>) {
        for r in &mut self.rows {
            if r.id == id {
                r.state = state;
            } else if state == RowState::Current && r.state == RowState::Current {
                r.state = RowState::Pending;
            }
        }
        if state != RowState::Pending {
            self.deferred.remove(&id);
        }
        fx.push(Effect::Write(Write::State(id, state)));
    }

    fn set_rows(&mut self, rows: Vec<QueueEntry>) {
        let had = self.current();
        self.rows = rows;
        let ids: HashSet<QueueId> = self.rows.iter().map(|r| r.id).collect();
        let gone = |id: &Option<QueueId>| id.is_some_and(|i| !ids.contains(&i));
        if had.is_some() && self.current() != had {
            self.owned = false;
            self.items.clear();
            self.live_until = None;
        }
        if self.resolving.is_some_and(|(id, _)| !ids.contains(&id)) {
            self.resolving = None;
        }
        if gone(&self.stop_after) {
            self.stop_after = None;
        }
        self.deferred.retain(|id| ids.contains(id));
        self.notes.retain(|id, _| ids.contains(id));
        self.refit();
    }

    /// After the rows or their facts changed: a card about a row that went (or is concealed
    /// now) closes; a countdown goes to the new next row.
    fn refit(&mut self) {
        if self.blocked.is_some_and(|(_, id)| id.is_some_and(|i| !self.visible(i) || self.row(i).is_none())) {
            self.blocked = None;
        }
        let next = self.next_row();
        if let Some(c) = &mut self.countdown
            && c.why != CountdownWhy::NextEpisode
        {
            match next {
                Some(n) => c.id = Some(n),
                None if c.why == CountdownWhy::SlotEnded => c.id = None,
                None => self.countdown = None,
            }
        }
    }

    // ---------------------------------------------------------------- player

    /// A snapshot of the main picture; false when nothing Up next shows changed.
    fn player(&mut self, s: &PlayerSnapshot, now: Timestamp, fx: &mut Vec<Effect>) -> bool {
        let item = s.playable.as_ref().and_then(|p| p.item.clone());
        let seen = Seen { generation: s.load_generation, state: std::mem::discriminant(&s.state), item, origin: s.origin };
        if self.last.as_ref() == Some(&seen) && !self.owned {
            return false;
        }
        let Some(prev) = self.last.replace(seen.clone()) else { return false }; // startup: never plays
        let ours = s.origin == Some(PlayOrigin::Queue) && self.current().is_some() && seen.item.as_ref().is_some_and(|i| self.items.contains(i));
        let position = s.position.filter(|_| ours).map(|p| (p / ETA_STEP_S).floor() * ETA_STEP_S);
        let moved = position.is_some() && position != self.position;
        if moved {
            self.position = position;
        }
        let new_load = prev.generation != seen.generation || prev.item != seen.item || prev.origin != seen.origin;
        if new_load && s.playable.is_some() && s.state != PlayerState::Idle {
            if ours {
                self.owned = true;
                self.interrupted = false;
            } else {
                self.step_aside(fx);
                self.origin = s.origin;
            }
        }
        if !new_load && prev.state == seen.state {
            return moved;
        }
        match &s.state {
            PlayerState::Playing if ours => self.failed_streak = 0,
            PlayerState::Ended if ours && self.owned => self.item_ended(s, now, fx),
            PlayerState::Ended if !self.owned => {
                if let Some(MediaRef::Episode(id)) = s.playable.as_ref().filter(|p| matches!(p.kind, PlayableKind::Episode { .. })).map(|p| &p.media)
                {
                    self.origin = s.origin.filter(|o| *o != PlayOrigin::Queue);
                    self.lookup = Some(Lookup::Countdown);
                    fx.push(Effect::LookupNextEpisode(*id));
                }
            }
            PlayerState::Error(_) if ours && self.owned => {
                if let Some(id) = self.current() {
                    self.item_failed(id, now, fx);
                }
            }
            PlayerState::Idle => {
                // Stop: never advances; the current row keeps its place
                if self.owned {
                    self.owned = false;
                    self.interrupted = true;
                }
                self.countdown = None;
                self.live_until = None;
                self.lookup = None;
            }
            _ => {}
        }
        true
    }

    /// Something else plays on the main picture: Up next steps aside, keeping its place.
    fn step_aside(&mut self, fx: &mut Vec<Effect>) {
        let queue_card = self.countdown.as_ref().is_some_and(|c| c.why != CountdownWhy::NextEpisode) || self.blocked.is_some();
        if self.owned || queue_card {
            fx.push(Effect::Toast(PAUSED_TOAST));
            self.interrupted = true;
        }
        self.owned = false;
        self.countdown = None;
        self.blocked = None;
        self.live_until = None;
        self.lookup = None;
        self.finished = None;
        self.next_episode = None;
        self.resolving = None;
    }

    fn item_ended(&mut self, s: &PlayerSnapshot, now: Timestamp, fx: &mut Vec<Effect>) {
        let Some(id) = self.current() else { return };
        self.owned = false;
        self.live_until = None;
        self.position = None;
        self.set_state(id, RowState::Played, fx);
        if self.stop_after == Some(id) {
            self.stop_after = None;
            self.finished = Some(FinishedWhy::StopAfter(id));
            return;
        }
        match self.next_row() {
            Some(next) => self.start_countdown(CountdownWhy::Ended, Some(next), now, fx),
            None => self.run_out(s, fx),
        }
    }

    /// Nothing left: "That was Up next." — an episode's successor is offered, no countdown.
    fn run_out(&mut self, s: &PlayerSnapshot, fx: &mut Vec<Effect>) {
        self.finished = Some(FinishedWhy::EndOfQueue);
        if let Some(MediaRef::Episode(ep)) = s.playable.as_ref().map(|p| &p.media) {
            self.lookup = Some(Lookup::Offer);
            fx.push(Effect::LookupNextEpisode(*ep));
        }
    }

    fn item_failed(&mut self, id: QueueId, now: Timestamp, fx: &mut Vec<Effect>) {
        self.owned = false;
        self.live_until = None;
        self.set_state(id, RowState::Failed, fx);
        self.notes.insert(id, "Couldn't play");
        self.failed_streak = self.failed_streak.saturating_add(1);
        if self.failed_streak >= MAX_FAILED_STREAK {
            self.blocked = Some((Blocked::FailedStreak, None));
            fx.push(Effect::Toast(STREAK_TOAST));
            return;
        }
        match self.next_row() {
            Some(next) => self.start_countdown(CountdownWhy::Failed, Some(next), now, fx),
            None => self.finished = Some(FinishedWhy::EndOfQueue),
        }
    }

    // ---------------------------------------------------------------- timers

    fn start_countdown(&mut self, why: CountdownWhy, id: Option<QueueId>, now: Timestamp, fx: &mut Vec<Effect>) {
        let wait = match why {
            CountdownWhy::SlotEnded => SLOT_END_CARD_S,
            _ => i64::from(self.cfg.countdown_s),
        };
        if wait == 0 && !self.cfg.ask {
            self.countdown = Some(Countdown { id, at: Some(now), why });
            self.fire_countdown(now, fx);
            return;
        }
        self.countdown = Some(Countdown { id, at: (!self.cfg.ask).then_some(now + wait), why });
    }

    fn fire_countdown(&mut self, now: Timestamp, fx: &mut Vec<Effect>) {
        let Some(c) = self.countdown.take() else { return };
        match c.why {
            CountdownWhy::NextEpisode => {
                if let Some(ep) = self.next_episode.take() {
                    fx.push(Effect::PlayEpisode { id: ep.id, origin: self.origin.unwrap_or(PlayOrigin::Fullscreen) });
                }
            }
            CountdownWhy::SlotEnded => {
                if let Some(cur) = self.current() {
                    self.owned = false;
                    self.set_state(cur, RowState::Played, fx);
                }
                self.advance(c.id, None, now, fx);
            }
            CountdownWhy::Ended | CountdownWhy::Failed => self.advance(c.id, None, now, fx),
        }
    }

    fn tick(&mut self, now: Timestamp, fx: &mut Vec<Effect>) {
        self.sweep_expired(now, fx);
        if self.countdown.as_ref().and_then(|c| c.at).is_some_and(|at| at <= now) {
            self.fire_countdown(now, fx);
        }
        if self.owned && self.live_until.is_some_and(|stop| stop <= now) && self.countdown.is_none() {
            self.live_until = None;
            match self.next_row() {
                Some(next) => self.start_countdown(CountdownWhy::SlotEnded, Some(next), now, fx),
                // nothing after it: keep watching the channel, outside Up next
                None => {
                    if let Some(cur) = self.current() {
                        self.owned = false;
                        self.set_state(cur, RowState::Played, fx);
                    }
                }
            }
        }
        if let Some((Blocked::NotStarted, Some(id))) = self.blocked
            && self.row(id).and_then(|r| r.programme.as_ref()).is_some_and(|s| s.start <= now)
        {
            self.advance(Some(id), None, now, fx);
        }
    }

    /// Pending slots that ended outside their catch-up window: skipped, "Expired".
    fn sweep_expired(&mut self, now: Timestamp, fx: &mut Vec<Effect>) {
        let expired: Vec<QueueId> = self
            .rows
            .iter()
            .filter(|r| r.state == RowState::Pending)
            .filter(|r| r.programme.as_ref().is_some_and(|s| s.stop <= now && !self.catchup_ok(r.id, s.start, now)))
            .filter(|r| self.facts.contains_key(&r.id))
            .map(|r| r.id)
            .collect();
        for id in expired {
            self.set_state(id, RowState::Skipped, fx);
            self.notes.insert(id, "Expired");
        }
    }

    fn catchup_ok(&self, id: QueueId, start: Timestamp, now: Timestamp) -> bool {
        let days = self.facts.get(&id).map_or(0, |f| f.catchup_days);
        days > 0 && start < now && start >= now - i64::from(days) * 86_400
    }

    // ---------------------------------------------------------------- playing

    /// Play `id` (or, when it can't play, the rows after it). `resume`: `None` follows the
    /// row's "from the start"; `Some(true)` resumes whatever it says (Resume of an interrupted
    /// row, "Resume from" in the Resume prompt); `Some(false)` starts over (the prompt's "Start
    /// over"). Only the row asked for gets the choice; rows played in its place follow their own.
    fn advance(&mut self, id: Option<QueueId>, resume: Option<bool>, now: Timestamp, fx: &mut Vec<Effect>) {
        self.countdown = None;
        self.blocked = None;
        self.finished = None;
        self.next_episode = None;
        self.lookup = None;
        self.resolving = None;
        let asked = id;
        let mut id = id;
        for _ in 0..=self.rows.len() {
            let Some(row) = id.and_then(|i| self.row(i)).cloned() else { return };
            let facts = self.facts.get(&row.id).copied().unwrap_or(RowFacts { concealed: true, ..Default::default() });
            if facts.concealed || facts.unavailable || row.orphaned {
                id = self.next_after(row.id);
                continue;
            }
            if self.multiview {
                self.blocked = Some((Blocked::MultiView, Some(row.id)));
                return;
            }
            if facts.needs_pin {
                self.blocked = Some((Blocked::NeedsPin, Some(row.id)));
                fx.push(Effect::NotifyPin(row.id));
                return;
            }
            let how = match &row.programme {
                None => How::Vod,
                Some(s) if now < s.start => {
                    self.blocked = Some((Blocked::NotStarted, Some(row.id)));
                    return;
                }
                Some(s) if now >= s.stop => {
                    if !self.catchup_ok(row.id, s.start, now) {
                        self.set_state(row.id, RowState::Skipped, fx);
                        self.notes.insert(row.id, "Expired");
                        id = self.next_row();
                        continue;
                    }
                    How::Catchup
                }
                Some(s) if row.from_start && self.catchup_ok(row.id, s.start, now) => How::Catchup,
                Some(_) => How::Live,
            };
            let resume = match (resume, asked == Some(row.id)) {
                (Some(r), true) => r,
                _ => !row.from_start,
            };
            self.resolving = Some((row.id, how));
            fx.push(Effect::Play { id: row.id, how, resume });
            return;
        }
    }

    /// The next playable row after `id` in list order (the rows before it were passed over).
    fn next_after(&self, id: QueueId) -> Option<QueueId> {
        let at = self.rows.iter().position(|r| r.id == id)?;
        self.rows[at + 1..].iter().find(|r| self.playable(r)).map(|r| r.id).or_else(|| self.next_row().filter(|n| *n != id))
    }

    fn resolved(&mut self, id: QueueId, outcome: Resolved, now: Timestamp, fx: &mut Vec<Effect>) {
        let Some((want, how)) = self.resolving else { return };
        if want != id {
            return; // superseded
        }
        self.resolving = None;
        match outcome {
            Resolved::Sent { items } => {
                self.set_state(id, RowState::Current, fx);
                self.items = items;
                self.interrupted = false;
                self.position = None;
                self.live_until = match how {
                    How::Live => self.row(id).and_then(|r| r.programme.as_ref()).map(|s| s.stop),
                    _ => None,
                };
            }
            Resolved::NeedsPin => {
                self.blocked = Some((Blocked::NeedsPin, Some(id)));
                fx.push(Effect::NotifyPin(id));
            }
            Resolved::Waits => self.blocked = Some((Blocked::MultiView, Some(id))),
            Resolved::Gone => {
                if let Some(f) = self.facts.get_mut(&id) {
                    f.unavailable = true;
                }
                let next = self.next_after(id);
                self.advance(next, None, now, fx);
            }
            Resolved::Concealed => {
                if let Some(f) = self.facts.get_mut(&id) {
                    f.concealed = true;
                }
                let next = self.next_after(id);
                self.advance(next, None, now, fx);
            }
            Resolved::Failed(_) => self.item_failed(id, now, fx),
        }
    }

    // ---------------------------------------------------------------- commands

    fn command(&mut self, c: QueueCommand, now: Timestamp, fx: &mut Vec<Effect>) {
        match c {
            QueueCommand::Add { targets, at, by } => fx.push(Effect::Add { targets, at, by }),
            QueueCommand::Move { ids, to } => fx.push(Effect::Write(Write::Move { ids, to })),
            QueueCommand::Reorder(ids) => fx.push(Effect::Write(Write::Order(ids))),
            QueueCommand::Remove(ids) => fx.push(Effect::Write(Write::Remove(ids))),
            QueueCommand::Clear { played_only } => {
                // only what the user can see: concealed rows stay pending in their places
                let ids = self.rows.iter().filter(|r| self.visible(r.id) && (!played_only || r.state.is_done())).map(|r| r.id).collect();
                fx.push(Effect::Write(Write::Remove(ids)));
            }
            QueueCommand::FromStart(id, on) => fx.push(Effect::Write(Write::FromStart(id, on))),
            QueueCommand::PinCopy(id, item) => fx.push(Effect::Write(Write::PinCopy(id, item))),
            QueueCommand::PlayAt(id) => {
                self.deferred.remove(&id);
                self.interrupted = false;
                self.advance(Some(id), None, now, fx);
            }
            QueueCommand::PlayAtFrom { id, resume } => {
                self.deferred.remove(&id);
                self.interrupted = false;
                self.advance(Some(id), Some(resume), now, fx);
            }
            QueueCommand::Next => self.next(now, fx),
            QueueCommand::Prev => {
                let before = self.current().or(self.next_row());
                let end = before.and_then(|b| self.rows.iter().position(|r| r.id == b)).unwrap_or(self.rows.len());
                match self.rows[..end].iter().rev().find(|r| r.state.is_done() && self.visible(r.id) && !r.orphaned).map(|r| r.id) {
                    Some(prev) => {
                        self.interrupted = false;
                        self.advance(Some(prev), None, now, fx);
                    }
                    None => fx.push(Effect::Toast("Nothing before this in Up next")),
                }
            }
            QueueCommand::Now => {
                if self.countdown.is_some() {
                    self.fire_countdown(now, fx);
                }
            }
            QueueCommand::Cancel => {
                if let Some(c) = self.countdown.take()
                    && c.why == CountdownWhy::SlotEnded
                    && let Some(cur) = self.current()
                {
                    // "Keep watching": the channel plays on, outside Up next
                    self.owned = false;
                    self.set_state(cur, RowState::Played, fx);
                }
                self.blocked = None;
                self.finished = None;
                self.next_episode = None;
                self.lookup = None;
            }
            QueueCommand::StopAfter(id) => self.stop_after = id,
            QueueCommand::Proceed { unlocked } => {
                if let Some((Blocked::NeedsPin, Some(id))) = self.blocked {
                    if unlocked {
                        // the parental state is re-read at play time: the PIN card's unlock
                        // counts only once the app published it
                        if let Some(f) = self.facts.get_mut(&id) {
                            f.needs_pin = false;
                        }
                        self.advance(Some(id), None, now, fx);
                    } else {
                        self.next(now, fx);
                    }
                }
            }
            QueueCommand::Resume => {
                self.interrupted = false;
                if matches!(self.blocked, Some((Blocked::FailedStreak, _))) {
                    self.failed_streak = 0;
                }
                match self.current() {
                    Some(cur) => self.advance(Some(cur), Some(true), now, fx),
                    None => self.advance(self.next_row(), None, now, fx),
                }
            }
        }
    }

    /// `n` / MPRIS Next: the locked row is skipped, a future slot is left for later, a
    /// countdown plays now, a playing row is skipped.
    fn next(&mut self, now: Timestamp, fx: &mut Vec<Effect>) {
        match self.blocked {
            Some((Blocked::NeedsPin, Some(id))) => {
                self.set_state(id, RowState::Skipped, fx);
                self.notes.insert(id, "Locked");
                self.advance(self.next_row(), None, now, fx);
                return;
            }
            Some((Blocked::NotStarted, Some(id))) => {
                self.deferred.insert(id);
                self.advance(self.next_row(), None, now, fx);
                return;
            }
            _ => {}
        }
        if self.countdown.is_some() {
            self.fire_countdown(now, fx);
            return;
        }
        let Some(next) = self.next_row() else {
            fx.push(Effect::Toast("Up next is empty"));
            return;
        };
        // a row Up next plays is skipped; an interrupted one keeps its place (Resume)
        if self.owned
            && let Some(cur) = self.current()
        {
            self.owned = false;
            self.set_state(cur, RowState::Skipped, fx);
        }
        self.interrupted = false;
        self.advance(Some(next), None, now, fx);
    }

    // ---------------------------------------------------------------- snapshot

    fn rebuild(&mut self, now: Timestamp) {
        let current = self.current();
        let driving = self.owned && current.is_some();
        // the ETA: from the countdown's end, or what is left of the playing row
        let mut cursor = self.countdown.as_ref().and_then(|c| c.at).unwrap_or(now);
        let mut exact = true;
        if driving && let Some(r) = current.and_then(|c| self.row(c)) {
            match (&r.programme, r.duration_s) {
                (Some(s), _) if self.live_until.is_some() => cursor = cursor.max(s.stop),
                (_, Some(d)) => cursor += (d - self.position.unwrap_or(0.0)).max(0.0) as i64,
                (_, None) => exact = false,
            }
        }
        let next = self.next_row();
        let mut rows = Vec::with_capacity(self.rows.len());
        let mut any_pending = false;
        for r in &self.rows {
            let Some(f) = self.facts.get(&r.id).filter(|f| !f.concealed) else { continue };
            let eta = if self.playable(r) {
                any_pending = true;
                let at = match &r.programme {
                    Some(s) => cursor.max(s.start),
                    None => cursor,
                };
                cursor = match (&r.programme, r.duration_s) {
                    (Some(s), _) => s.stop.max(at),
                    (None, Some(d)) => at + d as i64,
                    (None, None) => {
                        exact = false;
                        at
                    }
                };
                Some(at)
            } else {
                None
            };
            let note = self.notes.get(&r.id).copied().or((r.orphaned || f.unavailable).then_some("Unavailable"));
            rows.push(QueueRow { entry: r.clone(), locked: f.needs_pin, eta, note: note.map(str::to_owned) });
        }
        self.snapshot = QueueSnapshot {
            rows: rows.into(),
            current: current.filter(|c| self.visible(*c)),
            next,
            countdown: self.countdown.clone(),
            blocked: self.blocked.map(|b| b.0),
            blocked_id: self.blocked.and_then(|b| b.1),
            stop_after: self.stop_after.filter(|c| self.visible(*c)),
            eta_end: any_pending.then_some((cursor, exact)),
            interrupted: self.interrupted,
            failed_streak: self.failed_streak,
            driving,
            finished: self.finished,
            next_episode: self.next_episode.clone(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpisodeId, ItemKey, ItemKind, MovieId, SourceId};
    use crate::player::PlayerError;
    use crate::player::PlayerErrorKind;
    use crate::queue::model::Slot;

    const T0: Timestamp = 1_800_000_000;

    fn item(kind: ItemKind, key: &str) -> ItemRef {
        ItemRef { source: SourceId(1), kind, key: ItemKey(key.into()) }
    }

    fn movie(id: i64) -> QueueEntry {
        QueueEntry {
            id: QueueId(id),
            item: item(ItemKind::Movie, &format!("m{id}")),
            any_copy: true,
            programme: None,
            title: format!("Movie {id}"),
            subtitle: None,
            duration_s: Some(3600.0),
            from_start: false,
            state: RowState::Pending,
            by_agent: false,
            orphaned: false,
        }
    }

    fn slot(id: i64, start: Timestamp, stop: Timestamp) -> QueueEntry {
        QueueEntry {
            item: item(ItemKind::Channel, &format!("c{id}")),
            programme: Some(Slot { start, stop, catchup_id: None }),
            duration_s: Some((stop - start) as f64),
            ..movie(id)
        }
    }

    /// A machine over `rows`, every row visible and unlocked, the player idle.
    fn machine(rows: Vec<QueueEntry>) -> QueueMachine {
        let mut m = QueueMachine::new(MachineConfig { countdown_s: 10, ask: false });
        let facts = rows.iter().map(|r| (r.id, RowFacts { catchup_days: 7, ..Default::default() })).collect();
        m.step(Input::Rows(rows), T0);
        m.step(Input::Facts(facts), T0);
        m.step(Input::Player(Arc::new(PlayerSnapshot::default())), T0);
        m
    }

    fn snap(state: PlayerState, generation: u64, origin: PlayOrigin, it: &ItemRef, kind: PlayableKind, media: MediaRef) -> Arc<PlayerSnapshot> {
        Arc::new(PlayerSnapshot {
            state,
            load_generation: generation,
            origin: Some(origin),
            playable: Some(Arc::new(crate::model::Playable {
                media,
                kind,
                item: Some(it.clone()),
                source_id: Some(it.source),
                title: "t".into(),
                subtitle: None,
                logo: None,
                number: None,
                url: crate::redact::Secret::new("http://x".into()),
                headers: Default::default(),
                start_at: None,
                duration_hint: None,
            })),
            ..Default::default()
        })
    }

    fn plays(fx: &[Effect]) -> Vec<QueueId> {
        fx.iter().filter_map(|e| if let Effect::Play { id, .. } = e { Some(*id) } else { None }).collect()
    }

    /// Play row `id` through the engine's round trip: Play → Resolved → the player loads it.
    fn start(m: &mut QueueMachine, id: i64, generation: u64, now: Timestamp) -> Vec<Effect> {
        start_in(m, id, generation, now, PlayerState::Playing)
    }

    /// [`start`] up to `state` (a row that fails never gets to Playing).
    fn start_in(m: &mut QueueMachine, id: i64, generation: u64, now: Timestamp, state: PlayerState) -> Vec<Effect> {
        let mut fx = m.step(Input::Command(QueueCommand::PlayAt(QueueId(id))), now);
        let it = m.row(QueueId(id)).unwrap().item.clone();
        fx.extend(m.step(Input::Resolved { id: QueueId(id), outcome: Resolved::Sent { items: vec![it.clone()] } }, now));
        fx.extend(m.step(Input::Player(snap(state, generation, PlayOrigin::Queue, &it, PlayableKind::Movie, MediaRef::Movie(MovieId(id)))), now));
        fx
    }

    fn end(m: &mut QueueMachine, id: i64, generation: u64, now: Timestamp) -> Vec<Effect> {
        let it = m.row(QueueId(id)).unwrap().item.clone();
        m.step(Input::Player(snap(PlayerState::Ended, generation, PlayOrigin::Queue, &it, PlayableKind::Movie, MediaRef::Movie(MovieId(id)))), now)
    }

    /// EOF → countdown → exactly one Play for the next row; Now skips the wait; Cancel keeps
    /// the next row pending and plays nothing.
    #[test]
    fn an_ended_item_counts_down_to_exactly_one_play_of_the_next_row() {
        let mut m = machine(vec![movie(1), movie(2), movie(3)]);
        start(&mut m, 1, 1, T0);
        assert!(m.snapshot().driving);
        let fx = end(&mut m, 1, 1, T0 + 100);
        assert!(plays(&fx).is_empty(), "the countdown first");
        assert_eq!(m.snapshot().countdown, Some(Countdown { id: Some(QueueId(2)), at: Some(T0 + 110), why: CountdownWhy::Ended }));
        assert_eq!(m.snapshot().row(QueueId(1)).unwrap().entry.state, RowState::Played);
        assert!(plays(&m.step(Input::Tick, T0 + 105)).is_empty(), "not yet");
        assert_eq!(plays(&m.step(Input::Tick, T0 + 110)), vec![QueueId(2)]);
        assert!(plays(&m.step(Input::Tick, T0 + 111)).is_empty(), "once");

        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        end(&mut m, 1, 1, T0);
        assert_eq!(plays(&m.step(Input::Command(QueueCommand::Now), T0 + 1)), vec![QueueId(2)]);

        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        end(&mut m, 1, 1, T0);
        assert!(m.step(Input::Command(QueueCommand::Cancel), T0 + 1).is_empty());
        assert!(plays(&m.step(Input::Tick, T0 + 60)).is_empty());
        assert_eq!(m.snapshot().next, Some(QueueId(2)));
        assert_eq!(m.snapshot().row(QueueId(2)).unwrap().entry.state, RowState::Pending);
    }

    /// A foreign play mid-item: Up next steps aside with the row kept current; Resume plays it
    /// again at its position. Stop (Idle) never advances.
    #[test]
    fn a_foreign_play_interrupts_and_resume_carries_on() {
        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        let news = item(ItemKind::Channel, "news");
        let fx =
            m.step(Input::Player(snap(PlayerState::Playing, 2, PlayOrigin::Guide, &news, PlayableKind::Live, MediaRef::Url("n".into()))), T0 + 60);
        assert_eq!(fx, vec![Effect::Toast(PAUSED_TOAST)]);
        let s = m.snapshot();
        assert!(s.interrupted && !s.driving);
        assert_eq!(s.current, Some(QueueId(1)), "keeps its place");
        let fx = m.step(Input::Command(QueueCommand::Resume), T0 + 120);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(1), how: How::Vod, resume: true }]);

        // `n` after the interruption plays the next row; the interrupted one isn't skipped
        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        m.step(Input::Player(snap(PlayerState::Playing, 2, PlayOrigin::Guide, &news, PlayableKind::Live, MediaRef::Url("n".into()))), T0 + 60);
        let fx = m.step(Input::Command(QueueCommand::Next), T0 + 90);
        assert_eq!(plays(&fx), vec![QueueId(2)]);
        assert!(!fx.iter().any(|e| matches!(e, Effect::Write(Write::State(_, RowState::Skipped)))), "{fx:?}");
        m.step(Input::Resolved { id: QueueId(2), outcome: Resolved::Sent { items: vec![m.row(QueueId(2)).unwrap().item.clone()] } }, T0 + 90);
        assert_eq!(m.snapshot().row(QueueId(1)).unwrap().entry.state, RowState::Pending, "still to play, in its place");

        // a zap that keeps the Queue origin onto another channel is foreign too
        let mut m = machine(vec![movie(1)]);
        start(&mut m, 1, 1, T0);
        let fx = m.step(Input::Player(snap(PlayerState::Playing, 2, PlayOrigin::Queue, &news, PlayableKind::Live, MediaRef::Url("n".into()))), T0);
        assert_eq!(fx, vec![Effect::Toast(PAUSED_TOAST)]);

        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        let fx = m.step(Input::Player(Arc::new(PlayerSnapshot { load_generation: 1, ..Default::default() })), T0 + 60);
        assert!(fx.is_empty(), "{fx:?}");
        assert!(plays(&m.step(Input::Tick, T0 + 600)).is_empty());
        assert_eq!(m.snapshot().current, Some(QueueId(1)));
    }

    /// A locked next row with the session locked waits for the PIN (never skipped silently);
    /// the PIN plays it, `n` skips it as skipped.
    #[test]
    fn a_locked_row_waits_for_the_pin() {
        let locked = || {
            let mut m = machine(vec![movie(1), movie(2), movie(3)]);
            let mut facts = m.facts.clone();
            facts.get_mut(&QueueId(2)).unwrap().needs_pin = true;
            m.step(Input::Facts(facts), T0);
            start(&mut m, 1, 1, T0);
            end(&mut m, 1, 1, T0);
            let fx = m.step(Input::Command(QueueCommand::Now), T0);
            assert_eq!(fx, vec![Effect::NotifyPin(QueueId(2))]);
            m
        };
        let mut m = locked();
        assert_eq!((m.snapshot().blocked, m.snapshot().blocked_id), (Some(Blocked::NeedsPin), Some(QueueId(2))));
        assert!(m.snapshot().row(QueueId(2)).unwrap().locked);
        assert!(plays(&m.step(Input::Tick, T0 + 600)).is_empty(), "it waits");
        assert_eq!(plays(&m.step(Input::Command(QueueCommand::Proceed { unlocked: true }), T0)), vec![QueueId(2)]);

        let mut m = locked();
        let fx = m.step(Input::Command(QueueCommand::Next), T0);
        assert_eq!(plays(&fx), vec![QueueId(3)]);
        assert!(fx.contains(&Effect::Write(Write::State(QueueId(2), RowState::Skipped))));
        assert_eq!(m.snapshot().row(QueueId(2)).unwrap().note.as_deref(), Some("Locked"));
    }

    /// Explicit rows while hidden: absent from the rows, the counts and the ETA, and passed
    /// over; shown again, they are still pending in their place.
    #[test]
    fn concealed_rows_are_absent_and_passed_over_but_kept() {
        let mut m = machine(vec![movie(1), movie(2), movie(3)]);
        let mut facts = m.facts.clone();
        facts.get_mut(&QueueId(2)).unwrap().concealed = true;
        m.step(Input::Facts(facts), T0);
        let s = m.snapshot();
        assert_eq!(s.rows.iter().map(|r| r.entry.id.0).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(s.pending(), 2);
        assert_eq!(s.place(QueueId(3)), Some((2, 2)), "\"2 of 2\"");
        assert_eq!(s.eta_end, Some((T0 + 7200, true)), "two hours, not three");
        let clear = m.step(Input::Command(QueueCommand::Clear { played_only: false }), T0);
        assert_eq!(clear, vec![Effect::Write(Write::Remove(vec![QueueId(1), QueueId(3)]))], "\"Clear Up next…\" leaves what it can't show");
        start(&mut m, 1, 1, T0);
        end(&mut m, 1, 1, T0);
        assert_eq!(plays(&m.step(Input::Command(QueueCommand::Now), T0)), vec![QueueId(3)]);
        let shown = m.rows.iter().map(|r| (r.id, RowFacts::default())).collect();
        m.step(Input::Facts(shown), T0);
        let r2 = m.snapshot().row(QueueId(2)).expect("back");
        assert_eq!(r2.entry.state, RowState::Pending);
        assert_eq!(m.snapshot().rows.iter().position(|r| r.entry.id == QueueId(2)), Some(1), "in its place");
    }

    /// Stop-after and the last row stop without the next-episode countdown; an episode ending
    /// outside Up next gets it, run by the engine.
    #[test]
    fn the_end_of_up_next_stops_and_an_episode_outside_it_counts_down() {
        let mut m = machine(vec![movie(1), movie(2)]);
        m.step(Input::Command(QueueCommand::StopAfter(Some(QueueId(1)))), T0);
        start(&mut m, 1, 1, T0);
        assert!(end(&mut m, 1, 1, T0).iter().all(|e| matches!(e, Effect::Write(_))));
        assert_eq!((m.snapshot().finished, &m.snapshot().countdown), (Some(FinishedWhy::StopAfter(QueueId(1))), &None));

        // the last row was an episode: its successor is offered, no countdown
        let mut m = machine(vec![QueueEntry { item: item(ItemKind::Episode, "e1"), ..movie(1) }]);
        start(&mut m, 1, 1, T0);
        let it = m.row(QueueId(1)).unwrap().item.clone();
        let ep = PlayableKind::Episode { season: 1, episode: 1 };
        let fx = m.step(Input::Player(snap(PlayerState::Ended, 1, PlayOrigin::Queue, &it, ep.clone(), MediaRef::Episode(EpisodeId(7)))), T0);
        assert!(fx.contains(&Effect::LookupNextEpisode(EpisodeId(7))));
        let card = NextEpisode { id: EpisodeId(8), label: "S01E02 · Two".into(), series: "Show".into(), season: 1, episode: 2 };
        m.step(Input::NextEpisode(Some(card.clone())), T0);
        assert_eq!((m.snapshot().finished, &m.snapshot().countdown), (Some(FinishedWhy::EndOfQueue), &None));
        assert_eq!(m.snapshot().next_episode.as_ref(), Some(&card));

        // an episode played outside Up next: today's next-episode card, then the episode
        let mut m = machine(vec![]);
        let own = item(ItemKind::Episode, "x");
        let fx = m.step(Input::Player(snap(PlayerState::Playing, 1, PlayOrigin::Home, &own, ep.clone(), MediaRef::Episode(EpisodeId(3)))), T0);
        assert!(fx.is_empty());
        let fx = m.step(Input::Player(snap(PlayerState::Ended, 1, PlayOrigin::Home, &own, ep, MediaRef::Episode(EpisodeId(3)))), T0);
        assert_eq!(fx, vec![Effect::LookupNextEpisode(EpisodeId(3))]);
        m.step(Input::NextEpisode(Some(NextEpisode { id: EpisodeId(4), label: "S01E04".into(), series: "Show".into(), season: 1, episode: 4 })), T0);
        assert_eq!(m.snapshot().countdown.as_ref().map(|c| c.why), Some(CountdownWhy::NextEpisode));
        assert_eq!(m.step(Input::Tick, T0 + 10), vec![Effect::PlayEpisode { id: EpisodeId(4), origin: PlayOrigin::Home }]);
    }

    /// The Resume prompt's answer (Enter on a row): "Start over" plays from 0 and "Resume
    /// from" resumes, whatever the row's own "from the start" says; a row played in place of
    /// the one asked for (gone) follows its own setting.
    #[test]
    fn the_resume_prompt_answer_overrides_the_row_setting() {
        let mut m = machine(vec![movie(1), QueueEntry { from_start: true, ..movie(2) }]);
        let fx = m.step(Input::Command(QueueCommand::PlayAtFrom { id: QueueId(1), resume: false }), T0);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(1), how: How::Vod, resume: false }]);
        let fx = m.step(Input::Command(QueueCommand::PlayAtFrom { id: QueueId(2), resume: true }), T0);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(2), how: How::Vod, resume: true }]);
        // the plain PlayAt still follows the row
        let fx = m.step(Input::Command(QueueCommand::PlayAt(QueueId(2))), T0);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(2), how: How::Vod, resume: false }]);

        let mut m = machine(vec![movie(1), movie(2)]);
        m.step(Input::Facts([(QueueId(1), RowFacts { unavailable: true, ..Default::default() }), (QueueId(2), RowFacts::default())].into()), T0);
        let fx = m.step(Input::Command(QueueCommand::PlayAtFrom { id: QueueId(1), resume: false }), T0);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(2), how: How::Vod, resume: true }], "the next row resumes as it would");
    }

    /// Live slots: airing plays live and advances at its stop with a 30 s card; ended plays
    /// from the archive; expired is skipped with its reason; future waits until it starts —
    /// across a suspend that jumps the clock.
    #[test]
    fn programme_slots_follow_the_clock() {
        let h = 3600;
        // airing → live, then the 30 s card at the stop
        let mut m = machine(vec![slot(1, T0 - 600, T0 + h), movie(2)]);
        let fx = m.step(Input::Command(QueueCommand::PlayAt(QueueId(1))), T0);
        assert_eq!(fx, vec![Effect::Play { id: QueueId(1), how: How::Live, resume: true }]);
        let ch = m.row(QueueId(1)).unwrap().item.clone();
        m.step(Input::Resolved { id: QueueId(1), outcome: Resolved::Sent { items: vec![ch.clone()] } }, T0);
        m.step(Input::Player(snap(PlayerState::Playing, 1, PlayOrigin::Queue, &ch, PlayableKind::Live, MediaRef::Url("c".into()))), T0);
        assert_eq!(m.next_deadline(), Some(T0 + h));
        m.step(Input::Tick, T0 + h);
        assert_eq!(m.snapshot().countdown, Some(Countdown { id: Some(QueueId(2)), at: Some(T0 + h + 30), why: CountdownWhy::SlotEnded }));
        let fx = m.step(Input::Tick, T0 + h + 30);
        assert_eq!(plays(&fx), vec![QueueId(2)]);
        assert!(fx.contains(&Effect::Write(Write::State(QueueId(1), RowState::Played))));

        // from the start through catch-up when the row asks
        let mut m = machine(vec![QueueEntry { from_start: true, ..slot(1, T0 - 600, T0 + h) }]);
        assert_eq!(
            m.step(Input::Command(QueueCommand::PlayAt(QueueId(1))), T0),
            vec![Effect::Play { id: QueueId(1), how: How::Catchup, resume: false }]
        );

        // ended inside the window → catch-up; outside it → skipped "Expired"
        let mut m = machine(vec![slot(1, T0 - 2 * h, T0 - h), slot(2, T0 - 9 * 86_400, T0 - 9 * 86_400 + h), movie(3)]);
        assert_eq!(m.step(Input::Command(QueueCommand::PlayAt(QueueId(1))), T0)[0], Effect::Play { id: QueueId(1), how: How::Catchup, resume: true });
        let mut m2 = machine(m.rows.clone());
        let fx = m2.step(Input::Command(QueueCommand::PlayAt(QueueId(2))), T0);
        assert_eq!(plays(&fx), vec![QueueId(1)], "the expired one is skipped, the next plays");
        assert_eq!(m2.snapshot().row(QueueId(2)).unwrap().note.as_deref(), Some("Expired"));

        // future → wait card; a suspend past the start plays it on the next tick
        let mut m = machine(vec![slot(1, T0 + h, T0 + 2 * h)]);
        assert!(m.step(Input::Command(QueueCommand::PlayAt(QueueId(1))), T0).is_empty());
        assert_eq!(m.snapshot().blocked, Some(Blocked::NotStarted));
        assert_eq!(m.next_deadline(), Some(T0 + h));
        assert_eq!(m.step(Input::Tick, T0 + h + 900), vec![Effect::Play { id: QueueId(1), how: How::Live, resume: true }]);
        // … and one that jumped past the whole programme plays it from the archive
        let mut m = machine(vec![slot(1, T0 + h, T0 + 2 * h)]);
        m.step(Input::Command(QueueCommand::PlayAt(QueueId(1))), T0);
        assert_eq!(m.step(Input::Tick, T0 + 3 * h), vec![Effect::Play { id: QueueId(1), how: How::Catchup, resume: true }]);
    }

    /// Three failed rows in a row pause Up next; a row that plays resets the streak.
    #[test]
    fn three_failures_in_a_row_pause() {
        let mut m = machine((1..=5).map(movie).collect());
        let fail = |m: &mut QueueMachine, id: i64, generation: u64| {
            start_in(m, id, generation, T0, PlayerState::Loading { since: std::time::Instant::now(), attempt: 1 });
            let it = m.row(QueueId(id)).unwrap().item.clone();
            let err = PlayerState::Error(PlayerError { kind: PlayerErrorKind::HttpNotFound, message: String::new() });
            m.step(Input::Player(snap(err, generation, PlayOrigin::Queue, &it, PlayableKind::Movie, MediaRef::Movie(MovieId(id)))), T0)
        };
        fail(&mut m, 1, 1);
        fail(&mut m, 2, 2);
        assert_eq!(m.snapshot().failed_streak, 2);
        assert_eq!(m.snapshot().countdown.as_ref().map(|c| c.why), Some(CountdownWhy::Failed));
        start(&mut m, 3, 3, T0);
        assert_eq!(m.snapshot().failed_streak, 0, "it played");
        for id in 3..=5 {
            fail(&mut m, id, 10 + id as u64);
        }
        assert_eq!(m.snapshot().blocked, Some(Blocked::FailedStreak));
        assert_eq!(m.snapshot().countdown, None);
        assert_eq!(m.snapshot().row(QueueId(5)).unwrap().note.as_deref(), Some("Couldn't play"));
    }

    /// Up next waits while multi-view is on (B1) and carries on when it ends.
    #[test]
    fn multiview_holds_the_advance() {
        let mut m = machine(vec![movie(1), movie(2)]);
        start(&mut m, 1, 1, T0);
        end(&mut m, 1, 1, T0);
        m.step(Input::MultiView(true), T0);
        assert!(plays(&m.step(Input::Tick, T0 + 10)).is_empty());
        assert_eq!(m.snapshot().blocked, Some(Blocked::MultiView));
        assert_eq!(plays(&m.step(Input::MultiView(false), T0 + 20)), vec![QueueId(2)]);
    }
}
