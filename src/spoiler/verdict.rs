//! Judging a subject against the snapshot (time enters only as a scan over a topic's few
//! windows). The public types are `guard::{Subject, Verdict}` (B4). OWNER: S1.
//!
//! The rules (DESIGN §34 §2.1, narrowed by the user's decision):
//! * nothing is guarded before an event starts (topics open at their first airing);
//! * a programme, title or notification that mentions an active topic (whole words, in its
//!   category scope) loses its sub-title, description and image and has its scores rewritten; a
//!   title that also carries a result verb is hidden;
//! * while a topic with "sport descriptions" is active, every sport programme loses its
//!   sub-title and description;
//! * a guarded series hides episodes after the next-up one (EPG airings with S/E likewise; an
//!   airing without S/E is placed by [`airing_beyond`] — its sub-title naming an episode, a
//!   seen repeat, its date against the catalogue's air dates, the guide's "new" flag — and
//!   loses its sub-title and description when it's later, or when nothing places it and the
//!   series is part-way);
//! * before the first build the guard fails closed: every episode is hidden, sport text hidden,
//!   scores rewritten.

use crate::guard::{Reason, ReasonKind, Subject, Verdict};
use crate::model::Timestamp;
use crate::spoiler::snapshot::{GuardSnapshot, SeriesMark, folded};
use crate::textmatch::scores::has_result_verb;

/// The verdict on `subj` at `now`.
pub fn judge(snap: &GuardSnapshot, subj: &Subject<'_>, now: Timestamp) -> Verdict {
    if !snap.on {
        return Verdict::default();
    }
    if !snap.ready {
        return fail_closed(subj);
    }
    let mut v = Verdict { neutralise_title: snap.settings.hide_scores_everywhere, ..Default::default() };
    match *subj {
        Subject::Programme { title, sub_title, categories, season_episode, start, is_new } => {
            let ft = folded(title);
            let fs = sub_title.map(folded);
            topic_hits(snap, &ft, fs.as_deref(), categories, now, &mut v);
            if crate::spoiler::is_sport(categories)
                && let Some(t) = snap.topics.iter().find(|t| t.sport_descr && t.active(now).is_some())
            {
                v.hide_sub = true;
                v.hide_descr = true;
                v.neutralise_title = true;
                v.reason.get_or_insert_with(|| reason_of(t, now));
            }
            if let Some(m) = series_mark(snap, title) {
                let beyond = airing_beyond(m, &Airing { title, sub_title, season_episode, start, is_new });
                if beyond.unwrap_or(m.part_way) {
                    v.hide_sub = true;
                    v.hide_descr = true;
                    v.obscure_image |= beyond.is_some();
                    v.reason.get_or_insert_with(|| series_reason(m));
                }
            }
        }
        Subject::Episode { series_norm, season, episode } => {
            if let Some(m) = snap.series.get(series_norm)
                && (season, episode) > m.next_up
            {
                v.hide_sub = true;
                v.hide_descr = true;
                v.obscure_image = true;
                v.reason = Some(series_reason(m));
            }
        }
        Subject::Vod { title, .. } | Subject::Notification { title } => {
            let ft = folded(title);
            topic_hits(snap, &ft, None, &[], now, &mut v);
        }
    }
    v
}

/// Should the tracker record this programme when it's watched? Its title or sub-title names a
/// guarded topic, or it's an airing of a guarded series from its next-up episode on (watching
/// the next-up on TV moves the boundary on). Unrelated sport whose text an active topic hides
/// is never recorded.
pub fn tracks(snap: &GuardSnapshot, subj: &Subject<'_>, now: Timestamp) -> bool {
    if !snap.on || !snap.ready {
        return false;
    }
    let Subject::Programme { title, sub_title, categories, season_episode, .. } = *subj else { return false };
    let mut v = Verdict::default();
    topic_hits(snap, &folded(title), sub_title.map(folded).as_deref(), categories, now, &mut v);
    v.guarded()
        || series_mark(snap, title).is_some_and(|m| match airing_episode(m, season_episode, sub_title) {
            Some(se) => se >= m.next_up,
            // an airing nothing places: unless it is a repeat already seen
            None => !seen_airing(m, title, sub_title),
        })
}

/// What [`airing_beyond`] reads of a guide airing.
#[derive(Debug, Clone, Copy)]
pub struct Airing<'a> {
    pub title: &'a str,
    pub sub_title: Option<&'a str>,
    pub season_episode: Option<(i32, i32)>,
    /// EPG start; 0 when unknown (MCP replies).
    pub start: Timestamp,
    pub is_new: bool,
}

/// How far ahead of the guarded episodes' first air date an airing must be to be shown on
/// the date alone: catalogue dates are often another country's.
const AIR_DATE_MARGIN_DAYS: i64 = 7;

/// Is this airing of a guarded series after its next-up episode? `Some(true)`: later (guard
/// it), `Some(false)`: the next-up one or earlier, or a repeat already seen (show it), `None`:
/// nothing places it (the caller guards it only when the series is part-way). In order:
/// * the guide's S/E, else an episode marker or a catalogue episode title in the sub-title;
/// * an airing seen before (same title and sub-title) shows;
/// * an airing a week or more before the first guarded episode first aired can't show one;
/// * a first showing (the guide's "new" flag) is later, unless it airs before the next-up
///   episode first aired.
pub fn airing_beyond(m: &SeriesMark, a: &Airing<'_>) -> Option<bool> {
    if let Some(se) = airing_episode(m, a.season_episode, a.sub_title) {
        return Some(se > m.next_up);
    }
    if seen_airing(m, a.title, a.sub_title) {
        return Some(false);
    }
    let day = (a.start > 0).then(|| crate::spoiler::local_day(a.start)).flatten();
    if let (Some(day), Some(first)) = (day, m.airings.first_guarded_air)
        && day.saturating_add(jiff::Span::new().days(AIR_DATE_MARGIN_DAYS)) <= first
    {
        return Some(false);
    }
    if a.is_new {
        let earlier = matches!((day, m.airings.next_up_air), (Some(d), Some(n)) if d < n);
        return Some(!earlier);
    }
    None
}

/// The airing's episode: the guide's S/E, else what its sub-title names.
fn airing_episode(m: &SeriesMark, season_episode: Option<(i32, i32)>, sub_title: Option<&str>) -> Option<(i32, i32)> {
    season_episode.or_else(|| sub_title.filter(|s| !s.trim().is_empty()).and_then(|s| m.airings.episode_of(s)))
}

/// An airing of the series without S/E that was seen (its event key: title + sub-title).
fn seen_airing(m: &SeriesMark, title: &str, sub_title: Option<&str>) -> bool {
    let Some(sub) = sub_title.filter(|s| !s.trim().is_empty()) else { return false };
    if m.airings.seen.is_empty() {
        return false;
    }
    m.airings.seen.contains(&crate::spoiler::event_key_of(title, Some(sub), None, None, 0))
}

/// The episode an airing of a guarded series places itself at (a sub-title naming one), for
/// the seen row the tracker or Ctrl+W writes: watching it moves the series on.
pub fn placed_episode(snap: &GuardSnapshot, title: &str, sub_title: Option<&str>) -> Option<(i32, i32)> {
    series_mark(snap, title).and_then(|m| airing_episode(m, None, sub_title))
}

/// Is `title` a series the guard covers?
pub fn guards_series(snap: &GuardSnapshot, title: &str) -> bool {
    series_mark(snap, title).is_some()
}

/// Topics mentioned in the title or sub-title that are active and in scope.
fn topic_hits(snap: &GuardSnapshot, ft: &str, fs: Option<&str>, categories: &[String], now: Timestamp, v: &mut Verdict) {
    let in_title = snap.topics_in(ft);
    let in_sub = fs.map(|s| snap.topics_in(s)).unwrap_or_default();
    for (idx, title_hit) in in_title.iter().map(|&i| (i, true)).chain(in_sub.iter().map(|&i| (i, false))) {
        let Some(t) = snap.topics.get(idx as usize) else { continue };
        if !t.in_scope(categories) || t.active(now).is_none() {
            continue;
        }
        v.hide_sub = true;
        v.hide_descr = true;
        v.obscure_image = true;
        v.neutralise_title = true;
        if title_hit && has_result_verb(ft) {
            v.hide_title = true;
        }
        v.reason.get_or_insert_with(|| reason_of(t, now));
    }
}

fn series_mark<'s>(snap: &'s GuardSnapshot, title: &str) -> Option<&'s SeriesMark> {
    if snap.series.is_empty() {
        return None;
    }
    snap.series.get(&crate::spoiler::series_key(title))
}

fn reason_of(t: &crate::spoiler::snapshot::Topic, now: Timestamp) -> Reason {
    Reason { rule: t.label.clone(), kind: t.reason_kind(), until: t.active(now).flatten() }
}

fn series_reason(m: &SeriesMark) -> Reason {
    Reason { rule: m.label.clone(), kind: ReasonKind::Series, until: None }
}

/// Before the first build (well under a second): hide what could spoil without knowing the
/// rules.
fn fail_closed(subj: &Subject<'_>) -> Verdict {
    let reason = Some(Reason { rule: "Spoiler guard".into(), kind: ReasonKind::Embargo, until: None });
    match *subj {
        Subject::Episode { .. } => Verdict { hide_sub: true, hide_descr: true, obscure_image: true, reason, ..Default::default() },
        Subject::Programme { categories, .. } if crate::spoiler::is_sport(categories) => {
            Verdict { hide_sub: true, hide_descr: true, neutralise_title: true, reason, ..Default::default() }
        }
        _ => Verdict { neutralise_title: true, ..Default::default() },
    }
}
