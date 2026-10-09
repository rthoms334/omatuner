//! Lenient Xtream JSON models. Owner: WP-04.
//! Use the helpers on EVERY scalar: panels disagree on types (protocols.md §2 "JSON quirks").

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// number | "number" | "1.0" | bool → i64; null/""/[]/{} → None.
pub fn lenient_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        Value::Bool(b) => Some(b as i64),
        _ => None,
    })
}

/// number | "number" → f64.
pub fn lenient_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    })
}

/// Non-empty string | number → String; null/""/[]/{} → None.
pub fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.trim().is_empty() => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// "a" | ["a","b"] | null → Vec<String> (empties dropped).
pub fn string_or_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.trim().is_empty() => vec![s],
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        _ => vec![],
    })
}

/// Object, or `[]` (PHP empty array) → default.
pub fn object_or_empty<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let v = Value::deserialize(d)?;
    match v {
        Value::Object(_) => serde_json::from_value(v).map_err(serde::de::Error::custom),
        _ => Ok(T::default()),
    }
}

/// `episodes`: `{"1":[...],"2":[...]}` | `[[...],[...]]` | `[]` → season → episodes.
pub fn episodes_map<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<i64, Vec<XtEpisode>>, D::Error> {
    todo!("WP-04")
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtAuth {
    #[serde(default, deserialize_with = "object_or_empty")]
    pub user_info: XtUserInfo,
    #[serde(default, deserialize_with = "object_or_empty")]
    pub server_info: XtServerInfo,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtUserInfo {
    #[serde(default, deserialize_with = "lenient_string")]
    pub username: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub message: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub auth: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub exp_date: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub is_trial: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub active_cons: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub created_at: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub max_connections: Option<i64>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub allowed_output_formats: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtServerInfo {
    #[serde(default, deserialize_with = "lenient_string")]
    pub url: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub port: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub https_port: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub server_protocol: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub timezone: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub timestamp_now: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtCategory {
    #[serde(default, deserialize_with = "lenient_string")]
    pub category_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub category_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtLiveStream {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub num: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub stream_id: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub stream_icon: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub epg_channel_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub added: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub category_id: Option<String>,
    /// XUI.one: all categories.
    #[serde(default, deserialize_with = "string_or_vec_lenient")]
    pub category_ids: Vec<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub tv_archive: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub tv_archive_duration: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub direct_source: Option<String>,
}

/// Like `string_or_vec` but also accepts numbers inside the array (`category_ids: [1, 2]`).
pub fn string_or_vec_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.trim().is_empty() => vec![s],
        Value::Number(n) => vec![n.to_string()],
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Some(s),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtVodStream {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub num: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub stream_id: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub stream_icon: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub rating: Option<f64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub added: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub category_id: Option<String>,
    #[serde(default, deserialize_with = "string_or_vec_lenient")]
    pub category_ids: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub container_extension: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub year: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtSeries {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub num: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub series_id: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cover: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub plot: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cast: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub director: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub genre: Option<String>,
    #[serde(default, rename = "releaseDate", deserialize_with = "lenient_string")]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub last_modified: Option<i64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub rating: Option<f64>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub backdrop_path: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub category_id: Option<String>,
    #[serde(default, deserialize_with = "string_or_vec_lenient")]
    pub category_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtVodInfo {
    #[serde(default, deserialize_with = "object_or_empty")]
    pub info: XtVodInfoInner,
    #[serde(default, deserialize_with = "object_or_empty")]
    pub movie_data: XtMovieData,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtVodInfoInner {
    #[serde(default, deserialize_with = "lenient_string")]
    pub movie_image: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub tmdb_id: Option<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub backdrop_path: Vec<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub youtube_trailer: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub genre: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub plot: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cast: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub rating: Option<f64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub director: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub releasedate: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub duration_secs: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtMovieData {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub stream_id: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub container_extension: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtSeriesInfo {
    #[serde(default)]
    pub seasons: Vec<XtSeason>,
    #[serde(default, deserialize_with = "object_or_empty")]
    pub info: XtSeriesInfoInner,
    #[serde(default, deserialize_with = "episodes_map")]
    pub episodes: BTreeMap<i64, Vec<XtEpisode>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtSeriesInfoInner {
    #[serde(default, deserialize_with = "lenient_string")]
    pub plot: Option<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    pub backdrop_path: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtSeason {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub season_number: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cover: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub cover_big: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub episode_count: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub air_date: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub overview: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtEpisode {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub episode_num: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub container_extension: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub season: Option<i64>,
    #[serde(default, deserialize_with = "object_or_empty")]
    pub info: XtEpisodeInfo,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtEpisodeInfo {
    #[serde(default, deserialize_with = "lenient_string")]
    pub movie_image: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub plot: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub duration_secs: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub releasedate: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub rating: Option<f64>,
}

/// `get_short_epg` / `get_simple_data_table` listing (title/description usually base64).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtEpgListing {
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub start_timestamp: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub stop_timestamp: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub has_archive: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct XtEpgResponse {
    #[serde(default)]
    pub epg_listings: Vec<XtEpgListing>,
}

/// Decide per source whether EPG text is base64 (≥ 80 % of titles decode to valid UTF-8 with
/// printable chars), then decode accordingly. Short words like "News" are valid base64, so a
/// per-string guess is wrong.
pub fn decode_epg_texts(listings: &mut [XtEpgListing]) { todo!("WP-04") }
