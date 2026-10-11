//! `JobKind::EvaluateFollows`: evaluate every rule against the guide, write the plan, re-arm
//! reminders (DESIGN §33 §5.2–5.4, B7). OWNER: F-AB. Runs in the "features" job lane, never
//! together with `GuideCheck`; yields while any picture is Loading or Zapping.

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;
use crate::follow::FollowReason;

/// Evaluate (a no-op until `follow::ENABLED`).
pub async fn evaluate(_ctx: &JobContext, _reason: FollowReason, _progress: &Progress, _cancel: &CancellationToken) -> Result<()> {
    if !crate::follow::ENABLED {
        return Ok(());
    }
    Ok(())
}
