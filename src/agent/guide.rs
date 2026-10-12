//! `guide_fix` (Guide fixes, DESIGN §31 §6): issues → propose → (the user says yes) → apply →
//! undo. OWNER: agent track.
//!
//! Guardrails: at most 200 channels per proposal (`store::guide_fix::MAX_BATCH`); a proposal
//! lives 10 minutes and is bound to the guide check it was made from (applied after a guide
//! import it is `stale`); more than 50 channels go to Tuner's own screen instead of being
//! applied from chat; channels needing the PIN now are refused; explicit and hidden channels
//! don't exist here. Claude undoes only its own batches. Guide channels are named by
//! `guide:<guide id>:<xmltv id>` refs, never by an address.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};

use super::params::{GuideFix, GuideFixAction};
use super::proto::{AgentRef, ErrorCode, MAX_TEXT, fmt_time, item_ref};
use super::query::{Fail, Out, Q, channel_ref, clamp, clean, parse_ref, text};
use super::server::{Inner, to_value};
use crate::guide_fix::catalog::Catalog;
use crate::guide_fix::issues::{ClassifyOpts, IssueRow};
use crate::model::{
    ChannelId, ChannelSummary, EpgSourceId, FixActor, FixChange, GuideFixLogId, GuideTarget, IssueKind, NowNext, Proposal, SourceId, Timestamp,
};
use crate::msg::DataScope;
use crate::store::guide_fix::GuideOpts;

/// How long a proposal can be applied.
pub const PROPOSAL_TTL_S: i64 = 600;
/// Proposals kept at once (the oldest goes).
const LIVE_PROPOSALS: usize = 5;
/// Channels Claude applies on a "yes"; more go to Tuner's confirm card.
pub const CHAT_APPLY_MAX: usize = 50;

/// `propose`'s proposals until applied or expired (in the handler).
#[derive(Default)]
pub struct Proposals {
    live: Mutex<Vec<Held>>,
    next: AtomicU64,
}

struct Held {
    id: String,
    expires: Timestamp,
    proposal: Proposal,
    channels: usize,
}

impl Proposals {
    fn put(&self, proposal: Proposal, channels: usize, now: Timestamp) -> (String, Timestamp) {
        let id = format!("gf{}-{}", now % 1_000_000, self.next.fetch_add(1, Ordering::Relaxed));
        let expires = now + PROPOSAL_TTL_S;
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|h| h.expires > now);
        if live.len() >= LIVE_PROPOSALS {
            live.remove(0);
        }
        live.push(Held { id: id.clone(), expires, proposal, channels });
        (id, expires)
    }

    /// The proposal `id` (kept until applied); `Err`: unknown or expired.
    fn get(&self, id: &str, now: Timestamp) -> Out<(Proposal, usize)> {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        match live.iter().find(|h| h.id == id) {
            Some(h) if h.expires > now => Ok((h.proposal.clone(), h.channels)),
            Some(_) => Err(Fail::new(ErrorCode::Unavailable, "expired: that proposal is older than 10 minutes; propose again")),
            None => Err(Fail::not_found("proposal (propose again)")),
        }
    }

    fn take(&self, id: &str) {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).retain(|h| h.id != id);
    }
}

fn kind_name(k: IssueKind) -> &'static str {
    match k {
        IssueKind::NoGuide => "no_guide",
        IssueKind::Suggested => "suggested",
        IssueKind::PlusNoShift => "plus_no_shift",
        IssueKind::CopyHasGuide => "copy_has_guide",
        IssueKind::GuideEnded => "guide_ended",
        IssueKind::GoneFromGuide => "gone_from_guide",
        IssueKind::LooksWrong => "looks_wrong",
    }
}

fn kind_of(s: &str) -> Out<IssueKind> {
    IssueKind::ALL.into_iter().find(|k| kind_name(*k) == s.trim()).ok_or_else(|| Fail::bad(format!("unknown issue kind {s:?}")))
}

/// `guide:<guide id>:<xmltv id>`.
fn guide_ref(t: &GuideTarget) -> String {
    format!("guide:{}:{}", t.epg_source.0, t.xmltv_id)
}

fn parse_guide(cat: &Catalog, s: &str) -> Out<GuideTarget> {
    let bad = || Fail::bad(format!("not a guide ref: {s:?} (pass back a guide ref a tool returned)"));
    let rest = s.trim().strip_prefix("guide:").ok_or_else(bad)?;
    let (src, id) = rest.split_once(':').ok_or_else(bad)?;
    let src = EpgSourceId(src.parse().map_err(|_| bad())?);
    cat.find_id(src, id).ok_or_else(|| Fail::not_found("guide channel"))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuideChannelOut {
    pub guide: String,
    pub name: String,
    /// The guide (source) it is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

fn guide_out(cat: &Catalog, id: crate::model::EpgChannelId, score: Option<f32>) -> Option<GuideChannelOut> {
    let t = cat.target(id)?;
    Some(GuideChannelOut { guide: guide_ref(&t), name: clean(&t.name), guide_name: cat.guide_name(t.epg_source).map(clean), score })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueOut {
    #[serde(rename = "ref")]
    pub item: String,
    pub channel: String,
    pub playlist: String,
    pub kind: &'static str,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide: Option<GuideChannelOut>,
    /// The effective time shift in minutes.
    pub shift_min: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catchup_offset_min: Option<i64>,
    /// Tuner's fix, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<FixOut>,
    /// A fixed phrase ("+1 of E4", "same channel in TV").
    pub why: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shift_min: Option<i32>,
}

fn fix_out(c: &FixChange) -> FixOut {
    FixOut { guide: c.map.as_ref().and_then(|m| m.as_ref()).map(guide_ref), shift_min: c.shift_min.flatten() }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoverageOut {
    pub playlist: String,
    pub shown: u32,
    pub with_guide: u32,
    pub to_fix: u32,
    /// Fixes Tuner is sure of (one key in the app).
    pub one_key: u32,
    pub rules: RulesOut,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RulesOut {
    pub timeshift_siblings: bool,
    pub share_copies: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catchup_offset_min: Option<i64>,
}

/// Everything one guide-fix request reads.
struct Ctx {
    cat: Arc<Catalog>,
    facts: crate::store::guide_fix::GuideFacts,
    opts: GuideOpts,
}

fn ctx(q: &Q) -> Out<Ctx> {
    let opts = GuideOpts::from_config(q.cfg);
    Ok(Ctx { cat: crate::guide_fix::catalog::shared(q.conn)?, facts: crate::store::guide_fix::facts(q.conn, &opts, q.now)?, opts })
}

fn playlist_filter(q: &Q, p: Option<&str>) -> Out<Option<SourceId>> {
    let Some(p) = p else { return Ok(None) };
    let w = text("playlist", p)?.to_lowercase();
    q.lib.sources.iter().find(|s| s.name.to_lowercase().contains(&w)).map(|s| Some(s.id)).ok_or_else(|| Fail::not_found("playlist"))
}

fn issue_out(q: &Q, cx: &Ctx, c: &ChannelSummary, r: &IssueRow) -> IssueOut {
    let extra = cx.facts.extras.get(&c.id);
    IssueOut {
        item: item_ref(&channel_ref(c)),
        channel: clean(&c.name),
        playlist: q.lib.source(c.source_id).map(|m| clean(&m.name)).unwrap_or_default(),
        kind: kind_name(r.kind),
        locked: q.needs_pin(c.locked),
        guide: c.epg_channel_id.and_then(|e| guide_out(&cx.cat, e, None)),
        shift_min: c.tvg_shift_min,
        catchup_offset_min: extra.and_then(|x| x.catchup_offset_s).map(|s| s / 60),
        proposal: r.proposal.as_ref().map(fix_out),
        why: clean(&r.why),
        confidence: (r.confidence * 100.0).round() / 100.0,
    }
}

fn issues(q: &Q, p: &GuideFix) -> Out<Value> {
    let cx = ctx(q)?;
    let report = crate::guide_fix::issues::classify(q.lib, &cx.facts, Some(&cx.cat), ClassifyOpts { include_hidden: p.include_hidden == Some(true) });
    let source = playlist_filter(q, p.playlist.as_deref())?;
    let kinds: Option<HashSet<IssueKind>> = p.kinds.as_ref().map(|k| k.iter().map(|s| kind_of(s)).collect()).transpose()?;
    let coverage: Vec<CoverageOut> = report
        .coverage
        .iter()
        .filter(|c| source.is_none_or(|s| s == c.source))
        .map(|c| {
            let (to_fix, one_key) = report.to_fix(c.source, q.lib);
            let rules = cx.facts.rules.get(&c.source).cloned().unwrap_or_default();
            CoverageOut {
                playlist: q.lib.source(c.source).map(|m| clean(&m.name)).unwrap_or_default(),
                shown: c.shown as u32,
                with_guide: c.with_guide as u32,
                to_fix: to_fix as u32,
                one_key: one_key as u32,
                rules: RulesOut {
                    timeshift_siblings: rules.siblings_on(cx.opts.rules_default),
                    share_copies: rules.share_copies.unwrap_or(cx.opts.rules_default),
                    catchup_offset_min: rules.catchup_offset_s.map(|s| s / 60),
                },
            }
        })
        .collect();
    // one channel: its row (if any) and candidates
    if let Some(s) = p.item.as_deref() {
        let AgentRef::Item(r) = parse_ref(s)? else { return Err(Fail::bad("ref is a channel ref")) };
        let c = q.channel_by_ref(&r).filter(|_| !q.conceals(&r)).ok_or_else(|| Fail::not_found("channel"))?;
        let row = report.rows.iter().find(|x| x.channel == c.id);
        let name = p.query.as_deref().map(|s| text("query", s)).transpose()?.unwrap_or(&c.name);
        let candidates: Vec<GuideChannelOut> = cx
            .cat
            .suggest(name, 5, 0.5, None)
            .into_iter()
            .filter_map(|s| guide_out(&cx.cat, s.epg_channel_id, Some((s.score * 100.0).round() / 100.0)))
            .collect();
        return Ok(json!({
            "coverage": coverage,
            "issue": row.map(|r| issue_out(q, &cx, c, r)),
            "guide": c.epg_channel_id.and_then(|e| guide_out(&cx.cat, e, None)),
            "shift_min": c.tvg_shift_min,
            "candidates": candidates,
            "scan": if cx.facts.fresh() { "fresh" } else { "stale" },
        }));
    }
    let limit = clamp(p.limit, 50, 200);
    let offset: usize = p.cursor.as_deref().map(|c| c.parse().map_err(|_| Fail::bad("cursor is from a previous answer"))).transpose()?.unwrap_or(0);
    let rows: Vec<(&ChannelSummary, &IssueRow)> = report
        .rows
        .iter()
        .filter(|r| kinds.as_ref().is_none_or(|k| k.contains(&r.kind)))
        .filter_map(|r| q.channel(r.channel).filter(|c| !q.conceals(&channel_ref(c))).map(|c| (c, r)))
        .filter(|(c, _)| source.is_none_or(|s| s == c.source_id))
        .collect();
    let total = rows.len();
    let issues: Vec<IssueOut> = rows.iter().skip(offset).take(limit).map(|(c, r)| issue_out(q, &cx, c, r)).collect();
    let next = (offset + limit < total).then(|| (offset + limit).to_string());
    Ok(json!({
        "coverage": coverage,
        "issues": issues,
        "total": total,
        "cursor": next,
        "scan": if cx.facts.fresh() { "fresh" } else { "stale" },
    }))
}

/// A proposal, its preview and what was refused.
struct Proposed {
    proposal: Proposal,
    channels: Vec<ChannelId>,
    rejected: Vec<Value>,
}

fn propose(q: &Q, p: &GuideFix) -> Out<Proposed> {
    let cx = ctx(q)?;
    let mut changes: Vec<FixChange> = Vec::new();
    let mut channels: Vec<ChannelId> = Vec::new();
    let mut rejected: Vec<Value> = Vec::new();
    let mut take = |c: &ChannelSummary, fix: FixChange, rejected: &mut Vec<Value>| {
        if channels.contains(&c.id) {
            return;
        }
        if q.needs_pin(c.locked) {
            rejected.push(json!({ "ref": item_ref(&channel_ref(c)), "reason": "locked: needs your PIN in Tuner" }));
            return;
        }
        channels.push(c.id);
        changes.push(FixChange { channel: Some(channel_ref(c)), ..fix });
    };
    // explicit changes
    if let Some(list) = &p.changes {
        let list = list.as_array().ok_or_else(|| Fail::bad("changes is a list"))?;
        if list.len() > crate::store::guide_fix::MAX_BATCH {
            return Err(Fail::bad(format!("at most {} changes", crate::store::guide_fix::MAX_BATCH)));
        }
        for x in list {
            let o = x.as_object().ok_or_else(|| Fail::bad("a change is {ref, guide?, shift_min?, catchup_offset_min?, reset?, dismiss?}"))?;
            if let Some(k) = o.keys().find(|k| !["ref", "guide", "shift_min", "catchup_offset_min", "reset", "dismiss"].contains(&k.as_str())) {
                return Err(Fail::bad(format!("unknown change field {k:?}")));
            }
            let r = o.get("ref").and_then(Value::as_str).ok_or_else(|| Fail::bad("a change needs ref"))?;
            let item = match parse_ref(r)? {
                AgentRef::Item(i) => i,
                AgentRef::Programme { channel, .. } => channel,
            };
            let Some(c) = q.channel_by_ref(&item).filter(|_| !q.conceals(&item)) else {
                rejected.push(json!({ "ref": r, "reason": "not_found" }));
                continue;
            };
            let reset = o.get("reset").and_then(Value::as_bool) == Some(true);
            let mut fix = FixChange::default();
            if let Some(g) = o.get("guide").and_then(Value::as_str) {
                fix.map = Some(Some(parse_guide(&cx.cat, text("guide", g)?)?));
            } else if reset {
                fix.map = Some(None);
            }
            match o.get("shift_min").and_then(Value::as_i64) {
                Some(m) if !(-24 * 60..=24 * 60).contains(&m) => return Err(Fail::bad("shift_min is −1440 to 1440")),
                Some(m) => fix.shift_min = Some(Some(m as i32)),
                None if reset => fix.shift_min = Some(None),
                None => {}
            }
            match o.get("catchup_offset_min").and_then(Value::as_i64) {
                Some(m) if !(-120..=120).contains(&m) => return Err(Fail::bad("catchup_offset_min is −120 to 120")),
                Some(m) => fix.catchup_offset_s = Some(Some(m * 60)),
                None if reset => fix.catchup_offset_s = Some(None),
                None => {}
            }
            if let Some(d) = o.get("dismiss").and_then(Value::as_str) {
                fix.dismiss = Some(kind_of(d)?);
            }
            if fix == FixChange::default() {
                return Err(Fail::bad("a change needs guide, shift_min, catchup_offset_min, reset or dismiss"));
            }
            take(c, fix, &mut rejected);
        }
    }
    // Tuner's own fixes, by kind and confidence
    if let Some(a) = &p.accept {
        let kinds: Vec<IssueKind> = a
            .get("kinds")
            .and_then(Value::as_array)
            .ok_or_else(|| Fail::bad("accept needs kinds"))?
            .iter()
            .map(|k| k.as_str().ok_or_else(|| Fail::bad("kinds are names")).and_then(kind_of))
            .collect::<Out<_>>()?;
        let min = a.get("min_confidence").and_then(Value::as_f64).unwrap_or(0.95) as f32;
        let source = playlist_filter(q, a.get("playlist").and_then(Value::as_str).filter(|s| s.len() <= MAX_TEXT))?;
        let report = crate::guide_fix::issues::classify(q.lib, &cx.facts, Some(&cx.cat), ClassifyOpts::default());
        for r in report.rows.iter().filter(|r| kinds.contains(&r.kind) && r.confidence >= min) {
            let (Some(fix), Some(c)) = (&r.proposal, q.channel(r.channel)) else { continue };
            if source.is_some_and(|s| s != c.source_id) || q.conceals(&channel_ref(c)) {
                continue;
            }
            take(c, fix.clone(), &mut rejected);
        }
    }
    let mut proposal = Proposal { changes, ..Default::default() };
    if proposal.changes.len() > crate::store::guide_fix::MAX_BATCH {
        return Err(Fail::bad(format!("more than {} channels: narrow it (playlist, kinds, min_confidence)", crate::store::guide_fix::MAX_BATCH)));
    }
    // per-playlist rules
    if let Some(rules) = &p.rules {
        for r in rules.as_array().ok_or_else(|| Fail::bad("rules is a list"))? {
            let source = playlist_filter(q, r.get("playlist").and_then(Value::as_str))?.ok_or_else(|| Fail::bad("a rule needs playlist"))?;
            let mut now = cx.facts.rules.get(&source).cloned().unwrap_or_default();
            if let Some(b) = r.get("timeshift_siblings").and_then(Value::as_bool) {
                now.timeshift_siblings = Some(b);
            }
            if let Some(b) = r.get("share_copies").and_then(Value::as_bool) {
                now.share_copies = Some(b);
            }
            if let Some(m) = r.get("catchup_offset_min").and_then(Value::as_i64) {
                now.catchup_offset_s = Some(m.clamp(-120, 120) * 60);
            }
            proposal.rules.push((source, now));
        }
    }
    if proposal.changes.is_empty() && proposal.rules.is_empty() {
        return Err(Fail::new(ErrorCode::Unavailable, "nothing to change (everything asked for was refused or already right)"));
    }
    proposal.stamp = crate::store::guide_fix::stamp(q.conn, &cx.opts)?;
    Ok(Proposed { proposal, channels, rejected })
}

fn slim(nn: &NowNext) -> Value {
    let one = |p: &Option<crate::model::ProgrammeSlim>| p.as_ref().map(|p| json!({ "title": clean(&p.title), "start": fmt_time(p.start) }));
    json!({ "now": one(&nn.now), "next": one(&nn.next) })
}

fn proposal_out(q: &Q, pr: &Proposed, id: &str, expires: Timestamp) -> Out<Value> {
    let preview = crate::guide_fix::preview::preview(q.conn, &pr.proposal.changes, q.now)?;
    let changes: Vec<Value> = pr
        .proposal
        .changes
        .iter()
        .zip(&preview)
        .take(CHAT_APPLY_MAX)
        .zip(&pr.channels)
        .map(|((c, row), id)| {
            let name = q.channel(*id).map(|c| clean(&c.name)).unwrap_or_default();
            json!({
                "ref": c.channel.as_ref().map(item_ref),
                "channel": name,
                "fix": fix_out(c),
                "before": slim(&row.before),
                "after": slim(&row.after),
                "warnings": row.warnings.iter().map(|w| clean(w)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let summary = json!({
        "channels": pr.proposal.changes.len(),
        "maps": pr.proposal.changes.iter().filter(|c| c.map.is_some()).count(),
        "shifts": pr.proposal.changes.iter().filter(|c| c.shift_min.is_some()).count(),
        "offsets": pr.proposal.changes.iter().filter(|c| c.catchup_offset_s.is_some()).count(),
        "dismissals": pr.proposal.changes.iter().filter(|c| c.dismiss.is_some()).count(),
        "rules": pr.proposal.rules.len(),
    });
    Ok(json!({
        "proposal_id": id,
        "expires_at": fmt_time(expires),
        "summary": summary,
        "changes": changes,
        "rejected": pr.rejected,
        "applies_in_chat": pr.proposal.changes.len() <= CHAT_APPLY_MAX,
    }))
}

pub(super) async fn handle(h: &Arc<Inner>, p: GuideFix) -> Out<Value> {
    match p.action {
        GuideFixAction::Issues => h.run(true, move |q| issues(q, &p)).await,
        GuideFixAction::Propose => {
            let me = h.clone();
            h.run(true, move |q| {
                let pr = propose(q, &p)?;
                let (id, expires) = me.proposals.put(pr.proposal.clone(), pr.proposal.changes.len(), q.now);
                proposal_out(q, &pr, &id, expires)
            })
            .await
        }
        GuideFixAction::Apply => {
            if p.confirm != Some(true) {
                return Err(Fail::bad("apply needs confirm=true, after the user said yes to the proposal"));
            }
            let id = p.proposal_id.clone().ok_or_else(|| Fail::bad("apply needs proposal_id"))?;
            let now = crate::model::now();
            let (proposal, channels) = h.proposals.get(&id, now)?;
            if channels > CHAT_APPLY_MAX {
                return to_value(&json!({
                    "pending_review": true,
                    "message": format!("{channels} channels is more than Claude applies from chat: open Guide fixes in Tuner to review and apply them there."),
                }));
            }
            let opts = GuideOpts::from_config(&h.cfg());
            let label = format!("Claude: {channels} channel{}", if channels == 1 { "" } else { "s" });
            let report = h
                .write(DataScope::Epg, move |c| match crate::store::guide_fix::apply(c, FixActor::Claude, &label, &proposal, &opts, now) {
                    Ok(r) => Ok(Ok(r)),
                    Err(crate::error::Error::Other(m)) if m == crate::store::guide_fix::STALE => Ok(Err(m)),
                    Err(e) => Err(e),
                })
                .await?
                .map_err(|m| Fail::new(ErrorCode::Unavailable, format!("{m}; propose again")))?;
            h.proposals.take(&id);
            if report.mappings_changed {
                h.jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Mapping });
            }
            if report.reminders_moved > 0 {
                h.shared.wake_reminders();
            }
            tracing::info!("agent: guide_fix apply → {} channels", report.applied);
            to_value(&json!({
                "applied": report.applied,
                "skipped": report.skipped.iter().map(|(k, why)| json!({ "channel": k, "reason": clean(why) })).collect::<Vec<_>>(),
                "undo_id": report.log.map(|l| l.0),
                "reminders_moved": report.reminders_moved,
            }))
        }
        GuideFixAction::Undo => {
            let id = GuideFixLogId(p.undo_id.ok_or_else(|| Fail::bad("undo needs undo_id"))?);
            let entry = h
                .run(false, move |q| {
                    Ok(crate::store::guide_fix::recent(q.conn, crate::store::guide_fix::LOG_KEEP_N)?.into_iter().find(|e| e.id == id))
                })
                .await?
                .ok_or_else(|| Fail::not_found("guide fix batch"))?;
            if entry.actor != FixActor::Claude {
                return Err(Fail::new(
                    ErrorCode::Unavailable,
                    "Claude undoes only its own batches; the user can undo theirs in Tuner's Guide fixes (Ctrl+Z)",
                ));
            }
            if entry.undone_at.is_some() {
                return Err(Fail::new(ErrorCode::Unavailable, "that batch was undone already"));
            }
            let opts = GuideOpts::from_config(&h.cfg());
            let now = crate::model::now();
            let report = h.write(DataScope::Epg, move |c| crate::store::guide_fix::undo(c, id, &opts, now)).await?;
            h.jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Mapping });
            if report.reminders_moved > 0 {
                h.shared.wake_reminders();
            }
            tracing::info!("agent: guide_fix undo {} → {} reverted", id.0, report.reverted);
            to_value(&json!({
                "reverted": report.reverted,
                "skipped": report.skipped.iter().map(|(k, why)| json!({ "channel": k, "reason": clean(why) })).collect::<Vec<_>>(),
                "reminders_moved": report.reminders_moved,
            }))
        }
    }
}
