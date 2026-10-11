//! `JobKind::GuideCheck`: probes, fuzzy suggestions and the classification cache after a guide
//! or playlist import (DESIGN §31 §4.2, B7; debounced 30 s, skipped headless). OWNER: G-core.
//! Rule-row derivation runs in `rematch_all` (B7), not here. Same job lane as
//! `EvaluateFollows`; ≤ 2 threads; pauses while any picture is Loading or Zapping.

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;

/// Run the check (a no-op until `guide_fix::ENABLED`).
pub async fn run(_ctx: &JobContext, _progress: &Progress, _cancel: &CancellationToken) -> Result<()> {
    if !crate::guide_fix::ENABLED {
        return Ok(());
    }
    Ok(())
}
