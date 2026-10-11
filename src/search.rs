//! Search: global fuzzy search over the in-memory library (nucleo, threaded, incremental) +
//! EPG programme search (SQLite FTS5) + per-list type-to-filter (nucleo-matcher). Owner: WP-09.
//! DESIGN §14.
//!
//! * Global index: one `nucleo::Nucleo<SearchItem>` with a single match column
//!   ("name  group  source"); rebuilt (new injector) when the `Library` snapshot changes.
//!   Small libraries are injected inline; large ones (> 4 096 items) from a short-lived
//!   "search-inject" thread so the UI thread only pays for collecting the items (Arc clones);
//!   [`SearchIndex::rebuild_shared`] (the search card opening) collects them there too. The
//!   app frees the index when the card closes (~110 MiB for 460k items).
//!   `set_query` uses `reparse` with `append = true` when the new query extends the old one
//!   (except after a trailing `$` or `\`, whose meaning changes when text follows).
//!   `tick(10)` each frame while the overlay is open; results read from the snapshot (top 200).
//!   A blank query has no results. `notify` is debounced: it fires at most once between two
//!   `tick`s (nucleo would call it for every injected item).
//! * EPG search: debounced 150 ms; runs `store::epg::search` on the runtime with its own reader
//!   connection; results replace the "Guide" section when they arrive. Stale queries are
//!   dropped: the pending task is aborted, a superseded search that already reached the blocking
//!   pool is interrupted (or skipped before it starts), and only the latest query's results are
//!   published. Until the new query's results arrive, the previous ones stay visible; a blank
//!   query clears immediately.
//! * List filter: [`ListFilter`] keeps a `nucleo_matcher::Matcher` and returns matching row
//!   indices sorted by score (ties keep list order); < 5 ms for 20k rows, so it runs inline.
//!   IMPLEMENTED IN THE SKELETON (every list uses it). Channel haystacks are
//!   `"<number> <name>"`; an all-digit query ranks exact channel-number matches first.
//!
//! Locked items (parental: the item or one of its groups) are indexed but excluded from results
//! unless the session is unlocked; hidden items (hidden channels, items whose groups are all
//! hidden) never appear.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nucleo::Utf32String;
use nucleo::pattern::{CaseMatching, Normalization};

use crate::model::{ChannelId, GroupId, MovieId, ProgrammeHit, SeriesId, SourceId, Timestamp, VodKind};
use crate::store::library::Library;

/// Libraries up to this size are injected on the calling thread.
const SYNC_INJECT_MAX: usize = 4096;
/// Matcher threads (nucleo's rayon pool); matching 170k short strings needs few.
const MATCH_THREADS: usize = 4;
/// EPG search debounce and result cap.
const EPG_DEBOUNCE: Duration = Duration::from_millis(150);
const EPG_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchTarget {
    Channel(ChannelId),
    Movie(MovieId),
    Series(SeriesId),
}

impl SearchTarget {
    pub fn item_kind(self) -> crate::model::ItemKind {
        match self {
            SearchTarget::Channel(_) => crate::model::ItemKind::Channel,
            SearchTarget::Movie(_) => crate::model::ItemKind::Movie,
            SearchTarget::Series(_) => crate::model::ItemKind::Series,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchItem {
    pub target: SearchTarget,
    /// Parental lock (item or its group): excluded while the session is locked.
    pub locked: bool,
    pub source_id: SourceId,
    pub name: Arc<str>,
    pub group: Option<Arc<str>>,
    pub logo: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSection {
    #[default]
    All,
    Live,
    Movies,
    Series,
    Guide,
}

impl SearchSection {
    pub const ALL: [SearchSection; 5] = [SearchSection::All, SearchSection::Live, SearchSection::Movies, SearchSection::Series, SearchSection::Guide];

    fn accepts(self, t: SearchTarget) -> bool {
        matches!(
            (self, t),
            (SearchSection::All, _)
                | (SearchSection::Live, SearchTarget::Channel(_))
                | (SearchSection::Movies, SearchTarget::Movie(_))
                | (SearchSection::Series, SearchTarget::Series(_))
        )
    }
}

fn kind_rank(t: SearchTarget) -> usize {
    match t {
        SearchTarget::Channel(_) => 0,
        SearchTarget::Movie(_) => 1,
        SearchTarget::Series(_) => 2,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub item: SearchItem,
    pub score: u32,
    /// Char indices in `item.name` to highlight.
    pub indices: Vec<u32>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Global search state (owned by the search overlay; UI thread).
pub struct SearchIndex {
    nucleo: nucleo::Nucleo<SearchItem>,
    query: String,
    include_locked: bool,
    /// Set when `notify` was forwarded; cleared by `tick` (debounces nucleo's per-push notify).
    notified: Arc<AtomicBool>,
    /// Cancels the background injection of the previous `rebuild`.
    inject_cancel: Arc<AtomicBool>,
    /// Scores/highlights for the visible hits (`results` takes `&self`).
    matcher: Mutex<nucleo::Matcher>,
}

/// One item ready for injection, with the extra haystack parts.
struct Pending {
    item: SearchItem,
    source: Option<Arc<str>>,
}

impl Pending {
    fn haystack(&self) -> Utf32String {
        let mut s = String::with_capacity(self.item.name.len() + 32);
        s.push_str(&self.item.name);
        for part in [self.item.group.as_deref(), self.source.as_deref()].into_iter().flatten() {
            s.push_str("  ");
            s.push_str(part);
        }
        Utf32String::from(s)
    }
}

impl SearchIndex {
    /// `notify` is called by nucleo worker threads when results changed (→ request_repaint).
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let notified = Arc::new(AtomicBool::new(false));
        let flag = notified.clone();
        let debounced: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if !flag.swap(true, Ordering::AcqRel) {
                notify();
            }
        });
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).min(MATCH_THREADS);
        Self {
            nucleo: nucleo::Nucleo::new(nucleo::Config::DEFAULT, debounced, Some(threads), 1),
            query: String::new(),
            include_locked: false,
            notified,
            inject_cancel: Arc::new(AtomicBool::new(false)),
            matcher: Mutex::new(nucleo::Matcher::new(nucleo::Config::DEFAULT)),
        }
    }

    /// Replace all items from a library snapshot (hidden excluded; locked flagged). The items
    /// are collected here; a big library is injected on a background thread.
    pub fn rebuild(&mut self, lib: &Library, include_locked: bool) {
        let (injector, cancel) = self.restart(include_locked);
        let items = collect_items(lib);
        if items.len() <= SYNC_INJECT_MAX {
            inject(items, &injector, &cancel);
            return;
        }
        spawn_inject(move || inject(items, &injector, &cancel));
    }

    /// [`SearchIndex::rebuild`] for the search card opening: with a big library even collecting
    /// the items (`Arc` clones of the name, group and logo of every item) runs on the
    /// background thread, so the card opens at once and results stream in.
    pub fn rebuild_shared(&mut self, lib: &Arc<Library>, include_locked: bool) {
        if lib.live.channels.len() + lib.vod.movies.len() + lib.vod.series.len() <= SYNC_INJECT_MAX {
            return self.rebuild(lib, include_locked);
        }
        let (injector, cancel) = self.restart(include_locked);
        let lib = lib.clone();
        spawn_inject(move || {
            // opened and closed again before this thread ran: don't collect at all
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let items = collect_items(&lib);
            drop(lib);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            inject(items, &injector, &cancel)
        });
    }

    /// Stop the background collection and injection now (the index is about to be dropped on
    /// another thread, which may be busy for a while: quick reopen-and-close would otherwise
    /// run several full collections at once).
    pub fn cancel(&self) {
        self.inject_cancel.store(true, Ordering::Relaxed);
    }

    /// Cancel the previous injection and empty the index: the new injector and its cancel flag.
    fn restart(&mut self, include_locked: bool) -> (nucleo::Injector<SearchItem>, Arc<AtomicBool>) {
        self.include_locked = include_locked;
        self.inject_cancel.store(true, Ordering::Relaxed);
        self.inject_cancel = Arc::new(AtomicBool::new(false));
        self.nucleo.restart(true);
        (self.nucleo.injector(), self.inject_cancel.clone())
    }

    pub fn set_query(&mut self, q: &str) {
        if q == self.query {
            return;
        }
        // nucleo may only narrow the previous matches when the old text is a prefix, and not
        // when the old text ends in a postfix anchor (`foo$` → `foo$x`) or an escape (`foo\` →
        // `foo\ bar`): there the appended text changes what the old atom meant
        let append = !self.query.is_empty() && q.starts_with(self.query.as_str()) && !self.query.ends_with(['$', '\\']);
        self.nucleo.pattern.reparse(0, q, CaseMatching::Ignore, Normalization::Smart, append);
        self.query = q.to_owned();
    }

    /// Advance matching (≤ `timeout_ms`); returns true when results changed.
    pub fn tick(&mut self, timeout_ms: u64) -> bool {
        self.notified.store(false, Ordering::Release);
        self.nucleo.tick(timeout_ms).changed
    }

    /// Current top results for a section (All interleaves kinds by score).
    ///
    /// All: the best `limit` hits of every kind, grouped by kind (channels, movies, series);
    /// groups are ordered by their best hit, hits inside a group by score. Guide results come
    /// from [`EpgSearch`] (empty here).
    pub fn results(&self, section: SearchSection, limit: usize) -> Vec<SearchHit> {
        if section == SearchSection::Guide || limit == 0 || self.query.trim().is_empty() {
            return Vec::new();
        }
        let snap = self.nucleo.snapshot();
        let pattern = snap.pattern().column_pattern(0);
        let mut matcher = lock(&self.matcher);
        let mut buf = Vec::new();
        let mut hits = Vec::with_capacity(limit.min(256));
        for it in snap.matched_items(..) {
            let item = it.data;
            if (item.locked && !self.include_locked) || !section.accepts(item.target) {
                continue;
            }
            let hay = it.matcher_columns[0].slice(..);
            buf.clear();
            let Some(score) = pattern.indices(hay, &mut matcher, &mut buf) else { continue };
            hits.push(SearchHit { item: item.clone(), score, indices: name_indices(&item.name, &mut buf) });
            if hits.len() == limit {
                break;
            }
        }
        if section == SearchSection::All {
            // stable: keeps score order inside a kind
            let mut best = [usize::MAX; 3];
            for (i, h) in hits.iter().enumerate() {
                let k = kind_rank(h.item.target);
                best[k] = best[k].min(i);
            }
            hits.sort_by_key(|h| best[kind_rank(h.item.target)]);
        }
        hits
    }

    pub fn query(&self) -> &str {
        &self.query
    }
}

impl Drop for SearchIndex {
    fn drop(&mut self) {
        self.inject_cancel.store(true, Ordering::Relaxed);
    }
}

/// Push `items` into the index, checking `cancel` every 1 024 items (a newer rebuild started,
/// or the index was dropped).
fn inject(items: Vec<Pending>, injector: &nucleo::Injector<SearchItem>, cancel: &AtomicBool) {
    for (i, p) in items.into_iter().enumerate() {
        if i % 1024 == 0 && cancel.load(Ordering::Relaxed) {
            return;
        }
        let hay = p.haystack();
        injector.push(p.item, |_, cols| cols[0] = hay);
    }
}

fn spawn_inject(f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name("search-inject".into()).spawn(f) {
        tracing::warn!(error = %e, "search index: could not spawn the injection thread");
    }
}

/// Highlight positions that fall inside the name (the first part of the haystack), as char
/// indices, sorted and deduplicated (multi-word patterns report per-atom indices).
fn name_indices(name: &str, idx: &mut Vec<u32>) -> Vec<u32> {
    idx.sort_unstable();
    idx.dedup();
    if name.is_ascii() {
        let name_len = name.len() as u32;
        return idx.iter().copied().filter(|i| *i < name_len).collect();
    }
    // nucleo counts graphemes for non-ASCII text: map each grapheme to its first char
    let first_char = grapheme_starts(name);
    idx.iter().filter_map(|&g| first_char.get(g as usize).copied()).collect()
}

/// Char index of the first char of every grapheme of `name`, in nucleo's segmentation.
/// (Counted with `Utf32String` like the haystack: `Utf32Str::new` switches to byte units when
/// every grapheme starts with an ASCII char, e.g. "Cafe\u{301}".)
fn grapheme_starts(name: &str) -> Vec<u32> {
    let mut starts = Vec::new();
    for (ci, (b, c)) in name.char_indices().enumerate() {
        // the prefix ending with this char has (index of this char's grapheme + 1) graphemes
        let g = Utf32String::from(&name[..b + c.len_utf8()]).len();
        if g > starts.len() {
            starts.push(ci as u32);
        }
    }
    starts
}

/// Visible items of a snapshot, in library order (channels, movies, series).
fn collect_items(lib: &Library) -> Vec<Pending> {
    struct G {
        name: Arc<str>,
        hidden: bool,
        locked: bool,
    }
    let groups: HashMap<GroupId, G> = lib
        .live
        .groups
        .iter()
        .chain(&lib.vod.movie_groups)
        .chain(&lib.vod.series_groups)
        .map(|g| (g.id, G { name: Arc::from(g.rename.as_deref().unwrap_or(&g.name)), hidden: g.hidden, locked: g.locked }))
        .collect();
    let sources: HashMap<SourceId, Arc<str>> = lib.sources.iter().map(|s| (s.id, Arc::from(s.name.as_str()))).collect();
    // (hidden, locked-by-group, first group name)
    let group_facts = |ids: &[GroupId]| {
        let known: Vec<&G> = ids.iter().filter_map(|id| groups.get(id)).collect();
        let hidden = !known.is_empty() && known.iter().all(|g| g.hidden);
        let locked = known.iter().any(|g| g.locked);
        let name = known.iter().find(|g| !g.hidden).or(known.first()).map(|g| g.name.clone());
        (hidden, locked, name)
    };

    let mut out = Vec::with_capacity(lib.live.channels.len() + lib.vod.movies.len() + lib.vod.series.len());
    for c in &lib.live.channels {
        if lib.live.hidden.contains(&c.id) {
            continue;
        }
        let (hidden, group_locked, group) = group_facts(&c.group_ids);
        if hidden {
            continue;
        }
        out.push(Pending {
            item: SearchItem {
                target: SearchTarget::Channel(c.id),
                locked: c.locked || group_locked,
                source_id: c.source_id,
                name: c.name.clone(),
                group,
                logo: c.logo.clone(),
            },
            source: sources.get(&c.source_id).cloned(),
        });
    }
    for v in lib.vod.movies.iter().chain(&lib.vod.series) {
        let (hidden, group_locked, group) = group_facts(&v.group_ids);
        if hidden {
            continue;
        }
        let target = match v.kind {
            VodKind::Movie(id) => SearchTarget::Movie(id),
            VodKind::Series(id) => SearchTarget::Series(id),
        };
        out.push(Pending {
            item: SearchItem {
                target,
                locked: v.locked || group_locked,
                source_id: v.source_id,
                name: v.name.clone(),
                group,
                logo: v.poster.clone(),
            },
            source: sources.get(&v.source_id).cloned(),
        });
    }
    out
}

/// The FTS query function (a seam so the debounce/staleness logic is testable without data).
type EpgSearchFn = dyn Fn(&rusqlite::Connection, &str, Timestamp, usize) -> crate::error::Result<Vec<ProgrammeHit>> + Send + Sync;

/// Debounced EPG FTS search running off the UI thread.
pub struct EpgSearch {
    rt: tokio::runtime::Handle,
    store: crate::store::Store,
    pending: Option<tokio::task::JoinHandle<()>>,
    results: Arc<std::sync::Mutex<(u64, Vec<ProgrammeHit>)>>,
    seq: u64,
    /// The newest `seq`, readable by the search tasks (superseded ones skip or are discarded).
    latest: Arc<AtomicU64>,
    /// Lazily opened reader, reused across searches (one search runs at a time).
    reader: Arc<Mutex<Option<rusqlite::Connection>>>,
    /// Interrupts the query running on `reader` (set once the reader is open).
    interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
    search: Arc<EpgSearchFn>,
    debounce: Duration,
}

impl EpgSearch {
    pub fn new(rt: tokio::runtime::Handle, store: crate::store::Store) -> Self {
        Self::with_search(rt, store, Arc::new(crate::store::epg::search), EPG_DEBOUNCE)
    }

    fn with_search(rt: tokio::runtime::Handle, store: crate::store::Store, search: Arc<EpgSearchFn>, debounce: Duration) -> Self {
        Self {
            rt,
            store,
            pending: None,
            results: Arc::default(),
            seq: 0,
            latest: Arc::default(),
            reader: Arc::default(),
            interrupt: Arc::default(),
            search,
            debounce,
        }
    }

    /// Start (debounced 150 ms) a search; previous pending one is aborted.
    pub fn set_query(&mut self, q: &str, now: crate::model::Timestamp, repaint: Arc<dyn Fn() + Send + Sync>) {
        self.seq += 1;
        let seq = self.seq;
        self.latest.store(seq, Ordering::Release);
        if let Some(h) = self.pending.take() {
            h.abort();
        }
        // a search already on the blocking pool can't be aborted: stop its SQLite query (a no-op
        // when the reader is idle; this query's own search is still in its debounce)
        if let Some(h) = lock(&self.interrupt).as_ref() {
            h.interrupt();
        }
        let q = q.trim().to_owned();
        if q.is_empty() {
            *lock(&self.results) = (seq, Vec::new());
            return;
        }
        let (results, latest, reader, interrupt, store, search, debounce) = (
            self.results.clone(),
            self.latest.clone(),
            self.reader.clone(),
            self.interrupt.clone(),
            self.store.clone(),
            self.search.clone(),
            self.debounce,
        );
        self.pending = Some(self.rt.spawn(async move {
            tokio::time::sleep(debounce).await;
            let current = latest.clone();
            let run = tokio::task::spawn_blocking(move || {
                let mut conn = lock(&reader);
                // superseded while queued behind an older search: don't run the query at all
                if current.load(Ordering::Acquire) != seq {
                    return None;
                }
                if conn.is_none() {
                    match store.reader() {
                        Ok(c) => {
                            *lock(&interrupt) = Some(c.get_interrupt_handle());
                            *conn = Some(c);
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
                let c = conn.as_ref()?;
                Some(search(c, &q, now, EPG_LIMIT))
            });
            let outcome = run.await;
            if latest.load(Ordering::Acquire) != seq {
                return; // superseded: never published (an interrupted query's error is expected)
            }
            let hits = match outcome {
                Ok(Some(Ok(hits))) => hits,
                Ok(None) => return,
                Ok(Some(Err(e))) => {
                    tracing::warn!(error = %e, "EPG search failed");
                    Vec::new()
                }
                Err(e) => {
                    tracing::warn!(error = %e, "EPG search task failed");
                    Vec::new()
                }
            };
            let mut r = lock(&results);
            if seq > r.0 {
                *r = (seq, hits);
                drop(r);
                repaint();
            }
        }));
    }

    /// Results of the most recent search that finished. Earlier results stay until the newer
    /// query's results arrive; a blank query clears them at once.
    pub fn results(&self) -> Vec<ProgrammeHit> {
        lock(&self.results).1.clone()
    }
}

impl Drop for EpgSearch {
    fn drop(&mut self) {
        if let Some(h) = self.pending.take() {
            h.abort();
        }
    }
}

/// A typed character into a filter / search query: leading whitespace is ignored and any
/// whitespace (AltGr+Space types a no-break space) becomes a plain space.
pub fn push_typed(query: &mut String, c: char) {
    if query.is_empty() && c.is_whitespace() {
        return;
    }
    query.push(if c.is_whitespace() { ' ' } else { c });
}

/// Ctrl+Backspace in a filter / search query: drop trailing whitespace and the word before
/// it. Cuts after the whitespace *character* (a pasted no-break or ideographic space is
/// several bytes: a byte offset + 1 would split it and panic).
pub fn delete_last_word(query: &mut String) {
    let trimmed = query.trim_end();
    let cut = trimmed.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    query.truncate(cut);
}

/// Type-to-filter for one list (channels of a group, groups, episodes, settings rows).
pub struct ListFilter {
    matcher: nucleo_matcher::Matcher,
    query: String,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self { matcher: nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT), query: String::new() }
    }
}

impl ListFilter {
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn is_active(&self) -> bool {
        !self.query.is_empty()
    }
    pub fn push(&mut self, c: char) {
        push_typed(&mut self.query, c);
    }
    pub fn backspace(&mut self) {
        self.query.pop();
    }
    /// Delete the last word (Ctrl+Backspace).
    pub fn delete_word(&mut self) {
        delete_last_word(&mut self.query);
    }
    pub fn clear(&mut self) {
        self.query.clear();
    }

    /// Indices of `items` matching the query, best first (all, in order, when inactive).
    /// An all-digit query puts items whose first token equals it (channel number) first.
    pub fn apply<'a>(&mut self, items: impl Iterator<Item = &'a str>) -> Vec<u32> {
        use nucleo_matcher::Utf32Str;
        use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
        let q = self.query.trim();
        if q.is_empty() {
            return items.enumerate().map(|(i, _)| i as u32).collect();
        }
        let digits = q.chars().all(|c| c.is_ascii_digit());
        let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut hits: Vec<(u32, u32, u32)> = Vec::new(); // (exact-number bonus, score, index)
        for (i, s) in items.enumerate() {
            if let Some(score) = pattern.score(Utf32Str::new(s, &mut buf), &mut self.matcher) {
                let exact = digits && s.split_whitespace().next() == Some(q);
                hits.push((exact as u32, score, i as u32));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        hits.into_iter().map(|h| h.2).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use crate::model::{ChannelSummary, EpgChannelId, ItemKey, ProgrammeFlags, ProgrammeId, ProgrammeSlim};
    use crate::store::library::LiveIndex;

    fn channel(id: i64, name: &str) -> ChannelSummary {
        ChannelSummary {
            id: ChannelId(id),
            source_id: SourceId(1),
            key: ItemKey(format!("k{id}")),
            name: Arc::from(name),
            provider_name: None,
            number: Some(id as u32),
            logo: None,
            group_ids: vec![],
            epg_channel_id: None,
            tvg_shift_min: 0,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: id as i32,
            user_order: None,
            added_at: None,
            quality: None,
            family: 0,
        }
    }

    fn library(n: usize) -> Library {
        let chans = (0..n as i64).map(|i| channel(i, &format!("Channel {i:06}"))).collect();
        Library { live: LiveIndex::from_parts(chans, vec![], HashMap::new(), HashSet::new()).into(), ..Library::default() }
    }

    /// Tick until the matcher is idle and every injected item was matched.
    fn settle(idx: &mut SearchIndex, expected_items: u32) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = idx.nucleo.tick(10);
            idx.notified.store(false, Ordering::Release);
            let snap = idx.nucleo.snapshot();
            if !status.running && snap.item_count() == expected_items {
                return;
            }
            assert!(Instant::now() < deadline, "matcher did not settle ({} items)", snap.item_count());
        }
    }

    #[test]
    fn notify_is_debounced_until_the_next_tick() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut idx = SearchIndex::new(Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        idx.rebuild(&library(500), false);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "500 pushes, one notification");
        idx.set_query("channel");
        settle(&mut idx, 500);
        assert!(calls.load(Ordering::SeqCst) <= 5);
        assert_eq!(idx.results(SearchSection::Live, 1000).len(), 500);
    }

    #[test]
    fn large_libraries_are_injected_in_the_background() {
        let n = SYNC_INJECT_MAX * 3;
        let mut idx = SearchIndex::new(Arc::new(|| {}));
        let t = Instant::now();
        idx.rebuild(&library(n), false);
        let rebuild = t.elapsed();
        idx.set_query(&format!("channel {:06}", n - 1));
        settle(&mut idx, n as u32);
        let r = idx.results(SearchSection::All, 5);
        assert_eq!(r[0].item.target, SearchTarget::Channel(ChannelId(n as i64 - 1)), "last item injected: {r:?}");
        assert!(rebuild < Duration::from_secs(5));

        // a rebuild while injecting cancels the old thread; only the new items remain
        idx.rebuild(&library(n), false);
        idx.rebuild(&library(10), false);
        idx.set_query("channel");
        std::thread::sleep(Duration::from_millis(50));
        settle(&mut idx, 10);
        assert_eq!(idx.results(SearchSection::Live, 100).len(), 10);
    }

    /// The search card opening (`rebuild_shared`): a big library is collected and injected on
    /// the background thread (the snapshot is shared, not walked in the frame); a newer
    /// rebuild cancels it, and the index can be dropped while it runs.
    #[test]
    fn opening_search_collects_a_big_library_in_the_background() {
        let n = SYNC_INJECT_MAX * 3;
        let lib = Arc::new(library(n));
        let mut idx = SearchIndex::new(Arc::new(|| {}));
        idx.rebuild_shared(&lib, false);
        idx.set_query(&format!("channel {:06}", n - 1));
        settle(&mut idx, n as u32);
        assert_eq!(idx.results(SearchSection::All, 5)[0].item.target, SearchTarget::Channel(ChannelId(n as i64 - 1)));
        idx.rebuild_shared(&lib, false);
        idx.rebuild_shared(&Arc::new(library(10)), false);
        idx.set_query("channel");
        std::thread::sleep(Duration::from_millis(50));
        settle(&mut idx, 10);
        assert_eq!(idx.results(SearchSection::Live, 100).len(), 10);
        idx.rebuild_shared(&lib, false);
        // the card closed: cancelled before the index is handed to the maintenance thread
        idx.cancel();
        let t = Instant::now();
        while Arc::strong_count(&lib) > 1 {
            assert!(t.elapsed() < Duration::from_secs(10), "a cancelled collection lets go of the snapshot");
            std::thread::sleep(Duration::from_millis(5));
        }
        idx.rebuild_shared(&lib, false);
        drop(idx);
        let t = Instant::now();
        while Arc::strong_count(&lib) > 1 {
            assert!(t.elapsed() < Duration::from_secs(10), "the background thread lets go of the snapshot");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn highlight_indices_stay_inside_the_name() {
        let mut v = vec![9, 1, 1, 0, 40];
        assert_eq!(name_indices("BBC One", &mut v), vec![0, 1]);
        let mut v = vec![0, 3, 4];
        assert_eq!(name_indices("Café", &mut v), vec![0, 3], "precomposed é: one char, one grapheme");
        // decomposed "e\u{301}" is one grapheme (nucleo index 3) but two chars
        let name = "Cafe\u{301} Noir";
        let chars: Vec<char> = name.chars().collect();
        let mut v = vec![6, 5, 3];
        assert_eq!(name_indices(name, &mut v), vec![3, 6, 7], "grapheme indices mapped to chars");
        assert_eq!((chars[3], chars[6], chars[7]), ('e', 'N', 'o'));
        let mut v = vec![8, 9, 10];
        assert_eq!(name_indices(name, &mut v), vec![9], "past the name is dropped");
    }

    #[test]
    fn highlights_match_the_haystack_for_decomposed_names() {
        let lib = Library {
            live: LiveIndex::from_parts(vec![channel(1, "Cafe\u{301} Noir")], vec![], HashMap::new(), HashSet::new()).into(),
            ..Library::default()
        };
        let mut idx = SearchIndex::new(Arc::new(|| {}));
        idx.rebuild(&lib, false);
        idx.set_query("noir");
        settle(&mut idx, 1);
        let hits = idx.results(SearchSection::Live, 10);
        let chars: Vec<char> = hits[0].item.name.chars().collect();
        let lit: String = hits[0].indices.iter().map(|&i| chars[i as usize]).collect();
        assert_eq!(lit, "Noir");
    }

    #[test]
    fn append_is_not_used_after_anchors_or_escapes() {
        let names = ["foo", "foo$x", "foo\\ bar", "foo bar", "xfoo"];
        let lib = Library {
            live: LiveIndex::from_parts(
                names.iter().enumerate().map(|(i, n)| channel(i as i64, n)).collect(),
                vec![],
                HashMap::new(),
                HashSet::new(),
            )
            .into(),
            ..Library::default()
        };
        let ids = |idx: &SearchIndex| {
            let mut v: Vec<SearchTarget> = idx.results(SearchSection::Live, 100).iter().map(|h| h.item.target).collect();
            v.sort_by_key(|t| match t {
                SearchTarget::Channel(c) => c.0,
                _ => i64::MAX,
            });
            v
        };
        for (old, new) in [("foo$", "foo$x"), ("foo\\", "foo\\ bar"), ("foo", "foo bar")] {
            let mut idx = SearchIndex::new(Arc::new(|| {}));
            idx.rebuild(&lib, false);
            idx.set_query(old);
            settle(&mut idx, names.len() as u32);
            idx.set_query(new);
            settle(&mut idx, names.len() as u32);
            let mut fresh = SearchIndex::new(Arc::new(|| {}));
            fresh.rebuild(&lib, false);
            fresh.set_query(new);
            settle(&mut fresh, names.len() as u32);
            assert!(!ids(&fresh).is_empty(), "{new:?} matches something");
            assert_eq!(ids(&idx), ids(&fresh), "{old:?} → {new:?}");
        }
    }

    fn hit(id: i64, title: &str) -> ProgrammeHit {
        ProgrammeHit {
            programme: ProgrammeSlim {
                id: ProgrammeId(id),
                start: 0,
                stop: 60,
                title: Arc::from(title),
                sub_title: None,
                flags: ProgrammeFlags::default(),
            },
            epg_channel_id: EpgChannelId(1),
            channel_ids: vec![],
        }
    }

    struct EpgRig {
        rt: tokio::runtime::Runtime,
        calls: Arc<Mutex<Vec<String>>>,
        search: EpgSearch,
        repaints: Arc<AtomicUsize>,
    }

    /// Fake FTS: records queries; a query starting with "slow" takes 400 ms.
    fn epg_rig() -> EpgRig {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let store = crate::store::Store::open_temp("tuner-epgsearch-test", None).unwrap();
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = calls.clone();
        let f: Arc<EpgSearchFn> = Arc::new(move |_c, q, _now, limit| {
            assert_eq!(limit, EPG_LIMIT);
            lock(&log).push(q.to_owned());
            if q.starts_with("slow") {
                std::thread::sleep(Duration::from_millis(400));
            }
            Ok(vec![hit(q.len() as i64, q)])
        });
        let search = EpgSearch::with_search(rt.handle().clone(), store, f, Duration::from_millis(60));
        EpgRig { rt, calls, search, repaints: Arc::default() }
    }

    impl EpgRig {
        fn set(&mut self, q: &str) {
            let r = self.repaints.clone();
            self.search.set_query(
                q,
                0,
                Arc::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }),
            );
        }
        fn wait_for(&self, title: &str) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.search.results().first().is_some_and(|h| &*h.programme.title == title) {
                assert!(Instant::now() < deadline, "no results for {title}: {:?}", self.search.results());
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn epg_search_is_debounced() {
        let mut r = epg_rig();
        for q in ["b", "bb", "bbc", "bbc p", "bbc proms"] {
            r.set(q);
        }
        r.wait_for("bbc proms");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(*lock(&r.calls), vec!["bbc proms".to_owned()], "only the last keystroke searched");
        assert_eq!(r.repaints.load(Ordering::SeqCst), 1);
        // blank clears at once, without a search
        r.set("   ");
        assert!(r.search.results().is_empty());
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(lock(&r.calls).len(), 1);
        drop(r.rt);
    }

    #[test]
    fn epg_stale_results_are_dropped() {
        let mut r = epg_rig();
        r.set("slow query");
        // let the debounce pass so the slow search is running on the blocking pool
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&r.calls).is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        r.set("fast");
        r.wait_for("fast");
        // the slow search finishes later and must not replace the newer results
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(&*r.search.results()[0].programme.title, "fast");
        assert_eq!(*lock(&r.calls), vec!["slow query".to_owned(), "fast".to_owned()]);
        // previous results stay visible until the next query's results arrive
        r.set("faster");
        assert_eq!(&*r.search.results()[0].programme.title, "fast");
        r.wait_for("faster");
    }

    #[test]
    fn epg_superseded_searches_queued_behind_a_slow_one_never_run() {
        let mut r = epg_rig();
        r.set("slow query");
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&r.calls).is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        // "a" passes its debounce and waits for the reader behind the slow search …
        r.set("a");
        std::thread::sleep(Duration::from_millis(150));
        // … and is superseded before it gets it
        r.set("b");
        r.wait_for("b");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*lock(&r.calls), vec!["slow query".to_owned(), "b".to_owned()]);
        assert_eq!(r.repaints.load(Ordering::SeqCst), 1, "only the latest query published");
    }

    #[test]
    fn list_filter_ranks_numbers_and_keeps_order() {
        let items = ["101 BBC One", "1 BBC Two", "102 ITV", "10 Channel 4"];
        let mut f = ListFilter::default();
        assert_eq!(f.apply(items.iter().copied()), vec![0, 1, 2, 3]);
        for c in "10".chars() {
            f.push(c);
        }
        let r = f.apply(items.iter().copied());
        assert_eq!(r[0], 3, "exact number first: {r:?}");
        f.clear();
        for c in "bbc".chars() {
            f.push(c);
        }
        assert_eq!(f.apply(items.iter().copied()), vec![0, 1]);
    }

    /// Ctrl+Backspace after a multi-byte whitespace character used to cut inside it and panic
    /// (a no-break space typed with AltGr+Space, an ideographic space pasted in).
    #[test]
    fn delete_word_after_multibyte_whitespace() {
        let mut f = ListFilter::default();
        for c in "bbc\u{a0}one\u{a0}".chars() {
            f.push(c);
        }
        assert_eq!(f.query(), "bbc one ", "typed whitespace becomes a plain space");
        f.delete_word();
        assert_eq!(f.query(), "bbc ");
        for (raw, want) in [("bbc\u{a0}one", "bbc\u{a0}"), ("ニュース\u{3000}東京\u{3000}", "ニュース\u{3000}"), ("one", ""), ("\u{2003}", "")]
        {
            let mut q = raw.to_owned();
            delete_last_word(&mut q);
            assert_eq!(q, want, "{raw:?}");
        }
    }
}
