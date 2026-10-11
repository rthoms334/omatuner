//! Xtream Codes API (and XUI.one / forks). Owner: WP-04. Spec: protocols.md §2.
//!
//! * `types`  — lenient serde models (numbers-as-strings, `""`/`null`/`[]` empties, `info: []`,
//!   episodes as object-or-array, base64 EPG text).
//! * `client` — async `player_api.php` calls; big lists downloaded to bytes and parsed in
//!   `spawn_blocking`.
//! * `detect` — is an M3U an Xtream login? (stream-URL shapes, whole-playlist scan; §27.2)
//! * `urls`   — stream / catch-up / xmltv / m3u URL builders (credentials live only here and in
//!   the store; never logged).
//! * `convert` lives in `jobs::import_source` (types → `store::import::*`).

pub mod client;
pub mod detect;
pub mod types;
pub mod urls;

pub use client::XtreamClient;
