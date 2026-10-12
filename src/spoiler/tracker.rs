//! Marks guarded events seen while they play, polling every 30 s so it works while the window
//! is hidden: live ≥ 50 % or present during the last 10 minutes, catch-up ≥ 90 %. Only guarded
//! subjects are ever recorded. Also runs the snapshot builder service. OWNER: S1.
//!
//! [`start`] installs a live [`Guard`] in `AppShared` (fail-closed until the first snapshot,
//! well under a second after the first library) and spawns:
//! * the **builder**: a new snapshot (blocking pool) on `Guard::rebuild`, a settings change
//!   (`Guard::configure`), a new library (not a zap's copy), an hourly tick (extends the 36 h
//!   lookahead) — debounced 1 s;
//! * the **tracker**: every 30 s, each picture's snapshot (`AppShared::tile_player`, A and B)
//!   → what's on → [`Tracker::observe`] → a `spoiler_seen` row through the store writer
//!   (`DataScope::Spoiler`), `Guard::note_seen` for the UI's Undo, a rebuild and the toast
//!   "… marked seen — spoiler guard lifted · Undo…". An episode played to 90 % rebuilds on the
//!   next poll (its saved progress moves the series boundary on).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::guard::{Guard, Subject};
use crate::model::{ChannelId, MediaRef, PlayableKind, SeenHow, SeenRow, Timestamp};
use crate::player::{PlayerSnapshot, PlayerState};
use crate::shared::AppShared;
use crate::store::Store;
use crate::store::library::{Library, LiveIndex};

/// How often the tracker looks at the pictures.
pub const POLL: Duration = Duration::from_secs(30);
/// The builder's debounce.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// How often the builder checks for a new library.
const LIBRARY_POLL: Duration = Duration::from_secs(5);
/// Live: present this close to the end counts as seen.
pub const LAST_MINUTES_S: i64 = 10 * 60;

/// What is playing, resolved to its guide programme.
#[derive(Debug, Clone, PartialEq)]
pub struct Watching {
    pub event_key: String,
    /// EPG time of this airing.
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub categories: Vec<String>,
    pub season_episode: Option<(i32, i32)>,
    /// The guide marks it a first showing.
    pub is_new: bool,
    /// Live: `None`; catch-up: how far through (0–1).
    pub fraction: Option<f64>,
    /// The channel's shift (s): live airings compare displayed time with `now`.
    pub shift_s: i64,
}

impl Watching {
    pub fn subject(&self) -> Subject<'_> {
        Subject::Programme {
            title: &self.title,
            sub_title: self.sub_title.as_deref(),
            categories: &self.categories,
            season_episode: self.season_episode,
            start: self.start + self.shift_s,
            is_new: self.is_new,
        }
    }
}

/// The seen decision, pure (a fake clock drives it in tests).
#[derive(Debug, Default)]
pub struct Tracker {
    /// Live seconds watched per airing (event key, EPG start).
    live: HashMap<(String, Timestamp), i64>,
    /// When each airing was last observed (the next observation adds the gap, capped).
    last: HashMap<(String, Timestamp), Timestamp>,
    reported: HashSet<(String, Timestamp)>,
}

impl Tracker {
    /// One observation at `now`. `tracked`: the guard records this subject
    /// (`spoiler::verdict::tracks`); untracked subjects never produce a row. Returns the row to
    /// write once, when it counts as seen.
    pub fn observe(&mut self, w: &Watching, tracked: bool, now: Timestamp) -> Option<SeenRow> {
        if !tracked {
            return None;
        }
        let key = (w.event_key.clone(), w.start);
        if self.reported.contains(&key) {
            return None;
        }
        let (how, seen) = match w.fraction {
            Some(f) => (SeenHow::Catchup, f >= 0.9),
            None => {
                let (start, stop) = (w.start + w.shift_s, w.stop + w.shift_s);
                let gap = self.last.insert(key.clone(), now).map_or(0, |t| (now - t).clamp(0, 2 * POLL.as_secs() as i64));
                let secs = self.live.entry(key.clone()).or_default();
                *secs += gap;
                let half = *secs * 2 >= (stop - start).max(1);
                let last_minutes = now >= stop - LAST_MINUTES_S && now < stop;
                (SeenHow::Live, half || last_minutes)
            }
        };
        if !seen {
            return None;
        }
        self.live.remove(&key);
        self.last.remove(&key);
        self.reported.insert(key);
        let series_norm = w.season_episode.map(|_| crate::spoiler::series_key(&w.title));
        Some(SeenRow {
            event_key: w.event_key.clone(),
            first_start: w.start,
            title: w.title.clone(),
            series_norm,
            season: w.season_episode.map(|s| s.0),
            episode: w.season_episode.map(|s| s.1),
            seen_at: now,
            how,
        })
    }
}

/// Start the guard (`Services::start_integrations`, when `spoiler::ENABLED`): installs a live
/// guard in `shared` and runs the builder and the tracker on `rt`.
pub fn start(rt: &tokio::runtime::Handle, shared: Arc<AppShared>, store: Store) {
    let guard = Guard::live();
    shared.set_guard(guard.clone());
    rt.spawn(builder(guard.clone(), shared.clone(), store.clone()));
    rt.spawn(track(guard, shared, store));
}

/// What identifies a library build (a zap's copy shares both indexes).
fn library_identity(lib: &Library) -> (Weak<LiveIndex>, crate::store::library::VodIdentity) {
    (Arc::downgrade(&lib.live), lib.vod_identity())
}

async fn builder(guard: Guard, shared: Arc<AppShared>, store: Store) {
    let mut built: Option<(Weak<LiveIndex>, crate::store::library::VodIdentity)> = None;
    let mut built_at: Timestamp = 0;
    let mut pending = true;
    loop {
        tokio::select! {
            _ = guard.rebuild_requested() => pending = true,
            _ = tokio::time::sleep(LIBRARY_POLL) => {}
        }
        let lib = shared.library();
        if lib.built_at == 0 {
            continue; // nothing published yet: stays fail-closed
        }
        let now = crate::model::now();
        let same_lib = built.as_ref().is_some_and(|(live, vod)| live.ptr_eq(&Arc::downgrade(&lib.live)) && *vod == lib.vod_identity());
        pending |= !same_lib || now - built_at >= 3600;
        if !pending {
            continue;
        }
        drop(lib);
        tokio::time::sleep(DEBOUNCE).await;
        let lib = shared.library();
        let id = library_identity(&lib);
        let cfg = guard.config().unwrap_or_default();
        let reader = store.clone();
        let started = std::time::Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            let conn = reader.reader()?;
            super::snapshot::build(&conn, &cfg, &lib, now)
        })
        .await;
        built_at = now;
        match res {
            Ok(Ok(snap)) => {
                tracing::debug!(
                    "spoiler guard: snapshot {} ({} topics, {} series) in {} ms",
                    snap.generation,
                    snap.topics.len(),
                    snap.series.len(),
                    started.elapsed().as_millis()
                );
                guard.publish(snap);
                built = Some(id);
                pending = false;
            }
            // tried again on the next poke or tick
            Ok(Err(e)) => tracing::warn!("spoiler guard: building the snapshot: {e}"),
            Err(e) => tracing::warn!("spoiler guard: the snapshot build stopped: {e}"),
        }
    }
}

async fn track(guard: Guard, shared: Arc<AppShared>, store: Store) {
    let mut tracker = Tracker::default();
    let mut episodes_done: HashSet<crate::model::EpisodeId> = HashSet::new();
    let mut rebuild_next_poll = false;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // an episode reached 90 % last poll: its progress row is saved by now (every 10 s), so
        // the series boundary moves on (whatever plays now; the episode may have ended)
        if std::mem::take(&mut rebuild_next_poll) {
            guard.rebuild();
        }
        let gsnap = guard.snapshot();
        if !gsnap.on || !gsnap.ready {
            continue;
        }
        // every picture shown can mark something seen (two on one airing count it once: the
        // second observation in a poll adds no time)
        for tile in [crate::model::TileId::A, crate::model::TileId::B] {
            let Some(rx) = shared.tile_player(tile) else { continue };
            let snap: Arc<PlayerSnapshot> = rx.borrow().clone();
            if snap.state != PlayerState::Playing {
                continue;
            }
            let Some(p) = snap.playable.clone() else { continue };
            let now = crate::model::now();
            if let (MediaRef::Episode(id), PlayableKind::Episode { .. }) = (&p.media, &p.kind) {
                rebuild_next_poll |= fraction(&snap, None).is_some_and(|f| f >= 0.9) && episodes_done.insert(*id);
                continue;
            }
            let lib = shared.library();
            let (channel, catchup) = match (&p.media, &p.kind) {
                (MediaRef::Channel(ch), PlayableKind::Live) => (*ch, None),
                (MediaRef::Catchup { channel, start, stop, .. }, _) => (*channel, Some((*start, *stop))),
                _ => continue,
            };
            let Some((epg, shift)) = lib.live.get(channel).and_then(|c| Some((c.epg_channel_id?, c.tvg_shift_min))) else { continue };
            drop(lib);
            let frac = catchup.and_then(|(s, e)| fraction(&snap, Some(e - s)));
            let reader = store.clone();
            let watching = tokio::task::spawn_blocking(move || resolve(&reader, epg, shift, catchup, frac, now)).await;
            let w = match watching {
                Ok(Ok(Some(w))) => w,
                Ok(Ok(None)) => continue,
                Ok(Err(e)) => {
                    tracing::debug!("spoiler guard: what's on: {e}");
                    continue;
                }
                Err(_) => continue,
            };
            let tracked = super::verdict::tracks(&gsnap, &w.subject(), now);
            if let Some(mut row) = tracker.observe(&w, tracked, now) {
                super::place_seen_row(&gsnap, &mut row, w.sub_title.as_deref());
                record(&guard, &store, row, now, channel);
            }
        }
    }
}

/// How far through (0–1): the player's position over its duration, else over `length_s`.
fn fraction(snap: &PlayerSnapshot, length_s: Option<i64>) -> Option<f64> {
    let pos = snap.position?;
    let len = snap.duration.filter(|d| *d > 0.0).or(length_s.filter(|l| *l > 0).map(|l| l as f64))?;
    Some(pos / len)
}

/// The guide programme playing on `epg` (live at `now`, or the catch-up airing).
fn resolve(
    store: &Store,
    epg: crate::model::EpgChannelId,
    shift: i32,
    catchup: Option<(Timestamp, Timestamp)>,
    frac: Option<f64>,
    now: Timestamp,
) -> crate::error::Result<Option<Watching>> {
    let conn = store.reader()?;
    let shift_s = i64::from(shift) * 60;
    let id = match catchup {
        None => {
            let nn = crate::store::epg::now_next(&conn, &[(epg, shift)], now)?;
            match nn.get(&(epg, shift)).and_then(|n| n.now.as_ref()) {
                Some(p) => p.id,
                None => return Ok(None),
            }
        }
        Some((start, stop)) => {
            let mid = (start + stop) / 2 - shift_s;
            match crate::store::epg::window(&conn, epg, mid, mid + 1)?.into_iter().find(|p| p.start <= mid && mid < p.stop) {
                Some(p) => p.id,
                None => return Ok(None),
            }
        }
    };
    let p = crate::store::epg::programme(&conn, id)?;
    let se = super::programme_se(&p);
    Ok(Some(Watching {
        event_key: super::event_key_of(&p.title, p.sub_title.as_deref(), p.description.as_deref(), se, p.start),
        start: p.start,
        stop: p.stop,
        title: p.title,
        sub_title: p.sub_title,
        categories: p.categories,
        season_episode: se,
        is_new: p.is_new,
        fraction: catchup.map(|_| frac.unwrap_or(0.0)),
        shift_s,
    }))
}

/// Write a seen row, tell the UI, rebuild.
fn record(guard: &Guard, store: &Store, row: SeenRow, now: Timestamp, channel: ChannelId) {
    let shown = guard.outside_title(&row.title, now).into_owned();
    let bus = store.writer().bus().cloned();
    let (g, r) = (guard.clone(), row.clone());
    let _ = store.writer().submit_notify(crate::msg::DataScope::Spoiler, move |c| {
        let fresh = crate::store::spoiler::mark_seen(c, &r)?;
        if fresh {
            // by channel id, never the title (DESIGN §34 §5)
            tracing::info!("spoiler guard: an event on channel {} marked seen", channel.0);
            g.note_seen(r);
            g.rebuild();
            if let Some(b) = bus {
                b.toast(crate::msg::Toast {
                    action: Some(("Undo…".into(), crate::keymap::Action::SpoilerMenu)),
                    ttl_ms: 6000,
                    ..crate::msg::Toast::info(format!("“{shown}” marked seen — spoiler guard lifted"))
                });
            }
        }
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn match_at(start: Timestamp) -> Watching {
        Watching {
            event_key: "arsenal v chelsea@2026-10-18".into(),
            start,
            stop: start + 7200,
            title: "Arsenal v Chelsea".into(),
            sub_title: None,
            categories: vec!["Sport".into()],
            season_episode: None,
            is_new: false,
            fraction: None,
            shift_s: 0,
        }
    }

    #[test]
    fn live_counts_half_the_match_or_the_last_ten_minutes() {
        let w = match_at(0);
        // two minutes mid-match: not seen
        let mut t = Tracker::default();
        assert!(t.observe(&w, true, 3000).is_none());
        assert!(t.observe(&w, true, 3030).is_none());
        assert!(t.observe(&w, true, 3060).is_none() && t.observe(&w, true, 3090).is_none() && t.observe(&w, true, 3120).is_none());
        // tuned in for the last ten minutes: seen, once
        let row = t.observe(&w, true, 7200 - 300).expect("present near the end");
        assert_eq!((row.how, row.first_start, row.series_norm.as_deref()), (SeenHow::Live, 0, None));
        assert!(t.observe(&w, true, 7200 - 270).is_none(), "reported once");
        // half the match, polled every 30 s
        let mut t = Tracker::default();
        let seen_at = (0..).map(|i| 600 + i * 30).find(|&now| t.observe(&w, true, now).is_some()).unwrap();
        assert_eq!(seen_at, 600 + 3600, "3 600 s of a 7 200 s match");
        // a gap (the window hidden, the machine asleep) adds at most two polls
        let mut t = Tracker::default();
        assert!(t.observe(&w, true, 0).is_none() && t.observe(&w, true, 3500).is_none());
    }

    #[test]
    fn catch_up_needs_ninety_percent_and_untracked_writes_nothing() {
        let mut w = match_at(0);
        w.fraction = Some(0.5);
        let mut t = Tracker::default();
        assert!(t.observe(&w, true, 90_000).is_none());
        w.fraction = Some(0.91);
        assert!(t.observe(&w, false, 90_000).is_none(), "not guarded: no row");
        assert_eq!(t.observe(&w, true, 90_000).map(|r| r.how), Some(SeenHow::Catchup));
        // a series airing records its series and episode (they move the boundary on)
        let ep = Watching { title: "Severance".into(), season_episode: Some((2, 5)), fraction: Some(0.95), ..match_at(0) };
        let row = Tracker::default().observe(&ep, true, 1).unwrap();
        assert_eq!((row.series_norm.as_deref(), row.season, row.episode), (Some("severance"), Some(2), Some(5)));
    }
}
