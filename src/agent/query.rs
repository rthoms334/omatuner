//! The read-only tools (DESIGN §30.5) over the snapshot the app published and a store reader,
//! with the guards of §30.6. Owner: MCP-app track.
//!
//! Every function takes a [`Q`]: the snapshot as parental control wants it **now** (the
//! handler swaps in `Library::hiding_explicit` when the session relocked since the snapshot was
//! built), a reader, the parental view, the clock and the config. Visibility is the snapshot's:
//! a channel is visible when it is in `live.channels` and not in `live.hidden`; movies and
//! series in the VOD vectors are visible (hidden ones are never in them); hidden and empty
//! groups are left out; explicit content isn't in a hiding snapshot at all. Anything else is
//! `not_found`, worded like a real miss, and never counted.
//!
//! Locked (PIN) items stay in, flagged `locked` while the session is locked; `details` withholds
//! everything but their name then.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::proto::{
    self, AgentRef, Airing, Browse, BrowseOut, BrowseSort, Categories, CategoriesOut, CategoryOut, ChannelItemOut, ChannelOut, ContinueItem,
    ContinueOut, Details, DetailsOut, EpisodeOut, ErrorCode, FavListOut, Favourites, FavouritesOut, GuideSearch, GuideSearchOut, ItemOut, Kind,
    Limit, ProgrammeOut, ProgressOut, Search, SearchOut, SeasonOut, TitleOut, WhatsOn, WhatsOnOut, fmt_time, item_ref,
};
use crate::config::Config;
use crate::jobs::JobKind;
use crate::model::{
    ChannelId, ChannelSummary, Episode, GroupId, ItemKind, ItemRef, MovieId, ProgrammeSlim, SeriesId, SourceId, SourceKind, Timestamp, VodKind,
    VodSummary, WatchProgress,
};
use crate::shared::ParentalView;
use crate::store::library::{FavIndex, Library, SourceMeta};

/// A request that can't be answered: the error code and a sentence for the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub code: ErrorCode,
    pub message: String,
}

impl Fail {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }
    /// Missing — or concealed (the same words either way).
    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("No such {what} in Tuner (pass back a ref a tool returned, or search again)"))
    }
}

impl From<crate::error::Error> for Fail {
    fn from(e: crate::error::Error) -> Self {
        tracing::warn!("agent: store read failed: {e}");
        Fail::new(ErrorCode::Internal, "Tuner couldn't read its library just now; try again")
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        crate::error::Error::from(e).into()
    }
}

pub type Out<T> = std::result::Result<T, Fail>;

/// Free text of a param: trimmed, non-empty, at most `proto::MAX_TEXT` chars.
pub fn text<'a>(field: &str, s: &'a str) -> Out<&'a str> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Fail::bad(format!("{field} is empty")));
    }
    if t.chars().count() > proto::MAX_TEXT {
        return Err(Fail::bad(format!("{field} is longer than {} characters", proto::MAX_TEXT)));
    }
    Ok(t)
}

/// `limit` clamped to `1..=max` (`default` when absent).
pub fn clamp(limit: Option<u32>, default: u32, max: u32) -> usize {
    limit.unwrap_or(default).clamp(1, max) as usize
}

/// A time param (`proto::parse_when`) → UTC seconds.
pub fn when(field: &str, s: &str, now: Timestamp) -> Out<Timestamp> {
    proto::parse_when(text(field, s)?, now).map_err(Fail::bad)
}

/// Provider text (titles, descriptions, plots) without its links: a word holding `"://"`
/// becomes `[link]`. Programme descriptions and plots often carry a web address; the reply
/// check (`agent::guard::sanitize`) would otherwise withhold the whole answer.
pub fn clean(s: &str) -> String {
    if !s.contains("://") {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(if word.contains("://") { "[link]" } else { word });
        word.clear();
    };
    for ch in s.chars() {
        if ch.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// [`clean`] for an optional text, dropping blank ones.
fn clean_opt(s: Option<&str>) -> Option<String> {
    s.map(clean).filter(|t| !t.trim().is_empty())
}

/// Lower-case words of a name query.
fn words(q: &str) -> Vec<String> {
    q.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// Every word of the query is in `name` (case-insensitive).
fn matches_words(name: &str, words: &[String]) -> bool {
    let name = name.to_lowercase();
    !words.is_empty() && words.iter().all(|w| name.contains(w.as_str()))
}

/// Watch progress the tools show, read in two queries per request.
#[derive(Default)]
pub struct Progress {
    movies: HashMap<(SourceId, String), WatchProgress>,
    /// Newest episode progress per series.
    series: HashMap<SeriesId, ProgressOut>,
    /// `updated_at` of `series` (to keep the newest).
    series_at: HashMap<SeriesId, Timestamp>,
}

fn progress_out(position_s: f64, duration_s: Option<f64>, finished: bool, updated_at: Timestamp) -> ProgressOut {
    ProgressOut {
        position_min: (position_s.max(0.0) / 60.0) as u32,
        duration_min: duration_s.filter(|d| *d > 0.0).map(|d| (d / 60.0).round() as u32),
        finished,
        watched: fmt_time(updated_at),
    }
}

impl Progress {
    fn load(conn: &Connection) -> Out<Self> {
        let mut p = Progress::default();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM watch_progress WHERE kind = 'movie' AND orphaned_since IS NULL",
            crate::store::user::PROGRESS_COLS
        ))?;
        for row in stmt.query_map([], crate::store::user::progress_from_row)? {
            let row = row?;
            p.movies.insert((row.item.source, row.item.key.0.clone()), row);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT e.series_id, p.position_s, p.duration_s, p.finished, p.updated_at FROM watch_progress p
             JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
             WHERE p.kind = 'episode' AND p.orphaned_since IS NULL",
        )?;
        let rows =
            stmt.query_map([], |r| Ok((SeriesId(r.get(0)?), r.get::<_, f64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, bool>(3)?, r.get(4)?)))?;
        for row in rows {
            let (s, pos, dur, fin, at): (SeriesId, f64, Option<f64>, bool, Timestamp) = row?;
            if p.series_at.get(&s).is_none_or(|t| at > *t) {
                p.series_at.insert(s, at);
                p.series.insert(s, progress_out(pos, dur, fin, at));
            }
        }
        Ok(p)
    }

    /// (progress, its time) of one movie / series copy.
    fn of(&self, v: &VodSummary) -> Option<(ProgressOut, Timestamp)> {
        match v.kind {
            VodKind::Movie(_) => {
                let w = self.movies.get(&(v.source_id, v.key.0.clone()))?;
                Some((progress_out(w.position_s, w.duration_s, w.finished, w.updated_at), w.updated_at))
            }
            VodKind::Series(id) => Some((self.series.get(&id)?.clone(), *self.series_at.get(&id)?)),
        }
    }
}

/// Everything a query reads (one request).
pub struct Q<'a> {
    pub lib: &'a Library,
    pub conn: &'a Connection,
    pub view: ParentalView,
    pub now: Timestamp,
    pub cfg: &'a Config,
    favs: OnceCell<FavIndex>,
    groups: OnceCell<HashMap<GroupId, (String, bool)>>,
    progress: OnceCell<Progress>,
    runtimes: OnceCell<HashMap<MovieId, u32>>,
    /// Playlists of channels the user keeps out of All (`in_all`).
    out_of_all: OnceCell<Vec<SourceId>>,
}

impl<'a> Q<'a> {
    pub fn new(lib: &'a Library, conn: &'a Connection, view: ParentalView, now: Timestamp, cfg: &'a Config) -> Self {
        Self {
            lib,
            conn,
            view,
            now,
            cfg,
            favs: OnceCell::new(),
            groups: OnceCell::new(),
            progress: OnceCell::new(),
            runtimes: OnceCell::new(),
            out_of_all: OnceCell::new(),
        }
    }

    /// The favourites now (the snapshot's may be older than a toggle).
    pub fn favs(&self) -> &FavIndex {
        self.favs.get_or_init(|| {
            crate::store::library::load_favs(self.conn).unwrap_or_else(|e| {
                tracing::warn!("agent: favourites: {e}");
                self.lib.favs.clone()
            })
        })
    }

    fn progress(&self) -> &Progress {
        self.progress.get_or_init(|| {
            Progress::load(self.conn).unwrap_or_else(|e| {
                tracing::warn!("agent: progress: {}", e.message);
                Progress::default()
            })
        })
    }

    /// Known movie runtimes in minutes.
    fn runtimes(&self) -> &HashMap<MovieId, u32> {
        self.runtimes.get_or_init(|| {
            let read = || -> rusqlite::Result<HashMap<MovieId, u32>> {
                let mut stmt = self.conn.prepare_cached("SELECT id, duration_s FROM movie WHERE duration_s > 0")?;
                stmt.query_map([], |r| Ok((MovieId(r.get(0)?), (r.get::<_, i64>(1)? as f64 / 60.0).round() as u32)))?.collect()
            };
            read().unwrap_or_else(|e| {
                tracing::warn!("agent: runtimes: {e}");
                HashMap::new()
            })
        })
    }

    /// Group id → (display name, hidden), every kind.
    fn groups(&self) -> &HashMap<GroupId, (String, bool)> {
        self.groups.get_or_init(|| {
            let lib = self.lib;
            lib.live
                .groups
                .iter()
                .chain(&lib.vod.movie_groups)
                .chain(&lib.vod.series_groups)
                .map(|g| (g.id, (g.display_name().to_owned(), g.hidden)))
                .collect()
        })
    }

    /// The first visible group's name of an item.
    fn group_name(&self, ids: &[GroupId]) -> Option<String> {
        ids.iter().filter_map(|g| self.groups().get(g)).find(|(_, hidden)| !hidden).map(|(n, _)| clean(n))
    }

    /// Hidden by parental control now: in the snapshot's explicit index, or (episodes, items
    /// newer than the snapshot) explicit in the store while explicit content is hidden. An item
    /// that can't be read counts as hidden then.
    pub fn conceals(&self, item: &ItemRef) -> bool {
        self.lib.conceals(item) || (self.view.hides_explicit && crate::store::live::item_explicit(self.conn, item).unwrap_or(true))
    }

    /// Needs the PIN now.
    pub fn needs_pin(&self, locked: bool) -> bool {
        locked && !self.view.unlocked
    }

    fn source_name(&self, s: SourceId) -> String {
        self.lib.source(s).map(|m| clean(&m.name)).unwrap_or_default()
    }

    // ---------------------------------------------------------------- channels

    /// A visible channel.
    pub fn channel(&self, id: ChannelId) -> Option<&'a ChannelSummary> {
        let live = &self.lib.live;
        live.get(id).filter(|c| !live.hidden.contains(&c.id))
    }

    pub fn channel_by_ref(&self, r: &ItemRef) -> Option<&'a ChannelSummary> {
        self.channel(self.lib.live.by_ref(r)?)
    }

    pub fn channel_out(&self, c: &ChannelSummary) -> ChannelOut {
        let r = channel_ref(c);
        ChannelOut {
            item: item_ref(&r),
            name: clean(&c.name),
            number: c.number,
            playlist: self.source_name(c.source_id),
            group: self.group_name(&c.group_ids),
            quality: c.quality.map(|q| q.label().to_owned()),
            catchup_days: catchup_days(c),
            favourite: self.favs().is_fav(&r),
            locked: self.needs_pin(c.locked),
        }
    }

    fn channel_item(&self, c: &ChannelSummary) -> ItemOut {
        let now = self.now_on(c, self.now).ok().flatten().map(|p| clean(&p.title));
        ItemOut::Channel(ChannelItemOut { kind: Kind::Channel, channel: self.channel_out(c), now })
    }

    /// The programme on `c` at `at` (shifted), if the guide knows.
    pub fn now_on(&self, c: &ChannelSummary, at: Timestamp) -> Out<Option<ProgrammeSlim>> {
        let Some(epg) = c.epg_channel_id else { return Ok(None) };
        let nn = crate::store::epg::now_next(self.conn, &[(epg, c.tvg_shift_min)], at)?;
        Ok(nn.into_values().next().and_then(|n| n.now).filter(|p| p.contains(at)))
    }

    /// The programme of `c` starting at `start` (as shown on `c`, shifted).
    pub fn programme_at(&self, c: &ChannelSummary, start: Timestamp) -> Out<Option<ProgrammeSlim>> {
        let Some(epg) = c.epg_channel_id else { return Ok(None) };
        let t = start - i64::from(c.tvg_shift_min) * 60;
        let rows = crate::store::epg::window(self.conn, epg, t, t + 1)?;
        Ok(rows.into_iter().find(|p| p.start == t).map(|p| p.shifted(c.tvg_shift_min)))
    }

    pub fn airing(&self, p: &ProgrammeSlim) -> Airing {
        if p.stop <= self.now {
            Airing::Past
        } else if p.start <= self.now {
            Airing::Live
        } else {
            Airing::Upcoming
        }
    }

    /// `p` (shifted, on `c`) can be played from the archive: ended inside the window, or live
    /// and restartable.
    pub fn catchup(&self, c: &ChannelSummary, p: &ProgrammeSlim) -> bool {
        let days = catchup_days(c);
        days > 0 && p.start < self.now && p.start >= self.now - i64::from(days) * 86_400
    }

    pub fn programme_out(&self, c: &ChannelSummary, p: &ProgrammeSlim, also_on: Vec<ChannelOut>) -> ProgrammeOut {
        ProgrammeOut {
            item: AgentRef::programme(channel_ref(c), p.start).to_string(),
            title: clean(&p.title),
            subtitle: clean_opt(p.sub_title.as_deref()),
            start: fmt_time(p.start),
            end: fmt_time(p.stop),
            airing: self.airing(p),
            catchup: self.catchup(c, p),
            channel: self.channel_out(c),
            also_on,
        }
    }

    /// `c` is in a playlist the user keeps out of All.
    fn out_of_all(&self, c: &ChannelSummary) -> bool {
        self.out_of_all.get_or_init(|| crate::store::library::out_of_all_of(&self.lib.eligible(ItemKind::Channel))).contains(&c.source_id)
    }

    /// Preference among channels showing the same programme: in All, favourite, playable
    /// without the PIN, best quality, display order.
    fn channel_rank(&self, c: &ChannelSummary) -> (bool, bool, bool, i8, ChannelId) {
        let q = c.quality.map_or(-10, |q| 2 * q.class as i8 - i8::from(q.doubtful));
        (self.out_of_all(c), !self.favs().is_fav(&channel_ref(c)), self.needs_pin(c.locked), -q, c.id)
    }

    // ---------------------------------------------------------------- movies / series

    fn items(&self, movies: bool) -> &'a [VodSummary] {
        if movies { &self.lib.vod.movies } else { &self.lib.vod.series }
    }

    /// The copy a title's card shows: its stack's top, or itself.
    pub fn stack_key(&self, movies: bool, idx: u32) -> u32 {
        let st = self.lib.vod.stacks(movies);
        st.stack_of(idx).map_or(idx, |s| st.top(s))
    }

    /// Every copy of a title is locked (the stack card's lock).
    pub fn title_locked(&self, movies: bool, idx: u32) -> bool {
        let items = self.items(movies);
        self.lib.vod.stacks(movies).copies(idx).iter().all(|c| items.get(*c as usize).is_some_and(|v| v.locked))
    }

    pub fn title_out(&self, movies: bool, idx: u32) -> TitleOut {
        let items = self.items(movies);
        let v = &items[idx as usize];
        let copies = self.lib.vod.stacks(movies).copies(idx);
        let progress = copies.iter().filter_map(|c| self.progress().of(&items[*c as usize])).max_by_key(|(_, at)| *at).map(|(p, _)| p);
        let favs = self.favs();
        let favourite = copies.iter().any(|c| favs.is_fav(&crate::store::copies::vod_ref(&items[*c as usize])));
        let runtime_min = match v.kind {
            VodKind::Movie(id) => self.runtimes().get(&id).copied(),
            VodKind::Series(_) => None,
        };
        TitleOut {
            item: item_ref(&crate::store::copies::vod_ref(v)),
            kind: if movies { Kind::Movie } else { Kind::Series },
            name: clean(&v.name),
            year: v.year,
            rating: v.rating.filter(|r| *r > 0.0),
            runtime_min,
            playlist: self.source_name(v.source_id),
            group: self.group_name(&v.group_ids),
            other_copies: copies.len().saturating_sub(1) as u32,
            progress,
            favourite,
            locked: self.needs_pin(self.title_locked(movies, idx)),
        }
    }

    /// The index of a movie (`movies`) or series ref in the snapshot (visible ones only).
    pub fn vod_index(&self, r: &ItemRef) -> Out<Option<u32>> {
        let (table, movies) = match r.kind {
            ItemKind::Movie => ("movie", true),
            ItemKind::Series => ("series", false),
            _ => return Ok(None),
        };
        let id: Option<i64> = self
            .conn
            .prepare_cached(&format!("SELECT id FROM {table} WHERE source_id = ?1 AND key = ?2"))?
            .query_row(params![r.source, r.key.as_str()], |row| row.get(0))
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        let kind = if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) };
        Ok(self.lib.vod.index_of(kind))
    }

    /// An episode ref → the episode and its (visible) series' index.
    pub fn episode(&self, r: &ItemRef) -> Out<Option<(Episode, u32)>> {
        let Some(id) = crate::store::vod::resolve_episode(self.conn, r)? else { return Ok(None) };
        let ep = match crate::store::vod::episode(self.conn, id) {
            Ok(e) => e,
            Err(crate::error::Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(self.lib.vod.index_of(VodKind::Series(ep.series_id)).map(|idx| (ep, idx)))
    }

    /// The series ids of a title's copies, the copy that plays first first.
    pub fn series_copies(&self, idx: u32) -> Vec<SeriesId> {
        let items = self.items(false);
        let mut ids: Vec<u32> = self.lib.vod.stacks(false).copies(idx);
        ids.sort_by_key(|c| *c != idx);
        ids.into_iter()
            .filter_map(|c| match items.get(c as usize)?.kind {
                VodKind::Series(id) => Some(id),
                VodKind::Movie(_) => None,
            })
            .collect()
    }

    pub fn episode_out(&self, ep: &Episode) -> EpisodeOut {
        let r = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
        let progress =
            crate::store::user::progress(self.conn, &r).ok().flatten().map(|w| progress_out(w.position_s, w.duration_s, w.finished, w.updated_at));
        EpisodeOut { item: item_ref(&r), season: ep.season, episode: ep.episode, title: clean(&ep.title), progress }
    }

    /// The playlists of a kind a request covers: the named one, or All (`in_all` honoured).
    fn scope(&self, kind: ItemKind, playlist: Option<&str>) -> Out<(Vec<&'a SourceMeta>, bool)> {
        let eligible = self.lib.eligible(kind);
        match playlist {
            Some(p) => {
                let w = words(text("playlist", p)?);
                let hit: Vec<&SourceMeta> = eligible.into_iter().filter(|s| matches_words(&s.name, &w)).collect();
                if hit.is_empty() {
                    return Err(Fail::not_found("playlist"));
                }
                Ok((hit, false))
            }
            None => {
                let out = crate::store::library::out_of_all_of(&eligible);
                Ok((eligible.into_iter().filter(|s| !out.contains(&s.id)).collect(), true))
            }
        }
    }
}

pub fn channel_ref(c: &ChannelSummary) -> ItemRef {
    ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }
}

pub fn catchup_days(c: &ChannelSummary) -> u32 {
    if c.has_catchup { c.catchup_days } else { 0 }
}

/// A ref param → [`AgentRef`].
pub fn parse_ref(s: &str) -> Out<AgentRef> {
    text("ref", s)?.parse::<AgentRef>().map_err(Fail::bad)
}

// ------------------------------------------------------------------ guide_search

/// FTS hits read before visibility / time filtering and merging (`truncated` when reached: hidden
/// channels' matches may have taken the room).
const GUIDE_FETCH: usize = 500;
/// Other channels listed with a programme.
const MAX_ALSO_ON: usize = 10;

pub fn guide_search(q: &Q, p: &GuideSearch) -> Out<GuideSearchOut> {
    let query = text("query", &p.query)?;
    let limit = clamp(p.limit, 20, 50);
    let from = p.from.as_deref().map(|s| when("from", s, q.now)).transpose()?;
    let to = p.to.as_deref().map(|s| when("to", s, q.now)).transpose()?;
    let include_past = p.include_past.unwrap_or(true);
    let hits = crate::store::epg::search(q.conn, query, q.now, GUIDE_FETCH)?;
    // one row per programme as shown (title + shifted start): its channels best first
    let mut rows: Vec<(ProgrammeSlim, Vec<&ChannelSummary>)> = Vec::new();
    let mut index: HashMap<(String, Timestamp), usize> = HashMap::new();
    for hit in &hits {
        for c in hit.channel_ids.iter().filter_map(|id| q.channel(*id)) {
            let shown = hit.programme.shifted(c.tvg_shift_min);
            let key = (shown.title.to_lowercase(), shown.start);
            match index.get(&key) {
                Some(&i) => {
                    if !rows[i].1.iter().any(|x| x.id == c.id) {
                        rows[i].1.push(c);
                    }
                }
                None => {
                    index.insert(key, rows.len());
                    rows.push((shown, vec![c]));
                }
            }
        }
    }
    rows.retain(|(prog, _)| {
        let past = q.airing(prog) == Airing::Past;
        !(past && !include_past) && from.is_none_or(|f| prog.stop > f) && to.is_none_or(|t| prog.start < t)
    });
    // live, then upcoming soonest first, then past most recent first (as shown, shifts applied)
    rows.sort_by_key(|(prog, _)| match q.airing(prog) {
        Airing::Live => (0, prog.start),
        Airing::Upcoming => (1, prog.start),
        Airing::Past => (2, -prog.start),
    });
    let truncated = rows.len() > limit || hits.len() >= GUIDE_FETCH;
    let programmes = rows
        .into_iter()
        .take(limit)
        .map(|(prog, mut chans)| {
            chans.sort_by_key(|c| q.channel_rank(c));
            let also_on = chans[1..].iter().take(MAX_ALSO_ON).map(|c| q.channel_out(c)).collect();
            q.programme_out(chans[0], &prog, also_on)
        })
        .collect();
    Ok(GuideSearchOut { programmes, truncated })
}

// ------------------------------------------------------------------ whats_on

/// Channels `whats_on` looks at, at most.
const WHATS_ON_SCAN: usize = 1500;

pub fn whats_on(q: &Q, p: &WhatsOn) -> Out<WhatsOnOut> {
    let at = p.at.as_deref().map(|s| when("at", s, q.now)).transpose()?.unwrap_or(q.now);
    let limit = clamp(p.limit, 30, 100);
    let name = p.channel.as_deref().map(|s| text("channel", s)).transpose()?;
    let number = name.and_then(|n| n.parse::<u32>().ok());
    let name_words = name.map(words).unwrap_or_default();
    let group_words = p.group.as_deref().map(|s| text("group", s).map(words)).transpose()?;
    let groups: Option<HashSet<GroupId>> =
        group_words.map(|w| q.lib.live.groups.iter().filter(|g| !g.hidden && matches_words(g.display_name(), &w)).map(|g| g.id).collect());
    if groups.as_ref().is_some_and(HashSet::is_empty) {
        return Err(Fail::not_found("channel category"));
    }
    // like the guide's All playlists, unless a channel is named
    let scope: Option<HashSet<SourceId>> =
        name.is_none().then(|| q.scope(ItemKind::Channel, None)).transpose()?.map(|(s, _)| s.iter().map(|s| s.id).collect());
    // favourites (list order), recently watched, then display order
    let favs = q.favs();
    let mut order: Vec<ChannelId> = Vec::new();
    let mut seen: HashSet<ChannelId> = HashSet::new();
    let fav_ids: Vec<ChannelId> =
        favs.non_empty_lists().flat_map(|l| favs.items(l.id)).filter(|r| r.kind == ItemKind::Channel).filter_map(|r| q.lib.live.by_ref(r)).collect();
    let favourites_only = p.favourites_only.unwrap_or(false);
    let rest: Box<dyn Iterator<Item = ChannelId>> = if favourites_only {
        Box::new(std::iter::empty())
    } else {
        Box::new(q.lib.recent_channels.iter().copied().chain(q.lib.live.channels.iter().map(|c| c.id)))
    };
    for id in fav_ids.into_iter().chain(rest) {
        let Some(c) = q.channel(id) else { continue };
        if c.epg_channel_id.is_none() || scope.as_ref().is_some_and(|s| !s.contains(&c.source_id)) || !seen.insert(id) {
            continue;
        }
        let named = name.is_none() || number.is_some_and(|n| c.number == Some(n)) || matches_words(&c.name, &name_words);
        let grouped = groups.as_ref().is_none_or(|g| c.group_ids.iter().any(|id| g.contains(id)));
        if named && grouped {
            order.push(id);
        }
    }
    if name.is_some() && order.is_empty() {
        return Err(Fail::not_found("channel with a guide"));
    }
    let mut programmes = Vec::new();
    let mut looked = 0;
    for id in &order {
        if programmes.len() == limit || looked == WHATS_ON_SCAN {
            break;
        }
        looked += 1;
        let Some(c) = q.channel(*id) else { continue };
        if let Some(prog) = q.now_on(c, at)? {
            programmes.push(q.programme_out(c, &prog, Vec::new()));
        }
    }
    Ok(WhatsOnOut { at: fmt_time(at), programmes, truncated: looked < order.len() })
}

// ------------------------------------------------------------------ search

pub fn search(q: &Q, p: &Search) -> Out<SearchOut> {
    use nucleo_matcher::Utf32Str;
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    let query = text("query", &p.query)?;
    let limit = clamp(p.limit, 20, 50);
    let kinds: Vec<Kind> = p.kinds.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| vec![Kind::Channel, Kind::Movie, Kind::Series]);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut buf = Vec::new();
    // (score, kind order, position, candidate)
    let mut hits: Vec<(u32, u8, usize, Cand)> = Vec::new();
    let mut score = |name: &str| pattern.score(Utf32Str::new(name, &mut buf), &mut matcher);
    if kinds.contains(&Kind::Channel) {
        for (i, c) in q.lib.live.channels.iter().enumerate().filter(|(_, c)| !q.lib.live.hidden.contains(&c.id)) {
            if let Some(s) = score(&c.name) {
                // a playlist kept out of All: after every other match of the same score (the same
                // channel in an All playlist first)
                let order = if q.out_of_all(c) { 3 } else { 0 };
                hits.push((s, order, i, Cand::Channel(c.id)));
            }
        }
    }
    for (movies, kind, rank) in [(true, Kind::Movie, 1u8), (false, Kind::Series, 2u8)] {
        if !kinds.contains(&kind) {
            continue;
        }
        for (i, v) in q.items(movies).iter().enumerate() {
            let i = i as u32;
            if q.stack_key(movies, i) != i {
                continue; // one row per title: its card's copy
            }
            if let Some(s) = score(&v.name) {
                hits.push((s, rank, i as usize, Cand::Title(movies, i)));
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let results = hits
        .into_iter()
        .take(limit)
        .filter_map(|(_, _, _, c)| match c {
            Cand::Channel(id) => q.channel(id).map(|c| q.channel_item(c)),
            Cand::Title(movies, i) => Some(ItemOut::Title(q.title_out(movies, i))),
        })
        .collect();
    Ok(SearchOut { results })
}

#[derive(Debug, Clone, Copy)]
enum Cand {
    Channel(ChannelId),
    Title(bool, u32),
}

// ------------------------------------------------------------------ categories / browse

fn item_kind(k: Kind) -> ItemKind {
    match k {
        Kind::Channel => ItemKind::Channel,
        Kind::Movie => ItemKind::Movie,
        Kind::Series => ItemKind::Series,
    }
}

pub fn categories(q: &Q, p: &Categories) -> Out<CategoriesOut> {
    let (sources, _) = q.scope(item_kind(p.kind), p.playlist.as_deref())?;
    let ids: HashSet<SourceId> = sources.iter().map(|s| s.id).collect();
    let groups = match p.kind {
        Kind::Channel => &q.lib.live.groups,
        Kind::Movie => &q.lib.vod.movie_groups,
        Kind::Series => &q.lib.vod.series_groups,
    };
    let categories = groups
        .iter()
        .filter(|g| !g.hidden && ids.contains(&g.source_id))
        .filter_map(|g| {
            let count = match p.kind {
                Kind::Channel => q.lib.live.group_members(g.id).len(),
                _ => q.lib.vod.group_items(g.id, crate::model::SortMode::Provider).len(),
            };
            (count > 0).then(|| CategoryOut {
                name: clean(g.display_name()),
                playlist: q.source_name(g.source_id),
                count: count as u32,
                locked: q.needs_pin(g.locked),
            })
        })
        .collect();
    Ok(CategoriesOut { categories })
}

/// Visible groups of `kind` in `sources` whose name has every word of `group`.
fn matching_groups(q: &Q, kind: Kind, sources: &HashSet<SourceId>, group: &str) -> Out<HashSet<GroupId>> {
    let w = words(text("group", group)?);
    let groups = match kind {
        Kind::Channel => &q.lib.live.groups,
        Kind::Movie => &q.lib.vod.movie_groups,
        Kind::Series => &q.lib.vod.series_groups,
    };
    let hit: HashSet<GroupId> =
        groups.iter().filter(|g| !g.hidden && sources.contains(&g.source_id) && matches_words(g.display_name(), &w)).map(|g| g.id).collect();
    if hit.is_empty() {
        return Err(Fail::not_found("category (the categories tool lists them)"));
    }
    Ok(hit)
}

/// A small xorshift seeded from the clock: `random` sorts differ call to call.
fn shuffle<T>(v: &mut [T]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SALT: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    let mut x = (nanos ^ SALT.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)) | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

pub fn browse(q: &Q, p: &Browse) -> Out<BrowseOut> {
    let limit = clamp(p.limit, 20, 50);
    let offset = p.offset.unwrap_or(0) as usize;
    let sort = p.sort.unwrap_or(BrowseSort::Recent);
    let (sources, all) = q.scope(item_kind(p.kind), p.playlist.as_deref())?;
    let ids: HashSet<SourceId> = sources.iter().map(|s| s.id).collect();
    let groups = p.group.as_deref().map(|g| matching_groups(q, p.kind, &ids, g)).transpose()?;
    if let Some(r) = p.min_rating
        && !(0.0..=10.0).contains(&r)
    {
        return Err(Fail::bad("min_rating is 0–10"));
    }
    let in_groups = |g: &[GroupId]| groups.as_ref().is_none_or(|set| g.iter().any(|id| set.contains(id)));
    if p.kind == Kind::Channel {
        let mut chans: Vec<&ChannelSummary> = q
            .lib
            .live
            .channels
            .iter()
            .filter(|c| !q.lib.live.hidden.contains(&c.id) && ids.contains(&c.source_id) && in_groups(&c.group_ids))
            .collect();
        match sort {
            BrowseSort::Name => chans.sort_by_cached_key(|c| c.name.to_lowercase()),
            BrowseSort::Recent => chans.sort_by_key(|c| std::cmp::Reverse(c.added_at.unwrap_or(i64::MIN))),
            BrowseSort::Random => shuffle(&mut chans),
            BrowseSort::Rating | BrowseSort::Year => {}
        }
        let total = chans.len() as u32;
        let skip = if sort == BrowseSort::Random { 0 } else { offset };
        let items = chans.into_iter().skip(skip).take(limit).map(|c| q.channel_item(c)).collect();
        return Ok(BrowseOut { items, total });
    }
    let movies = p.kind == Kind::Movie;
    let all_items = q.items(movies);
    let mut seen: HashSet<u32> = HashSet::new();
    let mut picked: Vec<u32> = Vec::new();
    for (i, v) in all_items.iter().enumerate() {
        if !ids.contains(&v.source_id) || !in_groups(&v.group_ids) {
            continue;
        }
        // All: one row per title (its card's copy); one playlist: its own copy
        let key = if all { q.stack_key(movies, i as u32) } else { i as u32 };
        if seen.insert(key) {
            picked.push(key);
        }
    }
    let progress = q.progress();
    let st = q.lib.vod.stacks(movies);
    picked.retain(|&i| {
        let v = &all_items[i as usize];
        if p.unwatched == Some(true) && st.copies(i).iter().any(|c| progress.of(&all_items[*c as usize]).is_some()) {
            return false;
        }
        if let Some(r) = p.min_rating
            && v.rating.is_none_or(|x| x < r)
        {
            return false;
        }
        if p.year_from.is_some_and(|y| v.year.is_none_or(|x| x < y)) || p.year_to.is_some_and(|y| v.year.is_none_or(|x| x > y)) {
            return false;
        }
        true
    });
    let v = |i: &u32| &all_items[*i as usize];
    match sort {
        BrowseSort::Recent => picked.sort_by_key(|i| std::cmp::Reverse(v(i).added_at.unwrap_or(i64::MIN))),
        BrowseSort::Rating => picked.sort_by(|a, b| v(b).rating.unwrap_or(-1.0).total_cmp(&v(a).rating.unwrap_or(-1.0))),
        BrowseSort::Year => picked.sort_by_key(|i| std::cmp::Reverse(v(i).year.unwrap_or(0))),
        BrowseSort::Name => picked.sort_by_cached_key(|i| v(i).name.to_lowercase()),
        BrowseSort::Random => shuffle(&mut picked),
    }
    if let Some(max) = p.max_minutes.filter(|_| movies) {
        let runtimes = q.runtimes();
        let runtime = |i: &u32| match v(i).kind {
            VodKind::Movie(id) => runtimes.get(&id).copied(),
            VodKind::Series(_) => None,
        };
        picked.retain(|i| runtime(i).is_none_or(|m| m <= max));
        // known runtimes first (stable: the sort above holds inside each part)
        picked.sort_by_key(|i| runtime(i).is_none());
    }
    let total = picked.len() as u32;
    let skip = if sort == BrowseSort::Random { 0 } else { offset };
    let items = picked.into_iter().skip(skip).take(limit).map(|i| ItemOut::Title(q.title_out(movies, i))).collect();
    Ok(BrowseOut { items, total })
}

// ------------------------------------------------------------------ continue / favourites

/// Progress rows read for `continue_watching` (episodes of one series collapse).
const CONTINUE_FETCH: usize = 200;

pub fn continue_watching(q: &Q, p: &Limit) -> Out<ContinueOut> {
    let limit = clamp(p.limit, 10, 30);
    let rows = crate::store::user::continue_watching(q.conn, CONTINUE_FETCH)?;
    let mut seen: HashSet<(bool, u32)> = HashSet::new();
    let mut items = Vec::new();
    for row in rows {
        if items.len() == limit {
            break;
        }
        if q.lib.conceals(&row.item) {
            continue;
        }
        match row.item.kind {
            ItemKind::Movie => {
                let Some(idx) = q.vod_index(&row.item)? else { continue };
                let key = q.stack_key(true, idx);
                if seen.insert((true, key)) {
                    items.push(ContinueItem { title: q.title_out(true, key), next_episode: None });
                }
            }
            ItemKind::Episode => {
                let Some((_, idx)) = q.episode(&row.item)? else { continue };
                let key = q.stack_key(false, idx);
                if !seen.insert((false, key)) {
                    continue;
                }
                let next = crate::store::copies::next_episode(q.conn, &q.series_copies(key))?.map(|(_, e)| q.episode_out(&e));
                items.push(ContinueItem { title: q.title_out(false, key), next_episode: next });
            }
            _ => {}
        }
    }
    Ok(ContinueOut { items })
}

pub fn favourites(q: &Q, p: &Favourites) -> Out<FavouritesOut> {
    let limit = clamp(p.limit, 50, 100);
    let list_words = p.list.as_deref().map(|l| text("list", l).map(words)).transpose()?;
    let favs = q.favs();
    let lists: Vec<_> = favs.lists.iter().filter(|l| list_words.as_ref().is_none_or(|w| matches_words(&l.name, w))).collect();
    if lists.is_empty() && list_words.is_some() {
        return Err(Fail::not_found("favourites list"));
    }
    let mut out = Vec::new();
    for l in lists {
        let mut items = Vec::new();
        let mut seen: HashSet<(bool, u32)> = HashSet::new();
        for r in favs.items(l.id) {
            if items.len() == limit {
                break;
            }
            let wanted = |k: Kind| p.kind.is_none_or(|x| x == k);
            match r.kind {
                ItemKind::Channel if wanted(Kind::Channel) => {
                    if let Some(c) = q.channel_by_ref(r) {
                        items.push(q.channel_item(c));
                    }
                }
                ItemKind::Movie | ItemKind::Series if wanted(if r.kind == ItemKind::Movie { Kind::Movie } else { Kind::Series }) => {
                    let movies = r.kind == ItemKind::Movie;
                    if let Some(idx) = q.vod_index(r)? {
                        let key = q.stack_key(movies, idx);
                        if seen.insert((movies, key)) {
                            items.push(ItemOut::Title(q.title_out(movies, key)));
                        }
                    }
                }
                _ => {}
            }
        }
        if !items.is_empty() {
            out.push(FavListOut { name: clean(&l.name), items });
        }
    }
    Ok(FavouritesOut { lists: out })
}

// ------------------------------------------------------------------ details

/// Upcoming programmes a channel's details list after now / next.
const CHANNEL_UPCOMING: usize = 5;

/// `details`, and the job that would fetch a title's missing details (the handler submits it
/// and asks again for up to 3 s; `details_pending` says it is still loading).
pub fn details(q: &Q, p: &Details) -> Out<(DetailsOut, Option<JobKind>)> {
    match parse_ref(&p.item)? {
        AgentRef::Programme { channel, start } => {
            let c = q.channel_by_ref(&channel).ok_or_else(|| Fail::not_found("programme"))?;
            let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
            let mut out = DetailsOut {
                item: p.item.trim().to_owned(),
                kind: "programme".into(),
                name: clean(&prog.title),
                locked: q.needs_pin(c.locked),
                ..Default::default()
            };
            if out.locked {
                return Ok((out, None));
            }
            let full = crate::store::epg::programme(q.conn, prog.id)?;
            out.description = clean_opt(full.description.as_deref());
            out.categories = full.categories.iter().map(|c| clean(c)).collect();
            out.rating = full.star_rating.as_deref().and_then(star_rating);
            out.channel = Some(q.channel_out(c));
            out.schedule = vec![q.programme_out(c, &prog, Vec::new())];
            Ok((out, None))
        }
        AgentRef::Item(r) => match r.kind {
            ItemKind::Channel => {
                let c = q.channel_by_ref(&r).ok_or_else(|| Fail::not_found("channel"))?;
                let mut out = DetailsOut {
                    item: item_ref(&r),
                    kind: "channel".into(),
                    name: clean(&c.name),
                    locked: q.needs_pin(c.locked),
                    ..Default::default()
                };
                if out.locked {
                    return Ok((out, None));
                }
                out.channel = Some(q.channel_out(c));
                if let Some(epg) = c.epg_channel_id {
                    let shift = i64::from(c.tvg_shift_min) * 60;
                    let mut sched: Vec<ProgrammeSlim> = q.now_on(c, q.now)?.into_iter().collect();
                    let from = sched.last().map_or(q.now, |p| p.stop);
                    for p in crate::store::epg::upcoming(q.conn, epg, from - shift, CHANNEL_UPCOMING + 1)? {
                        sched.push(p.shifted(c.tvg_shift_min));
                    }
                    out.schedule = sched.iter().take(CHANNEL_UPCOMING + 1).map(|p| q.programme_out(c, p, Vec::new())).collect();
                }
                Ok((out, None))
            }
            ItemKind::Movie => {
                let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("movie"))?;
                let v = &q.lib.vod.movies[idx as usize];
                let title = q.title_out(true, idx);
                let mut out =
                    DetailsOut { item: item_ref(&r), kind: "movie".into(), name: clean(&v.name), locked: title.locked, ..Default::default() };
                if out.locked {
                    return Ok((out, None));
                }
                let VodKind::Movie(id) = v.kind else { return Err(Fail::not_found("movie")) };
                let m = crate::store::vod::movie(q.conn, id)?;
                let lazy = m.details_loaded_at.is_none() && q.lib.source(m.source_id).is_some_and(|s| s.kind == SourceKind::Xtream);
                out.year = m.year.or(v.year);
                out.rating = m.rating.or(v.rating).filter(|r| *r > 0.0);
                out.genre = clean_opt(m.genre.as_deref());
                out.plot = clean_opt(m.plot.as_deref());
                out.cast = clean_opt(m.cast.as_deref());
                out.director = clean_opt(m.director.as_deref());
                out.runtime_min = m.duration_s.filter(|d| *d > 0).map(|d| (f64::from(d) / 60.0).round() as u32).or(title.runtime_min);
                out.release_date = clean_opt(m.release_date.as_deref());
                out.progress = title.progress;
                out.details_pending = lazy;
                Ok((out, lazy.then_some(JobKind::LoadMovieDetails(id))))
            }
            ItemKind::Series => {
                let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("series"))?;
                let v = &q.lib.vod.series[idx as usize];
                let title = q.title_out(false, idx);
                let mut out =
                    DetailsOut { item: item_ref(&r), kind: "series".into(), name: clean(&v.name), locked: title.locked, ..Default::default() };
                if out.locked {
                    return Ok((out, None));
                }
                let VodKind::Series(id) = v.kind else { return Err(Fail::not_found("series")) };
                let s = crate::store::vod::series(q.conn, id)?;
                let lazy = s.details_loaded_at.is_none() && q.lib.source(s.source_id).is_some_and(|m| m.kind == SourceKind::Xtream);
                out.year = s.year.or(v.year);
                out.rating = s.rating.or(v.rating).filter(|r| *r > 0.0);
                out.genre = clean_opt(s.genre.as_deref());
                out.plot = clean_opt(s.plot.as_deref());
                out.cast = clean_opt(s.cast.as_deref());
                out.director = clean_opt(s.director.as_deref());
                out.release_date = clean_opt(s.release_date.as_deref());
                out.seasons =
                    crate::store::vod::seasons(q.conn, id)?.into_iter().map(|s| SeasonOut { number: s.number, episodes: s.episode_count }).collect();
                out.next_episode = crate::store::copies::next_episode(q.conn, &q.series_copies(idx))?.map(|(_, e)| q.episode_out(&e));
                out.progress = title.progress;
                out.details_pending = lazy;
                Ok((out, lazy.then_some(JobKind::LoadSeriesDetails(id))))
            }
            ItemKind::Episode => {
                let (ep, idx) = q.episode(&r)?.ok_or_else(|| Fail::not_found("episode"))?;
                let series = &q.lib.vod.series[idx as usize];
                let locked = q.needs_pin(q.title_locked(false, idx));
                let name = clean(&format!("{} — S{:02}E{:02} · {}", series.name, ep.season, ep.episode, ep.title));
                let mut out = DetailsOut { item: item_ref(&r), kind: "episode".into(), name, locked, ..Default::default() };
                if locked {
                    return Ok((out, None));
                }
                out.plot = clean_opt(ep.plot.as_deref());
                out.runtime_min = ep.duration_s.filter(|d| *d > 0).map(|d| (f64::from(d) / 60.0).round() as u32);
                out.rating = ep.rating.filter(|r| *r > 0.0);
                out.release_date = clean_opt(ep.air_date.as_deref());
                out.progress = q.episode_out(&ep).progress;
                Ok((out, None))
            }
        },
    }
}

/// XMLTV star rating `7/10` / `3.5/5` → 0–10.
fn star_rating(s: &str) -> Option<f32> {
    let (a, b) = s.split_once('/')?;
    let (a, b) = (a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?);
    (b > 0.0).then(|| (a / b * 10.0).clamp(0.0, 10.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_text_params() {
        assert!(matches_words("BBC One HD", &words("bbc one")));
        assert!(!matches_words("BBC Two", &words("bbc one")));
        assert!(!matches_words("anything", &words("  ")));
        assert_eq!(text("q", "  hi "), Ok("hi"));
        assert_eq!(text("q", " ").unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(text("q", &"x".repeat(201)).unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(clamp(Some(500), 20, 50), 50);
        assert_eq!(clamp(Some(0), 20, 50), 1);
        assert_eq!(clamp(None, 20, 50), 20);
        assert_eq!(star_rating("7/10"), Some(7.0));
        assert_eq!(star_rating("3.5 / 5"), Some(7.0));
        assert_eq!(star_rating("x"), None);
        assert_eq!(clean("More at https://bbc.co.uk/x, and\thttp://a.b"), "More at [link] and\t[link]");
        assert_eq!(clean("plain"), "plain");
        let mut v: Vec<u32> = (0..50).collect();
        shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
    }
}
