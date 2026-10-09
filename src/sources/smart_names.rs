//! Smart name cleanup (UHF "Smart Playlists"). Owner: WP-03.
//!
//! Display-only: the raw provider name is always kept (`name`) and identity uses
//! `identity::normalize_name`, so toggling cleanup never changes keys.
//!
//! Examples (fixture table in `tests/fixtures/m3u/smart_names.tsv`):
//! | raw                              | cleaned        | country | quality |
//! |----------------------------------|----------------|---------|---------|
//! | `UK | BBC One FHD`               | `BBC One`      | UK      | Fhd     |
//! | `|EN| CNN HD (backup)`           | `CNN`          | EN      | Hd      |
//! | `[VIP] US: ESPN 2 4K`            | `ESPN 2`       | US      | Uhd     |
//! | `DE - Das Erste HEVC`            | `Das Erste`    | DE      | —       |
//! | `Sky Cinema +1 HD`               | `Sky Cinema +1`| —       | Hd      |
//! | `The Matrix (1999) [MULTI-SUB]`  | `The Matrix`   | year 1999 (VOD) |

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    Sd,
    Hd,
    Fhd,
    Uhd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanName {
    pub name: String,
    /// Country/language prefix that was stripped (upper-case, 2-3 letters).
    pub country: Option<String>,
    pub quality: Option<Quality>,
    /// Other stripped tags: "VIP", "backup", "HEVC", "MULTI-SUB"...
    pub tags: Vec<String>,
}

/// Clean a live channel name. Never returns an empty name (falls back to the trimmed raw).
pub fn clean_channel_name(raw: &str) -> CleanName { todo!("WP-03") }

/// Clean a movie/series title; extracts a trailing `(YYYY)` / `- YYYY` year.
pub fn clean_vod_name(raw: &str) -> (String, Option<u16>) { todo!("WP-03") }

/// Clean a group/category name (`"UK| SPORTS ᴴᴰ"` → `"Sports"`, keeps country as prefix tag).
pub fn clean_group_name(raw: &str) -> String { todo!("WP-03") }
