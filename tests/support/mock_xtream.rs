//! In-process mock Xtream panel (axum) for integration tests. Owner: WP-04.
//!
//! Serves `tests/fixtures/xtream/*.json` for every `player_api.php` action, with switches for
//! the quirks we must survive: auth failure (`auth:0`), expired account, HTML-200 error page,
//! 401/403, slow responses (timeouts), `xmltv.php` (fixtures/xmltv/basic.xml.gz), `get.php`
//! (an M3U built from live_streams.json), and `/live|movie|series|timeshift/...` paths that
//! 302-redirect to local test media (see `media.rs`) so the player can be exercised end to end.
//! Records every request (path + query, credentials included) for assertions.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct MockOptions {
    pub auth: MockAuth,
    /// Return the HTML error page with status 200 for list actions.
    pub html_200: bool,
    /// Delay every response (ms) — timeout tests.
    pub delay_ms: u64,
    /// Where stream paths redirect (from `media::ensure_media`).
    pub media_base: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MockAuth {
    #[default]
    Ok,
    Rejected,
    Expired,
    Http403,
}

pub struct MockXtream {
    /// `http://127.0.0.1:<port>` — use as the Xtream server URL.
    pub base: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Start on an ephemeral port on the current tokio runtime.
pub async fn start(opts: MockOptions) -> MockXtream { todo!("WP-04") }
