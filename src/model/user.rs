//! User data: favourites, overrides, progress, history, reminders. FROZEN (skeleton).
//! All keyed by [`ItemRef`] so it survives playlist refreshes (DESIGN §8).

use serde::{Deserialize, Serialize};

use super::{FavListId, Fingerprint, FollowRuleId, GroupKey, ItemKind, ItemRef, ReminderId, SourceId, Timestamp};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavouriteList {
    pub id: FavListId,
    pub name: String,
    /// Nerd Font glyph.
    pub icon: String,
    pub sort_index: i32,
    /// The default list ("Favourites") cannot be deleted; `Ctrl+F` toggles membership in it.
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FavouriteEntry {
    pub list: FavListId,
    pub item: ItemRef,
    pub position: i32,
    pub added_at: Timestamp,
    /// Captured at add-time for reattach.
    pub fingerprint: Fingerprint,
    /// Set when the item vanished on refresh and could not be reattached.
    pub orphaned_since: Option<Timestamp>,
}

/// Per-item user customisation. Absent fields = no override.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemOverride {
    pub rename: Option<String>,
    pub hidden: bool,
    pub locked: bool,
    /// Manual order inside its group (reorder).
    pub user_order: Option<i32>,
    /// Per-channel catch-up offset in seconds.
    pub catchup_offset_s: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemOverrideRow {
    pub item: ItemRef,
    pub ov: ItemOverride,
    pub fingerprint: Fingerprint,
    pub orphaned_since: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupOverride {
    pub rename: Option<String>,
    pub hidden: bool,
    pub locked: bool,
    pub user_order: Option<i32>,
    /// Pinned to Home as a section.
    pub pinned_home: bool,
    /// Shown although the source's category filter hides its tag (DESIGN §28.3). Cleared by
    /// hiding the group.
    pub shown: bool,
    /// The user's word on explicit content (DESIGN §20.3): `Some(true)` "Mark as explicit",
    /// `Some(false)` "Not explicit", `None` = as detected.
    pub explicit: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupOverrideRow {
    pub source: SourceId,
    pub kind: ItemKind,
    pub key: GroupKey,
    pub ov: GroupOverride,
}

/// A group by its stable identity (bulk hide, Settings ▸ Hidden).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupRef {
    pub source: SourceId,
    pub kind: ItemKind,
    pub key: GroupKey,
}

impl From<&super::Group> for GroupRef {
    fn from(g: &super::Group) -> Self {
        Self { source: g.source_id, kind: g.kind, key: g.key.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchProgress {
    pub item: ItemRef,
    pub position_s: f64,
    pub duration_s: Option<f64>,
    pub updated_at: Timestamp,
    pub finished: bool,
    /// Display title captured at write time (Continue Watching works even if the item vanished).
    pub title: String,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub item: ItemRef,
    pub at: Timestamp,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReminderState {
    Pending,
    Fired,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    pub id: ReminderId,
    pub channel: ItemRef,
    pub channel_name: String,
    pub programme_title: String,
    pub start: Timestamp,
    pub stop: Timestamp,
    /// Seconds before `start` to notify.
    pub lead_s: i64,
    pub state: ReminderState,
    /// Notify, or switch to the channel by itself (migration 0008; older rows: `lead_s ==
    /// SWITCH_LEAD` means switch). Serde defaults: backups from before the batch.
    #[serde(default)]
    pub mode: ReminderMode,
    /// Seconds after `start` the switch happens / playback starts ("skip the build-up").
    #[serde(default)]
    pub offset_s: i64,
    /// Where scheduled playback opens; `None` = `reminders.open_in` (big-features additions).
    #[serde(default)]
    pub open_in: Option<OpenIn>,
    /// The Following rule that made it (migration 0011); `None` = the user's or Claude's own.
    /// Editing its lead or mode detaches it (`follow::on_reminder_edited`).
    #[serde(default)]
    pub rule_id: Option<FollowRuleId>,
}

/// What a reminder does at its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReminderMode {
    /// A notification "starts in 2 min" with Watch (the default).
    #[default]
    Notify,
    /// An automatic switch with the 10 s Cancel toast (DESIGN §30.8); routed by
    /// `reminders::fire_policy` (B1/B6).
    Switch,
}

/// Where scheduled playback (an automatic switch, a Following switch, a news flash, `tuner
/// --play <ref> --popout`) opens (Settings ▸ Reminders "Open scheduled playback in").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OpenIn {
    /// The pinned pop-out on every workspace, never taking keyboard focus (Hyprland; the
    /// in-app mini player elsewhere).
    #[default]
    PopOut,
    /// The full window, raised.
    FullWindow,
    /// Wherever Tuner is now (fullscreen stays fullscreen, the mini player stays mini).
    WhereIAm,
}

/// Why a reminder is removed (`store::user::remove_reminder`, the only delete path, B6): a
/// Following ledger row turns `skipped` when the user removed its reminder, never when its
/// rule did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoveWhy {
    /// Settings ▸ Reminders, the guide's `r` toggle, a Cancel toast, the Live reminders list.
    User,
    /// MCP `cancel_switch` / `reminders cancel`.
    Agent,
    /// The Following engine (rule deleted, match gone).
    Rule,
    /// Pruning (`jobs::maintenance`).
    Maintenance,
}

impl Reminder {
    /// `lead_s` of an automatic switch (DESIGN §30.8): at `start` Tuner switches to the channel
    /// by itself (a Cancel toast [`Reminder::SWITCH_WARN_S`] before) instead of notifying. A
    /// marker in an existing column, not a migration: older builds read it as "default lead"
    /// and simply notify.
    pub const SWITCH_LEAD: i64 = -1;
    /// How long before an automatic switch its Cancel toast shows.
    pub const SWITCH_WARN_S: i64 = 10;

    /// An automatic switch rather than a notification.
    pub fn switches(&self) -> bool {
        self.mode == ReminderMode::Switch || self.lead_s == Self::SWITCH_LEAD
    }

    /// A notification reminder with the defaults of the new columns (mode notify, no offset,
    /// the configured place, no rule).
    pub fn notify(channel: ItemRef, channel_name: String, programme_title: String, start: Timestamp, stop: Timestamp, lead_s: i64) -> Self {
        Self {
            id: ReminderId(0),
            channel,
            channel_name,
            programme_title,
            start,
            stop,
            lead_s,
            state: ReminderState::Pending,
            mode: ReminderMode::Notify,
            offset_s: 0,
            open_in: None,
            rule_id: None,
        }
    }
}

/// Reattach outcome for one orphan candidate (reported in import job results).
#[derive(Debug, Clone, PartialEq)]
pub enum ReattachOutcome {
    /// Primary key still present.
    Unchanged,
    /// Moved to a new key via the named fingerprint step.
    Reattached { new_key: super::ItemKey, via: &'static str },
    /// No unique candidate; user data kept as orphan.
    Orphaned,
}
