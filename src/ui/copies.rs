//! The UI side of a title's copies in "All playlists" (DESIGN §29.15): playing a combined
//! title with its fallback, the "fell back" toast, "Play from TV 2 at 41:20" after a failure
//! mid-film, and the combined actions of the recommendation's §3 table with their Undo.
//!
//! The data rules live in `store::copies` (which copy plays, what each action writes); this
//! module only builds the player commands and runs the writes on the store writer. The grids
//! and detail pages call it for a title shown combined (All, `interface.same_title =
//! combine`); inside one playlist they keep using `kit`, which touches the one copy.
//!
//! Undo: a combined write stashes the rows it found ([`store::copies::Undo`]) in the app's
//! `Services::undo` under a number and toasts "… · Undo" with `Action::Undo(n)`; the app hands
//! that action to [`undo`], which writes the rows back — whatever screen is showing. The last
//! [`copies::UNDO_KEEP`] actions can be undone.

use crate::keymap::Action;
use crate::model::{FavListId, ItemKind, ItemRef, MediaRef, PlayOrigin, PlayableKind, SeriesId, SourceId, VodKind};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::player::{CopyPlay, PlayRequest, PlayerCommand, PlayerSnapshot};
use crate::store::copies::{self, CopyPlan, Undo};
use crate::store::library::{Library, VodIndex};
use crate::ui::overlays::ResumeSpec;
use crate::ui::{OverlayRequest, Route, UiCtx};

/// How long an Undo toast stays.
const UNDO_TTL_MS: u32 = 8000;

/// `Action::Undo(n)` (the app): write back what action `n` found. False when it is too old.
pub fn undo(services: &crate::services::Services, n: u16) -> bool {
    let Some(u) = services.undo.take(n) else { return false };
    let _ = services.store.writer().submit_notify(u.scope(), move |c| copies::undo(c, &u));
    true
}

/// Run a combined write on the store writer; when it succeeds, toast `text` with an Undo
/// button for the rows it found.
pub(crate) fn write_with_undo(
    ctx: &crate::ui::UiCtx<'_>,
    scope: DataScope,
    text: String,
    f: impl FnOnce(&rusqlite::Connection) -> crate::error::Result<Undo> + Send + 'static,
) {
    let (bus, stash) = (ctx.services.bus.clone(), ctx.services.undo.clone());
    let _ = ctx.services.store.writer().submit_notify(scope, move |c| {
        let n = stash.stash(f(c)?);
        bus.toast(Toast { kind: ToastKind::Info, text, action: Some(("Undo".into(), Action::Undo(n))), also: None, ttl_ms: UNDO_TTL_MS });
        Ok(())
    });
}

/// The plan of `item`'s title for the current session (unlock state, copy order).
pub fn plan(ctx: &crate::ui::UiCtx<'_>, movies: bool, item: u32) -> crate::error::Result<CopyPlan> {
    let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
    copies::plan(ctx.db, &ctx.lib.vod, movies, item, unlocked, ctx.config.interface.copy_order, ctx.now)
}

/// Play a combined title from its card (Shift+Enter in a grid, on Home or in Search; DESIGN
/// §29.15): the copy that plays first — or `pick` — with the next one as its fallback, behind
/// the resume prompt when the latest progress of any copy can be resumed (`prompt`; else it
/// resumes directly). A series plays the episode its copy with the newest progress is at.
/// False when every copy is locked (the caller asks for the PIN).
pub fn play_title(ctx: &mut UiCtx<'_>, movies: bool, item: u32, pick: Option<u32>, prompt: bool) -> bool {
    let plan = match plan(ctx, movies, item) {
        Ok(p) => p,
        Err(e) => {
            ctx.toast(Toast::error(format!("Couldn't play: {e}")));
            return true;
        }
    };
    if pick.or(plan.first()).is_none() {
        return false;
    }
    let min = f64::from(ctx.config.player.resume_min_s);
    if movies {
        let Some(PlayerCommand::PlayCopy(copy)) = movie_command(&ctx.lib.vod, &plan, pick, true, PlayOrigin::Fullscreen) else { return false };
        let title = ctx.lib.vod.movies.get(item as usize).map(|v| v.name.to_string()).unwrap_or_default();
        let latest = plan.resume().and_then(|c| c.progress.clone()).filter(|p| prompt && !p.finished && p.position_s > min);
        match latest {
            Some(p) => {
                let spec = ResumeSpec {
                    media: copy.req.media.clone(),
                    title,
                    position_s: p.position_s,
                    duration_s: p.duration_s,
                    copy: Some(Box::new(copy)),
                    queue: None,
                    cast: None,
                    watchlist: None,
                };
                ctx.open(OverlayRequest::Resume(spec));
            }
            None => start(ctx, copy),
        }
        return true;
    }
    // a series: the episode of the copy with the newest progress (else of the copy that
    // plays); a picked copy plays its own next episode
    let ids: Vec<SeriesId> = match pick {
        Some(p) => vec![p],
        None => plan.order.clone(),
    }
    .into_iter()
    .filter_map(|c| match ctx.lib.vod.series.get(c as usize)?.kind {
        VodKind::Series(id) => Some(id),
        VodKind::Movie(_) => None,
    })
    .collect();
    match copies::next_episode(ctx.db, &ids) {
        Ok(Some((series, ep))) => play_episode(ctx, &plan, series, &ep, false, prompt),
        Ok(None) => {
            if let Some(id) = ids.first() {
                ctx.push(Route::SeriesDetail(*id));
            }
        }
        Err(e) => ctx.toast(Toast::error(format!("Couldn't play: {e}"))),
    }
    true
}

/// Play episode `ep` of the series copy `series` with the same episode of the next copy in
/// `plan` as its fallback; the resume prompt first when it has progress (`prompt`, and not
/// `from_start`).
pub fn play_episode(ctx: &mut UiCtx<'_>, plan: &CopyPlan, series: SeriesId, ep: &crate::model::Episode, from_start: bool, prompt: bool) {
    let db = ctx.db;
    let PlayerCommand::PlayCopy(mut copy) = episode_command(db, &ctx.lib.vod, plan, series, ep, !from_start, PlayOrigin::Fullscreen) else { return };
    let min = f64::from(ctx.config.player.resume_min_s);
    let item = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
    // the latest progress of this episode in any copy (matched by season and number): another
    // copy's position is where this one starts
    let ids: Vec<SeriesId> = plan
        .copies
        .iter()
        .filter_map(|c| match ctx.lib.vod.series.get(c.item as usize)?.kind {
            VodKind::Series(id) => Some(id),
            VodKind::Movie(_) => None,
        })
        .collect();
    let latest = if from_start {
        None
    } else if ids.len() > 1 {
        copies::episode_progress(db, &ids).ok().and_then(|mut m| m.remove(&(ep.season, ep.episode)))
    } else {
        crate::store::user::progress(db, &item).ok().flatten()
    };
    let latest = latest.filter(|p| !p.finished && p.position_s > 0.0);
    if let Some(p) = latest.as_ref().filter(|p| p.item != item) {
        copy.start_at = Some(p.position_s);
    }
    let p = latest.filter(|p| prompt && p.position_s > min);
    match p {
        Some(p) => {
            let name = ctx.lib.vod.find(VodKind::Series(series)).map(|v| v.name.to_string()).unwrap_or_default();
            let title = format!("{name} — S{:02}E{:02}", ep.season, ep.episode);
            let spec = ResumeSpec {
                media: copy.req.media.clone(),
                title,
                position_s: p.position_s,
                duration_s: p.duration_s,
                copy: Some(Box::new(copy)),
                queue: None,
                cast: None,
                watchlist: None,
            };
            ctx.open(OverlayRequest::Resume(spec));
        }
        None => start(ctx, copy),
    }
}

/// Start a copy fullscreen.
pub fn start(ctx: &mut UiCtx<'_>, copy: CopyPlay) {
    ctx.play(PlayerCommand::PlayCopy(copy));
    ctx.push(Route::Player);
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
    let next = plan.order.iter().copied().find(|c| *c != first);
    let fallback = next.and_then(|c| movie_media(vod, c));
    let elsewhere = elsewhere(plan, first, next, |c| vod.movies.get(c as usize).map(|v| v.source_id)).and_then(|c| movie_media(vod, c));
    // the latest progress belongs to another copy (a pin, a pick): start this one there
    let start_at = plan
        .resume()
        .filter(|c| resume && c.item != first)
        .and_then(|c| c.progress.as_ref())
        .filter(|p| !p.finished && p.position_s > 0.0)
        .map(|p| p.position_s);
    let req = PlayRequest { media: movie_media(vod, first)?, zap: None, origin, resume, immediate: true };
    Some(PlayerCommand::PlayCopy(CopyPlay { req, fallback, elsewhere, start_at }))
}

/// [`CopyPlay::elsewhere`]: when the next copy `next` is in the first copy's playlist, the
/// first copy of another playlist in play order.
fn elsewhere(plan: &CopyPlan, first: u32, next: Option<u32>, source: impl Fn(u32) -> Option<SourceId>) -> Option<u32> {
    let own = source(first);
    next.filter(|n| source(*n) == own)?;
    plan.order.iter().copied().find(|c| *c != first && source(*c) != own)
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
    let same = |c: u32| match vod.series.get(c as usize)?.kind {
        VodKind::Series(id) => copies::same_episode(conn, id, episode.season, episode.episode).ok().flatten().map(MediaRef::Episode),
        VodKind::Movie(_) => None,
    };
    let next = plan.order.iter().copied().find(|c| Some(*c) != this);
    let other = this.and_then(|t| elsewhere(plan, t, next, |c| vod.series.get(c as usize).map(|v| v.source_id)));
    let req = PlayRequest { media: MediaRef::Episode(episode.id), zap: None, origin, resume, immediate: true };
    PlayerCommand::PlayCopy(CopyPlay { req, fallback: next.and_then(same), elsewhere: other.and_then(same), start_at: None })
}

/// The toast of `PlayerEvent::FellBack`: "TV couldn't play Dune — playing from TV 2".
pub fn fell_back_toast(lib: &Library, title: &str, from: Option<SourceId>, to: Option<SourceId>) -> Toast {
    let name = |s: Option<SourceId>| s.and_then(|s| lib.source(s)).map(|m| m.name.clone());
    let text = match (name(from), name(to)) {
        (Some(f), Some(t)) => format!("{f} couldn't play {title} — playing from {t}"),
        (None, Some(t)) => format!("{title} couldn't start — playing from {t}"),
        _ => format!("{title} couldn't start — playing another copy"),
    };
    Toast { kind: ToastKind::Warning, text, action: None, also: None, ttl_ms: 5000 }
}

/// Another copy to offer on the player's error card after a failure mid-film.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherCopy {
    /// "Play from TV 2 at 41:20".
    pub label: String,
    pub play: CopyPlay,
}

/// The copy the error card offers for the failed playable in `snap`: the first of
/// [`other_copies`]. A few indexed reads: the player calls it once per failure.
pub fn other_copy(
    conn: &rusqlite::Connection,
    lib: &Library,
    plan_of: impl Fn(bool, u32) -> Option<CopyPlan>,
    snap: &PlayerSnapshot,
) -> Option<OtherCopy> {
    other_copies(conn, lib, plan_of, snap).into_iter().next()
}

/// The other copies of the title `snap` plays (a movie, or an episode that another series copy
/// has loaded), in play order, each starting where playback is ("Play from TV 2 at 41:20"):
/// the error card's offer and the player's Ctrl+O menu. Locked copies only for an unlocked
/// session (`plan_of` decides).
pub fn other_copies(
    conn: &rusqlite::Connection,
    lib: &Library,
    plan_of: impl Fn(bool, u32) -> Option<CopyPlan>,
    snap: &PlayerSnapshot,
) -> Vec<OtherCopy> {
    let Some(p) = snap.playable.as_ref() else { return Vec::new() };
    let vod = &lib.vod;
    let (movies, current, episode) = match (&p.media, &p.kind) {
        (MediaRef::Movie(id), _) => (true, vod.index_of(VodKind::Movie(*id)), None),
        (MediaRef::Episode(id), PlayableKind::Episode { season, episode }) => {
            let series = crate::store::vod::episode(conn, *id).ok().map(|e| e.series_id);
            (false, series.and_then(|s| vod.index_of(VodKind::Series(s))), Some((*season, *episode)))
        }
        _ => return Vec::new(),
    };
    let Some(plan) = current.and_then(|c| plan_of(movies, c)) else { return Vec::new() };
    let at = snap.last_position.or(snap.position).filter(|s| *s >= 1.0);
    let origin = snap.origin.unwrap_or(PlayOrigin::Fullscreen);
    plan.order
        .iter()
        .copied()
        .filter(|c| Some(*c) != current)
        .filter_map(|c| {
            let media = match episode {
                None => movie_media(vod, c)?,
                Some((season, number)) => {
                    let VodKind::Series(id) = vod.series.get(c as usize)?.kind else { return None };
                    MediaRef::Episode(copies::same_episode(conn, id, season, number).ok().flatten()?)
                }
            };
            // "TV 2", or "TV · 4K" for another copy in the same playlist
            let source = |i: u32| crate::ui::stacks::all(lib, movies).get(i as usize).map(|v| v.source_id);
            let name = match source(c) {
                Some(s) if current.and_then(source) != Some(s) => lib.source(s).map(|m| m.name.clone()).unwrap_or_default(),
                _ => crate::ui::stacks::copy_label(lib, movies, c),
            };
            let name = if name.is_empty() { "another copy".to_owned() } else { name };
            let label = match at {
                Some(s) => format!("Play from {name} at {}", crate::ui::screens::home::kit::clock(s)),
                None => format!("Play from {name}"),
            };
            let req = PlayRequest { media, zap: None, origin, resume: false, immediate: true };
            Some(OtherCopy { label, play: CopyPlay { req, fallback: None, elsewhere: None, start_at: at } })
        })
        .collect()
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

/// "`what` in TV and TV 2" (just `what` when no playlist is known).
fn in_playlists(what: &str, lib: &Library, refs: &[ItemRef]) -> String {
    match playlists(lib, refs) {
        p if p.is_empty() => what.to_owned(),
        p => format!("{what} in {p}"),
    }
}

/// ★ on a combined title (default Favourites list): add the copy that would play, or remove
/// every copy — the index is patched at once, the Undo toast follows the write.
pub fn toggle_favourite(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan) {
    toggle_favourite_of(ctx, plan, None);
}

/// [`toggle_favourite`] where `plays` is the copy that would play (a detail page's picked
/// copy) instead of the plan's first.
pub fn toggle_favourite_of(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, plays: Option<u32>) {
    if let Some(item) = plays.or(plan.first()).and_then(|i| plan.get(i)).map(|c| c.item_ref.clone())
        && item.kind != ItemKind::Channel
    {
        crate::ui::watchlists::toggle_my_list_of(ctx, &item, &plan.refs());
        return;
    }
    let Some(list) = ctx.favs.default_list() else {
        ctx.toast(Toast::error("The Favourites list is missing"));
        return;
    };
    let refs = plan.refs();
    let on = !refs.iter().any(|r| ctx.favs.is_fav(r));
    let Some(plays) = plays.or(plan.first()).or(plan.copies.first().map(|c| c.item)).and_then(|i| plan.get(i)).map(|c| c.item_ref.clone()) else {
        return;
    };
    if on {
        ctx.favs.apply_toggle(list, &plays, true);
    } else {
        refs.iter().for_each(|r| ctx.favs.apply_toggle(list, r, false));
    }
    // removing clears every copy: the toast names their playlists
    let text = if on { "Added to Favourites".to_owned() } else { in_playlists("Removed from Favourites", ctx.lib, &refs) };
    favourite_write(ctx, list, refs, plays, on, text);
}

fn favourite_write(ctx: &crate::ui::UiCtx<'_>, list: FavListId, refs: Vec<ItemRef>, plays: ItemRef, on: bool, text: String) {
    let now = ctx.now;
    write_with_undo(ctx, DataScope::Favourites, text, move |c| copies::favourite(c, list, &refs, &plays, on, now));
}

/// ✓ on a combined movie: mark the copy that would play watched, or (`on` false) forget every
/// copy's progress (a card's Remove in Continue watching).
pub fn mark_watched(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, title: &str, on: bool) {
    if !on {
        crate::ui::downloads::offer_cleanup(ctx, &plan.refs());
    }
    let text = if on { "Marked as watched" } else { "Removed from Continue watching" };
    watched_write(ctx, plan, None, title, on, text.to_owned());
}

/// [`mark_watched`] on a detail page, where `plays` is the copy that would play (its picked
/// copy); unmarking says which playlists it covered.
pub fn mark_watched_of(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, plays: Option<u32>, title: &str, on: bool) {
    let text = if on { "Marked as watched".to_owned() } else { in_playlists("Marked unwatched", ctx.lib, &plan.refs()) };
    watched_write(ctx, plan, plays, title, on, text);
}

fn watched_write(ctx: &crate::ui::UiCtx<'_>, plan: &CopyPlan, plays: Option<u32>, title: &str, on: bool, text: String) {
    let refs = plan.refs();
    let Some(plays) = plays.or(plan.first()).and_then(|i| plan.get(i)).or(plan.copies.first()).map(|c| c.item_ref.clone()) else { return };
    let (now, title) = (ctx.now, title.to_owned());
    write_with_undo(ctx, DataScope::Progress, text, move |c| copies::watched(c, &refs, (&plays, &title), on, now));
}

/// "Forget" a combined series: every episode progress of every copy.
pub fn forget_series(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan) {
    crate::ui::downloads::offer_cleanup(ctx, &plan.refs());
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

/// Ctrl+L on a combined title named `name`: every copy (the caller checked the PIN, as
/// `kit::toggle_lock`); "Locked Dune in TV and TV 2".
fn set_locked(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, name: &str, on: bool) {
    let refs = plan.refs();
    let text = in_playlists(&format!("{} {name}", if on { "Locked" } else { "Unlocked" }), ctx.lib, &refs);
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::lock(c, &refs, on));
}

/// Ctrl+L on a combined title (a grid card, a Home tile, a detail page): every copy locked
/// (its own lock or its category's) → unlock them all, else lock them all — after the PIN
/// checks of `kit::may_change_locks`; a copy locked by its category says so.
pub fn toggle_lock(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, movies: bool) {
    let all = crate::ui::stacks::all(ctx.lib, movies);
    let groups = if movies { &ctx.lib.vod.movie_groups } else { &ctx.lib.vod.series_groups };
    let copies: Vec<&crate::model::VodSummary> = plan.copies.iter().filter_map(|c| all.get(c.item as usize)).collect();
    let on = !copies.iter().all(|x| x.locked);
    let by_group = !on && copies.iter().any(|x| crate::ui::screens::home::kit::any_group_locked(groups, &x.group_ids));
    if crate::ui::screens::home::kit::may_change_locks(ctx) {
        if by_group {
            ctx.toast(Toast::info("A copy is locked by its category — unlock the category with Ctrl+L"));
        }
        let name = copies.first().map(|x| x.name.to_string()).unwrap_or_default();
        set_locked(ctx, plan, &name, on);
    }
}

/// F2 on a combined title: the rename dialog for every copy.
pub fn rename_dialog(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, name: &str) {
    ctx.open(OverlayRequest::Rename(crate::ui::overlays::RenameSpec {
        title: "Rename every copy".into(),
        initial: name.to_owned(),
        target: crate::ui::overlays::RenameTarget::Copies { items: plan.refs() },
    }));
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

/// "Same title as…" (P3): `item`'s title and `other`'s stack together from now on, with Undo.
pub fn join(ctx: &crate::ui::UiCtx<'_>, item: &ItemRef, other: &ItemRef, text: String) {
    let (item, other, now) = (item.clone(), other.clone(), ctx.now);
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::join(c, &item, &other, now));
}

/// The "Same title as…" picker for a copy (a card's or a detail page's menu).
pub fn same_title_picker(ctx: &mut crate::ui::UiCtx<'_>, item: ItemRef, name: &str) {
    ctx.open(OverlayRequest::SameTitle(crate::ui::overlays::same_title::SameTitleSpec { item, name: name.to_owned() }));
}

/// "Always play from <playlist>" on a copy of `plan` (or off), with Undo.
pub fn pin(ctx: &mut crate::ui::UiCtx<'_>, plan: &CopyPlan, item: &ItemRef, on: bool) {
    let playlist = ctx.lib.source(item.source).map_or_else(|| "this playlist".to_owned(), |s| s.name.clone());
    let text = if on { format!("Always plays from {playlist}") } else { "Plays the best copy again".to_owned() };
    let (refs, item, now) = (plan.refs(), item.clone(), ctx.now);
    write_with_undo(ctx, DataScope::Overrides, text, move |c| copies::pin(c, &item, &refs, on, now));
}

/// F2 on a combined title, once the user typed `name`: copies without their own rename get
/// it; with `overwrite` the others too (the rename overlay asks first with
/// [`copies::rename_conflicts`]).
pub fn rename(ctx: &mut crate::ui::UiCtx<'_>, refs: Vec<ItemRef>, name: &str, overwrite: bool) {
    let name = name.to_owned();
    write_with_undo(ctx, DataScope::Overrides, format!("Renamed to {name}"), move |c| copies::rename(c, &refs, &name, overwrite));
}

// ---------------------------------------------------------------------------------------------
// Series copies: episodes loaded on demand (P3)

/// Waits for the episodes of series copies that aren't loaded yet (Xtream series load them
/// lazily): [`EpisodeFetch::start`] submits `JobKind::LoadSeriesDetails` for each, and
/// [`EpisodeFetch::poll`] (every frame; a few cheap reads at most four times a second) says
/// when every one has settled — loaded, or its load finished without episodes (failed). Used
/// by the series page's "mark watched in every copy" and the delete-playlist dialog's move.
#[derive(Debug, Default)]
pub struct EpisodeFetch {
    pending: Vec<SeriesId>,
    failed: Vec<SeriesId>,
    /// How many were asked for (the progress line).
    asked: usize,
    next_check: Option<std::time::Instant>,
}

/// How often [`EpisodeFetch::poll`] looks at the jobs and the database.
const FETCH_POLL: std::time::Duration = std::time::Duration::from_millis(250);

impl EpisodeFetch {
    /// Load the episodes of `ids` (the caller passes the copies `copies::unloaded_series`
    /// named). Adds to a fetch already running.
    pub fn start(&mut self, ctx: &UiCtx<'_>, ids: &[SeriesId]) {
        if self.pending.is_empty() {
            self.failed.clear();
            self.asked = 0;
        }
        for &id in ids {
            if !self.pending.contains(&id) {
                ctx.services.jobs.submit(crate::jobs::JobKind::LoadSeriesDetails(id));
                self.pending.push(id);
                self.asked += 1;
            }
        }
        self.next_check = Some(std::time::Instant::now() + FETCH_POLL);
    }

    /// Still waiting for some copies.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// (loaded so far, asked for) — "Loading episodes of 1 of 2 copies…".
    pub fn progress(&self) -> (usize, usize) {
        (self.asked - self.pending.len(), self.asked)
    }

    /// `Some(failed)` once every copy settled (then idle again); `None` while waiting or idle.
    pub fn poll(&mut self, ctx: &UiCtx<'_>, egui: &eframe::egui::Context) -> Option<Vec<SeriesId>> {
        if self.pending.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        if self.next_check.is_some_and(|t| now < t) {
            egui.request_repaint_after(FETCH_POLL);
            return None;
        }
        self.next_check = Some(now + FETCH_POLL);
        let running: Vec<SeriesId> = ctx
            .services
            .jobs
            .active()
            .into_iter()
            .filter_map(|j| match j.kind {
                crate::jobs::JobKind::LoadSeriesDetails(id) => Some(id),
                _ => None,
            })
            .collect();
        let settled: Vec<SeriesId> = self.pending.iter().copied().filter(|id| !running.contains(id)).collect();
        if !settled.is_empty() {
            let unloaded = copies::unloaded_series(ctx.db, &settled).unwrap_or_else(|_| settled.clone());
            self.failed.extend(unloaded);
            self.pending.retain(|id| !settled.contains(id));
        }
        if self.pending.is_empty() {
            return Some(std::mem::take(&mut self.failed));
        }
        egui.request_repaint_after(FETCH_POLL);
        None
    }
}

/// ✓ / "Mark season watched" on a combined series: the episodes `eps` (season, number) of
/// every copy in `copies` are marked (or their progress forgotten), with an Undo toast naming
/// the episodes and the copies' playlists ("Marked S01 watched in TV and TV 2").
pub fn mark_episodes(ctx: &UiCtx<'_>, copies: Vec<SeriesId>, eps: Vec<(u32, u32)>, on: bool) {
    if copies.is_empty() || eps.is_empty() {
        return;
    }
    let text = format!("Marked {} {} in {}", episodes_label(&eps), if on { "watched" } else { "unwatched" }, series_playlists(ctx.lib, &copies));
    let now = ctx.now;
    write_with_undo(ctx, DataScope::Progress, text, move |c| copies::mark_episodes(c, &copies, &eps, on, now).map(|(u, _)| u));
}

/// "S01E03", "S01" (a season's episodes), or "3 episodes".
fn episodes_label(eps: &[(u32, u32)]) -> String {
    match eps {
        [(s, e)] => format!("S{s:02}E{e:02}"),
        [(s, _), rest @ ..] if rest.iter().all(|(r, _)| r == s) => format!("S{s:02}"),
        _ => format!("{} episodes", eps.len()),
    }
}

/// The playlists of the series copies `ids`, "TV and TV 2".
pub fn series_playlists(lib: &Library, ids: &[SeriesId]) -> String {
    let refs: Vec<ItemRef> = ids
        .iter()
        .filter_map(|id| lib.vod.find(VodKind::Series(*id)))
        .map(|v| ItemRef { source: v.source_id, kind: ItemKind::Series, key: v.key.clone() })
        .collect();
    playlists(lib, &refs)
}
