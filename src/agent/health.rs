//! The health tools of the skill plan (§2): `diagnose` (playback trouble) and `sources`
//! (playlists and guides). Read-only. OWNER: agent track.
//!
//! Nothing here carries an address, a login or a provider's message: failure reasons are
//! Tuner's own error kinds (`http-forbidden`, `stalled`, …), playlist errors are redacted when
//! stored and pass `query::clean` and the reply check again on the way out.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::act::error_kind;
use super::proto::{AgentRef, CopyOut, Diagnose, PlayerStateOut, TileName, fmt_time, item_ref};
use super::query::{Fail, Out, Q, channel_copies_out, clean, parse_ref, when};
use super::server::{Inner, to_value};
use crate::cast::firewall::{FirewallState, UnknownWhy};
use crate::model::{ItemKind, ItemRef, SourceId};
use crate::player::{PlayerSnapshot, PlayerState};

// ------------------------------------------------------------------ diagnose

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiagnoseOut {
    /// The pictures playing now and how they fare.
    pub playing: Vec<PictureHealth>,
    /// The channel or title asked about (default: what plays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemHealth>,
    /// Failures on each playlist in the window (channels that failed, how often).
    pub playlists: Vec<PlaylistHealth>,
    /// Since when failures count.
    pub since: String,
    /// Tuner's reading of it (`provider_outage`, `connection_limit`, `account`, `dead_channel`,
    /// `local_decode`, `network`): a hint, the skill explains it.
    pub likely: Vec<String>,
    /// Casting, once a TV is paired or a cast ran (DESIGN §40 §12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cast: Option<CastSection>,
}

/// `diagnose`'s cast section: what the TV is and how casting stands, with no address.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CastSection {
    /// The TV last cast to (or being cast to): "AppleTV14,1".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Its OS version ("26.6 (23L773)").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// The AirPlay version it reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
    /// The pairing variant that worked (`hkp3`, `hkp5`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<String>,
    /// How the TV is told to keep time: always `ptp-declared` in Phase 1.
    pub timing: String,
    /// `no_firewall`, `allowed`, `allowed_by_tuner`, `blocked`, `blocked_shadowed` or
    /// `unknown_<why>`. For the blocked ones the user presses **Allow on firewall** in Tuner
    /// (Settings ▸ Cast ▸ Network); agents never change the firewall.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firewall: Option<String>,
    /// The typed reason of the last cast failure (`cast-firewall`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// The firewall state as `diagnose` words it.
fn firewall_name(s: FirewallState) -> &'static str {
    match s {
        FirewallState::NoFirewall => "no_firewall",
        FirewallState::Allowed { ours: false } => "allowed",
        FirewallState::Allowed { ours: true } => "allowed_by_tuner",
        FirewallState::Blocked { shadowed: false } => "blocked",
        FirewallState::Blocked { shadowed: true } => "blocked_shadowed",
        FirewallState::Unknown { why: UnknownWhy::Firewalld } => "unknown_firewalld",
        FirewallState::Unknown { why: UnknownWhy::Nftables } => "unknown_nftables",
        FirewallState::Unknown { why: UnknownWhy::Unreadable } => "unknown_unreadable",
        FirewallState::Unknown { why: UnknownWhy::NoTv } => "unknown_no_tv",
    }
}

/// The cast section from the service's own report (`CastApi::diagnose`, the firewall detection
/// it last made). `None` while casting has nothing to say: no TV known and no cast ever failed.
pub fn cast_section(d: &crate::cast::CastDiagnose, known: bool) -> Option<CastSection> {
    if !known && d.device.is_none() && d.last_error.is_none() {
        return None;
    }
    let text = |s: &Option<String>| s.as_deref().map(clean).filter(|s| !s.is_empty());
    Some(CastSection {
        model: text(&d.model),
        os_version: text(&d.os_version),
        source_version: text(&d.source_version),
        pairing: d.pairing.map(|p| p.as_str().to_owned()),
        timing: d.timing.to_owned(),
        firewall: d.firewall.map(|f| firewall_name(f).to_owned()),
        last_error: d.last_error.map(|e| e.reason().to_owned()),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PictureHealth {
    pub tile: TileName,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub state: PlayerStateOut,
    /// The error kind (never the provider's message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Seconds buffered ahead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_frames: Option<i64>,
    /// The hardware decoder in use (`none` = software).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hwdec: Option<String>,
    /// `1920x1080`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemHealth {
    #[serde(rename = "ref")]
    pub item: String,
    pub name: String,
    pub failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<FailureOut>,
    /// Channels: the same channel in other playlists, with their failures.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<CopyOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailureOut {
    pub at: String,
    /// The error kind.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistHealth {
    pub playlist: String,
    /// Channels that failed in the window.
    pub failed_channels: u32,
    /// Failures in the window (at most the last per channel counts its repeats).
    pub failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountOut {
    /// `active`, `expired`, `banned`, `disabled`, `unknown`, `other`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trial: bool,
}

fn account_out(a: &crate::model::AccountInfo) -> AccountOut {
    use crate::model::AccountStatus as S;
    AccountOut {
        status: match a.status {
            S::Active => "active",
            S::Expired => "expired",
            S::Banned => "banned",
            S::Disabled => "disabled",
            S::Unknown => "unknown",
            S::Other => "other",
        }
        .into(),
        expires: a.exp_date.map(fmt_time),
        connections: a.active_cons,
        max_connections: a.max_connections,
        trial: a.is_trial,
    }
}

/// Channels of one playlist failing in the window that read as an outage of the provider.
const OUTAGE_CHANNELS: u32 = 3;
/// Dropped frames that point at local decoding.
const DROPPED_LOCAL: i64 = 300;

fn picture(q: &Q, tile: crate::model::TileId, s: &PlayerSnapshot) -> Option<PictureHealth> {
    let p = s.playable.as_ref().filter(|_| !matches!(s.state, PlayerState::Idle | PlayerState::Ended))?;
    let shown = p.item.as_ref().filter(|i| !q.conceals(i));
    let info = s.stream_info.as_ref();
    Some(PictureHealth {
        tile: TileName::of(tile),
        item: shown.map(item_ref),
        state: match &s.state {
            PlayerState::Zapping { .. } | PlayerState::Loading { .. } => PlayerStateOut::Loading,
            PlayerState::Buffering { .. } => PlayerStateOut::Buffering,
            PlayerState::Reconnecting { .. } => PlayerStateOut::Reconnecting,
            PlayerState::Error(_) => PlayerStateOut::Error,
            _ if s.paused => PlayerStateOut::Paused,
            _ => PlayerStateOut::Playing,
        },
        error: match &s.state {
            PlayerState::Error(e) => Some(error_kind(e.kind)),
            PlayerState::Reconnecting { reason, .. } => Some(error_kind(*reason)),
            _ => s.last_error.as_ref().map(|e| error_kind(e.kind)),
        },
        cache_s: s.cache_secs.or(info.and_then(|i| i.cache_secs)).map(|c| (c * 10.0).round() / 10.0),
        dropped_frames: info.and_then(|i| i.dropped_frames),
        hwdec: s.hwdec.clone().or(info.and_then(|i| i.hwdec.clone())),
        video: s.video_size.map(|(w, h)| format!("{w}x{h}")),
        video_codec: info.and_then(|i| i.video_codec.clone()),
        fps: info.and_then(|i| i.fps).map(|f| (f * 100.0).round() / 100.0),
        playlist: s.claim.and_then(|src| q.lib.source(src)).map(|m| clean(&m.name)),
    })
}

fn plan_diagnose(q: &Q, p: &Diagnose, snaps: &[(crate::model::TileId, Arc<PlayerSnapshot>)]) -> Out<DiagnoseOut> {
    let since = p.since.as_deref().map(|s| when("since", s, q.now)).transpose()?.unwrap_or(q.now - 86_400);
    let playing: Vec<PictureHealth> = snaps.iter().filter_map(|(t, s)| picture(q, *t, s)).collect();
    let failures = crate::store::live::failures(q.conn, since)?;
    // the item: the one asked about, else the first picture's
    let target: Option<ItemRef> = match p.item.as_deref() {
        Some(s) => Some(match parse_ref(s)? {
            AgentRef::Item(r) => r,
            AgentRef::Programme { channel, .. } => channel,
        }),
        None => snaps.first().and_then(|(_, s)| s.playable.as_ref()).and_then(|p| p.item.clone()),
    };
    let item = match target.filter(|r| !q.conceals(r)) {
        Some(r) if r.kind == ItemKind::Channel => {
            let c = q.channel_by_ref(&r).ok_or_else(|| Fail::not_found("channel"))?;
            let f = failures.get(&r);
            Some(ItemHealth {
                item: item_ref(&r),
                name: clean(&c.name),
                failures: f.map_or(0, |f| f.count),
                last_failure: f.map(|f| FailureOut { at: fmt_time(f.at), reason: clean(&f.reason) }),
                copies: channel_copies_out(q, c)?.into_iter().filter(|x| x.item != item_ref(&r)).collect(),
            })
        }
        Some(r) => {
            let name = match r.kind {
                ItemKind::Movie | ItemKind::Series => {
                    let movies = r.kind == ItemKind::Movie;
                    let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("title"))?;
                    clean(&if movies { &q.lib.vod.movies } else { &q.lib.vod.series }[idx as usize].name)
                }
                _ => q.episode(&r)?.map(|(e, _)| clean(&e.title)).ok_or_else(|| Fail::not_found("episode"))?,
            };
            Some(ItemHealth { item: item_ref(&r), name, failures: 0, last_failure: None, copies: Vec::new() })
        }
        None if p.item.is_some() => return Err(Fail::not_found("item")),
        None => None,
    };
    // failures per playlist (visible channels only)
    let mut per: HashMap<SourceId, (u32, u32)> = HashMap::new();
    for (r, f) in &failures {
        if q.channel_by_ref(r).is_some() && !q.conceals(r) {
            let e = per.entry(r.source).or_default();
            e.0 += 1;
            e.1 += f.count.max(1);
        }
    }
    let accounts: HashMap<SourceId, crate::model::AccountInfo> =
        crate::store::sources::list_sources(q.conn)?.into_iter().filter_map(|s| s.status.account.map(|a| (s.id, a))).collect();
    let mut playlists: Vec<PlaylistHealth> = q
        .lib
        .sources
        .iter()
        .filter(|s| s.enabled && s.has_live)
        .map(|s| {
            let (failed_channels, failures) = per.get(&s.id).copied().unwrap_or_default();
            PlaylistHealth { playlist: clean(&s.name), failed_channels, failures, account: accounts.get(&s.id).map(account_out) }
        })
        .collect();
    playlists.sort_by_key(|p| std::cmp::Reverse(p.failed_channels));
    // the hints
    let mut likely: Vec<String> = Vec::new();
    fn hint(likely: &mut Vec<String>, s: &str) {
        if !likely.iter().any(|x| x == s) {
            likely.push(s.to_owned());
        }
    }
    for pl in &playlists {
        if pl.account.as_ref().is_some_and(|a| a.status != "active" && a.status != "unknown") {
            hint(&mut likely, "account");
        }
        if pl.failed_channels >= OUTAGE_CHANNELS {
            hint(&mut likely, "provider_outage");
        }
    }
    for pic in &playing {
        match pic.error.as_deref() {
            Some("connection-limit" | "http-forbidden") => hint(&mut likely, "connection_limit"),
            Some("network" | "timeout") => hint(&mut likely, "network"),
            _ => {}
        }
        if pic.dropped_frames.is_some_and(|d| d >= DROPPED_LOCAL) {
            hint(&mut likely, "local_decode");
        }
    }
    if let Some(i) = &item
        && i.failures >= 2
        && !likely.iter().any(|x| x == "provider_outage")
    {
        hint(&mut likely, "dead_channel");
    }
    Ok(DiagnoseOut { playing, item, playlists, since: fmt_time(since), likely, cast: None })
}

pub(super) async fn diagnose(h: &Arc<Inner>, p: Diagnose) -> Out<Value> {
    let mut snaps = vec![(crate::model::TileId::A, h.player.snapshot())];
    if let Some(b) = h.shared.tile_player(crate::model::TileId::B) {
        snaps.push((crate::model::TileId::B, b.borrow().clone()));
    }
    let mut out = h.run(true, move |q| plan_diagnose(q, &p, &snaps)).await?;
    let cast = h.shared.cast();
    out.cast = cast_section(&cast.diagnose(), !cast.devices().is_empty());
    to_value(&out)
}

// ------------------------------------------------------------------ sources

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourcesOut {
    pub playlists: Vec<PlaylistOut>,
    pub guides: Vec<GuideOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistOut {
    pub playlist: String,
    /// `m3u` or `xtream`.
    pub kind: String,
    pub enabled: bool,
    /// In "All playlists".
    pub in_all: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    /// The last refresh failed: its (redacted) reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    /// What Tuner shows of it (the user's hidden items and, while locked, explicit ones aren't
    /// counted).
    pub channels: u32,
    pub movies: u32,
    pub series: u32,
    /// Shown channels and how many have guide data.
    pub guide_coverage: CoverageOut,
    pub failures_24h: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoverageOut {
    pub shown: u32,
    pub with_guide: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuideOut {
    pub guide: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    /// Hours since its last refresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_h: Option<u32>,
    /// Its last programme ends then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    pub channels: u32,
}

pub fn sources(q: &Q) -> Out<SourcesOut> {
    let failures = crate::store::live::failures(q.conn, q.now - 86_400)?;
    let coverage = crate::guide_fix::coverage(q.lib);
    let stored: HashMap<SourceId, crate::model::Source> = crate::store::sources::list_sources(q.conn)?.into_iter().map(|s| (s.id, s)).collect();
    let playlists = q
        .lib
        .sources
        .iter()
        .map(|m| {
            let s = stored.get(&m.id);
            let cov = coverage.iter().find(|c| c.source == m.id);
            let count = |k: crate::model::ItemKind| q.lib.count(k, m.id) as u32;
            PlaylistOut {
                playlist: clean(&m.name),
                kind: if m.kind == crate::model::SourceKind::Xtream { "xtream" } else { "m3u" }.into(),
                enabled: m.enabled,
                in_all: m.in_all,
                account: s.and_then(|s| s.status.account.as_ref()).map(account_out),
                last_refresh: m.last_refresh.map(fmt_time),
                refresh_error: s.and_then(|s| s.status.last_error.as_deref()).map(clean),
                channels: count(crate::model::ItemKind::Channel),
                movies: count(crate::model::ItemKind::Movie),
                series: count(crate::model::ItemKind::Series),
                guide_coverage: CoverageOut { shown: cov.map_or(0, |c| c.shown as u32), with_guide: cov.map_or(0, |c| c.with_guide as u32) },
                failures_24h: failures.iter().filter(|(r, _)| r.source == m.id && q.channel_by_ref(r).is_some()).map(|(_, f)| f.count.max(1)).sum(),
            }
        })
        .collect();
    let guides = crate::store::sources::list_epg_sources(q.conn)?
        .into_iter()
        .map(|g| GuideOut {
            guide: clean(&g.name),
            enabled: g.enabled,
            last_refresh: g.last_refresh.map(fmt_time),
            age_h: g.last_refresh.map(|t| ((q.now - t).max(0) / 3600) as u32),
            ends_at: g.max_stop.map(fmt_time),
            refresh_error: g.last_error.as_deref().map(clean),
            channels: g.channel_count,
        })
        .collect();
    Ok(SourcesOut { playlists, guides })
}
