//! Crate-wide error type. FROZEN (skeleton).
//!
//! Contract: every `Display` of an [`Error`] is safe to show in the UI and to log — it MUST
//! NOT contain credentials. Constructors that wrap foreign errors (reqwest, mpv, sqlite) pass
//! the message through [`crate::redact::redact`] first; use [`Error::net`] / [`Error::other`]
//! rather than building variants by hand when the message may contain a URL.

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Network failure (DNS, TLS, timeout, connection refused). Message already redacted.
    #[error("network error: {0}")]
    Network(String),
    /// Non-success HTTP status. `url` is redacted.
    #[error("HTTP {status} from {url}")]
    Http { status: u16, url: String },
    /// Provider rejected the credentials (Xtream `auth:0`, 401/403 on login).
    #[error("login rejected by provider")]
    AuthRejected,
    /// Account exists but is not usable (status Expired / Banned / Disabled).
    #[error("account {status}")]
    AccountUnavailable { status: String },
    /// The server returned HTML / garbage where a playlist, JSON or XML was expected.
    #[error("unexpected response: {0}")]
    BadPayload(String),
    /// Parse error with location information where available.
    #[error("parse error{}: {msg}", .line.map(|l| format!(" at line {l}")).unwrap_or_default())]
    Parse { line: Option<u64>, msg: String },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config error: {0}")]
    Config(String),
    /// The database's schema (`PRAGMA user_version`) is newer than this build's
    /// ([`crate::store::migrations::latest_version`]): opening it is refused, it would lose
    /// data. `opened_by` is the Tuner version that last opened it, when that one recorded it
    /// (`meta.app_version`, DESIGN §21.7). Typed so the startup report can say it in plain words.
    #[error("database schema v{found} is newer than this build (v{supported}){}; refusing to open", .opened_by.as_ref().map(|v| format!(", last opened by Tuner {v}")).unwrap_or_default())]
    NewerSchema { found: i64, supported: i64, opened_by: Option<String> },
    /// mpv returned an error code. Message is mpv's error string.
    #[error("player error: {0}")]
    Mpv(String),
    #[error("cancelled")]
    Cancelled,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Network error from any displayable source; redacts the message.
    pub fn net(e: impl std::fmt::Display) -> Self {
        Error::Network(crate::redact::redact(&e.to_string()).into_owned())
    }
    /// Generic error; redacts the message.
    pub fn other(e: impl std::fmt::Display) -> Self {
        Error::Other(crate::redact::redact(&e.to_string()).into_owned())
    }
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }
    pub fn parse(line: Option<u64>, msg: impl Into<String>) -> Self {
        Error::Parse { line, msg: msg.into() }
    }
    /// True for errors where retrying later may succeed (network, 5xx, timeouts).
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Network(_) => true,
            Error::Http { status, .. } => *status >= 500 || *status == 429 || *status == 408,
            _ => false,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if let Some(status) = e.status() {
            let url = e.url().map(|u| crate::redact::redact_url(u.as_str())).unwrap_or_default();
            Error::Http { status: status.as_u16(), url }
        } else {
            Error::net(e)
        }
    }
}

impl From<libmpv2::Error> for Error {
    fn from(e: libmpv2::Error) -> Self {
        Error::Mpv(crate::redact::redact(&format!("{e:?}")).into_owned())
    }
}
