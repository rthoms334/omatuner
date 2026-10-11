//! In-memory catalogue snapshot used by the UI thread for every list (DESIGN §5.4).
//! Owner: WP-02 for [`load`] / [`load_favs`] (SQL). The index types and their accessors are
//! implemented in the skeleton (they run every frame in WP-11/12 code and in the player's zap
//! logic) and are built through `from_parts`, which both `load` and tests use.
//!
//! Built off the UI thread by `jobs` (`JobKind::RebuildLibrary`) from a reader connection,
//! handed to the UI as `Arc<Library>` and swapped atomically. Rebuilt after `DataScope::{Live,
//! Vod, Overrides, Epg, All}` (Epg: `ChannelSummary.epg_channel_id` is a snapshot of
//! `channel_epg`). `DataScope::History` (every zap writes history) does NOT rebuild: the app
//! re-reads only the recently watched list ([`load_recent_channels`], ≤ 500 history rows) and
//! swaps in [`Library::with_recent_channels`], which shares the (`Arc`) indexes.
//!
//! Favourites are NOT read from the snapshot by screens: the app copies `Library::favs` into
//! its own mutable `FavIndex` on every swap (`UiCtx::favs`), patches it optimistically with
//! [`FavIndex::apply_toggle`] right after submitting a write, and replaces it with
//! [`load_favs`] on `DataScope::Favourites` (event-driven, ≤ 5 ms, never per frame).
//!
//! Budget: 100k channels + 50k movies + 20k series → < 400 ms build, < 60 MB RAM.
//! Hidden items are excluded from `members`/`collection` but kept in `channels` (Settings ▸
//! Hidden lists them); locked items are flagged, not removed (the UI asks for the PIN).
//!
//! Explicit content (DESIGN §20.3) is different again: while parental control hides it, its
//! groups and items are ABSENT from the snapshot — not in `channels`, `groups`, the VOD vectors,
//! `members`, counts, `recent_channels` or the eligible playlists — so every list, count, zap
//! list, search index and lookup leaves it out without asking. [`Visibility`] is the one
//! predicate ([`load_visible`] builds with it, [`Library::hiding_explicit`] re-filters a snapshot
//! on the UI thread when the session relocks); [`ExplicitIndex`] remembers what is explicit
//! either way.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    CategoryFilter, ChannelId, ChannelSummary, Collection, Explicit, FavListId, FavouriteEntry, FavouriteList, Group, GroupFilter, GroupId, ItemKey,
    ItemKind, ItemRef, MovieId, Quality, QualityBadge, SeriesId, SortMode, SourceColor, SourceId, SourceKind, SourceSettings, Timestamp, VodKind,
    VodSummary,
};
use crate::sources::category_tags::{self, CategorySummary};
use crate::sources::vod_match::{self, VersionTags};
use crate::store::live::{GROUP_COLS, GROUP_FROM, GROUP_ORDER, group_from_row};
use crate::store::sql::{enum_parse, from_json_opt, kind_from_sql, u32_of};
use crate::store::stacks::{CopyRaw, Stacks};
use crate::store::titles::LinkAction;

/// Non-secret source facts for badges/filters.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceMeta {
    pub id: SourceId,
    pub name: String,
    pub icon: String,
    pub color: SourceColor,
    pub kind: SourceKind,
    pub enabled: bool,
    pub last_refresh: Option<Timestamp>,
    pub has_vod: bool,
    pub has_series: bool,
    /// Has channels (hidden ones count): Live TV / the guide offer it (DESIGN §29).
    pub has_live: bool,
    /// "Show in All playlists" (`SourceSettings::in_all`, DESIGN §29).
    pub in_all: bool,
    /// The source's category filter (`SourceSettings::category_filter`, DESIGN §28).
    pub category_filter: Option<CategoryFilter>,
}

impl SourceMeta {
    /// Has items of `kind` (episodes count as their series).
    pub fn has(&self, kind: ItemKind) -> bool {
        match kind {
            ItemKind::Channel => self.has_live,
            ItemKind::Movie => self.has_vod,
            ItemKind::Series | ItemKind::Episode => self.has_series,
        }
    }
}

/// What a snapshot shows of the catalogue (DESIGN §20.3) — THE visibility predicate of the
/// library build. Everything built from a snapshot (lists, counts, zap lists, search, Home,
/// P2's merged stacks) reads vectors it already filtered; code that combines copies of an item
/// itself asks [`Visibility::admits`] with [`ExplicitIndex`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visibility {
    /// Explicit (adult) groups and items are in the snapshot.
    pub explicit: bool,
}

impl Visibility {
    /// The whole catalogue (an unlocked session, or "Show explicit content" without a PIN).
    pub const ALL: Visibility = Visibility { explicit: true };
    /// Explicit content left out (parental control hides it).
    pub const NO_EXPLICIT: Visibility = Visibility { explicit: false };

    /// Does a snapshot with this visibility show an item / group that is (`explicit`) or isn't
    /// explicit content?
    pub fn admits(self, explicit: bool) -> bool {
        self.explicit || !explicit
    }
}

/// `Library::default()` (no build yet, tests) shows everything; the app's builds always go
/// through [`ExplicitGate`].
impl Default for Visibility {
    fn default() -> Self {
        Visibility::ALL
    }
}

/// The app's current [`Visibility`], shared with the jobs that build snapshots (and the
/// reminder scheduler): written by the app from the parental state on every frame and before
/// it asks for a build. Starts hidden.
#[derive(Debug, Clone, Default)]
pub struct ExplicitGate(Arc<AtomicBool>);

impl ExplicitGate {
    pub fn visibility(&self) -> Visibility {
        Visibility { explicit: self.0.load(Ordering::Acquire) }
    }
    pub fn set(&self, v: Visibility) {
        self.0.store(v.explicit, Ordering::Release);
    }
}

/// The catalogue's explicit content (DESIGN §20.3): the same whether or not the snapshot that
/// holds it shows it. An item is explicit when the provider flags it (unless one of its
/// categories is marked "Not explicit") or when one of its categories is explicit.
#[derive(Debug, Default, Clone)]
pub struct ExplicitIndex {
    /// Explicit groups of every kind.
    pub groups: HashSet<GroupId>,
    pub channels: HashSet<ChannelId>,
    pub vod: HashSet<VodKind>,
    /// The same channels, movies and series by stable reference (lookups of rows read from the
    /// database: continue watching, reminders, Settings lists, `--play`).
    pub refs: HashSet<ItemRef>,
    /// (source, kind) with at least one item that is NOT explicit (hidden ones count): the
    /// playlist keeps `SourceMeta::has` for that kind while explicit content is hidden.
    clean: HashSet<(SourceId, ItemKind)>,
}

impl ExplicitIndex {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.channels.is_empty() && self.vod.is_empty() && self.refs.is_empty()
    }
    pub fn contains(&self, item: &ItemRef) -> bool {
        self.refs.contains(item)
    }
}

#[derive(Debug, Default, Clone)]
pub struct LiveIndex {
    /// All channels of enabled sources (hidden ones included, flagged via `hidden`), in
    /// display order (source sort_index, then user order, then provider order).
    pub channels: Vec<ChannelSummary>,
    pub hidden: HashSet<ChannelId>,
    by_id: HashMap<ChannelId, u32>,
    by_ref: HashMap<(SourceId, ItemKey), ChannelId>,
    /// Channel groups, display order, overrides merged.
    pub groups: Vec<Group>,
    /// Visible members per group, display order (hidden removed).
    members: HashMap<GroupId, Vec<ChannelId>>,
    /// Visible channels per source (the scope bar's counts, DESIGN §29).
    per_source: HashMap<SourceId, usize>,
}

impl LiveIndex {
    /// Build the lookup maps. `members` may contain hidden channels; they are removed here.
    pub fn from_parts(
        channels: Vec<ChannelSummary>,
        groups: Vec<Group>,
        mut members: HashMap<GroupId, Vec<ChannelId>>,
        hidden: HashSet<ChannelId>,
    ) -> Self {
        let by_id = channels.iter().enumerate().map(|(i, c)| (c.id, i as u32)).collect();
        let by_ref = channels.iter().map(|c| ((c.source_id, c.key.clone()), c.id)).collect();
        for v in members.values_mut() {
            v.retain(|c| !hidden.contains(c));
        }
        let mut per_source: HashMap<SourceId, usize> = HashMap::new();
        for c in channels.iter().filter(|c| !hidden.contains(&c.id)) {
            *per_source.entry(c.source_id).or_default() += 1;
        }
        Self { channels, hidden, by_id, by_ref, groups, members, per_source }
    }

    pub fn get(&self, id: ChannelId) -> Option<&ChannelSummary> {
        self.by_id.get(&id).map(|i| &self.channels[*i as usize])
    }

    pub fn by_ref(&self, item: &ItemRef) -> Option<ChannelId> {
        if item.kind != ItemKind::Channel {
            return None;
        }
        self.by_ref.get(&(item.source, item.key.clone())).copied()
    }

    pub fn group_members(&self, g: GroupId) -> &[ChannelId] {
        self.members.get(&g).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Visible groups (not hidden; non-empty when `hide_empty`), optionally one source.
    pub fn visible_groups(&self, source: Option<SourceId>, hide_empty: bool) -> Vec<&Group> {
        self.groups
            .iter()
            .filter(|g| !g.hidden)
            .filter(|g| source.is_none_or(|s| g.source_id == s))
            .filter(|g| !hide_empty || !self.group_members(g.id).is_empty())
            .collect()
    }

    /// Ordered channel ids of a collection (hidden removed), sorted by `sort` (`Provider` =
    /// display order: user order then provider order, with the quality duplicates of a channel
    /// best first — see [`Self::best_quality_first`]; favourites keep list order; history
    /// keeps recency). This is also the zap order.
    pub fn collection(&self, c: &Collection, favs: &FavIndex, recent: &[ChannelId], sort: SortMode) -> Vec<ChannelId> {
        let visible = |id: &ChannelId| !self.hidden.contains(id) && self.by_id.contains_key(id);
        let mut ids: Vec<ChannelId> = match c {
            Collection::AllChannels => self.channels.iter().map(|c| c.id).filter(visible).collect(),
            Collection::Group(g) => self.group_members(*g).to_vec(),
            Collection::Favourites(l) => favs.items(*l).iter().filter_map(|r| self.by_ref(r)).filter(visible).collect(),
            Collection::History => recent.iter().copied().filter(visible).collect(),
            Collection::Adhoc(v) => v.iter().copied().filter(visible).collect(),
        };
        let key = |id: &ChannelId| self.get(*id);
        match sort {
            SortMode::Provider if matches!(c, Collection::AllChannels | Collection::Group(_)) => self.best_quality_first(&mut ids),
            SortMode::Provider | SortMode::Rating | SortMode::Year => {}
            SortMode::Number => ids.sort_by_key(|id| key(id).and_then(|c| c.number).unwrap_or(u32::MAX)),
            SortMode::NameAsc => ids.sort_by_cached_key(|id| key(id).map(|c| c.name.to_lowercase()).unwrap_or_default()),
            SortMode::NameDesc => ids.sort_by_cached_key(|id| std::cmp::Reverse(key(id).map(|c| c.name.to_lowercase()).unwrap_or_default())),
            SortMode::RecentlyAdded => ids.sort_by_key(|id| std::cmp::Reverse(key(id).and_then(|c| c.added_at).unwrap_or(i64::MIN))),
        }
        ids
    }

    /// Duplicates of one channel in several qualities ("BBC One SD / HD / 4K": same
    /// normalised name, same primary group) trade places so the best known quality comes
    /// first; they keep the slots they had between them, and every other channel — a
    /// duplicate of unknown quality, or one the user placed (`user_order`), too — keeps its
    /// own (DESIGN §13.2). Stable for equal qualities.
    fn best_quality_first(&self, ids: &mut [ChannelId]) {
        let mut families: HashMap<(u64, Option<GroupId>), Vec<usize>> = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            if let Some(c) = self.get(*id).filter(|c| c.quality.is_some() && c.user_order.is_none()) {
                families.entry((c.family, c.group_ids.first().copied())).or_default().push(i);
            }
        }
        for slots in families.values().filter(|s| s.len() > 1) {
            let mut members: Vec<ChannelId> = slots.iter().map(|&i| ids[i]).collect();
            // a doubtful claim (`4K?`) between the class beneath it and the next (after `FHD`,
            // before `HD`), so the first row is never the one the app itself doubts
            members.sort_by_key(|id| std::cmp::Reverse(self.get(*id).and_then(|c| c.quality).map(|q| 2 * q.class as i8 - 3 * i8::from(q.doubtful))));
            for (&slot, id) in slots.iter().zip(members) {
                ids[slot] = id;
            }
        }
    }

    /// Next/previous channel in `ids` relative to `current`, wrapping, skipping `skip`.
    /// `current` not in `ids` → the first (forward) / last (backward) non-skipped entry.
    pub fn neighbour(ids: &[ChannelId], current: ChannelId, forward: bool, skip: &dyn Fn(ChannelId) -> bool) -> Option<ChannelId> {
        let n = ids.len();
        if n == 0 {
            return None;
        }
        let start = ids.iter().position(|x| *x == current);
        for step in 1..=n {
            let i = match (start, forward) {
                (Some(p), true) => (p + step) % n,
                (Some(p), false) => (p + n - step % n) % n,
                (None, true) => step - 1,
                (None, false) => n - step,
            };
            if Some(i) == start {
                break;
            }
            if !skip(ids[i]) {
                return Some(ids[i]);
            }
        }
        None
    }

    /// Channel by number (numeric entry), first visible match in display order.
    pub fn by_number(&self, n: u32) -> Option<ChannelId> {
        self.channels.iter().find(|c| c.number == Some(n) && !self.hidden.contains(&c.id)).map(|c| c.id)
    }

    /// This index without the channels and groups of `x` (explicit content hidden).
    fn without(self, x: &ExplicitIndex) -> LiveIndex {
        let Self { channels, hidden, groups, mut members, .. } = self;
        members.retain(|g, _| !x.groups.contains(g));
        // the counts the rows print leave the explicit channels out too
        let mut dropped: HashMap<GroupId, u32> = HashMap::new();
        for (g, v) in members.iter_mut() {
            let before = v.len();
            v.retain(|c| !x.channels.contains(c));
            dropped.insert(*g, (before - v.len()) as u32);
        }
        LiveIndex::from_parts(
            channels.into_iter().filter(|c| !x.channels.contains(&c.id)).collect(),
            groups.into_iter().filter(|g| !x.groups.contains(&g.id)).map(|g| uncount(g, &dropped)).collect(),
            members,
            hidden.into_iter().filter(|c| !x.channels.contains(c)).collect(),
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct VodIndex {
    /// Visible (non-hidden) movies in display order (source sort_index, then the user's custom
    /// order `item_override.user_order`, then provider order — `SortMode::Provider` is this
    /// order). Settings ▸ Hidden reads hidden VOD via SQL.
    pub movies: Vec<VodSummary>,
    pub series: Vec<VodSummary>,
    pub movie_groups: Vec<Group>,
    pub series_groups: Vec<Group>,
    /// Indices into `movies` (movie groups) or `series` (series groups).
    members: HashMap<GroupId, Vec<u32>>,
    movie_idx: HashMap<MovieId, u32>,
    series_idx: HashMap<SeriesId, u32>,
    /// (movies, series) per source (the scope bar's counts, DESIGN §29).
    per_source: HashMap<SourceId, (usize, usize)>,
    /// Copies of one title in All (DESIGN §29.11); empty until [`VodIndex::with_stacks`].
    movie_stacks: Stacks,
    series_stacks: Stacks,
    /// Name sort ranks (movies, series), computed on first use and kept for the snapshot.
    name_rank: [OnceLock<Vec<u32>>; 2],
}

impl VodIndex {
    pub fn from_parts(
        movies: Vec<VodSummary>,
        series: Vec<VodSummary>,
        movie_groups: Vec<Group>,
        series_groups: Vec<Group>,
        members: HashMap<GroupId, Vec<u32>>,
    ) -> Self {
        let movie_idx = movies
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v.kind {
                VodKind::Movie(id) => Some((id, i as u32)),
                _ => None,
            })
            .collect();
        let series_idx = series
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v.kind {
                VodKind::Series(id) => Some((id, i as u32)),
                _ => None,
            })
            .collect();
        let mut per_source: HashMap<SourceId, (usize, usize)> = HashMap::new();
        for v in &movies {
            per_source.entry(v.source_id).or_default().0 += 1;
        }
        for v in &series {
            per_source.entry(v.source_id).or_default().1 += 1;
        }
        Self {
            movies,
            series,
            movie_groups,
            series_groups,
            members,
            movie_idx,
            series_idx,
            per_source,
            movie_stacks: Stacks::default(),
            series_stacks: Stacks::default(),
            name_rank: Default::default(),
        }
    }

    /// This index with the stacks of its movies and series ([`crate::store::stacks::build`]).
    pub fn with_stacks(mut self, movies: Stacks, series: Stacks) -> Self {
        self.movie_stacks = movies;
        self.series_stacks = series;
        self
    }

    /// The stacks of movies (`true`) or series: copies of one title in All (DESIGN §29.11).
    pub fn stacks(&self, movies: bool) -> &Stacks {
        if movies { &self.movie_stacks } else { &self.series_stacks }
    }

    /// Each item's place in name order (case-insensitive; equal names share it), computed
    /// once per snapshot: a name sort is then an integer sort.
    fn name_rank(&self, movies: bool) -> &[u32] {
        self.name_rank[usize::from(!movies)].get_or_init(|| {
            let all = if movies { &self.movies } else { &self.series };
            let lower: Vec<String> = all.iter().map(|v| v.name.to_lowercase()).collect();
            let mut order: Vec<u32> = (0..all.len() as u32).collect();
            order.sort_unstable_by(|a, b| lower[*a as usize].cmp(&lower[*b as usize]));
            let mut rank = vec![0u32; all.len()];
            let mut r = 0u32;
            for (k, w) in order.iter().enumerate() {
                if k > 0 && lower[*w as usize] != lower[order[k - 1] as usize] {
                    r += 1;
                }
                rank[*w as usize] = r;
            }
            rank
        })
    }

    fn sort(&self, items: &mut [u32], movies: bool, sort: SortMode) {
        let all = if movies { &self.movies } else { &self.series };
        let v = |i: &u32| &all[*i as usize];
        match sort {
            // custom order: the display order of the vectors (user order applied by `load`)
            SortMode::Provider => items.sort_unstable(),
            SortMode::Number => items.sort_by_key(|i| v(i).provider_order),
            SortMode::NameAsc => {
                let rank = self.name_rank(movies);
                items.sort_by_key(|i| rank[*i as usize]);
            }
            SortMode::NameDesc => {
                let rank = self.name_rank(movies);
                items.sort_by_key(|i| std::cmp::Reverse(rank[*i as usize]));
            }
            SortMode::RecentlyAdded => items.sort_by_key(|i| std::cmp::Reverse(v(i).added_at.unwrap_or(i64::MIN))),
            SortMode::Rating => items.sort_by(|a, b| v(b).rating.unwrap_or(-1.0).total_cmp(&v(a).rating.unwrap_or(-1.0))),
            SortMode::Year => items.sort_by_key(|i| std::cmp::Reverse(v(i).year.unwrap_or(0))),
        }
    }

    /// Indices into `movies` (for movie groups) or `series` (series groups), sorted.
    pub fn group_items(&self, g: GroupId, sort: SortMode) -> Vec<u32> {
        let movies = self.movie_groups.iter().any(|x| x.id == g);
        let mut v = self.members.get(&g).cloned().unwrap_or_default();
        self.sort(&mut v, movies, sort);
        v
    }

    pub fn all_sorted(&self, movies: bool, sort: SortMode) -> Vec<u32> {
        let n = if movies { self.movies.len() } else { self.series.len() };
        let mut v: Vec<u32> = (0..n as u32).collect();
        self.sort(&mut v, movies, sort);
        v
    }

    /// The index of a movie / series in `movies` / `series` (what [`Stacks`] speaks in).
    pub fn index_of(&self, kind: VodKind) -> Option<u32> {
        match kind {
            VodKind::Movie(id) => self.movie_idx.get(&id).copied(),
            VodKind::Series(id) => self.series_idx.get(&id).copied(),
        }
    }

    pub fn find(&self, kind: VodKind) -> Option<&VodSummary> {
        match kind {
            VodKind::Movie(id) => self.movie_idx.get(&id).map(|i| &self.movies[*i as usize]),
            VodKind::Series(id) => self.series_idx.get(&id).map(|i| &self.series[*i as usize]),
        }
    }

    /// This index without the titles and categories of `x` (explicit content hidden); group
    /// members are re-pointed at the shorter vectors.
    fn without(self, x: &ExplicitIndex) -> VodIndex {
        let Self { movies, series, movie_groups, series_groups, members, movie_stacks, series_stacks, .. } = self;
        // old index → new index, per vector
        let keep = |v: Vec<VodSummary>| -> (Vec<VodSummary>, Vec<Option<u32>>) {
            let mut map = Vec::with_capacity(v.len());
            let mut out = Vec::with_capacity(v.len());
            for item in v {
                if x.vod.contains(&item.kind) {
                    map.push(None);
                } else {
                    map.push(Some(out.len() as u32));
                    out.push(item);
                }
            }
            (out, map)
        };
        let (movies, movie_map) = keep(movies);
        let (series, series_map) = keep(series);
        let movie_group_ids: HashSet<GroupId> = movie_groups.iter().map(|g| g.id).collect();
        let mut dropped: HashMap<GroupId, u32> = HashMap::new();
        let members = members
            .into_iter()
            .filter(|(g, _)| !x.groups.contains(g))
            .map(|(g, v)| {
                let map = if movie_group_ids.contains(&g) { &movie_map } else { &series_map };
                let before = v.len();
                let v: Vec<u32> = v.into_iter().filter_map(|i| map.get(i as usize).copied().flatten()).collect();
                dropped.insert(g, (before - v.len()) as u32);
                (g, v)
            })
            .collect();
        // the counts the rows print (and hide-empty) leave the explicit titles out too
        let drop_groups = |v: Vec<Group>| v.into_iter().filter(|g| !x.groups.contains(&g.id)).map(|g| uncount(g, &dropped)).collect();
        // the stacks lose the explicit copies too: re-pointed, a stack left with one copy
        // dissolves, its top re-chosen (removing copies never joins titles)
        let movie_stacks = movie_stacks.retain(&movie_map, &movies);
        let series_stacks = series_stacks.retain(&series_map, &series);
        VodIndex::from_parts(movies, series, drop_groups(movie_groups), drop_groups(series_groups), members).with_stacks(movie_stacks, series_stacks)
    }
}

/// `g` without the explicit members a hidden snapshot dropped from it (`dropped`, per group).
fn uncount(mut g: Group, dropped: &HashMap<GroupId, u32>) -> Group {
    g.item_count = g.item_count.saturating_sub(dropped.get(&g.id).copied().unwrap_or(0));
    g
}

#[derive(Debug, Default, Clone)]
pub struct FavIndex {
    /// Lists in user order.
    pub lists: Vec<FavouriteList>,
    items: HashMap<FavListId, Vec<ItemRef>>,
    membership: HashMap<ItemRef, Vec<FavListId>>,
}

impl FavIndex {
    /// Orphaned entries are skipped (they are listed in Settings ▸ Orphans).
    pub fn from_parts(mut lists: Vec<FavouriteList>, mut entries: Vec<FavouriteEntry>) -> Self {
        lists.sort_by_key(|l| (l.sort_index, l.id));
        entries.sort_by_key(|e| (e.list, e.position));
        let mut f = Self { lists, ..Default::default() };
        for e in entries.into_iter().filter(|e| e.orphaned_since.is_none()) {
            f.membership.entry(e.item.clone()).or_default().push(e.list);
            f.items.entry(e.list).or_default().push(e.item);
        }
        f
    }

    pub fn items(&self, list: FavListId) -> &[ItemRef] {
        self.items.get(&list).map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn lists_of(&self, item: &ItemRef) -> &[FavListId] {
        self.membership.get(item).map(Vec::as_slice).unwrap_or(&[])
    }
    /// Member of the default list (what Ctrl+F toggles and the ★ glyph shows).
    pub fn is_fav(&self, item: &ItemRef) -> bool {
        self.default_list().is_some_and(|d| self.lists_of(item).contains(&d))
    }
    pub fn default_list(&self) -> Option<FavListId> {
        self.lists.iter().find(|l| l.is_default).map(|l| l.id)
    }
    /// Non-empty lists in order (Guide default collection, Home rails).
    pub fn non_empty_lists(&self) -> impl Iterator<Item = &FavouriteList> {
        self.lists.iter().filter(|l| !self.items(l.id).is_empty())
    }
    pub fn is_empty(&self) -> bool {
        self.items.values().all(Vec::is_empty)
    }

    /// Optimistic local update after `user::set_favourite` was submitted (appends at the end).
    pub fn apply_toggle(&mut self, list: FavListId, item: &ItemRef, on: bool) {
        let items = self.items.entry(list).or_default();
        let lists = self.membership.entry(item.clone()).or_default();
        if on {
            if !items.contains(item) {
                items.push(item.clone());
            }
            if !lists.contains(&list) {
                lists.push(list);
            }
        } else {
            items.retain(|x| x != item);
            lists.retain(|l| *l != list);
        }
    }
}

/// The whole snapshot.
#[derive(Debug, Default)]
pub struct Library {
    pub sources: Vec<SourceMeta>,
    /// Shared between the snapshots of one build (see [`Library::with_recent_channels`]).
    pub live: Arc<LiveIndex>,
    pub vod: Arc<VodIndex>,
    /// Favourites at build time — the app copies this into `TunerApp.favs` on swap; screens use
    /// `UiCtx::favs` (see module docs), never this field.
    pub favs: FavIndex,
    /// Distinct recently watched channels, most recent first (`Collection::History`, Home).
    pub recent_channels: Vec<ChannelId>,
    pub built_at: Timestamp,
    /// Unique per snapshot (a build or a [`Library::with_recent_channels`] copy; 0 = the empty
    /// default): what screens compare to notice a swap. Never the `Arc` address — a dropped
    /// snapshot's allocation is often reused by the next one.
    pub generation: u64,
    /// What this snapshot shows (DESIGN §20.3).
    pub visibility: Visibility,
    /// The catalogue's explicit content, shown or not (shared between copies of a build).
    pub explicit: Arc<ExplicitIndex>,
}

/// Identity of a snapshot's VOD lists: a zap's copy ([`Library::with_recent_channels`]) shares
/// it, a rebuild or an explicit-content hide doesn't; indices into the lists stay valid while
/// it is equal. Holds a `Weak`, so the address can't be reused while it is compared.
#[derive(Debug, Clone)]
pub struct VodIdentity(std::sync::Weak<VodIndex>);

impl PartialEq for VodIdentity {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Weak::ptr_eq(&self.0, &other.0)
    }
}

impl Library {
    pub fn vod_identity(&self) -> VodIdentity {
        VodIdentity(Arc::downgrade(&self.vod))
    }

    pub fn source(&self, id: SourceId) -> Option<&SourceMeta> {
        self.sources.iter().find(|s| s.id == id)
    }
    /// The playlists a screen of `kind` can show (DESIGN §29): enabled and with items of that
    /// kind (movies, series, channels), in playlist order. "Is there more than one playlist?"
    /// means `eligible(kind).len() > 1` — never `sources.len()`, which counts disabled
    /// playlists and free-channel presets without movies.
    pub fn eligible(&self, kind: ItemKind) -> Vec<&SourceMeta> {
        self.sources.iter().filter(|s| s.enabled && s.has(kind)).collect()
    }
    /// Visible (not hidden) items of `kind` from `source` in this snapshot.
    pub fn count(&self, kind: ItemKind, source: SourceId) -> usize {
        match kind {
            ItemKind::Channel => self.live.per_source.get(&source).copied().unwrap_or(0),
            ItemKind::Movie => self.vod.per_source.get(&source).map_or(0, |c| c.0),
            ItemKind::Series | ItemKind::Episode => self.vod.per_source.get(&source).map_or(0, |c| c.1),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
    pub fn has_vod(&self) -> bool {
        !self.vod.movies.is_empty()
    }
    pub fn has_series(&self) -> bool {
        !self.vod.series.is_empty()
    }

    /// This catalogue with another recently-watched list: the indexes are shared, not copied,
    /// and `built_at` is kept (screens keyed on it don't rebuild for a zap); `generation` is new
    /// (screens showing the recent list do).
    pub fn with_recent_channels(&self, recent_channels: Vec<ChannelId>) -> Library {
        Library {
            sources: self.sources.clone(),
            live: self.live.clone(),
            vod: self.vod.clone(),
            favs: self.favs.clone(),
            recent_channels,
            built_at: self.built_at,
            generation: next_generation(),
            visibility: self.visibility,
            explicit: self.explicit.clone(),
        }
    }

    /// The snapshot holds explicit content right now (shows it, and there is some).
    pub fn shows_explicit(&self) -> bool {
        self.visibility.explicit && !self.explicit.is_empty()
    }

    /// `item` (a channel, movie or series) is explicit content this snapshot leaves out: rows
    /// read from the database (continue watching, reminders, Settings lists, `--play`) check
    /// it before they show or play anything.
    ///
    /// Fails closed: before the first build (`built_at` 0, nothing known yet) every item is
    /// concealed, so no row read from the database flashes up at startup.
    pub fn conceals(&self, item: &ItemRef) -> bool {
        self.built_at == 0 || (!self.visibility.explicit && self.explicit.contains(item))
    }

    /// This snapshot without its explicit content: the UI thread's re-filter when the parental
    /// session relocks, so nothing explicit stays on screen until a rebuild lands (a copy of
    /// the indexes, tens of ms at the size budget). A new `generation` and `built_at`, so every
    /// screen rebuilds from it.
    pub fn hiding_explicit(&self) -> Library {
        let copy = Library {
            sources: self.sources.clone(),
            live: self.live.clone(),
            vod: self.vod.clone(),
            favs: self.favs.clone(),
            recent_channels: self.recent_channels.clone(),
            built_at: crate::model::now().max(self.built_at + 1),
            generation: next_generation(),
            visibility: self.visibility,
            explicit: self.explicit.clone(),
        };
        copy.with_visibility(Visibility::NO_EXPLICIT)
    }

    /// This snapshot with the stacks of the movies and series it shows (`raw`: their match
    /// data, same order): copies of the playlists in All stack (DESIGN §29.11). The build's
    /// last step, after [`Library::with_visibility`], so hidden explicit copies never join.
    fn stacked(mut self, movie_raw: Vec<CopyRaw>, series_raw: Vec<CopyRaw>, prefs: &[String]) -> Library {
        let vod = Arc::try_unwrap(std::mem::take(&mut self.vod)).unwrap_or_else(|a| (*a).clone());
        let out_movies = out_of_all(&self.sources, ItemKind::Movie);
        let out_series = out_of_all(&self.sources, ItemKind::Series);
        let movies = crate::store::stacks::build(&vod.movies, movie_raw, |s| !out_movies.contains(&s), prefs);
        let series = crate::store::stacks::build(&vod.series, series_raw, |s| !out_series.contains(&s), prefs);
        self.vod = Arc::new(vod.with_stacks(movies, series));
        self
    }

    /// Apply `v` to a snapshot that shows everything (the build's last step).
    fn with_visibility(mut self, v: Visibility) -> Library {
        self.visibility = v;
        if v.explicit || self.explicit.is_empty() {
            return self;
        }
        let x = self.explicit.clone();
        let live = Arc::try_unwrap(std::mem::take(&mut self.live)).unwrap_or_else(|a| (*a).clone());
        let vod = Arc::try_unwrap(std::mem::take(&mut self.vod)).unwrap_or_else(|a| (*a).clone());
        self.live = Arc::new(live.without(&x));
        self.vod = Arc::new(vod.without(&x));
        self.recent_channels.retain(|c| !x.channels.contains(c));
        for s in &mut self.sources {
            s.has_live &= x.clean.contains(&(s.id, ItemKind::Channel));
            s.has_vod &= x.clean.contains(&(s.id, ItemKind::Movie));
            s.has_series &= x.clean.contains(&(s.id, ItemKind::Series));
        }
        self
    }
}

fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Recently watched channels kept in the snapshot (`Collection::History`, Home rail).
const RECENT_CHANNELS: usize = 50;

fn load_sources(conn: &Connection) -> Result<Vec<SourceMeta>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, s.name, s.icon, s.color, s.kind, s.enabled, s.last_refresh,
                EXISTS (SELECT 1 FROM movie m WHERE m.source_id = s.id), EXISTS (SELECT 1 FROM series x WHERE x.source_id = s.id),
                s.settings, EXISTS (SELECT 1 FROM channel c WHERE c.source_id = s.id)
         FROM source s ORDER BY s.sort_index, s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(4)?;
        let settings = from_json_opt::<SourceSettings>(r.get(9)?);
        Ok(SourceMeta {
            id: r.get(0)?,
            name: r.get(1)?,
            icon: r.get(2)?,
            color: enum_parse(&r.get::<_, String>(3)?).unwrap_or_default(),
            kind: enum_parse(&kind).unwrap_or(SourceKind::M3uUrl),
            enabled: r.get(5)?,
            last_refresh: r.get(6)?,
            has_vod: r.get(7)?,
            has_series: r.get(8)?,
            has_live: r.get(10)?,
            in_all: settings.as_ref().and_then(|st| st.in_all).unwrap_or(true),
            category_filter: settings.and_then(|st| st.category_filter),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Groups of every kind of enabled sources, in display order per kind, with each source's
/// category filter applied (`hidden` is effective; DESIGN §28.4).
fn load_groups(conn: &Connection, sources: &[SourceMeta]) -> Result<(Vec<Group>, Vec<Group>, Vec<Group>)> {
    let rules: HashMap<SourceId, &CategoryFilter> = sources.iter().filter_map(|s| Some((s.id, s.category_filter.as_ref()?))).collect();
    let mut stmt = conn.prepare_cached(&format!("SELECT {GROUP_COLS} {GROUP_FROM} WHERE s.enabled = 1 ORDER BY g.kind, {GROUP_ORDER}"))?;
    let (mut ch, mut mv, mut se) = (Vec::new(), Vec::new(), Vec::new());
    for g in stmt.query_map([], group_from_row)? {
        let mut g = g?;
        let rule = rules.get(&g.source_id).copied();
        apply_category_filter(&mut g, rule);
        match g.kind {
            ItemKind::Channel => ch.push(g),
            ItemKind::Movie => mv.push(g),
            ItemKind::Series => se.push(g),
            ItemKind::Episode => {}
        }
    }
    Ok((ch, mv, se))
}

/// Apply a source's category filter to one of its groups, loaded with its override flags
/// (`user_hidden`; `filter` = `Exempt` when the override has `shown`, else `Kept`).
/// Precedence (DESIGN §28.3): the user's hide > the user's show > the rule. Afterwards
/// `hidden` is the effective flag and `filter` says what the rule did.
pub fn apply_category_filter(g: &mut Group, filter: Option<&CategoryFilter>) {
    let rule_hides = filter.is_some_and(|f| !f.keeps(g.tag.as_ref()));
    g.filter = match (rule_hides, g.filter) {
        (false, _) => GroupFilter::Kept,
        (true, GroupFilter::Exempt) => GroupFilter::Exempt,
        (true, _) => GroupFilter::Hidden,
    };
    g.hidden = g.user_hidden || g.filter == GroupFilter::Hidden;
}

/// Per-tag summary of one source's categories (all kinds, the source enabled or not) for the
/// "Choose what to show" picker (DESIGN §28.6). Reads `grp` only (≤ a few thousand rows): run
/// it off the UI thread (a `Loader`), never per frame. `hide_explicit`: leave explicit
/// categories out of the totals (DESIGN §20.3).
pub fn category_summary(conn: &Connection, source: SourceId, hide_explicit: bool) -> Result<CategorySummary> {
    let mut stmt = conn.prepare_cached(
        "SELECT g.kind, COALESCE(g.raw_name, g.name), g.item_count, g.raw_name IS NULL FROM grp g
         LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
         WHERE g.source_id = ?1 AND NOT (?2 AND COALESCE(o.explicit, g.explicit > 0) = 1)",
    )?;
    let rows: Vec<(ItemKind, String, u32, bool)> = stmt
        .query_map(rusqlite::params![source, hide_explicit], |r| {
            Ok((kind_from_sql(&r.get::<_, String>(0)?)?, r.get(1)?, u32_of(r.get(2)?), r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(CategorySummary {
        tags: category_tags::summarize(rows.iter().map(|(kind, name, n, _)| (*kind, name.as_str(), *n))),
        needs_refresh: rows.iter().any(|r| r.3),
    })
}

/// Per group: what its items inherit.
#[derive(Debug, Clone, Copy)]
struct GroupFlags {
    /// Display position.
    pos: usize,
    hidden: bool,
    locked: bool,
    explicit: Explicit,
}

fn group_flags(groups: &[Group]) -> HashMap<GroupId, GroupFlags> {
    groups.iter().enumerate().map(|(i, g)| (g.id, GroupFlags { pos: i, hidden: g.hidden, locked: g.locked, explicit: g.explicit })).collect()
}

/// Group memberships `item id → [(group, primary)]` from a link table, restricted to groups
/// in `flags` (enabled sources).
fn load_memberships(conn: &Connection, sql: &str, flags: &HashMap<GroupId, GroupFlags>) -> Result<HashMap<i64, Vec<(GroupId, bool)>>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut out: HashMap<i64, Vec<(GroupId, bool)>> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, GroupId>(1)?, r.get::<_, bool>(2)?)))? {
        let (item, g, primary) = row?;
        if flags.contains_key(&g) {
            out.entry(item).or_default().push((g, primary));
        }
    }
    Ok(out)
}

/// What an item inherits from its groups.
struct Inherited {
    /// Primary first, then display order.
    groups: Vec<GroupId>,
    /// Every group is hidden.
    all_hidden: bool,
    any_locked: bool,
    /// One of its groups is explicit content.
    any_explicit: bool,
    /// One of its groups is marked "Not explicit" (overrides the provider's flag on the item).
    any_cleared: bool,
}

impl Inherited {
    /// The item is explicit content (DESIGN §20.3): one of its groups is, or the provider flags
    /// the item (`own`) and the user didn't say "Not explicit" about one of its groups.
    fn explicit(&self, own: bool) -> bool {
        self.any_explicit || (own && !self.any_cleared)
    }
}

fn effective_groups(links: Option<&Vec<(GroupId, bool)>>, flags: &HashMap<GroupId, GroupFlags>) -> Inherited {
    let Some(links) = links else {
        return Inherited { groups: Vec::new(), all_hidden: false, any_locked: false, any_explicit: false, any_cleared: false };
    };
    let mut v: Vec<&(GroupId, bool)> = links.iter().collect();
    v.sort_by_key(|(g, primary)| (!*primary, flags.get(g).map_or(usize::MAX, |f| f.pos)));
    let flag = |g: &GroupId| flags.get(g).copied();
    Inherited {
        all_hidden: !v.is_empty() && v.iter().all(|(g, _)| flag(g).is_some_and(|f| f.hidden)),
        any_locked: v.iter().any(|(g, _)| flag(g).is_some_and(|f| f.locked)),
        any_explicit: v.iter().any(|(g, _)| flag(g).is_some_and(|f| f.explicit.is_explicit())),
        any_cleared: v.iter().any(|(g, _)| flag(g).is_some_and(|f| f.explicit == Explicit::Cleared)),
        groups: v.into_iter().map(|(g, _)| *g).collect(),
    }
}

fn load_live(conn: &Connection, groups: Vec<Group>, x: &mut ExplicitIndex) -> Result<LiveIndex> {
    let flags = group_flags(&groups);
    let trust = crate::store::live::label_trust(conn)?;
    let links = load_memberships(conn, "SELECT cg.channel_id, cg.group_id, cg.is_primary FROM channel_group cg", &flags)?;
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.source_id, c.key, COALESCE(o.rename, c.clean_name), c.number, c.logo, ce.epg_channel_id, c.tvg_shift_min,
                c.catchup IS NOT NULL, COALESCE(json_extract(c.catchup, '$.days'), 0), c.radio, c.unsupported IS NOT NULL,
                c.provider_order, o.user_order, c.added_at, COALESCE(o.hidden, 0), COALESCE(o.locked, 0),
                c.quality, v.width, v.height, COALESCE(c.family, c.norm_name), c.name, o.rename IS NOT NULL, c.explicit
         FROM channel c JOIN source s ON s.id = c.source_id AND s.enabled = 1
         LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
         LEFT JOIN channel_epg ce ON ce.channel_id = c.id
         LEFT JOIN channel_video v ON v.source_id = c.source_id AND v.item_key = c.key AND v.name = c.name
                   AND (v.url_key IS NULL OR c.url_key IS NULL OR v.url_key = c.url_key)
         ORDER BY s.sort_index, s.id, (o.user_order IS NULL), o.user_order, c.provider_order, c.id",
    )?;
    let mut channels = Vec::new();
    let mut hidden = HashSet::new();
    let mut members: HashMap<GroupId, Vec<ChannelId>> = HashMap::new();
    let mut renamed = Vec::new();
    let rows = stmt.query_map([], |r| {
        let radio: bool = r.get(10)?;
        let name: String = r.get(3)?;
        let provider_name = match r.get_ref(21)? {
            rusqlite::types::ValueRef::Text(raw) if raw != name.as_bytes() => Some(Arc::from(String::from_utf8_lossy(raw))),
            _ => None,
        };
        Ok((
            ChannelSummary {
                id: r.get(0)?,
                source_id: r.get(1)?,
                key: ItemKey(r.get(2)?),
                name: Arc::from(name),
                provider_name,
                number: r.get(4)?,
                logo: r.get::<_, Option<String>>(5)?.map(Arc::from),
                group_ids: Vec::new(),
                epg_channel_id: r.get(6)?,
                tvg_shift_min: r.get(7)?,
                has_catchup: r.get(8)?,
                catchup_days: u32_of(r.get(9)?),
                radio,
                locked: r.get(16)?,
                unsupported: r.get(11)?,
                provider_order: r.get(12)?,
                user_order: r.get(13)?,
                added_at: r.get(14)?,
                // measured beats the name, whose claim is doubtful when the source's claims of
                // that class measured lower too often; radio has none (a cover picture is no video)
                quality: {
                    let claimed = r.get::<_, Option<i64>>(17)?.and_then(Quality::from_code);
                    let source: SourceId = r.get(1)?;
                    match (r.get::<_, Option<i64>>(18)?, r.get::<_, Option<i64>>(19)?) {
                        (Some(w), Some(h)) => Quality::of_video(w, h),
                        _ => None,
                    }
                    .map(|m| QualityBadge::measured(m, claimed))
                    .or_else(|| claimed.map(|c| QualityBadge::named(c, trust.get(&source).is_some_and(|t| t.doubts(c)))))
                    .filter(|_| !radio)
                },
                family: text_hash(r.get_ref(20)?),
            },
            r.get::<_, bool>(15)?,
            r.get::<_, bool>(22)?,
            r.get::<_, bool>(23)?,
        ))
    })?;
    for row in rows {
        let (mut c, item_hidden, is_renamed, own_explicit) = row?;
        renamed.push(is_renamed);
        let inherited = effective_groups(links.get(&c.id.0), &flags);
        c.locked |= inherited.any_locked;
        if item_hidden || inherited.all_hidden {
            hidden.insert(c.id);
        }
        if inherited.explicit(own_explicit) {
            x.channels.insert(c.id);
            x.refs.insert(ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() });
        } else {
            x.clean.insert((c.source_id, ItemKind::Channel));
        }
        for g in &inherited.groups {
            members.entry(*g).or_default().push(c.id);
        }
        c.group_ids = inherited.groups;
        channels.push(c);
    }
    distinguish_channels(&mut channels, &renamed);
    Ok(LiveIndex::from_parts(channels, groups, members, hidden))
}

/// Channels of one category that would show the same name and badge although the provider
/// names differ get what smart cleanup dropped (`FI: TV5` / `FI: TV5 ᴿᴬᵂ` → `TV5` / `TV5 ·
/// RAW`; `PL:` / `AF: CANAL+ SPORT 5` → `… · PL` / `… · AF`). Renamed channels keep the user's
/// name. Display only: keys, families and the stored names are unchanged.
fn distinguish_channels(channels: &mut [ChannelSummary], renamed: &[bool]) {
    type Seen = (SourceId, Option<GroupId>, Arc<str>, &'static str);
    let mut sets: HashMap<Seen, Vec<usize>> = HashMap::new();
    for (i, c) in channels.iter().enumerate().filter(|(i, _)| !renamed[*i]) {
        sets.entry((c.source_id, c.group_ids.first().copied(), c.name.clone(), c.quality.map_or("", |q| q.label()))).or_default().push(i);
    }
    for set in sets.into_values().filter(|s| s.len() > 1) {
        let raws: Vec<&str> = set.iter().map(|&i| channels[i].provider_name.as_deref().unwrap_or(&channels[i].name)).collect();
        let suffixes = crate::sources::smart_names::distinguishing_suffixes(&raws, false);
        for (i, suffix) in set.into_iter().zip(suffixes) {
            if let Some(suffix) = suffix {
                channels[i].name = Arc::from(format!("{} · {suffix}", channels[i].name));
            }
        }
    }
}

/// Stable-within-a-build hash of a text column (no allocation per row).
fn text_hash(v: rusqlite::types::ValueRef<'_>) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    if let rusqlite::types::ValueRef::Text(b) = v {
        b.hash(&mut h);
    }
    h.finish()
}

/// Group id → indices into the matching `VodIndex` vector.
type VodMembers = HashMap<GroupId, Vec<u32>>;

/// What the library build is told besides the database (`Default` = English, the whole
/// catalogue shown).
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// The user's audio languages, most preferred first (`player.audio_langs`; any ISO 639
    /// form): the top card of a stack is a copy in one of them (DESIGN §29.11). Empty = `en`.
    pub audio_langs: Vec<String>,
    /// What the snapshot shows (DESIGN §20.3) — **the one visibility predicate for stacks
    /// too**: while explicit content is hidden, explicit copies are left out before the stacks
    /// are built, so they are never listed, counted, searched or stacked (DESIGN §29.11), and
    /// [`Library::hiding_explicit`] drops them from the stacks of a snapshot on relock.
    pub visibility: Visibility,
}

impl LoadOptions {
    /// The options the app builds with.
    pub fn from_config(c: &crate::config::Config, visibility: Visibility) -> Self {
        Self { audio_langs: c.player.audio_langs.clone(), visibility }
    }

    /// The audio languages as `vod_match::norm_lang` codes, deduplicated (`en` when none).
    pub fn lang_prefs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for l in self.audio_langs.iter().map(|l| vod_match::norm_lang(l)).filter(|l| !l.is_empty()) {
            if !out.contains(&l) {
                out.push(l);
            }
        }
        if out.is_empty() {
            out.push("en".to_owned());
        }
        out
    }
}

/// The playlists "All playlists" leaves out for `kind`: eligible ones with "Show in All
/// playlists" off — none when that would leave All empty (DESIGN §29.3; `ui::scope` and the
/// stacks use it).
pub fn out_of_all(sources: &[SourceMeta], kind: ItemKind) -> Vec<SourceId> {
    out_of_all_of(&sources.iter().filter(|s| s.enabled && s.has(kind)).collect::<Vec<_>>())
}

/// [`out_of_all`] of the eligible playlists of a kind.
pub fn out_of_all_of(eligible: &[&SourceMeta]) -> Vec<SourceId> {
    let out: Vec<SourceId> = eligible.iter().filter(|s| !s.in_all).map(|s| s.id).collect();
    if eligible.len() < 2 || out.len() == eligible.len() { Vec::new() } else { out }
}

/// Visible movies or series + group members (indices into the returned vector) + their match
/// data (same order).
fn load_vod(conn: &Connection, movies: bool, groups: &[Group], x: &mut ExplicitIndex) -> Result<(Vec<VodSummary>, VodMembers, Vec<CopyRaw>)> {
    let flags = group_flags(groups);
    let (table, link, col, kind, poster) =
        if movies { ("movie", "movie_group", "movie_id", "movie", "poster") } else { ("series", "series_group", "series_id", "series", "cover") };
    let item_kind = if movies { ItemKind::Movie } else { ItemKind::Series };
    let links = load_memberships(conn, &format!("SELECT l.{col}, l.group_id, 0 FROM {link} l"), &flags)?;
    let split = crate::store::titles::linked_ids(conn, item_kind, LinkAction::Split)?;
    let pinned = crate::store::titles::linked_ids(conn, item_kind, LinkAction::Pin)?;
    let joins = crate::store::titles::join_groups(conn, item_kind)?;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT x.id, x.source_id, x.key, COALESCE(o.rename, x.clean_name), x.year, x.{poster}, x.rating, x.added_at,
                x.provider_order, COALESCE(o.locked, 0), COALESCE(o.hidden, 0),
                x.match_hash, x.match_lang, x.match_tags, x.match_origin, x.tmdb_id, x.explicit
         FROM {table} x JOIN source s ON s.id = x.source_id AND s.enabled = 1
         LEFT JOIN item_override o ON o.source_id = x.source_id AND o.kind = '{kind}' AND o.item_key = x.key
         ORDER BY s.sort_index, s.id, (o.user_order IS NULL), o.user_order, x.provider_order, x.id"
    ))?;
    let mut items = Vec::new();
    let mut raws = Vec::new();
    let mut members = VodMembers::new();
    // `match_lang` values are few: one allocation each
    let mut langs: HashMap<Vec<u8>, Arc<str>> = HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let item_hidden: bool = r.get(10)?;
        let own_explicit: bool = r.get(16)?;
        let inherited = effective_groups(links.get(&id), &flags);
        let kind = if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) };
        let source_id: SourceId = r.get(1)?;
        let key = ItemKey(r.get(2)?);
        if inherited.explicit(own_explicit) {
            x.vod.insert(kind);
            x.refs.insert(ItemRef { source: source_id, kind: item_kind, key: key.clone() });
        } else {
            x.clean.insert((source_id, item_kind));
        }
        if item_hidden || inherited.all_hidden {
            continue;
        }
        let mut v = VodSummary {
            kind,
            source_id,
            key,
            name: Arc::from(r.get::<_, String>(3)?),
            year: r.get(4)?,
            poster: r.get::<_, Option<String>>(5)?.map(Arc::from),
            rating: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
            group_ids: inherited.groups,
            added_at: r.get(7)?,
            provider_order: r.get(8)?,
            locked: r.get::<_, bool>(9)? || inherited.any_locked,
        };
        let lang = match r.get_ref(12)? {
            rusqlite::types::ValueRef::Text(t) if !t.is_empty() => {
                Some(langs.entry(t.to_vec()).or_insert_with(|| Arc::from(String::from_utf8_lossy(t).as_ref())).clone())
            }
            _ => None,
        };
        let tmdb = match r.get_ref(15)? {
            rusqlite::types::ValueRef::Integer(n) => u32::try_from(n).ok().filter(|n| *n > 0),
            rusqlite::types::ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(vod_match::tmdb_id),
            _ => None,
        };
        let origin = match r.get_ref(14)? {
            rusqlite::types::ValueRef::Text([a, b]) => Some([*a, *b]),
            _ => None,
        };
        raws.push(CopyRaw {
            hash: r.get(11)?,
            lang,
            tags: VersionTags(r.get::<_, Option<i64>>(13)?.and_then(|t| u16::try_from(t).ok()).unwrap_or(0)),
            origin,
            tmdb,
            split: split.contains(&id),
            pinned: pinned.contains(&id),
            join: joins.get(&id).copied(),
        });
        let idx = items.len() as u32;
        for g in &v.group_ids {
            members.entry(*g).or_default().push(idx);
        }
        v.group_ids.shrink_to_fit();
        items.push(v);
    }
    Ok((items, members, raws))
}

/// Build a full snapshot from a reader connection (uses the `from_parts` constructors) with
/// the default [`LoadOptions`]: English, everything shown, explicit content included (the
/// app's builds pass [`LoadOptions::from_config`] with its [`ExplicitGate`]). Never call on the
/// UI thread.
///
/// Only enabled sources contribute channels/VOD/groups (`sources` lists every source).
/// Effective flags: an item is hidden when it is hidden itself or every one of its groups is
/// hidden; it is locked when it or any of its groups is locked; it is explicit as
/// [`ExplicitIndex`] says. Movies and series of the playlists in All stack (DESIGN §29.11).
pub fn load(conn: &Connection) -> Result<Library> {
    load_with(conn, &LoadOptions::default())
}

/// [`load`], then leave out what `visibility` hides (DESIGN §20.3).
pub fn load_visible(conn: &Connection, visibility: Visibility) -> Result<Library> {
    load_with(conn, &LoadOptions { visibility, ..LoadOptions::default() })
}

/// [`load`] with options.
pub fn load_with(conn: &Connection, opts: &LoadOptions) -> Result<Library> {
    let sources = load_sources(conn)?;
    let (ch_groups, movie_groups, series_groups) = load_groups(conn, &sources)?;
    let mut explicit = ExplicitIndex {
        groups: ch_groups.iter().chain(&movie_groups).chain(&series_groups).filter(|g| g.explicit.is_explicit()).map(|g| g.id).collect(),
        ..Default::default()
    };
    let live = load_live(conn, ch_groups, &mut explicit)?;
    let (movies, mut members, movie_raw) = load_vod(conn, true, &movie_groups, &mut explicit)?;
    let (series, series_members, series_raw) = load_vod(conn, false, &series_groups, &mut explicit)?;
    members.extend(series_members);
    disabled_explicit_refs(conn, &mut explicit)?;
    // the match data of what the snapshot will show (`with_visibility` keeps the order)
    let shown = |items: &[VodSummary], raw: Vec<CopyRaw>| -> Vec<CopyRaw> {
        if opts.visibility.explicit || explicit.vod.is_empty() {
            return raw;
        }
        items.iter().zip(raw).filter(|(v, _)| !explicit.vod.contains(&v.kind)).map(|(_, r)| r).collect()
    };
    let (movie_raw, series_raw) = (shown(&movies, movie_raw), shown(&series, series_raw));
    let vod = VodIndex::from_parts(movies, series, movie_groups, series_groups, members);
    let favs = load_favs(conn)?;
    let recent_channels = load_recent_channels(conn, &live)?;
    let lib = Library {
        sources,
        live: Arc::new(live),
        vod: Arc::new(vod),
        favs,
        recent_channels,
        built_at: crate::model::now(),
        generation: next_generation(),
        visibility: Visibility::ALL,
        explicit: Arc::new(explicit),
    };
    Ok(lib.with_visibility(opts.visibility).stacked(movie_raw, series_raw, &opts.lang_prefs()))
}

/// Explicit channels, movies and series of DISABLED playlists into `x.refs` (the enabled ones
/// came with `load_live` / `load_vod`): rows read straight from the database — continue
/// watching, reminders, Settings lists — don't filter by enabled playlist, and must not name
/// an explicit title of a playlist switched off ([`Library::conceals`]). The predicate is
/// `store::live::explicit_row`'s, set-based.
fn disabled_explicit_refs(conn: &Connection, x: &mut ExplicitIndex) -> Result<()> {
    for (table, link, col, kind) in [
        ("channel", "channel_group", "channel_id", ItemKind::Channel),
        ("movie", "movie_group", "movie_id", ItemKind::Movie),
        ("series", "series_group", "series_id", ItemKind::Series),
    ] {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT x.source_id, x.key FROM {table} x JOIN source s ON s.id = x.source_id AND s.enabled = 0
             WHERE EXISTS (SELECT 1 FROM {link} l JOIN grp g ON g.id = l.group_id
                             LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
                           WHERE l.{col} = x.id AND COALESCE(o.explicit, g.explicit > 0) = 1)
                OR (x.explicit = 1 AND NOT EXISTS (SELECT 1 FROM {link} l JOIN grp g ON g.id = l.group_id
                             JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
                           WHERE l.{col} = x.id AND o.explicit = 0))"
        ))?;
        let rows = stmt.query_map([], |r| Ok(ItemRef { source: r.get(0)?, kind, key: ItemKey(r.get(1)?) }))?;
        for r in rows {
            x.refs.insert(r?);
        }
    }
    Ok(())
}

/// Distinct recently watched channels of `live`, most recent first (one aggregate over the
/// ≤ 500-row history table: cheap enough for the UI reader on `DataScope::History`).
pub fn load_recent_channels(conn: &Connection, live: &LiveIndex) -> Result<Vec<ChannelId>> {
    Ok(crate::store::user::recent_channels(conn, RECENT_CHANNELS)?.iter().filter_map(|h| live.by_ref(&h.item)).collect())
}

/// Rebuild only the favourites part (cheap; event-driven on the UI thread, < 5 ms).
pub fn load_favs(conn: &Connection) -> Result<FavIndex> {
    let lists = crate::store::user::fav_lists(conn)?;
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {} FROM fav_item ORDER BY list_id, position, added_at", crate::store::user::FAV_ITEM_COLS))?;
    let entries = stmt.query_map([], crate::store::user::fav_entry_from_row)?.collect::<rusqlite::Result<Vec<FavouriteEntry>>>()?;
    Ok(FavIndex::from_parts(lists, entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ch(id: i64, n: u32, name: &str) -> ChannelSummary {
        ChannelSummary {
            id: ChannelId(id),
            source_id: SourceId(1),
            key: ItemKey(format!("k{id}")),
            name: Arc::from(name),
            provider_name: None,
            number: Some(n),
            logo: None,
            group_ids: vec![GroupId(1)],
            epg_channel_id: None,
            tvg_shift_min: 0,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: id as i32,
            user_order: None,
            added_at: Some(id),
            quality: None,
            family: id as u64,
        }
    }

    #[test]
    fn collections_hide_sort_and_zap() {
        let chans = vec![ch(1, 3, "Charlie"), ch(2, 1, "alpha"), ch(3, 2, "Bravo")];
        let members = HashMap::from([(GroupId(1), vec![ChannelId(1), ChannelId(2), ChannelId(3)])]);
        let live = LiveIndex::from_parts(chans, vec![], members, HashSet::from([ChannelId(3)]));
        let favs = FavIndex::default();
        let g = Collection::Group(GroupId(1));
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Provider), vec![ChannelId(1), ChannelId(2)]);
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Number), vec![ChannelId(2), ChannelId(1)]);
        assert_eq!(live.collection(&Collection::AllChannels, &favs, &[], SortMode::NameAsc), vec![ChannelId(2), ChannelId(1)]);
        assert_eq!(live.by_number(2), None, "hidden");
        let ids = [ChannelId(1), ChannelId(2), ChannelId(3)];
        let skip2 = |c: ChannelId| c == ChannelId(2);
        assert_eq!(LiveIndex::neighbour(&ids, ChannelId(1), true, &skip2), Some(ChannelId(3)));
        assert_eq!(LiveIndex::neighbour(&ids, ChannelId(1), false, &|_| false), Some(ChannelId(3)));
        assert_eq!(LiveIndex::neighbour(&ids, ChannelId(1), true, &|_| true), None);
    }

    /// Default sort: the SD / HD / 4K duplicates of a channel trade places best first, inside
    /// the slots they occupy; other channels (and an untagged duplicate) don't move. Other
    /// sorts and favourites keep their order.
    #[test]
    fn duplicates_sort_best_quality_first() {
        let dup = |id: i64, q: Option<Quality>| ChannelSummary {
            quality: q.map(|q| QualityBadge::named(q, false)),
            family: 7,
            ..ch(id, id as u32, "BBC One")
        };
        let chans = vec![
            dup(1, Some(Quality::Sd)),
            ch(2, 2, "ITV"),
            dup(3, None),
            dup(4, Some(Quality::Uhd)),
            dup(5, Some(Quality::Hd)),
            ChannelSummary { group_ids: vec![GroupId(2)], ..dup(6, Some(Quality::Fhd)) },
        ];
        let ids = |v: &[i64]| v.iter().map(|i| ChannelId(*i)).collect::<Vec<_>>();
        let members = HashMap::from([(GroupId(1), ids(&[1, 2, 3, 4, 5])), (GroupId(2), ids(&[6]))]);
        let live = LiveIndex::from_parts(chans, vec![], members, HashSet::new());
        let favs = FavIndex::default();
        let g = Collection::Group(GroupId(1));
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Provider), ids(&[4, 2, 3, 5, 1]));
        // all channels: 6 is the same name in another group, so not a duplicate
        assert_eq!(live.collection(&Collection::AllChannels, &favs, &[], SortMode::Provider), ids(&[4, 2, 3, 5, 1, 6]));
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Number), ids(&[1, 2, 3, 4, 5]));
        assert_eq!(live.collection(&Collection::History, &favs, &ids(&[1, 4]), SortMode::Provider), ids(&[1, 4]));
        // a custom order (Alt+↑/↓ wrote `user_order`) is the user's: no reshuffle
        let placed: Vec<ChannelSummary> = live.channels.iter().map(|c| ChannelSummary { user_order: Some(c.id.0 as i32), ..c.clone() }).collect();
        let members = HashMap::from([(GroupId(1), ids(&[1, 2, 3, 4, 5]))]);
        let live = LiveIndex::from_parts(placed, vec![], members, HashSet::new());
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Provider), ids(&[1, 2, 3, 4, 5]));
        // a doubtful `4K?` goes after an FHD copy, before an HD one
        let doubted = |id: i64, q: Quality, doubtful: bool| ChannelSummary { quality: Some(QualityBadge::named(q, doubtful)), ..dup(id, None) };
        let chans = vec![doubted(1, Quality::Hd, false), doubted(2, Quality::Uhd, true), doubted(3, Quality::Fhd, false)];
        let live = LiveIndex::from_parts(chans, vec![], HashMap::from([(GroupId(1), ids(&[1, 2, 3]))]), HashSet::new());
        assert_eq!(live.collection(&g, &favs, &[], SortMode::Provider), ids(&[3, 2, 1]));
    }

    #[test]
    fn favourites_toggle() {
        let list = FavouriteList { id: FavListId(1), name: "Favourites".into(), icon: String::new(), sort_index: 0, is_default: true };
        let mut f = FavIndex::from_parts(vec![list], vec![]);
        let item = ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("k1".into()) };
        f.apply_toggle(FavListId(1), &item, true);
        assert!(f.is_fav(&item));
        f.apply_toggle(FavListId(1), &item, false);
        assert!(!f.is_fav(&item));
    }
}
