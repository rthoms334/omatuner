//! Playlist import pipelines (M3U + Xtream). Owner: WP-06. DESIGN §9.1–9.2.
//!
//! M3U:   download (conditional) → `open_decompressed` → `m3u::parse` in spawn_blocking →
//!        convert entries (smart names, identity keys, groups) → batches of 2 000 through
//!        `store::import::write_channels` → `finish_catalogue` → provider EPG sources from
//!        `url-tvg` → `RebuildLibrary`.
//! Xtream: authenticate (store AccountInfo; abort on AuthRejected/AccountUnavailable without
//!        touching the catalogue) → categories → live streams → vod → series (each kind
//!        completes independently; a failed kind is not swept) → provider EPG source
//!        (`xmltv.php`) → `RebuildLibrary`.

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;
use crate::model::SourceId;
use crate::sources::m3u::M3uEntry;
use crate::sources::xtream::types::{XtCategory, XtLiveStream, XtSeries, XtVodStream};
use crate::store::import::{ImportChannel, ImportGroup, ImportMovie, ImportSeries, ImportSummary};

/// Outcome of a refresh: summary + whether the server said 304.
pub struct RefreshResult {
    pub summary: ImportSummary,
    pub not_modified: bool,
    /// Provider guide URLs discovered (M3U `url-tvg`, Xtream `xmltv.php`).
    pub epg_urls: Vec<String>,
}

pub async fn refresh_source(ctx: &JobContext, source: SourceId, force: bool, progress: &Progress, cancel: &CancellationToken) -> Result<RefreshResult> {
    todo!("WP-06")
}

/// Convert one M3U entry. `smart` = smart-name cleanup on. Group keys are returned in
/// `ImportChannel.groups`; `groups_out` collects new `ImportGroup`s (deduped by key).
#[allow(clippy::ptr_arg)] // pushes into groups_out (stub body)
pub fn m3u_entry_to_channel(
    e: &M3uEntry,
    order: i32,
    smart: bool,
    keys: &mut crate::sources::identity::KeyAllocator,
    groups_out: &mut Vec<ImportGroup>,
) -> ImportChannel {
    todo!("WP-06")
}

pub fn xt_live_to_channel(s: &XtLiveStream, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportChannel> { todo!("WP-06") }
pub fn xt_vod_to_movie(s: &XtVodStream, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportMovie> { todo!("WP-06") }
pub fn xt_series_to_series(s: &XtSeries, order: i32, smart: bool, cats: &[XtCategory]) -> Option<ImportSeries> { todo!("WP-06") }
pub fn xt_categories_to_groups(kind: crate::model::ItemKind, cats: &[XtCategory], smart: bool) -> Vec<ImportGroup> { todo!("WP-06") }
