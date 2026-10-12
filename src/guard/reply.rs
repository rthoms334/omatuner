//! The guard over an MCP reply (B4; DESIGN §34 §6): the body of `agent::guard::guard_reply`,
//! the one choke point every read result passes. OWNER: S1 (the agent track calls it).
//!
//! It walks the result's JSON, so new tools are covered by field name without code here:
//! * an object's `title` (a programme, or an episode under a series), `name` (when its `kind` is
//!   a movie, series or programme) and `now` (a channel's programme) are titles: rewritten
//!   ("Arsenal v Chelsea") or, when they must not show, replaced by "‹rule› · title hidden";
//! * `subtitle` / `sub_title`, `description` and `plot` are removed when guarded;
//! * an object with `season` and `episode` numbers under a series (an object of `kind` series, or
//!   one carrying a `series` name) is an episode: after the next-up one its `title` and `plot`
//!   go;
//! * every object that lost or changed something gains
//!   `spoiler: {hidden: [fields], rule, reason, until}`;
//! * `reveal(ref)` true for an object's `ref` → that object and everything in it unguarded
//!   (`reveal_spoilers`, only when the user asked in that turn — the skill's rule).
//!
//! The reply never says more than the screen would: hidden text is removed, not marked.

use serde_json::{Map, Value, json};

use super::{Field, Guard, Guarded, Policy, Reason, Subject};
use crate::model::Timestamp;

/// Guard one result in place.
pub fn guard_json(v: &mut Value, guard: &Guard, policy: Policy, reveal: &dyn Fn(&str) -> bool, now: Timestamp) {
    walk(v, &Ctx { guard, policy, reveal, now, series: None });
}

struct Ctx<'a> {
    guard: &'a Guard,
    policy: Policy,
    reveal: &'a dyn Fn(&str) -> bool,
    now: Timestamp,
    /// The series key the objects inside belong to.
    series: Option<String>,
}

fn walk(v: &mut Value, ctx: &Ctx<'_>) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(|x| walk(x, ctx)),
        Value::Object(o) => object(o, ctx),
        _ => {}
    }
}

fn object(o: &mut Map<String, Value>, ctx: &Ctx<'_>) {
    let policy = match o.get("ref").and_then(Value::as_str) {
        Some(r) if (ctx.reveal)(r) => Policy::Reveal,
        _ => ctx.policy,
    };
    let kind = o.get("kind").and_then(Value::as_str).map(str::to_owned);
    let series = match (kind.as_deref(), o.get("name").and_then(Value::as_str), o.get("series").and_then(Value::as_str)) {
        (_, _, Some(s)) => Some(crate::spoiler::series_key(s)),
        (Some("series"), Some(n), _) => Some(crate::spoiler::series_key(n)),
        _ => ctx.series.clone(),
    };
    if policy != Policy::Reveal {
        guard_fields(o, kind.as_deref(), series.as_deref(), policy, ctx);
    }
    let inner = Ctx { guard: ctx.guard, policy, reveal: ctx.reveal, now: ctx.now, series };
    for (k, x) in o.iter_mut() {
        if k != "spoiler" {
            walk(x, &inner);
        }
    }
}

fn int(o: &Map<String, Value>, k: &str) -> Option<i32> {
    o.get(k).and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok())
}

fn string(o: &Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// The guarded fields of one object.
fn guard_fields(o: &mut Map<String, Value>, kind: Option<&str>, series: Option<&str>, policy: Policy, ctx: &Ctx<'_>) {
    let (g, now) = (ctx.guard, ctx.now);
    let mut hidden: Vec<&'static str> = Vec::new();
    let mut rewritten = false;
    let mut reason: Option<Reason> = None;
    let categories: Vec<String> =
        o.get("categories").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let se = int(o, "season").zip(int(o, "episode"));
    let title_key = if o.contains_key("title") {
        Some("title")
    } else if matches!(kind, Some("movie" | "series" | "programme")) && o.contains_key("name") {
        Some("name")
    } else {
        None
    };
    let title = title_key.and_then(|k| string(o, k));
    let sub_key = ["subtitle", "sub_title"].into_iter().find(|k| o.contains_key(*k));
    let sub = sub_key.and_then(|k| string(o, k));

    let mut set = |o: &mut Map<String, Value>, key: &'static str, out: Guarded<'_>, title: bool| match out {
        Guarded::Shown(_) => {}
        Guarded::Rewritten(t) => {
            o.insert(key.into(), Value::String(t.into_owned()));
            rewritten = true;
        }
        Guarded::Hidden { reason: r } => {
            if title {
                let label = r.as_ref().map_or("Spoiler guard", |r| r.rule.as_str()).to_owned();
                o.insert(key.into(), Value::String(format!("{label} · title hidden")));
            } else {
                o.remove(key);
            }
            hidden.push(key);
            if reason.is_none() {
                reason = r;
            }
        }
    };

    // an episode of a known series
    if let (Some(norm), Some((s, e)), None) = (series, se, kind.filter(|k| *k == "series")) {
        let subj = Subject::Episode { series_norm: norm, season: s, episode: e };
        for (key, field) in [("title", Field::EpisodeTitle), ("plot", Field::Plot)] {
            if let Some(text) = string(o, key) {
                let out = g.text(&subj, field, &text, policy, now);
                set(o, key, out, false);
            }
        }
    } else if let (Some(key), Some(t)) = (title_key, title.as_deref()) {
        let vod = matches!(kind, Some("movie" | "series"));
        let subj = if vod {
            Subject::Vod { title: t, series_norm: series }
        } else {
            Subject::Programme { title: t, sub_title: sub.as_deref(), categories: &categories, season_episode: se, start: 0 }
        };
        let out = g.text(&subj, Field::Title, t, policy, now);
        set(o, if key == "title" { "title" } else { "name" }, out, true);
        if let (Some(k), Some(s)) = (sub_key, sub.as_deref()) {
            let out = g.text(&subj, Field::SubTitle, s, policy, now);
            set(o, if k == "subtitle" { "subtitle" } else { "sub_title" }, out, false);
        }
        for (k, field) in [("description", Field::Description), ("plot", Field::Plot)] {
            if let Some(text) = string(o, k) {
                let out = g.text(&subj, field, &text, policy, now);
                set(o, k, out, false);
            }
        }
    }
    // a channel's programme on now
    if let Some(t) = string(o, "now") {
        let subj = Subject::Programme { title: &t, sub_title: None, categories: &[], season_episode: None, start: 0 };
        let out = g.text(&subj, Field::Title, &t, policy, now);
        set(o, "now", out, true);
    }
    if hidden.is_empty() && !rewritten {
        return;
    }
    let mut note = json!({ "hidden": hidden });
    if let Some(r) = reason {
        note["rule"] = Value::String(r.rule);
        note["reason"] = Value::String(r.kind.as_str().into());
        if let Some(u) = r.until.filter(|u| *u != Timestamp::MAX) {
            note["until"] = Value::String(crate::agent::proto::fmt_time(u));
        }
    } else if rewritten {
        note["reason"] = Value::String("scores".into());
    }
    o.insert("spoiler".into(), note);
}
