//! The "In the guide now" preview of the rule editor and the Follow sheet (DESIGN §33 §5.2): a
//! blocking-pool task with a cancellation token, 350 ms debounce, newest request wins; answers
//! with `AppEvent::Follow(FollowEvent::PreviewReady)`. Not a job (typing must not clutter the
//! jobs overlay). [`preview`] is the same run for MCP `follow preview`. OWNER: F-AB (F-C starts
//! the service lazily from `follow/ui.rs`).

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::follow::plan::{self, LedgerRow, MatchState, Pool, Preview};
use crate::follow::{FollowEvent, FollowRule};
use crate::model::Timestamp;
use crate::store::follow::{EpgSet, scan};
use crate::store::library::Library;

/// How long typing settles before a preview runs.
pub const DEBOUNCE: Duration = Duration::from_millis(350);

/// What a preview needs besides the rule.
#[derive(Debug, Clone)]
pub struct PreviewOpts {
    pub now: Timestamp,
    /// Look this many days ahead (1–14).
    pub days: u32,
    /// At most this many matches and as many left-outs.
    pub limit: usize,
    /// A PIN is set (a switch onto a locked channel becomes a notification).
    pub pin_active: bool,
    pub max_pending: usize,
}

/// Preview `rule` (saved or not) against the guide: what it would remind about, list or switch
/// to over the next `days`, what it leaves out and why, and warnings. Explicit channels are
/// never looked at (`plan::Pool`), so nothing explicit is matched, counted or shown.
pub fn preview(
    conn: &Connection,
    lib: &Library,
    rule: &FollowRule,
    opts: &PreviewOpts,
    tz: &jiff::tz::TimeZone,
    cancelled: &dyn Fn() -> bool,
) -> Result<Preview> {
    let trouble: HashSet<_> = crate::store::follow::trouble(conn, opts.now - 86_400)?.iter().filter_map(|r| lib.live.by_ref(r)).collect();
    let pool = Pool::build(lib, trouble, opts.pin_active);
    let mut rule = rule.clone();
    rule.enabled = true;
    let rules = [rule];
    let matcher = crate::follow::matcher::Matcher::new(&rules);
    let mut hits = Vec::new();
    let until = opts.now + i64::from(opts.days.clamp(1, 14)) * 86_400;
    scan(conn, &EpgSet::new(pool.epg_channels()), &matcher, (opts.now, Some(until)), true, cancelled, &mut hits)?;
    let warnings = sport_warning(&rules[0], &hits).into_iter().collect();
    // the rule's own ledger: aired rows for "new only" / first airings, skipped ones stay skipped
    let ledger: Vec<LedgerRow> = if rules[0].id.0 == 0 {
        Vec::new()
    } else {
        crate::store::follow::ledger(conn, &|_| false)?.into_iter().filter(|r| r.rule == rules[0].id).collect()
    };
    let aired: Vec<LedgerRow> = ledger.iter().filter(|r| r.stop <= opts.now).cloned().collect();
    let manual = crate::store::follow::manual_switches(conn, opts.now)?;
    let ctx =
        plan::PlanCtx { rules: &rules, pool: &pool, ledger: &aired, manual_switches: &manual, max_pending: opts.max_pending, now: opts.now, tz };
    let built = plan::build(&mut hits, &ctx);
    let mut matches: Vec<_> = built.steps.into_iter().map(|s| s.to).collect();
    for m in &mut matches {
        if ledger.iter().any(|r| r.state == MatchState::Skipped && r.event_key == m.event_key && r.start == m.start) {
            m.state = MatchState::Skipped;
        }
    }
    matches.truncate(opts.limit);
    let mut left_out = built.left_out;
    left_out.truncate(opts.limit);
    Ok(Preview { matches, left_out, warnings })
}

/// "'England' matches cricket, football and rugby": the matches span several sports.
fn sport_warning(rule: &FollowRule, hits: &[plan::Hit]) -> Option<String> {
    const SPORTS: [&str; 12] =
        ["football", "rugby", "cricket", "tennis", "golf", "basketball", "darts", "snooker", "boxing", "netball", "hockey", "motorsport"];
    let mut found = BTreeSet::new();
    let mut buf = String::new();
    for h in hits.iter().filter(|h| h.verdict.is_ok()) {
        let Some(c) = &h.categories else { continue };
        buf.clear();
        for part in c.split('\x1f') {
            crate::textmatch::fold::fold_append(part, &mut buf);
        }
        for s in SPORTS {
            if crate::textmatch::fold::has_token(&buf, s) {
                found.insert(s);
            }
        }
    }
    (found.len() > 1).then(|| {
        let list: Vec<&str> = found.into_iter().collect();
        let (last, rest) = list.split_last().expect("two or more");
        format!("'{}' matches {} and {}", rule.name, rest.join(", "), last)
    })
}

/// One preview request: (editor revision, the rule as drafted, days ahead).
type Request = (u64, FollowRule, u32);

/// The preview service (cheap to clone).
#[derive(Clone)]
pub struct PreviewService {
    tx: tokio::sync::mpsc::UnboundedSender<Request>,
}

impl PreviewService {
    /// Start it on `rt`: requests settle for [`DEBOUNCE`], the newest wins and cancels a run in
    /// progress; the answer comes as `AppEvent::Follow(PreviewReady)`. The library is the
    /// app's published snapshot (`AppShared::library`).
    pub fn start(
        rt: &tokio::runtime::Handle,
        store: crate::store::Store,
        shared: Arc<crate::shared::AppShared>,
        bus: crate::msg::EventBus,
        config: crate::config::SharedConfig,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Request>();
        rt.spawn(async move {
            let mut running: Option<CancellationToken> = None;
            while let Some(mut req) = rx.recv().await {
                // debounce: the newest request after a quiet spell
                loop {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(newer)) => req = newer,
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                if let Some(t) = running.take() {
                    t.cancel();
                }
                let token = CancellationToken::new();
                running = Some(token.clone());
                let (store, shared, bus) = (store.clone(), shared.clone(), bus.clone());
                let cfg = crate::config::current(&config);
                tokio::task::spawn_blocking(move || {
                    let (rev, rule, days) = req;
                    let lib = shared.library();
                    if lib.built_at == 0 {
                        return; // no library yet: the editor shows "counting…" until the next edit
                    }
                    let opts = PreviewOpts {
                        now: crate::model::now(),
                        days,
                        limit: 50,
                        pin_active: cfg.parental.pin_active(),
                        max_pending: cfg.follow.max_pending as usize,
                    };
                    let tz = jiff::tz::TimeZone::system();
                    let result = store.reader().and_then(|c| preview(&c, &lib, &rule, &opts, &tz, &|| token.is_cancelled()));
                    drop(lib);
                    match result {
                        Ok(preview) if !token.is_cancelled() => bus.send(crate::msg::AppEvent::Follow(FollowEvent::PreviewReady { rev, preview })),
                        Ok(_) | Err(crate::error::Error::Cancelled) => {}
                        Err(e) => tracing::debug!(target: "tuner::follow", "preview failed: {e}"),
                    }
                });
            }
        });
        Self { tx }
    }

    /// Preview `rule` over the next `days` days (`rev`: the editor's revision; older answers
    /// are dropped by the UI).
    pub fn request(&self, rev: u64, rule: FollowRule, days: u32) {
        let _ = self.tx.send((rev, rule, days));
    }
}
