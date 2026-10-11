//! The UI side of stacks: copies of one movie or series in "All playlists" shown as one card
//! (DESIGN §29.14). The library builds the stacks (`store::stacks`); this module turns a
//! screen's list into cards, says what a card shows and which copy plays.
//!
//! * [`collapse`]: each stack once, where its first copy comes in the list (the list's sort
//!   holds), as its **representative**: the stack's top card when the list has it (All), else
//!   the first copy of the list in copy order, unlocked first (a category, Favourites, Continue
//!   watching: the copy that is in that list). [`fan_out`] puts the other copies right after
//!   an open stack's card.
//! * Lists are collapsed only in "All playlists" and with `interface.same_title = combine`
//!   ([`combine`]); one playlist never collapses (it shows "+1" for a title that is also
//!   elsewhere, [`elsewhere`]).
//! * [`play_order`]: the copies of a title in the order they are tried (`stacks::play_order`
//!   with what the UI knows: the latest progress, [`trouble`] and the container).
//! * [`trouble`] is the playback track's hook (a copy that failed recently, an account at its
//!   connection limit or expired): it answers `None` until the player records failures.

use std::collections::HashSet;

use crate::config::{Config, SameTitle};
use crate::keymap::Action;
use crate::model::{ItemKind, ItemRef, SourceId, Timestamp, VodKind, VodSummary, WatchProgress};
use crate::msg::{DataScope, Toast, ToastKind};
use crate::sources::vod_match::VersionTags;
use crate::store::library::Library;
use crate::store::stacks::{self, CopyFacts, StackId, Stacks};
use crate::ui::UiCtx;

/// Settings ▸ Interface ▸ "Same title in several playlists" is Combine.
pub(crate) fn combine(c: &Config) -> bool {
    c.interface.same_title == SameTitle::Combine
}

pub(crate) fn is_movie(kind: VodKind) -> bool {
    matches!(kind, VodKind::Movie(_))
}

/// The movies (`true`) or series of the snapshot.
pub(crate) fn all(lib: &Library, movies: bool) -> &[VodSummary] {
    if movies { &lib.vod.movies } else { &lib.vod.series }
}

/// The index of a movie or series in its kind's vector (what stacks are keyed by).
pub(crate) fn index_of(lib: &Library, kind: VodKind) -> Option<u32> {
    let all = all(lib, is_movie(kind));
    let v = lib.vod.find(kind)?;
    // `find` hands out an element of that vector: its index from its address
    let at = (v as *const VodSummary as usize).checked_sub(all.as_ptr() as usize)? / std::mem::size_of::<VodSummary>().max(1);
    (at < all.len()).then_some(at as u32)
}

/// The stack of a movie or series while copies combine (Home, Search: views of everything).
pub(crate) fn stack_of_kind(lib: &Library, config: &Config, kind: VodKind) -> Option<StackId> {
    if !combine(config) {
        return None;
    }
    index_of(lib, kind).and_then(|i| lib.vod.stacks(is_movie(kind)).stack_of(i))
}

/// The copies of a title, `item` first when it stands alone: its stack's members in copy
/// order while combining, else `[item]`.
pub(crate) fn copies_of(lib: &Library, config: &Config, movies: bool, item: u32) -> Vec<u32> {
    if !combine(config) {
        return vec![item];
    }
    lib.vod.stacks(movies).copies(item)
}

/// Which items a list holds: a flag per item for long lists, a set for short ones (Favourites
/// are recomputed every frame: no work proportional to the catalogue there).
enum Present {
    Dense(Vec<bool>),
    Sparse(HashSet<u32>),
}

impl Present {
    fn of(items: &[u32], n_all: usize) -> Self {
        if items.len().saturating_mul(16) < n_all {
            Present::Sparse(items.iter().copied().collect())
        } else {
            let mut v = vec![false; n_all];
            for &i in items {
                if let Some(x) = v.get_mut(i as usize) {
                    *x = true;
                }
            }
            Present::Dense(v)
        }
    }
    fn has(&self, i: u32) -> bool {
        match self {
            Present::Dense(v) => v.get(i as usize).copied().unwrap_or(false),
            Present::Sparse(s) => s.contains(&i),
        }
    }
}

/// The card a stack gets in a list holding `present`: its top when the list has it, else the
/// list's first copy in copy order, unlocked first.
fn representative(stacks: &Stacks, s: StackId, all: &[VodSummary], present: &Present) -> u32 {
    let top = stacks.top(s);
    if present.has(top) {
        return top;
    }
    let members = stacks.members(s);
    let locked = |i: u32| all.get(i as usize).is_some_and(|v| v.locked);
    members.iter().copied().find(|i| present.has(*i) && !locked(*i)).or_else(|| members.iter().copied().find(|i| present.has(*i))).unwrap_or(top)
}

/// A list of "All playlists" with each stack once (module docs). O(list + its stacks' copies).
pub(crate) fn collapse(stacks: &Stacks, items: &[u32], all: &[VodSummary]) -> Vec<u32> {
    if stacks.is_empty() || items.len() < 2 {
        return items.to_vec();
    }
    let present = Present::of(items, all.len());
    // stacks already placed: a flag per stack for long lists, a set for short ones
    let dense = matches!(present, Present::Dense(_));
    let mut seen_flags = if dense { vec![false; stacks.len()] } else { Vec::new() };
    let mut seen_set: HashSet<StackId> = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for &i in items {
        match stacks.stack_of(i) {
            Some(s) => {
                let first = match seen_flags.get_mut(s.0 as usize) {
                    Some(f) => !std::mem::replace(f, true),
                    None => seen_set.insert(s),
                };
                if first {
                    out.push(representative(stacks, s, all, &present));
                }
            }
            None => out.push(i),
        }
    }
    out
}

/// A collapsed list with the stacks in `open` fanned out: the stack's card, then its other
/// copies in copy order.
pub(crate) fn fan_out(stacks: &Stacks, collapsed: &[u32], open: &[StackId]) -> Vec<u32> {
    if open.is_empty() {
        return collapsed.to_vec();
    }
    let mut out = Vec::with_capacity(collapsed.len());
    for &i in collapsed {
        out.push(i);
        if let Some(s) = stacks.stack_of(i).filter(|s| open.contains(s)) {
            out.extend(stacks.members(s).iter().copied().filter(|m| *m != i));
        }
    }
    out
}

/// The distinct playlists of some copies, in playlist order (Settings ▸ Playlists).
pub(crate) fn playlists_of(stacks: &Stacks, copies: &[u32], all: &[VodSummary]) -> Vec<SourceId> {
    let mut out: Vec<SourceId> = Vec::new();
    for &c in copies {
        if let Some(v) = all.get(c as usize)
            && !out.contains(&v.source_id)
        {
            out.push(v.source_id);
        }
    }
    out.sort_by_key(|s| stacks.playlist_rank(*s));
    out
}

/// The other playlists a title is in, for "+1 also in TV" inside one playlist (none while
/// copies aren't combined).
pub(crate) fn elsewhere(config: &Config, stacks: &Stacks, item: u32, all: &[VodSummary]) -> Vec<SourceId> {
    if !combine(config) {
        return Vec::new();
    }
    stacks.elsewhere(item, all)
}

/// What a stack card shows of its stack.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CardStack {
    pub id: StackId,
    /// Copies in the stack (the badge).
    pub count: usize,
    /// Their playlists, in playlist order.
    pub playlists: Vec<SourceId>,
    /// Every copy is locked: the card is drawn locked (dimmed while the session is locked).
    pub all_locked: bool,
}

/// The stack of a card, if it heads one.
pub(crate) fn card_stack(stacks: &Stacks, item: u32, all: &[VodSummary]) -> Option<CardStack> {
    let s = stacks.stack_of(item)?;
    let members = stacks.members(s);
    Some(CardStack {
        id: s,
        count: members.len(),
        playlists: playlists_of(stacks, members, all),
        all_locked: members.iter().all(|m| all.get(*m as usize).is_some_and(|v| v.locked)),
    })
}

/// Why a copy plays last ([`trouble`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // made by the playback track's hook (CROSS-TRACK), read by the Copies section
pub(crate) enum TroubleKind {
    /// It failed to play recently.
    Failed,
    /// Its account is at its connection limit.
    AtLimit,
    /// Its account has expired.
    Expired,
}

/// A copy in trouble since `at` ("failed 2 h ago").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Trouble {
    pub kind: TroubleKind,
    pub at: Timestamp,
}

/// **Playback hook** (CROSS-TRACK): a copy that failed recently, or whose account is at its
/// connection limit or expired, plays last and its mini-card says so. Nothing records copy
/// failures yet, so no copy is in trouble.
pub(crate) fn trouble(_lib: &Library, _item: &VodSummary) -> Option<Trouble> {
    None
}

/// The copies of a title in the order they are tried (`stacks::play_order`): `last_played`
/// and `container_ok` come from the caller (progress, the movie rows), trouble from
/// [`trouble`]. Locked copies are left out unless `unlocked`.
pub(crate) fn play_order(
    lib: &Library,
    config: &Config,
    movies: bool,
    copies: &[u32],
    unlocked: bool,
    last_played: impl Fn(u32) -> Option<Timestamp>,
    container_ok: impl Fn(u32) -> Option<bool>,
) -> Vec<u32> {
    let all = all(lib, movies);
    let stacks = lib.vod.stacks(movies);
    let facts: Vec<CopyFacts> = copies
        .iter()
        .map(|&c| CopyFacts {
            last_played: last_played(c),
            trouble: all.get(c as usize).is_some_and(|v| trouble(lib, v).is_some()),
            container_ok: container_ok(c),
            ..stacks.facts(c, all)
        })
        .collect();
    stacks::play_order(&facts, unlocked, config.interface.copy_order)
}

/// "4K HDR", "CAM", "3D" of a copy (`None` when its name says nothing).
pub(crate) fn tags_label(tags: VersionTags) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(q) = tags.quality() {
        parts.push(q.label());
    }
    for (flag, word) in [(VersionTags::HDR, "HDR"), (VersionTags::DV, "DV"), (VersionTags::THREE_D, "3D"), (VersionTags::CAM, "CAM")] {
        if tags.has(flag) {
            parts.push(word);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A copy's language and version, "EN · 4K HDR", for chips and menus (`None`: nothing known).
pub(crate) fn copy_facts_label(stacks: &Stacks, item: u32) -> Option<String> {
    let parts: Vec<String> = [stacks.lang_label(item), tags_label(stacks.meta(item).tags)].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// "TV · EN · 4K" (the playlist, then [`copy_facts_label`]): a copy in "Play from…".
pub(crate) fn copy_label(lib: &Library, movies: bool, item: u32) -> String {
    let all = all(lib, movies);
    let stacks = lib.vod.stacks(movies);
    let playlist = all.get(item as usize).and_then(|v| lib.source(v.source_id)).map(|s| s.name.clone()).unwrap_or_default();
    match copy_facts_label(stacks, item) {
        Some(f) if !playlist.is_empty() => format!("{playlist} · {f}"),
        Some(f) => f,
        None => playlist,
    }
}

/// The durable reference of a copy.
pub(crate) fn item_ref(v: &VodSummary) -> ItemRef {
    crate::ui::screens::home::vod_ref(v)
}

/// A change a toast offers to undo ([`undo`]).
pub(crate) enum Undo {
    /// "Not the same title": join the copy to its stack again.
    Split(ItemRef),
    /// Every copy taken out of Favourites: put them back.
    Favourites(Vec<ItemRef>),
    /// Progress forgotten on every copy: the rows, once the write has read them.
    Progress(crate::store::Ticket<Vec<WatchProgress>>),
}

fn undo_toast(text: String) -> Toast {
    Toast { kind: ToastKind::Info, text, action: Some(("Undo".into(), Action::Undo(0))), ttl_ms: 8000 }
}

/// "Not the same title" (DESIGN §29.14): the copy leaves its stack for good (a refresh keeps
/// it apart), with an Undo toast. The library rebuilds on the write.
pub(crate) fn split(ctx: &mut UiCtx<'_>, item: &ItemRef, name: &str) -> Undo {
    write_split(ctx, item, true);
    ctx.toast(undo_toast(format!("{name} is shown on its own now")));
    Undo::Split(item.clone())
}

fn write_split(ctx: &UiCtx<'_>, item: &ItemRef, on: bool) {
    let (item, now) = (item.clone(), ctx.now);
    let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::titles::set_split(c, &item, on, now));
}

/// "Always play from this copy" (`on`; one pin per stack: the other copies' go) or not.
pub(crate) fn set_pin(ctx: &mut UiCtx<'_>, item: &ItemRef, others: Vec<ItemRef>, on: bool, playlist: &str) {
    let (write_item, now) = (item.clone(), ctx.now);
    let _ = ctx.services.store.writer().submit_notify(DataScope::Overrides, move |c| crate::store::titles::set_pin(c, &write_item, &others, on, now));
    ctx.toast(Toast::info(if on { format!("Always plays from {playlist}") } else { "Plays from the best copy again".to_owned() }));
}

/// Ctrl+F on a stack card (DESIGN §29.12): when no copy is a favourite, the copy that would
/// play becomes one; otherwise every copy leaves Favourites, with an Undo.
pub(crate) fn toggle_favourite(ctx: &mut UiCtx<'_>, copies: &[ItemRef], plays: &ItemRef) -> Option<Undo> {
    let favs: Vec<ItemRef> = copies.iter().filter(|r| ctx.favs.is_fav(r)).cloned().collect();
    if favs.is_empty() {
        crate::ui::screens::home::kit::toggle_favourite(ctx, plays);
        return None;
    }
    let list = ctx.favs.default_list()?;
    for r in &favs {
        crate::ui::screens::home::kit::set_in_list(ctx, list, r, false);
    }
    let text = if favs.len() == 1 { "Removed from Favourites".to_owned() } else { format!("Removed {} copies from Favourites", favs.len()) };
    ctx.toast(undo_toast(text));
    Some(Undo::Favourites(favs))
}

/// Forget the progress of every copy (movies; for series, of their episodes) — "Remove from
/// Continue watching" on a stack card — with an Undo that restores the rows.
pub(crate) fn forget_progress(ctx: &mut UiCtx<'_>, copies: Vec<(ItemRef, VodKind)>) -> Undo {
    let now = ctx.now;
    let ticket = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| {
        let mut items: Vec<(ItemRef, String)> = Vec::new();
        for (r, kind) in copies {
            match kind {
                VodKind::Series(id) => {
                    for e in crate::store::vod::episodes(c, id, None)? {
                        items.push((ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }, e.title));
                    }
                }
                VodKind::Movie(_) => items.push((r, String::new())),
            }
        }
        let mut saved = Vec::new();
        for (r, _) in &items {
            if let Some(p) = crate::store::user::progress(c, r)? {
                saved.push(p);
            }
        }
        crate::store::user::mark_watched(c, &items, false, now)?;
        Ok(saved)
    });
    ctx.toast(undo_toast("Removed from Continue watching".to_owned()));
    Undo::Progress(ticket)
}

/// A toast's Undo.
pub(crate) fn undo(ctx: &mut UiCtx<'_>, u: Undo) {
    match u {
        Undo::Split(item) => {
            write_split(ctx, &item, false);
            ctx.toast(Toast::info("Combined again"));
        }
        Undo::Favourites(items) => {
            let Some(list) = ctx.favs.default_list() else { return };
            for r in &items {
                crate::ui::screens::home::kit::set_in_list(ctx, list, r, true);
            }
            ctx.toast(Toast::info("Back in Favourites"));
        }
        Undo::Progress(mut ticket) => match ticket.try_take() {
            Some(Ok(rows)) => {
                let _ = ctx.services.store.writer().submit_notify(DataScope::Progress, move |c| {
                    for p in &rows {
                        crate::store::user::save_progress(c, p)?;
                    }
                    Ok(())
                });
                ctx.toast(Toast::info("Back in Continue watching"));
            }
            _ => ctx.toast(Toast::error("Couldn't undo that")),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::CopyOrder;
    use crate::model::{GroupId, ItemKey, ItemKind, MovieId, Quality};
    use crate::sources::vod_match::{self, PrefixCodes};
    use crate::store::stacks::CopyRaw;

    /// Copies (source, provider name, locked) as the library would hold them, with their stacks.
    fn lib(rows: &[(i64, &str, bool)]) -> (Vec<VodSummary>, Stacks) {
        let codes = PrefixCodes::default();
        let (items, raw): (Vec<VodSummary>, Vec<CopyRaw>) = rows
            .iter()
            .enumerate()
            .map(|(i, (src, name, locked))| {
                let m = vod_match::parse(name, None, &codes);
                let v = VodSummary {
                    kind: VodKind::Movie(MovieId(i as i64)),
                    source_id: SourceId(*src),
                    key: ItemKey(format!("k{i}")),
                    name: Arc::from(*name),
                    year: m.year,
                    poster: None,
                    rating: None,
                    group_ids: vec![GroupId(*src)],
                    added_at: None,
                    provider_order: i as i32,
                    locked: *locked,
                };
                let raw = CopyRaw {
                    hash: m.stack_hash(ItemKind::Movie),
                    lang: m.lang_text().map(Arc::from),
                    tags: m.tags,
                    origin: m.origin,
                    ..Default::default()
                };
                (v, raw)
            })
            .unzip();
        let stacks = stacks::build(&items, raw, |_| true, &["en".to_owned()]);
        (items, stacks)
    }

    /// Dune in three copies (French first in the provider's order, a locked English copy, an
    /// English 4K copy in the second playlist), Heat alone.
    fn dune() -> (Vec<VodSummary>, Stacks) {
        lib(&[
            (1, "FR ★ Dune - 2021", false),          // 0
            (1, "EN ★ Dune - 2021", true),           // 1 locked
            (2, "EN - Dune (2021) [4K HDR]", false), // 2 top
            (2, "EN - Heat (1995)", false),          // 3
        ])
    }

    /// All: a stack is one card, its top, where its first copy comes — the list's sort holds.
    /// A list without the top (one playlist's category, Favourites) shows its own copy,
    /// unlocked first; fanning out puts the other copies after the card in copy order.
    #[test]
    fn collapse_picks_the_card_and_fan_out_follows_the_copy_order() {
        let (all, s) = dune();
        assert_eq!(collapse(&s, &[0, 1, 2, 3], &all), vec![2, 3], "All: the top, at the stack's first place");
        assert_eq!(collapse(&s, &[3, 0, 2], &all), vec![3, 2], "sorted: Heat first");
        assert_eq!(collapse(&s, &[0, 1, 3], &all), vec![0, 3], "playlist 1's category: its unlocked copy");
        assert_eq!(collapse(&s, &[1, 3], &all), vec![1, 3], "only a locked copy in the list: that one");
        let dune = s.stack_of(0).unwrap();
        assert_eq!(fan_out(&s, &[2, 3], &[dune]), vec![2, 1, 0, 3], "the card, then English before French");
        assert_eq!(fan_out(&s, &[0, 3], &[dune]), vec![0, 1, 2, 3], "a category's card leads its fan");
        assert_eq!(fan_out(&s, &[2, 3], &[]), vec![2, 3]);
        // a short list of a big catalogue (Favourites, every frame) uses a set, not a flag per title
        for (n_all, sparse) in [(4, false), (100, true)] {
            let p = Present::of(&[1, 3], n_all);
            assert_eq!(matches!(p, Present::Sparse(_)), sparse);
            assert!(p.has(1) && p.has(3) && !p.has(2) && !p.has(99));
        }
    }

    /// A stack card: its copy count, its playlists in playlist order, locked only when every
    /// copy is.
    #[test]
    fn card_stacks() {
        let (all, s) = dune();
        let c = card_stack(&s, 2, &all).unwrap();
        assert_eq!((c.count, c.playlists.clone(), c.all_locked), (3, vec![SourceId(1), SourceId(2)], false));
        assert_eq!(card_stack(&s, 3, &all), None, "Heat stands alone");
        let (all, s) = lib(&[(1, "EN ★ Alien - 1979", true), (2, "EN - Alien (1979)", true)]);
        assert!(card_stack(&s, 0, &all).unwrap().all_locked);
    }

    /// Labels: language (inferred and dub markers included) and version tags.
    #[test]
    fn copy_labels() {
        let (_, s) = dune();
        assert_eq!(copy_facts_label(&s, 2).as_deref(), Some("EN · 4K HDR"));
        assert_eq!(copy_facts_label(&s, 0).as_deref(), Some("FR"));
        let mut t = VersionTags::default();
        assert_eq!(tags_label(t), None);
        t.0 |= VersionTags::CAM;
        assert_eq!(tags_label(t).as_deref(), Some("CAM"));
        assert_eq!(Quality::Uhd.label(), "4K", "the quality pill's word");
    }

    /// Which copy plays, as the UI asks: progress beats language and playlist order, locked
    /// copies only with the session unlocked, the copy order setting decides between equal
    /// copies.
    #[test]
    fn play_order_with_progress_locks_and_the_setting() {
        let (all, s) = dune();
        let mut library = Library::default();
        let vod = crate::store::library::VodIndex::from_parts(all, Vec::new(), Vec::new(), Vec::new(), Default::default())
            .with_stacks(s, Stacks::default());
        library.vod = Arc::new(vod);
        let mut config = Config::default();
        let copies = library.vod.stacks(true).copies(2);
        let order = |cfg: &Config, unlocked: bool, played: Option<u32>| {
            play_order(&library, cfg, true, &copies, unlocked, |c| (Some(c) == played).then_some(100), |_| None)
        };
        assert_eq!(order(&config, false, None), vec![2, 0], "English first; the locked copy is skipped");
        assert_eq!(order(&config, true, None), vec![1, 2, 0], "unlocked: playlist order between the English copies");
        assert_eq!(order(&config, false, Some(0)), vec![0, 2], "the copy you were watching");
        config.interface.copy_order = CopyOrder::BestQuality;
        assert_eq!(order(&config, true, None), vec![2, 1, 0], "4K first, still within the language");
        config.interface.same_title = SameTitle::ShowEach;
        assert_eq!(copies_of(&library, &config, true, 2), vec![2], "show each copy: a title is its own copy");
        assert_eq!(index_of(&library, VodKind::Movie(MovieId(3))), Some(3));
        assert_eq!(index_of(&library, VodKind::Movie(MovieId(9))), None);
    }
}
