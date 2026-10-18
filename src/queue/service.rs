//! The Up next engine task (tokio): commands in, `AppShared::publish_queue` out, timers
//! (countdown, slot start / stop, ≤ 60 s wall-clock re-check, suspend-safe), play-time
//! resolution on the blocking pool. OWNER: Q-core.
//!
//! One task runs [`QueueMachine`] and performs its effects in order, each to completion, before
//! the next input: store writes go through the writer (one small transaction each) and the list
//! is re-read after every add, move or removal, so the machine and `queue_item` never disagree.
//! Reads per operation take `AppShared::library()` and drop it (never held across an await),
//! the parental state comes from `AppShared::parental` with the live config, and every play
//! goes through B1 (`playback::route`, `PlaySource::Queue`: it waits while multi-view is on).
//!
//! Guardrails: a locked row never plays without the PIN (the engine re-reads the parental state
//! at play time and never unlocks anything); concealed rows are invisible everywhere and never
//! play; titles are never logged (`queue: #12 → #13 (episode, source 2)`); restarting never
//! plays anything.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{Config, SharedConfig};
use crate::model::{ChannelId, ItemKind, ItemRef, MediaRef, PlayOrigin, PlayableKind, SeriesId, Timestamp, VodKind, WatchlistId, WatchlistItemId};
use crate::msg::{EventBus, Toast, ToastKind};
use crate::playback::{Kind, PlayIntent, PlaySource, Route, RouteState};
use crate::player::{CopyPlay, PlayRequest, PlayerCommand, PlayerHandle, PlayerSnapshot};
use crate::queue::machine::{Effect, How, Input, MachineConfig, PAUSED_TOAST, QueueMachine, Resolved, RowFacts, Write};
use crate::queue::model::{AddTarget, AddedBy, Blocked, Insert, NextEpisode, PlayReply, QueueCommand, QueueEntry, QueueReply, Refusal, Slot};
use crate::shared::{AppShared, ParentalView};
use crate::store::Store;
use crate::store::library::Library;
use crate::store::queue::{MAX_ADD, NewRow};

/// The longest the engine sleeps without re-reading the wall clock and the rows.
const RECHECK: Duration = Duration::from_secs(60);

/// Where the engine sends its plays (the main picture; tests record them).
pub trait PlayerPort: Send + Sync + 'static {
    fn send(&self, cmd: PlayerCommand);
}

impl PlayerPort for PlayerHandle {
    fn send(&self, cmd: PlayerCommand) {
        PlayerHandle::send(self, cmd);
    }
}

/// What the engine works with (`Services::start_integrations`).
pub struct EngineDeps {
    pub store: Store,
    pub config: SharedConfig,
    /// The main picture (tile A).
    pub player: Arc<dyn PlayerPort>,
    pub bus: EventBus,
    /// Lazy series details, needed when a watchlist reaches an unopened series.
    pub jobs: Option<crate::jobs::JobManager>,
}

type Msg = (QueueCommand, Option<oneshot::Sender<QueueReply>>);

/// The UI's / MPRIS's / the agent's way in (cheap to clone).
#[derive(Debug, Clone)]
pub struct QueueHandle {
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<AppShared>,
}

impl QueueHandle {
    /// Send a command (never blocks; dropped when the engine isn't running).
    pub fn send(&self, cmd: QueueCommand) {
        let _ = self.tx.send((cmd, None));
    }

    /// Send a command and wait for its outcome (MCP: what was added or refused, whether `play`
    /// plays or waits for the PIN).
    pub async fn request(&self, cmd: QueueCommand) -> QueueReply {
        let (tx, rx) = oneshot::channel();
        if self.tx.send((cmd, Some(tx))).is_err() {
            return QueueReply::Error("Up next isn't running".into());
        }
        rx.await.unwrap_or_else(|_| QueueReply::Error("Up next isn't running".into()))
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> Arc<crate::queue::QueueSnapshot> {
        self.shared.queue()
    }

    /// Changes of the snapshot.
    pub fn subscribe(&self) -> watch::Receiver<Arc<crate::queue::QueueSnapshot>> {
        self.shared.subscribe_queue()
    }
}

/// The batch base's placeholder (`Services::start_integrations` calls it while
/// `queue::ENABLED` is false): commands are drained and ignored; the snapshot stays empty. The
/// integrator replaces the call with [`spawn`].
pub fn start(rt: &tokio::runtime::Handle, shared: Arc<AppShared>) -> QueueHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    rt.spawn(async move { while rx.recv().await.is_some() {} });
    QueueHandle { tx, shared }
}

/// Start the engine: it loads the stored rows (a row left `current` stays current — Home's
/// "Resume Up next"; nothing plays) and follows the main picture (`AppShared::main_player`) and
/// the pictures (`AppShared::subscribe_tiles`).
pub fn spawn(rt: &tokio::runtime::Handle, shared: Arc<AppShared>, deps: EngineDeps) -> QueueHandle {
    let (tx, rx) = mpsc::unbounded_channel::<Msg>();
    let cfg = crate::config::current(&deps.config);
    let engine = Engine {
        machine: QueueMachine::new(MachineConfig::from(&cfg.queue)),
        shared: shared.clone(),
        deps,
        seen: None,
        last: Arc::default(),
        reply: None,
        sent: None,
        lists: HashMap::new(),
        reported: Default::default(),
    };
    rt.spawn(engine.run(rx));
    QueueHandle { tx, shared }
}

struct Engine {
    shared: Arc<AppShared>,
    deps: EngineDeps,
    machine: QueueMachine,
    /// The library generation and parental state the facts were computed with.
    seen: Option<(u64, ParentalView)>,
    /// The main picture's last snapshot.
    last: Arc<PlayerSnapshot>,
    /// The answer for the command being handled, when an effect decided it.
    reply: Option<QueueReply>,
    /// The row sent to the player while handling the current command.
    sent: Option<crate::model::QueueId>,
    /// Derived once per edit, library swap or end; never per frame or position tick.
    lists: HashMap<WatchlistId, crate::watchlist::model::ListView>,
    reported: std::collections::HashSet<WatchlistItemId>,
}

/// Wait for a change of an optional watch (pending forever without one).
async fn changed<T>(rx: &mut Option<watch::Receiver<T>>) -> bool {
    match rx {
        Some(r) => r.changed().await.is_ok(),
        None => std::future::pending::<bool>().await,
    }
}

fn now() -> Timestamp {
    crate::model::now()
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut snaps = self.shared.main_player();
        let mut tiles = Some(self.shared.subscribe_tiles());
        // the engine usually starts before the first library: its rows show once one is in
        let mut libs = Some(self.shared.subscribe_library());
        if let Some(s) = &mut snaps {
            self.last = s.borrow_and_update().clone();
        }
        self.reload().await;
        self.refresh_lists().await;
        self.input(Input::Player(self.last.clone())).await;
        self.input(Input::MultiView(self.shared.tiles().on)).await;
        self.publish();
        loop {
            let wait = match self.machine.next_deadline() {
                Some(at) => Duration::from_secs(at.saturating_sub(now()).max(0) as u64).min(RECHECK),
                None => RECHECK,
            };
            tokio::select! {
                msg = rx.recv() => {
                    let Some((cmd, reply)) = msg else { break };
                    self.refresh_facts().await;
                    let out = self.command(cmd).await;
                    self.refresh_lists().await;
                    self.publish(); // before the answer: the caller may read the snapshot next
                    if let Some(r) = reply {
                        let _ = r.send(out);
                    }
                }
                ok = changed(&mut snaps) => {
                    if !ok {
                        snaps = None;
                        continue;
                    }
                    let s = snaps.as_mut().map(|r| r.borrow_and_update().clone()).unwrap_or_default();
                    let ended = matches!(s.state, crate::player::PlayerState::Ended | crate::player::PlayerState::Error(_));
                    self.last = s.clone();
                    self.input(Input::Player(s)).await;
                    if ended { self.refresh_lists().await; }
                }
                ok = changed(&mut tiles) => {
                    if !ok {
                        tiles = None;
                        continue;
                    }
                    let on = tiles.as_mut().is_some_and(|t| t.borrow_and_update().on);
                    self.input(Input::MultiView(on)).await;
                }
                ok = changed(&mut libs) => {
                    if !ok {
                        libs = None;
                        continue;
                    }
                    if let Some(l) = libs.as_mut() {
                        l.borrow_and_update();
                    }
                    self.refresh_facts().await;
                    self.refresh_lists().await;
                }
                _ = tokio::time::sleep(wait) => {
                    if wait == RECHECK {
                        self.reload().await; // outside changes: retime, prune, a refresh's orphans
                    } else {
                        self.refresh_facts().await;
                    }
                    self.input(Input::Tick).await;
                    self.refresh_lists().await;
                }
            }
            self.publish();
        }
    }

    fn publish(&self) {
        let mut snap = self.machine.snapshot().clone();
        for list in self.lists.values() {
            for (position, entry) in list.entries.iter().enumerate() {
                snap.watchlist_chips.insert(
                    entry.entry.id,
                    crate::queue::WatchlistChip {
                        name: list.list.name.clone(),
                        any_order: list.list.play_order == crate::watchlist::PlayOrder::Any,
                        position: position + 1,
                        total: list.entries.len(),
                    },
                );
            }
        }
        snap.watchlist = snap.context.and_then(|id| self.lists.get(&id)).map(|list| {
            let current_entry = snap.current.and_then(|id| snap.row(id)).and_then(|r| r.entry.watchlist_item_id);
            let position =
                current_entry.or(list.next).and_then(|id| list.place(id)).map_or_else(|| list.entries.iter().filter(|e| e.done()).count(), |p| p.0);
            let then = list
                .entries
                .iter()
                .filter(|e| !e.done() && e.playable.is_some())
                .take(3)
                .map(|e| crate::queue::ListPreview {
                    id: e.entry.id,
                    kind: e.entry.kind,
                    title: e.entry.title.clone(),
                    series: e.series.clone(),
                    season: e.episode.map(|(s, _)| s),
                    episode: e.episode.map(|(_, e)| e),
                    locked: e.locked,
                })
                .collect();
            crate::queue::ListContext {
                id: list.list.id,
                name: list.list.name.clone(),
                any_order: list.list.play_order == crate::watchlist::model::PlayOrder::Any,
                position,
                total: list.entries.len(),
                playing_through: snap.driving || snap.between_items(),
                then,
            }
        });
        snap.finished_watchlist = match snap.finished {
            Some(crate::queue::FinishedWhy::EndOfList(id)) => self.lists.get(&id).map(|l| l.list.name.clone()),
            _ => None,
        };
        if *self.shared.queue() != snap {
            self.shared.publish_queue(Arc::new(snap));
        }
    }

    fn config(&self) -> Arc<Config> {
        crate::config::current(&self.deps.config)
    }

    /// Run `f` on the blocking pool with a reader, the published library and the parental state.
    async fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection, &Library, ParentalView, &Config) -> crate::error::Result<R> + Send + 'static,
    ) -> crate::error::Result<R> {
        let (store, shared, cfg) = (self.deps.store.clone(), self.shared.clone(), self.config());
        tokio::task::spawn_blocking(move || {
            let conn = store.reader()?;
            let lib = shared.library();
            let view = shared.parental(&cfg.parental);
            f(&conn, &lib, view, &cfg)
        })
        .await
        .map_err(crate::error::Error::other)?
    }

    /// Re-read the rows and their facts.
    async fn reload(&mut self) {
        self.load_rows().await;
        self.seen = None;
        self.refresh_facts().await;
    }

    async fn refresh_lists(&mut self) {
        let context = self.machine.context;
        let (excluded, ended, agent) = self.machine.preview_exclusions();
        let finished = match self.machine.snapshot().finished {
            Some(crate::queue::FinishedWhy::EndOfList(id)) => Some(id),
            _ => None,
        };
        let result = self
            .read(move |c, lib, view, cfg| {
                let mut ids: Vec<WatchlistId> = c
                    .prepare_cached("SELECT DISTINCT w.list_id FROM queue_item q JOIN watchlist_item w ON w.id=q.watchlist_item_id")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                ids.extend(context);
                ids.extend(finished);
                ids.sort_unstable();
                ids.dedup();
                let mut lists = HashMap::new();
                for id in ids {
                    if let Some(mut list) = crate::store::watchlist::view_for_run(
                        c,
                        lib,
                        id,
                        view.unlocked,
                        context == Some(id) && agent,
                        view.hides_explicit,
                        cfg.interface.copy_order,
                        if context == Some(id) { &ended } else { &[] },
                    )? {
                        if context == Some(id) {
                            for entry in &mut list.entries {
                                if excluded.contains(&entry.entry.id) {
                                    entry.playable = None;
                                }
                            }
                        }
                        lists.insert(id, list);
                    }
                }
                Ok(lists)
            })
            .await;
        match result {
            Ok(lists) => self.lists = lists,
            Err(e) => {
                self.lists.clear(); // names/counts fail closed
                tracing::warn!("queue: watchlist view: {e}");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_row(
        &mut self,
        list: WatchlistId,
        from: Option<WatchlistItemId>,
        by: AddedBy,
        mut excluded: Vec<WatchlistItemId>,
        ended: Vec<ItemRef>,
        queue_at: Option<Insert>,
        item: Option<ItemRef>,
    ) -> Vec<Input> {
        // The player submits its final progress before publishing Ended. Drain those writes
        // before deriving the next entry, including the same series entry's next episode.
        if let Err(e) = self.deps.store.writer().run(|_| Ok(())).await {
            return self.list_error(e, queue_at.is_none());
        }
        let mut from = from;
        loop {
            let item = item.clone();
            let excluded_pick = excluded.clone();
            let ended = ended.clone();
            let guard = self.shared.guard();
            let selection = self
                .read(move |c, lib, view, cfg| {
                    let Some(list_view) = crate::store::watchlist::view_for_run(
                        c,
                        lib,
                        list,
                        view.unlocked,
                        by == AddedBy::Agent,
                        view.hides_explicit,
                        cfg.interface.copy_order,
                        &ended,
                    )?
                    else {
                        if from.is_some() {
                            return Err(crate::error::Error::other("This watchlist entry is no longer available"));
                        }
                        return Ok((None, Vec::new()));
                    };
                    let mut passed = list_view.passed_over.clone();
                    for item in &mut passed {
                        if let Some(e) = list_view.entries.iter().find(|e| e.entry.id == item.id && e.entry.kind == ItemKind::Episode)
                            && let (Some(series), Some((season, episode))) = (&e.series, e.episode)
                        {
                            let norm = crate::spoiler::series_key(series);
                            let subject = crate::guard::Subject::Episode { series_norm: &norm, season: season as i32, episode: episode as i32 };
                            item.title = guard
                                .text(&subject, crate::guard::Field::EpisodeTitle, &item.title, crate::guard::Policy::Normal, now())
                                .text()
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{series} S{season}E{episode}"));
                        }
                    }
                    let selected = list_pick(&list_view, from, &excluded_pick, by);
                    let Some(entry) = selected else {
                        if let Some(id) = from {
                            let why = match list_view.entries.iter().find(|e| e.entry.id == id) {
                                Some(e) if e.locked => "This entry is locked: it needs your PIN in Tuner",
                                _ => "This entry is no longer in your playlists",
                            };
                            return Err(crate::error::Error::other(why));
                        }
                        return Ok((None, passed));
                    };
                    let item = match item {
                        Some(item) if !entry.entry.any_copy => {
                            // A page can show a combined row from another copy; the entry's
                            // explicit pin still selects the copy for these coordinates.
                            match entry.entry.item.as_ref() {
                                Some(pinned) if pinned.kind == ItemKind::Series && item.kind == ItemKind::Episode => {
                                    let ep = crate::store::vod::resolve_episode(c, &item)?.map(|id| crate::store::vod::episode(c, id)).transpose()?;
                                    let series = c
                                        .query_row(
                                            "SELECT id FROM series WHERE source_id=?1 AND key=?2",
                                            params![pinned.source, pinned.key.as_str()],
                                            |r| r.get::<_, SeriesId>(0),
                                        )
                                        .optional()?;
                                    match ep.zip(series) {
                                        Some((ep, series)) => crate::store::copies::same_episode(c, series, ep.season, ep.episode)?
                                            .map(|id| crate::store::vod::episode(c, id))
                                            .transpose()?
                                            .map(|ep| ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key }),
                                        None => None,
                                    }
                                }
                                Some(pinned) if pinned.kind == ItemKind::Movie => Some(pinned.clone()),
                                _ => Some(item),
                            }
                        }
                        Some(item) => Some(item),
                        None => entry.playable.clone(),
                    };
                    let Some(item) = item else { return Err(crate::error::Error::other("This episode is not available in the entry's pinned copy")) };
                    if item.kind == ItemKind::Series {
                        let id = c
                            .prepare_cached("SELECT id FROM series WHERE source_id=?1 AND key=?2")?
                            .query_row(params![item.source, item.key.as_str()], |r| r.get::<_, SeriesId>(0))
                            .optional()?;
                        return Ok((id.map(|id| ListPick::Load(entry.entry.id, id, entry.entry.title.clone())), passed));
                    }
                    let target = AddTarget { item, programme: None, from_start: entry.progress.is_none(), any_copy: entry.entry.any_copy };
                    let row = new_row(c, lib, view, target, by, cfg.interface.clock_24h, now())?;
                    match row {
                        Ok(row) => Ok((Some(ListPick::Row(entry.entry.id, row)), passed)),
                        Err(why) => Err(crate::error::Error::other(format!("This watchlist entry cannot play: {why:?}"))),
                    }
                })
                .await;
            let selection = match selection {
                Ok((pick, passed)) => {
                    for item in passed {
                        if queue_at.is_none() && self.reported.insert(item.id) {
                            self.deps.bus.toast(Toast::info(format!("Skipping {}: {}", item.title, item.why)));
                        }
                    }
                    pick
                }
                Err(e) => return self.list_error(e, queue_at.is_none()),
            };
            match selection {
                Some(ListPick::Load(entry, series, title)) => {
                    if let Some(jobs) = &self.deps.jobs {
                        jobs.submit(crate::jobs::JobKind::LoadSeriesDetails(series));
                    }
                    if queue_at.is_some() {
                        return self.list_error(crate::error::Error::other("Loading this series' episodes; try again shortly"), false);
                    }
                    if self.reported.insert(entry) {
                        self.deps.bus.toast(Toast::info(format!("Skipping {title}: episodes aren't loaded yet")));
                    }
                    self.machine.step(Input::ListPassed(entry), now());
                    excluded.push(entry);
                    from = None;
                }
                Some(ListPick::Row(entry, row)) => {
                    let item = row.item.clone();
                    let at = match queue_at {
                        Some(Insert::End) if self.machine.context.is_some() => self
                            .machine
                            .rows()
                            .iter()
                            .position(|r| r.state == crate::queue::RowState::Pending && r.list_context)
                            .map_or(Insert::End, Insert::Index),
                        Some(at) => at,
                        None => Insert::Next,
                    };
                    let write = self
                        .deps
                        .store
                        .writer()
                        .run(move |c| {
                            crate::store::sql::atomic(c, |c| {
                                let rows = crate::store::queue::load(c)?;
                                let existing = rows.iter().find(|r| {
                                    !r.state.is_done() && if queue_at.is_some() { r.item == row.item } else { r.watchlist_item_id == Some(entry) }
                                });
                                let id = if let Some(existing) = existing {
                                    if queue_at.is_some() {
                                        return Ok((existing.id, rows, false));
                                    }
                                    crate::store::queue::refresh_list_row(c, existing.id, &row)?;
                                    existing.id
                                } else {
                                    let added = crate::store::queue::add(c, &[row], at, now())?;
                                    let Some(id) = added.ids.first().copied() else { return Err(crate::error::Error::other("Up next is full")) };
                                    id
                                };
                                crate::store::queue::link_watchlist(c, id, entry, queue_at.is_none())?;
                                if queue_at.is_none() {
                                    crate::store::watchlist::played(c, list, now())?;
                                    crate::store::watchlist::consume_next(c, list, entry)?;
                                }
                                Ok((id, crate::store::queue::load(c)?, true))
                            })
                        })
                        .await;
                    let (id, rows, added) = match write {
                        Ok(v) => v,
                        Err(e) => return self.list_error(e, queue_at.is_none()),
                    };
                    self.deps.bus.data_changed(crate::msg::DataScope::Watchlists);
                    let copy = rows.clone();
                    let facts = self.read(move |c, lib, view, _| Ok(facts(c, lib, view, &copy))).await;
                    return match facts {
                        Ok(facts) => {
                            let mut inputs = vec![Input::Rows(rows), Input::Facts(facts)];
                            if let Some(at) = queue_at {
                                let (added, refused) =
                                    if added { (vec![(id, item)], Vec::new()) } else { (Vec::new(), vec![(item, Refusal::Duplicate)]) };
                                if let Some(text) = add_toast(&added, &refused, at, &self.machine.snapshot().rows) {
                                    self.deps.bus.toast(Toast::info(text));
                                }
                                self.reply = Some(QueueReply::Added { added, refused });
                            } else {
                                inputs.push(Input::ListNext { list, row: Some(id) });
                            }
                            inputs
                        }
                        Err(e) => self.list_error(e, queue_at.is_none()),
                    };
                }
                None if queue_at.is_some() => return self.list_error(crate::error::Error::other("This watchlist entry isn't available"), false),
                None => return vec![Input::ListNext { list, row: None }],
            }
        }
    }

    fn list_error(&mut self, e: crate::error::Error, playing: bool) -> Vec<Input> {
        tracing::warn!("queue: watchlist: {e}");
        self.deps.bus.toast(Toast::error(format!("Couldn't {} the watchlist entry: {e}", if playing { "play" } else { "add" })));
        self.reply = Some(QueueReply::Error(e.to_string()));
        if playing { vec![Input::Command(QueueCommand::StopList)] } else { Vec::new() }
    }

    /// Re-read the rows (a reader: an idle engine never waits on the writer's queue).
    async fn load_rows(&mut self) {
        match self.read(|c, _, _, _| Ok((crate::store::queue::load(c)?, list_context(c)?))).await {
            Ok((rows, context)) => {
                self.input(Input::Rows(rows)).await;
                self.input(Input::Context(context)).await;
            }
            Err(e) => tracing::warn!("queue: reading Up next: {e}"),
        }
    }

    /// New facts when the library or the parental state changed (or were never computed). A
    /// new library also re-reads the rows: a refresh may have re-keyed or orphaned some, and
    /// removing a playlist deletes its rows.
    async fn refresh_facts(&mut self) {
        let lib = self.shared.library();
        let key = (lib.generation, self.shared.parental(&self.config().parental));
        drop(lib);
        if self.seen == Some(key) {
            return;
        }
        if self.seen.is_some_and(|(generation, _)| generation != key.0) {
            self.load_rows().await;
        }
        let rows = self.machine.rows().to_vec();
        match self.read(move |c, lib, view, _| Ok(facts(c, lib, view, &rows))).await {
            Ok(f) => {
                self.seen = Some(key);
                self.input(Input::Facts(f)).await;
            }
            Err(e) => tracing::warn!("queue: row facts: {e}"),
        }
    }

    /// Step the machine with `first` and everything its effects feed back.
    async fn input(&mut self, first: Input) {
        let mut todo = VecDeque::from([first]);
        while let Some(i) = todo.pop_front() {
            self.machine.set_config(MachineConfig::from(&self.config().queue));
            for e in self.machine.step(i, now()) {
                todo.extend(self.effect(e).await);
            }
        }
    }

    async fn command(&mut self, cmd: QueueCommand) -> QueueReply {
        self.reply = None;
        self.sent = None;
        if cmd == QueueCommand::RefreshLists {
            self.reload().await;
        }
        let plays = matches!(
            cmd,
            QueueCommand::PlayAt(_)
                | QueueCommand::PlayList { .. }
                | QueueCommand::PlayListAgent { .. }
                | QueueCommand::PlayListFrom { .. }
                | QueueCommand::PlayAtFrom { .. }
                | QueueCommand::Next
                | QueueCommand::Prev
                | QueueCommand::Now
                | QueueCommand::Proceed { .. }
                | QueueCommand::Resume
        );
        self.input(Input::Command(cmd)).await;
        if let Some(r) = self.reply.take() {
            return r;
        }
        if !plays {
            return QueueReply::Done;
        }
        let s = self.machine.snapshot();
        QueueReply::Play(match (s.blocked, s.blocked_id, self.sent) {
            (Some(Blocked::NeedsPin), Some(id), _) => PlayReply::WaitingForPin(id),
            (Some(b), Some(id), _) => PlayReply::Waiting(id, b),
            (_, _, Some(id)) => PlayReply::Playing(id),
            _ => PlayReply::Nothing,
        })
    }

    async fn effect(&mut self, e: Effect) -> Vec<Input> {
        match e {
            Effect::LookupList { item, list, from, by, excluded, ended } => self.list_row(list, from, by, excluded, ended, None, item).await,
            Effect::AddListEntry { list, entry, at } => self.list_row(list, Some(entry), AddedBy::App, Vec::new(), Vec::new(), Some(at), None).await,
            Effect::Context(context) => {
                self.reported.clear();
                let r = self
                    .deps
                    .store
                    .writer()
                    .run(move |c| {
                        crate::store::sql::atomic(c, |c| {
                            c.execute("DELETE FROM meta WHERE key IN ('queue:context','queue:context_agent')", [])?;
                            if let Some((list, by)) = context {
                                c.execute("INSERT INTO meta(key,value) VALUES ('queue:context',?1)", [list.to_string()])?;
                                if by == AddedBy::Agent {
                                    c.execute("INSERT INTO meta(key,value) VALUES ('queue:context_agent','1')", [])?;
                                }
                            }
                            Ok(())
                        })
                    })
                    .await;
                if let Err(e) = r {
                    self.reply = Some(QueueReply::Error(e.to_string()));
                }
                Vec::new()
            }
            Effect::Play { id, how, resume } => vec![self.play(id, how, resume).await],
            Effect::PlayEpisode { id, origin } => {
                // the PIN window may have closed during the episode before: re-read it now
                let may = self.read(move |c, lib, view, _| episode_may_play(c, lib, view, id)).await.unwrap_or_else(|e| {
                    tracing::debug!("queue: next episode: {e}");
                    EpisodeCheck::Gone
                });
                match may {
                    EpisodeCheck::Plays => {
                        let req = PlayRequest { media: MediaRef::Episode(id), zap: None, origin, resume: true, immediate: true };
                        if self.routes(Kind::Vod, None) {
                            self.deps.player.send(PlayerCommand::Play(req));
                        }
                    }
                    EpisodeCheck::Locked => self.deps.bus.toast(Toast::info("Next episode is locked")),
                    EpisodeCheck::Gone => {}
                }
                Vec::new()
            }
            Effect::LookupNextEpisode(ep) => {
                let next = self.read(move |c, lib, _, _| next_episode(c, lib, ep)).await.unwrap_or_else(|e| {
                    tracing::debug!("queue: next episode: {e}");
                    None
                });
                vec![Input::NextEpisode(next)]
            }
            Effect::Add { targets, at, by } => self.add(targets, at, by).await,
            Effect::Write(w) => self.write(w).await,
            Effect::Toast(text) => {
                let action = (text == PAUSED_TOAST).then(|| ("Up next".to_owned(), crate::keymap::Action::GoQueue));
                self.deps.bus.toast(Toast { kind: ToastKind::Info, text: text.into(), action, also: None, ttl_ms: 5000 });
                Vec::new()
            }
            Effect::NotifyPin(id) => {
                // the card shows it in the window; a hidden window, or the pop-out (it never
                // takes the keyboard: an agent's Up next plays there, DESIGN §30.13), gets a
                // notification. Casting suspends no local video (there is none): there, a
                // window without the focus gets it (DESIGN §40 §3.8, §11.4)
                let casting_unseen = self.last.cast.is_some() && !self.shared.window().is_some_and(|w| w.focused);
                if (self.last.video_suspended || casting_unseen || self.shared.window().is_some_and(|w| w.popped_out))
                    && let Some(r) = self.machine.rows().iter().find(|r| r.id == id)
                {
                    let title = self.shared.guard().outside_title(&r.title, now()).into_owned();
                    tokio::task::spawn_blocking(move || crate::platform::notify::notify("Up next is waiting for your PIN", &title, false));
                }
                Vec::new()
            }
        }
    }

    /// B1: may Up next play `kind` on the main picture now?
    fn routes(&self, kind: Kind, live_channel: Option<ChannelId>) -> bool {
        let st = RouteState {
            tiles: self.shared.tiles(),
            main_kind: main_kind(&self.last),
            queue_running: true,
            follow_interrupt_vod: self.config().follow.interrupt_vod,
            casting: self.last.cast.is_some(),
        };
        let intent = PlayIntent { source: PlaySource::Queue, kind, live_channel };
        matches!(crate::playback::route(&intent, &st, &self.shared.guard(), now()), Route::Tile { tile: crate::model::TileId::A, .. })
    }

    async fn play(&mut self, id: crate::model::QueueId, how: How, resume: bool) -> Input {
        let Some(row) = self.machine.rows().iter().find(|r| r.id == id).cloned() else {
            return Input::Resolved { id, outcome: Resolved::Gone };
        };
        let (kind, source) = (row.item.kind, row.item.source);
        let r = self.read(move |c, lib, view, cfg| resolve(c, lib, view, cfg, &row, how, resume, now())).await;
        let outcome = match r {
            Ok(Plan::Play { cmd, items, kind: k, channel }) => {
                if self.routes(k, channel) {
                    let from = self.machine.snapshot().current;
                    tracing::info!("queue: #{} → #{id} ({}, source {source})", from.map_or("-".into(), |f| f.to_string()), kind.as_str());
                    self.deps.player.send(PlayerCommand::PlayCopy(*cmd));
                    self.sent = Some(id);
                    Resolved::Sent { items }
                } else {
                    Resolved::Waits
                }
            }
            Ok(Plan::Not(outcome)) => outcome,
            Err(e) => {
                tracing::warn!("queue: #{id}: {e}");
                Resolved::Failed(e.to_string())
            }
        };
        Input::Resolved { id, outcome }
    }

    /// Run a write, then re-read the list (a state change needs no re-read: the machine made it).
    /// Facts are per row and only a pinned copy changes a row's item; a move or a removal keeps
    /// every other row's facts.
    async fn write(&mut self, w: Write) -> Vec<Input> {
        let at = now();
        let reload = !matches!(w, Write::State(..));
        let counts = matches!(w, Write::Remove(_));
        let new_facts = matches!(w, Write::PinCopy(..));
        let r = self
            .deps
            .store
            .writer()
            .run(move |c| {
                let n = match &w {
                    Write::State(id, s) => crate::store::queue::set_state(c, *id, *s, at).map(|_| 0)?,
                    Write::Move { ids, to } => crate::store::queue::move_rows(c, ids, *to).map(|_| 0)?,
                    Write::Order(ids) => crate::store::queue::reorder(c, ids).map(|_| 0)?,
                    Write::Remove(ids) => crate::store::queue::remove(c, ids)?,
                    Write::FromStart(id, on) => crate::store::queue::set_from_start(c, *id, *on).map(|_| 0)?,
                    Write::PinCopy(id, item) => crate::store::queue::pin_copy(c, *id, item).map(|_| 0)?,
                };
                Ok((n, if reload { Some(crate::store::queue::load(c)?) } else { None }))
            })
            .await;
        match r {
            Ok((n, rows)) => {
                if counts {
                    self.reply = Some(QueueReply::Removed(n));
                }
                let mut out: Vec<Input> = rows.map(Input::Rows).into_iter().collect();
                if new_facts && let Some(Input::Rows(rows)) = out.first() {
                    let rows = rows.clone();
                    if let Ok(f) = self.read(move |c, lib, view, _| Ok(facts(c, lib, view, &rows))).await {
                        out.push(Input::Facts(f));
                    }
                }
                out
            }
            Err(e) => {
                tracing::warn!("queue: write: {e}");
                self.reply = Some(QueueReply::Error(e.to_string()));
                Vec::new()
            }
        }
    }

    async fn add(&mut self, targets: Vec<AddTarget>, at: Insert, by: AddedBy) -> Vec<Input> {
        let existing = self.machine.rows().to_vec();
        // A list's countdown may already have materialised its next row. A Ctrl+N made on
        // that card still joins the user's queue before the list (DESIGN §42 §3).
        let at = if at == Insert::End && self.machine.context.is_some() {
            existing.iter().position(|r| r.state == crate::queue::RowState::Pending && r.list_context).map_or(at, Insert::Index)
        } else {
            at
        };
        let h24 = self.config().interface.clock_24h;
        let prepared = self.read(move |c, lib, view, _| Ok(prepare(c, lib, view, &existing, targets, by, h24, now()))).await;
        let (rows, mut refused) = match prepared {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("queue: add: {e}");
                self.reply = Some(QueueReply::Error(e.to_string()));
                return Vec::new();
            }
        };
        let when = now();
        let rows2 = rows.clone();
        let r = self
            .deps
            .store
            .writer()
            .run(move |c| {
                let added = crate::store::queue::add(c, &rows2, at, when)?;
                Ok((added, crate::store::queue::load(c)?))
            })
            .await;
        let (added, list) = match r {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("queue: add: {e}");
                self.reply = Some(QueueReply::Error(e.to_string()));
                return Vec::new();
            }
        };
        refused.extend(rows[added.ids.len()..].iter().map(|r| (r.item.clone(), Refusal::Full)));
        let added: Vec<(crate::model::QueueId, ItemRef)> = added.ids.iter().copied().zip(rows.iter().map(|r| r.item.clone())).collect();
        tracing::info!("queue: added {} ({} refused, by {})", added.len(), refused.len(), if by == AddedBy::Agent { "agent" } else { "app" });
        if by == AddedBy::App
            && let Some(text) = add_toast(&added, &refused, at, &self.machine.snapshot().rows)
        {
            self.deps.bus.toast(Toast::info(text));
        }
        self.reply = Some(QueueReply::Added { added, refused });
        let rows = list.clone();
        let mut out = vec![Input::Rows(list)];
        if let Ok(f) = self.read(move |c, lib, view, _| Ok(facts(c, lib, view, &rows))).await {
            out.push(Input::Facts(f));
        }
        out
    }
}

enum ListPick {
    Load(WatchlistItemId, SeriesId, String),
    Row(WatchlistItemId, NewRow),
}

fn list_context(c: &Connection) -> crate::error::Result<Option<(WatchlistId, AddedBy)>> {
    let id: Option<WatchlistId> = c
        .prepare_cached("SELECT w.id FROM meta m JOIN watchlist w ON w.id=CAST(m.value AS INTEGER) WHERE m.key='queue:context'")?
        .query_row([], |r| r.get(0))
        .optional()?;
    let agent: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='queue:context_agent' AND value='1')", [], |r| r.get(0))?;
    Ok(id.map(|id| (id, if agent { AddedBy::Agent } else { AddedBy::App })))
}

fn list_pick<'a>(
    list: &'a crate::watchlist::model::ListView,
    from: Option<WatchlistItemId>,
    excluded: &[WatchlistItemId],
    by: AddedBy,
) -> Option<&'a crate::watchlist::model::EntryState> {
    if let Some(id) = from {
        return list.entries.iter().find(|e| e.entry.id == id && e.playable.is_some() && (by == AddedBy::App || !e.locked));
    }
    let eligible = |e: &&crate::watchlist::model::EntryState| {
        !e.done() && e.playable.is_some() && !excluded.contains(&e.entry.id) && (by == AddedBy::App || !e.locked)
    };
    let candidates: Vec<_> = list.entries.iter().filter(eligible).collect();
    // The view includes the user's explicit Make next choice. Honour it in either mode.
    if let Some(next) = list.next.and_then(|id| candidates.iter().find(|e| e.entry.id == id)) {
        return Some(*next);
    }
    if list.list.play_order == crate::watchlist::model::PlayOrder::Ordered {
        return candidates.first().copied();
    }
    if let Some(partial) = candidates
        .iter()
        .filter(|e| e.progress.as_ref().is_some_and(|p| p.position_s > 0.0))
        .max_by_key(|e| e.progress.as_ref().map(|p| p.updated_at))
    {
        return Some(*partial);
    }
    // When a failure excludes the view's choice, pick over the remaining candidates.
    if candidates.is_empty() {
        return None;
    }
    let mut bytes = [0u8; 8];
    use std::io::Read;
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        return candidates.first().copied();
    }
    candidates.get((u64::from_ne_bytes(bytes) % candidates.len() as u64) as usize).copied()
}

/// What the main picture plays now (B1's `main_kind`).
fn main_kind(s: &PlayerSnapshot) -> Option<Kind> {
    if matches!(s.state, crate::player::PlayerState::Idle | crate::player::PlayerState::Ended | crate::player::PlayerState::Error(_)) {
        return None;
    }
    Some(match s.playable.as_ref()?.kind {
        PlayableKind::Live => Kind::Live,
        PlayableKind::Catchup { .. } => Kind::Catchup,
        _ => Kind::Vod,
    })
}

/// The app's toast after an add from the UI. `visible`: the snapshot's rows (a duplicate's place
/// never counts rows it conceals).
fn add_toast(
    added: &[(crate::model::QueueId, ItemRef)],
    refused: &[(ItemRef, Refusal)],
    at: Insert,
    visible: &[crate::queue::QueueRow],
) -> Option<String> {
    if let [(item, why)] = refused
        && added.is_empty()
    {
        return Some(match why {
            Refusal::Duplicate => {
                let mut pending = visible.iter().filter(|r| !r.entry.state.is_done());
                match pending.position(|r| &r.entry.item == item) {
                    Some(i) => format!("Already in Up next ({})", ordinal(i + 1)),
                    None => "Already in Up next".into(),
                }
            }
            Refusal::NoGuide => "Live channels join Up next by programme; this one has no guide".into(),
            Refusal::Full => "Up next is full".into(),
            Refusal::NotFound | Refusal::Locked => "Couldn't add that to Up next".into(),
        });
    }
    let n = added.len();
    let text = match (n, at) {
        (0, _) => "Nothing added to Up next".into(),
        (1, Insert::Next) => "Plays next in Up next".into(),
        (1, _) => "Added to Up next".into(),
        (n, _) => format!("Added {n} to Up next"),
    };
    Some(match refused.len() {
        0 => text,
        k => format!("{text} · {k} not added"),
    })
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

// ---------------------------------------------------------------------------------------------
// Reads (blocking pool)

/// Explicit while hidden now: the snapshot's index, or (episodes, items newer than the
/// snapshot) the store while explicit content is hidden. Unreadable counts as hidden.
fn conceals(conn: &Connection, lib: &Library, view: ParentalView, item: &ItemRef) -> bool {
    lib.conceals(item) || (view.hides_explicit && crate::store::live::item_explicit(conn, item).unwrap_or(true))
}

/// A movie or series ref's index in the snapshot.
fn vod_index(conn: &Connection, lib: &Library, item: &ItemRef) -> crate::error::Result<Option<u32>> {
    let kind = match item.kind {
        ItemKind::Movie => crate::store::vod::resolve_movie(conn, item)?.map(VodKind::Movie),
        ItemKind::Series => conn
            .prepare_cached("SELECT id FROM series WHERE source_id = ?1 AND key = ?2")?
            .query_row(params![item.source, item.key.as_str()], |r| r.get::<_, i64>(0))
            .optional()?
            .map(|id| VodKind::Series(SeriesId(id))),
        _ => None,
    };
    Ok(kind.and_then(|k| lib.vod.index_of(k)))
}

/// Where "Resume" would start a film or episode row: the latest unfinished progress of the
/// title across its copies, as [`resolve`] resumes it (the plan's newest progress for a film;
/// for an episode the same episode's newest progress in any copy). `None` for a slot, a row
/// that won't play now (concealed, gone, locked while the session is locked: the engine says
/// why) or no progress. The Up next list's Resume prompt (Enter on a row).
pub fn resume_point(
    conn: &Connection,
    lib: &Library,
    view: ParentalView,
    cfg: &Config,
    row: &QueueEntry,
    now: Timestamp,
) -> crate::error::Result<Option<ResumePoint>> {
    if row.programme.is_some() || conceals(conn, lib, view, &row.item) {
        return Ok(None);
    }
    let order = cfg.interface.copy_order;
    let unfinished = |position_s: f64, finished: bool| !finished && position_s > 0.0;
    match row.item.kind {
        ItemKind::Movie => {
            let Some(idx) = vod_index(conn, lib, &row.item)? else { return Ok(None) };
            if title_locked(lib, true, idx, row.any_copy) && !view.unlocked {
                return Ok(None);
            }
            let Some(id) = crate::store::vod::resolve_movie(conn, &row.item)? else { return Ok(None) };
            let plan = crate::store::copies::plan(conn, &lib.vod, true, idx, view.unlocked, order, now)?;
            Ok(plan.resume().and_then(|c| c.progress.clone()).filter(|p| unfinished(p.position_s, p.finished)).map(|p| ResumePoint {
                media: MediaRef::Movie(id),
                position_s: p.position_s,
                duration_s: p.duration_s,
            }))
        }
        ItemKind::Episode => {
            let Some((ep, idx)) = episode_of(conn, lib, &row.item)? else { return Ok(None) };
            if title_locked(lib, false, idx, row.any_copy) && !view.unlocked {
                return Ok(None);
            }
            let plan = crate::store::copies::plan(conn, &lib.vod, false, idx, view.unlocked, order, now)?;
            let ids: Vec<SeriesId> = plan
                .copies
                .iter()
                .filter_map(|c| match lib.vod.series.get(c.item as usize)?.kind {
                    VodKind::Series(id) => Some(id),
                    VodKind::Movie(_) => None,
                })
                .collect();
            let p = if ids.len() > 1 {
                crate::store::copies::episode_progress(conn, &ids)?.remove(&(ep.season, ep.episode))
            } else {
                crate::store::user::progress(conn, &row.item)?
            };
            Ok(p.filter(|p| unfinished(p.position_s, p.finished)).map(|p| ResumePoint {
                media: MediaRef::Episode(ep.id),
                position_s: p.position_s,
                duration_s: p.duration_s,
            }))
        }
        _ => Ok(None),
    }
}

/// [`resume_point`]'s answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumePoint {
    /// The row's own film or episode.
    pub media: MediaRef,
    pub position_s: f64,
    pub duration_s: Option<f64>,
}

/// An episode ref → the episode and its series' index in the snapshot.
fn episode_of(conn: &Connection, lib: &Library, item: &ItemRef) -> crate::error::Result<Option<(crate::model::Episode, u32)>> {
    let Some(id) = crate::store::vod::resolve_episode(conn, item)? else { return Ok(None) };
    let ep = match crate::store::vod::episode(conn, id) {
        Ok(e) => e,
        Err(crate::error::Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(lib.vod.index_of(VodKind::Series(ep.series_id)).map(|idx| (ep, idx)))
}

/// Every copy of a title is locked (`any_copy`), or this copy is.
fn title_locked(lib: &Library, movies: bool, idx: u32, any_copy: bool) -> bool {
    let items = if movies { &lib.vod.movies } else { &lib.vod.series };
    if !any_copy {
        return items.get(idx as usize).is_some_and(|v| v.locked);
    }
    lib.vod.stacks(movies).copies(idx).iter().all(|c| items.get(*c as usize).is_some_and(|v| v.locked))
}

/// The facts of every row (≤ 200 rows, a few indexed reads each).
fn facts(conn: &Connection, lib: &Library, view: ParentalView, rows: &[QueueEntry]) -> HashMap<crate::model::QueueId, RowFacts> {
    rows.iter().map(|r| (r.id, row_facts(conn, lib, view, r).unwrap_or(RowFacts { concealed: true, ..Default::default() }))).collect()
}

fn row_facts(conn: &Connection, lib: &Library, view: ParentalView, r: &QueueEntry) -> crate::error::Result<RowFacts> {
    if conceals(conn, lib, view, &r.item) {
        return Ok(RowFacts { concealed: true, ..Default::default() });
    }
    let (locked, catchup_days) = match r.item.kind {
        ItemKind::Movie | ItemKind::Series => {
            let movies = r.item.kind == ItemKind::Movie;
            (vod_index(conn, lib, &r.item)?.map(|i| title_locked(lib, movies, i, r.any_copy)), 0)
        }
        ItemKind::Episode => (episode_of(conn, lib, &r.item)?.map(|(_, i)| title_locked(lib, false, i, r.any_copy)), 0),
        ItemKind::Channel => match lib.live.by_ref(&r.item).filter(|id| !lib.live.hidden.contains(id)).and_then(|id| lib.live.get(id)) {
            Some(c) => (Some(c.locked), if c.has_catchup { c.catchup_days } else { 0 }),
            None => (None, 0),
        },
    };
    Ok(RowFacts {
        concealed: false,
        needs_pin: locked.unwrap_or(false) && (!view.unlocked || (r.by_agent && r.watchlist_item_id.is_some())),
        unavailable: locked.is_none() || r.orphaned,
        catchup_days,
    })
}

/// The episode after `ep` in its series, with its "S02E06 · Title" label and its series' name
/// (the spoiler guard's subject); none when the series isn't in the library now.
fn next_episode(conn: &Connection, lib: &Library, ep: crate::model::EpisodeId) -> crate::error::Result<Option<NextEpisode>> {
    let e = crate::store::vod::episode(conn, ep)?;
    let Some(idx) = lib.vod.index_of(VodKind::Series(e.series_id)) else { return Ok(None) };
    let series = lib.vod.series[idx as usize].name.to_string();
    let eps = crate::store::vod::episodes(conn, e.series_id, None)?;
    Ok(eps.iter().position(|x| x.id == ep).and_then(|i| eps.get(i + 1)).map(|n| NextEpisode {
        id: n.id,
        label: format!("S{:02}E{:02} · {}", n.season, n.episode, crate::ui::widgets::rows::episode_title(n)),
        series,
        season: n.season,
        episode: n.episode,
    }))
}

/// Whether the next-episode countdown may play `id` now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpisodeCheck {
    Plays,
    /// Its series is locked and the session is locked (the PIN window closed meanwhile).
    Locked,
    /// Gone, or explicit while hidden: nothing plays, nothing is said.
    Gone,
}

fn episode_may_play(conn: &Connection, lib: &Library, view: ParentalView, id: crate::model::EpisodeId) -> crate::error::Result<EpisodeCheck> {
    let e = crate::store::vod::episode(conn, id)?;
    let item = ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key.clone() };
    if conceals(conn, lib, view, &item) {
        return Ok(EpisodeCheck::Gone);
    }
    let Some(idx) = lib.vod.index_of(VodKind::Series(e.series_id)) else { return Ok(EpisodeCheck::Gone) };
    Ok(if title_locked(lib, false, idx, false) && !view.unlocked { EpisodeCheck::Locked } else { EpisodeCheck::Plays })
}

/// What a row resolves to at play time.
enum Plan {
    Play { cmd: Box<CopyPlay>, items: Vec<ItemRef>, kind: Kind, channel: Option<ChannelId> },
    Not(Resolved),
}

/// Play-time resolution of a row: the copy plan (pin → latest progress → trouble last →
/// language → playlist order), the PIN, the catch-up window; one `CopyPlay` with its fallbacks.
#[allow(clippy::too_many_arguments)]
fn resolve(
    conn: &Connection,
    lib: &Library,
    view: ParentalView,
    cfg: &Config,
    row: &QueueEntry,
    how: How,
    resume: bool,
    now: Timestamp,
) -> crate::error::Result<Plan> {
    // A list started over MCP must never choose a locked fallback, even if the app
    // has a live PIN session (DESIGN §42 §6).
    let view = if row.by_agent && row.watchlist_item_id.is_some() { ParentalView { unlocked: false, ..view } } else { view };
    if conceals(conn, lib, view, &row.item) {
        return Ok(Plan::Not(Resolved::Concealed));
    }
    if row.by_agent && row.watchlist_item_id.is_some() && row_facts(conn, lib, view, row)?.needs_pin {
        return Ok(Plan::Not(Resolved::NeedsPin));
    }
    let order = cfg.interface.copy_order;
    match row.item.kind {
        ItemKind::Movie => {
            let Some(idx) = vod_index(conn, lib, &row.item)? else { return Ok(Plan::Not(Resolved::Gone)) };
            if title_locked(lib, true, idx, row.any_copy) && !view.unlocked {
                return Ok(Plan::Not(Resolved::NeedsPin));
            }
            let plan = crate::store::copies::plan(conn, &lib.vod, true, idx, view.unlocked, order, now)?;
            let pick = (!row.any_copy).then_some(idx);
            let Some(PlayerCommand::PlayCopy(cmd)) = crate::ui::copies::movie_command(&lib.vod, &plan, pick, resume, PlayOrigin::Queue) else {
                return Ok(Plan::Not(Resolved::NeedsPin));
            };
            Ok(Plan::Play { cmd: Box::new(cmd), items: plan.refs(), kind: Kind::Vod, channel: None })
        }
        ItemKind::Episode | ItemKind::Series => {
            let found = match row.item.kind {
                ItemKind::Episode => episode_of(conn, lib, &row.item)?,
                _ => None,
            };
            let Some((ep, idx)) = found else { return Ok(Plan::Not(Resolved::Gone)) };
            if title_locked(lib, false, idx, row.any_copy) && !view.unlocked {
                return Ok(Plan::Not(Resolved::NeedsPin));
            }
            let plan = crate::store::copies::plan(conn, &lib.vod, false, idx, view.unlocked, order, now)?;
            let series_of = |c: u32| match lib.vod.series.get(c as usize)?.kind {
                VodKind::Series(id) => Some(id),
                VodKind::Movie(_) => None,
            };
            // Use the first permitted copy which actually has this episode. Falling back
            // to the reference itself could bypass a locked copy excluded by the plan.
            let (series, ep) = if row.any_copy {
                let mut selected = None;
                for s in plan.order.iter().copied().filter_map(series_of) {
                    if let Some(other) = crate::store::copies::same_episode(conn, s, ep.season, ep.episode)? {
                        selected = Some((s, crate::store::vod::episode(conn, other)?));
                        break;
                    }
                }
                let Some(selected) = selected else { return Ok(Plan::Not(Resolved::Gone)) };
                selected
            } else {
                (ep.series_id, ep)
            };
            let PlayerCommand::PlayCopy(mut cmd) = crate::ui::copies::episode_command(conn, &lib.vod, &plan, series, &ep, resume, PlayOrigin::Queue)
            else {
                return Ok(Plan::Not(Resolved::Gone));
            };
            // another copy's progress of this episode is where this one starts
            let ids: Vec<SeriesId> = plan.copies.iter().filter_map(|c| series_of(c.item)).collect();
            let this = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
            if resume
                && ids.len() > 1
                && let Some(p) = crate::store::copies::episode_progress(conn, &ids)?.remove(&(ep.season, ep.episode))
                && !p.finished
                && p.position_s > 0.0
                && p.item != this
            {
                cmd.start_at = Some(p.position_s);
            }
            let mut items = vec![row.item.clone(), this];
            for s in &ids {
                if let Some(id) = crate::store::copies::same_episode(conn, *s, ep.season, ep.episode)? {
                    let e = crate::store::vod::episode(conn, id)?;
                    items.push(ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key });
                }
            }
            Ok(Plan::Play { cmd: Box::new(cmd), items, kind: Kind::Vod, channel: None })
        }
        ItemKind::Channel => {
            let Some(slot) = row.programme.as_ref() else { return Ok(Plan::Not(Resolved::Gone)) };
            let Some(ch) = lib.live.by_ref(&row.item).filter(|id| !lib.live.hidden.contains(id)).and_then(|id| lib.live.get(id)) else {
                return Ok(Plan::Not(Resolved::Gone));
            };
            slot_plan(conn, lib, view, row, ch, slot, how, now)
        }
    }
}

/// A live slot: the row's channel and (any copy) its copies in other playlists, ranked by B5;
/// for the archive only channels whose catch-up reaches the programme.
#[allow(clippy::too_many_arguments)]
fn slot_plan(
    conn: &Connection,
    lib: &Library,
    view: ParentalView,
    row: &QueueEntry,
    ch: &crate::model::ChannelSummary,
    slot: &Slot,
    how: How,
    now: Timestamp,
) -> crate::error::Result<Plan> {
    use crate::store::copies::{RankCand, RankCtx, rank_channels};
    let archive = how == How::Catchup;
    let reaches = |c: &crate::model::ChannelSummary| c.has_catchup && c.catchup_days > 0 && slot.start >= now - i64::from(c.catchup_days) * 86_400;
    let mut cands: Vec<&crate::model::ChannelSummary> = vec![ch];
    if row.any_copy {
        for copy in crate::store::copies::channel_copies(lib, conn, ch.id)? {
            if copy.explicit && view.hides_explicit {
                continue;
            }
            if let Some(c) = lib.live.get(copy.id) {
                cands.push(c);
            }
        }
    }
    cands.retain(|c| !archive || reaches(c));
    let mut trouble = std::collections::HashSet::new();
    let mut failed = conn.prepare_cached("SELECT 1 FROM stream_failure WHERE source_id = ?1 AND item_key = ?2 AND at >= ?3")?;
    for c in &cands {
        if failed.query_row(params![c.source_id, c.key.as_str(), now - crate::store::copies::FAILED_RECENTLY], |_| Ok(())).optional()?.is_some() {
            trouble.insert(c.id);
        }
    }
    let ctx = RankCtx {
        pinned: (!row.any_copy).then_some(ch.id),
        session_locked: !view.unlocked,
        trouble,
        source_order: lib.sources.iter().map(|s| s.id).collect(),
        ..Default::default()
    };
    let rank: Vec<RankCand> = cands.iter().map(|c| RankCand::from(*c)).collect();
    let by_id = |id: ChannelId| cands.iter().find(|c| c.id == id).copied();
    let order: Vec<&crate::model::ChannelSummary> =
        rank_channels(&rank, &ctx).into_iter().filter_map(|(id, _)| by_id(id)).filter(|c| view.unlocked || !c.locked).collect();
    let Some(first) = order.first().copied() else {
        return Ok(Plan::Not(if cands.is_empty() { Resolved::Gone } else { Resolved::NeedsPin }));
    };
    let media = |c: &crate::model::ChannelSummary| {
        if archive {
            let catchup_id = if c.id == ch.id { slot.catchup_id.clone() } else { None };
            MediaRef::Catchup { channel: c.id, start: slot.start, stop: slot.stop, catchup_id }
        } else {
            MediaRef::Channel(c.id)
        }
    };
    let fallback = order.get(1).copied();
    let elsewhere = fallback.filter(|f| f.source_id == first.source_id).and_then(|_| order.iter().find(|c| c.source_id != first.source_id).copied());
    let cmd = CopyPlay {
        req: PlayRequest { media: media(first), zap: None, origin: PlayOrigin::Queue, resume: false, immediate: true },
        fallback: fallback.map(media),
        elsewhere: elsewhere.map(media),
        start_at: None,
    };
    let items = cands.iter().map(|c| ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }).collect();
    Ok(Plan::Play { cmd: Box::new(cmd), items, kind: if archive { Kind::Catchup } else { Kind::Live }, channel: Some(first.id) })
}

/// The rows an add writes (display fields looked up now) and the targets it refuses.
#[allow(clippy::too_many_arguments)]
fn prepare(
    conn: &Connection,
    lib: &Library,
    view: ParentalView,
    existing: &[QueueEntry],
    targets: Vec<AddTarget>,
    by: AddedBy,
    h24: bool,
    now: Timestamp,
) -> (Vec<NewRow>, Vec<(ItemRef, Refusal)>) {
    let mut rows: Vec<NewRow> = Vec::new();
    let mut refused = Vec::new();
    for (i, t) in targets.into_iter().enumerate() {
        if i >= MAX_ADD {
            refused.push((t.item, Refusal::Full));
            continue;
        }
        let item = t.item.clone();
        match new_row(conn, lib, view, t, by, h24, now) {
            Ok(Ok(row)) => {
                let start = row.programme.as_ref().map(|s| s.start);
                let same = |it: &ItemRef, s: Option<Timestamp>| *it == row.item && s == start;
                let dup = existing.iter().any(|e| !e.state.is_done() && same(&e.item, e.programme.as_ref().map(|s| s.start)))
                    || rows.iter().any(|r| same(&r.item, r.programme.as_ref().map(|s| s.start)));
                if dup {
                    refused.push((item, Refusal::Duplicate));
                } else {
                    rows.push(row);
                }
            }
            Ok(Err(why)) => refused.push((item, why)),
            Err(e) => {
                tracing::warn!("queue: add: {e}");
                refused.push((item, Refusal::NotFound));
            }
        }
    }
    (rows, refused)
}

#[allow(clippy::too_many_arguments)]
fn new_row(
    conn: &Connection,
    lib: &Library,
    view: ParentalView,
    t: AddTarget,
    by: AddedBy,
    h24: bool,
    now: Timestamp,
) -> crate::error::Result<std::result::Result<NewRow, Refusal>> {
    if conceals(conn, lib, view, &t.item) {
        return Ok(Err(Refusal::NotFound));
    }
    // over MCP a locked item is never added; the app may queue it (the PIN at play time)
    let locked = |l: bool| by == AddedBy::Agent && l && !view.unlocked;
    let row = |item: ItemRef, title: String, subtitle: Option<String>, duration_s: Option<f64>, programme: Option<Slot>| NewRow {
        item,
        any_copy: t.any_copy,
        programme,
        title,
        subtitle,
        duration_s,
        from_start: t.from_start,
        by,
    };
    match t.item.kind {
        ItemKind::Movie => {
            let Some(idx) = vod_index(conn, lib, &t.item)? else { return Ok(Err(Refusal::NotFound)) };
            if locked(title_locked(lib, true, idx, t.any_copy)) {
                return Ok(Err(Refusal::Locked));
            }
            let v = &lib.vod.movies[idx as usize];
            let VodKind::Movie(id) = v.kind else { return Ok(Err(Refusal::NotFound)) };
            let duration: Option<f64> =
                conn.prepare_cached("SELECT duration_s FROM movie WHERE id = ?1")?.query_row([id], |r| r.get(0)).optional()?.flatten();
            let title = match v.year {
                Some(y) => format!("{} ({y})", v.name),
                None => v.name.to_string(),
            };
            Ok(Ok(row(t.item.clone(), title, None, duration.filter(|d| *d > 0.0), None)))
        }
        ItemKind::Series | ItemKind::Episode => {
            let (ep, idx) = if t.item.kind == ItemKind::Series {
                // a series joins as its next episode (the app's picker sends episodes)
                let Some(idx) = vod_index(conn, lib, &t.item)? else { return Ok(Err(Refusal::NotFound)) };
                let ids: Vec<SeriesId> = lib
                    .vod
                    .stacks(false)
                    .copies(idx)
                    .iter()
                    .filter_map(|c| match lib.vod.series.get(*c as usize)?.kind {
                        VodKind::Series(id) => Some(id),
                        VodKind::Movie(_) => None,
                    })
                    .collect();
                match crate::store::copies::next_episode(conn, &ids)? {
                    Some((s, ep)) => match lib.vod.index_of(VodKind::Series(s)) {
                        Some(i) => (ep, i),
                        None => return Ok(Err(Refusal::NotFound)),
                    },
                    None => return Ok(Err(Refusal::NotFound)),
                }
            } else {
                match episode_of(conn, lib, &t.item)? {
                    Some(x) => x,
                    None => return Ok(Err(Refusal::NotFound)),
                }
            };
            if locked(title_locked(lib, false, idx, t.any_copy)) {
                return Ok(Err(Refusal::Locked));
            }
            let series = lib.vod.series[idx as usize].name.to_string();
            let subtitle = format!("S{:02}E{:02} · {}", ep.season, ep.episode, crate::ui::widgets::rows::episode_title(&ep));
            let item = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
            Ok(Ok(row(item, series, Some(subtitle), ep.duration_s.map(f64::from), None)))
        }
        ItemKind::Channel => {
            let Some(slot) = t.programme.clone() else { return Ok(Err(Refusal::NoGuide)) };
            let Some(c) = lib.live.by_ref(&t.item).filter(|id| !lib.live.hidden.contains(id)).and_then(|id| lib.live.get(id)) else {
                return Ok(Err(Refusal::NotFound));
            };
            if locked(c.locked) {
                return Ok(Err(Refusal::Locked));
            }
            let Some(epg) = c.epg_channel_id else { return Ok(Err(Refusal::NoGuide)) };
            // the programme as shown on the channel (shifted); its stored times are guide times
            let at = slot.start - i64::from(c.tvg_shift_min) * 60;
            let Some(p) = crate::store::epg::window(conn, epg, at, at + 1)?.into_iter().find(|p| p.start == at).map(|p| p.shifted(c.tvg_shift_min))
            else {
                return Ok(Err(Refusal::NotFound));
            };
            if p.stop <= now && !(c.has_catchup && c.catchup_days > 0 && p.start >= now - i64::from(c.catchup_days) * 86_400) {
                return Ok(Err(Refusal::NotFound)); // ended and gone from the archive
            }
            let catchup_id = if p.flags.has_catchup_id { crate::store::epg::programme(conn, p.id)?.catchup_id } else { None };
            let subtitle = format!("{} · {}", c.name, crate::ui::widgets::time::day_time(p.start, h24));
            let programme = Some(Slot { start: p.start, stop: p.stop, catchup_id });
            Ok(Ok(row(t.item.clone(), p.title.to_string(), Some(subtitle), Some((p.stop - p.start) as f64), programme)))
        }
    }
}
