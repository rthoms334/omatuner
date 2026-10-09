//! Async Xtream client. Owner: WP-04.
//!
//! Base URL is exactly what the user typed (scheme://host[:port]); `server_info.url` is never
//! used for requests. All requests carry the source User-Agent. Failed login is any of: HTTP
//! 401/403, empty body, `[]`, `user_info.auth == 0` → `Error::AuthRejected`; status Expired /
//! Banned / Disabled → `Error::AccountUnavailable`.

use tokio_util::sync::CancellationToken;

use super::types::*;
use crate::error::Result;
use crate::model::{AccountInfo, HttpHeaders, XtreamCredentials};
use crate::sources::http::HttpClient;

#[derive(Clone)]
pub struct XtreamClient {
    http: HttpClient,
    creds: XtreamCredentials,
    headers: HttpHeaders,
}

impl XtreamClient {
    pub fn new(http: HttpClient, creds: XtreamCredentials, user_agent: Option<String>) -> Self {
        Self { http, creds, headers: HttpHeaders { user_agent, ..Default::default() } }
    }

    pub fn credentials(&self) -> &XtreamCredentials { &self.creds }

    /// `player_api.php` with no action. Maps to [`AccountInfo`] (status, expiry, connections,
    /// formats, server tz, clock skew vs `now`).
    pub async fn authenticate(&self, now: i64, cancel: &CancellationToken) -> Result<(XtAuth, AccountInfo)> { todo!("WP-04") }

    pub async fn live_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> { todo!("WP-04") }
    /// All live streams in ONE call (no category filter); may be 20–100 MB.
    pub async fn live_streams(&self, cancel: &CancellationToken) -> Result<Vec<XtLiveStream>> { todo!("WP-04") }
    pub async fn vod_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> { todo!("WP-04") }
    pub async fn vod_streams(&self, cancel: &CancellationToken) -> Result<Vec<XtVodStream>> { todo!("WP-04") }
    pub async fn series_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> { todo!("WP-04") }
    pub async fn series(&self, cancel: &CancellationToken) -> Result<Vec<XtSeries>> { todo!("WP-04") }
    pub async fn vod_info(&self, vod_id: i64, cancel: &CancellationToken) -> Result<XtVodInfo> { todo!("WP-04") }
    /// Tries `series_id=` then the `series=` fork spelling when the first returns empty.
    pub async fn series_info(&self, series_id: i64, cancel: &CancellationToken) -> Result<XtSeriesInfo> { todo!("WP-04") }
    /// Fallback now/next for channels without XMLTV data (texts decoded).
    pub async fn short_epg(&self, stream_id: i64, limit: u32, cancel: &CancellationToken) -> Result<Vec<XtEpgListing>> { todo!("WP-04") }
}
