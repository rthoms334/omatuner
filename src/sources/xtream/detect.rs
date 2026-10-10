//! Is this M3U an Xtream login? (DESIGN §27.2). Owner: P5 sources track.
//!
//! Two signals, both pure (no I/O):
//! * the playlist URL is `…/get.php?username=…&password=…` (`m3u::detect_xtream_url`);
//! * the entries are Xtream stream URLs,
//!   `scheme://host[:port][/prefix]/{live|movie|series}/<user>/<pass>/<id>[.<ext>]`
//!   ([`parse_stream_url`]), and the whole playlist is one account ([`PlaylistScan`]).
//!
//! Credentials found here are registered with `redact::register_secret` by the caller before
//! anything is logged; nothing in this module logs.

use crate::model::{XtreamCredentials, XtreamStreamKind};

/// One Xtream stream URL taken apart.
#[derive(Debug, Clone, PartialEq)]
pub struct XtreamStreamUrl {
    /// `server` = `scheme://host[:port][/prefix]` (host lowercased, no trailing `/`); user and
    /// password percent-decoded.
    pub creds: XtreamCredentials,
    pub kind: XtreamStreamKind,
    pub stream_id: i64,
    /// Container extension without the dot (`ts`, `m3u8`, `mkv`…), if the URL has one.
    pub ext: Option<String>,
}

/// Parse `scheme://host[:port][/prefix]/{live|movie|series}/<user>/<pass>/<id>[.<ext>]`
/// (http/https only; `<id>` all digits, > 0; the kind segment is the fourth from the end and
/// matched case-insensitively; query and fragment ignored). `timeshift/…` and the short live
/// form `host/<user>/<pass>/<id>` are NOT matched (§27.2).
pub fn parse_stream_url(url: &str) -> Option<XtreamStreamUrl> {
    let _ = url;
    todo!("P5-sources")
}

/// Whole-playlist detection (§27.2). Feed every entry URL in file order with [`push`]; stop
/// early when it returns `false`. The playlist is one Xtream account when it has at least one
/// stream URL, all of them carry the same username + password, and every other entry is
/// served by a host that also serves stream URLs. The API is then tried on the most frequent
/// `server` (first seen on ties).
///
/// [`push`]: PlaylistScan::push
#[derive(Debug, Default)]
pub struct PlaylistScan {
    // P5-sources: per-server counts, the login, the hosts of non-stream entries.
}

impl PlaylistScan {
    /// Account for one entry URL. `false` once the playlist cannot be one Xtream account.
    pub fn push(&mut self, url: &str) -> bool {
        let _ = url;
        todo!("P5-sources")
    }

    /// The login to try, if the playlist qualified.
    pub fn finish(self) -> Option<XtreamCredentials> {
        todo!("P5-sources")
    }
}
