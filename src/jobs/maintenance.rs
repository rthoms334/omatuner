//! Library rebuild, pruning, backup/restore jobs. Owner: WP-06.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;
use crate::store::backup::{BackupOptions, RestoreMode, RestoreReport};
use crate::store::library::Library;

/// Build a new `Library` from a fresh reader connection on a blocking thread.
pub async fn rebuild_library(ctx: &JobContext) -> Result<Arc<Library>> { todo!("WP-06") }

/// Prune programmes older than max(keep_past_days, max catch-up days), sweep inactive EPG
/// generations, prune history, `PRAGMA optimize`, `wal_checkpoint(PASSIVE)`.
pub async fn maintenance(ctx: &JobContext, progress: &Progress, cancel: &CancellationToken) -> Result<()> { todo!("WP-06") }

pub async fn export_backup(ctx: &JobContext, path: &Path, opts: BackupOptions) -> Result<()> { todo!("WP-06") }

/// Restore, then submit `RefreshSource` for every restored source.
pub async fn import_backup(ctx: &JobContext, path: &Path, mode: RestoreMode) -> Result<RestoreReport> { todo!("WP-06") }
