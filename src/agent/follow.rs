//! `follow` (Following + news flash, DESIGN §33 §8): standing guide searches. OWNER: agent
//! track.
//!
//! preview → show → confirm → add (the skill's rule: always preview, one rule per add, switch
//! mode only when the user asked). Rules Claude makes are marked (`by_agent`, Settings shows
//! them), capped at 10 an hour and 20 in all. Previews never look at explicit or hidden
//! channels (`follow::plan::Pool`); a spoiler-safe rule's matches show only the rule's name,
//! the time and the channel.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::params::{Follow, FollowAction, FollowRuleInput};
use super::proto::{AgentRef, ChannelOut, ErrorCode, MAX_TEXT, fmt_time, item_ref};
use super::query::{Fail, Out, Q, clamp, clean, parse_ref, text};
use super::server::{Inner, to_value};
use crate::follow::flash::{FlashLength, FlashVolume};
use crate::follow::plan::{MatchState, Upcoming};
use crate::follow::{Airings, ChannelScope, FollowKind, FollowMode, FollowRule};
use crate::model::FollowRuleId;
use crate::msg::DataScope;

/// Rules Claude may add in an hour, and in all.
pub const AGENT_RULES_PER_HOUR: usize = 10;
pub const AGENT_RULES_MAX: usize = 20;
/// Terms (names and aliases) a rule takes at most.
const MAX_TERMS: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchOut {
    /// The programme (`programme:` ref on the chosen channel).
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_title: Option<String>,
    pub start: String,
    pub end: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelOut>,
    /// Other channels showing it (refs), best first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_on: Vec<String>,
    /// `would_remind`, `would_switch`, `would_flash`, `listed`, `over_cap`, `skipped`,
    /// `left_out`, `scheduled`, `adopted`.
    pub state: String,
    /// Why it is left out or downgraded (`locked`, `clash`, a left-out reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreviewOut {
    pub rule: RuleOut,
    pub matches: Vec<MatchOut>,
    pub left_out: Vec<MatchOut>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub name: String,
    /// `team`, `show`, `event`, `custom`, `news_flash`.
    pub kind: &'static str,
    /// `notify`, `switch`, `list`, `flash`.
    pub mode: &'static str,
    pub terms: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub without: Vec<String>,
    pub spoiler_safe: bool,
    pub enabled: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_claude: bool,
    /// List rules: the next few matches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next: Vec<MatchOut>,
}

fn kind_name(k: FollowKind) -> &'static str {
    match k {
        FollowKind::Team => "team",
        FollowKind::Show => "show",
        FollowKind::Event => "event",
        FollowKind::Custom => "custom",
        FollowKind::News => "news_flash",
    }
}

fn mode_name(m: FollowMode) -> &'static str {
    match m {
        FollowMode::Notify => "notify",
        FollowMode::Switch => "switch",
        FollowMode::List => "list",
        FollowMode::Flash => "flash",
    }
}

fn rule_out(r: &FollowRule) -> RuleOut {
    RuleOut {
        id: (r.id.0 != 0).then_some(r.id.0),
        name: clean(&r.name),
        kind: kind_name(r.kind),
        mode: mode_name(r.mode),
        terms: r.spec.terms.iter().map(|t| clean(t)).collect(),
        without: r.spec.without.iter().map(|t| clean(t)).collect(),
        spoiler_safe: r.spoiler_safe,
        enabled: r.enabled,
        by_claude: r.by_agent,
        next: Vec::new(),
    }
}

/// One match as the tools show it; `safe`: only the rule's name, the time and the channel.
fn match_out(q: &Q, u: &Upcoming, rule: &FollowRule, safe: bool, preview: bool) -> MatchOut {
    let concealed = u.channel.as_ref().is_some_and(|c| q.conceals(c));
    let channel = u.channel.as_ref().filter(|_| !concealed).and_then(|c| q.channel_by_ref(c));
    // a hidden or locked-away channel's programme isn't named either
    let safe = safe || concealed;
    let state = match (u.state, preview) {
        (MatchState::Scheduled, true) => match u.mode {
            FollowMode::Switch => "would_switch",
            FollowMode::Flash => "would_flash",
            FollowMode::List => "listed",
            FollowMode::Notify => "would_remind",
        },
        (MatchState::LeftOut, _) => "left_out",
        (s, _) => s.as_str(),
    };
    MatchOut {
        item: channel.map(|c| AgentRef::programme(super::query::channel_ref(c), u.start).to_string()),
        title: clean(if safe { &rule.name } else { &u.title }),
        sub_title: if safe { None } else { u.sub_title.as_deref().map(clean) },
        start: fmt_time(u.start),
        end: fmt_time(u.stop),
        channel: channel.map(|c| q.channel_out(c)),
        also_on: u.alternates.iter().filter(|a| !q.conceals(a) && q.channel_by_ref(a).is_some()).map(item_ref).collect(),
        state: state.replace('-', "_"),
        note: u.note.as_deref().map(|n| if n.starts_with("clash:") { "clash".to_owned() } else { clean(n) }),
    }
}

fn words(field: &str, v: &[String]) -> Out<Vec<String>> {
    if v.len() > MAX_TERMS {
        return Err(Fail::bad(format!("{field}: at most {MAX_TERMS}")));
    }
    v.iter().map(|t| text(field, t).map(str::to_owned)).collect()
}

/// A rule from Claude's description (`id` 0 unless replacing).
pub fn rule_from_input(q: &Q, i: &FollowRuleInput) -> Out<FollowRule> {
    let name = text("rule.name", &i.name)?;
    let kind = match i.kind.as_str() {
        "team" => FollowKind::Team,
        "show" => FollowKind::Show,
        "event" => FollowKind::Event,
        "custom" => FollowKind::Custom,
        "news_flash" => FollowKind::News,
        _ => return Err(Fail::bad("rule.kind is team, show, event, custom or news_flash")),
    };
    let terms = words("rule.terms", &i.terms)?;
    let terms = if terms.is_empty() { vec![name.to_owned()] } else { terms };
    let mut r = FollowRule::draft(name, kind, terms, &q.cfg.follow, q.now);
    r.by_agent = true;
    let s = &mut r.spec;
    s.with_any = words("rule.with_any", &i.with_any)?;
    for w in words("rule.without", &i.without)? {
        if !s.without.iter().any(|x| x.eq_ignore_ascii_case(&w)) {
            s.without.push(w);
        }
    }
    s.categories = words("rule.categories", &i.categories)?;
    if let Some(a) = i.airings.as_deref() {
        s.airings = match a {
            "live_only" => Airings::LiveOnly,
            "live_or_new" => Airings::LiveOrNew,
            "new_only" => Airings::NewOnly,
            "any" => Airings::Any,
            _ => return Err(Fail::bad("rule.airings is live_only, live_or_new, new_only or any")),
        };
    }
    if let Some(ch) = &i.channels {
        s.channels = channel_scope(q, ch)?;
    }
    if let Some(days) = &i.days {
        let mut mask = 0u8;
        for d in days {
            let bit = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                .iter()
                .position(|x| d.trim().to_lowercase().starts_with(x))
                .ok_or_else(|| Fail::bad("rule.days are mon … sun"))?;
            mask |= 1 << bit;
        }
        s.days = mask;
    }
    if kind == FollowKind::News {
        let flash = s.flash.get_or_insert_with(Default::default);
        if let Some(m) = i.headlines_min {
            flash.length = FlashLength::Headlines(m.clamp(1, 60) as u16);
        }
        if let Some(v) = i.volume.as_deref() {
            flash.volume = match v {
                "normal" => FlashVolume::Normal,
                "mute" => FlashVolume::Mute,
                _ => return Err(Fail::bad("rule.volume is normal or mute")),
            };
        }
    } else if i.headlines_min.is_some() || i.volume.is_some() {
        return Err(Fail::bad("headlines_min and volume are for news_flash rules"));
    }
    if let Some(m) = i.mode.as_deref() {
        r.mode = match m {
            "notify" => FollowMode::Notify,
            "switch" => FollowMode::Switch,
            "list" => FollowMode::List,
            "flash" if kind == FollowKind::News => FollowMode::Flash,
            "flash" => return Err(Fail::bad("mode flash is for news_flash rules")),
            _ => return Err(Fail::bad("rule.mode is notify, switch, list or flash")),
        };
    }
    if let Some(l) = i.lead_min {
        if l > 120 {
            return Err(Fail::bad("rule.lead_min is 0–120"));
        }
        r.lead_s = i64::from(l) * 60;
    }
    if let Some(o) = i.offset_min {
        if !(-60..=120).contains(&o) {
            return Err(Fail::bad("rule.offset_min is −60 to 120"));
        }
        r.offset_s = i64::from(o) * 60;
    }
    if let Some(safe) = i.spoiler_safe {
        r.spoiler_safe = safe;
    }
    if let Some(m) = i.max_per_week {
        r.max_per_week = m.clamp(1, 50);
    }
    if let Some(why) = r.spec.problem() {
        return Err(Fail::bad(why));
    }
    Ok(r)
}

/// `"any"`, `"favourites"`, `{"list": name}`, `{"playlist": name}`, `{"channels": [refs]}`.
fn channel_scope(q: &Q, v: &Value) -> Out<ChannelScope> {
    let bad = || Fail::bad("rule.channels is \"any\", \"favourites\", {\"list\": name}, {\"playlist\": name} or {\"channels\": [refs]}");
    if let Some(s) = v.as_str() {
        return match s {
            "any" => Ok(ChannelScope::Any),
            "favourites" => Ok(ChannelScope::Favourites),
            _ => Err(bad()),
        };
    }
    let o = v.as_object().filter(|o| o.len() == 1).ok_or_else(bad)?;
    let (k, x) = o.iter().next().ok_or_else(bad)?;
    let name = || x.as_str().filter(|s| !s.trim().is_empty() && s.len() <= MAX_TEXT).ok_or_else(bad);
    match k.as_str() {
        "list" => {
            let n = name()?.to_lowercase();
            let l = q.favs().lists.iter().find(|l| l.name.to_lowercase() == n).ok_or_else(|| Fail::not_found("favourites list"))?;
            Ok(ChannelScope::List(l.id))
        }
        "playlist" => {
            let n = name()?.to_lowercase();
            let s = q.lib.sources.iter().find(|s| s.name.to_lowercase().contains(&n)).ok_or_else(|| Fail::not_found("playlist"))?;
            Ok(ChannelScope::Playlist(s.id))
        }
        "channels" => {
            let refs = x.as_array().filter(|a| !a.is_empty() && a.len() <= 50).ok_or_else(bad)?;
            let mut out = Vec::new();
            for r in refs {
                let AgentRef::Item(r) = parse_ref(r.as_str().ok_or_else(bad)?)? else { return Err(bad()) };
                if q.conceals(&r) || q.channel_by_ref(&r).is_none() {
                    return Err(Fail::not_found("channel"));
                }
                out.push(r);
            }
            Ok(ChannelScope::Channels(out))
        }
        _ => Err(bad()),
    }
}

fn saved_rule(q: &Q, id: i64) -> Out<FollowRule> {
    crate::store::follow::rule(q.conn, FollowRuleId(id))?.ok_or_else(|| Fail::not_found("rule"))
}

fn preview(q: &Q, p: &Follow) -> Out<PreviewOut> {
    let rule = match (&p.rule, p.id) {
        (Some(i), None) => rule_from_input(q, i)?,
        (None, Some(id)) => saved_rule(q, id)?,
        _ => return Err(Fail::bad("preview takes rule or id")),
    };
    let opts = crate::follow::preview::PreviewOpts {
        now: q.now,
        days: p.days.unwrap_or(14).clamp(1, 14),
        limit: clamp(p.limit, 20, 50),
        pin_active: q.view.pin_active,
        max_pending: q.cfg.follow.max_pending as usize,
    };
    let pv = crate::follow::preview::preview(q.conn, q.lib, &rule, &opts, &jiff::tz::TimeZone::system(), &|| false)?;
    let safe = rule.spoiler_safe || p.spoiler_safe == Some(true);
    Ok(PreviewOut {
        matches: pv.matches.iter().map(|u| match_out(q, u, &rule, safe, true)).collect(),
        left_out: pv.left_out.iter().map(|u| match_out(q, u, &rule, safe, true)).collect(),
        warnings: pv.warnings.iter().map(|w| clean(w)).collect(),
        rule: rule_out(&rule),
    })
}

fn list(q: &Q) -> Out<Value> {
    let rules = crate::store::follow::rules(q.conn)?;
    let upcoming = crate::store::follow::upcoming(q.conn, q.now, 300)?;
    let out: Vec<RuleOut> = rules
        .iter()
        .map(|r| {
            let mut o = rule_out(r);
            // `upcoming` already shows a spoiler-safe rule's matches by its name
            o.next = upcoming.iter().filter(|u| u.rule == r.id).take(3).map(|u| match_out(q, u, r, r.spoiler_safe, false)).collect();
            o
        })
        .collect();
    Ok(json!({ "rules": out }))
}

pub(super) async fn handle(h: &Arc<Inner>, p: Follow) -> Out<Value> {
    match p.action {
        FollowAction::Preview => {
            let out = h.run(true, move |q| preview(q, &p)).await?;
            to_value(&out)
        }
        FollowAction::List => h.run(true, list).await,
        FollowAction::Add => {
            let input = p.rule.clone().ok_or_else(|| Fail::bad("add needs rule"))?;
            let replace = p.replace_id;
            let (rule, would) = h
                .run(true, move |q| {
                    let mut rule = rule_from_input(q, &input)?;
                    if let Some(id) = replace {
                        let old = saved_rule(q, id)?;
                        // Claude never loosens the user's guard
                        if old.spoiler_safe && !rule.spoiler_safe {
                            return Err(Fail::bad("that rule is spoiler-safe; only the user can turn that off in Tuner ▸ Settings ▸ Following"));
                        }
                        rule.by_agent = old.by_agent;
                        rule.id = old.id;
                        rule.priority = old.priority;
                        rule.created_at = old.created_at;
                    }
                    let opts = crate::follow::preview::PreviewOpts {
                        now: q.now,
                        days: 14,
                        limit: 50,
                        pin_active: q.view.pin_active,
                        max_pending: q.cfg.follow.max_pending as usize,
                    };
                    let pv = crate::follow::preview::preview(q.conn, q.lib, &rule, &opts, &jiff::tz::TimeZone::system(), &|| false)?;
                    let would = pv.matches.iter().filter(|m| m.state == MatchState::Scheduled && m.mode != FollowMode::List).count();
                    Ok((rule, would))
                })
                .await?;
            let max_rules = h.cfg().follow.max_rules;
            let now = crate::model::now();
            let to_save = rule.clone();
            let id = h
                .write(DataScope::Follow, move |c| {
                    let (hour, all) = crate::store::follow::agent_rule_counts(c, now - 3600)?;
                    if to_save.id.0 == 0 && (hour >= AGENT_RULES_PER_HOUR || all >= AGENT_RULES_MAX) {
                        return Ok(Err(format!(
                            "Claude may add {AGENT_RULES_PER_HOUR} rules an hour and {AGENT_RULES_MAX} in all; the user can add more in Tuner"
                        )));
                    }
                    Ok(Ok(crate::store::follow::save_rule(c, &to_save, max_rules, now)?))
                })
                .await?
                .map_err(|m| Fail::new(ErrorCode::Unavailable, m))?;
            h.jobs.submit(crate::jobs::JobKind::EvaluateFollows { reason: crate::follow::FollowReason::Rules });
            tracing::info!("agent: follow add → rule {}", id.0);
            // a new rule: the user sees it and can take it back at once
            if rule.id.0 == 0 {
                let name = clean(&rule.name);
                let reminders = if would == 1 { "1 reminder".to_owned() } else { format!("{would} reminders") };
                let mut toast = crate::msg::Toast::info(format!("Claude added {name} · {reminders}"));
                toast.action = Some(("Undo".into(), crate::keymap::Action::UndoFollow(id)));
                h.bus.toast(toast);
            }
            let mut out = rule_out(&FollowRule { id, ..rule });
            out.by_claude = true;
            to_value(&json!({ "rule": out, "reminders_to_set": would, "message": "Tuner checks the guide for this after every refresh." }))
        }
        FollowAction::Remove => {
            let id = p.id.ok_or_else(|| Fail::bad("remove needs id (from list)"))?;
            let keep = p.keep_reminders.unwrap_or(false);
            h.run(true, move |q| saved_rule(q, id)).await?;
            let cancelled = h.write(DataScope::Follow, move |c| crate::store::follow::delete_rule(c, FollowRuleId(id), keep)).await?;
            h.shared.wake_reminders();
            tracing::info!("agent: follow remove rule {id} → {cancelled} reminders cancelled");
            to_value(&json!({ "removed": true, "reminders_cancelled": cancelled }))
        }
    }
}
