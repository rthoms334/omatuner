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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use crate::error::Result;
use crate::store::live::{GROUP_COLS, GROUP_FROM, GROUP_ORDER, group_from_row};
use crate::store::sql::{enum_parse, u32_of};
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
}

impl Library {
    pub fn source(&self, id: SourceId) -> Option<&SourceMeta> { self.sources.iter().find(|s| s.id == id) }
    pub fn is_empty(&self) -> bool { self.sources.is_empty() }
    pub fn has_vod(&self) -> bool { !self.vod.movies.is_empty() }
    pub fn has_series(&self) -> bool { !self.vod.series.is_empty() }

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
        }
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
                EXISTS (SELECT 1 FROM movie m WHERE m.source_id = s.id), EXISTS (SELECT 1 FROM series x WHERE x.source_id = s.id)
         FROM source s ORDER BY s.sort_index, s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(4)?;
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
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Groups of every kind of enabled sources, in display order per kind.
fn load_groups(conn: &Connection) -> Result<(Vec<Group>, Vec<Group>, Vec<Group>)> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {GROUP_COLS} {GROUP_FROM} WHERE s.enabled = 1 ORDER BY g.kind, {GROUP_ORDER}"
    ))?;
    let (mut ch, mut mv, mut se) = (Vec::new(), Vec::new(), Vec::new());
    for g in stmt.query_map([], group_from_row)? {
        let g = g?;
        match g.kind {
            ItemKind::Channel => ch.push(g),
            ItemKind::Movie => mv.push(g),
            ItemKind::Series => se.push(g),
            ItemKind::Episode => {}
        }
    }
    Ok((ch, mv, se))
}

/// Per group: (display position, hidden, locked).
fn group_flags(groups: &[Group]) -> HashMap<GroupId, (usize, bool, bool)> {
    groups.iter().enumerate().map(|(i, g)| (g.id, (i, g.hidden, g.locked))).collect()
}

/// Group memberships `item id → [(group, primary)]` from a link table, restricted to groups
/// in `flags` (enabled sources).
fn load_memberships(conn: &Connection, sql: &str, flags: &HashMap<GroupId, (usize, bool, bool)>) -> Result<HashMap<i64, Vec<(GroupId, bool)>>> {
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

/// Ordered group ids (primary first, then display order) + (all groups hidden, any locked).
fn effective_groups(links: Option<&Vec<(GroupId, bool)>>, flags: &HashMap<GroupId, (usize, bool, bool)>) -> (Vec<GroupId>, bool, bool) {
    let Some(links) = links else { return (Vec::new(), false, false) };
    let mut v: Vec<&(GroupId, bool)> = links.iter().collect();
    v.sort_by_key(|(g, primary)| (!*primary, flags.get(g).map_or(usize::MAX, |f| f.0)));
    let all_hidden = !v.is_empty() && v.iter().all(|(g, _)| flags.get(g).is_some_and(|f| f.1));
    let any_locked = v.iter().any(|(g, _)| flags.get(g).is_some_and(|f| f.2));
    (v.into_iter().map(|(g, _)| *g).collect(), all_hidden, any_locked)
}

fn load_live(conn: &Connection, groups: Vec<Group>) -> Result<LiveIndex> {
    let flags = group_flags(&groups);
    let links = load_memberships(
        conn,
        "SELECT cg.channel_id, cg.group_id, cg.is_primary FROM channel_group cg",
        &flags,
    )?;
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.source_id, c.key, COALESCE(o.rename, c.clean_name), c.number, c.logo, ce.epg_channel_id, c.tvg_shift_min,
                c.catchup IS NOT NULL, COALESCE(json_extract(c.catchup, '$.days'), 0), c.radio, c.unsupported IS NOT NULL,
                c.provider_order, o.user_order, c.added_at, COALESCE(o.hidden, 0), COALESCE(o.locked, 0)
         FROM channel c JOIN source s ON s.id = c.source_id AND s.enabled = 1
         LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
         LEFT JOIN channel_epg ce ON ce.channel_id = c.id
         ORDER BY s.sort_index, s.id, (o.user_order IS NULL), o.user_order, c.provider_order, c.id",
    )?;
    let mut channels = Vec::new();
    let mut hidden = HashSet::new();
    let mut members: HashMap<GroupId, Vec<ChannelId>> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            ChannelSummary {
                id: r.get(0)?,
                source_id: r.get(1)?,
                key: ItemKey(r.get(2)?),
                name: Arc::from(r.get::<_, String>(3)?),
                number: r.get(4)?,
                logo: r.get::<_, Option<String>>(5)?.map(Arc::from),
                group_ids: Vec::new(),
                epg_channel_id: r.get(6)?,
                tvg_shift_min: r.get(7)?,
                has_catchup: r.get(8)?,
                catchup_days: u32_of(r.get(9)?),
                radio: r.get(10)?,
                locked: r.get(16)?,
                unsupported: r.get(11)?,
                provider_order: r.get(12)?,
                user_order: r.get(13)?,
                added_at: r.get(14)?,
            },
            r.get::<_, bool>(15)?,
        ))
    })?;
    for row in rows {
        let (mut c, item_hidden) = row?;
        let (gids, all_hidden, any_locked) = effective_groups(links.get(&c.id.0), &flags);
        c.locked |= any_locked;
        if item_hidden || all_hidden {
            hidden.insert(c.id);
        }
        for g in &gids {
            members.entry(*g).or_default().push(c.id);
        }
        c.group_ids = gids;
        channels.push(c);
    }
    Ok(LiveIndex::from_parts(channels, groups, members, hidden))
}

/// Group id → indices into the matching `VodIndex` vector.
type VodMembers = HashMap<GroupId, Vec<u32>>;

/// Visible movies or series + group members (indices into the returned vector).
fn load_vod(conn: &Connection, movies: bool, groups: &[Group]) -> Result<(Vec<VodSummary>, VodMembers)> {
    let flags = group_flags(groups);
    let (table, link, col, kind, poster) =
        if movies { ("movie", "movie_group", "movie_id", "movie", "poster") } else { ("series", "series_group", "series_id", "series", "cover") };
    let links = load_memberships(conn, &format!("SELECT l.{col}, l.group_id, 0 FROM {link} l"), &flags)?;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT x.id, x.source_id, x.key, COALESCE(o.rename, x.clean_name), x.year, x.{poster}, x.rating, x.added_at,
                x.provider_order, COALESCE(o.locked, 0), COALESCE(o.hidden, 0)
         FROM {table} x JOIN source s ON s.id = x.source_id AND s.enabled = 1
         LEFT JOIN item_override o ON o.source_id = x.source_id AND o.kind = '{kind}' AND o.item_key = x.key
         ORDER BY s.sort_index, s.id, (o.user_order IS NULL), o.user_order, x.provider_order, x.id"
    ))?;
    let mut items = Vec::new();
    let mut members = VodMembers::new();
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        Ok((
            VodSummary {
                kind: if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) },
                source_id: r.get(1)?,
                key: ItemKey(r.get(2)?),
                name: Arc::from(r.get::<_, String>(3)?),
                year: r.get(4)?,
                poster: r.get::<_, Option<String>>(5)?.map(Arc::from),
                rating: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
                group_ids: Vec::new(),
                added_at: r.get(7)?,
                provider_order: r.get(8)?,
                locked: r.get(9)?,
            },
            id,
            r.get::<_, bool>(10)?,
        ))
    })?;
    for row in rows {
        let (mut v, id, item_hidden) = row?;
        let (gids, all_hidden, any_locked) = effective_groups(links.get(&id), &flags);
        if item_hidden || all_hidden {
            continue;
        }
        v.locked |= any_locked;
        let idx = items.len() as u32;
        for g in &gids {
            members.entry(*g).or_default().push(idx);
        }
        v.group_ids = gids;
        items.push(v);
    }
    Ok((items, members))
}

/// Build a full snapshot from a reader connection (uses the `from_parts` constructors). Never
/// call on the UI thread.
///
/// Only enabled sources contribute channels/VOD/groups (`sources` lists every source).
/// Effective flags: an item is hidden when it is hidden itself or every one of its groups is
/// hidden; it is locked when it or any of its groups is locked.
pub fn load(conn: &Connection) -> Result<Library> {
    let sources = load_sources(conn)?;
    let (ch_groups, movie_groups, series_groups) = load_groups(conn)?;
    let live = load_live(conn, ch_groups)?;
    let (movies, mut members) = load_vod(conn, true, &movie_groups)?;
    let (series, series_members) = load_vod(conn, false, &series_groups)?;
    members.extend(series_members);
    let vod = VodIndex::from_parts(movies, series, movie_groups, series_groups, members);
    let favs = load_favs(conn)?;
    let recent_channels = load_recent_channels(conn, &live)?;
    Ok(Library { sources, live: Arc::new(live), vod: Arc::new(vod), favs, recent_channels, built_at: crate::model::now(), generation: next_generation() })
}

/// Distinct recently watched channels of `live`, most recent first (one aggregate over the
/// ≤ 500-row history table: cheap enough for the UI reader on `DataScope::History`).
pub fn load_recent_channels(conn: &Connection, live: &LiveIndex) -> Result<Vec<ChannelId>> {
    Ok(crate::store::user::recent_channels(conn, RECENT_CHANNELS)?.iter().filter_map(|h| live.by_ref(&h.item)).collect())
}

/// Rebuild only the favourites part (cheap; event-driven on the UI thread, < 5 ms).
pub fn load_favs(conn: &Connection) -> Result<FavIndex> {
    let lists = crate::store::user::fav_lists(conn)?;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {} FROM fav_item ORDER BY list_id, position, added_at",
        crate::store::user::FAV_ITEM_COLS
    ))?;
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
