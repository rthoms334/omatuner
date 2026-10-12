//! `reminders` (list / add / cancel; notify or switch, `open_in`) and `sleep_timer` (MCP pass 2,
//! the skill plan §2). OWNER: agent track.
//!
//! Reminders are ordinary rows (`store::user::add_reminder`, the one delete path
//! `remove_reminder`) fired by Tuner's scheduler, also while Tuner is closed (B6). Claude can't
//! schedule a channel that needs the PIN now, never sees reminders of concealed channels, and a
//! Following rule's reminder shows the rule's name when the rule is spoiler-safe. The sleep
//! timer runs in Tuner (`reminders::sleep::SleepHandle`), so it outlives the conversation.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::act::sleep_out;
use super::params::{RemindersAction, SleepAction as SleepVerb};
use super::proto::{AgentRef, ChannelOut, ErrorCode, Reminders, SleepTimer, fmt_time};
use super::query::{Fail, Out, Q, channel_ref, clean, parse_ref, when};
use super::server::{Inner, to_value};
use crate::model::{OpenIn, Reminder, ReminderId, ReminderMode, ReminderState};
use crate::msg::DataScope;

/// Pending reminders at most (each `add` is one; the skill caps a request at about 20).
pub const MAX_PENDING: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReminderOut {
    pub id: i64,
    /// The programme (`programme:` ref) — pass to `play` once it airs.
    #[serde(rename = "ref")]
    pub item: String,
    pub title: String,
    pub channel: ChannelOut,
    pub start: String,
    /// `notify` or `switch`.
    pub mode: &'static str,
    /// When the notification goes out, or the switch happens.
    pub fires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead_min: Option<u32>,
    #[serde(skip_serializing_if = "is_zero")]
    pub offset_min: i64,
    /// Switches: where playback opens (`popout`, `full_window`, `where_i_am`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_in: Option<&'static str>,
    /// Made by a Following rule (its name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemindersOut {
    pub reminders: Vec<ReminderOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AddedOut {
    pub reminder: ReminderOut,
    /// It already existed (a notification became a switch, or nothing changed).
    pub existed: bool,
    /// Pending reminders whose programmes overlap this one (two switches can't both happen).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clashes: Vec<ClashOut>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClashOut {
    pub id: i64,
    pub title: String,
    pub start: String,
    /// `overlap` or `switches` (both switch: only one can).
    pub kind: &'static str,
}

fn open_in_name(o: OpenIn) -> &'static str {
    match o {
        OpenIn::PopOut => "popout",
        OpenIn::FullWindow => "full_window",
        OpenIn::WhereIAm => "where_i_am",
    }
}

/// A reminder as the tools show it (`None`: its channel isn't visible now).
fn reminder_out(q: &Q, r: &Reminder, rules: &HashMap<crate::model::FollowRuleId, crate::follow::FollowRule>) -> Option<ReminderOut> {
    if q.conceals(&r.channel) {
        return None;
    }
    let c = q.channel_by_ref(&r.channel)?;
    let rule = r.rule_id.and_then(|id| rules.get(&id));
    let title = rule.map_or(r.programme_title.as_str(), |rule| rule.safe_title(&r.programme_title));
    let switch = r.switches();
    let lead_default = i64::from(q.cfg.reminders.lead_min) * 60;
    Some(ReminderOut {
        id: r.id.0,
        item: AgentRef::programme(channel_ref(c), r.start).to_string(),
        title: clean(title),
        channel: q.channel_out(c),
        start: fmt_time(r.start),
        mode: if switch { "switch" } else { "notify" },
        fires_at: fmt_time(if switch { crate::reminders::switch_at(r) } else { crate::reminders::notify_at(r, lead_default) }),
        lead_min: (!switch).then(|| (crate::reminders::lead_of(r, lead_default) / 60) as u32),
        offset_min: r.offset_s / 60,
        open_in: switch.then(|| open_in_name(crate::reminders::open_in(r, q.cfg.reminders.open_in))),
        rule: rule.map(|x| clean(&x.name)),
    })
}

fn rules(q: &Q) -> Out<HashMap<crate::model::FollowRuleId, crate::follow::FollowRule>> {
    Ok(crate::store::follow::rules(q.conn)?.into_iter().map(|r| (r.id, r)).collect())
}

fn list(q: &Q) -> Out<RemindersOut> {
    let rules = rules(q)?;
    let late = q.now - crate::platform::notify::LATE_SWITCH_S;
    let reminders = crate::store::user::all_reminders(q.conn)?
        .iter()
        .filter(|r| crate::reminders::effective_stop(r) > late)
        .filter_map(|r| reminder_out(q, r, &rules))
        .collect();
    Ok(RemindersOut { reminders })
}

/// What `add` writes.
struct AddPlan {
    reminder: Reminder,
    log_ref: String,
}

fn plan_add(q: &Q, p: &Reminders) -> Out<AddPlan> {
    let item = p.programme.as_deref().ok_or_else(|| Fail::bad("add needs programme (a programme ref)"))?;
    let AgentRef::Programme { channel, start } = parse_ref(item)? else {
        return Err(Fail::bad("programme is a programme ref (from guide_search, whats_on or details)"));
    };
    let c = q.channel_by_ref(&channel).filter(|_| !q.conceals(&channel)).ok_or_else(|| Fail::not_found("programme"))?;
    let prog = q.programme_at(c, start)?.ok_or_else(|| Fail::not_found("programme"))?;
    if q.needs_pin(c.locked) {
        return Err(Fail::new(ErrorCode::Unavailable, "locked: needs your PIN in Tuner (locked channels can't be scheduled)"));
    }
    let mode = match p.mode.as_deref() {
        None | Some("notify") => ReminderMode::Notify,
        Some("switch") => ReminderMode::Switch,
        Some(_) => return Err(Fail::bad("mode is notify or switch")),
    };
    let offset_s = i64::from(p.offset_min.unwrap_or(0)) * 60;
    if !(-3600..=7200).contains(&offset_s) {
        return Err(Fail::bad("offset_min is −60 to 120"));
    }
    if prog.start + offset_s <= q.now {
        return Err(Fail::new(ErrorCode::Unavailable, "That has already started: use play instead."));
    }
    let lead_s = match (mode, p.lead_min) {
        (ReminderMode::Switch, Some(_)) => return Err(Fail::bad("lead_min is for notify: a switch happens at the start (with a 10 s Cancel)")),
        (ReminderMode::Switch, None) => Reminder::SWITCH_LEAD,
        (ReminderMode::Notify, Some(m)) if m > 120 => return Err(Fail::bad("lead_min is 0–120")),
        (ReminderMode::Notify, m) => i64::from(m.unwrap_or(q.cfg.reminders.lead_min)) * 60,
    };
    let open_in = match p.open_in.as_deref() {
        None => None,
        Some(_) if mode == ReminderMode::Notify => return Err(Fail::bad("open_in is for switch")),
        Some("popout") => Some(OpenIn::PopOut),
        Some("full_window") => Some(OpenIn::FullWindow),
        Some("where_i_am") => Some(OpenIn::WhereIAm),
        Some(_) => return Err(Fail::bad("open_in is popout, full_window or where_i_am")),
    };
    Ok(AddPlan {
        reminder: Reminder {
            id: ReminderId(0),
            channel: channel_ref(c),
            channel_name: c.name.to_string(),
            programme_title: prog.title.to_string(),
            start: prog.start,
            stop: prog.stop,
            lead_s,
            state: ReminderState::Pending,
            mode,
            offset_s,
            open_in,
            rule_id: None,
        },
        log_ref: AgentRef::programme(channel_ref(c), prog.start).to_string(),
    })
}

/// A pending reminder the agent may cancel (`not_found` for anything else, a concealed one too).
fn check_cancel(q: &Q, id: i64) -> Out<ReminderId> {
    match crate::store::user::reminder(q.conn, ReminderId(id))? {
        Some(r) if r.state == ReminderState::Pending && !q.conceals(&r.channel) && q.channel_by_ref(&r.channel).is_some() => Ok(r.id),
        _ => Err(Fail::not_found("reminder")),
    }
}

pub(super) async fn handle(h: &Arc<Inner>, p: Reminders) -> Out<Value> {
    match p.action {
        RemindersAction::List => {
            if p.programme.is_some() || p.id.is_some() {
                return Err(Fail::bad("list takes no programme or id"));
            }
            let out = h.run(true, list).await?;
            to_value(&out)
        }
        RemindersAction::Cancel => {
            let id = p.id.ok_or_else(|| Fail::bad("cancel needs id (from list)"))?;
            let id = h.run(true, move |q| check_cancel(q, id)).await?;
            h.write(DataScope::Reminders, move |c| crate::store::user::remove_reminder(c, id, crate::model::RemoveWhy::Agent)).await?;
            h.shared.wake_reminders();
            tracing::info!("agent: reminders cancel {}", id.0);
            to_value(&serde_json::json!({ "cancelled": true }))
        }
        RemindersAction::Add => {
            let plan = h.run(true, move |q| plan_add(q, &p)).await?;
            let r = plan.reminder.clone();
            let late = crate::model::now() - crate::platform::notify::LATE_SWITCH_S;
            let written = h
                .write(DataScope::Reminders, move |c| {
                    let pending = crate::store::user::all_reminders(c)?;
                    // the row of the same programme in any state: one whose notification has
                    // already fired still holds the (channel, start) slot
                    let same = crate::store::user::reminders_for_channel(c, &r.channel)?.into_iter().find(|x| x.start == r.start);
                    let was_pending = same.as_ref().is_some_and(|x| x.state == ReminderState::Pending);
                    if !was_pending && pending.len() >= MAX_PENDING {
                        return Ok(Err("Too many reminders are pending; cancel some first"));
                    }
                    let was_switch = was_pending && same.as_ref().is_some_and(Reminder::switches);
                    if r.switches() && !was_switch && pending.iter().filter(|x| x.switches() && x.start >= late).count() >= super::act::MAX_SWITCHES {
                        return Ok(Err("Too many switches are scheduled; cancel one first"));
                    }
                    let id = crate::store::user::add_reminder(c, &r)?;
                    // an existing row of the same programme takes this one's mode and times
                    // (pending again when its new time is still ahead)
                    if let Some(old) = &same {
                        let row = crate::store::user::edit_reminder(
                            c,
                            id,
                            &crate::store::user::ReminderEdit {
                                lead_s: Some(r.lead_s),
                                mode: Some(r.mode),
                                offset_s: Some(r.offset_s),
                                open_in: Some(r.open_in.or(old.open_in)),
                            },
                        )?;
                        if row.state == ReminderState::Dismissed && crate::reminders::arming::fire_at(&row, 0) > crate::model::now() {
                            crate::store::user::set_reminder_state(c, id, ReminderState::Pending)?;
                        }
                    }
                    let clashes: Vec<(Reminder, crate::reminders::ClashKind)> = {
                        let now_row = crate::store::user::reminder(c, id)?.unwrap_or(r.clone());
                        let around = crate::store::user::reminders_overlapping(c, now_row.start, crate::reminders::effective_stop(&now_row))?;
                        crate::reminders::clashes_with(&now_row, &around).into_iter().map(|(x, k)| (x.clone(), k)).collect()
                    };
                    Ok(Ok((id, same.is_some(), clashes)))
                })
                .await?;
            let (id, existed, clashes) = written.map_err(|m| Fail::new(ErrorCode::Unavailable, m))?;
            h.shared.wake_reminders();
            tracing::info!("agent: reminders add {} → reminder {}", plan.log_ref, id.0);
            let out = h
                .run(true, move |q| {
                    let rules = rules(q)?;
                    let row = crate::store::user::reminder(q.conn, id)?.ok_or_else(|| Fail::not_found("reminder"))?;
                    let reminder = reminder_out(q, &row, &rules).ok_or_else(|| Fail::not_found("reminder"))?;
                    let clashes = clashes
                        .iter()
                        .filter_map(|(x, k)| {
                            let o = reminder_out(q, x, &rules)?;
                            Some(ClashOut {
                                id: o.id,
                                title: o.title,
                                start: o.start,
                                kind: match k {
                                    crate::reminders::ClashKind::Switches => "switches",
                                    _ => "overlap",
                                },
                            })
                        })
                        .collect();
                    Ok(AddedOut { reminder, existed, clashes })
                })
                .await?;
            to_value(&out)
        }
    }
}

// ------------------------------------------------------------------ sleep_timer

/// The minute a bare `HH:MM` that has passed today means tomorrow.
fn sleep_when(q: &Q, p: &SleepTimer) -> Out<(crate::reminders::sleep::SleepWhen, Option<crate::model::Timestamp>)> {
    use crate::reminders::sleep::SleepWhen;
    let given = [p.at.is_some(), p.after_min.is_some(), p.end_of_programme == Some(true), p.after_items.is_some(), p.end_of_queue == Some(true)];
    if given.iter().filter(|g| **g).count() != 1 {
        return Err(Fail::bad("set takes exactly one of at, after_min, end_of_programme, after_items, end_of_queue"));
    }
    if let Some(at) = p.at.as_deref() {
        let mut t = when("at", at, q.now)?;
        if t <= q.now && super::act::is_clock_time(at) {
            t += 86_400;
        } else if t < q.now {
            return Err(Fail::bad("at is in the past"));
        }
        return Ok((SleepWhen::At(t), None));
    }
    if let Some(m) = p.after_min {
        return Ok((SleepWhen::At(q.now + i64::from(m) * 60), None));
    }
    if let Some(n) = p.after_items {
        return Ok((SleepWhen::AfterItems(n), None));
    }
    if p.end_of_queue == Some(true) {
        return Ok((SleepWhen::EndOfQueue, None));
    }
    Ok((SleepWhen::EndOfProgramme, None))
}

pub(super) async fn sleep_timer(h: &Arc<Inner>, p: SleepTimer) -> Out<Value> {
    let Some(timer) = h.shared.sleep() else {
        return Err(Fail::new(ErrorCode::Unavailable, "The sleep timer isn't running in this Tuner"));
    };
    match p.action {
        SleepVerb::Status => to_value(&serde_json::json!({ "set": timer.status().map(|s| sleep_out(&s)) })),
        SleepVerb::Cancel => {
            let cancelled = timer.cancel();
            tracing::info!("agent: sleep_timer cancel → {cancelled}");
            to_value(&serde_json::json!({ "cancelled": cancelled }))
        }
        SleepVerb::Set => {
            let action = match p.then.as_deref() {
                None | Some("stop") => crate::reminders::sleep::SleepAction::Stop,
                Some("pause") => crate::reminders::sleep::SleepAction::Pause,
                Some(_) => return Err(Fail::bad("then is stop or pause")),
            };
            // end of a live programme: the guide's end of what is on now
            let playing = h.player.snapshot().playable.clone();
            let (when, stop) = h
                .run(false, move |q| {
                    let (when, _) = sleep_when(q, &p)?;
                    let stop = match (&when, playing.as_deref()) {
                        (crate::reminders::sleep::SleepWhen::EndOfProgramme, Some(pl)) if pl.kind.is_live() => {
                            let ch = pl.item.as_ref().and_then(|i| q.lib.live.by_ref(i));
                            match ch {
                                Some(id) => crate::reminders::sleep::live_programme_stop(q.conn, q.lib, id, q.now)?,
                                None => None,
                            }
                        }
                        _ => None,
                    };
                    Ok((when, stop))
                })
                .await?;
            let warn = h.cfg().reminders.sleep_warn_s;
            let status = timer.set(when, action, warn, stop).map_err(|e| Fail::new(ErrorCode::Unavailable, e.message()))?;
            tracing::info!("agent: sleep_timer set");
            to_value(&serde_json::json!({ "set": sleep_out(&status) }))
        }
    }
}
