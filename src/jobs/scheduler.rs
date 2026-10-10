//! Refresh scheduling. Owner: WP-06. DESIGN §9.5.
//!
//! * On launch: if any source/EPG is older than `general.refresh_on_launch_after_h` → submit
//!   `RefreshStale` 5 s after start (so first paint isn't competing for the disk).
//! * While running: tick every 15 min → `RefreshStale` (honours per-source `refresh_h`, config
//!   intervals, and "guide runs out in < 12 h"). Ticks are skipped while a stream is in the
//!   Loading/Zapping state (`JobContext::player_busy`; don't compete with zapping).
//! * Daily: `Maintenance`.
//! * No background refresh while the app is closed in v1 (documented; a systemd user timer
//!   calling `tuner --refresh` is a v2 option).
//!
//! Details fixed by the implementation:
//! * The launch check submits `RefreshSource` / `RefreshEpg` for exactly the sources older than
//!   `refresh_on_launch_after_h` (0 = off) — `RefreshStale` would apply the longer running
//!   intervals instead. While the player is busy the launch check waits for a quiet moment.
//! * `playlist_refresh_h` / `epg_refresh_h` = 0 mean "manual only": `RefreshStale` then only
//!   picks sources never imported (and guides about to run out) unless a source sets its own
//!   `refresh_h`.
//! * Maintenance runs when the last successful run (kept in the DB) is ≥ 24 h old, at the
//!   earliest 10 min after launch and never while the player is busy.
//! * The decisions are a pure state machine ([`Plan`]) driven by a 5 s timer, so they are unit
//!   tested without waiting.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{JobContext, JobKind, JobManager, read};
use crate::error::Result;
use crate::model::Timestamp;

/// Delay of the launch check.
const LAUNCH_DELAY_S: i64 = 5;
/// Interval of `RefreshStale` ticks.
const TICK_S: i64 = 15 * 60;
/// Maintenance cadence and its earliest start after launch.
const MAINTENANCE_EVERY_S: i64 = 86_400;
const MAINTENANCE_DELAY_S: i64 = 10 * 60;
/// Timer resolution of the scheduler loop.
const POLL: Duration = Duration::from_secs(5);

/// What the scheduler should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    LaunchRefresh,
    RefreshStale,
    Maintenance,
}

/// Scheduling state (pure; times in unix seconds).
#[derive(Debug, Clone)]
struct Plan {
    started: Timestamp,
    launch_done: bool,
    next_tick: Timestamp,
    last_maintenance: Option<Timestamp>,
}

impl Plan {
    fn new(started: Timestamp, last_maintenance: Option<Timestamp>) -> Self {
        Self { started, launch_done: false, next_tick: started + TICK_S, last_maintenance }
    }

    /// Everything due at `now`; `busy` = the player is Loading/Zapping.
    fn poll(&mut self, now: Timestamp, busy: bool) -> Vec<Due> {
        let mut out = Vec::new();
        if !self.launch_done && now >= self.started + LAUNCH_DELAY_S && !busy {
            self.launch_done = true;
            out.push(Due::LaunchRefresh);
        }
        if now >= self.next_tick {
            // a busy tick is skipped, not postponed (the next one is 15 min away)
            self.next_tick = now + TICK_S;
            if !busy {
                out.push(Due::RefreshStale);
            }
        }
        let maintenance_due = now >= self.started + MAINTENANCE_DELAY_S
            && self.last_maintenance.is_none_or(|t| now - t >= MAINTENANCE_EVERY_S);
        if maintenance_due && !busy {
            self.last_maintenance = Some(now);
            out.push(Due::Maintenance);
        }
        out
    }
}

/// Spawn the scheduler task on the manager's runtime. Returns its abort handle.
pub fn spawn(manager: JobManager) -> tokio::task::AbortHandle {
    let rt = manager.runtime().clone();
    rt.spawn(async move {
        let ctx = manager.context().clone();
        let last = read(&ctx, super::db::last_maintenance).await.unwrap_or_else(|e| {
            tracing::warn!("scheduler: reading the last maintenance time: {e}");
            None
        });
        let mut plan = Plan::new(crate::model::now(), last);
        let mut timer = tokio::time::interval(POLL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            timer.tick().await;
            let busy = ctx.player_busy.load(Ordering::Relaxed);
            for due in plan.poll(crate::model::now(), busy) {
                match due {
                    Due::LaunchRefresh => {
                        if let Err(e) = launch_refresh(&ctx).await {
                            tracing::warn!(target: "tuner::jobs", "Couldn't check for updates at launch: {e}");
                        }
                    }
                    Due::RefreshStale => {
                        manager.submit(JobKind::RefreshStale);
                    }
                    Due::Maintenance => {
                        manager.submit(JobKind::Maintenance);
                    }
                }
            }
        }
    })
    .abort_handle()
}

/// Sources and guides older than `refresh_on_launch_after_h`.
async fn launch_refresh(ctx: &JobContext) -> Result<()> {
    let h = crate::config::current(&ctx.config).general.refresh_on_launch_after_h;
    if h == 0 {
        return Ok(());
    }
    submit_stale(ctx, h, h).await
}

/// `RefreshStale` job body: every stale enabled playlist and guide (DESIGN §9.5).
pub(super) async fn refresh_stale(ctx: &JobContext) -> Result<()> {
    let g = crate::config::current(&ctx.config).general.clone();
    let manual = |h: u32| if h == 0 { u32::MAX } else { h };
    submit_stale(ctx, manual(g.playlist_refresh_h), manual(g.epg_refresh_h)).await
}

async fn submit_stale(ctx: &JobContext, playlist_h: u32, epg_h: u32) -> Result<()> {
    let now = crate::model::now();
    let (sources, guides) = read(ctx, move |c| {
        Ok((crate::store::sources::stale_sources(c, now, playlist_h)?, crate::store::sources::stale_epg_sources(c, now, epg_h)?))
    })
    .await?;
    if !sources.is_empty() || !guides.is_empty() {
        tracing::info!("refreshing {} playlist(s) and {} guide(s)", sources.len(), guides.len());
    }
    for source in sources {
        (ctx.submit)(JobKind::RefreshSource { source, force: false });
    }
    for epg_source in guides {
        (ctx.submit)(JobKind::RefreshEpg { epg_source, force: false });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Timestamp = 1_791_576_000;

    #[test]
    fn launch_check_runs_once_after_the_delay_and_waits_for_a_quiet_player() {
        let mut p = Plan::new(T0, Some(T0));
        assert!(p.poll(T0 + 1, false).is_empty());
        assert!(p.poll(T0 + LAUNCH_DELAY_S, true).is_empty(), "busy: launch refresh waits");
        assert_eq!(p.poll(T0 + LAUNCH_DELAY_S + 5, false), [Due::LaunchRefresh]);
        assert!(p.poll(T0 + LAUNCH_DELAY_S + 10, false).is_empty());
    }

    #[test]
    fn ticks_every_fifteen_minutes_and_skips_busy_ticks() {
        let mut p = Plan::new(T0, Some(T0));
        p.poll(T0 + LAUNCH_DELAY_S, false);
        assert!(p.poll(T0 + TICK_S - 1, false).is_empty());
        assert_eq!(p.poll(T0 + TICK_S, false), [Due::RefreshStale]);
        assert!(p.poll(T0 + TICK_S + 5, false).is_empty());
        // the next tick falls while zapping: skipped, not delivered when the player settles
        assert!(p.poll(T0 + 2 * TICK_S, true).is_empty());
        assert!(p.poll(T0 + 2 * TICK_S + 5, false).is_empty());
        assert_eq!(p.poll(T0 + 3 * TICK_S, false), [Due::RefreshStale]);
    }

    #[test]
    fn maintenance_is_daily_and_never_right_at_launch() {
        // never ran: due once the launch grace period is over
        let mut p = Plan::new(T0, None);
        assert!(!p.poll(T0 + LAUNCH_DELAY_S, false).contains(&Due::Maintenance));
        assert!(!p.poll(T0 + MAINTENANCE_DELAY_S - 1, false).contains(&Due::Maintenance));
        assert!(!p.poll(T0 + MAINTENANCE_DELAY_S, true).contains(&Due::Maintenance), "not while busy");
        assert!(p.poll(T0 + MAINTENANCE_DELAY_S + 5, false).contains(&Due::Maintenance));
        assert!(!p.poll(T0 + MAINTENANCE_DELAY_S + 10, false).contains(&Due::Maintenance));
        assert!(p.poll(T0 + MAINTENANCE_DELAY_S + 5 + MAINTENANCE_EVERY_S, false).contains(&Due::Maintenance));
        // ran 2 h ago in an earlier session: not due today
        let mut p = Plan::new(T0, Some(T0 - 7200));
        assert!(!p.poll(T0 + MAINTENANCE_DELAY_S, false).contains(&Due::Maintenance));
        assert!(p.poll(T0 + MAINTENANCE_EVERY_S - 7200, false).contains(&Due::Maintenance));
    }
}
