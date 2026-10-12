//! `JobKind::EvaluateFollows`: evaluate every rule against the guide, write the plan, announce
//! (DESIGN §33 §5.2–5.4, B7). OWNER: F-AB. Runs in the "features" job lane, never together
//! with `GuideCheck`.
//!
//! Read side ([`prepare`], blocking pool, the job's own reader): the followable channels from a
//! library snapshot, one scan of the guide through the rules' automaton, the ledger → a
//! [`Plan`]. Write side ([`commit`], the writer thread, one transaction): ledger rows and the
//! rules' reminders. Then `DataScope::Reminders` + `DataScope::Follow`, one toast per moved
//! match and at most one summary every 12 h.
//!
//! Privacy: INFO logs carry counts and timing only ("follow: 4 rules, 0.71 s, +2 ~1 −0"); no
//! terms, titles or channels.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress, notify};
use crate::config::FollowConfig;
use crate::error::Result;
use crate::follow::plan::{self, LedgerRow, Plan, Pool};
use crate::follow::{FollowReason, FollowRule};
use crate::model::Timestamp;
use crate::msg::DataScope;
use crate::store::follow::{self as store_follow, EpgSet, ScanStats};
use crate::store::library::Library;

/// `meta` keys.
const META_INPUTS: &str = "follow:inputs";
const META_LAST_RUN: &str = "follow:last_run";
const META_SUMMARY_AT: &str = "follow:summary_at";
/// An unchanged guide and unchanged rules are not re-evaluated more often than this.
const RERUN_S: i64 = 6 * 3600;
/// At most one summary this often.
const SUMMARY_EVERY_S: i64 = 12 * 3600;
/// Move toasts per evaluation (more: "and N more moved").
const MOVE_TOASTS: usize = 3;

/// What an evaluation needs besides the database.
#[derive(Debug, Clone)]
pub struct Opts {
    pub now: Timestamp,
    pub reason: FollowReason,
    pub cfg: FollowConfig,
    /// A PIN is set (`ParentalConfig::pin_active`).
    pub pin_active: bool,
    /// Local time (days, hours, ISO weeks, toast times).
    pub tz: jiff::tz::TimeZone,
}

/// The read side's answer.
#[derive(Debug)]
pub struct Evaluation {
    pub plan: Plan,
    /// The evaluated (enabled) rules.
    pub rules: Vec<FollowRule>,
    pub stats: ScanStats,
    inputs: String,
}

/// Evaluate (a no-op until `follow::ENABLED`, or with `follow.enabled` off).
pub async fn evaluate(ctx: &JobContext, reason: FollowReason, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    if !crate::follow::ENABLED {
        return Ok(());
    }
    let cfg = crate::config::current(&ctx.config);
    if !cfg.follow.enabled {
        return Ok(());
    }
    progress.set(super::JobPhase::Indexing);
    let started = Instant::now();
    let opts =
        Opts { now: crate::model::now(), reason, cfg: cfg.follow.clone(), pin_active: cfg.parental.pin_active(), tz: jiff::tz::TimeZone::system() };
    let (o, token) = (opts.clone(), cancel.clone());
    // The app's published snapshot; headless (`--refresh`) or before the first, build one that
    // never shows explicit content. The pool drops explicit channels either way.
    let published = ctx.published_library();
    let ev = super::read(ctx, move |c| {
        let lib = match published {
            Some(lib) => lib,
            None => Arc::new(crate::store::library::load_visible(c, crate::store::library::Visibility::NO_EXPLICIT)?),
        };
        prepare(c, &lib, &o, &|| token.is_cancelled())
    })
    .await?;
    let Some(ev) = ev else {
        tracing::debug!(target: "tuner::follow", "follow: guide and rules unchanged, skipped");
        return Ok(());
    };
    super::check(cancel)?;
    let o = opts.clone();
    let (rules_n, stats) = (ev.rules.len(), ev.stats);
    let out = super::write(ctx, move |c| commit(c, &ev, &o)).await?;
    notify(ctx, DataScope::Reminders);
    notify(ctx, DataScope::Follow);
    tracing::info!(
        target: "tuner::follow",
        "follow: {rules_n} rules, {} rows, {} hits, {:.2} s, +{} ~{} −{}",
        stats.rows,
        stats.hits,
        started.elapsed().as_secs_f64(),
        out.applied.reminders_set,
        out.applied.moved,
        out.applied.gone
    );
    for t in &out.moves {
        ctx.bus.toast(crate::msg::Toast::info(t.clone()));
    }
    announce(ctx, &opts).await
}

/// The read side: `None` when the guide and the rules are unchanged since a run less than 6 h
/// ago (and the reason is a guide import or the daily tick).
pub fn prepare(conn: &Connection, lib: &Library, opts: &Opts, cancelled: &dyn Fn() -> bool) -> Result<Option<Evaluation>> {
    let inputs = store_follow::inputs_hash(conn)?;
    if matches!(opts.reason, FollowReason::Guide | FollowReason::Daily) {
        let same = store_follow::meta_get(conn, META_INPUTS)?.as_deref() == Some(inputs.as_str());
        let recent = store_follow::meta_get(conn, META_LAST_RUN)?.and_then(|v| v.parse::<i64>().ok()).is_some_and(|t| opts.now - t < RERUN_S);
        if same && recent {
            return Ok(None);
        }
    }
    let rules: Vec<FollowRule> = store_follow::rules(conn)?.into_iter().filter(|r| r.enabled && r.spec.problem().is_none()).collect();
    let trouble: HashSet<_> = store_follow::trouble(conn, opts.now - 86_400)?.iter().filter_map(|r| lib.live.by_ref(r)).collect();
    let pool = Pool::build(lib, trouble, opts.pin_active);
    let matcher = crate::follow::matcher::Matcher::new(&rules);
    let mut hits = Vec::new();
    let stats = store_follow::scan(conn, &EpgSet::new(pool.epg_channels()), &matcher, (opts.now, None), false, cancelled, &mut hits)?;
    let coverage = store_follow::coverage(conn)?;
    let covered = |r: &LedgerRow| {
        r.channel
            .as_ref()
            .and_then(|c| lib.live.by_ref(c))
            .and_then(|id| lib.live.get(id))
            .and_then(|c| c.epg_channel_id)
            .and_then(|e| coverage.get(&e.0))
            .is_some_and(|max_stop| {
                let shift = r.channel.as_ref().and_then(|c| pool.channel(c)).map_or(0, |c| c.shift_s);
                r.start - shift < *max_stop
            })
    };
    let ledger = store_follow::ledger(conn, &covered)?;
    let manual = store_follow::manual_switches(conn, opts.now)?;
    let ctx = plan::PlanCtx {
        rules: &rules,
        pool: &pool,
        ledger: &ledger,
        manual_switches: &manual,
        max_pending: opts.cfg.max_pending as usize,
        now: opts.now,
        tz: &opts.tz,
    };
    let plan = plan::build(&mut hits, &ctx);
    Ok(Some(Evaluation { plan, rules, stats, inputs }))
}

/// What [`commit`] did.
#[derive(Debug, Clone, Default)]
pub struct Committed {
    pub applied: store_follow::Applied,
    /// Toast texts for moved matches ("Arsenal moved to Sat 17:30"), spoiler-safe.
    pub moves: Vec<String>,
}

/// The write side (one transaction): the plan, then the early-exit inputs.
pub fn commit(conn: &mut Connection, ev: &Evaluation, opts: &Opts) -> Result<Committed> {
    let tx = conn.transaction()?;
    let applied = store_follow::apply_plan(&tx, &ev.plan, &ev.rules, opts.now)?;
    store_follow::meta_set(&tx, META_INPUTS, &ev.inputs)?;
    store_follow::meta_set(&tx, META_LAST_RUN, &opts.now.to_string())?;
    tx.commit()?;
    let tz = &opts.tz;
    let mut moves: Vec<String> = ev
        .plan
        .steps
        .iter()
        .filter(|s| s.moved_from.is_some())
        .filter_map(|s| {
            let rule = ev.rules.iter().find(|r| r.id == s.to.rule)?;
            Some(format!("{} moved to {}", rule.safe_title(&s.to.title), when(s.to.start, tz)))
        })
        .collect();
    if moves.len() > MOVE_TOASTS {
        let more = moves.len() - (MOVE_TOASTS - 1);
        moves.truncate(MOVE_TOASTS - 1);
        moves.push(format!("Following: {more} more matches moved"));
    }
    Ok(Committed { applied, moves })
}

/// "Sat 17:30" in local time.
fn when(t: Timestamp, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_second(t).map(|ts| ts.to_zoned(tz.clone()).strftime("%a %H:%M").to_string()).unwrap_or_default()
}

/// The summary text of new matches: ("Following: 3 new", up to two lines and "+1 more").
pub fn summary_text(rows: &[store_follow::Unannounced], tz: &jiff::tz::TimeZone) -> (String, Vec<String>) {
    let title = format!("Following: {} new", rows.len());
    let mut lines: Vec<String> = rows
        .iter()
        .take(2)
        .map(|r| {
            let at = when(r.start, tz);
            if r.channel.is_empty() { format!("{} · {at}", r.title) } else { format!("{} · {at} · {}", r.title, r.channel) }
        })
        .collect();
    if rows.len() > 2 {
        lines.push(format!("+{} more", rows.len() - 2));
    }
    (title, lines)
}

/// At most one summary every 12 h, only with additions: a desktop notification, or an in-app
/// toast (`FollowEvent::Summary`) when no notification server answers. Rows are marked
/// announced either way (with the summary off too, so they never pile up).
async fn announce(ctx: &JobContext, opts: &Opts) -> Result<()> {
    let (now, summary_on) = (opts.now, opts.cfg.summary);
    let rows = super::read(ctx, move |c| {
        let last = store_follow::meta_get(c, META_SUMMARY_AT)?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        if summary_on && now - last < SUMMARY_EVERY_S {
            return Ok(Vec::new());
        }
        store_follow::unannounced(c, now)
    })
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    if summary_on {
        let (title, lines) = summary_text(&rows, &opts.tz);
        let body = lines.join("\n");
        let (t, b) = (title.clone(), body.clone());
        let shown = super::blocking(move || Ok(crate::platform::notify::notify(&t, &b, false))).await.unwrap_or(false);
        if !shown {
            ctx.bus.send(crate::msg::AppEvent::Follow(crate::follow::FollowEvent::Summary { title, lines }));
        }
    }
    super::write(ctx, move |c| {
        store_follow::mark_announced(c, &ids)?;
        if summary_on {
            store_follow::meta_set(c, META_SUMMARY_AT, &now.to_string())?;
        }
        Ok(())
    })
    .await
}
