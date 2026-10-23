//! New episodes' shared UI state and writes (DESIGN §44 §2). The snapshot is read on the
//! blocking pool and cached for Home, the sidebar, the Watchlists screen, the series page and
//! Settings; its writes (dismiss, mute, opened, the first-run card) own their error and Undo
//! toast. Toasts and menus reach the app as `Action::NewEpisodes`, whose numbers name the notes
//! and undos stashed here.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::keymap::{Action, NewEpisodesAct};
use crate::model::{ItemKind, ItemRef, SeriesId, VodKind};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::new_episodes::{NewEpisodesSnapshot, Note, NoteAction, SeriesNews};
use crate::store::new_episodes as store;
use crate::ui::screens::home::kit::{self, Loader};
use crate::ui::{Route, UiCtx};
use crate::watchlist::Entry;

/// Dismiss and mute toasts offer Undo this long.
const UNDO_TTL_MS: u32 = 10_000;
/// A summary toast waits this long for a click.
const NOTE_TTL_MS: u32 = 10_000;
/// Undos and notes kept for their toasts.
const KEEP: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    dir: std::path::PathBuf,
    generation: u64,
    revision: u64,
    /// `[new_episodes]`: on, binged, binge share, Home days.
    config: (bool, bool, u32, u32),
}

#[derive(Default)]
struct Cache {
    key: Option<Key>,
    snapshot: Arc<NewEpisodesSnapshot>,
    pending: Option<Loader<NewEpisodesSnapshot>>,
    /// Bumped whenever the snapshot's content changes (Home's rails key).
    revision: u64,
    /// The first read of this key finished.
    loaded: bool,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::default();
}
static REVISION: AtomicU64 = AtomicU64::new(1);
static FOCUS_ROW: AtomicBool = AtomicBool::new(false);
static UNDO_NEXT: AtomicU16 = AtomicU16::new(1);
static NOTE_NEXT: AtomicU16 = AtomicU16::new(1);
static UNDOS: Mutex<VecDeque<(u16, store::Undo)>> = Mutex::new(VecDeque::new());
static NOTES: Mutex<VecDeque<(u16, Note)>> = Mutex::new(VecDeque::new());

/// Something that feeds the snapshot changed (`DataScope::{NewEpisodes, Progress, Watchlists,
/// Vod, VodDetails}`): read it again.
pub fn invalidate() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Whether the scope changes what the snapshot shows.
pub fn feeds(scope: DataScope) -> bool {
    matches!(scope, DataScope::NewEpisodes | DataScope::Progress | DataScope::Watchlists | DataScope::Vod | DataScope::VodDetails | DataScope::All)
}

/// The feature is on (the kill switch and `[new_episodes] enabled`).
pub fn on(ctx: &UiCtx<'_>) -> bool {
    crate::new_episodes::ENABLED && ctx.config.new_episodes.enabled
}

/// What Home, the sidebar, Watchlists, the series page and Settings show. Empty while the
/// feature is off; the previous content stays until a re-read lands.
pub fn snapshot(ctx: &UiCtx<'_>) -> Arc<NewEpisodesSnapshot> {
    CACHE.with(|cell| {
        let mut c = cell.borrow_mut();
        let cfg = &ctx.config.new_episodes;
        let on = on(ctx);
        let key = Key {
            dir: ctx.services.paths.data_dir.clone(),
            generation: ctx.lib.generation,
            revision: REVISION.load(Ordering::Relaxed),
            config: (on, cfg.binged, cfg.binge_pct, cfg.home_days),
        };
        if c.key.as_ref() != Some(&key) {
            if c.key.as_ref().is_none_or(|old| old.dir != key.dir || old.config.0 != on) {
                c.snapshot = Arc::default();
                c.loaded = !on;
            }
            c.key = Some(key);
            c.pending = on.then(|| {
                let lib = ctx.lib.clone();
                let opts = store::Opts::from_config(cfg, ctx.now);
                Loader::start(ctx, move |db| store::snapshot(db, &lib, &opts))
            });
        }
        if let Some(result) = kit::poll(&mut c.pending) {
            match result {
                Ok(s) => {
                    c.loaded = true;
                    if *c.snapshot != s {
                        c.snapshot = Arc::new(s);
                        c.revision += 1;
                    }
                }
                Err(e) => tracing::warn!("new episodes read: {e}"),
            }
        }
        c.snapshot.clone()
    })
}

/// A counter that moves when the snapshot's content does.
pub fn revision() -> u64 {
    CACHE.with(|c| c.borrow().revision)
}

/// The first read finished (a write that would change nothing is skipped until then only
/// when the feature is off).
fn loaded() -> bool {
    CACHE.with(|c| c.borrow().loaded)
}

// ------------------------------------------------------------------ what a screen asks

/// The Watchlists chip of `entry` ("S2E9–E10"): its title's new episodes inside the entry's
/// coverage that nobody opened yet. A range entry covers its seasons and every later one (an
/// arrival past the range was covered by the open end or another entry).
pub fn entry_chip(snap: &NewEpisodesSnapshot, entry: &Entry) -> Option<String> {
    if entry.kind != ItemKind::Series {
        return None;
    }
    let news = snap.of(entry.item.as_ref()?).filter(|n| n.on_list())?;
    let eps: Vec<(u32, u32)> =
        news.new.iter().filter(|e| !e.opened && entry.season_from.is_none_or(|from| e.season >= from)).map(|e| (e.season, e.episode)).collect();
    (!eps.is_empty()).then(|| crate::new_episodes::span(&eps))
}

/// A list holds an entry with the chip (the lists pane's "● New").
pub fn list_has_news(snap: &NewEpisodesSnapshot, entries: &[crate::watchlist::EntryState]) -> bool {
    entries.iter().any(|e| entry_chip(snap, &e.entry).is_some())
}

/// "2 new episodes since Tue" for the series page header.
pub fn since_line(news: &SeriesNews) -> String {
    let since = news.new.iter().map(|e| e.arrived_at).min().unwrap_or(0);
    let n = news.new.len();
    format!("{n} new episode{} since {}", if n == 1 { "" } else { "s" }, crate::ui::widgets::time::local(since, "%a", ""))
}

/// "Stop alerts for this series" is on for `item`'s copy. One indexed read on the UI thread,
/// made when a menu opens.
pub fn is_muted(ctx: &UiCtx<'_>, item: &ItemRef) -> bool {
    ctx.db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM newep_mute WHERE source_id = ?1 AND series_key = ?2)",
            rusqlite::params![item.source, item.key.as_str()],
            |r| r.get(0),
        )
        .unwrap_or(false)
}

/// The library index of series copy `item`.
fn index_of(ctx: &UiCtx<'_>, item: &ItemRef) -> Option<u32> {
    let id: SeriesId = ctx
        .db
        .query_row("SELECT id FROM series WHERE source_id = ?1 AND key = ?2", rusqlite::params![item.source, item.key.as_str()], |r| r.get(0))
        .ok()?;
    ctx.lib.vod.index_of(VodKind::Series(id))
}

/// "Play S1E6": the label of what [`play`] starts for the title holding the series copies
/// `series` (Q4: the next unwatched episode, not the first new one).
pub fn play_label(ctx: &UiCtx<'_>, series: &[SeriesId]) -> Option<String> {
    let (_, ep) = crate::store::copies::next_episode(ctx.db, series).ok().flatten()?;
    Some(format!("Play S{}E{}", ep.season, ep.episode))
}

// ------------------------------------------------------------------ what a screen does

/// Play the next unwatched episode of the title holding series copy `series`, through its copy
/// plan like Continue watching. The title's chip goes: one of its episodes plays (§3.5).
pub fn play(ctx: &mut UiCtx<'_>, series: &ItemRef) {
    let Some(idx) = index_of(ctx, series) else {
        ctx.toast(Toast::info("That series is no longer in your playlists"));
        return;
    };
    mark_opened(ctx, series);
    if !crate::ui::copies::play_title(ctx, false, idx, None, false) {
        ctx.toast(Toast::info("Unlock to play this series"));
    }
}

/// Queue the new episodes in Downloads (any copy; the downloads service picks the copy).
pub fn download(ctx: &mut UiCtx<'_>, episodes: &[ItemRef]) {
    if !crate::downloads::ENABLED || episodes.is_empty() {
        return;
    }
    let targets =
        episodes.iter().map(|item| crate::downloads::Target { item: item.clone(), programme: None, minutes: None, any_copy: true }).collect();
    crate::ui::downloads::send(ctx, crate::downloads::DownloadCommand::Add { targets, agent: false });
    ctx.toast(Toast::info(match episodes.len() {
        1 => "Added the new episode to Downloads".to_owned(),
        n => format!("Added {n} new episodes to Downloads"),
    }));
}

/// Show Home with the New episodes row under the cursor (the summary's Open).
pub fn show_row(ctx: &mut UiCtx<'_>) {
    FOCUS_ROW.store(true, Ordering::Relaxed);
    ctx.navigate(crate::ui::NavRequest::Switch(Route::Home));
}

/// Home takes the request to focus its New episodes row (once).
pub fn take_focus_row() -> bool {
    FOCUS_ROW.swap(false, Ordering::Relaxed)
}

/// The title holding series copy `item` was opened (its page, its list entry, one of its new
/// episodes played): the Watchlists chip goes in every list.
pub fn mark_opened(ctx: &UiCtx<'_>, item: &ItemRef) {
    if !on(ctx) || (loaded() && !snapshot(ctx).of(item).is_some_and(SeriesNews::unopened)) {
        return;
    }
    let (lib, item, now) = (ctx.lib.clone(), item.clone(), ctx.now);
    let _ = ctx.services.store.writer().submit_notify(DataScope::NewEpisodes, move |c| store::mark_opened(c, &lib, &item, now).map(drop));
}

/// The Watchlists screen opened: the sidebar's dot goes.
pub fn watchlists_opened(ctx: &UiCtx<'_>) {
    if !on(ctx) || (loaded() && !snapshot(ctx).list_dot) {
        return;
    }
    let now = ctx.now;
    let _ = ctx.services.store.writer().submit_notify(DataScope::NewEpisodes, move |c| store::watchlists_opened(c, now));
}

/// The first-run card was answered.
pub fn ack_card(ctx: &UiCtx<'_>) {
    let now = ctx.now;
    let _ = ctx.services.store.writer().submit_notify(DataScope::NewEpisodes, move |c| store::ack_card(c, now));
}

/// Whether the first-run card is due (one indexed read; Home asks on entering, not per frame).
pub fn card_due(db: &rusqlite::Connection) -> bool {
    store::card_due(db).unwrap_or(false)
}

/// Delete on a Home tile or the menu's Dismiss: the title's new episodes leave Home, with Undo.
pub fn dismiss_title(ctx: &UiCtx<'_>, item: &ItemRef, name: &str) {
    let (lib, item, now) = (ctx.lib.clone(), item.clone(), ctx.now);
    write_undoable(ctx, format!("Dismissed the new episodes of {name}"), move |c| store::dismiss_title(c, &lib, &item, now));
}

/// "Stop alerts for this series" (`on`) or "Allow alerts again", with Undo.
pub fn mute(ctx: &UiCtx<'_>, item: &ItemRef, name: &str, on: bool) {
    let (lib, item, now) = (ctx.lib.clone(), item.clone(), ctx.now);
    let text = if on { format!("No more alerts for {name}") } else { format!("Alerts for {name} are back on") };
    write_undoable(ctx, text, move |c| store::mute(c, &lib, &item, on, now));
}

/// Run a store write that returns its [`store::Undo`] and toast `text` with an Undo button.
fn write_undoable(ctx: &UiCtx<'_>, text: String, f: impl FnOnce(&mut rusqlite::Connection) -> Result<store::Undo> + Send + 'static) {
    let bus = ctx.services.bus.clone();
    let _ = ctx.services.store.writer().submit(move |c| {
        match f(c) {
            Ok(undo) => {
                invalidate();
                bus.data_changed(DataScope::NewEpisodes);
                let mut toast = Toast::info(text);
                if !undo.is_empty() {
                    let n = UNDO_NEXT.fetch_add(1, Ordering::Relaxed).max(1);
                    stash(&UNDOS, n, undo);
                    toast.action = Some(("Undo".into(), Action::NewEpisodes(NewEpisodesAct::Undo(n))));
                    toast.ttl_ms = UNDO_TTL_MS;
                }
                bus.toast(toast);
            }
            Err(e) => bus.toast(Toast::error(crate::redact::redact(&e.to_string()))),
        }
        Ok(())
    });
}

fn stash<T>(q: &Mutex<VecDeque<(u16, T)>>, n: u16, v: T) {
    let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
    q.push_back((n, v));
    while q.len() > KEEP {
        q.pop_front();
    }
}

fn take<T>(q: &Mutex<VecDeque<(u16, T)>>, n: u16) -> Option<T> {
    let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
    let at = q.iter().position(|(k, _)| *k == n)?;
    q.remove(at).map(|(_, v)| v)
}

/// A toast's Undo: put the dismissed episodes or the mute back (once).
pub fn undo(services: &crate::services::Services, n: u16) {
    let Some(undo) = take(&UNDOS, n) else {
        services.bus.toast(Toast::info("That can no longer be undone"));
        return;
    };
    let bus = services.bus.clone();
    let _ = services.store.writer().submit(move |c| {
        match store::undo(c, &undo) {
            Ok(()) => {
                invalidate();
                bus.data_changed(DataScope::NewEpisodes);
                bus.toast(Toast::info("Undone"));
            }
            Err(e) => bus.toast(Toast::error(crate::redact::redact(&e.to_string()))),
        }
        Ok(())
    });
}

// ------------------------------------------------------------------ the toast fallback

/// The in-app toast for a notification no server showed (§2.2): the same text, with Play (or
/// Open) and, when it offers one, Download.
pub fn toast(note: Note) -> Toast {
    let n = NOTE_NEXT.fetch_add(1, Ordering::Relaxed).max(1);
    let mut toast = Toast { kind: ToastKind::Info, text: note.toast_text(), action: None, also: None, ttl_ms: NOTE_TTL_MS };
    toast.action = Some(match &note.action {
        NoteAction::Play { label, .. } => (label.clone(), Action::NewEpisodes(NewEpisodesAct::Play(n))),
        NoteAction::Open => ("Open".into(), Action::NewEpisodes(NewEpisodesAct::Open)),
    });
    if !note.download.is_empty() && crate::downloads::ENABLED {
        toast.also = Some(("Download".into(), Action::NewEpisodes(NewEpisodesAct::Download(n))));
    }
    stash(&NOTES, n, note);
    toast
}

/// The note behind a toast's Play or Download button (kept while the toast may show).
pub fn note(n: u16) -> Option<Note> {
    NOTES.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone())
}

/// The master switch ("New episode alerts" in Settings, "Turn off" on the first-run card).
/// Off forgets the ledger, so switching it on again starts with a silent baseline (§3.4); the
/// next playlist refresh or a "Check now" does that.
pub fn set_enabled(ctx: &mut UiCtx<'_>, on: bool) {
    ctx.edit_config(move |c| c.new_episodes.enabled = on);
    if !on {
        let _ = ctx.services.store.writer().submit_notify(DataScope::NewEpisodes, |c| store::forget_ledger(c).map(drop));
    }
}

/// Fixtures for the screens' tests: a series on My list whose catalogue then grows.
#[cfg(test)]
pub(crate) mod testkit {
    use rusqlite::{Connection, params};

    use crate::error::Result;
    use crate::model::{ItemKey, ItemKind, ItemRef, SeriesId};
    use crate::store::new_episodes::{CheckInput, Opts, check};
    use crate::ui::screens::home::testkit as home;
    use crate::ui::testing::TestUi;

    pub(crate) fn opts() -> Opts {
        Opts::from_config(&crate::config::NewEpisodesConfig::default(), crate::model::now())
    }

    /// One check on the library as it stands (the first one is the silent baseline).
    pub(crate) fn run(c: &mut Connection) -> Result<crate::store::new_episodes::CheckReport> {
        let lib = crate::store::library::load(c)?;
        check(c, &lib, &opts(), &CheckInput { diff: true, ..Default::default() })
    }

    pub(crate) fn series_ref(c: &Connection, id: SeriesId) -> Result<ItemRef> {
        let key: String = c.query_row("SELECT key FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        Ok(ItemRef {
            source: c.query_row("SELECT source_id FROM series WHERE id = ?1", [id], |r| r.get(0))?,
            kind: ItemKind::Series,
            key: ItemKey(key),
        })
    }

    /// Season 1 episodes `from..=to` of series `id` (named like `testkit::named_series`).
    pub(crate) fn add_episodes(c: &Connection, id: SeriesId, from: u32, to: u32) -> Result<()> {
        let src: i64 = c.query_row("SELECT source_id FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        for e in from..=to {
            c.execute(
                "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, 1, ?4, ?5, 2700, ?6)",
                params![
                    id,
                    src,
                    format!("m3u:added-ep{}-{e}", id.0),
                    e,
                    format!("Episode {e}"),
                    format!(r#"{{"t":"url","url":"http://127.0.0.1:9/series/{}/{e}.mp4"}}"#, id.0)
                ],
            )?;
        }
        Ok(())
    }

    /// "Landman" with `before` episodes on My list, baselined; then episodes `before + 1..=
    /// before + new` arrive and are found by a check.
    pub(crate) fn landman_with_news(t: &mut TestUi, before: u32, new: u32) -> SeriesId {
        home::seed(t, move |c| {
            let tv = home::source(c, "TV")?;
            let id = home::named_series(c, tv, &[("Landman", "Landman")], before)?[0];
            let list = crate::store::user::default_fav_list(c)?;
            home::save_favourite(c, list, &series_ref(c, id)?, true, crate::model::now())?;
            run(c)?;
            // the first-run card has been answered (it has its own test)
            crate::store::new_episodes::ack_card(c, crate::model::now())?;
            add_episodes(c, id, before + 1, before + new)?;
            let found = run(c)?;
            assert_eq!(found.arrivals, new as usize, "the new episodes are found");
            Ok(id)
        })
    }
}
