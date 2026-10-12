//! `spoiler_guard` (DESIGN §34 §6): the app's spoiler guard as one action-shaped tool. OWNER:
//! agent track.
//!
//! The guard itself already applies to every reply (`agent::guard::guard_reply`); this tool
//! reads and adds its rules. Claude can add guards and mark things seen when the user says they
//! watched them; it can't pause the guard (that stays in the app) and never reveals anything on
//! its own (`reveal_spoilers` exists only on the read tools, for refs the user asked about in
//! that turn).

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::params::{SpoilerAction, SpoilerGuard};
use super::proto::{AgentRef, fmt_time};
use super::query::{Fail, Out, Q, clean, parse_ref, text, when};
use super::server::{Inner, to_value};
use crate::model::{ItemKind, SeenHow, SeenRow, SpoilerMode, SpoilerRule, SpoilerRuleId, SpoilerRuleKind};
use crate::msg::DataScope;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopicOut {
    pub rule: String,
    /// `None`: a Following rule's topic (edit it with `follow`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// `embargo`, `series`, `blackout`.
    pub reason: &'static str,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The event it guards now or next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<EventOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventOut {
    pub title: String,
    pub start: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleOut {
    pub id: i64,
    /// `topic`, `series`, `blackout`.
    pub kind: &'static str,
    pub label: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// `until_seen` or `always`.
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_h: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// A series row with `enabled` false is the user's "don't guard" exception.
    pub enabled: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_claude: bool,
}

/// Terms per topic, and the longest label (the store's limits, checked here for a clear
/// message).
const MAX_TERMS: usize = crate::store::spoiler::MAX_TERMS;

fn rule_out(r: &SpoilerRule) -> RuleOut {
    RuleOut {
        id: r.id.0,
        kind: match r.kind {
            SpoilerRuleKind::Topic => "topic",
            SpoilerRuleKind::Series => "series",
            SpoilerRuleKind::Blackout => "blackout",
        },
        label: clean(&r.label),
        terms: r.terms.iter().map(|t| clean(t)).collect(),
        categories: r.categories.iter().map(|t| clean(t)).collect(),
        mode: match r.mode {
            SpoilerMode::UntilSeen => "until_seen",
            SpoilerMode::Always => "always",
        },
        window_h: (r.kind != SpoilerRuleKind::Series).then_some(r.window_h),
        until: r.until_at.map(fmt_time),
        enabled: r.enabled,
        by_claude: r.by_agent,
    }
}

fn status(h: &Inner) -> Value {
    let g = h.shared.guard();
    let now = crate::model::now();
    let topics: Vec<TopicOut> = g
        .topics(now)
        .into_iter()
        .map(|t| TopicOut {
            rule: clean(&t.label),
            id: t.rule.map(|r| r.0),
            reason: t.kind.as_str(),
            active: t.active,
            until: t.until.filter(|u| *u != crate::model::Timestamp::MAX).map(fmt_time),
            event: t.event.map(|(title, start)| EventOut { title: clean(&title), start: fmt_time(start) }),
        })
        .collect();
    json!({
        "enabled": h.cfg().spoiler.enabled && crate::spoiler::ENABLED,
        "on": g.on(),
        "paused_until": g.paused_until(now).map(|t| if t == crate::model::Timestamp::MAX { "restart".to_owned() } else { fmt_time(t) }),
        "topics": topics,
    })
}

fn terms(field: &str, v: Option<&[String]>) -> Out<Vec<String>> {
    let v = v.unwrap_or_default();
    if v.len() > MAX_TERMS {
        return Err(Fail::bad(format!("{field}: at most {MAX_TERMS}")));
    }
    v.iter().map(|t| text(field, t).map(str::to_owned)).collect()
}

fn mode(p: &SpoilerGuard) -> Out<SpoilerMode> {
    match p.mode.as_deref() {
        None | Some("until_seen") => Ok(SpoilerMode::UntilSeen),
        Some("always") => Ok(SpoilerMode::Always),
        Some(_) => Err(Fail::bad("mode is until_seen or always")),
    }
}

fn base_rule(kind: SpoilerRuleKind, label: &str, now: crate::model::Timestamp) -> SpoilerRule {
    SpoilerRule {
        id: SpoilerRuleId(0),
        kind,
        label: label.to_owned(),
        terms: Vec::new(),
        series_norm: None,
        categories: Vec::new(),
        mode: SpoilerMode::UntilSeen,
        window_h: 48,
        until_at: None,
        sport_descr: true,
        shield: true,
        enabled: true,
        by_agent: true,
        created_at: now,
    }
}

/// What a write does.
enum Write {
    Add(SpoilerRule),
    Series { norm: String, label: String },
    Remove(SpoilerRuleId),
    Seen(SeenRow),
}

fn plan(q: &Q, p: &SpoilerGuard) -> Out<Write> {
    let window_h = p.window_h.unwrap_or(q.cfg.spoiler.default_window_h).min(336);
    match p.action {
        SpoilerAction::AddTopic => {
            let label = text("label", p.label.as_deref().ok_or_else(|| Fail::bad("add_topic needs label"))?)?;
            let mut terms = terms("terms", p.terms.as_deref())?;
            if terms.is_empty() {
                terms.push(label.to_owned());
            }
            Ok(Write::Add(SpoilerRule {
                terms,
                categories: self::terms("categories", p.categories.as_deref())?,
                mode: mode(p)?,
                window_h,
                ..base_rule(SpoilerRuleKind::Topic, label, q.now)
            }))
        }
        SpoilerAction::AddSeries => {
            let s = p.series_ref.as_deref().ok_or_else(|| Fail::bad("add_series needs series_ref"))?;
            let AgentRef::Item(r) = parse_ref(s)? else { return Err(Fail::bad("series_ref is a series ref")) };
            if r.kind != ItemKind::Series {
                return Err(Fail::bad("series_ref is a series ref"));
            }
            let idx = q.vod_index(&r)?.ok_or_else(|| Fail::not_found("series"))?;
            let name = q.lib.vod.series[idx as usize].name.to_string();
            Ok(Write::Series { norm: crate::spoiler::series_key(&name), label: name })
        }
        SpoilerAction::Blackout => {
            // a programme (a fixture's two teams, else its core title, like the app's "I'll
            // watch it later") or a topic, until a time
            let (label, terms, until_default) = match (p.item.as_deref(), p.topic.as_deref()) {
                (Some(s), None) => {
                    let AgentRef::Programme { channel, start } = parse_ref(s)? else { return Err(Fail::bad("ref is a programme ref")) };
                    let c = q.channel_by_ref(&channel).ok_or_else(|| Fail::not_found("programme"))?;
                    let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
                    let until = Some(prog.stop + i64::from(window_h) * 3600);
                    match crate::textmatch::fixture::parse_fixture(&prog.title, prog.sub_title.as_deref()) {
                        Some((a, b)) => (format!("{a} v {b}"), vec![a, b], until),
                        None => {
                            let core = crate::textmatch::event::core_title(&prog.title);
                            let core = if core.is_empty() { prog.title.to_string() } else { core };
                            (prog.title.to_string(), vec![core], until)
                        }
                    }
                }
                (None, Some(t)) => {
                    let t = text("topic", t)?.to_owned();
                    (t.clone(), vec![t], None)
                }
                _ => return Err(Fail::bad("blackout takes ref or topic")),
            };
            let until = match p.until.as_deref() {
                Some(u) => when("until", u, q.now)?,
                None => until_default.ok_or_else(|| Fail::bad("a topic blackout needs until"))?,
            };
            if until <= q.now || until > q.now + 14 * 86_400 {
                return Err(Fail::bad("until is in the next 14 days"));
            }
            Ok(Write::Add(SpoilerRule { terms, until_at: Some(until), window_h, ..base_rule(SpoilerRuleKind::Blackout, &label, q.now) }))
        }
        SpoilerAction::Remove => {
            // only Claude's own rules: removing the user's would loosen their guard
            let id = SpoilerRuleId(p.id.ok_or_else(|| Fail::bad("remove needs id (from list)"))?);
            let r = crate::store::spoiler::rule(q.conn, id).map_err(|_| Fail::not_found("spoiler rule"))?;
            if !r.by_agent {
                return Err(Fail::bad("that rule is yours — remove it in Tuner ▸ Settings ▸ Spoiler guard"));
            }
            Ok(Write::Remove(id))
        }
        SpoilerAction::MarkSeen => {
            let s = p.item.as_deref().ok_or_else(|| Fail::bad("mark_seen needs ref"))?;
            match parse_ref(s)? {
                AgentRef::Programme { channel, start } => {
                    let c = q.channel_by_ref(&channel).ok_or_else(|| Fail::not_found("programme"))?;
                    let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
                    // EPG time, like the tracker and Ctrl+W (a shifted start could change the
                    // fixture key's date)
                    let full = crate::store::epg::programme(q.conn, prog.id)?;
                    Ok(Write::Seen(crate::spoiler::seen_row_for(&full, SeenHow::Agent, q.now)))
                }
                AgentRef::Item(r) if r.kind == ItemKind::Episode => {
                    let (ep, idx) = q.episode(&r)?.ok_or_else(|| Fail::not_found("episode"))?;
                    let norm = crate::spoiler::series_key(&q.lib.vod.series[idx as usize].name);
                    Ok(Write::Seen(SeenRow {
                        event_key: format!("ep:{norm}:{}:{}", ep.season, ep.episode),
                        first_start: q.now,
                        title: ep.title.clone(),
                        series_norm: Some(norm),
                        season: i32::try_from(ep.season).ok(),
                        episode: i32::try_from(ep.episode).ok(),
                        seen_at: q.now,
                        how: SeenHow::Agent,
                    }))
                }
                _ => Err(Fail::bad("ref is a programme or episode ref")),
            }
        }
        SpoilerAction::Status | SpoilerAction::List => Err(Fail::bad("not a write")),
    }
}

pub(super) async fn handle(h: &Arc<Inner>, p: SpoilerGuard) -> Out<Value> {
    match p.action {
        SpoilerAction::Status => Ok(status(h)),
        SpoilerAction::List => {
            let rules = h.run(false, |q| Ok(crate::store::spoiler::list_rules(q.conn)?)).await?;
            let following = h.run(false, |q| Ok(crate::store::spoiler::follow_topics(q.conn)?)).await?;
            to_value(&json!({
                "rules": rules.iter().map(rule_out).collect::<Vec<_>>(),
                "following": following.iter().map(|t| clean(&t.term.label)).collect::<Vec<_>>(),
            }))
        }
        action => {
            let write = h.run(true, move |q| plan(q, &p)).await?;
            let now = crate::model::now();
            let out = h
                .write(DataScope::Spoiler, move |c| {
                    Ok(match write {
                        Write::Add(r) => json!({ "id": crate::store::spoiler::add_rule(c, &r, now)?.0 }),
                        Write::Series { norm, label } => {
                            json!({ "id": crate::store::spoiler::set_series_guard(c, &norm, &label, true, true, now)?.0 })
                        }
                        Write::Remove(id) => json!({ "removed": crate::store::spoiler::remove_rule(c, id)? }),
                        Write::Seen(row) => json!({ "marked": crate::store::spoiler::mark_seen(c, &row)? }),
                    })
                })
                .await?;
            h.shared.guard().rebuild();
            tracing::info!("agent: spoiler_guard {action:?}");
            Ok(out)
        }
    }
}
