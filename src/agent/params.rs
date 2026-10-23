//! Parameters of the batch's tools (DESIGN §31–§35, the skill plan): the action-shaped feature
//! tools and the extra library / health tools. Re-exported by `proto`. Batch base wrote the
//! shapes from the designs; the agent track may refine them (with `tools` and their examples).
//! OWNER: agent track.

use serde::{Deserialize, Serialize};

use super::actions::tool_actions;

// ------------------------------------------------------------------ action-shaped tools

tool_actions!(
    /// `queue` (Up next, DESIGN §32).
    pub enum QueueAction {
        List => ("list", false),
        Add => ("add", true),
        Move => ("move", true),
        Remove => ("remove", true),
        Clear => ("clear", true),
        Play => ("play", true),
        Skip => ("skip", true),
        StopAfter => ("stop_after", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Queue {
    pub action: QueueAction,
    /// add: ≤ 50 item or programme refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    /// add: `end` (default), `next` or an index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_start: Option<bool>,
    /// add: `best` (default) or `this` copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<String>,
    /// move / remove: row ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<i64>>,
    /// move: an index, `top`, `bottom` or `next`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// play / stop_after: a row id (default: the next).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_played: Option<bool>,
    /// clear: default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub played_only: Option<bool>,
    /// play / skip: where it plays, as `play`'s `open_in` (DESIGN §30.13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_in: Option<String>,
}

tool_actions!(
    /// `watchlist` (DESIGN §42 §7).
    pub enum WatchlistAction {
        Lists => ("lists", false),
        Show => ("show", false),
        Next => ("next", false),
        Play => ("play", true),
        Stop => ("stop", true),
        Create => ("create", true),
        Add => ("add", true),
        Move => ("move", true),
        Remove => ("remove", true),
        Skip => ("skip", true),
        Restart => ("restart", true),
        Rename => ("rename", true),
        Delete => ("delete", true),
    }
);

/// Names are matched without guessing when several lists fit; ids are exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WatchlistSelector {
    Id(i64),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchlistPlaceholder {
    pub title: Option<String>,
    pub series_title: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub year: Option<u16>,
    pub kind: crate::model::ItemKind,
    pub tmdb_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchlistSeasons {
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watchlist {
    pub action: WatchlistAction,
    pub list: Option<WatchlistSelector>,
    pub include_done: Option<bool>,
    pub id: Option<i64>,
    pub just_one: Option<bool>,
    pub open_in: Option<String>,
    pub name: Option<String>,
    pub note: Option<String>,
    pub play_order: Option<String>,
    pub refs: Option<Vec<String>>,
    pub placeholders: Option<Vec<WatchlistPlaceholder>>,
    /// `end`, `next`, or a zero-based position.
    #[serde(default, deserialize_with = "watchlist_position")]
    pub at: Option<String>,
    pub seasons: Option<WatchlistSeasons>,
    pub ids: Option<Vec<i64>>,
    /// move: the same positions as `at`.
    #[serde(default, deserialize_with = "watchlist_position")]
    pub to: Option<String>,
    /// skip: false clears a skip; absent toggles it.
    pub skipped: Option<bool>,
}

fn watchlist_position<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => Ok(Some(n.to_string())),
        _ => Err(D::Error::custom("give end, next or a nonnegative index")),
    }
}

tool_actions!(
    /// `follow` (Following, DESIGN §33; news flash included).
    pub enum FollowAction {
        Record => ("record", true),
        Preview => ("preview", false),
        List => ("list", false),
        Add => ("add", true),
        Remove => ("remove", true),
    }
);

/// A rule as the agent describes it (`follow preview` / `add`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowRuleInput {
    pub name: String,
    /// `team`, `show`, `event`, `custom` or `news_flash`.
    pub kind: String,
    #[serde(default)]
    pub terms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with_any: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub without: Vec<String>,
    /// Guide categories (sports), any of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// `live_only`, `live_or_new`, `new_only`, `any`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub airings: Option<String>,
    /// `"any"`, `"favourites"`, `{"list": …}`, `{"playlist": …}`, `{"channels": [refs]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<serde_json::Value>,
    /// `notify`, `switch`, `list`, `flash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_min: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_week: Option<u32>,
    /// News flash: weekdays (`mon`…`sun`; default every day).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<String>>,
    /// News flash: minutes of headlines (absent = the whole programme).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headlines_min: Option<u32>,
    /// News flash: `normal`, `mute`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Follow {
    pub action: FollowAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<FollowRuleInput>,
    /// preview / remove: a rule id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_id: Option<i64>,
    /// preview: 1–14 (default 14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    /// remove: keep its reminders (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_reminders: Option<bool>,
}

tool_actions!(
    /// `spoiler_guard` (DESIGN §34). Pausing stays in the app.
    pub enum SpoilerAction {
        Status => ("status", false),
        List => ("list", false),
        AddTopic => ("add_topic", true),
        AddSeries => ("add_series", true),
        Blackout => ("blackout", true),
        Remove => ("remove", true),
        MarkSeen => ("mark_seen", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpoilerGuard {
    pub action: SpoilerAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    /// `until_seen` or `always`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_h: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_ref: Option<String>,
    /// blackout / mark_seen: a programme or episode ref.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
}

tool_actions!(
    /// `guide_fix` (Guide fixes, DESIGN §31).
    pub enum GuideFixAction {
        Issues => ("issues", false),
        Propose => ("propose", false),
        Apply => ("apply", true),
        Undo => ("undo", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuideFix {
    pub action: GuideFixAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// issues: one channel (adds candidates).
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// propose: `[{ref, guide?, shift_min?, catchup_offset_min?, reset?, dismiss?}]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<serde_json::Value>,
    /// propose: `{kinds[], min_confidence?, playlist?}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<serde_json::Value>,
    /// propose: `[{playlist, timeshift_siblings?, share_copies?, catchup_offset_min?}]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    /// apply: must be true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_id: Option<i64>,
}

tool_actions!(
    /// `multiview` (DESIGN §35).
    pub enum MultiViewAction {
        Open => ("open", true),
        Swap => ("swap", true),
        Close => ("close", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiView {
    pub action: MultiViewAction,
    /// open: 1–2 channel / programme refs (one = alongside what plays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    /// `side_by_side`, `big_small`, `pip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// open: index into refs of the picture with sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_start: Option<bool>,
    /// open: allow moving the playing picture to another copy (only after asking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rearrange: Option<bool>,
    /// swap: `sound`, `places`, `focus`, `layout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what: Option<String>,
    /// close: `a`, `b` or `sound` (default) — the picture kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
}

tool_actions!(
    /// `reminders` (pass 2; also covers `schedule_switch`).
    pub enum RemindersAction {
        List => ("list", false),
        Add => ("add", true),
        Cancel => ("cancel", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reminders {
    pub action: RemindersAction,
    /// add: a programme ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub programme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_min: Option<i32>,
    /// `notify` (default) or `switch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `popout`, `full_window`, `where_i_am`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_in: Option<String>,
    /// cancel: the reminder id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
}

tool_actions!(
    /// `sleep_timer` (in the app, so it outlives the agent's session).
    pub enum SleepAction {
        Status => ("status", false),
        Set => ("set", true),
        Cancel => ("cancel", true),
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SleepTimer {
    pub action: SleepAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_of_programme: Option<bool>,
    /// Queue items while Up next drives playback, else episodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_of_queue: Option<bool>,
    /// `stop` (default; every picture) or `pause`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
}

// ------------------------------------------------------------------ library and health tools

/// One title the agent found on the web (`match_titles`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleQuery {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    /// `movie` or `series`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchTitles {
    /// ≤ 100.
    pub titles: Vec<TitleQuery>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<super::proto::Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Episodes {
    pub series_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_spoilers: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FavouritesEdit {
    /// The list (name); or `create` a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// `lock` can only add locks: no unlock, PIN or explicit-content parameter exists.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// Lock the parental session now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_now: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnose {
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actions::assert_wire_names;

    #[test]
    fn verb_names_match_the_wire() {
        assert_wire_names::<QueueAction>();
        assert_wire_names::<WatchlistAction>();
        assert_wire_names::<FollowAction>();
        assert_wire_names::<SpoilerAction>();
        assert_wire_names::<GuideFixAction>();
        assert_wire_names::<MultiViewAction>();
        assert_wire_names::<RemindersAction>();
        assert_wire_names::<SleepAction>();
    }
}

// Downloads and recordings (DESIGN §43 §7).
tool_actions!(pub enum DownloadsAction {
    List => ("list", false), Add => ("add", true), Series => ("series", true), Record => ("record", true),
    Pause => ("pause", true), Resume => ("resume", true), Cancel => ("cancel", true), Delete => ("delete", true), Retry => ("retry", true)
});
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Downloads {
    pub action: DownloadsAction,
    pub state: Option<String>,
    pub refs: Option<Vec<String>>,
    #[serde(rename = "ref")]
    pub item: Option<String>,
    pub channel: Option<String>,
    pub minutes: Option<u32>,
    pub copy: Option<String>,
    pub scope: Option<String>,
    pub n: Option<u32>,
    pub season: Option<u32>,
    pub ids: Option<Vec<i64>>,
}

// New episodes (DESIGN §44 §7). `list` and `tracked` only read; `seen` and `mute` change what the
// user sees and need their yes (RULES.md).
tool_actions!(pub enum NewEpisodesAction {
    List => ("list", false), Tracked => ("tracked", false), Seen => ("seen", true), Mute => ("mute", true)
});

fn list_action() -> NewEpisodesAction {
    NewEpisodesAction::List
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEpisodes {
    #[serde(default = "list_action")]
    pub action: NewEpisodesAction,
    /// list: a time or "7d" / "12h"; default: everything not cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// list: also episodes watched, dismissed or expired in the last 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_cleared: Option<bool>,
    /// list / tracked: series, 1-50, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoiler_safe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_spoilers: Option<Vec<String>>,
    /// seen / mute: a series ref from list or tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_ref: Option<String>,
    /// seen: episode refs from list's `new` (at most 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    /// seen: every new episode on Home ("clear my new episodes").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    /// mute: true (default) stops alerts for the series, false allows them again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}
