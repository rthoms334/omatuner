//! Shared Watchlists UI state and writes (DESIGN §42 §2–§5). Reads run on the blocking pool;
//! the cached snapshot is shared by Home, Search, details and the Watchlists screen.
use crate::error::Result;
use crate::keymap::Action;
use crate::model::{ItemKind, ItemRef, WatchlistId, WatchlistItemId};
use crate::msg::{DataScope, Toast};
use crate::store::watchlist as store;
use crate::ui::screens::home::kit::{self, Loader};
use crate::ui::{OverlayRequest, UiCtx};
use crate::watchlist::{EntryState, Undo, WatchlistsSnapshot};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Cache {
    key: Option<(std::path::PathBuf, u64, bool, u64, bool, crate::config::CopyOrder)>,
    snapshot: Arc<WatchlistsSnapshot>,
    pending: Option<Loader<WatchlistsSnapshot>>,
}
thread_local! { static CACHE: RefCell<Cache> = RefCell::default(); }
static REVISION: AtomicU64 = AtomicU64::new(1);
static UNDOS: Mutex<Vec<(u16, Undo)>> = Mutex::new(Vec::new());
static NEXT_UNDO: AtomicU64 = AtomicU64::new(0);

pub fn invalidate() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

pub fn snapshot(ctx: &UiCtx<'_>) -> Arc<WatchlistsSnapshot> {
    CACHE.with(|cell| {
        let mut c = cell.borrow_mut();
        let unlocked = kit::unlocked(ctx);
        let key = (
            ctx.services.paths.data_dir.clone(),
            ctx.lib.generation,
            unlocked,
            REVISION.load(Ordering::Relaxed),
            ctx.parental.hides_explicit(&ctx.config.parental),
            ctx.config.interface.copy_order,
        );
        if c.key.as_ref() != Some(&key) {
            if c.key.as_ref().is_none_or(|old| old.0 != key.0 || old.2 != key.2 || old.4 != key.4) {
                c.snapshot = Arc::default();
            }
            c.key = Some(key);
            let lib = ctx.lib.clone();
            let hides = ctx.parental.hides_explicit(&ctx.config.parental);
            let copy_order = ctx.config.interface.copy_order;
            c.pending = Some(Loader::start(ctx, move |db| store::snapshot_with_preferences(db, &lib, unlocked, false, hides, copy_order)));
            // Relocking must conceal rows in the same frame, before the worker answers.
            let mut visible = (*c.snapshot).clone();
            visible.lists.retain_mut(|l| {
                let was_empty = l.total == 0;
                l.entries.retain(|e| !(hides && e.entry.explicit) && e.entry.item.as_ref().is_none_or(|i| !ctx.lib.conceals(i)));
                l.total = l.entries.len();
                l.done = l.entries.iter().filter(|e| e.done()).count();
                if l.next.is_some_and(|id| !l.entries.iter().any(|e| e.entry.id == id)) {
                    l.next = None;
                }
                l.total != 0 || was_empty
            });
            c.snapshot = Arc::new(visible);
        }
        if let Some(result) = kit::poll(&mut c.pending) {
            match result {
                Ok(s) => c.snapshot = Arc::new(s),
                Err(e) => tracing::warn!("watchlist read: {e}"),
            }
        }
        c.snapshot.clone()
    })
}

/// Writes own their error and optional Undo toast, even after the opener closes.
pub fn write(ctx: &UiCtx<'_>, text: impl Into<String>, f: impl FnOnce(&mut rusqlite::Connection) -> Result<Option<Undo>> + Send + 'static) {
    let text = text.into();
    let bus = ctx.services.bus.clone();
    let _ = ctx.services.store.writer().submit(move |c| {
        match f(c) {
            Ok(undo) => {
                invalidate();
                bus.data_changed(DataScope::Watchlists);
                let mut toast = Toast::info(text);
                if let Some(undo) = undo {
                    let id = 0xE000 | (NEXT_UNDO.fetch_add(1, Ordering::Relaxed) as u16 & 0x0fff);
                    let mut undos = UNDOS.lock().unwrap_or_else(|e| e.into_inner());
                    undos.push((id, undo));
                    if undos.len() > 32 {
                        undos.remove(0);
                    }
                    toast.action = Some(("Undo".into(), Action::Undo(id)));
                    toast.ttl_ms = 6000;
                }
                bus.toast(toast);
            }
            Err(e) => bus.toast(Toast::error(crate::redact::redact(&e.to_string()))),
        }
        Ok(())
    });
}

pub fn is_undo(id: u16) -> bool {
    (0xE000..0xF000).contains(&id)
}
pub fn undo(services: &crate::services::Services, id: u16) {
    let mut all = UNDOS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(at) = all.iter().position(|(n, _)| *n == id) else { return };
    let (_, undo) = all.remove(at);
    let bus = services.bus.clone();
    let _ = services.store.writer().submit(move |c| {
        match store::undo(c, &undo) {
            Ok(()) => {
                invalidate();
                bus.data_changed(DataScope::Watchlists);
                bus.toast(Toast::info("Undone"));
            }
            Err(e) => bus.toast(Toast::error(crate::redact::redact(&e.to_string()))),
        }
        Ok(())
    });
}

pub fn toggle_my_list(ctx: &mut UiCtx<'_>, item: &ItemRef) {
    let copies = store::title_copies(ctx.db, ctx.lib, item).unwrap_or_else(|_| vec![item.clone()]);
    toggle_my_list_of(ctx, item, &copies);
}

pub fn toggle_my_list_of(ctx: &mut UiCtx<'_>, item: &ItemRef, copies: &[ItemRef]) {
    let item = item.clone();
    let lib = ctx.lib.clone();
    let now = ctx.now;
    let on = !copies.iter().any(|r| ctx.favs.is_fav(r));
    if on {
        ctx.favs.apply_my_list(&item, true);
    } else {
        for r in copies {
            ctx.favs.apply_my_list(r, false);
        }
    }
    write(ctx, if on { "Saved to My list" } else { "Removed from My list" }, move |c| {
        store::toggle_my_list(c, &lib, &item, now).map(|(_, undo)| Some(undo))
    });
}

pub fn picker(ctx: &mut UiCtx<'_>, items: Vec<ItemRef>, title: &str) {
    ctx.open(OverlayRequest::WatchlistPicker(crate::ui::overlays::watchlists::PickerSpec { items, title: title.into(), edit: None, create: false }));
}

type Memberships = Vec<(crate::watchlist::List, usize, usize)>;
#[derive(Default)]
struct MembershipCache {
    key: Option<(std::path::PathBuf, u64, u64, bool)>,
    entries: std::collections::HashMap<ItemRef, (Arc<Memberships>, Option<Loader<Memberships>>)>,
}
thread_local! { static MEMBERSHIPS: RefCell<MembershipCache> = RefCell::default(); }

pub fn memberships(ctx: &UiCtx<'_>, item: &ItemRef) -> Arc<Memberships> {
    MEMBERSHIPS.with(|cache| {
        let mut c = cache.borrow_mut();
        let key = (
            ctx.services.paths.data_dir.clone(),
            ctx.lib.generation,
            REVISION.load(Ordering::Relaxed),
            ctx.parental.hides_explicit(&ctx.config.parental),
        );
        if c.key.as_ref() != Some(&key) {
            c.key = Some(key);
            c.entries.clear();
        }
        if !c.entries.contains_key(item) {
            if c.entries.len() >= 64 {
                c.entries.clear();
            }
            let lib = if ctx.parental.hides_explicit(&ctx.config.parental) { Arc::new(ctx.lib.hiding_explicit()) } else { ctx.lib.clone() };
            let target = item.clone();
            let pending = Loader::start(ctx, move |db| store::memberships(db, &lib, &target));
            c.entries.insert(item.clone(), (Arc::default(), Some(pending)));
        }
        let (value, pending) = c.entries.get_mut(item).expect("inserted membership");
        if let Some(Ok(result)) = kit::poll(pending) {
            *value = Arc::new(result);
        }
        value.clone()
    })
}

pub fn membership_line(ctx: &UiCtx<'_>, item: &ItemRef) -> Option<String> {
    let memberships = memberships(ctx, item);
    if memberships.is_empty() {
        return None;
    }
    Some(format!(
        "In {}",
        memberships
            .iter()
            .map(|(l, p, n)| {
                if l.play_order == crate::watchlist::PlayOrder::Ordered { format!("{} ({p} of {n})", l.name) } else { l.name.clone() }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    ))
}

pub fn play(ctx: &mut UiCtx<'_>, list: WatchlistId, from: Option<WatchlistItemId>, just_one: bool) {
    let q = crate::queue::ui::snapshot(ctx);
    if from.is_none() && !just_one && q.context == Some(list) && q.current.is_some() {
        crate::queue::ui::send(ctx, crate::queue::QueueCommand::Resume);
        return;
    }
    let snap = snapshot(ctx);
    let entry = snap.list(list).and_then(|l| from.and_then(|id| l.entries.iter().find(|e| e.entry.id == id)).or_else(|| l.next_entry()));
    if let Some(e) = entry
        && let Some(p) = e.progress.as_ref().filter(|p| p.position_s >= ctx.config.player.resume_min_s as f64)
        && let Some(item) = &e.playable
    {
        let media = match item.kind {
            ItemKind::Movie => crate::store::vod::resolve_movie(ctx.db, item).ok().flatten().map(crate::model::MediaRef::Movie),
            ItemKind::Episode => crate::store::vod::resolve_episode(ctx.db, item).ok().flatten().map(crate::model::MediaRef::Episode),
            _ => None,
        };
        if let Some(media) = media {
            ctx.open(OverlayRequest::Resume(crate::ui::overlays::ResumeSpec {
                media,
                title: label(ctx, e),
                position_s: p.position_s,
                duration_s: p.duration_s,
                copy: None,
                queue: None,
                watchlist: Some(crate::ui::overlays::WatchlistResume { list, from: Some(e.entry.id), just_one }),
            }));
            return;
        }
    }
    let from = from.or_else(|| snap.list(list).filter(|l| l.list.play_order == crate::watchlist::PlayOrder::Any).and_then(|l| l.next));
    crate::queue::ui::send(ctx, crate::queue::QueueCommand::PlayList { list, from, just_one });
}

pub fn label(ctx: &UiCtx<'_>, e: &EntryState) -> String {
    let mut title = e.entry.title.clone();
    if e.entry.kind == ItemKind::Episode
        && let Some((s, n)) = e.episode
    {
        let series = e.series.as_deref().unwrap_or("");
        let norm = crate::spoiler::series_key(series);
        let sp = crate::ui::widgets::spoiler::Spoilers::of(ctx);
        let subject = crate::guard::Subject::Episode { series_norm: &norm, season: s as i32, episode: n as i32 };
        title = match sp.text(&subject, crate::guard::Field::EpisodeTitle, &title) {
            Some(t) if e.entry.item.is_some() => format!("{series} · S{s}E{n} · {t}"),
            Some(_) => format!("{series} · S{s}E{n}"),
            None => format!("{series} · S{s}E{n}"),
        };
    }
    if let Some(y) = e.entry.year {
        title.push_str(&format!(" ({y})"));
    }
    if let Some(a) = e.entry.season_from {
        let b = e.entry.season_to.unwrap_or(a);
        title.push_str(&if a == b { format!(" · S{a}") } else { format!(" · S{a}–S{b}") });
    }
    title
}

/// A global watched/unwatched edit follows every copy; ranges include only their episodes.
pub fn mark_watched(ctx: &UiCtx<'_>, e: EntryState, on: bool) {
    let Some(item) = e.entry.item else { return };
    let lib = ctx.lib.clone();
    let now = ctx.now;
    let bus = ctx.services.bus.clone();
    write(ctx, if on { "Marked as watched" } else { "Marked unwatched" }, move |c| {
        let copies = store::title_copies(c, &lib, &item)?;
        let mut items = Vec::new();
        if item.kind == ItemKind::Series {
            let from = e.entry.season_from.unwrap_or(1);
            let to = e.entry.season_to.unwrap_or(u32::MAX);
            for series in copies {
                let mut q=c.prepare_cached("SELECT e.source_id,e.key,e.title FROM episode e JOIN series s ON s.id=e.series_id WHERE s.source_id=?1 AND s.key=?2 AND e.season BETWEEN ?3 AND ?4")?;
                let rows = q.query_map(rusqlite::params![series.source, series.key.as_str(), from, to], |r| {
                    Ok((ItemRef { source: r.get(0)?, kind: ItemKind::Episode, key: crate::model::ItemKey(r.get(1)?) }, r.get(2)?))
                })?;
                items.extend(rows.collect::<rusqlite::Result<Vec<(ItemRef, String)>>>()?);
            }
        } else {
            items.extend(copies.into_iter().map(|r| (r, e.entry.title.clone())));
        }
        crate::store::user::mark_watched(c, &items, on, now)?;
        bus.data_changed(DataScope::Progress);
        Ok(None)
    });
}
