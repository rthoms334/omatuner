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
//!   airing without S/E loses its sub-title and description);
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
        Subject::Programme { title, sub_title, categories, season_episode, .. } => {
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
                let hide = season_episode.is_none_or(|se| se > m.next_up);
                if hide {
                    v.hide_sub = true;
                    v.hide_descr = true;
                    v.obscure_image |= season_episode.is_some();
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
    v.guarded() || series_mark(snap, title).is_some_and(|m| season_episode.is_none_or(|se| se >= m.next_up))
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
