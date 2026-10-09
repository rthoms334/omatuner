//! Credential redaction. Implemented in the skeleton (security-critical, used everywhere).
//! Owner for extensions/tests: WP-08.
//!
//! Rules (DESIGN §20):
//! 1. Every registered secret (Xtream username/password, M3U URL query values, header values)
//!    is replaced by `***` wherever it appears.
//! 2. URLs: userinfo (`user:pass@`) is masked; query parameters named like credentials
//!    (`username`, `password`, `pass`, `token`, `auth`, `key`, `sig`...) are masked; Xtream
//!    path forms `/live|movie|series|timeshift/<u>/<p>/...` mask the two segments.
//! 3. [`Secret<T>`] never prints its content via `Debug`/`Display`; use [`Secret::expose`].
//!
//! The tracing layer, the log viewer, error constructors, mpv log forwarding and the stream
//! info panel all go through [`redact`]. `cargo test redact` must stay green.

use std::borrow::Cow;
use std::sync::{LazyLock, RwLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A value that must never be logged. Serializes transparently (DB/backup need the value).
#[derive(Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(v: T) -> Self { Secret(v) }
    /// Access the secret value. Grep for `expose(` when auditing.
    pub fn expose(&self) -> &T { &self.0 }
    pub fn into_inner(self) -> T { self.0 }
}

impl<T> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Secret(***)") }
}

impl Secret<String> {
    /// Redacted rendering suitable for UI/logs.
    pub fn redacted(&self) -> String { redact_url(&self.0) }
}

impl From<String> for Secret<String> {
    fn from(s: String) -> Self { Secret(s) }
}

static SECRETS: LazyLock<RwLock<Vec<String>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a secret string (min length 3; shorter values would mangle normal text).
/// Called when sources are loaded/created and when headers are parsed.
pub fn register_secret(s: &str) {
    let s = s.trim();
    if s.len() < 3 {
        return;
    }
    let mut v = SECRETS.write().unwrap_or_else(|e| e.into_inner());
    if !v.iter().any(|x| x == s) {
        v.push(s.to_owned());
        // longest first so overlapping secrets are fully masked
        v.sort_by_key(|x| std::cmp::Reverse(x.len()));
    }
}

/// Forget all registered secrets (source deletion re-registers the rest). NEVER call it from
/// tests: the registry is process-global and tests in one binary run in parallel — use a
/// unique secret per test instead.
pub fn clear_secrets() {
    SECRETS.write().unwrap_or_else(|e| e.into_inner()).clear();
}

static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)([^/@\s:]+)(:[^/@\s]*)?@").unwrap());
static QUERY_CRED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([?&;](?:username|user|password|pass|pwd|token|auth|key|sig|signature|apikey|api_key|access_token)=)([^&#\s]*)")
        .unwrap()
});
static XTREAM_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(/(?:live|movie|series|timeshift)/)([^/\s]+)/([^/\s]+)/").unwrap()
});

/// Redact credentials from free text (log lines, error messages, mpv messages).
pub fn redact(s: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(s);
    if USERINFO.is_match(&out) {
        out = Cow::Owned(USERINFO.replace_all(&out, "${1}***@").into_owned());
    }
    if QUERY_CRED.is_match(&out) {
        out = Cow::Owned(QUERY_CRED.replace_all(&out, "${1}***").into_owned());
    }
    if XTREAM_PATH.is_match(&out) {
        out = Cow::Owned(XTREAM_PATH.replace_all(&out, "${1}***/***/").into_owned());
    }
    let secrets = SECRETS.read().unwrap_or_else(|e| e.into_inner());
    for sec in secrets.iter() {
        if out.contains(sec.as_str()) {
            out = Cow::Owned(out.replace(sec.as_str(), "***"));
        }
    }
    out
}

/// Redact a URL for display (same rules as [`redact`]).
pub fn redact_url(url: &str) -> String { redact(url).into_owned() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_userinfo_query_and_xtream_paths() {
        assert_eq!(redact_url("http://bob:hunter2@h.tv/x"), "http://***@h.tv/x");
        assert_eq!(
            redact_url("http://h.tv:8080/get.php?username=bob&password=hunter2&type=m3u_plus"),
            "http://h.tv:8080/get.php?username=***&password=***&type=m3u_plus"
        );
        assert_eq!(redact_url("http://h.tv/live/bob/hunter2/123.ts"), "http://h.tv/live/***/***/123.ts");
        assert_eq!(
            redact_url("http://h.tv/timeshift/bob/hunter2/60/2026-10-09:20-00/123.ts"),
            "http://h.tv/timeshift/***/***/60/2026-10-09:20-00/123.ts"
        );
    }

    #[test]
    fn secret_debug_is_masked() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "Secret(***)");
    }
}
