//! Settings ▸ New episodes (DESIGN §44 §2.6), a page after Following. OWNER: N-ui. Listed once
//! `new_episodes::ENABLED`.
//!
//! * The switches of `[new_episodes]`: the master switch, desktop notifications, "Also series
//!   I've binged". Off, the master switch forgets the ledger, so on again starts with a silent
//!   baseline.
//! * "Check now": the same check as after a playlist refresh, at most once every 10 minutes
//!   (the page says how long to wait).
//! * Tracking: every series Tuner watches and why ("My list", "binged (S4: 7 of 8)"), its
//!   playlists and when it last had something new. Enter opens the series; Delete or the gear
//!   opens its menu — Open series, Stop alerts for this series / Allow alerts again (with Undo).
//! * Recent: the last 30 days of arrivals and what became of each (notified, quiet, watched).
//!
//! Series names go through `Guard::outside_title`, episodes show numbers only.

use std::cell::{Cell, RefCell};
use std::time::Instant;

use super::*;
use crate::model::Timestamp;
use crate::new_episodes::{Arrival, Cleared, Quiet, Tracked};

/// "Check now".
const ACT_CHECK: u16 = 1;
/// Recent lists at most this many lines.
const RECENT_MAX: usize = 60;

static ENABLED_T: CfgToggle = CfgToggle { label: "New episode alerts", get: |c| c.new_episodes.enabled, set: |c, v| c.new_episodes.enabled = v };
static NOTIFY_T: CfgToggle = CfgToggle { label: "Desktop notifications", get: |c| c.new_episodes.notify, set: |c, v| c.new_episodes.notify = v };
static BINGED_T: CfgToggle = CfgToggle { label: "Also series I've binged", get: |c| c.new_episodes.binged, set: |c, v| c.new_episodes.binged = v };

/// The master switch's row (the page turns it through `ui::new_episodes::set_enabled`).
pub(super) fn is_master(t: &CfgToggle) -> bool {
    std::ptr::eq(t, &ENABLED_T)
}

/// What the page shows, read in the background.
struct Data {
    /// Each tracked title and when it last had a new episode.
    tracked: Vec<(Tracked, Option<Timestamp>)>,
    recent: Vec<Arrival>,
}

fn load(c: &rusqlite::Connection, lib: &crate::store::library::Library, opts: &crate::store::new_episodes::Opts) -> crate::error::Result<Data> {
    use crate::store::new_episodes as store;
    let tracked = store::tracked(c, lib, opts)?;
    let last = store::last_new(c, &tracked)?;
    let recent = store::recent(c, lib, opts.now, store::RECENT_DAYS)?;
    Ok(Data { tracked: tracked.into_iter().zip(last).collect(), recent })
}

/// The page's state, kept in `SettingsScreen` (interior mutability: the rows are built from
/// `&SettingsScreen`, which is where the read is polled).
#[derive(Default)]
pub(super) struct State {
    loader: RefCell<Option<Loader<Data>>>,
    data: RefCell<Option<Data>>,
    /// "Check now" is not offered before this.
    wait_until: Cell<Option<Instant>>,
}

impl State {
    fn poll(&self) {
        match kit::poll(&mut self.loader.borrow_mut()) {
            Some(Ok(d)) => *self.data.borrow_mut() = Some(d),
            Some(Err(e)) => tracing::warn!(target: "tuner::new_episodes", "reading New episodes: {e}"),
            None => {}
        }
    }

    /// Minutes until "Check now" is available again, while it is not.
    fn wait_min(&self) -> Option<u64> {
        let left = self.wait_until.get()?.checked_duration_since(Instant::now())?;
        Some(left.as_secs().div_ceil(60).max(1))
    }
}

pub(super) fn enter(s: &mut SettingsScreen, ctx: &UiCtx<'_>) {
    let (lib, opts) = (ctx.lib.clone(), crate::store::new_episodes::Opts::from_config(&ctx.config.new_episodes, ctx.now));
    *s.new_episodes.loader.borrow_mut() = Some(Loader::start(ctx, move |c| load(c, &lib, &opts)));
}

pub(super) fn search_ready(s: &SettingsScreen) -> bool {
    s.new_episodes.poll();
    s.new_episodes.loader.borrow().is_none()
}

/// "Tue 21 Oct".
fn day(ts: Timestamp) -> String {
    crate::ui::widgets::time::local(ts, "%a %-d %b", "?")
}

/// The Recent rows: one per series per check ("Landman · S2E9, S2E10"), newest first.
fn recent_rows(ctx: &UiCtx<'_>, recent: &[Arrival]) -> Vec<String> {
    let guard = ctx.services.agent.guard();
    let mut groups: Vec<(&Arrival, Vec<(u32, u32)>)> = Vec::new();
    for a in recent {
        match groups.iter_mut().find(|(g, _)| g.series_title == a.series_title && g.arrived_at == a.arrived_at) {
            Some((_, eps)) => eps.push((a.season, a.episode)),
            None => groups.push((a, vec![(a.season, a.episode)])),
        }
    }
    groups
        .into_iter()
        .take(RECENT_MAX)
        .map(|(a, eps)| {
            let status = match (a.quiet, a.notified_at) {
                (Some(Quiet::Following), _) => "quiet: aired on TV (Following)",
                (Some(Quiet::Seen), _) => "quiet: already seen",
                (None, Some(_)) => "notified",
                (None, None) => "not notified",
            };
            let end = match a.cleared_how {
                Some(Cleared::Watched) => " · watched",
                Some(Cleared::Dismissed) => " · dismissed",
                Some(Cleared::Expired) => " · expired",
                None => "",
            };
            let title = guard.outside_title(&a.series_title, ctx.now);
            format!("{}  {title} · {} · {status}{end}", day(a.arrived_at), crate::new_episodes::span(&eps))
        })
        .collect()
}

/// The page's rows.
pub(super) fn rows(s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    s.new_episodes.poll();
    let data = s.new_episodes.data.borrow();
    let snapshot = crate::ui::new_episodes::snapshot(ctx);
    let cfg = &ctx.config.new_episodes;
    v.push(Row::Section("New episodes".into()));
    let refresh = match ctx.config.general.playlist_refresh_h {
        0 => "when you refresh them".to_owned(),
        h => format!("at each refresh (every {h} h, Settings ▸ Playlists)"),
    };
    let mut intro =
        format!("Tuner checks your playlists' series {refresh} and tells you when new episodes arrive. Airings on live TV are Following's job.");
    if let Some(at) = snapshot.checked_at {
        intro.push_str(&format!(" Checked {}", crate::ui::widgets::time::day_time(at, ctx.config.interface.clock_24h)));
        if let Some(d) = data.as_ref().filter(|_| cfg.enabled) {
            intro.push_str(&format!(" · tracking {} series", d.tracked.len()));
        }
        intro.push('.');
    }
    if !cfg.enabled {
        intro.push_str(" Switched off: nothing is checked, and switching it on starts from what exists today.");
    }
    v.push(Row::Note(intro, false));
    v.push(Row::Toggle(&ENABLED_T));
    if !cfg.enabled {
        return;
    }
    v.push(Row::Toggle(&NOTIFY_T));
    v.push(Row::Note("The Home row and the badges stay when this is off.".into(), false));
    v.push(Row::Toggle(&BINGED_T));
    v.push(Row::Note(
        format!("Over {} % of a season watched, at least {} episodes of it.", cfg.binge_pct, crate::store::new_episodes::BINGE_MIN),
        false,
    ));
    let running = ctx.services.jobs.context().new_episodes.running();
    v.push(Row::Act {
        act: Act::Feature(Feature::NewEpisodes, ACT_CHECK),
        icon: glyphs::REFRESH,
        label: if running { "Checking…".into() } else { "Check now".into() },
        danger: false,
    });
    if let Some(min) = s.new_episodes.wait_min() {
        v.push(Row::Note(format!("Checked a moment ago. You can check again in {min} min."), false));
    }
    let Some(d) = data.as_ref() else {
        v.push(Row::Note("Loading…".into(), false));
        return;
    };
    let guard = ctx.services.agent.guard();
    v.push(Row::Section(format!("Tracking · {} series", d.tracked.len())));
    if d.tracked.is_empty() {
        v.push(Row::Note("Nothing yet. Add a series to a watchlist and Tuner will tell you when new episodes arrive.".into(), false));
    }
    for (i, (t, last)) in d.tracked.iter().enumerate() {
        let why = t.why.iter().map(crate::new_episodes::Why::label).collect::<Vec<_>>().join(" · ");
        let mut playlists: Vec<&str> = t.copies.iter().filter_map(|c| ctx.lib.source(c.source)).map(|m| m.name.as_str()).collect();
        playlists.dedup();
        let mut detail = format!("{why} · {}", playlists.join(" · "));
        detail.push_str(&match last {
            Some(at) => format!(" · last new {}", day(*at)),
            None => " · nothing new yet".to_owned(),
        });
        if t.muted {
            detail = format!("alerts stopped · {detail}");
        }
        v.push(Row::Item {
            item: Item::Feature(Feature::NewEpisodes, i as i64),
            icon: if t.muted { glyphs::PAUSE } else { glyphs::SERIES }.into(),
            color: None,
            label: guard.outside_title(&t.title, ctx.now).into_owned(),
            detail: Some(detail),
            reorder: false,
        });
    }
    v.push(Row::Section(format!("Recent · {} days", crate::store::new_episodes::RECENT_DAYS)));
    let lines = recent_rows(ctx, &d.recent);
    if lines.is_empty() {
        v.push(Row::Note("No new episodes in this time.".into(), false));
    }
    v.extend(lines.into_iter().map(|l| Row::Note(l, false)));
}

/// Its `Act::Feature(Feature::NewEpisodes, n)` buttons: "Check now".
pub(super) fn act(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    if n != ACT_CHECK {
        return;
    }
    if ctx.services.jobs.context().new_episodes.running() {
        ctx.toast(Toast::info("Already checking for new episodes"));
        return;
    }
    match crate::jobs::new_episodes::check_now(&ctx.services.jobs) {
        Ok(_) => {
            s.new_episodes.wait_until.set(Some(Instant::now() + crate::jobs::new_episodes::CHECK_NOW_EVERY));
            ctx.toast(Toast::info("Checking for new episodes…"));
        }
        Err(wait) => {
            s.new_episodes.wait_until.set(Some(Instant::now() + wait));
            ctx.toast(Toast::info(format!("Checked a moment ago — try again in {} min", wait.as_secs().div_ceil(60).max(1))));
        }
    }
}

/// Enter on a tracked row: the series page.
pub(super) fn item(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) {
    let series = s.new_episodes.data.borrow().as_ref().and_then(|d| d.tracked.get(id as usize)).and_then(|(t, _)| t.series.first().copied());
    match series {
        Some(series) => ctx.push(Route::SeriesDetail(series)),
        None => ctx.toast(Toast::info("That series is no longer in your playlists")),
    }
}

/// Delete / right-click / the gear on a tracked row.
pub(super) fn menu(s: &SettingsScreen, id: i64) -> Option<(String, Vec<MenuItem>)> {
    let data = s.new_episodes.data.borrow();
    let (t, _) = data.as_ref()?.tracked.get(id as usize)?;
    let mi = |icon: &'static str, label: &str, code: u8| MenuItem {
        icon,
        label: label.into(),
        action: Action::Digit(code),
        enabled: true,
        shortcut: None,
    };
    Some((
        t.title.clone(),
        vec![
            mi(glyphs::INFO, "Open series", MENU_EDIT),
            if t.muted {
                mi(glyphs::CHECK, "Allow alerts again", MENU_TOGGLE)
            } else {
                mi(glyphs::HIDDEN, "Stop alerts for this series", MENU_TOGGLE)
            },
        ],
    ))
}

/// A choice in a tracked row's menu.
pub(super) fn menu_choice(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64, code: u8) {
    match code {
        MENU_EDIT => item(s, ctx, id),
        MENU_TOGGLE => {
            let row = s
                .new_episodes
                .data
                .borrow()
                .as_ref()
                .and_then(|d| d.tracked.get(id as usize))
                .map(|(t, _)| (t.copies.first().cloned(), t.title.clone(), t.muted));
            if let Some((Some(copy), title, muted)) = row {
                crate::ui::new_episodes::mute(ctx, &copy, &title, !muted);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_arrivals_of_one_check_read_as_one_line_per_series() {
        let (mut t, _one) = crate::ui::screens::home::testkit::rig();
        let arrival = |id: i64, ep: u32, quiet: Option<Quiet>, notified: Option<Timestamp>, how: Option<Cleared>| Arrival {
            id: crate::model::ArrivalId(id),
            series: None,
            season: 2,
            episode: ep,
            series_title: "Landman".into(),
            why: vec![],
            arrived_at: 1_760_000_000,
            notified_at: notified,
            quiet,
            opened_at: None,
            cleared_at: None,
            cleared_how: how,
        };
        let rows = t.with_ctx(|ctx| {
            recent_rows(
                ctx,
                &[
                    arrival(1, 10, None, Some(1), None),
                    arrival(2, 9, None, Some(1), None),
                    Arrival { series_title: "Taskmaster".into(), ..arrival(3, 4, Some(Quiet::Following), None, Some(Cleared::Watched)) },
                ],
            )
        });
        assert!(rows[0].contains("Landman · S2E9–E10 · notified"), "{rows:?}");
        assert!(rows[1].contains("Taskmaster · S2E4 · quiet: aired on TV (Following) · watched"), "{rows:?}");
        assert_eq!(rows.len(), 2);
    }
}
