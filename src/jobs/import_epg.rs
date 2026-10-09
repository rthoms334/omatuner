//! XMLTV import pipeline. Owner: WP-06. DESIGN §9.3.
//!
//! download (conditional, to `~/.cache/tuner/epg/<id>.xml[.gz]`) → `open_decompressed` →
//! `xmltv::parse` on a blocking thread feeding a bounded channel (cap 4 batches) →
//! async consumer submits `write_epg_channels` / `write_programmes` batches of 5 000 to the
//! writer → `finish_epg_import` → re-match this EPG source against all playlist sources
//! (`epg_match::match_all` + `replace_auto_mappings` + `resolve_mappings`) →
//! `sweep_old_generations` in chunks → `DataScope::Epg`.
//! Back-pressure: the parser blocks when the channel is full, so memory stays bounded
//! (~4 × 5 000 programmes) regardless of file size. Target: 415k programmes < 8 s total.

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;
use crate::model::EpgSourceId;
use crate::store::epg::EpgImportSummary;

pub struct EpgRefreshResult {
    pub summary: EpgImportSummary,
    pub not_modified: bool,
}

pub async fn refresh_epg(ctx: &JobContext, id: EpgSourceId, force: bool, progress: &Progress, cancel: &CancellationToken) -> Result<EpgRefreshResult> {
    todo!("WP-06")
}

/// Re-run automatic matching of every enabled EPG source against every playlist source.
pub async fn rematch_all(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<u32> { todo!("WP-06") }
