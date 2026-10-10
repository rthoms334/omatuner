//! Async Xtream client. Owner: WP-04.
//!
//! Base URL is exactly what the user typed (scheme://host[:port]); `server_info.url` is never
//! used for requests. All requests carry the source User-Agent. Failed login is any of: HTTP
//! 401/403, empty body, `[]`, `user_info.auth == 0` → `Error::AuthRejected`; status Expired /
//! Banned / Disabled → `Error::AccountUnavailable`.
//!
//! Every list action goes through [`LenientList`] (array, id-keyed object or `{}` → `Vec`,
//! malformed elements skipped); a catalogue reply that is no list at all (empty body, `null`,
//! `{"error":…}`) is `Error::BadPayload`, never an empty catalogue, so an outage can't sweep the
//! user's lists (DESIGN §7.2). The login object in place of data (`{"user_info":…}`) maps to
//! `AuthRejected` / `AccountUnavailable` / `BadPayload` by its contents. Detail actions treat a
//! non-object reply as "no data". Errors are scrubbed of this client's username/password (raw
//! and percent-encoded) on top of the generic redaction, so they are safe even before the
//! secrets are registered.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::types::*;
use super::urls;
use crate::error::{Error, Result};
use crate::model::{AccountInfo, AccountStatus, HttpHeaders, XtreamCredentials};
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
    pub async fn authenticate(&self, now: i64, cancel: &CancellationToken) -> Result<(XtAuth, AccountInfo)> {
        let url = urls::player_api_url(&self.creds, None, &[]);
        let v: Value = match self.http.get_json(&url, &self.headers, cancel).await {
            Ok(v) => v,
            Err(Error::Http { status: 401 | 403, .. }) => return Err(Error::AuthRejected),
            Err(e) => return Err(self.scrub(e)),
        };
        if !v.is_object() {
            // empty body (→ null), `[]`, `false`, …
            return Err(Error::AuthRejected);
        }
        let auth: XtAuth = serde_json::from_value(v).map_err(|e| self.scrub(Error::parse(None, format!("login response: {e}"))))?;
        let ui = &auth.user_info;
        let looks_empty = ui.auth.is_none() && ui.status.is_none() && ui.username.is_none();
        if ui.auth == Some(0) || looks_empty {
            return Err(Error::AuthRejected);
        }
        let account = account_info(&auth, now);
        match unavailable(account.status) {
            Some(e) => Err(e),
            None => Ok((auth, account)),
        }
    }

    pub async fn live_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> {
        self.list("get_live_categories", &[], cancel).await
    }
    /// All live streams in ONE call (no category filter); may be 20–100 MB.
    pub async fn live_streams(&self, cancel: &CancellationToken) -> Result<Vec<XtLiveStream>> {
        self.list("get_live_streams", &[], cancel).await
    }
    pub async fn vod_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> {
        self.list("get_vod_categories", &[], cancel).await
    }
    pub async fn vod_streams(&self, cancel: &CancellationToken) -> Result<Vec<XtVodStream>> {
        self.list("get_vod_streams", &[], cancel).await
    }
    pub async fn series_categories(&self, cancel: &CancellationToken) -> Result<Vec<XtCategory>> {
        self.list("get_series_categories", &[], cancel).await
    }
    pub async fn series(&self, cancel: &CancellationToken) -> Result<Vec<XtSeries>> {
        self.list("get_series", &[], cancel).await
    }
    /// A non-object reply (`[]`, `null`) means the panel has no such movie → `Error::NotFound`.
    pub async fn vod_info(&self, vod_id: i64, cancel: &CancellationToken) -> Result<XtVodInfo> {
        let v = self.call("get_vod_info", &[("vod_id", vod_id.to_string())], cancel).await?;
        if !v.is_object() {
            return Err(Error::NotFound(format!("movie {vod_id}")));
        }
        if let Some(e) = login_reply(&v) {
            return Err(e);
        }
        self.decode(v, "get_vod_info")
    }
    /// Tries `series_id=` then the `series=` fork spelling when the first returns empty.
    pub async fn series_info(&self, series_id: i64, cancel: &CancellationToken) -> Result<XtSeriesInfo> {
        let first = self.series_info_with("series_id", series_id, cancel).await?;
        if !is_empty_series(&first) {
            return Ok(first);
        }
        match self.series_info_with("series", series_id, cancel).await {
            Ok(second) if !is_empty_series(&second) => Ok(second),
            Ok(_) => Ok(first),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(e) => {
                tracing::debug!("series_info fallback spelling failed: {e}");
                Ok(first)
            }
        }
    }
    /// Fallback now/next for channels without XMLTV data (texts decoded, sorted by start).
    pub async fn short_epg(&self, stream_id: i64, limit: u32, cancel: &CancellationToken) -> Result<Vec<XtEpgListing>> {
        let extra = [("stream_id", stream_id.to_string()), ("limit", limit.to_string())];
        let v = self.call("get_short_epg", &extra, cancel).await?;
        let mut listings = match v {
            Value::Object(mut m) => match m.remove("epg_listings") {
                Some(l) => self.decode::<LenientList<XtEpgListing>>(l, "get_short_epg")?.items,
                None => match m.get("user_info") {
                    Some(ui) => return Err(login_reply_error(ui)),
                    None => vec![],
                },
            },
            // some forks answer with the bare list
            v @ Value::Array(_) => self.decode::<LenientList<XtEpgListing>>(v, "get_short_epg")?.items,
            _ => vec![],
        };
        decode_epg_texts(&mut listings);
        // id-keyed listings lose their document order inside a `Value`
        listings.sort_by_key(|l| l.start_timestamp.unwrap_or(i64::MAX));
        Ok(listings)
    }

    /// A catalogue list. Only a real list may be empty (`[]`, `{}`): anything else that decodes
    /// as "nothing" is an error, so a failing panel never looks like an emptied catalogue.
    async fn list<T: DeserializeOwned + Send + 'static>(&self, action: &str, extra: &[(&str, String)], cancel: &CancellationToken) -> Result<Vec<T>> {
        let url = urls::player_api_url(&self.creds, Some(action), extra);
        let l = self.http.get_json::<LenientList<T>>(&url, &self.headers, cancel).await.map_err(|e| self.scrub(e))?;
        match l.user_info {
            Some(ui) => Err(login_reply_error(&ui)),
            None if l.is_list => Ok(l.items),
            None => Err(Error::BadPayload(format!("{action}: the server returned an empty or invalid list"))),
        }
    }

    async fn call(&self, action: &str, extra: &[(&str, String)], cancel: &CancellationToken) -> Result<Value> {
        let url = urls::player_api_url(&self.creds, Some(action), extra);
        self.http.get_json::<Value>(&url, &self.headers, cancel).await.map_err(|e| self.scrub(e))
    }

    async fn series_info_with(&self, param: &str, series_id: i64, cancel: &CancellationToken) -> Result<XtSeriesInfo> {
        let v = self.call("get_series_info", &[(param, series_id.to_string())], cancel).await?;
        if !v.is_object() {
            return Ok(XtSeriesInfo::default());
        }
        if let Some(e) = login_reply(&v) {
            return Err(e);
        }
        self.decode(v, "get_series_info")
    }

    fn decode<T: DeserializeOwned>(&self, v: Value, what: &str) -> Result<T> {
        serde_json::from_value(v).map_err(|e| self.scrub(Error::parse(None, format!("{what}: {e}"))))
    }

    /// Remove this account's credentials from an error message (defence in depth: the generic
    /// redactor only knows registered secrets and URL shapes).
    fn scrub(&self, e: Error) -> Error {
        let secrets: Vec<String> = [self.creds.password.expose().as_str(), self.creds.username.as_str()]
            .into_iter()
            .map(str::trim)
            .filter(|s| s.len() >= 3)
            .flat_map(|s| [urls::enc(s), s.to_owned()])
            .collect();
        let clean = |m: String| {
            secrets.iter().fold(m, |m, s| if m.contains(s.as_str()) { m.replace(s.as_str(), "***") } else { m })
        };
        match e {
            Error::Network(m) => Error::Network(clean(m)),
            Error::Http { status, url } => Error::Http { status, url: clean(url) },
            Error::BadPayload(m) => Error::BadPayload(clean(m)),
            Error::Parse { line, msg } => Error::Parse { line, msg: clean(msg) },
            Error::NotFound(m) => Error::NotFound(clean(m)),
            Error::Other(m) => Error::Other(clean(m)),
            Error::AccountUnavailable { status } => Error::AccountUnavailable { status: clean(status) },
            other => other,
        }
    }
}

fn is_empty_series(s: &XtSeriesInfo) -> bool { s.episodes.is_empty() && s.seasons.is_empty() }

/// The login object in place of data (`{"user_info":…}`), as an error.
fn login_reply(v: &Value) -> Option<Error> { v.get("user_info").map(login_reply_error) }

/// Panels answer actions with the login object when the credentials are rejected or the account
/// is unusable; some forks also send it, account fine, for actions they don't support. Never
/// "no data": that would read as an emptied catalogue.
fn login_reply_error(user_info: &Value) -> Error {
    let ui = XtUserInfo::deserialize(user_info).unwrap_or_default();
    if let Some(e) = parse_status(ui.status.as_deref()).and_then(unavailable) {
        return e;
    }
    if ui.auth.unwrap_or(0) == 0 {
        return Error::AuthRejected;
    }
    Error::BadPayload("the panel answered with account info instead of data".into())
}

/// `user_info.status` text → status; `None` when the panel sent none.
fn parse_status(s: Option<&str>) -> Option<AccountStatus> {
    Some(match s?.trim().to_ascii_lowercase().as_str() {
        "active" => AccountStatus::Active,
        "expired" => AccountStatus::Expired,
        "banned" => AccountStatus::Banned,
        "disabled" => AccountStatus::Disabled,
        _ => AccountStatus::Other,
    })
}

/// Expired / Banned / Disabled → `Error::AccountUnavailable`.
fn unavailable(status: AccountStatus) -> Option<Error> {
    let status = match status {
        AccountStatus::Expired => "expired",
        AccountStatus::Banned => "banned",
        AccountStatus::Disabled => "disabled",
        _ => return None,
    };
    Some(Error::AccountUnavailable { status: status.into() })
}

/// `user_info` / `server_info` → [`AccountInfo`].
fn account_info(auth: &XtAuth, now: i64) -> AccountInfo {
    let ui = &auth.user_info;
    let si = &auth.server_info;
    let count = |v: Option<i64>| v.and_then(|n| u32::try_from(n).ok());
    let epoch = |v: Option<i64>| v.filter(|&t| t > 0);
    let exp_date = epoch(ui.exp_date);
    let status = match parse_status(ui.status.as_deref()) {
        Some(s) => s,
        // no status at all: judge by the expiry date, else it logged in fine
        None if exp_date.is_some_and(|e| e < now) => AccountStatus::Expired,
        None => AccountStatus::Unknown,
    };
    AccountInfo {
        status,
        exp_date,
        is_trial: ui.is_trial.is_some_and(|t| t != 0),
        active_cons: count(ui.active_cons),
        max_connections: count(ui.max_connections),
        created_at: epoch(ui.created_at),
        allowed_formats: ui.allowed_output_formats.iter().map(|f| f.trim().to_ascii_lowercase()).filter(|f| !f.is_empty()).collect(),
        server_tz: si.timezone.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned),
        clock_skew_s: si.timestamp_now.filter(|&t| t > 0).map(|t| t - now),
        message: ui.message.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(|m| crate::redact::redact(m).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(json: &str) -> XtAuth { serde_json::from_str(json).unwrap() }

    #[test]
    fn account_info_maps_every_field() {
        let a = auth(
            r#"{"user_info":{"username":"u","message":" Welcome ","auth":1,"status":"Active","exp_date":"1798761600","is_trial":"1",
                "active_cons":"0","created_at":"1700000000","max_connections":"2","allowed_output_formats":["M3U8","ts",""]},
                "server_info":{"timezone":"Europe/London","timestamp_now":1791576030}}"#,
        );
        let info = account_info(&a, 1_791_576_000);
        assert_eq!(info.status, AccountStatus::Active);
        assert_eq!(info.exp_date, Some(1_798_761_600));
        assert!(info.is_trial);
        assert_eq!(info.active_cons, Some(0));
        assert_eq!(info.max_connections, Some(2));
        assert_eq!(info.created_at, Some(1_700_000_000));
        assert_eq!(info.allowed_formats, vec!["m3u8".to_string(), "ts".to_string()]);
        assert_eq!(info.server_tz.as_deref(), Some("Europe/London"));
        assert_eq!(info.clock_skew_s, Some(30));
        assert_eq!(info.message.as_deref(), Some("Welcome"));
    }

    #[test]
    fn account_status_variants() {
        let st = |json: &str, now| account_info(&auth(json), now).status;
        assert_eq!(st(r#"{"user_info":{"auth":1,"status":"EXPIRED"}}"#, 0), AccountStatus::Expired);
        assert_eq!(st(r#"{"user_info":{"auth":1,"status":"Banned"}}"#, 0), AccountStatus::Banned);
        assert_eq!(st(r#"{"user_info":{"auth":1,"status":"Disabled"}}"#, 0), AccountStatus::Disabled);
        assert_eq!(st(r#"{"user_info":{"auth":1,"status":"Trial"}}"#, 0), AccountStatus::Other);
        assert_eq!(st(r#"{"user_info":{"auth":1,"exp_date":"100"}}"#, 200), AccountStatus::Expired);
        assert_eq!(st(r#"{"user_info":{"auth":1,"exp_date":null}}"#, 200), AccountStatus::Unknown);
        // unlimited accounts send 0 / null as the expiry
        assert_eq!(account_info(&auth(r#"{"user_info":{"auth":1,"exp_date":"0"}}"#), 5).exp_date, None);
        // negative counts are garbage, not huge u32s
        assert_eq!(account_info(&auth(r#"{"user_info":{"auth":1,"max_connections":-1}}"#), 5).max_connections, None);
    }

    #[test]
    fn login_object_in_place_of_data() {
        let err = |json: &str| login_reply(&serde_json::from_str(json).unwrap());
        assert!(err(r#"{"data":[]}"#).is_none());
        assert!(matches!(err(r#"{"user_info":{"auth":0}}"#), Some(Error::AuthRejected)));
        assert!(matches!(err(r#"{"user_info":{"status":"Active"}}"#), Some(Error::AuthRejected)));
        assert!(matches!(err(r#"{"user_info":[]}"#), Some(Error::AuthRejected)));
        let status_of = |json: &str| match err(json) {
            Some(Error::AccountUnavailable { status }) => status,
            other => panic!("{json}: {other:?}"),
        };
        assert_eq!(status_of(r#"{"user_info":{"auth":"1","status":"Banned"}}"#), "banned");
        assert_eq!(status_of(r#"{"user_info":{"auth":0,"status":"Expired"}}"#), "expired");
        // account fine: the panel just doesn't serve this action
        assert!(matches!(err(r#"{"user_info":{"auth":1,"status":"Active"}}"#), Some(Error::BadPayload(_))));
    }
}
