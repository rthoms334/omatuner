//! Events → channel choice → airings → caps → clashes → reconcile against the ledger (pure;
//! DESIGN §33 §5.4). OWNER: F-AB.
//!
//! The scan hands over [`Hit`]s (a rule's terms hit a guide row on a followable channel); this
//! module groups them into events (one airing, simulcasts merged), picks the channel through the
//! shared ranking (B5), applies the rule's airings, days and hours, the weekly and global caps
//! and switch clashes, and compares the result with the ledger: the [`Plan`] says which ledger
//! rows to create, update or retire and which reminders to create, replace or cancel.
//! `store::follow::apply_plan` writes it in one transaction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::follow::matcher::LeftOut;
use crate::follow::{Airings, ChannelScope, FollowMode, FollowRule};
use crate::model::{ChannelId, FavListId, FollowRuleId, ItemRef, ReminderId, Timestamp};
use crate::store::copies::{RankCand, RankCtx, rank_channels};
use crate::textmatch::airing::AiringKind;

/// The state of one ledger row (`follow_match.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchState {
    /// A reminder the rule owns.
    Scheduled,
    /// The user's own reminder already covered it (never deleted by Following).
    Adopted,
    /// A "just list it" rule: no reminder.
    Listed,
    /// Over the rule's weekly limit or the global pending cap: greyed out, no reminder.
    OverCap,
    /// The user removed its reminder: never re-created.
    Skipped,
    /// No longer in the guide (inside its coverage), or the rule was paused.
    Gone,
    /// It aired (maintenance).
    Aired,
    /// Preview only: the rule's terms hit, but it is left out (the note says why).
    LeftOut,
}

impl MatchState {
    /// The `follow_match.state` text.
    pub fn as_str(self) -> &'static str {
        match self {
            MatchState::Scheduled => "scheduled",
            MatchState::Adopted => "adopted",
            MatchState::Listed => "listed",
            MatchState::OverCap => "over-cap",
            MatchState::Skipped => "skipped",
            MatchState::Gone => "gone",
            MatchState::Aired => "aired",
            MatchState::LeftOut => "left-out",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "scheduled" => MatchState::Scheduled,
            "adopted" => MatchState::Adopted,
            "listed" => MatchState::Listed,
            "over-cap" => MatchState::OverCap,
            "skipped" => MatchState::Skipped,
            "gone" => MatchState::Gone,
            "aired" => MatchState::Aired,
            _ => return None,
        })
    }

    /// Counts toward the rule's weekly limit.
    fn counts(self) -> bool {
        matches!(self, MatchState::Scheduled | MatchState::Adopted | MatchState::Listed | MatchState::Aired)
    }
}

/// One matched airing (preview rows, the Upcoming list, ledger writes).
#[derive(Debug, Clone, PartialEq)]
pub struct Upcoming {
    pub rule: FollowRuleId,
    pub event_key: String,
    /// Displayed times (the chosen channel's effective shift applied).
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub channel: Option<ItemRef>,
    pub channel_name: String,
    /// ≤ 3 other channels showing it, best first ("Watch on another channel", "Also on").
    pub alternates: Vec<ItemRef>,
    pub state: MatchState,
    /// What its reminder does: the rule's mode, or Notify when downgraded (`note`).
    pub mode: FollowMode,
    /// `clash:<rule>` (`clash:0`: your own switch) | `locked` | a left-out reason in previews.
    pub note: Option<String>,
}

/// A preview run's answer ("In the guide now · 4 match · 2 left out").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Preview {
    pub matches: Vec<Upcoming>,
    pub left_out: Vec<Upcoming>,
    /// "'England' also matches rugby and cricket".
    pub warnings: Vec<String>,
}

/// What happens to a row's reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderOp {
    Keep,
    /// Make one (or adopt the user's reminder already there).
    Create,
    /// Cancel the owned reminder (`RemoveWhy::Rule`) and make a new one (moved, other channel,
    /// other mode).
    Replace(ReminderId),
    /// Cancel the owned reminder (`RemoveWhy::Rule`).
    Cancel(ReminderId),
}

/// One ledger write.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The ledger row (`None`: a new row).
    pub row: Option<i64>,
    pub to: Upcoming,
    pub reminder: ReminderOp,
    /// It moved from this start (one toast: "Arsenal match moved to 17:30").
    pub moved_from: Option<Timestamp>,
}

/// What an evaluation writes (one transaction, `store::follow::apply_plan`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Hits that were left out (previews show them; evaluations drop them).
    pub left_out: Vec<Upcoming>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ------------------------------------------------------------------ inputs

/// A rule's terms hit a guide row on a followable channel (the scan's output).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Index into the evaluated rules.
    pub rule: usize,
    pub epg_channel: i64,
    /// Guide times (no channel shift).
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub categories: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub verdict: Result<AiringKind, LeftOut>,
}

/// A channel Following may choose: visible, supported, from an enabled playlist, mapped to a
/// guide channel and **never explicit** (in any parental state).
#[derive(Debug, Clone)]
pub struct Cand {
    pub id: ChannelId,
    pub item: ItemRef,
    pub name: Arc<str>,
    pub epg_channel: i64,
    /// Effective shift (`ChannelSummary::tvg_shift_min`, B8), seconds.
    pub shift_s: i64,
    pub rank: RankCand,
}

/// The followable channels of one library snapshot.
#[derive(Debug, Default)]
pub struct Pool {
    cands: Vec<Cand>,
    by_epg: HashMap<i64, Vec<u32>>,
    /// Index into `cands`.
    by_ref: HashMap<ItemRef, u32>,
    fav_lists: HashMap<ChannelId, Vec<FavListId>>,
    /// The shared ranking's context (B5) without a rule's pin and quality cap.
    rank: RankCtx,
    /// A PIN is set: a locked channel needs it (a rule's switch onto it becomes a notification).
    pin_active: bool,
}

impl Pool {
    /// From a library snapshot: `trouble` = channels that failed in the last 24 h.
    pub fn build(lib: &crate::store::library::Library, trouble: HashSet<ChannelId>, pin_active: bool) -> Self {
        let live = &lib.live;
        let mut pool = Pool { pin_active, ..Default::default() };
        for c in &live.channels {
            let Some(epg) = c.epg_channel_id else { continue };
            if c.unsupported || c.radio || live.hidden.contains(&c.id) {
                continue;
            }
            let item = ItemRef { source: c.source_id, kind: crate::model::ItemKind::Channel, key: c.key.clone() };
            if lib.explicit.channels.contains(&c.id) || lib.explicit.refs.contains(&item) {
                continue;
            }
            pool.by_epg.entry(epg.0).or_default().push(pool.cands.len() as u32);
            pool.by_ref.insert(item.clone(), pool.cands.len() as u32);
            pool.cands.push(Cand {
                id: c.id,
                item,
                name: c.name.clone(),
                epg_channel: epg.0,
                shift_s: i64::from(c.tvg_shift_min) * 60,
                rank: RankCand::from(c),
            });
        }
        let mut favourites = HashMap::new();
        for (li, list) in lib.favs.lists.iter().enumerate() {
            for (pi, item) in lib.favs.items(list.id).iter().enumerate() {
                if let Some(id) = pool.by_ref.get(item).map(|&i| pool.cands[i as usize].id) {
                    favourites.entry(id).or_insert((li, pi));
                    pool.fav_lists.entry(id).or_default().push(list.id);
                }
            }
        }
        pool.rank = RankCtx {
            session_locked: pin_active,
            trouble,
            favourites,
            source_order: lib.sources.iter().map(|s| s.id).collect(),
            ..Default::default()
        };
        pool
    }

    /// The guide channels worth scanning.
    pub fn epg_channels(&self) -> impl Iterator<Item = i64> + '_ {
        self.by_epg.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.cands.is_empty()
    }

    /// A channel of the pool by reference.
    pub fn channel(&self, item: &ItemRef) -> Option<&Cand> {
        self.by_ref.get(item).map(|&i| &self.cands[i as usize])
    }

    fn in_scope(&self, c: &Cand, scope: &ChannelScope) -> bool {
        match scope {
            ChannelScope::Any => true,
            ChannelScope::Favourites => self.fav_lists.contains_key(&c.id),
            ChannelScope::List(l) => self.fav_lists.get(&c.id).is_some_and(|v| v.contains(l)),
            ChannelScope::Playlist(s) => c.item.source == *s,
            ChannelScope::Channels(refs) => refs.contains(&c.item),
        }
    }

    /// The candidates showing guide channels `epgs` for `rule`, best first (B5): only the
    /// smallest time shift (a "+1" copy airs an hour later — never the live match).
    fn ranked(&self, epgs: &[i64], rule: &FollowRule) -> Vec<&Cand> {
        let mut cands: Vec<&Cand> = epgs
            .iter()
            .filter_map(|e| self.by_epg.get(e))
            .flatten()
            .map(|&i| &self.cands[i as usize])
            .filter(|c| self.in_scope(c, &rule.spec.channels))
            .collect();
        let Some(min_shift) = cands.iter().map(|c| c.shift_s.abs()).min() else { return Vec::new() };
        cands.retain(|c| c.shift_s.abs() == min_shift);
        let mut ctx = self.rank.clone();
        ctx.pinned = rule.spec.prefer.pin.as_ref().and_then(|p| self.channel(p)).map(|c| c.id);
        ctx.max_quality = rule.spec.prefer.max_quality;
        let ranks: Vec<RankCand> = cands.iter().map(|c| c.rank).collect();
        let order = rank_channels(&ranks, &ctx);
        order.iter().filter_map(|(id, _)| cands.iter().find(|c| c.id == *id).copied()).collect()
    }
}

/// One ledger row as the plan reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerRow {
    pub id: i64,
    pub rule: FollowRuleId,
    pub event_key: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub title: String,
    pub sub_title: Option<String>,
    pub channel: Option<ItemRef>,
    pub alternates: Vec<ItemRef>,
    pub state: MatchState,
    pub note: Option<String>,
    pub reminder: Option<ReminderId>,
    /// The guide still covers its start (`start < epg_source.max_stop` of its channel's guide):
    /// a row outside the coverage never turns `gone` because a guide got shorter.
    pub covered: bool,
}

/// Everything a plan reads besides the hits.
pub struct PlanCtx<'a> {
    /// The evaluated rules (`Hit::rule` indexes them), enabled ones only.
    pub rules: &'a [FollowRule],
    pub pool: &'a Pool,
    /// Ledger rows of every rule (paused and deleted rules' rows retire).
    pub ledger: &'a [LedgerRow],
    /// Pending switches that are not a rule's (yours or Claude's): they win every clash.
    pub manual_switches: &'a [(Timestamp, Timestamp)],
    /// Rule-made pending reminders, at most (`follow.max_pending`).
    pub max_pending: usize,
    pub now: Timestamp,
    pub tz: &'a jiff::tz::TimeZone,
}

/// Hits within this many seconds are one event (simulcasts on different guide channels).
const SIMULCAST_S: i64 = 600;
/// A ledger row and a fresh event of the same key this close are one airing (it moved).
const MOVE_WINDOW_S: i64 = 36 * 3600;

/// An event before the ledger: one airing of one rule.
#[derive(Debug, Clone)]
struct Event {
    rule: usize,
    up: Upcoming,
    airing: AiringKind,
    week: (i16, i8),
    left_out: Option<LeftOut>,
}

/// Local (date, weekday Monday = 0, minute of day, ISO week) of `t`.
fn local(t: Timestamp, tz: &jiff::tz::TimeZone) -> (String, u8, u16, (i16, i8)) {
    match jiff::Timestamp::from_second(t) {
        Ok(ts) => {
            let z = ts.to_zoned(tz.clone());
            let wk = z.date().iso_week_date();
            (z.date().to_string(), z.weekday().to_monday_zero_offset() as u8, (z.hour() as u16) * 60 + z.minute() as u16, (wk.year(), wk.week()))
        }
        Err(_) => (String::new(), 0, 0, (0, 0)),
    }
}

/// The more telling of two keys: a fixture, then an episode, then a dated title.
fn key_rank(k: &str) -> u8 {
    if k.contains('|') {
        0
    } else if k.contains('#') {
        1
    } else {
        2
    }
}

/// Group hits into events and choose their channels (left-outs kept apart): one rule's hits
/// within [`SIMULCAST_S`] of each other, split by what they are — two different games at the
/// same time on two channels are two events, never one with the other as an alternate.
fn events(hits: &mut [Hit], ctx: &PlanCtx<'_>) -> Vec<Event> {
    hits.sort_by_key(|h| (h.rule, h.start, h.epg_channel));
    let mut out = Vec::new();
    let mut i = 0;
    while i < hits.len() {
        let mut j = i + 1;
        while j < hits.len() && hits[j].rule == hits[i].rule && hits[j].start - hits[i].start <= SIMULCAST_S {
            j += 1;
        }
        for group in split_window(&hits[i..j], ctx.tz) {
            if let Some(e) = event(&group, ctx) {
                out.push(e);
            }
        }
        i = j;
    }
    out
}

/// One window's hits by event. A fixture is its teams (two spellings of one match — "Man Utd"
/// and "Manchester United" against the same side — stay one), a show episode its core title
/// (one guide may carry S/E and another an episode title). A hit with only a title joins its
/// show's episode, else the window's one fixture or episode, else its own title.
fn split_window<'h>(window: &'h [Hit], tz: &jiff::tz::TimeZone) -> Vec<Vec<&'h Hit>> {
    if window.len() == 1 {
        return vec![window.iter().collect()];
    }
    let date = local(window[0].start, tz).0;
    let keyed: Vec<(&Hit, String)> = window.iter().map(|h| (h, key_of(h, &date))).collect();
    // (the first key of a group, its hits)
    let mut groups: Vec<(String, Vec<&Hit>)> = Vec::new();
    let mut plain: Vec<(&Hit, &str)> = Vec::new();
    for (h, k) in &keyed {
        let found = match key_rank(k) {
            0 => groups.iter_mut().find(|(g, _)| same_fixture(g, k)),
            1 => groups.iter_mut().find(|(g, _)| key_rank(g) == 1 && core_of(g) == core_of(k)),
            _ => {
                plain.push((h, k));
                continue;
            }
        };
        match found {
            Some((_, v)) => v.push(h),
            None => groups.push((k.clone(), vec![h])),
        }
    }
    let specific = groups.len();
    for (h, k) in plain {
        let to = groups[..specific]
            .iter()
            .position(|(g, _)| key_rank(g) == 1 && core_of(g) == core_of(k))
            .or_else(|| (specific == 1).then_some(0))
            .or_else(|| groups.iter().position(|(g, _)| g == k));
        match to {
            Some(i) => groups[i].1.push(h),
            None => groups.push((k.to_owned(), vec![h])),
        }
    }
    groups.into_iter().map(|(_, v)| v).collect()
}

/// A key's title part (before its `#` episode or `@` date).
fn core_of(k: &str) -> &str {
    k.split(['#', '@']).next().unwrap_or(k)
}

/// The two sides of a fixture key (`a|b@date`).
fn sides(k: &str) -> Option<(&str, &str)> {
    k.rsplit_once('@').map_or(k, |(body, _)| body).split_once('|')
}

/// Two spellings of one side ("man utd", "manchester united"; "wolves", "wolverhampton"): a
/// word in common, or first words where one begins the other or that share four letters.
fn alike(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let words = |s: &'_ str| s.split_whitespace().filter(|w| w.chars().count() >= 3).map(str::to_owned).collect::<Vec<_>>();
    let wb = words(b);
    if words(a).iter().any(|w| wb.contains(w)) {
        return true;
    }
    let (Some(x), Some(y)) = (a.split_whitespace().next(), b.split_whitespace().next()) else { return false };
    let common = x.chars().zip(y.chars()).take_while(|(p, q)| p == q).count();
    common >= 4 || (common >= 3 && (x.starts_with(y) || y.starts_with(x)))
}

/// One match under two fixture keys: a side in common and the other side alike (a team never
/// plays two matches at once).
fn same_fixture(k1: &str, k2: &str) -> bool {
    if k1 == k2 {
        return true;
    }
    let (Some((a, b)), Some((c, d))) = (sides(k1), sides(k2)) else { return false };
    (a == c && alike(b, d)) || (a == d && alike(b, c)) || (b == c && alike(a, d)) || (b == d && alike(a, c))
}

fn event(group: &[&Hit], ctx: &PlanCtx<'_>) -> Option<Event> {
    let rule = &ctx.rules[group[0].rule];
    let ok: Vec<&Hit> = group.iter().copied().filter(|h| h.verdict.is_ok()).collect();
    let (used, left_out): (Vec<&Hit>, Option<LeftOut>) = if ok.is_empty() { (group.to_vec(), group[0].verdict.clone().err()) } else { (ok, None) };
    let mut epgs: Vec<i64> = used.iter().map(|h| h.epg_channel).collect();
    epgs.sort_unstable();
    epgs.dedup();
    let ranked = ctx.pool.ranked(&epgs, rule);
    let Some(chosen) = ranked.first() else {
        // nothing in the rule's channel scope shows it
        let h = used[0];
        let (date, _, _, week) = local(h.start, ctx.tz);
        return Some(Event {
            rule: h.rule,
            up: upcoming(rule, h, h.start, h.stop, key_of(h, &date), None, String::new(), Vec::new()),
            airing: h.verdict.clone().unwrap_or(AiringKind::Unknown),
            week,
            left_out: Some(left_out.unwrap_or(LeftOut::Channel)),
        });
    };
    let hit = used.iter().find(|h| h.epg_channel == chosen.epg_channel).copied().unwrap_or(used[0]);
    let (start, stop) = (hit.start + chosen.shift_s, hit.stop + chosen.shift_s);
    let (date, wd, minute, week) = local(start, ctx.tz);
    let key = used.iter().map(|h| key_of(h, &date)).min_by(|a, b| (key_rank(a), a).cmp(&(key_rank(b), b))).unwrap_or_default();
    let airing = used
        .iter()
        .filter_map(|h| h.verdict.clone().ok())
        .min_by_key(|a| match a {
            AiringKind::Live => 0,
            AiringKind::New => 1,
            _ => 2,
        })
        .unwrap_or(AiringKind::Unknown);
    let mut alternates: Vec<ItemRef> = Vec::new();
    for c in ranked.iter().skip(1) {
        if alternates.len() == 3 {
            break;
        }
        if !alternates.contains(&c.item) {
            alternates.push(c.item.clone());
        }
    }
    let left_out = left_out.or_else(|| (!rule.spec.admits_time(wd, minute)).then_some(LeftOut::Time));
    Some(Event {
        rule: hit.rule,
        up: upcoming(rule, hit, start, stop, key, Some(chosen.item.clone()), chosen.name.to_string(), alternates),
        airing,
        week,
        left_out,
    })
}

fn key_of(h: &Hit, local_date: &str) -> String {
    crate::textmatch::event::event_key(&crate::textmatch::event::EventInput {
        title: &h.title,
        sub_title: h.sub_title.as_deref(),
        description: None,
        season: h.season,
        episode: h.episode,
        local_date,
    })
}

#[allow(clippy::too_many_arguments)]
fn upcoming(
    rule: &FollowRule,
    h: &Hit,
    start: Timestamp,
    stop: Timestamp,
    key: String,
    channel: Option<ItemRef>,
    channel_name: String,
    alternates: Vec<ItemRef>,
) -> Upcoming {
    Upcoming {
        rule: rule.id,
        event_key: key,
        start,
        stop,
        title: h.title.clone(),
        sub_title: h.sub_title.clone(),
        channel,
        channel_name,
        alternates,
        state: MatchState::Scheduled,
        mode: rule.mode,
        note: None,
    }
}

/// Build the plan (`hits` are reordered). Left-out hits and events end in `Plan::left_out`.
pub fn build(hits: &mut [Hit], ctx: &PlanCtx<'_>) -> Plan {
    let mut events = events(hits, ctx);
    events.sort_by_key(|e| (e.up.start, e.rule));
    let mut plan = Plan::default();

    // ---- airings: live / new / first airing (per rule and event key)
    let mut live_keys: HashSet<(usize, String)> = HashSet::new();
    for e in events.iter().filter(|e| e.left_out.is_none() && e.airing == AiringKind::Live) {
        live_keys.insert((e.rule, e.up.event_key.clone()));
    }
    let aired: HashSet<(FollowRuleId, &str)> =
        ctx.ledger.iter().filter(|r| r.stop <= ctx.now && r.state != MatchState::Gone).map(|r| (r.rule, r.event_key.as_str())).collect();
    let mut seen: HashSet<(usize, String)> = HashSet::new();
    for e in events.iter_mut().filter(|e| e.left_out.is_none()) {
        let rule = &ctx.rules[e.rule];
        let key = (e.rule, e.up.event_key.clone());
        let first = !seen.contains(&key) && !aired.contains(&(rule.id, e.up.event_key.as_str()));
        seen.insert(key.clone());
        e.left_out = match rule.spec.airings {
            Airings::Any => None,
            Airings::NewOnly => (!first).then_some(LeftOut::NotNew),
            Airings::LiveOnly | Airings::LiveOrNew => {
                let marked = e.airing == AiringKind::Live || (rule.spec.airings == Airings::LiveOrNew && e.airing == AiringKind::New);
                (!marked && (live_keys.contains(&key) || !first)).then_some(LeftOut::NotLive)
            }
        };
    }
    let (events, left): (Vec<Event>, Vec<Event>) = events.into_iter().partition(|e| e.left_out.is_none());
    plan.left_out = left
        .into_iter()
        .map(|mut e| {
            e.up.state = MatchState::LeftOut;
            e.up.note = e.left_out.map(|l| l.label());
            e.up
        })
        .collect();

    // ---- the ledger: which row is which event
    let mut used_rows: HashSet<i64> = HashSet::new();
    let by_rule: HashMap<FollowRuleId, Vec<&LedgerRow>> = ctx.ledger.iter().fold(HashMap::new(), |mut m, r| {
        m.entry(r.rule).or_default().push(r);
        m
    });
    let mut matched: Vec<Option<&LedgerRow>> = Vec::with_capacity(events.len());
    for e in &events {
        let rows = by_rule.get(&e.up.rule).map(Vec::as_slice).unwrap_or(&[]);
        let open = |r: &&&LedgerRow| !used_rows.contains(&r.id) && (r.stop > ctx.now || r.state == MatchState::Gone);
        let same_key = rows.iter().filter(open).filter(|r| r.event_key == e.up.event_key && (r.start - e.up.start).abs() <= MOVE_WINDOW_S);
        let same_slot = || rows.iter().filter(open).filter(|r| r.channel == e.up.channel && (r.start - e.up.start).abs() <= SIMULCAST_S);
        let row = same_key.min_by_key(|r| (r.start - e.up.start).abs()).or_else(|| same_slot().min_by_key(|r| (r.start - e.up.start).abs())).copied();
        if let Some(r) = row {
            used_rows.insert(r.id);
        }
        matched.push(row);
    }

    // ---- desired state: skipped stays skipped, adopted stays adopted, caps
    let mut week_count: HashMap<(FollowRuleId, (i16, i8)), u32> = HashMap::new();
    for r in ctx.ledger.iter().filter(|r| r.stop <= ctx.now && r.state.counts()) {
        let (_, _, _, week) = local(r.start, ctx.tz);
        *week_count.entry((r.rule, week)).or_default() += 1;
    }
    let mut pending = 0usize;
    let mut desired: Vec<Option<Upcoming>> = Vec::with_capacity(events.len());
    for (e, row) in events.iter().zip(&matched) {
        let rule = &ctx.rules[e.rule];
        let mut up = e.up.clone();
        if row.is_none() && up.start <= ctx.now {
            // already on: a new match that started is not worth a reminder
            desired.push(None);
            continue;
        }
        up.state = match row.map(|r| r.state) {
            Some(MatchState::Skipped) => MatchState::Skipped,
            // adopted while that reminder is still there (another rule's may have gone with it)
            Some(MatchState::Adopted) if row.is_some_and(|r| r.start == up.start && r.reminder.is_some()) => MatchState::Adopted,
            _ if rule.mode == FollowMode::List => MatchState::Listed,
            _ => MatchState::Scheduled,
        };
        if up.state != MatchState::Skipped {
            let n = week_count.entry((rule.id, e.week)).or_default();
            if *n >= rule.max_per_week {
                up.state = MatchState::OverCap;
            } else {
                *n += 1;
            }
        }
        if up.state == MatchState::Scheduled {
            if pending >= ctx.max_pending {
                up.state = MatchState::OverCap;
            } else {
                pending += 1;
            }
        }
        desired.push(Some(up));
    }

    // ---- switches: locked targets and clashes become notifications
    let mut order: Vec<usize> = (0..events.len())
        .filter(|&i| {
            desired[i].as_ref().is_some_and(|u| u.state == MatchState::Scheduled && matches!(u.mode, FollowMode::Switch | FollowMode::Flash))
        })
        .collect();
    order.sort_by_key(|&i| (ctx.rules[events[i].rule].priority, events[i].up.start));
    let mut taken: Vec<(Timestamp, Timestamp, FollowRuleId)> = ctx.manual_switches.iter().map(|&(s, e)| (s, e, FollowRuleId(0))).collect();
    for i in order {
        let up = desired[i].as_mut().expect("filtered");
        let locked = up.channel.as_ref().and_then(|c| ctx.pool.channel(c)).is_some_and(|c| c.rank.locked) && ctx.pool.pin_active;
        if locked {
            up.mode = FollowMode::Notify;
            up.note = Some("locked".into());
        } else if let Some(w) = taken.iter().find(|(s, e, _)| *s < up.stop && up.start < *e) {
            up.mode = FollowMode::Notify;
            up.note = Some(format!("clash:{}", w.2.0));
        } else {
            taken.push((up.start, up.stop, up.rule));
        }
    }

    // ---- steps: compare with the ledger
    for ((e, row), to) in events.iter().zip(&matched).zip(desired) {
        let Some(to) = to else { continue };
        let Some(row) = row else {
            let op = if to.state == MatchState::Scheduled { ReminderOp::Create } else { ReminderOp::Keep };
            plan.steps.push(Step { row: None, to, reminder: op, moved_from: None });
            continue;
        };
        let rule = &ctx.rules[e.rule];
        let was_mode = if row.note.as_deref().is_some_and(|n| n == "locked" || n.starts_with("clash:")) { FollowMode::Notify } else { rule.mode };
        let moved = row.start != to.start;
        let reminder = match (row.state, to.state, row.reminder) {
            (MatchState::Scheduled, MatchState::Scheduled, Some(id)) if moved || row.channel != to.channel || was_mode != to.mode => {
                ReminderOp::Replace(id)
            }
            (MatchState::Scheduled, MatchState::Scheduled, Some(_)) => ReminderOp::Keep,
            (MatchState::Scheduled, _, Some(id)) => ReminderOp::Cancel(id),
            (_, MatchState::Scheduled, _) => ReminderOp::Create,
            _ => ReminderOp::Keep,
        };
        // a match already on gets no new reminder (it would only fire late)
        let reminder =
            if to.start <= ctx.now && matches!(reminder, ReminderOp::Create | ReminderOp::Replace(_)) { ReminderOp::Keep } else { reminder };
        let moved_from =
            (moved && matches!(row.state, MatchState::Scheduled | MatchState::Adopted | MatchState::Listed) && (to.start - row.start).abs() > 60)
                .then_some(row.start);
        let same = !moved
            && row.stop == to.stop
            && row.state == to.state
            && row.note == to.note
            && row.channel == to.channel
            && row.alternates == to.alternates
            && row.title == to.title
            && row.sub_title == to.sub_title
            && row.event_key == to.event_key
            && reminder == ReminderOp::Keep;
        if !same {
            plan.steps.push(Step { row: Some(row.id), to, reminder, moved_from });
        }
    }

    // ---- rows no fresh event claimed: gone (inside the guide's coverage, or the rule paused)
    let active: HashSet<FollowRuleId> = ctx.rules.iter().map(|r| r.id).collect();
    for r in ctx.ledger {
        if used_rows.contains(&r.id) || !matches!(r.state, MatchState::Scheduled | MatchState::Adopted | MatchState::Listed | MatchState::OverCap) {
            continue;
        }
        if r.start <= ctx.now || !(r.covered || !active.contains(&r.rule)) {
            continue;
        }
        let reminder = match (r.state, r.reminder) {
            (MatchState::Scheduled, Some(id)) => ReminderOp::Cancel(id),
            _ => ReminderOp::Keep,
        };
        let to = Upcoming {
            rule: r.rule,
            event_key: r.event_key.clone(),
            start: r.start,
            stop: r.stop,
            title: r.title.clone(),
            sub_title: r.sub_title.clone(),
            channel: r.channel.clone(),
            channel_name: String::new(),
            alternates: r.alternates.clone(),
            state: MatchState::Gone,
            mode: FollowMode::Notify,
            note: None,
        };
        plan.steps.push(Step { row: Some(r.id), to, reminder, moved_from: None });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::FollowKind;
    use crate::model::{ChannelSummary, EpgChannelId, ItemKey, ItemKind, Quality, QualityBadge, SourceId};
    use crate::store::library::{Library, LiveIndex};

    const NOW: Timestamp = 1_790_000_000; // 2026-09-21 (Monday) 14:13 UTC
    const H: i64 = 3600;

    fn summary(id: i64, src: i64, epg: i64, name: &str, q: Option<Quality>, shift_min: i32) -> ChannelSummary {
        ChannelSummary {
            id: ChannelId(id),
            source_id: SourceId(src),
            key: ItemKey(format!("k{id}")),
            name: name.into(),
            provider_name: None,
            number: None,
            logo: None,
            group_ids: Vec::new(),
            epg_channel_id: Some(EpgChannelId(epg)),
            tvg_shift_min: shift_min,
            has_catchup: false,
            catchup_days: 0,
            radio: false,
            locked: false,
            unsupported: false,
            provider_order: id as i32,
            user_order: None,
            added_at: None,
            quality: q.map(|class| QualityBadge::named(class, false)),
            family: 0,
        }
    }

    fn item(id: i64, src: i64) -> ItemRef {
        ItemRef { source: SourceId(src), kind: ItemKind::Channel, key: ItemKey(format!("k{id}")) }
    }

    fn lib(channels: Vec<ChannelSummary>) -> Library {
        Library { live: Arc::new(LiveIndex::from_parts(channels, Vec::new(), HashMap::new(), HashSet::new())), ..Default::default() }
    }

    fn rule(id: i64, kind: FollowKind, mode: FollowMode) -> FollowRule {
        let mut r = FollowRule::draft(format!("rule {id}"), kind, vec!["Arsenal".into()], &Default::default(), 0);
        r.id = FollowRuleId(id);
        r.priority = id;
        r.mode = mode;
        r
    }

    fn hit(rule: usize, epg: i64, start: Timestamp, title: &str, sub: Option<&str>, verdict: Result<AiringKind, LeftOut>) -> Hit {
        Hit {
            rule,
            epg_channel: epg,
            start,
            stop: start + 2 * H,
            title: title.into(),
            sub_title: sub.map(Into::into),
            categories: None,
            season: None,
            episode: None,
            verdict,
        }
    }

    fn ctx<'a>(
        rules: &'a [FollowRule],
        pool: &'a Pool,
        ledger: &'a [LedgerRow],
        manual: &'a [(Timestamp, Timestamp)],
        tz: &'a jiff::tz::TimeZone,
    ) -> PlanCtx<'a> {
        PlanCtx { rules, pool, ledger, manual_switches: manual, max_pending: 300, now: NOW, tz }
    }

    /// The ledger rows a plan would leave (what `apply_plan` writes, ids made up).
    fn applied(plan: &Plan, ledger: &[LedgerRow]) -> Vec<LedgerRow> {
        let mut rows: Vec<LedgerRow> = ledger.to_vec();
        for (n, s) in plan.steps.iter().enumerate() {
            let reminder = match s.reminder {
                ReminderOp::Create | ReminderOp::Replace(_) => Some(ReminderId(1000 + n as i64)),
                ReminderOp::Cancel(_) => None,
                ReminderOp::Keep => s.row.and_then(|id| rows.iter().find(|r| r.id == id)).and_then(|r| r.reminder),
            };
            let row = LedgerRow {
                id: s.row.unwrap_or(500 + n as i64),
                rule: s.to.rule,
                event_key: s.to.event_key.clone(),
                start: s.to.start,
                stop: s.to.stop,
                title: s.to.title.clone(),
                sub_title: s.to.sub_title.clone(),
                channel: s.to.channel.clone(),
                alternates: s.to.alternates.clone(),
                state: s.to.state,
                note: s.to.note.clone(),
                reminder,
                covered: true,
            };
            match rows.iter_mut().find(|r| Some(r.id) == s.row) {
                Some(r) => *r = row,
                None => rows.push(row),
            }
        }
        rows
    }

    fn utc() -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::UTC
    }

    /// Favourites beat quality, quality beats playlist order; an explicit, hidden or
    /// unsupported channel is never chosen; the effective shift moves the start; a "+1" copy
    /// is never the live channel.
    #[test]
    fn channel_choice_follows_the_shared_ranking() {
        let tz = utc();
        let mut chans = vec![
            summary(1, 1, 10, "Sky Sports HD", Some(Quality::Hd), 0),
            summary(2, 2, 10, "Sky Sports FHD", Some(Quality::Fhd), 0),
            summary(3, 1, 10, "Sky Sports UHD", Some(Quality::Uhd), 0),
            summary(4, 1, 10, "Sky Sports +1", Some(Quality::Uhd), 60),
            summary(5, 1, 11, "Adult", Some(Quality::Uhd), 0),
            summary(6, 2, 10, "Sky Sports 4K test", Some(Quality::Uhd), 0),
        ];
        chans[5].unsupported = true;
        let mut l = lib(chans);
        Arc::get_mut(&mut l.explicit).unwrap().channels.insert(ChannelId(5));
        let rules = [rule(1, FollowKind::Team, FollowMode::Notify)];
        let start = NOW + 24 * H;
        let run = |pool: &Pool| {
            let mut hits = vec![
                hit(0, 10, start, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
                hit(0, 11, start, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
            ];
            build(&mut hits, &ctx(&rules, pool, &[], &[], &tz))
        };
        let pool = Pool::build(&l, HashSet::new(), false);
        assert!(pool.channel(&item(5, 1)).is_none(), "explicit: never followable");
        let plan = run(&pool);
        assert_eq!(plan.steps.len(), 1, "one event for the simulcast");
        let to = &plan.steps[0].to;
        assert_eq!(to.channel, Some(item(3, 1)), "best quality first");
        assert_eq!(to.alternates, [item(2, 2), item(1, 1)], "the +1 copy and the unsupported one are no alternates");
        assert_eq!(to.start, start);
        // a favourite wins over quality
        l.favs = crate::store::library::FavIndex::from_parts(
            vec![crate::model::FavouriteList { id: FavListId(1), name: "Favourites".into(), icon: String::new(), sort_index: 0, is_default: true }],
            vec![crate::model::FavouriteEntry {
                list: FavListId(1),
                item: item(1, 1),
                position: 0,
                added_at: 0,
                fingerprint: Default::default(),
                orphaned_since: None,
            }],
        );
        assert_eq!(run(&Pool::build(&l, HashSet::new(), false)).steps[0].to.channel, Some(item(1, 1)));
        // only the +1 channel shows it: its start is shifted
        let l = lib(vec![summary(4, 1, 10, "Sky Sports +1", Some(Quality::Uhd), 60)]);
        assert_eq!(run(&Pool::build(&l, HashSet::new(), false)).steps[0].to.start, start + H);

        // two games at the same time on two channels are two events, neither the other's
        // alternate; one game under two spellings stays one
        let l = lib(vec![summary(1, 1, 10, "TNT Sports 1", None, 0), summary(2, 2, 11, "TNT Sports 2", None, 0)]);
        let pool = Pool::build(&l, HashSet::new(), false);
        let rules = [rule(1, FollowKind::Event, FollowMode::Notify)];
        let two = |a: &str, b: &str| {
            let mut hits = vec![hit(0, 10, start, a, None, Ok(AiringKind::Live)), hit(0, 11, start + 300, b, None, Ok(AiringKind::Live))];
            build(&mut hits, &ctx(&rules, &pool, &[], &[], &tz)).steps
        };
        let steps = two("Real Madrid v Bayern Munich", "PSG v Inter");
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().all(|s| s.to.alternates.is_empty()), "{steps:?}");
        let steps = two("Man Utd v Arsenal", "Manchester United v Arsenal");
        assert_eq!((steps.len(), steps[0].to.alternates.len()), (1, 1));
    }

    /// A moved kick-off moves the reminder (no duplicate); a dismissed match stays dismissed;
    /// `gone` only inside the guide's coverage; a manual reminder is adopted.
    #[test]
    fn reconcile_against_the_ledger() {
        let tz = utc();
        let l = lib(vec![summary(1, 1, 10, "Sky Sports", Some(Quality::Hd), 0)]);
        let pool = Pool::build(&l, HashSet::new(), false);
        let rules = [rule(1, FollowKind::Team, FollowMode::Notify)];
        let sat = NOW + 5 * 24 * H;
        let first = {
            let mut hits = vec![
                hit(0, 10, sat, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
                hit(0, 10, sat + 2 * 24 * H, "Spurs v Arsenal", None, Ok(AiringKind::Live)),
            ];
            build(&mut hits, &ctx(&rules, &pool, &[], &[], &tz))
        };
        assert_eq!(first.steps.iter().map(|s| s.reminder).collect::<Vec<_>>(), [ReminderOp::Create, ReminderOp::Create]);
        let ledger = applied(&first, &[]);

        // unchanged guide: nothing to write
        let mut same = vec![
            hit(0, 10, sat, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
            hit(0, 10, sat + 2 * 24 * H, "Spurs v Arsenal", None, Ok(AiringKind::Live)),
        ];
        assert!(build(&mut same, &ctx(&rules, &pool, &ledger, &[], &tz)).is_empty());

        // the kick-off moves 15:00 → 17:30: the same row, its reminder replaced, one toast
        let mut moved = vec![
            hit(0, 10, sat + 9000, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
            hit(0, 10, sat + 2 * 24 * H, "Spurs v Arsenal", None, Ok(AiringKind::Live)),
        ];
        let plan = build(&mut moved, &ctx(&rules, &pool, &ledger, &[], &tz));
        assert_eq!(plan.steps.len(), 1);
        let s = &plan.steps[0];
        assert_eq!((s.row, s.moved_from, s.to.start), (Some(ledger[0].id), Some(sat), sat + 9000));
        assert_eq!(s.reminder, ReminderOp::Replace(ledger[0].reminder.unwrap()));

        // the user removed the Spurs reminder: skipped, twice over
        let mut skipped = ledger.clone();
        skipped[1].state = MatchState::Skipped;
        skipped[1].reminder = None;
        for _ in 0..2 {
            let mut again = vec![
                hit(0, 10, sat, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
                hit(0, 10, sat + 2 * 24 * H, "Spurs v Arsenal", None, Ok(AiringKind::Live)),
            ];
            let plan = build(&mut again, &ctx(&rules, &pool, &skipped, &[], &tz));
            assert!(plan.is_empty(), "{plan:?}");
            skipped = applied(&plan, &skipped);
        }

        // the Spurs game left the guide: gone only while the guide still covers its day
        let mut only_first = vec![hit(0, 10, sat, "Arsenal v Chelsea", None, Ok(AiringKind::Live))];
        let plan = build(&mut only_first.clone(), &ctx(&rules, &pool, &ledger, &[], &tz));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!((plan.steps[0].to.state, plan.steps[0].reminder), (MatchState::Gone, ReminderOp::Cancel(ledger[1].reminder.unwrap())));
        let mut uncovered = ledger.clone();
        uncovered[1].covered = false;
        assert!(build(&mut only_first, &ctx(&rules, &pool, &uncovered, &[], &tz)).is_empty(), "a shorter guide cancels nothing");

        // pausing the rule retires its rows (the reminders go)
        let plan = build(&mut [], &ctx(&[], &pool, &uncovered, &[], &tz));
        assert_eq!(plan.steps.iter().filter(|s| s.to.state == MatchState::Gone).count(), 2);
    }

    /// The weekly limit holds across evaluations; new-only and live-only skip unmarked
    /// repeats; a clash downgrades by priority with your own switch winning; a locked target
    /// turns a switch into a notification.
    #[test]
    fn caps_airings_and_clashes() {
        let tz = utc();
        let mut chans = vec![summary(1, 1, 10, "One", None, 0), summary(2, 1, 11, "Two", None, 0)];
        chans[1].locked = true;
        let l = lib(chans);
        let pool = Pool::build(&l, HashSet::new(), true);
        let day = 24 * H;
        let mon = 1_790_553_600; // Monday 2026-09-28 00:00 UTC

        // limit 2 a week: Mon, Tue in; Wed over the cap; next Monday in again
        let mut capped = rule(1, FollowKind::Team, FollowMode::Notify);
        capped.max_per_week = 2;
        let mut hits: Vec<Hit> =
            [0, 1, 2, 7].iter().map(|d| hit(0, 10, mon + d * day + 15 * H, &format!("Arsenal v Team{d}"), None, Ok(AiringKind::Live))).collect();
        let plan = build(&mut hits, &ctx(std::slice::from_ref(&capped), &pool, &[], &[], &tz));
        let states: Vec<MatchState> = plan.steps.iter().map(|s| s.to.state).collect();
        assert_eq!(states, [MatchState::Scheduled, MatchState::Scheduled, MatchState::OverCap, MatchState::Scheduled]);

        // new only: the unflagged Saturday repeat of Tuesday's episode stays quiet, and so does
        // a repeat of an episode that aired last week
        let show = rule(2, FollowKind::Show, FollowMode::Notify);
        let ep = |start, ep| Hit { season: Some(20), episode: Some(ep), ..hit(0, 10, start, "Taskmaster", None, Ok(AiringKind::Unknown)) };
        let aired = LedgerRow {
            id: 9,
            rule: show.id,
            event_key: "taskmaster#s20e03".into(),
            start: NOW - 6 * day,
            stop: NOW - 6 * day + H,
            title: "Taskmaster".into(),
            sub_title: None,
            channel: Some(item(1, 1)),
            alternates: Vec::new(),
            state: MatchState::Aired,
            note: None,
            reminder: None,
            covered: true,
        };
        let mut hits = vec![ep(mon + day + 21 * H, 4), ep(mon + 5 * day + 13 * H, 4), ep(mon + 2 * day, 3)];
        let plan = build(&mut hits, &ctx(std::slice::from_ref(&show), &pool, std::slice::from_ref(&aired), &[], &tz));
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].to.event_key, "taskmaster#s20e04");
        assert_eq!(plan.left_out.iter().map(|u| u.note.clone().unwrap()).collect::<Vec<_>>(), ["shown before", "shown before"]);

        // live only: an unmarked airing after a live-marked one is the replay
        let team = rule(3, FollowKind::Team, FollowMode::Notify);
        let mut hits = vec![
            hit(0, 10, mon + 15 * H, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
            hit(0, 10, mon + 22 * H, "Arsenal v Chelsea", None, Ok(AiringKind::Unknown)),
        ];
        let plan = build(&mut hits, &ctx(std::slice::from_ref(&team), &pool, &[], &[], &tz));
        assert_eq!((plan.steps.len(), plan.left_out[0].note.as_deref()), (1, Some("not live")));

        // clashes: your own switch beats rule 1; rule 1 beats rule 4 (lower priority number wins)
        let (a, b) = (rule(1, FollowKind::Team, FollowMode::Switch), rule(4, FollowKind::Event, FollowMode::Switch));
        let rules = [a, b];
        let t = mon + 2 * day + 14 * H;
        let mut hits = vec![
            hit(0, 10, t, "Arsenal v Chelsea", None, Ok(AiringKind::Live)),
            hit(1, 10, t + H, "Formula 1: Race", None, Ok(AiringKind::Live)),
            hit(0, 10, t + day, "Arsenal v Spurs", None, Ok(AiringKind::Live)),
        ];
        let manual = [(t + day - H, t + day + H)];
        let plan = build(&mut hits, &ctx(&rules, &pool, &[], &manual, &tz));
        let got: Vec<(FollowMode, Option<&str>)> = plan.steps.iter().map(|s| (s.to.mode, s.to.note.as_deref())).collect();
        assert_eq!(got, [(FollowMode::Switch, None), (FollowMode::Notify, Some("clash:1")), (FollowMode::Notify, Some("clash:0"))]);

        // a locked target (PIN set) never switches by itself
        let mut hits = vec![hit(0, 11, t, "Arsenal v Chelsea", None, Ok(AiringKind::Live))];
        let plan = build(&mut hits, &ctx(&rules[..1], &pool, &[], &[], &tz));
        assert_eq!((plan.steps[0].to.mode, plan.steps[0].to.note.as_deref()), (FollowMode::Notify, Some("locked")));
    }
}
