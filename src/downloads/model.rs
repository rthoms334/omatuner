//! Stored jobs and bounded progress snapshots (DESIGN §43 §4–§5).
use crate::model::{DownloadId, ItemKey, ItemKind, ItemRef, SourceId, Timestamp};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Movie,
    Episode,
    Live,
    Catchup,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Episode => "episode",
            Self::Live => "live",
            Self::Catchup => "catchup",
        }
    }
    pub fn recording(self) -> bool {
        matches!(self, Self::Live | Self::Catchup)
    }
    pub fn item_kind(self) -> ItemKind {
        match self {
            Self::Movie => ItemKind::Movie,
            Self::Episode => ItemKind::Episode,
            _ => ItemKind::Channel,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Scheduled,
    Waiting,
    Running,
    Finishing,
    Paused,
    Done,
    Partial,
    Failed,
    Missed,
    Cancelled,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Finishing => "finishing",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Missed => "missed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn active(self) -> bool {
        matches!(self, Self::Running | Self::Finishing)
    }
    pub fn playable(self) -> bool {
        matches!(self, Self::Done | Self::Partial)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: DownloadId,
    pub position: i64,
    pub kind: Kind,
    pub source_id: Option<SourceId>,
    pub item_key: Option<ItemKey>,
    pub any_copy: bool,
    pub prog_start: Option<Timestamp>,
    pub prog_stop: Option<Timestamp>,
    pub pad_before_s: u32,
    pub pad_after_s: u32,
    pub catchup_id: Option<String>,
    pub title: String,
    pub subtitle: Option<String>,
    pub state: State,
    pub reason: Option<String>,
    pub store_name: Option<String>,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub attempts: u32,
    pub added_at: Timestamp,
    pub added_by: String,
    pub done_at: Option<Timestamp>,
    pub watched_at: Option<Timestamp>,
    /// Fingerprint plus retained display/guard metadata for a deleted playlist.
    pub fp: serde_json::Value,
    pub orphaned_since: Option<Timestamp>,
}
impl Job {
    pub fn matches(&self, item: &ItemRef) -> bool {
        self.item().as_ref() == Some(item)
            || self
                .fp
                .get("download_copies")
                .and_then(|v| serde_json::from_value::<Vec<ItemRef>>(v.clone()).ok())
                .is_some_and(|items| items.contains(item))
    }
    pub fn item(&self) -> Option<ItemRef> {
        Some(ItemRef { source: self.source_id?, kind: self.kind.item_kind(), key: self.item_key.clone()? })
    }
    pub fn start(&self) -> Option<Timestamp> {
        self.prog_start.map(|s| s.saturating_sub(i64::from(self.pad_before_s)))
    }
    pub fn stop(&self) -> Option<Timestamp> {
        self.prog_stop.map(|s| s.saturating_add(i64::from(self.pad_after_s)))
    }
    pub fn progress(&self) -> Option<f64> {
        if self.state == State::Finishing {
            return self.fp.get("download_finishing").and_then(serde_json::Value::as_f64);
        }
        self.bytes_total.filter(|n| *n > 0).map(|n| (self.bytes_done as f64 / n as f64).clamp(0.0, 1.0))
    }
    pub fn explicit(&self) -> bool {
        self.fp.get("download_explicit").and_then(serde_json::Value::as_bool).unwrap_or(false)
    }
    pub fn path(&self, root: &std::path::Path) -> Option<PathBuf> {
        let name = self.store_name.as_deref()?;
        let prefix = self.id.to_string();
        if name == format!("{prefix}/") {
            return Some(root.join(prefix));
        }
        let (number, extension) = name.split_once('.')?;
        (number == prefix && !extension.is_empty() && extension.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| root.join(name))
    }
}

#[derive(Debug, Clone, Default)]
pub struct DownloadsSnapshot {
    pub jobs: Vec<Job>,
    pub rules: Vec<KeepRule>,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub cap_bytes: u64,
    pub speed_bps: f64,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct KeepRule {
    pub id: i64,
    pub item: ItemRef,
    pub title: String,
    pub keep: u32,
}

/// The guide and item menus share one add path; a bare channel supplies `minutes` instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub item: ItemRef,
    pub programme: Option<crate::queue::Slot>,
    pub minutes: Option<u32>,
    pub any_copy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesScope {
    Next(u32),
    Season(u32),
    All,
    Keep(u32),
}

#[derive(Debug, Clone)]
pub enum DownloadCommand {
    Add {
        targets: Vec<Target>,
        agent: bool,
    },
    Series {
        item: ItemRef,
        scope: SeriesScope,
        agent: bool,
    },
    Pause(Vec<DownloadId>),
    Resume(Vec<DownloadId>),
    Retry(Vec<DownloadId>),
    Cancel(Vec<DownloadId>),
    Delete(Vec<DownloadId>),
    Undo(Vec<DownloadId>),
    DeleteRule(i64),
    UndoRule(i64),
    Move {
        id: DownloadId,
        to: usize,
    },
    Pin {
        id: DownloadId,
        item: ItemRef,
    },
    PauseAll(bool),
    PauseForPlay(SourceId),
    /// The recording-due prompt. No answer keeps watching after sixty seconds.
    Clash {
        id: DownloadId,
        record: bool,
        alternate: bool,
    },
    StopRecording(DownloadId),
    MoveFolder(PathBuf),
    Refresh,
    Shutdown,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadReply {
    pub added: Vec<DownloadId>,
    pub refused: Vec<(ItemRef, String)>,
    pub size_bytes: u64,
    pub rule: Option<i64>,
    pub error: Option<String>,
}
