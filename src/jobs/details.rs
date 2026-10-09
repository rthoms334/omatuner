//! Lazy VOD/series detail loading. Owner: WP-06.
//! Details older than 7 days are refetched when the detail page opens; failures keep the
//! list-level data and surface a non-blocking toast.

use tokio_util::sync::CancellationToken;

use super::{JobContext, Progress};
use crate::error::Result;
use crate::model::{MovieId, SeriesId};

pub async fn load_movie(ctx: &JobContext, id: MovieId, progress: &Progress, cancel: &CancellationToken) -> Result<()> { todo!("WP-06") }

pub async fn load_series(ctx: &JobContext, id: SeriesId, progress: &Progress, cancel: &CancellationToken) -> Result<()> { todo!("WP-06") }
