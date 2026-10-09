//! Onboarding/source-editor validation and creation. Owner: WP-06.
//!
//! Probe rules: a pasted URL matching `m3u::detect_xtream_url` is upgraded to Xtream; an M3U
//! URL is fetched with a 1 MiB range/limit and sniffed (HTML → "This looks like a web page,
//! not a playlist"; HLS media → offer "Add as single stream"); Xtream does `authenticate`
//! and returns AccountInfo (expired accounts are reported but may still be saved).

use tokio_util::sync::CancellationToken;

use super::{JobContext, ProbeResult, Progress};
use crate::error::Result;
use crate::model::{SourceDraft, SourceId};

pub async fn probe(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<ProbeResult> { todo!("WP-06") }

/// Insert the source and run its first refresh inline (progress continues on the same job).
pub async fn add_source(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<SourceId> { todo!("WP-06") }
