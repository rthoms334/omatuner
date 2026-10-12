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
//! Budget: 100k channels + 50k movies + 20k series → < 400 ms build, < 60 MB RAM; a two-playlist
//! catalogue of 80k channels, 300k movies and 40k series with a category rule → < 550 ms
//! (`benches/startup.rs`). The item tables are read in id order, without joins or sorts in SQL
//! (an `ORDER BY` carried every column of every row through SQLite's sorter, hidden rows too),
//! merged with their group links (`Links`) and the user's overrides by lookup, and put in
//! display order in memory; a hidden title is dropped before the rest of its row is read.
//! [`load_profiled`] times the steps; a build over a second logs them.
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
    /// The category rule that applies to the source (DESIGN §28.3): its own, else the shared one.
    pub category_filter: Option<CategoryFilter>,
    /// The source has its own rule ("Use a different rule for this playlist"), not the shared one.
    pub own_category_filter: bool,
    /// Movies and series of the source that "Also hide titles in other languages" left out of
    /// this snapshot (DESIGN §28.12).
    pub language_hidden: u32,
    /// Its short code on cards and rows ("NW", DESIGN §13.0): the user's
    /// (`SourceSettings::code`) or derived from the name, unique among the playlists; resolved
    /// once per build (`sources::codes::assign`).
    pub code: String,
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

    /// Add what another part of the same build found (the channels, read on their own thread).
    fn merge(&mut self, other: ExplicitIndex) {
        self.groups.extend(other.groups);
        self.channels.extend(other.channels);
        self.vod.extend(other.vod);
        self.refs.extend(other.refs);
        self.clean.extend(other.clean);
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
    let shared = crate::store::sources::shared_category_filter(conn)?;
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, s.name, s.icon, s.color, s.kind, s.enabled, s.last_refresh,
                EXISTS (SELECT 1 FROM movie m WHERE m.source_id = s.id), EXISTS (SELECT 1 FROM series x WHERE x.source_id = s.id),
                s.settings, EXISTS (SELECT 1 FROM channel c WHERE c.source_id = s.id)
         FROM source s ORDER BY s.sort_index, s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(4)?;
        let settings = from_json_opt::<SourceSettings>(r.get(9)?);
        let user_code = settings.as_ref().and_then(|st| st.code.clone());
        Ok((
            user_code,
            SourceMeta {
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
                own_category_filter: settings.as_ref().is_some_and(|st| st.has_own_category_filter()),
                language_hidden: 0,
                category_filter: match &settings {
                    Some(st) => st.effective_category_filter(shared.as_ref()).cloned(),
                    None => shared.clone(),
                },
                code: String::new(),
            },
        ))
    })?;
    let rows: Vec<(Option<String>, SourceMeta)> = rows.collect::<rusqlite::Result<_>>()?;
    let codes = crate::sources::codes::assign(rows.iter().map(|(c, s)| (s.id, s.name.as_str(), c.as_deref())));
    Ok(rows.into_iter().zip(codes).map(|((_, s), code)| SourceMeta { code, ..s }).collect())
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
    category_summary_of(conn, &[source], hide_explicit)
}

/// [`category_summary`] of several sources together (the shared rule's playlists, DESIGN §28.3).
pub fn category_summary_of(conn: &Connection, sources: &[SourceId], hide_explicit: bool) -> Result<CategorySummary> {
    let mut stmt = conn.prepare_cached(
        "SELECT g.kind, COALESCE(g.raw_name, g.name), g.item_count, g.raw_name IS NULL FROM grp g
         LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
         WHERE g.source_id = ?1 AND NOT (?2 AND COALESCE(o.explicit, g.explicit > 0) = 1)",
    )?;
    let mut rows: Vec<(ItemKind, String, u32, bool)> = Vec::new();
    for source in sources {
        let part = stmt.query_map(rusqlite::params![source, hide_explicit], |r| {
            Ok((kind_from_sql(&r.get::<_, String>(0)?)?, r.get(1)?, u32_of(r.get(2)?), r.get(3)?))
        })?;
        for r in part {
            rows.push(r?);
        }
    }
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

/// One group membership row, with what the item inherits from the group.
#[derive(Debug, Clone, Copy)]
struct Link {
    item: i64,
    group: GroupId,
    primary: bool,
    flags: GroupFlags,
}

/// The group memberships of a link table (groups of enabled sources only), sorted by item id
/// and handed out in that order ([`Links::of`]): the item tables are read in id order too, so
/// the two merge without a map from item to groups (300k small vectors and a hash per item at
/// the catalogue sizes of DESIGN §28.12).
struct Links {
    rows: Vec<Link>,
    at: usize,
}

impl Links {
    /// `sql` selects (item id, group id, primary).
    fn load(conn: &Connection, sql: &str, flags: &HashMap<GroupId, GroupFlags>) -> Result<Links> {
        let mut stmt = conn.prepare_cached(sql)?;
        let mut rows = Vec::new();
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let group: GroupId = r.get(1)?;
            if let Some(f) = flags.get(&group) {
                rows.push(Link { item: r.get(0)?, group, primary: r.get(2)?, flags: *f });
            }
        }
        rows.sort_unstable_by_key(|l| l.item);
        Ok(Links { rows, at: 0 })
    }

    /// The memberships of `item`; items must be asked for in ascending id order (the ones
    /// between are skipped).
    fn of(&mut self, item: i64) -> &[Link] {
        while self.rows.get(self.at).is_some_and(|l| l.item < item) {
            self.at += 1;
        }
        let start = self.at;
        while self.rows.get(self.at).is_some_and(|l| l.item == item) {
            self.at += 1;
        }
        &self.rows[start..self.at]
    }
}

/// The user's overrides of one kind of movies or series (`item_override`; few rows) by
/// playlist and key, looked up per item instead of a join per row.
#[derive(Default)]
struct Overrides(HashMap<SourceId, HashMap<String, ItemOverride>>);

struct ItemOverride {
    rename: Option<String>,
    user_order: Option<i64>,
    hidden: bool,
    locked: bool,
}

impl Overrides {
    fn load(conn: &Connection, kind: &str) -> Result<Overrides> {
        let mut stmt = conn.prepare_cached("SELECT source_id, item_key, rename, user_order, hidden, locked FROM item_override WHERE kind = ?1")?;
        let mut out = Overrides::default();
        let mut q = stmt.query([kind])?;
        while let Some(r) = q.next()? {
            let o = ItemOverride { rename: r.get(2)?, user_order: r.get(3)?, hidden: r.get(4)?, locked: r.get(5)? };
            out.0.entry(r.get(0)?).or_default().insert(r.get(1)?, o);
        }
        Ok(out)
    }

    fn get(&self, source: SourceId, key: &str) -> Option<&ItemOverride> {
        self.0.get(&source)?.get(key)
    }
}

/// Display position of each enabled playlist (`load_sources` order: `sort_index`, id); the
/// item tables are read in id order and sorted in memory ([`display_order`]).
struct SourceOrder(Vec<SourceId>);

impl SourceOrder {
    fn of(sources: &[SourceMeta]) -> SourceOrder {
        SourceOrder(sources.iter().filter(|s| s.enabled).map(|s| s.id).collect())
    }
    /// `None`: the playlist is disabled (its items are left out).
    fn pos(&self, source: SourceId) -> Option<u32> {
        self.0.iter().position(|s| *s == source).map(|p| p as u32)
    }
}

/// Display order of an item: playlist, the user's order (placed items first), provider order,
/// id — what the `ORDER BY` of the item queries said before they were read in id order.
type OrderKey = (u32, bool, i64, i64, i64);

fn order_key(source_pos: u32, user_order: Option<i64>, provider_order: i64, id: i64) -> OrderKey {
    (source_pos, user_order.is_none(), user_order.unwrap_or(0), provider_order, id)
}

/// The permutation that puts `keys` in order; `None` when they already are (an import writes
/// rows in provider order, so a catalogue without user ordering usually is).
fn display_order(keys: &[OrderKey]) -> Option<Vec<u32>> {
    if keys.is_sorted() {
        return None;
    }
    let mut order: Vec<u32> = (0..keys.len() as u32).collect();
    order.sort_unstable_by_key(|i| keys[*i as usize]);
    Some(order)
}

/// `v` in `order` (a permutation of its indices).
fn permute<T>(v: Vec<T>, order: &[u32]) -> Vec<T> {
    let mut slots: Vec<Option<T>> = v.into_iter().map(Some).collect();
    order.iter().filter_map(|i| slots[*i as usize].take()).collect()
}

/// A TEXT column borrowed from the row (no allocation).
fn text<'r>(r: &'r rusqlite::Row<'_>, i: usize) -> rusqlite::Result<&'r str> {
    r.get_ref(i)?.as_str().map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
}

/// An optional TEXT column as a shared string (one allocation).
fn arc_text(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<Arc<str>>> {
    Ok(match r.get_ref(i)? {
        rusqlite::types::ValueRef::Text(t) => Some(Arc::from(String::from_utf8_lossy(t).as_ref())),
        _ => None,
    })
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

fn effective_groups(links: &[Link]) -> Inherited {
    let groups = match links {
        [] => Vec::new(),
        [one] => vec![one.group],
        _ => {
            let mut v: Vec<&Link> = links.iter().collect();
            v.sort_by_key(|l| (!l.primary, l.flags.pos));
            v.into_iter().map(|l| l.group).collect()
        }
    };
    Inherited {
        all_hidden: !links.is_empty() && links.iter().all(|l| l.flags.hidden),
        any_locked: links.iter().any(|l| l.flags.locked),
        any_explicit: links.iter().any(|l| l.flags.explicit.is_explicit()),
        any_cleared: links.iter().any(|l| l.flags.explicit == Explicit::Cleared),
        groups,
    }
}

fn load_live(conn: &Connection, groups: Vec<Group>, order: &SourceOrder, x: &mut ExplicitIndex) -> Result<LiveIndex> {
    let flags = group_flags(&groups);
    // `tvg_shift_min` is the EFFECTIVE shift (B8, DESIGN §31): the user's override, else the
    // Guide fixes rule row's, else the provider's tvg-shift — every reader of
    // `ChannelSummary::tvg_shift_min` / `ProgrammeSlim::shifted` gets it; only the catch-up
    // request uses the provider's alone (`sources::resolve`).
    let trust = crate::store::live::label_trust(conn)?;
    let mut links = Links::load(conn, "SELECT cg.channel_id, cg.group_id, cg.is_primary FROM channel_group cg", &flags)?;
    // read in id order (no sort in SQL: it carried every column of every row), put in display
    // order below; channels of disabled playlists are skipped
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.source_id, c.key, COALESCE(o.rename, c.clean_name), c.number, c.logo, ce.epg_channel_id, COALESCE(o.shift_min, ce.shift_min, c.tvg_shift_min),
                c.catchup IS NOT NULL, COALESCE(json_extract(c.catchup, '$.days'), 0), c.radio, c.unsupported IS NOT NULL,
                c.provider_order, o.user_order, c.added_at, COALESCE(o.hidden, 0), COALESCE(o.locked, 0),
                c.quality, v.width, v.height, COALESCE(c.family, c.norm_name), c.name, o.rename IS NOT NULL, c.explicit
         FROM channel c
         LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
         LEFT JOIN channel_epg ce ON ce.channel_id = c.id
         LEFT JOIN channel_video v ON v.source_id = c.source_id AND v.item_key = c.key AND v.name = c.name
                   AND (v.url_key IS NULL OR c.url_key IS NULL OR v.url_key = c.url_key)
         ORDER BY c.id",
    )?;
    let mut channels = Vec::new();
    let mut keys: Vec<OrderKey> = Vec::new();
    let mut hidden = HashSet::new();
    let mut renamed = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let source_id: SourceId = r.get(1)?;
        let Some(pos) = order.pos(source_id) else { continue };
        let radio: bool = r.get(10)?;
        let name = text(r, 3)?;
        let provider_name = match r.get_ref(21)? {
            rusqlite::types::ValueRef::Text(raw) if raw != name.as_bytes() => Some(Arc::from(String::from_utf8_lossy(raw))),
            _ => None,
        };
        let mut c = ChannelSummary {
            id: ChannelId(id),
            source_id,
            key: ItemKey(text(r, 2)?.to_owned()),
            name: Arc::from(name),
            provider_name,
            number: r.get(4)?,
            logo: arc_text(r, 5)?,
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
                match (r.get::<_, Option<i64>>(18)?, r.get::<_, Option<i64>>(19)?) {
                    (Some(w), Some(h)) => Quality::of_video(w, h),
                    _ => None,
                }
                .map(|m| QualityBadge::measured(m, claimed))
                .or_else(|| claimed.map(|c| QualityBadge::named(c, trust.get(&source_id).is_some_and(|t| t.doubts(c)))))
                .filter(|_| !radio)
            },
            family: text_hash(r.get_ref(20)?),
        };
        let (item_hidden, is_renamed, own_explicit): (bool, bool, bool) = (r.get(15)?, r.get(22)?, r.get(23)?);
        let inherited = effective_groups(links.of(id));
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
        c.group_ids = inherited.groups;
        keys.push(order_key(pos, c.user_order.map(i64::from), i64::from(c.provider_order), id));
        renamed.push(is_renamed);
        channels.push(c);
    }
    if let Some(order) = display_order(&keys) {
        channels = permute(channels, &order);
        renamed = permute(renamed, &order);
    }
    let mut members: HashMap<GroupId, Vec<ChannelId>> = HashMap::new();
    for c in &channels {
        for g in &c.group_ids {
            members.entry(*g).or_default().push(c.id);
        }
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
fn load_vod(
    conn: &Connection,
    movies: bool,
    groups: &[Group],
    order: &SourceOrder,
    x: &mut ExplicitIndex,
) -> Result<(Vec<VodSummary>, VodMembers, Vec<CopyRaw>)> {
    let flags = group_flags(groups);
    let (table, link, col, kind, poster) =
        if movies { ("movie", "movie_group", "movie_id", "movie", "poster") } else { ("series", "series_group", "series_id", "series", "cover") };
    let item_kind = if movies { ItemKind::Movie } else { ItemKind::Series };
    let mut links = Links::load(conn, &format!("SELECT l.{col}, l.group_id, 0 FROM {link} l"), &flags)?;
    let overrides = Overrides::load(conn, kind)?;
    let split = crate::store::titles::linked_ids(conn, item_kind, LinkAction::Split)?;
    let pinned = crate::store::titles::linked_ids(conn, item_kind, LinkAction::Pin)?;
    let joins = crate::store::titles::join_groups(conn, item_kind)?;
    // read in id order (no join, no sort in SQL: the sort carried every column of every row,
    // hidden ones too), put in display order below
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT x.id, x.source_id, x.key, x.explicit, x.clean_name, x.year, x.{poster}, x.rating, x.added_at,
                x.provider_order, x.match_hash, x.match_lang, x.match_tags, x.match_origin, x.tmdb_id
         FROM {table} x ORDER BY x.id"
    ))?;
    let mut items = Vec::new();
    let mut raws = Vec::new();
    let mut keys: Vec<OrderKey> = Vec::new();
    // `match_lang` values are few: one allocation each
    let mut langs: HashMap<Vec<u8>, Arc<str>> = HashMap::new();
    let mut clean_seen: Option<SourceId> = None;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let source_id: SourceId = r.get(1)?;
        let Some(pos) = order.pos(source_id) else { continue };
        let inherited = effective_groups(links.of(id));
        let key = text(r, 2)?;
        let o = overrides.get(source_id, key);
        let kind = if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) };
        if inherited.explicit(r.get(3)?) {
            x.vod.insert(kind);
            x.refs.insert(ItemRef { source: source_id, kind: item_kind, key: ItemKey(key.to_owned()) });
        } else if clean_seen != Some(source_id) {
            x.clean.insert((source_id, item_kind));
            clean_seen = Some(source_id);
        }
        // hidden titles are left out before anything else of the row is read
        if o.is_some_and(|o| o.hidden) || inherited.all_hidden {
            continue;
        }
        let name: Arc<str> = match o.and_then(|o| o.rename.as_deref()) {
            Some(rename) => Arc::from(rename),
            None => Arc::from(text(r, 4)?),
        };
        let provider_order: i32 = r.get(9)?;
        keys.push(order_key(pos, o.and_then(|o| o.user_order), i64::from(provider_order), id));
        items.push(VodSummary {
            kind,
            source_id,
            key: ItemKey(key.to_owned()),
            name,
            year: r.get(5)?,
            poster: arc_text(r, 6)?,
            rating: r.get::<_, Option<f64>>(7)?.map(|v| v as f32),
            group_ids: inherited.groups,
            added_at: r.get(8)?,
            provider_order,
            locked: o.is_some_and(|o| o.locked) || inherited.any_locked,
        });
        let lang = match r.get_ref(11)? {
            rusqlite::types::ValueRef::Text(t) if !t.is_empty() => Some(match langs.get(t) {
                Some(l) => l.clone(),
                None => langs.entry(t.to_vec()).or_insert_with(|| Arc::from(String::from_utf8_lossy(t).as_ref())).clone(),
            }),
            _ => None,
        };
        let tmdb = match r.get_ref(14)? {
            rusqlite::types::ValueRef::Integer(n) => u32::try_from(n).ok().filter(|n| *n > 0),
            rusqlite::types::ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(vod_match::tmdb_id),
            _ => None,
        };
        let origin = match r.get_ref(13)? {
            rusqlite::types::ValueRef::Text([a, b]) => Some([*a, *b]),
            _ => None,
        };
        raws.push(CopyRaw {
            hash: r.get(10)?,
            lang,
            tags: VersionTags(r.get::<_, Option<i64>>(12)?.and_then(|t| u16::try_from(t).ok()).unwrap_or(0)),
            origin,
            tmdb,
            split: !split.is_empty() && split.contains(&id),
            pinned: !pinned.is_empty() && pinned.contains(&id),
            join: if joins.is_empty() { None } else { joins.get(&id).copied() },
        });
    }
    if let Some(order) = display_order(&keys) {
        items = permute(items, &order);
        raws = permute(raws, &order);
    }
    let mut members = VodMembers::new();
    for (i, v) in items.iter().enumerate() {
        for g in &v.group_ids {
            members.entry(*g).or_default().push(i as u32);
        }
    }
    Ok((items, members, raws))
}

/// "Also hide titles in other languages" (DESIGN §28.12) for one build: per playlist whose rule
/// asks for it, the languages it keeps; and the titles never hidden this way.
struct LanguageRule {
    /// Source → kept languages (ISO 639-1: the rule's and the user's audio languages).
    keep: HashMap<SourceId, Vec<String>>,
    /// Keys of favourites and titles with watch progress (an episode's counts for its series)
    /// per (source, kind): looked up by the title's key without copying it.
    protected: HashMap<(SourceId, ItemKind), HashSet<String>>,
}

impl LanguageRule {
    fn of(conn: &Connection, sources: &[SourceMeta], prefs: &[String]) -> Result<LanguageRule> {
        let keep: HashMap<SourceId, Vec<String>> = sources
            .iter()
            .filter_map(|s| {
                let rule = s.category_filter.as_ref().filter(|r| category_tags::hides_other_languages(r))?;
                let mut langs: Vec<String> = category_tags::rule_languages(rule).into_iter().map(str::to_owned).collect();
                langs.extend(prefs.iter().filter(|p| !langs.contains(p)).cloned().collect::<Vec<_>>());
                // Ex-Yugoslav titles for a Serbian, Croatian or Bosnian speaker
                if prefs.iter().any(|p| matches!(p.as_str(), "sr" | "hr" | "bs" | "bos")) {
                    langs.push(category_tags::SERBO_CROATIAN.to_owned());
                }
                Some((s.id, langs))
            })
            .collect();
        let mut protected: HashMap<(SourceId, ItemKind), HashSet<String>> = HashMap::new();
        if !keep.is_empty() {
            let mut stmt = conn.prepare_cached(
                "SELECT source_id, kind, item_key FROM fav_item WHERE kind IN ('movie', 'series')
                 UNION SELECT source_id, kind, item_key FROM watch_progress WHERE kind IN ('movie', 'series')
                 UNION SELECT e.source_id, 'series', x.key FROM watch_progress w
                       JOIN episode e ON e.source_id = w.source_id AND e.key = w.item_key JOIN series x ON x.id = e.series_id
                       WHERE w.kind = 'episode'",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, SourceId>(0)?, kind_from_sql(&r.get::<_, String>(1)?)?, r.get::<_, String>(2)?)))?;
            for r in rows {
                let (source, kind, key) = r?;
                protected.entry((source, kind)).or_default().insert(key);
            }
        }
        Ok(LanguageRule { keep, protected })
    }

    /// The items (with their members and match data) without the titles in languages their
    /// playlist's rule doesn't keep: a title's language is the market its own name marks
    /// (`vod_match::market_lang`); an untagged title in a category whose tagged titles are
    /// mostly one market (`stacks::dominant_market`) takes that market. Untagged and
    /// multi-audio titles, favourites and titles with progress stay. The categories' counts
    /// and `SourceMeta::language_hidden` leave the dropped titles out.
    fn apply(
        &self,
        kind: ItemKind,
        items: Vec<VodSummary>,
        members: VodMembers,
        raw: Vec<CopyRaw>,
        groups: &mut [Group],
        sources: &mut [SourceMeta],
    ) -> (Vec<VodSummary>, VodMembers, Vec<CopyRaw>) {
        if self.keep.is_empty() {
            return (items, members, raw);
        }
        // the market of each distinct `match_lang` text (few), and whether it is a bare
        // subtitle mark (`SUBS - …`, `+SUB`), which counts as untagged: the copy is for its
        // category's language
        // keyed by the text's address: `load_vod` shares one allocation per distinct text, so
        // a title costs a pointer hash, not a string hash
        let mut markets: HashMap<*const u8, (Option<&'static str>, bool)> = HashMap::new();
        let (market, tagged): (Vec<Option<&'static str>>, Vec<bool>) = raw
            .iter()
            .map(|r| {
                let Some(t) = r.lang.as_ref() else { return (None, false) };
                let (m, bare) = *markets.entry(t.as_ptr()).or_insert_with(|| {
                    let (code, dub) = vod_match::split_lang_text(t);
                    (code.and_then(|c| vod_match::market_lang(c, dub)), dub != vod_match::Dub::Multi && code.is_none_or(vod_match::is_bare_subtitles))
                });
                (m, !bare)
            })
            .unzip();
        let dominant = crate::store::stacks::dominant_market(&items, &market, &tagged);
        let mut map: Vec<Option<u32>> = Vec::with_capacity(items.len());
        let (mut out, mut out_raw) = (Vec::with_capacity(items.len()), Vec::with_capacity(raw.len()));
        let mut dropped_per_source: HashMap<SourceId, u32> = HashMap::new();
        for (i, (v, r)) in items.into_iter().zip(raw).enumerate() {
            let lang = market[i].or_else(|| if tagged[i] { None } else { v.group_ids.iter().find_map(|g| dominant.get(g).copied()) });
            let hide = match (self.keep.get(&v.source_id), lang) {
                (Some(keep), Some(l)) => {
                    !keep.iter().any(|k| k == l) && !self.protected.get(&(v.source_id, kind)).is_some_and(|p| p.contains(v.key.as_str()))
                }
                _ => false,
            };
            if hide {
                *dropped_per_source.entry(v.source_id).or_default() += 1;
                map.push(None);
            } else {
                map.push(Some(out.len() as u32));
                out.push(v);
                out_raw.push(r);
            }
        }
        if dropped_per_source.is_empty() {
            return (out, members, out_raw);
        }
        let mut dropped: HashMap<GroupId, u32> = HashMap::new();
        let members = members
            .into_iter()
            .map(|(g, v)| {
                let before = v.len();
                let v: Vec<u32> = v.into_iter().filter_map(|i| map.get(i as usize).copied().flatten()).collect();
                dropped.insert(g, (before - v.len()) as u32);
                (g, v)
            })
            .collect();
        for g in groups.iter_mut() {
            g.item_count = g.item_count.saturating_sub(dropped.get(&g.id).copied().unwrap_or(0));
        }
        for s in sources.iter_mut() {
            s.language_hidden += dropped_per_source.get(&s.id).copied().unwrap_or(0);
        }
        (out, members, out_raw)
    }
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
    let (lib, profile) = load_profiled(conn, opts)?;
    // a slow build says which step was slow (field reports come without the catalogue)
    if profile.total() >= SLOW_BUILD {
        tracing::info!("library build: {profile}");
    } else {
        tracing::debug!("library build: {profile}");
    }
    Ok(lib)
}

/// A build this slow is logged with its steps at `info`.
const SLOW_BUILD: std::time::Duration = std::time::Duration::from_secs(1);

/// How long each step of one build took (the perf benches print it; a handful of clock
/// reads, nothing else).
#[derive(Debug, Default, Clone)]
pub struct LoadProfile(pub Vec<(&'static str, std::time::Duration)>);

impl std::fmt::Display for LoadProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ms (", self.total().as_millis())?;
        for (i, (step, d)) in self.0.iter().enumerate() {
            write!(f, "{}{step} {} ms", if i > 0 { ", " } else { "" }, d.as_millis())?;
        }
        write!(f, ")")
    }
}

impl LoadProfile {
    pub fn total(&self) -> std::time::Duration {
        self.0.iter().map(|(_, d)| *d).sum()
    }

    fn lap(&mut self, step: &'static str, since: &mut std::time::Instant) {
        let now = std::time::Instant::now();
        self.0.push((step, now - *since));
        *since = now;
    }
}

/// [`load_with`], with the time of each step.
///
/// The build reads ONE snapshot of the database: it opens a read transaction on `conn` (or
/// reads in the caller's, when one is open). The channels are read on their own thread, on a
/// second connection whose read transaction is on the same snapshot ([`Snapshot::take`]),
/// while `conn` reads the movies and series; when no such connection can be had (an in-memory
/// database, a caller's transaction, writes landing while the snapshots are taken), `conn`
/// reads everything, the channels first. With the thread, the `channels` step is the wait for
/// it after the VOD steps, so the steps still add up to the build's wall time.
pub fn load_profiled(conn: &Connection, opts: &LoadOptions) -> Result<(Library, LoadProfile)> {
    let mut profile = LoadProfile::default();
    let mut t = std::time::Instant::now();
    // `_txn` (not `_`): the read transaction lasts the whole build
    let Snapshot { txn: _txn, twin } = Snapshot::take(conn)?;
    profile.lap("snapshot", &mut t);
    let sources = load_sources(conn)?;
    profile.lap("sources", &mut t);
    let (ch_groups, movie_groups, series_groups) = load_groups(conn, &sources)?;
    profile.lap("groups", &mut t);
    let mut explicit = ExplicitIndex {
        groups: ch_groups.iter().chain(&movie_groups).chain(&series_groups).filter(|g| g.explicit.is_explicit()).map(|g| g.id).collect(),
        ..Default::default()
    };
    let order = SourceOrder::of(&sources);
    let mut sources = sources;
    let prefs = opts.lang_prefs();
    // the movies and series, on `conn`
    let read_vod =
        |explicit: &mut ExplicitIndex, sources: &mut [SourceMeta], profile: &mut LoadProfile, t: &mut std::time::Instant| -> Result<VodRead> {
            let (movies, members, movie_raw) = load_vod(conn, true, &movie_groups, &order, explicit)?;
            profile.lap("movies", t);
            let (series, series_members, series_raw) = load_vod(conn, false, &series_groups, &order, explicit)?;
            profile.lap("series", t);
            // "Also hide titles in other languages" (DESIGN §28.12)
            let langs = LanguageRule::of(conn, sources, &prefs)?;
            let (mut movie_groups, mut series_groups) = (movie_groups, series_groups);
            let (movies, mut members, movie_raw) = langs.apply(ItemKind::Movie, movies, members, movie_raw, &mut movie_groups, sources);
            let (series, series_members, series_raw) = langs.apply(ItemKind::Series, series, series_members, series_raw, &mut series_groups, sources);
            members.extend(series_members);
            profile.lap("title languages", t);
            disabled_explicit_refs(conn, explicit)?;
            Ok(VodRead { movies, series, members, movie_raw, series_raw, movie_groups, series_groups })
        };
    let (live, vod) = match twin {
        Some(twin) => std::thread::scope(|s| -> Result<_> {
            let order = &order;
            let channels = std::thread::Builder::new()
                .name("library-channels".into())
                .spawn_scoped(s, move || {
                    let mut x = ExplicitIndex::default();
                    load_live(&twin, ch_groups, order, &mut x).map(|live| (live, x))
                })
                .map_err(crate::error::Error::other)?;
            let vod = read_vod(&mut explicit, &mut sources, &mut profile, &mut t);
            let channels = channels.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            let (vod, (live, x)) = (vod?, channels?);
            explicit.merge(x);
            profile.lap("channels", &mut t);
            Ok((live, vod))
        })?,
        None => {
            let live = load_live(conn, ch_groups, &order, &mut explicit)?;
            profile.lap("channels", &mut t);
            (live, read_vod(&mut explicit, &mut sources, &mut profile, &mut t)?)
        }
    };
    let VodRead { movies, series, members, movie_raw, series_raw, movie_groups, series_groups } = vod;
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
    profile.lap("vod index, favourites", &mut t);
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
    let lib = lib.with_visibility(opts.visibility);
    profile.lap("visibility", &mut t);
    let lib = lib.stacked(movie_raw, series_raw, &prefs);
    profile.lap("stacks", &mut t);
    Ok((lib, profile))
}

/// The movies and series of a build (title languages applied), with their groups.
struct VodRead {
    movies: Vec<VodSummary>,
    series: Vec<VodSummary>,
    members: VodMembers,
    movie_raw: Vec<CopyRaw>,
    series_raw: Vec<CopyRaw>,
    movie_groups: Vec<Group>,
    series_groups: Vec<Group>,
}

/// The read transactions one build reads its snapshot in.
struct Snapshot<'c> {
    /// The read transaction the build opened on its caller's connection (`None`: the caller
    /// has one open, and the build reads in it).
    txn: Option<ReadTxn<'c>>,
    /// A second read-only connection, in a read transaction on the same snapshot.
    twin: Option<Connection>,
}

impl<'c> Snapshot<'c> {
    /// Times a write may land while the two snapshots are taken before the build gives the
    /// second connection up.
    const TRIES: usize = 3;

    /// Open a read transaction on `conn` and, beside it, a second connection on the same
    /// snapshot. WAL gives every read transaction the last commit at its start, so the two
    /// agree when no write commits between their starts: the second connection reads
    /// `PRAGMA data_version` (which changes when ANOTHER connection committed since its last
    /// read) before `conn`'s transaction starts and again at its own transaction's start; the
    /// same value, and `conn`'s snapshot (taken in between) is the second one's. A changed
    /// value may also be a checkpoint: the build tries again, then reads on `conn` alone.
    /// With the caller's transaction open there is nothing to share: `conn` may have
    /// uncommitted writes, or a snapshot older than any new connection's.
    fn take(conn: &'c Connection) -> Result<Snapshot<'c>> {
        Self::take_with(conn, || {})
    }

    /// [`Snapshot::take`]; `between` runs after `conn`'s snapshot is taken, before the second
    /// connection's (tests land a write there).
    fn take_with(conn: &'c Connection, mut between: impl FnMut()) -> Result<Snapshot<'c>> {
        if !conn.is_autocommit() {
            return Ok(Snapshot { txn: None, twin: None });
        }
        // an in-memory or temporary database has no path (another connection can't open it)
        let twin = conn.path().filter(|p| !p.is_empty()).and_then(|p| crate::store::open_reader(std::path::Path::new(p)).ok());
        if let Some(twin) = twin {
            for _ in 0..Self::TRIES {
                let Ok(before) = data_version(&twin) else { break };
                let txn = ReadTxn::begin(conn)?;
                between();
                if twin.execute_batch("BEGIN").is_ok() && data_version(&twin).is_ok_and(|after| after == before) {
                    return Ok(Snapshot { txn: Some(txn), twin: Some(twin) });
                }
                if !twin.is_autocommit() {
                    let _ = twin.execute_batch("ROLLBACK");
                }
            }
        }
        Ok(Snapshot { txn: Some(ReadTxn::begin(conn)?), twin: None })
    }
}

/// `PRAGMA data_version` (in a transaction: of its snapshot, which a read starts).
fn data_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA data_version", [], |r| r.get(0))
}

/// A read transaction the build opened on its caller's connection, ended on drop.
struct ReadTxn<'c>(&'c Connection);

impl<'c> ReadTxn<'c> {
    /// `BEGIN`, and a read so that the snapshot is taken now, not at the build's first query.
    fn begin(conn: &'c Connection) -> Result<ReadTxn<'c>> {
        conn.execute_batch("BEGIN")?;
        let txn = ReadTxn(conn);
        data_version(conn)?;
        Ok(txn)
    }
}

impl Drop for ReadTxn<'_> {
    fn drop(&mut self) {
        // nothing was written: either ends it
        if self.0.execute_batch("COMMIT").is_err() && !self.0.is_autocommit() {
            let _ = self.0.execute_batch("ROLLBACK");
        }
    }
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

    /// The item tables are read in id order and put in display order in memory: playlists by
    /// their order, the user's order first, then provider order; the user's overrides of
    /// movies by lookup; disabled playlists left out.
    #[test]
    fn load_puts_items_in_display_order_with_overrides() {
        let store = crate::store::Store::open_temp("tuner-lib-order", None).unwrap();
        store
            .writer()
            .blocking(|c| {
                c.execute_batch(
                    "INSERT INTO source(id, name, kind, url, created_at, generation, sort_index, enabled) VALUES
                       (1, 'A', 'm3u-url', 'http://a', 0, 1, 1, 1), (2, 'B', 'm3u-url', 'http://b', 0, 1, 0, 1),
                       (3, 'Off', 'm3u-url', 'http://c', 0, 1, 2, 0);
                     INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES
                       (1, 1, 'movie', 'movie:m', 'M', 0, 3, 1), (2, 2, 'movie', 'movie:m', 'M', 0, 1, 1),
                       (3, 3, 'movie', 'movie:m', 'M', 0, 1, 1), (4, 1, 'channel', 'channel:c', 'C', 0, 2, 1),
                       (5, 2, 'channel', 'channel:c', 'C', 0, 1, 1);
                     -- ids out of provider order
                     INSERT INTO movie(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen) VALUES
                       (1, 1, 'm1', 'One', 'One', 'one', '{}', 2, 1), (2, 1, 'm2', 'Two', 'Two', 'two', '{}', 0, 1),
                       (3, 1, 'm3', 'Three', 'Three', 'three', '{}', 1, 1), (4, 1, 'm6', 'Six', 'Six', 'six', '{}', 3, 1),
                       (5, 2, 'm4', 'Four', 'Four', 'four', '{}', 0, 1), (6, 3, 'm5', 'Five', 'Five', 'five', '{}', 0, 1);
                     INSERT INTO movie_group(group_id, movie_id, position) VALUES (1, 1, 0), (1, 2, 1), (1, 3, 2), (1, 4, 3), (2, 5, 0), (3, 6, 0);
                     INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen) VALUES
                       (1, 1, 'c1', 'C1', 'C1', 'c1', '{}', 1, 1), (2, 1, 'c2', 'C2', 'C2', 'c2', '{}', 0, 1),
                       (3, 2, 'c3', 'C3', 'C3', 'c3', '{}', 0, 1);
                     INSERT INTO channel_group(group_id, channel_id, position) VALUES (4, 1, 0), (4, 2, 1), (5, 3, 0);
                     INSERT INTO item_override(source_id, kind, item_key, rename, hidden, locked, user_order, fp) VALUES
                       (1, 'movie', 'm3', NULL, 0, 0, 0, '{}'), (1, 'movie', 'm1', 'Renamed', 0, 1, NULL, '{}'),
                       (1, 'movie', 'm2', NULL, 1, 0, NULL, '{}'), (1, 'channel', 'c1', NULL, 0, 0, 0, '{}');",
                )?;
                Ok(())
            })
            .unwrap();
        let lib = load(&store.reader().unwrap()).unwrap();
        let keys = |v: &[VodSummary]| v.iter().map(|m| m.key.as_str().to_owned()).collect::<Vec<_>>();
        assert_eq!(keys(&lib.vod.movies), ["m4", "m3", "m1", "m6"], "B first; A: user order, then provider order; hidden and disabled out");
        assert_eq!((&*lib.vod.movies[2].name, lib.vod.movies[2].locked), ("Renamed", true));
        assert_eq!(lib.vod.group_items(GroupId(1), SortMode::Provider), [1, 2, 3]);
        assert_eq!(lib.vod.group_items(GroupId(2), SortMode::Provider), [0]);
        let channels: Vec<&str> = lib.live.channels.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(channels, ["c3", "c1", "c2"]);
        assert_eq!(lib.live.group_members(GroupId(4)), [ChannelId(1), ChannelId(2)]);
    }

    /// The build read the channels on their own thread (the `channels` step comes last: the
    /// wait for that thread) rather than first on the caller's connection.
    fn channels_beside_vod(profile: &LoadProfile) -> bool {
        let at = |step: &str| profile.0.iter().position(|(s, _)| *s == step).unwrap();
        at("channels") > at("movies")
    }

    /// One playlist with an explicit category, a provider-flagged channel and movie, a
    /// disabled playlist with an explicit channel; `extra` more channels and movies.
    fn seed_two_playlists(store: &crate::store::Store) {
        store
            .writer()
            .blocking(|c| {
                c.execute_batch(
                    "INSERT INTO source(id, name, kind, url, created_at, generation, sort_index, enabled) VALUES
                       (1, 'A', 'm3u-url', 'http://a', 0, 1, 0, 1), (2, 'Off', 'm3u-url', 'http://b', 0, 1, 1, 0);
                     INSERT INTO grp(id, source_id, kind, key, name, provider_order, item_count, seen_gen, explicit) VALUES
                       (1, 1, 'channel', 'channel:news', 'News', 0, 2, 1, 0), (2, 1, 'channel', 'channel:xxx', 'XXX', 1, 1, 1, 1),
                       (3, 1, 'movie', 'movie:m', 'Films', 0, 3, 1, 0), (4, 2, 'channel', 'channel:xxx', 'XXX', 0, 1, 1, 1);
                     INSERT INTO channel(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen, explicit) VALUES
                       (1, 1, 'c1', 'One', 'One', 'one', '{}', 0, 1, 0), (2, 1, 'c2', 'Two', 'Two', 'two', '{}', 1, 1, 1),
                       (3, 1, 'c3', 'Late', 'Late', 'late', '{}', 2, 1, 0), (4, 2, 'c4', 'Off', 'Off', 'off', '{}', 0, 1, 0);
                     INSERT INTO channel_group(group_id, channel_id, position) VALUES (1, 1, 0), (1, 2, 1), (2, 3, 0), (4, 4, 0);
                     INSERT INTO movie(id, source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen, explicit) VALUES
                       (1, 1, 'm1', 'Film', 'Film', 'film', '{}', 0, 1, 0), (2, 1, 'm2', 'Adult', 'Adult', 'adult', '{}', 1, 1, 1);
                     INSERT INTO movie_group(group_id, movie_id, position) VALUES (3, 1, 0), (3, 2, 1);",
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn channels_read_on_their_own_thread_build_the_same_library() {
        let store = crate::store::Store::open_temp("tuner-lib-twin", None).unwrap();
        seed_two_playlists(&store);
        let reader = store.reader().unwrap();
        let (beside, profile) = load_profiled(&reader, &LoadOptions::default()).unwrap();
        assert!(channels_beside_vod(&profile), "a quiet database on disk: the channels are read beside the VOD ({profile})");
        assert!(reader.is_autocommit(), "the build ends the read transaction it opened");
        // in the caller's transaction the build reads everything on the caller's connection
        reader.execute_batch("BEGIN").unwrap();
        let (serial, profile) = load_profiled(&reader, &LoadOptions::default()).unwrap();
        assert!(!channels_beside_vod(&profile), "a caller's transaction: one connection ({profile})");
        assert!(!reader.is_autocommit(), "the caller's transaction is left open");
        reader.execute_batch("COMMIT").unwrap();

        let keys = |lib: &Library| lib.live.channels.iter().map(|c| c.key.as_str().to_owned()).collect::<Vec<_>>();
        assert_eq!(keys(&beside), ["c1", "c2", "c3"]);
        assert_eq!(keys(&beside), keys(&serial));
        assert_eq!(beside.live.group_members(GroupId(1)), serial.live.group_members(GroupId(1)));
        let movies = |lib: &Library| lib.vod.movies.iter().map(|m| m.key.as_str().to_owned()).collect::<Vec<_>>();
        assert_eq!(movies(&beside), ["m1", "m2"]);
        assert_eq!(movies(&beside), movies(&serial));
        // the explicit index merges the channel thread's part with the VOD's and the
        // disabled playlist's
        let (x, y) = (&beside.explicit, &serial.explicit);
        assert_eq!(x.channels, HashSet::from([ChannelId(2), ChannelId(3)]));
        assert_eq!(x.vod, HashSet::from([VodKind::Movie(MovieId(2))]));
        let r = |source: i64, kind: ItemKind, key: &str| ItemRef { source: SourceId(source), kind, key: ItemKey(key.to_owned()) };
        assert_eq!(
            x.refs,
            HashSet::from([r(1, ItemKind::Channel, "c2"), r(1, ItemKind::Channel, "c3"), r(1, ItemKind::Movie, "m2"), r(2, ItemKind::Channel, "c4")])
        );
        assert_eq!(x.clean, HashSet::from([(SourceId(1), ItemKind::Channel), (SourceId(1), ItemKind::Movie)]));
        assert_eq!((&x.groups, &x.channels, &x.vod, &x.refs, &x.clean), (&y.groups, &y.channels, &y.vod, &y.refs, &y.clean));
    }

    #[test]
    fn a_write_between_the_two_snapshots_is_caught() {
        let store = crate::store::Store::open_temp("tuner-lib-between", None).unwrap();
        seed_two_playlists(&store);
        let add_channel = |n: u32| {
            store
                .writer()
                .blocking(move |c| {
                    c.execute(
                        "INSERT INTO channel(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                         VALUES (1, ?1, 'New', 'New', 'new', '{}', 100, 1)",
                        [format!("between{n}")],
                    )?;
                    Ok(())
                })
                .unwrap();
        };
        let count = |c: &Connection| c.query_row("SELECT COUNT(*) FROM channel", [], |r| r.get::<_, i64>(0)).unwrap();
        let reader = store.reader().unwrap();

        // a commit lands after the caller's snapshot, before the second connection's, once:
        // the build takes both again, and they agree
        let mut landed = 0;
        let snap = Snapshot::take_with(&reader, || {
            if landed == 0 {
                add_channel(landed);
            }
            landed += 1;
        })
        .unwrap();
        assert_eq!(landed, 2, "the first pair was given up");
        let twin = snap.twin.as_ref().expect("the second try agrees");
        assert_eq!((count(&reader), count(twin)), (5, 5));
        drop(snap);
        assert!(reader.is_autocommit());

        // writes land every time: the caller's connection reads alone, in one transaction
        let mut tries = 0;
        let snap = Snapshot::take_with(&reader, || {
            tries += 1;
            add_channel(tries);
        })
        .unwrap();
        assert_eq!(tries, Snapshot::TRIES as u32);
        assert!(snap.twin.is_none());
        let seen = count(&reader);
        add_channel(100);
        assert_eq!(count(&reader), seen, "the build's transaction holds its snapshot");
        drop(snap);
        assert_eq!(count(&reader), seen + 1);
    }

    #[test]
    fn channels_and_movies_come_from_one_snapshot_while_imports_write() {
        let store = crate::store::Store::open_temp("tuner-lib-snapshot", None).unwrap();
        seed_two_playlists(&store);
        // an import's batches: a channel and a movie in each transaction, back to back
        let stop = Arc::new(AtomicBool::new(false));
        let writes = {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    store
                        .writer()
                        .blocking(move |c| {
                            let tx = c.transaction()?;
                            tx.execute(
                                "INSERT INTO channel(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                                 VALUES (1, ?1, 'New', 'New', 'new', '{}', 100, 1)",
                                [format!("new{n}")],
                            )?;
                            tx.execute(
                                "INSERT INTO movie(source_id, key, name, clean_name, norm_name, stream, provider_order, seen_gen)
                                 VALUES (1, ?1, 'New', 'New', 'new', '{}', 100, 1)",
                                [format!("new{n}")],
                            )?;
                            tx.commit()?;
                            Ok(())
                        })
                        .unwrap();
                }
                n
            })
        };
        let (mut builds, mut beside) = (0, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while builds < 60 && std::time::Instant::now() < deadline {
            let (lib, profile) = load_profiled(&store.reader().unwrap(), &LoadOptions::default()).unwrap();
            // 3 seeded channels (+ 1 of the disabled playlist, left out), 2 seeded movies
            let (channels, movies) = (lib.live.channels.len() - 3, lib.vod.movies.len() - 2);
            assert_eq!(channels, movies, "channels and movies of one snapshot ({profile})");
            builds += 1;
            beside += usize::from(channels_beside_vod(&profile));
        }
        stop.store(true, Ordering::Relaxed);
        let written = writes.join().unwrap();
        assert!(written > 0 && builds > 0);
        println!("{builds} builds during {written} writes, {beside} with the channels on their own thread");
        // quiet again: the build reads the channels beside the VOD, on the same snapshot
        let (lib, profile) = load_profiled(&store.reader().unwrap(), &LoadOptions::default()).unwrap();
        assert!(channels_beside_vod(&profile), "{profile}");
        assert_eq!(lib.live.channels.len() - 3, written as usize);
        assert_eq!(lib.vod.movies.len() - 2, written as usize);
    }
}
