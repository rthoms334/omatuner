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
///
/// Season numbers come from the object key; for arrays (PHP re-indexes sequential keys) from the
/// episodes' own `season` field, else the 1-based position. A flat array of episode objects is
/// grouped by `season` (default 1). Episodes that are not objects are dropped.
pub fn episodes_map<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<i64, Vec<XtEpisode>>, D::Error> {
    let mut out: BTreeMap<i64, Vec<XtEpisode>> = BTreeMap::new();
    match Value::deserialize(d)? {
        Value::Object(seasons) => {
            for (key, eps) in seasons {
                let eps = episode_list(eps);
                let season = key.trim().parse::<i64>().ok().or_else(|| eps.iter().find_map(|e| e.season)).unwrap_or(0);
                if !eps.is_empty() {
                    out.entry(season).or_default().extend(eps);
                }
            }
        }
        Value::Array(items) => {
            for (idx, item) in items.into_iter().enumerate() {
                if item.is_object() {
                    // flat list of episodes
                    if let Ok(ep) = serde_json::from_value::<XtEpisode>(item) {
                        out.entry(ep.season.unwrap_or(1)).or_default().push(ep);
                    }
                    continue;
                }
                let eps = episode_list(item);
                let season = eps.iter().find_map(|e| e.season).unwrap_or(idx as i64 + 1);
                if !eps.is_empty() {
                    out.entry(season).or_default().extend(eps);
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// [`LenientList`] as a field helper (`seasons`, `epg_listings`: `null`, `{}`, keyed objects).
fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    LenientList::<T>::deserialize(d).map(|l| l.items)
}

/// One season's episodes: an array, or an object keyed by index (`{"0": {...}}`).
fn episode_list(v: Value) -> Vec<XtEpisode> {
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        Value::Object(m) => {
            // `serde_json::Map` iterates in key TEXT order ("10" < "2"): sort by the index
            // (non-numeric keys last, in their original order).
            let mut keyed: Vec<(i64, Value)> = m.into_iter().map(|(k, v)| (k.trim().parse::<i64>().unwrap_or(i64::MAX), v)).collect();
            keyed.sort_by_key(|(idx, _)| *idx);
            keyed.into_iter().map(|(_, v)| v).collect()
        }
        _ => return vec![],
    };
    items.into_iter().filter(Value::is_object).filter_map(|v| serde_json::from_value(v).ok()).collect()
}

/// A JSON list that never fails as a whole: an array, an object keyed by numeric ids (values in
/// document order, i.e. the panel's order), or `null`/`""`/`false`/`{}` (empty). Elements that
/// don't deserialize as `T` (not an object, …) are skipped. Used for every `get_*` list action.
///
/// `is_list` tells a real (possibly empty) list — an array, `{}` or an id-keyed object — from a
/// reply that merely *decodes* as empty (`null`, `""`, `false`, `{"error":…}`): catalogue
/// actions must not mistake an outage for an empty catalogue (DESIGN §7.2). Panels answer
/// actions with the login object (`{"user_info":{…}}`) when something is wrong with the account;
/// that is kept in `user_info` instead of yielding a bogus element.
#[derive(Debug, Clone)]
pub(crate) struct LenientList<T> {
    pub items: Vec<T>,
    pub is_list: bool,
    pub user_info: Option<Value>,
}

impl<T> LenientList<T> {
    fn empty() -> Self { Self { items: vec![], is_list: false, user_info: None } }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for LenientList<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);

        impl<'de, T: serde::de::DeserializeOwned> serde::de::Visitor<'de> for V<T> {
            type Value = LenientList<T>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("a list") }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = LenientList { is_list: true, ..LenientList::empty() };
                out.items.reserve(seq.size_hint().unwrap_or(0).min(1 << 16));
                while let Some(v) = seq.next_element::<Value>()? {
                    if let Ok(t) = serde_json::from_value(v) {
                        out.items.push(t);
                    }
                }
                Ok(out)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = LenientList::empty();
                let mut any_key = false;
                while let Some(key) = map.next_key::<String>()? {
                    any_key = true;
                    if key.trim().parse::<i64>().is_ok() {
                        out.is_list = true;
                        if let Ok(t) = serde_json::from_value(map.next_value::<Value>()?) {
                            out.items.push(t);
                        }
                    } else if key == "user_info" {
                        out.user_info = Some(map.next_value::<Value>()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                // `{}`: PHP's empty array under JSON_FORCE_OBJECT
                out.is_list |= !any_key;
                Ok(out)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok(LenientList::empty()) }
        }

        d.deserialize_any(V(std::marker::PhantomData))
    }
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
    #[serde(default, deserialize_with = "lenient_vec")]
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
    #[serde(default, deserialize_with = "lenient_vec")]
    pub epg_listings: Vec<XtEpgListing>,
}

/// Decide per source whether EPG text is base64 (≥ 80 % of titles decode to valid UTF-8 with
/// printable chars), then decode accordingly. Short words like "News" are valid base64, so a
/// per-string guess is wrong.
///
/// Titles are the sample; descriptions only when no listing has a title. Under a "base64"
/// verdict, individual strings that don't decode are kept verbatim.
pub fn decode_epg_texts(listings: &mut [XtEpgListing]) {
    let titles: Vec<&str> = listings.iter().filter_map(|l| l.title.as_deref()).collect();
    let sample = if titles.is_empty() { listings.iter().filter_map(|l| l.description.as_deref()).collect() } else { titles };
    if sample.is_empty() {
        return;
    }
    let decodable = sample.iter().filter(|s| decode_b64_text(s).is_some()).count();
    // ≥ 80 %, in integers
    if decodable * 5 < sample.len() * 4 {
        return;
    }
    for l in listings.iter_mut() {
        for field in [&mut l.title, &mut l.description] {
            if let Some(decoded) = field.as_deref().and_then(decode_b64_text) {
                *field = (!decoded.trim().is_empty()).then_some(decoded);
            }
        }
    }
}

/// Base64 (standard alphabet, padding optional; URL-safe as a fallback) → printable UTF-8.
fn decode_b64_text(s: &str) -> Option<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT};
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = STANDARD_PAD_INDIFFERENT.decode(s).or_else(|_| URL_SAFE_PAD_INDIFFERENT.decode(s)).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    text.chars().all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn listing(title: Option<&str>, desc: Option<&str>) -> XtEpgListing {
        XtEpgListing { title: title.map(Into::into), description: desc.map(Into::into), ..Default::default() }
    }

    #[test]
    fn lenient_scalars() {
        #[derive(Deserialize)]
        struct S {
            #[serde(default, deserialize_with = "lenient_i64")]
            a: Option<i64>,
            #[serde(default, deserialize_with = "lenient_f64")]
            b: Option<f64>,
            #[serde(default, deserialize_with = "lenient_string")]
            c: Option<String>,
        }
        let s: S = serde_json::from_str(r#"{"a":" 12 ","b":"7.5","c":42}"#).unwrap();
        assert_eq!((s.a, s.b, s.c.as_deref()), (Some(12), Some(7.5), Some("42")));
        let s: S = serde_json::from_str(r#"{"a":"1.9","b":null,"c":"  "}"#).unwrap();
        assert_eq!((s.a, s.b, s.c), (Some(1), None, None));
        let s: S = serde_json::from_str(r#"{"a":[],"b":{},"c":[]}"#).unwrap();
        assert_eq!((s.a, s.b, s.c), (None, None, None));
        let s: S = serde_json::from_str(r#"{"a":true}"#).unwrap();
        assert_eq!(s.a, Some(1));
    }

    #[test]
    fn episodes_object_array_flat_keyed_and_empty() {
        let info: XtSeriesInfo = serde_json::from_str(
            r#"{"episodes":{"1":[{"id":"1","season":"1"},{"id":2}],"2":[{"id":"3"}],"3":[],"x":[{"id":"9","season":4}]}}"#,
        )
        .unwrap();
        assert_eq!(info.episodes.keys().copied().collect::<Vec<_>>(), vec![1, 2, 4]);
        assert_eq!(info.episodes[&1].len(), 2);
        assert_eq!(info.episodes[&1][1].id.as_deref(), Some("2"));

        // PHP list: season from the episodes, else the position
        let info: XtSeriesInfo = serde_json::from_str(r#"{"episodes":[[{"id":"a"}],[{"id":"b","season":"5"}]]}"#).unwrap();
        assert_eq!(info.episodes.keys().copied().collect::<Vec<_>>(), vec![1, 5]);

        // flat list grouped by `season`; junk dropped
        let info: XtSeriesInfo =
            serde_json::from_str(r#"{"episodes":[{"id":"a","season":2},{"id":"b","season":"2"},{"id":"c"},"junk",null,7]}"#).unwrap();
        assert_eq!(info.episodes[&2].len(), 2);
        assert_eq!(info.episodes[&1][0].id.as_deref(), Some("c"));

        // season keyed by episode index
        let info: XtSeriesInfo = serde_json::from_str(r#"{"episodes":{"3":{"0":{"id":"k"},"1":{"id":"l"}}}}"#).unwrap();
        assert_eq!(info.episodes[&3].len(), 2);
        // … in index order, not key-text order ("10" < "2"); non-numeric keys last
        let season: Vec<String> = (0..12).map(|i| format!(r#""{i}":{{"id":"e{i}"}}"#)).collect();
        let json = format!(r#"{{"episodes":{{"1":{{"x":{{"id":"last"}},{}}}}}}}"#, season.join(","));
        let info: XtSeriesInfo = serde_json::from_str(&json).unwrap();
        let ids: Vec<String> = info.episodes[&1].iter().map(|e| e.id.clone().unwrap()).collect();
        let want: Vec<String> = (0..12).map(|i| format!("e{i}")).chain(["last".into()]).collect();
        assert_eq!(ids, want);

        for empty in [r#"{"episodes":[]}"#, r#"{"episodes":{}}"#, r#"{"episodes":null}"#, r#"{"episodes":""}"#, "{}"] {
            let info: XtSeriesInfo = serde_json::from_str(empty).unwrap();
            assert!(info.episodes.is_empty(), "{empty}");
        }
        // `seasons: null` must not fail the whole reply
        let info: XtSeriesInfo = serde_json::from_str(r#"{"seasons":null,"episodes":{"1":[{"id":"z"}]}}"#).unwrap();
        assert!(info.seasons.is_empty());
        assert_eq!(info.episodes[&1].len(), 1);
    }

    #[test]
    fn lenient_list_shapes() {
        let l: LenientList<XtCategory> = serde_json::from_str(r#"[{"category_id":1},null,"x",{"category_id":"2"}]"#).unwrap();
        assert_eq!(l.items.len(), 2);
        assert!(l.is_list && l.user_info.is_none());
        // keyed by id: document order, not key order
        let l: LenientList<XtCategory> = serde_json::from_str(r#"{"8":{"category_id":8},"7":{"category_id":7},"total":2}"#).unwrap();
        assert_eq!(l.items.iter().map(|c| c.category_id.clone().unwrap()).collect::<Vec<_>>(), vec!["8", "7"]);
        assert!(l.is_list);
        // genuinely empty lists
        for empty in ["[]", "{}"] {
            let l: LenientList<XtCategory> = serde_json::from_str(empty).unwrap();
            assert!(l.items.is_empty() && l.is_list && l.user_info.is_none(), "{empty}");
        }
        // decodes as empty, but is no list (outage, rate limit, …)
        for junk in ["null", "\"\"", "false", "0", r#"{"error":"rate limited"}"#] {
            let l: LenientList<XtCategory> = serde_json::from_str(junk).unwrap();
            assert!(l.items.is_empty() && !l.is_list && l.user_info.is_none(), "{junk}");
        }
        let l: LenientList<XtCategory> = serde_json::from_str(r#"{"user_info":{"auth":0}}"#).unwrap();
        assert!(l.items.is_empty() && !l.is_list);
        assert_eq!(l.user_info.unwrap()["auth"], 0);
    }

    #[test]
    fn epg_texts_base64_decided_per_response() {
        let mut l = vec![
            listing(Some(&b64("News at Ten")), Some(&b64("Headlines & weather"))),
            listing(Some(&b64("Drama ünïcödé")), Some("")),
            listing(Some(&b64("Film")), None),
            listing(Some(&b64("Sport")), Some(&b64("Live"))),
            // one plain title in a base64 source stays as it is
            listing(Some("News"), Some("not base64!")),
        ];
        decode_epg_texts(&mut l);
        assert_eq!(l[0].title.as_deref(), Some("News at Ten"));
        assert_eq!(l[0].description.as_deref(), Some("Headlines & weather"));
        assert_eq!(l[1].title.as_deref(), Some("Drama ünïcödé"));
        assert_eq!(l[2].title.as_deref(), Some("Film"));
        assert_eq!(l[3].description.as_deref(), Some("Live"));
        assert_eq!(l[4].title.as_deref(), Some("News"));
        assert_eq!(l[4].description.as_deref(), Some("not base64!"));
    }

    #[test]
    fn epg_texts_plain_source_untouched() {
        // "News", "Live", "Film1234" use only base64 characters; the per-response verdict must
        // keep them as they are.
        let mut l = vec![
            listing(Some("News"), Some("Live")),
            listing(Some("Film1234"), None),
            listing(Some("Tom & Jerry"), None),
            listing(Some("Weather"), None),
        ];
        let before: Vec<_> = l.iter().map(|x| (x.title.clone(), x.description.clone())).collect();
        decode_epg_texts(&mut l);
        assert_eq!(l.iter().map(|x| (x.title.clone(), x.description.clone())).collect::<Vec<_>>(), before);
        // no titles at all: descriptions decide
        let mut d = vec![listing(None, Some(&b64("Only a description")))];
        decode_epg_texts(&mut d);
        assert_eq!(d[0].description.as_deref(), Some("Only a description"));
        decode_epg_texts(&mut []);
    }

    #[test]
    fn epg_texts_unpadded_base64() {
        let mut l = vec![listing(Some("TmV3cw"), None), listing(Some("U3BvcnQ"), None)];
        decode_epg_texts(&mut l);
        assert_eq!(l[0].title.as_deref(), Some("News"));
        assert_eq!(l[1].title.as_deref(), Some("Sport"));
    }
}
