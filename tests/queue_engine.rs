//! Up next (DESIGN §32): `queue_item` storage (order, cap, pruning, reattach, backup, retime)
//! and the engine end to end — real store, real library snapshot, a recording player — from
//! an add to the advance after `Ended`, the PIN and MCP guardrails, and a restart that plays
//! nothing. The state machine's own rules are unit-tested in `queue::machine`. Owner: Q-core.

mod support;

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use support::seed;
use tokio::sync::watch;
use tuner::config::{Config, SharedConfig};
use tuner::model::{ItemKey, ItemKind, ItemRef, MediaRef, PlayOrigin, Playable, PlayableKind, QueueId, SourceId, Timestamp};
use tuner::player::{PlayerCommand, PlayerSnapshot, PlayerState};
use tuner::queue::{AddTarget, AddedBy, EngineDeps, Insert, PlayReply, PlayerPort, QueueCommand, QueueHandle, QueueReply, Refusal, RowState, Slot};
use tuner::shared::AppShared;
use tuner::store::Store;
use tuner::store::queue::{self as q, NewRow};

const NOW: Timestamp = 1_800_000_000;

fn store() -> Store {
    Store::open_temp("tuner-queue-test", None).unwrap()
}

fn w<R: Send + 'static>(s: &Store, f: impl FnOnce(&mut Connection) -> tuner::error::Result<R> + Send + 'static) -> R {
    s.writer().blocking(f).unwrap()
}

fn movie_ref(src: SourceId, i: usize) -> ItemRef {
    ItemRef { source: src, kind: ItemKind::Movie, key: ItemKey(format!("m3u:movie{i:010}")) }
}

fn row(item: ItemRef) -> NewRow {
    NewRow { item, any_copy: true, programme: None, title: "t".into(), subtitle: None, duration_s: Some(60.0), from_start: false, by: AddedBy::App }
}

/// (id, position, state) in list order.
fn listing(s: &Store) -> Vec<(QueueId, i64, String)> {
    let c = s.reader().unwrap();
    let mut st = c.prepare("SELECT id, position, state FROM queue_item ORDER BY position, id").unwrap();
    st.query_map([], |r| Ok((QueueId(r.get(0)?), r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
}

fn dense(s: &Store) -> bool {
    listing(s).iter().enumerate().all(|(i, (_, p, _))| *p == i as i64)
}

/// Adds, "play next", moves and removals keep positions dense; a new current row demotes the
/// old one and the unique index refuses a second.
#[test]
fn order_stays_dense_and_play_next_goes_before_the_first_pending_row() {
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 6));
    let ids = w(&s, move |c| q::add(c, &(0..4).map(|i| row(movie_ref(src, i))).collect::<Vec<_>>(), Insert::End, NOW)).ids;
    let (a, b) = (ids[0], ids[1]);
    w(&s, move |c| q::set_state(c, a, RowState::Played, NOW));
    w(&s, move |c| q::set_state(c, b, RowState::Current, NOW));
    let n = w(&s, move |c| q::add(c, &[row(movie_ref(src, 4))], Insert::Next, NOW)).ids[0];
    let order = |s: &Store| listing(s).into_iter().map(|(id, ..)| id).collect::<Vec<_>>();
    assert_eq!(order(&s), vec![a, b, n, ids[2], ids[3]], "before the first pending row");
    let last = ids[3];
    w(&s, move |c| q::move_rows(c, &[last], Insert::Index(0)));
    assert_eq!(order(&s)[0], last);
    w(&s, move |c| q::remove(c, &[b]));
    assert!(dense(&s));
    assert_eq!(order(&s).len(), 4);
    // "Shuffle the rest": the rows given swap places; the others stay put
    let before = order(&s);
    let (x, y) = (before[1], before[3]);
    w(&s, move |c| q::reorder(c, &[y, x, y]));
    assert_eq!(order(&s), vec![before[0], y, before[2], x]);
    assert!(dense(&s));

    w(&s, move |c| q::set_state(c, n, RowState::Current, NOW));
    let three = ids[2];
    w(&s, move |c| q::set_state(c, three, RowState::Current, NOW));
    let current: Vec<QueueId> = listing(&s).into_iter().filter(|r| r.2 == "current").map(|r| r.0).collect();
    assert_eq!(current, vec![three], "the old current row went back to pending");
    let two = s.writer().blocking(move |c| Ok(c.execute("UPDATE queue_item SET state = 'current' WHERE id = ?1", [n.0])?));
    assert!(two.is_err(), "at most one current row");
}

/// Past 200 rows the oldest played rows make room; with only unplayed rows left the rest is
/// refused. Pruning drops played rows after a week only.
#[test]
fn the_cap_evicts_old_played_rows_first_and_pruning_keeps_a_week() {
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 1));
    let all: Vec<QueueId> = (0..4).flat_map(|_| w(&s, move |c| q::add(c, &vec![row(movie_ref(src, 0)); q::MAX_ADD], Insert::End, NOW)).ids).collect();
    assert_eq!(all.len(), q::MAX_ROWS);
    // played at different times: the oldest goes first
    for (i, id) in all[..3].iter().copied().enumerate() {
        w(&s, move |c| q::set_state(c, id, RowState::Played, NOW - 10 * 86_400 + i as i64));
    }
    let added = w(&s, move |c| q::add(c, &vec![row(movie_ref(src, 0)); 2], Insert::End, NOW));
    assert_eq!((added.ids.len(), added.full), (2, 0));
    let left: Vec<QueueId> = listing(&s).into_iter().map(|r| r.0).collect();
    assert!(!left.contains(&all[0]) && !left.contains(&all[1]) && left.contains(&all[2]), "the two oldest played rows");
    let added = w(&s, move |c| q::add(c, &vec![row(movie_ref(src, 0)); 3], Insert::End, NOW));
    assert_eq!((added.ids.len(), added.full), (1, 2), "one played row left to make room");
    assert_eq!(listing(&s).len(), q::MAX_ROWS);
    assert!(dense(&s));

    let (old, recent) = (all[3], all[4]);
    w(&s, move |c| q::set_state(c, old, RowState::Played, NOW - 8 * 86_400));
    w(&s, move |c| q::set_state(c, recent, RowState::Skipped, NOW - 86_400));
    assert_eq!(w(&s, |c| q::prune(c, NOW)), 1);
    let left: Vec<QueueId> = listing(&s).into_iter().map(|r| r.0).collect();
    assert!(!left.contains(&old) && left.contains(&recent));
    assert!(dense(&s));
}

/// A refresh that re-keys a movie moves its row along (`store::user::HOLDERS`); a movie that
/// vanishes leaves an orphaned row, listed under Orphans and loaded as such.
#[test]
fn a_refresh_reattaches_rekeyed_rows_and_orphans_vanished_ones() {
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 3));
    w(&s, move |c| q::add(c, &[row(movie_ref(src, 0)), row(movie_ref(src, 1))], Insert::End, NOW));
    w(&s, move |c| {
        c.execute("UPDATE movie SET key = 'm3u:renamed' WHERE key = 'm3u:movie0000000000'", [])?;
        c.execute("DELETE FROM movie WHERE key = 'm3u:movie0000000001'", [])?;
        tuner::store::import::reattach_user_data(c, src, NOW)
    });
    let rows = q::load(&s.reader().unwrap()).unwrap();
    assert_eq!(rows[0].item.key, ItemKey("m3u:renamed".into()));
    assert!(!rows[0].orphaned);
    assert!(rows[1].orphaned);
    let orphans = tuner::store::user::orphans(&s.reader().unwrap()).unwrap();
    assert!(orphans.iter().any(|o| o.item == movie_ref(src, 1) && o.holders.contains(&"queue")));
}

/// The unplayed queue survives a backup round trip (merging twice adds nothing); a time-shift
/// change moves the pending slots of that channel only.
#[test]
fn backup_round_trip_and_retime() {
    let (a, b) = (store(), store());
    let src = w(&a, |c| seed::seed_source(c, "TV"));
    w(&a, move |c| seed::seed_movies(c, src, 2));
    w(&a, move |c| seed::seed_channels(c, src, 2));
    let ch = move |i: usize| ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey(format!("m3u:seed{i:011}")) };
    let slot = |s: Timestamp| Some(Slot { start: s, stop: s + 1800, catchup_id: Some("c1".into()) });
    let ids = w(&a, move |c| {
        q::add(
            c,
            &[
                row(movie_ref(src, 0)),
                NewRow { programme: slot(NOW), ..row(ch(0)) },
                NewRow { programme: slot(NOW + 1800), ..row(ch(1)) },
                row(movie_ref(src, 1)),
            ],
            Insert::End,
            NOW,
        )
    })
    .ids;
    let played = ids[3];
    w(&a, move |c| q::set_state(c, played, RowState::Played, NOW));

    let key = ch(0).key.0.clone();
    assert_eq!(w(&a, move |c| q::retime(c, src, &key, 3600)), 1);
    let rows = q::load(&a.reader().unwrap()).unwrap();
    assert_eq!(rows[1].programme.as_ref().map(|s| (s.start, s.stop)), Some((NOW + 3600, NOW + 5400)));
    assert_eq!(rows[2].programme.as_ref().map(|s| s.start), Some(NOW + 1800), "another channel");

    let v = q::export(&a.reader().unwrap()).unwrap().expect("something to keep");
    let src_b = w(&b, |c| seed::seed_source(c, "TV"));
    assert_eq!(src_b, src);
    let v2 = v.clone();
    assert_eq!(w(&b, move |c| q::import(c, &v2, &|r| Some(r.clone()))), 3, "unplayed rows only");
    assert_eq!(w(&b, move |c| q::import(c, &v, &|r| Some(r.clone()))), 0, "already there");
    let back = q::load(&b.reader().unwrap()).unwrap();
    let mine: Vec<_> = q::load(&a.reader().unwrap()).unwrap().into_iter().filter(|r| !r.state.is_done()).collect();
    let strip = |r: &tuner::queue::QueueEntry| (r.item.clone(), r.programme.clone(), r.title.clone(), r.any_copy, r.state);
    assert_eq!(back.iter().map(strip).collect::<Vec<_>>(), mine.iter().map(strip).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------------------------
// The engine

#[derive(Default)]
struct Recorder(Mutex<Vec<PlayerCommand>>);

impl PlayerPort for Recorder {
    fn send(&self, cmd: PlayerCommand) {
        self.0.lock().unwrap().push(cmd);
    }
}

impl Recorder {
    fn plays(&self) -> Vec<(MediaRef, PlayOrigin)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::PlayCopy(p) => Some((p.req.media.clone(), p.req.origin)),
                _ => None,
            })
            .collect()
    }
}

struct Rig {
    player: Arc<Recorder>,
    snaps: watch::Sender<Arc<PlayerSnapshot>>,
    queue: QueueHandle,
    events: crossbeam_channel::Receiver<tuner::msg::AppEvent>,
}

impl Rig {
    fn new(rt: &tokio::runtime::Runtime, store: Store, cfg: Config) -> Self {
        let shared = Arc::new(AppShared::default());
        let lib = tuner::store::library::load(&store.reader().unwrap()).unwrap();
        let _ = shared.publish_library(&Arc::new(lib));
        let snaps = watch::Sender::new(Arc::new(PlayerSnapshot::default()));
        shared.set_main_player(snaps.subscribe());
        let player = Arc::new(Recorder::default());
        let config: SharedConfig = Arc::new(RwLock::new(Arc::new(cfg)));
        let (bus, events) = tuner::msg::EventBus::new(None);
        let deps = EngineDeps { store, config, player: player.clone(), bus, jobs: None };
        Self { queue: tuner::queue::service::spawn(rt.handle(), shared, deps), player, snaps, events }
    }

    /// The main picture plays the film `item` (origin, state, load generation).
    fn player(&self, item: &ItemRef, media: MediaRef, origin: PlayOrigin, state: PlayerState, generation: u64) {
        self.player_kind(item, media, PlayableKind::Movie, origin, state, generation);
    }

    fn player_kind(&self, item: &ItemRef, media: MediaRef, kind: PlayableKind, origin: PlayOrigin, state: PlayerState, generation: u64) {
        let playable = Playable {
            media,
            kind,
            item: Some(item.clone()),
            source_id: Some(item.source),
            title: "t".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: tuner::redact::Secret::new("http://x".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        };
        self.snaps.send_replace(Arc::new(PlayerSnapshot {
            state,
            playable: Some(Arc::new(playable)),
            origin: Some(origin),
            load_generation: generation,
            ..Default::default()
        }));
    }
}

fn until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn target(item: ItemRef) -> AddTarget {
    AddTarget { item, programme: None, from_start: false, any_copy: true }
}

/// Up next end to end: adds (with the duplicate, no-guide and MCP lock refusals), a play
/// through the copy plan with origin Queue, the advance after `Ended`, a live slot, the PIN
/// card for a locked row — and a restart that restores the queue and plays nothing.
#[test]
fn the_engine_adds_plays_advances_and_never_plays_after_a_restart() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 4));
    let chans = w(&s, move |c| seed::seed_channels(c, src, 2));
    let now = tuner::model::now();
    let half = now - now % 1800;
    w(&s, move |c| seed::seed_epg(c, &chans, half - 3600, 3).map(|_| ()));
    // movie 3 is locked behind a PIN
    let locked = movie_ref(src, 3);
    let l = locked.clone();
    w(&s, move |c| tuner::store::copies::lock(c, &[l], true).map(|_| ()));
    let salt = tuner::ui::parental::new_salt();
    let mut cfg = Config::default();
    cfg.queue.countdown_s = 0;
    cfg.parental.enabled = true;
    cfg.parental.pin_hash = Some(tuner::ui::parental::hash_pin("1234", &salt));
    cfg.parental.pin_salt = Some(salt);
    let rig = Rig::new(&rt, s.clone(), cfg.clone());
    let req = |cmd: QueueCommand| rt.block_on(rig.queue.request(cmd));
    let add = |items: Vec<AddTarget>, by: AddedBy| req(QueueCommand::Add { targets: items, at: Insert::End, by });

    let QueueReply::Added { added, refused } =
        add(vec![target(movie_ref(src, 0)), target(movie_ref(src, 1)), target(movie_ref(src, 0))], AddedBy::App)
    else {
        panic!("added")
    };
    assert_eq!(added.len(), 2);
    assert_eq!(refused, vec![(movie_ref(src, 0), Refusal::Duplicate)]);
    let ch0 = ItemRef { source: src, kind: ItemKind::Channel, key: ItemKey("m3u:seed00000000000".into()) };
    let QueueReply::Added { refused, .. } = add(vec![target(ch0.clone()), target(locked.clone())], AddedBy::Agent) else { panic!() };
    assert_eq!(refused, vec![(ch0.clone(), Refusal::NoGuide), (locked.clone(), Refusal::Locked)], "MCP never adds a locked item");
    let QueueReply::Added { added: lock_row, .. } = add(vec![target(locked.clone())], AddedBy::App) else { panic!() };
    assert_eq!(lock_row.len(), 1, "the app may queue it: the PIN at play time");
    let slot = Slot { start: half, stop: half + 1800, catchup_id: None };
    let QueueReply::Added { added: slot_row, .. } =
        add(vec![AddTarget { item: ch0.clone(), programme: Some(slot), from_start: false, any_copy: false }], AddedBy::App)
    else {
        panic!()
    };
    let snap = rig.queue.snapshot();
    assert_eq!(snap.rows.len(), 4);
    let slot_entry = &snap.row(slot_row[0].0).unwrap().entry;
    assert_eq!(slot_entry.title, "Show 2 on 0", "the programme's title");
    assert!(snap.row(lock_row[0].0).unwrap().locked);

    // play the first row: its copy plan's command, origin Queue
    let (first, second) = (added[0].0, added[1].0);
    assert_eq!(req(QueueCommand::PlayAt(first)), QueueReply::Play(PlayReply::Playing(first)));
    let m0 = match &rig.player.plays()[..] {
        [(MediaRef::Movie(id), PlayOrigin::Queue)] => *id,
        other => panic!("{other:?}"),
    };
    rig.player(&movie_ref(src, 0), MediaRef::Movie(m0), PlayOrigin::Queue, PlayerState::Playing, 1);
    until("driving", || rig.queue.snapshot().driving);
    // EOF: played, and (countdown 0) the next row plays at once
    rig.player(&movie_ref(src, 0), MediaRef::Movie(m0), PlayOrigin::Queue, PlayerState::Ended, 1);
    until("the second play", || rig.player.plays().len() == 2);
    assert!(matches!(rig.player.plays()[1], (MediaRef::Movie(id), PlayOrigin::Queue) if id != m0));
    until("the row states", || {
        let s = rig.queue.snapshot();
        s.row(first).is_some_and(|r| r.entry.state == RowState::Played) && s.row(second).is_some_and(|r| r.entry.state == RowState::Current)
    });

    // the locked row waits for the PIN and nothing plays
    assert_eq!(req(QueueCommand::PlayAt(lock_row[0].0)), QueueReply::Play(PlayReply::WaitingForPin(lock_row[0].0)));
    assert_eq!(rig.player.plays().len(), 2);
    // the airing slot plays live
    assert_eq!(req(QueueCommand::PlayAt(slot_row[0].0)), QueueReply::Play(PlayReply::Playing(slot_row[0].0)));
    assert!(matches!(rig.player.plays().last(), Some((MediaRef::Channel(_), PlayOrigin::Queue))));
    drop(rig);

    // restart: the queue is back, the slot row still current, and nothing plays
    let again = Rig::new(&rt, s, cfg);
    until("the restored queue", || again.queue.snapshot().rows.len() == 4);
    let snap = again.queue.snapshot();
    assert_eq!(snap.current, Some(slot_row[0].0));
    assert!(!snap.driving);
    std::thread::sleep(Duration::from_millis(200));
    assert!(again.player.plays().is_empty(), "never auto-plays");
}

/// The app starts the engine before its first library: the stored rows show as soon as one
/// is published (not at the next minute's re-check, nor only after a command).
#[test]
fn stored_rows_show_once_the_first_library_is_published() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 3));
    w(&s, move |c| q::add(c, &[row(movie_ref(src, 0)), row(movie_ref(src, 1))], Insert::End, NOW));
    let shared = Arc::new(AppShared::default());
    let snaps = watch::Sender::new(Arc::new(PlayerSnapshot::default()));
    shared.set_main_player(snaps.subscribe());
    let config: SharedConfig = Arc::new(RwLock::new(Arc::new(Config::default())));
    let (bus, _events) = tuner::msg::EventBus::new(None);
    let deps = EngineDeps { store: s.clone(), config, player: Arc::new(Recorder::default()), bus, jobs: None };
    let queue = tuner::queue::service::spawn(rt.handle(), shared.clone(), deps);
    std::thread::sleep(Duration::from_millis(200));
    assert!(queue.snapshot().rows.is_empty(), "no library yet: nothing shows");
    let lib = tuner::store::library::load(&s.reader().unwrap()).unwrap();
    let _ = shared.publish_library(&Arc::new(lib));
    until("the rows", || queue.snapshot().rows.len() == 2);
}

/// An episode that ends outside Up next gets the engine's next-episode countdown (any
/// placement, a hidden window): the next episode plays. When the PIN window closed during the
/// episode and its series is locked, it doesn't play: "Next episode is locked".
#[test]
fn an_episode_ending_outside_up_next_plays_the_next_unless_it_is_locked_now() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_series_with_episodes(c, src, 1, 1, 3));
    let eps: Vec<tuner::model::EpisodeId> = w(&s, |c| {
        let mut st = c.prepare("SELECT id FROM episode ORDER BY season, episode")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    });
    let ep_ref = |e: u32| ItemRef { source: src, kind: ItemKind::Episode, key: ItemKey(format!("m3u:ep0-1-{e}")) };
    let ep_kind = |e: u32| PlayableKind::Episode { season: 1, episode: e };
    let mut cfg = Config::default();
    cfg.queue.countdown_s = 0;
    let episode_plays = |rig: &Rig| -> Vec<MediaRef> {
        rig.player
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::Play(r) => Some(r.media.clone()),
                _ => None,
            })
            .collect()
    };

    // the engine has started (its first look at the player never plays anything)
    let started = |rig: &Rig| assert_eq!(rt.block_on(rig.queue.request(QueueCommand::Cancel)), QueueReply::Done);
    let rig = Rig::new(&rt, s.clone(), cfg.clone());
    started(&rig);
    rig.player_kind(&ep_ref(1), MediaRef::Episode(eps[0]), ep_kind(1), PlayOrigin::Home, PlayerState::Playing, 1);
    rig.player_kind(&ep_ref(1), MediaRef::Episode(eps[0]), ep_kind(1), PlayOrigin::Home, PlayerState::Ended, 1);
    until("the next episode", || episode_plays(&rig) == vec![MediaRef::Episode(eps[1])]);
    drop(rig);

    // the series is locked and the session is (no PIN entered)
    let series = ItemRef { source: src, kind: ItemKind::Series, key: ItemKey("m3u:series000000000".into()) };
    w(&s, move |c| tuner::store::copies::lock(c, &[series], true).map(|_| ()));
    let salt = tuner::ui::parental::new_salt();
    cfg.parental.enabled = true;
    cfg.parental.pin_hash = Some(tuner::ui::parental::hash_pin("1234", &salt));
    cfg.parental.pin_salt = Some(salt);
    let rig = Rig::new(&rt, s, cfg);
    started(&rig);
    rig.player_kind(&ep_ref(2), MediaRef::Episode(eps[1]), ep_kind(2), PlayOrigin::Home, PlayerState::Playing, 1);
    rig.player_kind(&ep_ref(2), MediaRef::Episode(eps[1]), ep_kind(2), PlayOrigin::Home, PlayerState::Ended, 1);
    let t = Instant::now();
    let toast = loop {
        match rig.events.recv_timeout(Duration::from_millis(100)) {
            Ok(tuner::msg::AppEvent::Toast(t)) => break t.text,
            _ => assert!(t.elapsed() < Duration::from_secs(5), "no toast"),
        }
    };
    assert_eq!(toast, "Next episode is locked");
    assert!(episode_plays(&rig).is_empty());
}

/// The list is read again at EOF, after progress writes. Reordering and watches elsewhere
/// change the next pick; a Ctrl+N row takes precedence (DESIGN §42 §8.7–10).
#[test]
fn watchlist_reads_fresh_order_and_progress_after_the_users_queue() {
    use tuner::store::watchlist as wl;
    use tuner::watchlist::PlayOrder;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    let movies = w(&s, move |c| seed::seed_movies(c, src, 5));
    let (list, entries) = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, PlayOrder::Ordered, "app", NOW)?;
        let rows = (0..4).map(|i| wl::new_entry(c, &movie_ref(src, i))).collect::<tuner::error::Result<Vec<_>>>()?;
        let added = wl::add(c, &lib, list, &rows, tuner::watchlist::Insert::End, NOW)?;
        Ok((list, added.added))
    });
    let mut cfg = Config::default();
    cfg.queue.countdown_s = 0;
    let rig = Rig::new(&rt, s.clone(), cfg);
    let req = |cmd| rt.block_on(rig.queue.request(cmd));
    assert!(matches!(req(QueueCommand::PlayList { list, from: None, just_one: false }), QueueReply::Play(PlayReply::Playing(_))));
    assert_eq!(rig.queue.snapshot().rows.len(), 1);
    assert_eq!(rig.queue.snapshot().rows[0].entry.watchlist_item_id, Some(entries[0]));
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Playing, 1);
    until("list owns first film", || rig.queue.snapshot().driving);
    req(QueueCommand::Add { targets: vec![target(movie_ref(src, 4))], at: Insert::End, by: AddedBy::App });
    let moved = entries[3];
    w(&s, move |c| {
        wl::move_entries(c, list, &[moved], 1)?;
        tuner::store::user::mark_watched(c, &[(movie_ref(src, 0), "First".into()), (movie_ref(src, 1), "Elsewhere".into())], true, NOW)
    });
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Ended, 1);
    until("the queued short", || rig.player.plays().len() == 2);
    assert_eq!(rig.player.plays()[1].0, MediaRef::Movie(movies[4]));
    assert_eq!(rig.queue.snapshot().rows.len(), 2, "no list row fetched while the short waits");
    rig.player(&movie_ref(src, 4), MediaRef::Movie(movies[4]), PlayOrigin::Queue, PlayerState::Playing, 2);
    until("short owns playback", || rig.queue.snapshot().driving);
    rig.player(&movie_ref(src, 4), MediaRef::Movie(movies[4]), PlayOrigin::Queue, PlayerState::Ended, 2);
    until("reordered entry", || rig.player.plays().len() == 3);
    assert_eq!(rig.player.plays()[2].0, MediaRef::Movie(movies[3]));
    rig.player(&movie_ref(src, 3), MediaRef::Movie(movies[3]), PlayOrigin::Queue, PlayerState::Playing, 3);
    until("third film owns playback", || rig.queue.snapshot().driving);
    w(&s, move |c| tuner::store::user::mark_watched(c, &[(movie_ref(src, 3), "Third".into())], true, NOW));
    rig.player(&movie_ref(src, 3), MediaRef::Movie(movies[3]), PlayOrigin::Queue, PlayerState::Ended, 3);
    until("remaining entry", || rig.player.plays().len() == 4);
    assert_eq!(rig.player.plays()[3].0, MediaRef::Movie(movies[2]));
    assert!(!rig.player.plays().iter().any(|(m, _)| *m == MediaRef::Movie(movies[1])), "watched elsewhere is passed over");
    rig.player(&movie_ref(src, 2), MediaRef::Movie(movies[2]), PlayOrigin::Queue, PlayerState::Playing, 4);
    until("last film owns playback", || rig.queue.snapshot().driving);
    w(&s, move |c| tuner::store::user::mark_watched(c, &[(movie_ref(src, 2), "Last".into())], true, NOW));
    rig.player(&movie_ref(src, 2), MediaRef::Movie(movies[2]), PlayOrigin::Queue, PlayerState::Ended, 4);
    until("list finished", || rig.queue.snapshot().finished_watchlist.as_deref() == Some("Saga"));
    assert_eq!(rig.queue.snapshot().context, None);
    assert_eq!(rig.queue.snapshot().rows.len(), 4);
    assert_eq!(rig.queue.snapshot().watchlist_chips.len(), 4);
    assert!(s.reader().unwrap().query_row("SELECT count(*)=0 FROM meta WHERE key='queue:context'", [], |r| r.get::<_, bool>(0)).unwrap());
}

#[test]
fn watchlist_context_survives_restart_without_a_play_and_deletion_removes_chips() {
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_movies(c, src, 2));
    let list = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let rows = (0..2).map(|i| wl::new_entry(c, &movie_ref(src, i))).collect::<tuner::error::Result<Vec<_>>>()?;
        wl::add(c, &lib, list, &rows, tuner::watchlist::Insert::End, NOW)?;
        Ok(list)
    });
    let first = Rig::new(&rt, s.clone(), Config::default());
    rt.block_on(first.queue.request(QueueCommand::PlayList { list, from: None, just_one: false }));
    assert_eq!(first.player.plays().len(), 1);
    drop(first);
    let second = Rig::new(&rt, s.clone(), Config::default());
    rt.block_on(second.queue.request(QueueCommand::RefreshLists));
    assert_eq!(second.queue.snapshot().context, Some(list));
    assert!(second.player.plays().is_empty());
    assert_eq!(second.queue.snapshot().watchlist.as_ref().map(|l| l.name.as_str()), Some("Saga"));
    w(&s, move |c| wl::delete(c, list));
    rt.block_on(second.queue.request(QueueCommand::RefreshLists));
    let snap = second.queue.snapshot();
    assert_eq!(snap.context, None);
    assert_eq!(snap.rows.len(), 1, "the queue row outlives its list");
    assert_eq!(snap.rows[0].entry.watchlist_item_id, None);
    assert!(snap.watchlist_chips.is_empty());
}

/// Ctrl+N from a list keeps the range and pin, adds its chip, and never starts the list.
#[test]
fn watchlist_add_to_queue_resolves_the_entry_without_starting_its_context() {
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    w(&s, move |c| seed::seed_series_with_episodes(c, src, 1, 3, 2));
    let (list, entry) = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Second season", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let series = ItemRef { source: src, kind: ItemKind::Series, key: ItemKey("m3u:series000000000".into()) };
        let mut row = wl::new_entry(c, &series)?;
        row.season_from = Some(2);
        row.season_to = Some(2);
        let entry = wl::add(c, &lib, list, &[row], tuner::watchlist::Insert::End, NOW)?.added[0];
        wl::pin(c, entry, &series)?;
        Ok((list, entry))
    });
    let rig = Rig::new(&rt, s, Config::default());
    let add = || rt.block_on(rig.queue.request(QueueCommand::AddListEntry { list, entry, at: Insert::End }));
    assert!(matches!(add(), QueueReply::Added { added, refused } if added.len()==1 && refused.is_empty()));
    let snap = rig.queue.snapshot();
    assert_eq!(snap.rows.len(), 1);
    let row = &snap.rows[0].entry;
    assert_eq!(row.item.key.as_str(), "m3u:ep0-2-1");
    assert_eq!(row.watchlist_item_id, Some(entry));
    assert!(!row.any_copy);
    assert!(snap.watchlist_chips.contains_key(&entry));
    assert_eq!(snap.context, None);
    assert!(rig.player.plays().is_empty());
    assert!(matches!(add(), QueueReply::Added { added, refused } if added.is_empty() && refused[0].1==Refusal::Duplicate));
    assert_eq!(rig.queue.snapshot().rows.len(), 1);
}
#[test]
fn watchlist_interrupted_rows_are_reused_or_retired_and_pending_feeds_are_cancelled() {
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    let movies = w(&s, move |c| seed::seed_movies(c, src, 3));
    let (list, entries) = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let rows = (0..3).map(|i| wl::new_entry(c, &movie_ref(src, i))).collect::<tuner::error::Result<Vec<_>>>()?;
        Ok((list, wl::add(c, &lib, list, &rows, tuner::watchlist::Insert::End, NOW)?.added))
    });
    let mut cfg = Config::default();
    cfg.queue.ask = true;
    let rig = Rig::new(&rt, s.clone(), cfg);
    let req = |cmd| rt.block_on(rig.queue.request(cmd));
    req(QueueCommand::PlayList { list, from: None, just_one: false });
    let first = rig.queue.snapshot().current.unwrap();
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Playing, 1);
    until("playing", || rig.queue.snapshot().driving);
    rig.snaps.send_replace(Arc::new(PlayerSnapshot::default()));
    until("stopped", || rig.queue.snapshot().interrupted);
    w(&s, move |c| {
        c.execute("INSERT INTO watch_progress(source_id,kind,item_key,position_s,duration_s,finished,updated_at,title) VALUES (?1,'movie',?2,100,600,0,?3,'First')", rusqlite::params![src, movie_ref(src, 0).key.as_str(), NOW])?;
        Ok(())
    });
    req(QueueCommand::Next);
    assert_eq!(rig.queue.snapshot().current, Some(first));
    assert!(!rig.queue.snapshot().row(first).unwrap().entry.from_start, "reused row resumes the new progress");
    assert_eq!(q::load(&s.reader().unwrap()).unwrap().len(), 1, "Next reuses the stopped row");
    req(QueueCommand::PlayListAgent { list, from: Some(entries[1]), just_one: false });
    assert_eq!(rig.queue.snapshot().row(first).unwrap().entry.state, RowState::Skipped);
    assert_eq!(rig.queue.snapshot().pending(), 1, "old Current cannot become Pending again");
    rig.player(&movie_ref(src, 1), MediaRef::Movie(movies[1]), PlayOrigin::Queue, PlayerState::Playing, 2);
    until("second playing", || rig.queue.snapshot().driving);
    rig.player(&movie_ref(src, 1), MediaRef::Movie(movies[1]), PlayOrigin::Queue, PlayerState::Ended, 2);
    until("list countdown", || rig.queue.snapshot().countdown.is_some());
    let pending = rig.queue.snapshot().countdown.as_ref().unwrap().id.unwrap();
    req(QueueCommand::PlayList { list, from: Some(entries[0]), just_one: false });
    assert_eq!(rig.queue.snapshot().current, Some(pending), "starting the same pending entry reuses its row");
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Playing, 3);
    until("first replay playing", || rig.queue.snapshot().driving);
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Ended, 3);
    until("new list countdown", || rig.queue.snapshot().countdown.is_some());
    let pending = rig.queue.snapshot().countdown.as_ref().unwrap().id.unwrap();
    req(QueueCommand::AddListEntry { list, entry: entries[2], at: Insert::End });
    let manual = rig.queue.snapshot().rows.iter().find(|r| r.entry.watchlist_item_id == Some(entries[2]) && !r.entry.list_context).unwrap().entry.id;
    req(QueueCommand::StopList);
    let snap = rig.queue.snapshot();
    assert_eq!(snap.context, None);
    assert_ne!(snap.countdown.as_ref().and_then(|c| c.id), Some(pending));
    assert_eq!(snap.row(pending).unwrap().entry.state, RowState::Skipped);
    assert_eq!(snap.row(manual).unwrap().entry.state, RowState::Pending);
}

#[test]
fn watchlist_eof_without_watched_progress_moves_on_and_reports_a_placeholder_once() {
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    let movies = w(&s, move |c| seed::seed_movies(c, src, 2));
    let list = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let mut placeholder = wl::new_entry(c, &movie_ref(src, 0))?;
        placeholder.item = None;
        placeholder.title = "Missing title".into();
        placeholder.year = Some(1900);
        let rows = [placeholder, wl::new_entry(c, &movie_ref(src, 0))?, wl::new_entry(c, &movie_ref(src, 1))?];
        wl::add(c, &lib, list, &rows, tuner::watchlist::Insert::End, NOW)?;
        Ok(list)
    });
    let mut cfg = Config::default();
    cfg.queue.countdown_s = 0;
    let rig = Rig::new(&rt, s.clone(), cfg);
    rt.block_on(rig.queue.request(QueueCommand::PlayList { list, from: None, just_one: false }));
    for (i, movie) in movies.iter().enumerate() {
        rig.player(&movie_ref(src, i), MediaRef::Movie(*movie), PlayOrigin::Queue, PlayerState::Playing, i as u64 + 1);
        until("playing", || rig.queue.snapshot().driving);
        rig.player(&movie_ref(src, i), MediaRef::Movie(*movie), PlayOrigin::Queue, PlayerState::Ended, i as u64 + 1);
        if i == 0 {
            until("second film", || rig.player.plays().len() == 2);
        }
    }
    until("list ended", || rig.queue.snapshot().context.is_none());
    assert_eq!(rig.player.plays().len(), 2);
    assert!(tuner::store::user::progress(&s.reader().unwrap(), &movie_ref(src, 0)).unwrap().is_none());
    let skipped = rig.events.try_iter().filter(|e| matches!(e, tuner::msg::AppEvent::Toast(t) if t.text.contains("Skipping Missing title"))).count();
    assert_eq!(skipped, 1);
}

#[test]
fn watchlist_unloaded_series_passes_to_the_next_film_and_missing_selection_reports_error() {
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    let movies = w(&s, move |c| seed::seed_movies(c, src, 1));
    let (list, series) = w(&s, move |c| {
        c.execute("UPDATE source SET kind='xtream',xt_server='http://localhost',xt_username='test',xt_password='test' WHERE id=?1", [src])?;
        c.execute("INSERT INTO series(source_id,key,name,clean_name,norm_name,provider_order,seen_gen) VALUES (?1,'show','Unopened show','Unopened show','unopenedshow',0,1)", [src])?;
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let show = ItemRef { source: src, kind: ItemKind::Series, key: ItemKey("show".into()) };
        let ids =
            wl::add(c, &lib, list, &[wl::new_entry(c, &show)?, wl::new_entry(c, &movie_ref(src, 0))?], tuner::watchlist::Insert::End, NOW)?.added;
        Ok((list, ids[0]))
    });
    let rig = Rig::new(&rt, s.clone(), Config::default());
    let reply = rt.block_on(rig.queue.request(QueueCommand::PlayList { list, from: None, just_one: false }));
    assert!(matches!(reply, QueueReply::Play(PlayReply::Playing(_))), "{reply:?}");
    assert_eq!(rig.player.plays()[0].0, MediaRef::Movie(movies[0]));
    assert!(rig.events.try_iter().any(|e| matches!(e, tuner::msg::AppEvent::Toast(t) if t.text.contains("Skipping Unopened show"))));
    w(&s, move |c| wl::remove(c, list, &[series]).map(|_| ()));
    let reply = rt.block_on(rig.queue.request(QueueCommand::PlayList { list, from: Some(series), just_one: false }));
    assert!(matches!(reply, QueueReply::Error(ref message) if message.contains("no longer")), "{reply:?}");
    assert!(rig.queue.snapshot().finished.is_none());
}

#[test]
fn watchlist_timed_sleep_keeps_context_but_end_of_queue_ends_it() {
    use tuner::reminders::sleep::{SleepAction, SleepService, SleepWhen};
    use tuner::store::watchlist as wl;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let s = store();
    let src = w(&s, |c| seed::seed_source(c, "TV"));
    let movies = w(&s, move |c| seed::seed_movies(c, src, 2));
    let list = w(&s, move |c| {
        let lib = tuner::store::library::load(c)?;
        let list = wl::create(c, "Saga", None, tuner::watchlist::PlayOrder::Ordered, "app", NOW)?;
        let rows = (0..2).map(|i| wl::new_entry(c, &movie_ref(src, i))).collect::<tuner::error::Result<Vec<_>>>()?;
        wl::add(c, &lib, list, &rows, tuner::watchlist::Insert::End, NOW)?;
        Ok(list)
    });
    let mut cfg = Config::default();
    cfg.queue.ask = true;
    let rig = Rig::new(&rt, s, cfg);
    let req = |cmd| rt.block_on(rig.queue.request(cmd));
    req(QueueCommand::PlayList { list, from: None, just_one: false });
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Playing, 1);
    until("first playing", || rig.queue.snapshot().driving);
    rig.player(&movie_ref(src, 0), MediaRef::Movie(movies[0]), PlayOrigin::Queue, PlayerState::Ended, 1);
    until("countdown", || rig.queue.snapshot().countdown.is_some());
    let pending = rig.queue.snapshot().countdown.as_ref().unwrap().id.unwrap();
    let (bus, _events) = tuner::msg::EventBus::new(None);
    let fired = Arc::new(Mutex::new(0));
    let count = fired.clone();
    let snaps = rig.snaps.clone();
    let sleep = SleepService::start(
        rt.handle(),
        rig.snaps.subscribe(),
        rig.queue.subscribe(),
        Arc::new(move |_| {
            snaps.send_replace(Arc::new(PlayerSnapshot::default()));
            *count.lock().unwrap() += 1;
        }),
        bus,
    );
    sleep.connect_queue(Some(rig.queue.clone()));
    sleep.set(SleepWhen::At(tuner::model::now() + 1), SleepAction::Stop, 0, None).unwrap();
    until("timed stop", || *fired.lock().unwrap() == 1);
    until("stopped countdown", || rig.queue.snapshot().countdown.is_none());
    req(QueueCommand::EndAfterQueue(false));
    assert_eq!(rig.queue.snapshot().context, Some(list));
    assert_eq!(rig.queue.snapshot().row(pending).unwrap().entry.state, RowState::Pending);

    req(QueueCommand::Next);
    rig.player(&movie_ref(src, 1), MediaRef::Movie(movies[1]), PlayOrigin::Queue, PlayerState::Playing, 2);
    until("next playing", || rig.queue.snapshot().driving);
    sleep.set(SleepWhen::EndOfQueue, SleepAction::Stop, 0, None).unwrap();
    req(QueueCommand::EndAfterQueue(true)); // wait until the engine knows to stop feeding
    assert_eq!(rig.queue.snapshot().context, Some(list), "arming alone keeps the current context");
    rig.player(&movie_ref(src, 1), MediaRef::Movie(movies[1]), PlayOrigin::Queue, PlayerState::Ended, 2);
    until("end-of-queue stop", || *fired.lock().unwrap() == 2);
    until("end-of-queue clears context", || rig.queue.snapshot().context.is_none());
}
