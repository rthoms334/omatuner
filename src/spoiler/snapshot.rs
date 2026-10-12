//! The guard snapshot: matcher, topics with their embargo windows, series boundaries and the
//! zap shield's spans (DESIGN §34 §4). Built on the tokio blocking pool (debounced 1 s) on
//! `DataScope::{Epg, Live, Vod, Progress, Spoiler, Follow}`, a config change and an hourly tick
//! (`tracker::start`'s builder). OWNER: S1.
//!
//! Time only enters judging as a scan over a topic's few windows, so a snapshot stays right as
//! time passes; the hourly rebuild only extends the 36 h lookahead.
//!
//! Guardrails: programmes count only through channels that are neither hidden nor explicit
//! (the guard never builds events from, lists or counts explicit content, whatever the session's
//! lock state); explicit series never get a series mark. A broken rule is skipped (store layer).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Config;
use crate::error::Result;
use crate::guard::ReasonKind;
use crate::model::{ChannelId, EpgChannelId, SEEN_SPAN_S, SpoilerMode, SpoilerRuleId, SpoilerRuleKind, Timestamp};
use crate::store::library::Library;
use crate::store::spoiler as db;
use crate::textmatch::terms::TermSet;

/// How far ahead events and shield spans are looked up (the hourly rebuild extends it).
pub const LOOKAHEAD_S: i64 = 36 * 3600;
/// Automatic series guarding needs activity within this long.
pub const SERIES_ACTIVE_S: i64 = 180 * 86_400;
/// Event lookback cap (a window longer than this after its event is cut here).
const MAX_LOOKBACK_S: i64 = 14 * 86_400;

/// An embargo window of one event (scanned by time; no rebuild as time passes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The event's first airing starts (EPG time).
    pub open: Timestamp,
    /// The first airing's end plus the rule's window (`Timestamp::MAX`: until seen).
    pub close: Timestamp,
    pub event_key: String,
    /// The first airing's title (Settings "guarding now", tooltips "until you watch …").
    pub title: String,
}

/// Where a topic comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicSource {
    Rule(SpoilerRuleId),
    /// A spoiler-safe Following rule (B4).
    Follow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicMode {
    /// Each matching event opens a window at its first airing.
    UntilSeen,
    /// Every mention, always.
    Always,
    /// "I'll watch it later": every mention (the live event too) until then.
    Blackout { until: Timestamp },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Topic {
    pub label: String,
    pub source: TopicSource,
    pub mode: TopicMode,
    /// Only programmes in a sport category (or with no category at all — grid cells and many
    /// guides carry none).
    pub sport_only: bool,
    /// Other category scope (folded words; empty = any).
    pub categories: Vec<String>,
    /// While embargoed, also hide sub-titles and descriptions of every sport programme.
    pub sport_descr: bool,
    /// The zap shield applies.
    pub shield: bool,
    /// Sorted by `open` (set through [`Topic::set_windows`]).
    pub windows: Vec<Window>,
    /// The windows merged into disjoint `[open, close)` spans, sorted: `active` is one binary
    /// search however many events a broad topic has.
    spans: Vec<(Timestamp, Timestamp)>,
}

impl Topic {
    /// Active at `now`: `Some(until)` (when it lifts by itself; `None` = when watched / never).
    pub fn active(&self, now: Timestamp) -> Option<Option<Timestamp>> {
        match self.mode {
            TopicMode::Always => Some(None),
            TopicMode::Blackout { until } => (now < until).then_some(Some(until)),
            TopicMode::UntilSeen => {
                let i = self.spans.partition_point(|s| s.0 <= now).checked_sub(1)?;
                let close = self.spans[i].1;
                (close > now).then_some((close != Timestamp::MAX).then_some(close))
            }
        }
    }

    /// Set the windows (sorted here) and their merged spans.
    pub fn set_windows(&mut self, mut windows: Vec<Window>) {
        windows.sort_by(|a, b| a.open.cmp(&b.open).then_with(|| a.event_key.cmp(&b.event_key)));
        let mut spans: Vec<(Timestamp, Timestamp)> = Vec::new();
        for w in &windows {
            match spans.last_mut() {
                Some(last) if w.open <= last.1 => last.1 = last.1.max(w.close),
                _ => spans.push((w.open, w.close)),
            }
        }
        self.windows = windows;
        self.spans = spans;
    }

    /// The open window's event (the earliest still open).
    pub fn open_window(&self, now: Timestamp) -> Option<&Window> {
        let open = self.windows.partition_point(|w| w.open <= now);
        self.windows[..open].iter().find(|w| w.close > now)
    }

    /// The categories of a programme are in this topic's scope.
    pub fn in_scope(&self, categories: &[String]) -> bool {
        if categories.is_empty() {
            return true;
        }
        if self.sport_only && !super::is_sport(categories) {
            return false;
        }
        self.categories.is_empty()
            || categories.iter().any(|c| {
                let c = c.to_lowercase();
                self.categories.iter().any(|w| c.contains(w.as_str()))
            })
    }

    pub fn reason_kind(&self) -> ReasonKind {
        match self.mode {
            TopicMode::Blackout { .. } => ReasonKind::Blackout,
            _ => ReasonKind::Embargo,
        }
    }
}

/// A guarded series: episodes after `next_up` are hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMark {
    pub label: String,
    /// The episode you're about to play (shown); later ones are guarded.
    pub next_up: (i32, i32),
    /// Part-way through (the automatic rule) rather than the user's own row.
    pub automatic: bool,
    pub rule: Option<SpoilerRuleId>,
}

/// A programme on a live channel that mentions a shielding topic (displayed time: the
/// channel's shift applied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShieldSpan {
    pub start: Timestamp,
    pub stop: Timestamp,
    /// Index into `topics`.
    pub topic: u32,
    /// The airing IS an "until seen" event (watching the match itself isn't a spoiler).
    pub is_event: bool,
}

/// The settings a snapshot was built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings {
    pub hide_scores_everywhere: bool,
    pub shield_zap: bool,
    pub series_auto: bool,
}

/// The built snapshot (well under 1 MB at the user's scale).
#[derive(Debug, Default)]
pub struct GuardSnapshot {
    pub generation: u64,
    /// The guard is on (`[spoiler] enabled`).
    pub on: bool,
    /// Built from the data; `false` is the fail-closed placeholder before the first build.
    pub ready: bool,
    pub built_at: Timestamp,
    pub settings: Settings,
    pub topics: Vec<Topic>,
    /// Folded terms → topic index.
    pub(crate) matcher: TermSet,
    /// By series key (`spoiler::series_key`; every alias of a series points at its mark).
    pub series: HashMap<String, SeriesMark>,
    pub(crate) shields: HashMap<ChannelId, Vec<ShieldSpan>>,
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

impl GuardSnapshot {
    /// Before the first build: on and not ready, so judging fails closed.
    pub fn fail_closed() -> Self {
        Self { generation: next_generation(), on: true, ready: false, ..Default::default() }
    }

    /// The guard turned off.
    pub fn off(now: Timestamp) -> Self {
        Self { generation: next_generation(), on: false, ready: true, built_at: now, ..Default::default() }
    }

    /// Topics whose terms occur (whole tokens) in folded text.
    pub fn topics_in(&self, folded: &str) -> Vec<u32> {
        if self.matcher.is_empty() || folded.is_empty() { Vec::new() } else { self.matcher.owners_in(folded) }
    }

    /// A snapshot from parts (tests, benches): ready, on, the matcher built from the topics'
    /// own terms.
    pub fn from_parts(
        settings: Settings,
        topics: Vec<(Topic, Vec<String>)>,
        series: HashMap<String, SeriesMark>,
        shields: HashMap<ChannelId, Vec<ShieldSpan>>,
    ) -> Self {
        let terms: Vec<(String, u32)> = topics.iter().enumerate().flat_map(|(i, (_, ts))| ts.iter().map(move |t| (t.clone(), i as u32))).collect();
        Self {
            generation: next_generation(),
            on: true,
            ready: true,
            built_at: 0,
            settings,
            topics: topics
                .into_iter()
                .map(|(mut t, _)| {
                    let w = std::mem::take(&mut t.windows);
                    t.set_windows(w);
                    t
                })
                .collect(),
            matcher: TermSet::new(&terms),
            series,
            shields,
        }
    }

    pub(crate) fn shield_spans(&self, ch: ChannelId) -> &[ShieldSpan] {
        self.shields.get(&ch).map_or(&[], Vec::as_slice)
    }
}

/// Fold for matching.
pub(crate) fn folded(s: &str) -> String {
    let mut out = String::new();
    crate::textmatch::fold::fold(s, &mut out);
    out
}

/// Build a snapshot from the rules, Following's spoiler-safe rules (virtual topics, B4), the
/// guide and merged series progress. `conn` is a reader; runs off the UI thread.
pub fn build(conn: &rusqlite::Connection, cfg: &Config, lib: &Library, now: Timestamp) -> Result<GuardSnapshot> {
    if !cfg.spoiler.enabled {
        return Ok(GuardSnapshot::off(now));
    }
    let settings = Settings {
        hide_scores_everywhere: cfg.spoiler.hide_scores_everywhere,
        shield_zap: cfg.spoiler.shield_zap,
        series_auto: cfg.spoiler.series_auto,
    };
    let rules = db::list_rules(conn)?;
    let mut topics: Vec<(Topic, Vec<String>, u32)> = Vec::new(); // (topic, terms, window_h)
    for r in rules.iter().filter(|r| r.enabled && r.kind != SpoilerRuleKind::Series && !r.terms.is_empty()) {
        let mode = match (r.kind, r.mode) {
            (SpoilerRuleKind::Blackout, _) => match r.until_at {
                Some(until) if until > now => TopicMode::Blackout { until },
                _ => continue, // over
            },
            (_, SpoilerMode::Always) => TopicMode::Always,
            _ => TopicMode::UntilSeen,
        };
        let cats: Vec<String> = r.categories.iter().map(|c| c.trim().to_lowercase()).filter(|c| !c.is_empty()).collect();
        let sport_only = cats.iter().any(|c| c.starts_with("sport"));
        let categories = cats.into_iter().filter(|c| !c.starts_with("sport")).collect();
        let topic = Topic {
            label: r.label.clone(),
            source: TopicSource::Rule(r.id),
            mode,
            sport_only,
            categories,
            sport_descr: r.sport_descr,
            shield: r.shield,
            windows: Vec::new(),
            spans: Vec::new(),
        };
        topics.push((topic, r.terms.clone(), r.window_h));
    }
    for f in db::follow_topics(conn)? {
        let topic = Topic {
            label: f.label,
            source: TopicSource::Follow,
            mode: TopicMode::UntilSeen,
            sport_only: f.sport_only,
            categories: Vec::new(),
            sport_descr: f.sport_only,
            shield: true,
            windows: Vec::new(),
            spans: Vec::new(),
        };
        topics.push((topic, f.terms, cfg.spoiler.default_window_h));
    }
    let terms: Vec<(String, u32)> = topics.iter().enumerate().flat_map(|(i, (_, ts, _))| ts.iter().map(move |t| (t.clone(), i as u32))).collect();
    let matcher = TermSet::new(&terms);

    let mut shields: HashMap<ChannelId, Vec<ShieldSpan>> = HashMap::new();
    if !topics.is_empty() {
        // guide channel → the playlist channels showing it (hidden and explicit ones left out)
        let mut chans: HashMap<EpgChannelId, Vec<(ChannelId, i32)>> = HashMap::new();
        for c in &lib.live.channels {
            if let Some(e) = c.epg_channel_id
                && !lib.live.hidden.contains(&c.id)
                && !lib.explicit.channels.contains(&c.id)
            {
                chans.entry(e).or_default().push((c.id, c.tvg_shift_min));
            }
        }
        let seen: HashMap<String, Vec<Timestamp>> =
            db::seen_since(conn, now - MAX_LOOKBACK_S - 2 * SEEN_SPAN_S)?.into_iter().fold(HashMap::new(), |mut m, s| {
                m.entry(s.event_key).or_default().push(s.first_start);
                m
            });
        let max_h = topics.iter().map(|t| if t.2 == 0 { i64::MAX } else { i64::from(t.2) }).max().unwrap_or(48);
        let lookback = max_h.saturating_mul(3600).saturating_add(SEEN_SPAN_S).min(MAX_LOOKBACK_S);
        let shield_from = now - 6 * 3600;
        for (idx, (topic, terms, window_h)) in topics.iter_mut().enumerate() {
            let idx = idx as u32;
            let until_seen = topic.mode == TopicMode::UntilSeen;
            let from = if until_seen { now - lookback } else { shield_from };
            let wants_shield = settings.shield_zap && topic.shield;
            if !until_seen && !wants_shield {
                continue;
            }
            // one FTS query per topic: titles make events, titles and sub-titles shield spans
            let cands = db::event_candidates(conn, terms, !wants_shield, from, now + LOOKAHEAD_S)?;
            let mut events: HashMap<String, Window> = HashMap::new();
            let mut event_ids = HashSet::new();
            for p in &cands {
                let Some(on) = chans.get(&p.epg_channel_id) else { continue };
                if !topic.in_scope(&p.categories) {
                    continue;
                }
                let in_title = matcher.owners_in(&folded(&p.title)).contains(&idx);
                if until_seen && in_title {
                    let se = p.season.zip(p.episode);
                    let key = super::event_key_of(&p.title, p.sub_title.as_deref(), p.description.as_deref(), se, p.start);
                    // candidates come ordered by start: the first one seen is the first airing
                    events.entry(key.clone()).or_insert_with(|| Window { open: p.start, close: p.stop, event_key: key, title: p.title.clone() });
                    event_ids.insert(p.id);
                }
                if wants_shield && p.stop > now {
                    let mentions = in_title || p.sub_title.as_deref().is_some_and(|s| matcher.owners_in(&folded(s)).contains(&idx));
                    if mentions {
                        let is_event = until_seen && event_ids.contains(&p.id);
                        for &(ch, shift) in on {
                            let d = i64::from(shift) * 60;
                            shields.entry(ch).or_default().push(ShieldSpan { start: p.start + d, stop: p.stop + d, topic: idx, is_event });
                        }
                    }
                }
            }
            if until_seen {
                let span = if *window_h == 0 { None } else { Some(i64::from(*window_h) * 3600) };
                let windows: Vec<Window> = events
                    .into_values()
                    .filter(|w| !seen.get(&w.event_key).is_some_and(|starts| starts.iter().any(|s| (s - w.open).abs() <= SEEN_SPAN_S)))
                    .map(|w| Window { close: span.map_or(Timestamp::MAX, |s| w.close.saturating_add(s)), ..w })
                    .filter(|w| w.close > now)
                    .collect();
                topic.set_windows(windows);
            }
        }
    }
    let series = if settings.series_auto || rules.iter().any(|r| r.kind == SpoilerRuleKind::Series) {
        series_marks(conn, &rules, settings.series_auto, lib, now)?
    } else {
        HashMap::new()
    };
    Ok(GuardSnapshot {
        generation: next_generation(),
        on: true,
        ready: true,
        built_at: now,
        settings,
        topics: topics.into_iter().map(|t| t.0).collect(),
        matcher,
        series,
        shields,
    })
}

/// Series marks: part-way series (at least one episode done, activity within 180 days, and a
/// next episode to come) when `auto`, plus the user's own series rows; a "don't guard" row wins.
/// Progress merges across playlists through the episode fingerprint's series key; seen series
/// airings (TV, catch-up) count as done.
fn series_marks(
    conn: &rusqlite::Connection,
    rules: &[crate::model::SpoilerRule],
    auto: bool,
    lib: &Library,
    now: Timestamp,
) -> Result<HashMap<String, SeriesMark>> {
    #[derive(Default)]
    struct Acc {
        done: Option<(i32, i32)>,
        started: Option<(i32, i32)>,
        last_at: Timestamp,
    }
    let mut acc: HashMap<String, Acc> = HashMap::new();
    for p in db::episode_progress(conn)? {
        let a = acc.entry(p.series_norm).or_default();
        let se = (p.season, p.episode);
        if p.done {
            a.done = a.done.max(Some(se));
        } else if p.started {
            a.started = a.started.max(Some(se));
        }
        a.last_at = a.last_at.max(p.updated_at);
    }
    for s in db::seen_episodes(conn)? {
        if let (Some(norm), Some(se), Some(ep)) = (s.series_norm, s.season, s.episode) {
            let a = acc.entry(norm).or_default();
            a.done = a.done.max(Some((se, ep)));
            a.last_at = a.last_at.max(s.seen_at);
        }
    }
    let series_rules: HashMap<&str, &crate::model::SpoilerRule> =
        rules.iter().filter(|r| r.kind == SpoilerRuleKind::Series).filter_map(|r| Some((r.series_norm.as_deref()?, r))).collect();
    let mut wanted: Vec<(String, Option<&crate::model::SpoilerRule>)> = Vec::new();
    if auto {
        for (norm, a) in &acc {
            if a.done.is_some() && a.last_at >= now - SERIES_ACTIVE_S && !series_rules.contains_key(norm.as_str()) {
                wanted.push((norm.clone(), None));
            }
        }
    }
    wanted.extend(series_rules.iter().filter(|(_, r)| r.enabled).map(|(n, r)| (n.to_string(), Some(*r))));
    let mut out = HashMap::new();
    for (norm, rule) in wanted {
        let rows = db::series_rows(conn, &norm)?;
        if rows.iter().any(|r| lib.explicit.refs.contains(&r.item)) {
            continue; // never guard (or list) explicit series
        }
        let a = acc.get(&norm);
        let done = a.and_then(|a| a.done);
        let started = a.and_then(|a| a.started);
        let next_up = match (started, done) {
            (Some(s), d) if Some(s) > d => Some(s),
            _ => match db::next_episode_after(conn, &norm, done.unwrap_or((0, 0)))? {
                Some(n) => Some(n),
                // nothing more in any playlist: a finished series isn't part-way; the user's own
                // row (or a series the catalogue lost) guards after the last one seen
                None if rule.is_some() || rows.is_empty() => Some(done.map_or((1, 1), |(s, e)| (s, e + 1))),
                None => None,
            },
        };
        let Some(next_up) = next_up else { continue };
        let label = rule.map(|r| r.label.clone()).or_else(|| rows.first().map(|r| r.name.clone())).unwrap_or_else(|| norm.clone());
        let mark = SeriesMark { label, next_up, automatic: rule.is_none(), rule: rule.map(|r| r.id) };
        for r in &rows {
            out.entry(super::series_key(&r.name)).or_insert_with(|| mark.clone());
        }
        out.insert(norm, mark);
    }
    Ok(out)
}
