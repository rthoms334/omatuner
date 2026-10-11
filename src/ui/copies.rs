//! The UI side of a title's copies in "All playlists" (DESIGN §29.14): playing a combined
//! title with its fallback, the "fell back" toast, "Play from TV 2 at 41:20" after a failure
//! mid-film, and the combined actions of the recommendation's §3 table with their Undo.
//!
//! The data rules live in `store::copies` (which copy plays, what each action writes); this
//! module only builds the player commands and runs the writes on the store writer. The grids
//! and detail pages call it for a title shown combined (All, `interface.same_title =
//! combine`); inside one playlist they keep using `kit`, which touches the one copy.
//!
//! Undo: a combined write stashes the rows it found ([`store::copies::Undo`]) under a number
//! and toasts "… · Undo" with `Action::Undo(n)`; the app hands that action to [`undo`], which
//! writes the rows back. The last [`KEEP`] actions can be undone.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::keymap::Action;
use crate::model::{FavListId, ItemRef, MediaRef, PlayOrigin, PlayableKind, SeriesId, SourceId, VodKind};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::player::{CopyPlay, PlayRequest, PlayerCommand, PlayerSnapshot};
use crate::store::copies::{self, CopyPlan, Undo};
use crate::store::library::{Library, VodIndex};

/// How many combined actions can still be undone.
pub const KEEP: usize = 8;
/// How long an Undo toast stays.
const UNDO_TTL_MS: u32 = 8000;

static UNDO: Mutex<VecDeque<(u16, Undo)>> = Mutex::new(VecDeque::new());
static NEXT: AtomicU16 = AtomicU16::new(1);

/// Keep `u` for an Undo button; returns its number.
pub fn stash(u: Undo) -> u16 {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut q = UNDO.lock().unwrap_or_else(|e| e.into_inner());
    q.push_back((n, u));
    while q.len() > KEEP {
        q.pop_front();
    }
    n
}

/// The stashed rows of Undo number `n` (once).
pub fn take(n: u16) -> Option<Undo> {
    let mut q = UNDO.lock().unwrap_or_else(|e| e.into_inner());
    let at = q.iter().position(|(k, _)| *k == n)?;
    q.remove(at).map(|(_, u)| u)
}

/// `Action::Undo(n)` (the app): write back what action `n` found. False when it is too old.
pub fn undo(services: &crate::services::Services, n: u16) -> bool {
    let Some(u) = take(n) else { return false };
    let _ = services.store.writer().submit_notify(u.scope(), move |c| copies::undo(c, &u));
    true
}

/// Run a combined write on the store writer; when it succeeds, toast `text` with an Undo
/// button for the rows it found.
fn write_with_undo(
    ctx: &crate::ui::UiCtx<'_>,
    scope: DataScope,
    text: String,
    f: impl FnOnce(&rusqlite::Connection) -> crate::error::Result<Undo> + Send + 'static,
) {
    let bus = ctx.services.bus.clone();
    let _ = ctx.services.store.writer().submit_notify(scope, move |c| {
        let n = stash(f(c)?);
        bus.toast(Toast { kind: ToastKind::Info, text, action: Some(("Undo".into(), Action::Undo(n))), ttl_ms: UNDO_TTL_MS });
        Ok(())
    });
}

/// The plan of `item`'s title for the current session (unlock state, copy order).
pub fn plan(ctx: &crate::ui::UiCtx<'_>, movies: bool, item: u32) -> crate::error::Result<CopyPlan> {
    let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
    copies::plan(ctx.db, &ctx.lib.vod, movies, item, unlocked, ctx.config.interface.copy_order, ctx.now)
}

fn movie_media(vod: &VodIndex, item: u32) -> Option<MediaRef> {
    match vod.movies.get(item as usize)?.kind {
        VodKind::Movie(id) => Some(MediaRef::Movie(id)),
        VodKind::Series(_) => None,
    }
}

/// The command that plays a combined movie: the plan's first copy (or `pick`, a copy the user
/// chose on the detail page) with the next copy as its fallback. `None` when nothing may play
/// (every copy locked while the session is: ask for the PIN). `resume` continues the most
/// recently played copy's position (the card's "Resume 41:20"), whichever copy plays.
pub fn movie_command(vod: &VodIndex, plan: &CopyPlan, pick: Option<u32>, resume: bool, origin: PlayOrigin) -> Option<PlayerCommand> {
    let first = pick.or(plan.first())?;
    let fallback = plan.order.iter().copied().find(|c| *c != first).and_then(|c| movie_media(vod, c));
    // the latest progress belongs to another copy (a pin, a pick): start this one there
    let start_at = plan
        .resume()
        .filter(|c| resume && c.item != first)
        .and_then(|c| c.progress.as_ref())
        .filter(|p| !p.finished && p.position_s > 0.0)
        .map(|p| p.position_s);
    let req = PlayRequest { media: movie_media(vod, first)?, zap: None, origin, resume, immediate: true };
    Some(PlayerCommand::PlayCopy(CopyPlay { req, fallback, start_at }))
}

/// The command that plays an episode of the series copy `series` (the page's copy), with the
/// same episode of the next series copy in `plan` as the fallback when its episodes are loaded.
pub fn episode_command(
    conn: &rusqlite::Connection,
    vod: &VodIndex,
    plan: &CopyPlan,
    series: SeriesId,
    episode: &crate::model::Episode,
    resume: bool,
    origin: PlayOrigin,
) -> PlayerCommand {
    let this = vod.index_of(VodKind::Series(series));
    let next = plan.order.iter().copied().find(|c| Some(*c) != this).and_then(|c| match vod.series.get(c as usize)?.kind {
        VodKind::Series(id) => copies::same_episode(conn, id, episode.season, episode.episode).ok().flatten(),
        VodKind::Movie(_) => None,
    });
    let req = PlayRequest { media: MediaRef::Episode(episode.id), zap: None, origin, resume, immediate: true };
    PlayerCommand::PlayCopy(CopyPlay { req, fallback: next.map(MediaRef::Episode), start_at: None })
}

/// The toast of `PlayerEvent::FellBack`: "TV couldn't play Dune — playing from TV 2".
pub fn fell_back_toast(lib: &Library, title: &str, from: Option<SourceId>, to: Option<SourceId>) -> Toast {
    let name = |s: Option<SourceId>| s.and_then(|s| lib.source(s)).map(|m| m.name.clone());
    let text = match (name(from), name(to)) {
        (Some(f), Some(t)) => format!("{f} couldn't play {title} — playing from {t}"),
        (None, Some(t)) => format!("{title} couldn't start — playing from {t}"),
        _ => format!("{title} couldn't start — playing another copy"),
    };
    Toast { kind: ToastKind::Warning, text, action: None, ttl_ms: 5000 }
}

/// Another copy to offer on the player's error card after a failure mid-film.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherCopy {
    /// "Play from TV 2 at 41:20".
    pub label: String,
    pub play: CopyPlay,
}

/// The copy the error card offers for the failed playable in `snap` (a movie, or an episode
/// whose other series copy has it loaded), starting where playback stopped. A few indexed
/// reads: the player calls it once per failure.
pub fn other_copy(
    conn: &rusqlite::Connection,
    lib: &Library,
    plan_of: impl Fn(bool, u32) -> Option<CopyPlan>,
    snap: &PlayerSnapshot,
) -> Option<OtherCopy> {
    let p = snap.playable.as_ref()?;
    let vod = &lib.vod;
    let (media, source) = match (&p.media, &p.kind) {
        (MediaRef::Movie(id), _) => {
            let at = vod.index_of(VodKind::Movie(*id))?;
            let other = plan_of(true, at)?.instead_of(at)?;
            let v = vod.movies.get(other as usize)?;
            (movie_media(vod, other)?, v.source_id)
        }
        (MediaRef::Episode(id), PlayableKind::Episode { season, episode }) => {
            let series = crate::store::vod::episode(conn, *id).ok()?.series_id;
            let at = vod.index_of(VodKind::Series(series))?;
            let other = plan_of(false, at)?.instead_of(at)?;
            let v = vod.series.get(other as usize)?;
            let VodKind::Series(other_id) = v.kind else { return None };
            let ep = copies::same_episode(conn, other_id, *season, *episode).ok().flatten()?;
            (MediaRef::Episode(ep), v.source_id)
        }
        _ => return None,
    };
    let at = snap.last_position.or(snap.position).filter(|s| *s >= 1.0);
    let name = lib.source(source).map_or_else(|| "another playlist".to_owned(), |s| s.name.clone());
    let label = match at {
        Some(s) => format!("Play from {name} at {}", crate::ui::screens::home::kit::clock(s)),
        None => format!("Play from {name}"),
    };
    let req = PlayRequest { media, zap: None, origin: snap.origin.unwrap_or(PlayOrigin::Fullscreen), resume: false, immediate: true };
    Some(OtherCopy { label, play: CopyPlay { req, fallback: None, start_at: at } })
}

// ---------------------------------------------------------------------------------------------
// Combined actions (store::copies module docs)

/// The names of the playlists of `refs`, "TV and TV 2" (the hide toast).
fn playlists(lib: &Library, refs: &[ItemRef]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for r in refs {
        if let Some(s) = lib.source(r.source)
            && !names.contains(&s.name.as_str())
        {
            names.push(&s.name);
        }
    }
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// ★ on a combined title (default Favourites list): add the copy that would play, or remove
/// every copy — the index is patched at once, the Undo toast follows the write.
pub fn toggle_favourite(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan) {
    let Some(list) = ctx.favs.default_list() else {
        ctx.toast(Toast::error("The Favourites list is missing"));
        return;
    };
    let refs = plan.refs();
    let on = !refs.iter().any(|r| ctx.favs.is_fav(r));
    let Some(plays) = plan.first().or(plan.copies.first().map(|c| c.item)).and_then(|i| plan.get(i)).map(|c| c.item_ref.clone()) else { return };
    if on {
        ctx.favs.apply_toggle(list, &plays, true);
    } else {
        refs.iter().for_each(|r| ctx.favs.apply_toggle(list, r, false));
    }
    let text = if on { "Added to Favourites" } else { "Removed from Favourites" };
    favourite_write(ctx, list, refs, plays, on, text.to_owned());
}

fn favourite_write(ctx: &crate::ui::UiCtx<'_>, list: FavListId, refs: Vec<ItemRef>, plays: ItemRef, on: bool, text: String) {
    let now = ctx.now;
    write_with_undo(ctx, DataScope::Favourites, text, move |c| copies::favourite(c, list, &refs, &plays, on, now));
}

/// ✓ on a combined movie: mark the copy that would play watched, or (`on` false) forget every
/// copy's progress.
pub fn mark_watched(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, title: &str, on: bool) {
    let refs = plan.refs();
    let Some(plays) = plan.first().and_then(|i| plan.get(i)).or(plan.copies.first()).map(|c| c.item_ref.clone()) else { return };
    let (now, title) = (ctx.now, title.to_owned());
    let text = if on { "Marked as watched" } else { "Removed from Continue watching" };
    write_with_undo(ctx, DataScope::Progress, text.to_owned(), move |c| copies::watched(c, &refs, (&plays, &title), on, now));
}

/// "Forget" a combined series: every episode progress of every copy.
pub fn forget_series(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan) {
    let ids: Vec<SeriesId> = plan
        .copies
        .iter()
        .filter_map(|c| match ctx.lib.vod.series.get(c.item as usize)?.kind {
            VodKind::Series(id) => Some(id),
            VodKind::Movie(_) => None,
        })
        .collect();
    write_with_undo(ctx, DataScope::Progress, "Removed from Continue watching".into(), move |c| copies::forget_series(c, &ids));
}

/// Ctrl+L on a combined title: every copy (the caller checked the PIN, as `kit::toggle_lock`).
pub fn set_locked(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, on: bool) {
    let refs = plan.refs();
    let text = if on { "Locked every copy" } else { "Unlocked every copy" };
    write_with_undo(ctx, DataScope::Overrides, text.into(), move |c| copies::lock(c, &refs, on));
}

/// Ctrl+H on a combined title: every copy; the toast names their playlists.
pub fn hide(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, name: &str) {
    let refs = plan.refs();
    let text = format!("Hid {name} in {} — Settings ▸ Hidden items shows it again", playlists(ctx.lib, &refs));
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::hide(c, &refs));
}

/// "Not the same title" on a copy (mini-card menu): it leaves its stack, with Undo.
pub fn split(ctx: &mut crate::ui::UiCtx<'_>, item: &ItemRef, name: &str) {
    let (item, now) = (item.clone(), ctx.now);
    let text = format!("{name} is shown on its own now");
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::split(c, &item, true, now));
}

/// "Always play from <playlist>" on a copy of `plan` (or off), with Undo.
pub fn pin(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, item: &ItemRef, on: bool) {
    let playlist = ctx.lib.source(item.source).map_or_else(|| "this playlist".to_owned(), |s| s.name.clone());
    let text = if on { format!("Always plays from {playlist}") } else { "Plays the best copy again".to_owned() };
    let (refs, item, now) = (plan.refs(), item.clone(), ctx.now);
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::pin(c, &item, &refs, on, now));
}

/// F2 on a combined title, once the user typed `name`: copies without their own rename get
/// it; with `overwrite` the others too (ask first with [`copies::rename_conflicts`]).
pub fn rename(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, name: &str, overwrite: bool) {
    let (refs, name) = (plan.refs(), name.to_owned());
    write_with_undo(ctx, DataScope::Overrides, format!("Renamed to {name}"), move |c| copies::rename(c, &refs, &name, overwrite));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Undo numbers are handed out once and only the last [`KEEP`] actions can be undone.
    #[test]
    fn undo_stash_keeps_the_last_actions_once() {
        let empty = Undo::empty;
        let first = stash(empty());
        let kept: Vec<u16> = (0..KEEP).map(|_| stash(empty())).collect();
        assert!(take(first).is_none(), "pushed out by {KEEP} newer actions");
        assert!(take(kept[KEEP - 1]).is_some());
        assert!(take(kept[KEEP - 1]).is_none(), "once");
    }
}
