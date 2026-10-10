//! Credential redaction. Implemented in the skeleton (security-critical, used everywhere).
//! Owner for extensions/tests: WP-08.
//!
//! Rules (DESIGN §20):
//! 1. Every registered secret (Xtream username/password, M3U URL query values, header values)
//!    is replaced by `***` wherever it appears — verbatim and in its percent-encoded forms
//!    (`p@ss word` also matches `p%40ss%20word` and `p%40ss+word`).
//! 2. URLs: userinfo (`user:pass@`) is masked; query parameters named like credentials
//!    (`username`, `password`, `pass`, `token`, `auth`, `key`, `sig`...) are masked; Xtream
//!    path forms `/live|movie|series|timeshift/<u>/<p>/...` mask the two segments, and so does
//!    the bare Xtream live form `scheme://host/<u>/<p>/<stream id>[.ext]` that panel playlists
//!    use.
//! 3. Credential-bearing HTTP headers (`Authorization`, `Cookie`, `X-Api-Key`...) keep their
//!    name and lose their value — in log lines and in mpv's `http-header-fields` lists.
//! 4. JSON members named like credentials (`"password":"…"`) are masked (provider responses
//!    quoted in errors carry `user_info.password`).
//! 5. [`Secret<T>`] never prints its content via `Debug`/`Display`; use [`Secret::expose`].
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
    pub fn new(v: T) -> Self {
        Secret(v)
    }
    /// Access the secret value. Grep for `expose(` when auditing.
    pub fn expose(&self) -> &T {
        &self.0
    }
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> std::fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Secret<String> {
    /// Redacted rendering suitable for UI/logs.
    pub fn redacted(&self) -> String {
        redact_url(&self.0)
    }
}

impl From<String> for Secret<String> {
    fn from(s: String) -> Self {
        Secret(s)
    }
}

static SECRETS: LazyLock<RwLock<Vec<String>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Shortest secret we register; shorter values would mangle normal text.
const MIN_SECRET_LEN: usize = 3;

/// Register a secret string (min length 3; shorter values would mangle normal text).
/// Called when sources are loaded/created and when headers are parsed. The percent-encoded
/// spellings of the value are registered too, so a password with `@`, `/` or spaces is also
/// masked inside URLs.
pub fn register_secret(s: &str) {
    let s = s.trim();
    if s.len() < MIN_SECRET_LEN {
        return;
    }
    let mut v = SECRETS.write().unwrap_or_else(|e| e.into_inner());
    let mut added = false;
    for form in [s.to_owned(), percent_encode(s), url::form_urlencoded::byte_serialize(s.as_bytes()).collect()] {
        if !v.contains(&form) {
            v.push(form);
            added = true;
        }
    }
    if added {
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

/// RFC 3986 percent-encoding of everything but unreserved characters (upper-case hex, the form
/// `url` and most HTTP stacks produce).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Query/form/JSON member names whose values are credentials.
const CRED_NAMES: &str = "username|user|password|passwd|pass|pwd|token|auth|key|sig|signature|apikey|api_key|\
                          access_token|refresh_token|id_token|client_secret|secret";

static USERINFO: LazyLock<Regex> =
    // The password runs to the last `@` before the path: an unencoded `@` inside it must not
    // leave the rest readable.
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)([^/@\s:]+)(:[^/\s]*)?@").unwrap());
/// `?name=value`, `&name=value`, `;name=value` and free-standing `name=value` (form bodies,
/// mpv option strings). The value stops at separators and quotes.
static QUERY_CRED: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r#"(?i)((?:[?&;]|\b)(?:{CRED_NAMES})=)([^&#\s"'<>]*)"#)).unwrap());
static XTREAM_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(/(?:live|movie|series|timeshift)/)([^/\s]+)/([^/\s]+)/").unwrap());
/// Xtream panels' M3U output: `http://host:port/<user>/<pass>/<stream id>[.ext]`.
static XTREAM_BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(https?://[^/\s"'<>]+/)([^/\s?#"'<>]+)/([^/\s?#"'<>]+)/(\d+(?:\.[a-z0-9]{2,5})?)($|[\s?#"'<>,;)])"#).unwrap()
});
static HEADER_CRED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        // Separators: `Name: v` / `Name=v`, and the quoted forms `"Name": "v"` (JSON object)
        // and `"Name", "v"` (`Debug`/serde of a `(name, value)` header pair).
        r#"(?i)\b(authorization|proxy-authorization|cookie|set-cookie|x-api-key|x-auth-token|x-access-token)("\s*[:,]\s*"|\s*[:=]\s*)([^\r\n,"']+)"#,
    )
    .unwrap()
});
static JSON_CRED: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r#"(?i)("(?:{CRED_NAMES})"\s*:\s*")((?:[^"\\]|\\.)*)""#)).unwrap());

/// Redact credentials from free text (log lines, error messages, mpv messages).
pub fn redact(s: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(s);
    // Registered secrets first: a pattern below could otherwise mask only part of one (e.g. a
    // password containing `key=`) and leave the rest readable.
    {
        let secrets = SECRETS.read().unwrap_or_else(|e| e.into_inner());
        for sec in secrets.iter() {
            if out.contains(sec.as_str()) {
                out = Cow::Owned(out.replace(sec.as_str(), "***"));
            }
        }
    }
    if USERINFO.is_match(&out) {
        out = Cow::Owned(USERINFO.replace_all(&out, "${1}***@").into_owned());
    }
    if QUERY_CRED.is_match(&out) {
        out = Cow::Owned(QUERY_CRED.replace_all(&out, "${1}***").into_owned());
    }
    if XTREAM_PATH.is_match(&out) {
        out = Cow::Owned(XTREAM_PATH.replace_all(&out, "${1}***/***/").into_owned());
    }
    if XTREAM_BARE.is_match(&out) {
        out = Cow::Owned(XTREAM_BARE.replace_all(&out, "${1}***/***/${4}${5}").into_owned());
    }
    if HEADER_CRED.is_match(&out) {
        out = Cow::Owned(HEADER_CRED.replace_all(&out, "${1}${2}***").into_owned());
    }
    if JSON_CRED.is_match(&out) {
        out = Cow::Owned(JSON_CRED.replace_all(&out, "${1}***\"").into_owned());
    }
    out
}

/// Redact a URL for display (same rules as [`redact`]).
pub fn redact_url(url: &str) -> String {
    redact(url).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_userinfo_query_and_xtream_paths() {
        assert_eq!(redact_url("http://bob:hunter2@h.tv/x"), "http://***@h.tv/x");
        // an unencoded `@` in the password: masked up to the last one before the host
        assert_eq!(redact_url("http://bob:p@ss@h.tv/list.m3u"), "http://***@h.tv/list.m3u");
        assert_eq!(redact_url("see http://bob@h.tv/a and mail@example.com"), "see http://***@h.tv/a and mail@example.com");
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

    #[test]
    fn bare_xtream_live_urls_lose_both_segments() {
        assert_eq!(redact_url("http://h.tv:8080/alice/s3cr3t/12345"), "http://h.tv:8080/***/***/12345");
        assert_eq!(redact_url("Opening http://h.tv/alice/s3cr3t/12345.m3u8 now"), "Opening http://h.tv/***/***/12345.m3u8 now");
        // ordinary paths without a final numeric stream id stay readable
        assert_eq!(redact_url("https://i.imgur.com/a/b/logo.png"), "https://i.imgur.com/a/b/logo.png");
        assert_eq!(redact_url("https://news.tv/2024/10/12/story"), "https://news.tv/2024/10/12/story");
    }

    #[test]
    fn headers_keep_their_name_and_lose_their_value() {
        assert_eq!(redact("Authorization: Bearer abc.def.ghi"), "Authorization: ***");
        assert_eq!(redact("http-header-fields=Cookie: sid=1; x=2,User-Agent: Tuner/1.0"), "http-header-fields=Cookie: ***,User-Agent: Tuner/1.0");
        assert_eq!(redact("x-api-key=0123456789"), "x-api-key=***");
    }

    #[test]
    fn json_members_and_form_bodies() {
        assert_eq!(
            redact(r#"{"user_info":{"username":"bob","password":"hu\"nter2","status":"Active"}}"#),
            r#"{"user_info":{"username":"***","password":"***","status":"Active"}}"#
        );
        assert_eq!(redact("body: username=bob&password=hunter2"), "body: username=***&password=***");
        // field names that merely end in a credential word are not touched
        assert_eq!(redact("sort_key=name"), "sort_key=name");
    }

    #[test]
    fn percent_encoded_secret_forms_are_masked() {
        register_secret("p@ss w/rd-unit-7f3a");
        assert_eq!(redact("u=p%40ss%20w%2Frd-unit-7f3a"), "u=***");
        assert_eq!(redact("u=p%40ss+w%2Frd-unit-7f3a"), "u=***");
        assert_eq!(redact("raw p@ss w/rd-unit-7f3a here"), "raw *** here");
    }

    #[test]
    fn secrets_are_masked_whole_before_patterns_run() {
        register_secret("pre;key=fix-unit-2b");
        assert_eq!(redact("pw pre;key=fix-unit-2b end"), "pw *** end");
    }

    #[test]
    fn short_secrets_are_ignored_and_clean_text_is_borrowed() {
        register_secret("ab");
        assert_eq!(redact("abc ab"), "abc ab");
        assert!(matches!(redact("nothing to see here"), Cow::Borrowed(_)));
    }

    #[test]
    fn percent_encode_matches_rfc3986_unreserved() {
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a b@c/é"), "a%20b%40c%2F%C3%A9");
    }
}
