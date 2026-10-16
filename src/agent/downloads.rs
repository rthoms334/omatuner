//! Downloads tool: opaque ids and guarded state, never filenames, URLs or titles
//! (DESIGN §43 §6–§7). Confirmations live in the plugin's RULES.md.
use super::params::{Downloads, DownloadsAction};
use super::proto::{AgentRef, ErrorCode, item_ref};
use super::query::{Fail, Out, Q, parse_ref};
use super::server::Inner;
use crate::downloads::{DownloadCommand, SeriesScope, Target};
use crate::model::{DownloadId, ItemKind};
use serde_json::{Value, json};
use std::sync::Arc;

fn target(q: &Q, reference: &str, minutes: Option<u32>, any_copy: bool) -> Out<Target> {
    let (item, programme) = match parse_ref(reference)? {
        AgentRef::Item(item) => (item, None),
        AgentRef::Programme { channel, start } => {
            let p =
                q.channel_by_ref(&channel).map(|ch| q.programme_at(ch, start)).transpose()?.flatten().ok_or_else(|| Fail::not_found("programme"))?;
            let catchup_id = if p.flags.has_catchup_id { crate::store::epg::programme(q.conn, p.id)?.catchup_id } else { None };
            (channel, Some(crate::queue::Slot { start: p.start, stop: p.stop, catchup_id }))
        }
    };
    if q.conceals(&item) {
        return Err(Fail::not_found("item"));
    }
    if item.kind == ItemKind::Channel && programme.is_none() && minutes.is_none() {
        return Err(Fail::bad("a channel recording needs minutes"));
    }
    Ok(Target { item, programme, minutes, any_copy })
}

pub(super) async fn handle(h: &Arc<Inner>, p: Downloads) -> Out<Value> {
    let handle = h.shared.downloads().ok_or_else(|| Fail::new(ErrorCode::Unavailable, "Downloads are unavailable"))?;
    if p.action == DownloadsAction::List {
        let snapshot = handle.snapshot();
        return h.run(false, move |q| {
            let items: Vec<_> = snapshot.jobs.iter().filter(|j| !crate::downloads::planning::concealed(q.conn, q.lib, q.view, j))
                .filter(|j| p.state.as_deref().is_none_or(|s| s == j.state.as_str())).map(|j| json!({
                    "id":j.id.0, "ref":j.item().as_ref().map(item_ref), "kind":j.kind.as_str(), "state":j.state.as_str(),
                    "progress":j.progress(), "eta_s":j.bytes_total.zip(j.fp.get("download_speed_bps").and_then(|v|v.as_f64()).filter(|n|*n>0.0)).map(|(n,rate)|(n.saturating_sub(j.bytes_done) as f64/rate).ceil() as u64), "account":j.source_id.map(|s| s.0), "reason":j.reason,
                    "when":j.prog_start, "stop":j.prog_stop, "bytes":j.bytes_done,
                    "s":j.fp.get("download_season"), "e":j.fp.get("download_episode")
                })).collect();
            Ok(json!({"used_gb":snapshot.used_bytes as f64 / 1e9,"cap_gb":snapshot.cap_bytes as f64 / 1e9,"free_gb":snapshot.free_bytes as f64 / 1e9,"items":items}))
        }).await;
    }
    let requested_ids = p.ids.clone();
    let command = match p.action {
        DownloadsAction::Add | DownloadsAction::Record => {
            let refs = if p.action == DownloadsAction::Record {
                vec![p.item.clone().or(p.channel.clone()).ok_or_else(|| Fail::bad("record needs ref or channel"))?]
            } else {
                p.refs.clone().filter(|v| !v.is_empty() && v.len() <= 50).ok_or_else(|| Fail::bad("add needs 1 to 50 refs"))?
            };
            let any = match p.copy.as_deref() {
                None | Some("best") => true,
                Some("this") => false,
                _ => return Err(Fail::bad("copy is best or this")),
            };
            let targets = h.run(true, move |q| refs.iter().map(|r| target(q, r, p.minutes, any)).collect::<Out<Vec<_>>>()).await?;
            if p.action == DownloadsAction::Record && targets.iter().any(|t| t.item.kind != ItemKind::Channel) {
                return Err(Fail::bad("record needs a programme or channel"));
            }
            DownloadCommand::Add { targets, agent: true }
        }
        DownloadsAction::Series => {
            let reference = p.item.clone().ok_or_else(|| Fail::bad("series needs ref"))?;
            let item = h
                .run(true, move |q| {
                    let AgentRef::Item(item) = parse_ref(&reference)? else { return Err(Fail::bad("give a series ref")) };
                    if q.conceals(&item) {
                        return Err(Fail::not_found("series"));
                    }
                    let index = q.vod_index(&item)?.ok_or_else(|| Fail::not_found("series"))?;
                    if q.title_locked(false, index) {
                        return Err(Fail::new(ErrorCode::Unavailable, "locked: needs your PIN in Tuner"));
                    }
                    Ok(item)
                })
                .await?;
            let check = item.clone();
            let unloaded = h
                .run(true, move |q| {
                    let sid = crate::downloads::planning::series_id(q.conn, &check)?;
                    crate::store::copies::unloaded_series(q.conn, &[sid]).map_err(Fail::from)
                })
                .await?;
            if !unloaded.is_empty() {
                for id in unloaded {
                    h.jobs.submit(crate::jobs::JobKind::LoadSeriesDetails(id));
                }
                return Err(Fail::new(ErrorCode::Unavailable, "Loading episodes; try again in a few seconds"));
            }
            let scope = match p.scope.as_deref() {
                Some("next") => SeriesScope::Next(1),
                Some("next_n") => SeriesScope::Next(p.n.filter(|n| *n > 0 && *n <= 50).ok_or_else(|| Fail::bad("n must be 1 to 50"))?),
                Some("season") => SeriesScope::Season(p.season.ok_or_else(|| Fail::bad("give season"))?),
                Some("all") => SeriesScope::All,
                Some("keep") => SeriesScope::Keep(p.n.filter(|n| (1..=10).contains(n)).ok_or_else(|| Fail::bad("n must be 1 to 10"))?),
                _ => return Err(Fail::bad("scope is next, next_n, season, all or keep")),
            };
            DownloadCommand::Series { item, scope, agent: true }
        }
        _ => {
            let ids: Vec<_> = p
                .ids
                .filter(|v| !v.is_empty() && v.len() <= 50)
                .ok_or_else(|| Fail::bad("give 1 to 50 ids from list"))?
                .into_iter()
                .map(DownloadId)
                .collect();
            match p.action {
                DownloadsAction::Pause => DownloadCommand::Pause(ids),
                DownloadsAction::Resume => DownloadCommand::Resume(ids),
                DownloadsAction::Cancel => DownloadCommand::Cancel(ids),
                DownloadsAction::Delete => DownloadCommand::Delete(ids),
                DownloadsAction::Retry => DownloadCommand::Retry(ids),
                _ => unreachable!(),
            }
        }
    };
    let reply = handle.request(command).await;
    if let Some(error) = reply.error {
        return Err(Fail::new(ErrorCode::Unavailable, error));
    }
    let refused: Vec<_> = reply.refused.iter().map(|(item, error)| json!({"ref":item_ref(item),"error":error})).collect();
    let mut out = json!({"added":reply.added.iter().map(|id| id.0).collect::<Vec<_>>(),"refused":refused,"rule":reply.rule,"size_gb":reply.size_bytes as f64 / 1e9});
    if p.action == DownloadsAction::Record
        && let Some(id) = reply.added.first()
    {
        out["id"] = id.0.into();
        let id = *id;
        let timing = h.run(false, move |q| Ok(crate::store::downloads::job(q.conn, id)?)).await?;
        if let Some(j) = timing {
            out["start"] = j.start().into();
            out["stop"] = j.stop().into();
            if let Some(source) = j.source_id.filter(|s| !handle.budget().admits(j.id, *s)) {
                out["clash"] = json!({"account":source.0,"what":if handle.budget().download_holder(source).is_some(){"download"}else{"watching"}});
            }
        }
    }
    if let Some(ids) = requested_ids {
        out["items"] = h
            .run(false, move |q| {
                let mut items = Vec::new();
                for id in ids {
                    if let Some(job) = crate::store::downloads::job(q.conn, DownloadId(id))?
                        && !crate::downloads::planning::concealed(q.conn, q.lib, q.view, &job)
                    {
                        items.push(json!({"id":id,"state":job.state.as_str()}));
                    }
                }
                Ok(json!(items))
            })
            .await?;
    }
    Ok(out)
}
