//! Search: global fuzzy search over the in-memory library (nucleo, threaded, incremental) +
//! EPG programme search (SQLite FTS5) + per-list type-to-filter (nucleo-matcher). Owner: WP-09.
//! DESIGN §14.
//!
//! * Global index: one `nucleo::Nucleo<SearchItem>` with a single match column
//!   ("name  group  source"); rebuilt (new injector) when the `Library` snapshot changes.
//!   `set_query` uses `reparse` with `append = true` when the new query extends the old one.
//!   `tick(10)` each frame while the overlay is open; results read from the snapshot (top 200).
//! * EPG search: debounced 150 ms; runs `store::epg::search` on the runtime with its own reader
//!   connection; results replace the "Guide" section when they arrive (stale queries dropped).
//! * List filter: [`ListFilter`] keeps a `nucleo_matcher::Matcher` and returns matching row
//!   indices sorted by score (ties keep list order); < 5 ms for 20k rows, so it runs inline.
//!   IMPLEMENTED IN THE SKELETON (every list uses it). Channel haystacks are
//!   `"<number> <name>"`; an all-digit query ranks exact channel-number matches first.
//!
//! Locked items (parental) are excluded unless the session is unlocked; hidden items never
//! appear.

use std::sync::Arc;

use crate::model::{ChannelId, MovieId, ProgrammeHit, SeriesId, SourceId};
use crate::store::library::Library;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchTarget {
    Channel(ChannelId),
    Movie(MovieId),
    Series(SeriesId),
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
    pub const ALL: [SearchSection; 5] =
        [SearchSection::All, SearchSection::Live, SearchSection::Movies, SearchSection::Series, SearchSection::Guide];
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub item: SearchItem,
    pub score: u32,
    /// Char indices in `item.name` to highlight.
    pub indices: Vec<u32>,
}

/// Global search state (owned by the search overlay; UI thread).
pub struct SearchIndex {
    nucleo: nucleo::Nucleo<SearchItem>,
    query: String,
    include_locked: bool,
}

impl SearchIndex {
    /// `notify` is called by nucleo worker threads when results changed (→ request_repaint).
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self { todo!("WP-09") }

    /// Replace all items from a library snapshot (hidden excluded; locked flagged).
    pub fn rebuild(&mut self, lib: &Library, include_locked: bool) { todo!("WP-09") }

    pub fn set_query(&mut self, q: &str) { todo!("WP-09") }

    /// Advance matching (≤ `timeout_ms`); returns true when results changed.
    pub fn tick(&mut self, timeout_ms: u64) -> bool { todo!("WP-09") }

    /// Current top results for a section (All interleaves kinds by score).
    pub fn results(&self, section: SearchSection, limit: usize) -> Vec<SearchHit> { todo!("WP-09") }

    pub fn query(&self) -> &str { &self.query }
}

/// Debounced EPG FTS search running off the UI thread.
pub struct EpgSearch {
    rt: tokio::runtime::Handle,
    store: crate::store::Store,
    pending: Option<tokio::task::JoinHandle<()>>,
    results: Arc<std::sync::Mutex<(u64, Vec<ProgrammeHit>)>>,
    seq: u64,
}

impl EpgSearch {
    pub fn new(rt: tokio::runtime::Handle, store: crate::store::Store) -> Self {
        Self { rt, store, pending: None, results: Arc::default(), seq: 0 }
    }
    /// Start (debounced 150 ms) a search; previous pending one is aborted.
    pub fn set_query(&mut self, q: &str, now: crate::model::Timestamp, repaint: Arc<dyn Fn() + Send + Sync>) { todo!("WP-09") }
    /// Latest finished results for the latest query.
    pub fn results(&self) -> Vec<ProgrammeHit> { todo!("WP-09") }
}

/// Type-to-filter for one list (channels of a group, groups, episodes, settings rows).
pub struct ListFilter {
    matcher: nucleo_matcher::Matcher,
    query: String,
}

impl Default for ListFilter {
    fn default() -> Self { Self { matcher: nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT), query: String::new() } }
}

impl ListFilter {
    pub fn query(&self) -> &str { &self.query }
    pub fn is_active(&self) -> bool { !self.query.is_empty() }
    pub fn push(&mut self, c: char) {
        if !(self.query.is_empty() && c.is_whitespace()) {
            self.query.push(c);
        }
    }
    pub fn backspace(&mut self) { self.query.pop(); }
    /// Delete the last word (Ctrl+Backspace).
    pub fn delete_word(&mut self) {
        let trimmed = self.query.trim_end();
        let cut = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        self.query.truncate(cut);
    }
    pub fn clear(&mut self) { self.query.clear(); }

    /// Indices of `items` matching the query, best first (all, in order, when inactive).
    /// An all-digit query puts items whose first token equals it (channel number) first.
    pub fn apply<'a>(&mut self, items: impl Iterator<Item = &'a str>) -> Vec<u32> {
        use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
        use nucleo_matcher::Utf32Str;
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
}
