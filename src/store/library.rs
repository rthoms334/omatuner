//! In-memory catalogue snapshot used by the UI thread for every list (DESIGN §5.4).
//! Owner: WP-02 for [`load`] / [`load_favs`] (SQL). The index types and their accessors are
//! implemented in the skeleton (they run every frame in WP-11/12 code and in the player's zap
//! logic) and are built through `from_parts`, which both `load` and tests use.
//!
//! Built off the UI thread by `jobs` (`JobKind::RebuildLibrary`) from a reader connection,
//! handed to the UI as `Arc<Library>` and swapped atomically. Rebuilt after `DataScope::{Live,
//! Vod, Overrides, Epg, History, All}` (Epg: `ChannelSummary.epg_channel_id` is a snapshot of
//! `channel_epg`; History: `recent_channels`).
//!
//! Favourites are NOT read from the snapshot by screens: the app copies `Library::favs` into
//! its own mutable `FavIndex` on every swap (`UiCtx::favs`), patches it optimistically with
//! [`FavIndex::apply_toggle`] right after submitting a write, and replaces it with
//! [`load_favs`] on `DataScope::Favourites` (event-driven, ≤ 5 ms, never per frame).
//!
//! Budget: 100k channels + 50k movies + 20k series → < 400 ms build, < 60 MB RAM.
//! Hidden items are excluded from `members`/`collection` but kept in `channels` (Settings ▸
//! Hidden lists them); locked items are flagged, not removed (the UI asks for the PIN).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{
    ChannelId, ChannelSummary, Collection, FavListId, FavouriteEntry, FavouriteList, Group, GroupId, ItemKey, ItemKind,
    ItemRef, MovieId, SeriesId, SortMode, SourceColor, SourceId, SourceKind, Timestamp, VodKind, VodSummary,
};

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
}

#[derive(Debug, Default)]
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
        Self { channels, hidden, by_id, by_ref, groups, members }
    }

    pub fn get(&self, id: ChannelId) -> Option<&ChannelSummary> { self.by_id.get(&id).map(|i| &self.channels[*i as usize]) }

    pub fn by_ref(&self, item: &ItemRef) -> Option<ChannelId> {
        if item.kind != ItemKind::Channel {
            return None;
        }
        self.by_ref.get(&(item.source, item.key.clone())).copied()
    }

    pub fn group_members(&self, g: GroupId) -> &[ChannelId] { self.members.get(&g).map(Vec::as_slice).unwrap_or(&[]) }

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
    /// display order: user order then provider order; favourites keep list order; history
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
            SortMode::Provider | SortMode::Rating | SortMode::Year => {}
            SortMode::Number => ids.sort_by_key(|id| key(id).and_then(|c| c.number).unwrap_or(u32::MAX)),
            SortMode::NameAsc => ids.sort_by_cached_key(|id| key(id).map(|c| c.name.to_lowercase()).unwrap_or_default()),
            SortMode::NameDesc => {
                ids.sort_by_cached_key(|id| std::cmp::Reverse(key(id).map(|c| c.name.to_lowercase()).unwrap_or_default()))
            }
            SortMode::RecentlyAdded => ids.sort_by_key(|id| std::cmp::Reverse(key(id).and_then(|c| c.added_at).unwrap_or(i64::MIN))),
        }
        ids
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
}

#[derive(Debug, Default)]
pub struct VodIndex {
    /// Visible (non-hidden) movies, provider order. Settings ▸ Hidden reads hidden VOD via SQL.
    pub movies: Vec<VodSummary>,
    pub series: Vec<VodSummary>,
    pub movie_groups: Vec<Group>,
    pub series_groups: Vec<Group>,
    /// Indices into `movies` (movie groups) or `series` (series groups).
    members: HashMap<GroupId, Vec<u32>>,
    movie_idx: HashMap<MovieId, u32>,
    series_idx: HashMap<SeriesId, u32>,
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
        Self { movies, series, movie_groups, series_groups, members, movie_idx, series_idx }
    }

    fn sort(&self, items: &mut [u32], movies: bool, sort: SortMode) {
        let all = if movies { &self.movies } else { &self.series };
        let v = |i: &u32| &all[*i as usize];
        match sort {
            SortMode::Provider | SortMode::Number => items.sort_by_key(|i| v(i).provider_order),
            SortMode::NameAsc => items.sort_by_cached_key(|i| v(i).name.to_lowercase()),
            SortMode::NameDesc => items.sort_by_cached_key(|i| std::cmp::Reverse(v(i).name.to_lowercase())),
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

    pub fn find(&self, kind: VodKind) -> Option<&VodSummary> {
        match kind {
            VodKind::Movie(id) => self.movie_idx.get(&id).map(|i| &self.movies[*i as usize]),
            VodKind::Series(id) => self.series_idx.get(&id).map(|i| &self.series[*i as usize]),
        }
    }
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

    pub fn items(&self, list: FavListId) -> &[ItemRef] { self.items.get(&list).map(Vec::as_slice).unwrap_or(&[]) }
    pub fn lists_of(&self, item: &ItemRef) -> &[FavListId] { self.membership.get(item).map(Vec::as_slice).unwrap_or(&[]) }
    /// Member of the default list (what Ctrl+F toggles and the ★ glyph shows).
    pub fn is_fav(&self, item: &ItemRef) -> bool { self.default_list().is_some_and(|d| self.lists_of(item).contains(&d)) }
    pub fn default_list(&self) -> Option<FavListId> { self.lists.iter().find(|l| l.is_default).map(|l| l.id) }
    /// Non-empty lists in order (Guide default collection, Home rails).
    pub fn non_empty_lists(&self) -> impl Iterator<Item = &FavouriteList> { self.lists.iter().filter(|l| !self.items(l.id).is_empty()) }
    pub fn is_empty(&self) -> bool { self.items.values().all(Vec::is_empty) }

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
    pub live: LiveIndex,
    pub vod: VodIndex,
    /// Favourites at build time — the app copies this into `TunerApp.favs` on swap; screens use
    /// `UiCtx::favs` (see module docs), never this field.
    pub favs: FavIndex,
    /// Distinct recently watched channels, most recent first (`Collection::History`, Home).
    pub recent_channels: Vec<ChannelId>,
    pub built_at: Timestamp,
}

impl Library {
    pub fn source(&self, id: SourceId) -> Option<&SourceMeta> { self.sources.iter().find(|s| s.id == id) }
    pub fn is_empty(&self) -> bool { self.sources.is_empty() }
    pub fn has_vod(&self) -> bool { !self.vod.movies.is_empty() }
    pub fn has_series(&self) -> bool { !self.vod.series.is_empty() }
}

/// Build a full snapshot from a reader connection (uses the `from_parts` constructors). Never
/// call on the UI thread.
pub fn load(conn: &Connection) -> Result<Library> { todo!("WP-02") }

/// Rebuild only the favourites part (cheap; event-driven on the UI thread, < 5 ms).
pub fn load_favs(conn: &Connection) -> Result<FavIndex> { todo!("WP-02") }

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
