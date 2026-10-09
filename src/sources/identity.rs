//! Stable identity: item keys, normalization, fingerprints. DESIGN §8.
//! IMPLEMENTED IN THE SKELETON and FROZEN (architect-owned contract): every importer, the
//! store, EPG matching and reattach depend on these bodies, and changing them orphans user
//! data. WP-03 owns `tests/identity.rs` (golden + property tests); body changes need architect
//! sign-off and a migration plan.
//!
//! Key rules (must stay stable forever):
//! * Xtream: `xt:live:{stream_id}`, `xt:vod:{stream_id}`, `xt:series:{series_id}`, `xt:ep:{id}`.
//! * M3U: `m3u:` + first 16 hex chars of blake3(`tvg_id` | `norm_name` | `norm_group`), where
//!   empty fields stay empty. The URL is deliberately NOT part of the key (tokenized URLs
//!   rotate). Duplicates within one import get `#2`, `#3`… in file order ([`KeyAllocator`]).
//! * Groups: `<kind>:<normalize_group(name)>` ("uncategorized" when empty).
//!   `Fingerprint.norm_group` is the same string without the `<kind>:` prefix.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::model::{Fingerprint, GroupKey, ItemKey, ItemKind};

/// Quality / codec tags dropped by [`normalize_name`] (whole tokens).
const QUALITY_TAGS: &[&str] = &["hd", "fhd", "uhd", "4k", "sd", "1080p", "720p", "1080i", "hevc", "h265", "h.265", "h264", "h.264"];

static PREFIX_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^\[[^\]]*\]\s*",          // [vip]
        r"^\|[a-z]{2,3}\|\s*",      // |en|
        r"^[a-z]{2,3}\s*[:|]\s*",   // us:  uk |
        r"^[a-z]{2}\s+-\s+",        // de - Das Erste
    ]
    .iter()
    .map(|r| Regex::new(r).expect("static regex"))
    .collect()
});

/// Normalize a name for matching/identity: deunicode → lowercase → strip country/language
/// prefixes (`us:`, `uk |`, `|en|`, `[vip]`, `de - `) and quality tags (hd fhd uhd 4k sd
/// 1080p 720p hevc h.265 "(backup)") → remove punctuation & whitespace. KEEPS `+1`/`+2`
/// (time-shifted channels are different channels). `"UK | BBC One HD"` → `"bbcone"`,
/// `"Sky Cinema +1"` → `"skycinema+1"`. Never empty for input with any alphanumeric char.
pub fn normalize_name(s: &str) -> String {
    let lower = deunicode::deunicode(s).to_lowercase();
    let mut t = lower.trim().replace("(backup)", " ");
    loop {
        let before = t.len();
        for re in PREFIX_RES.iter() {
            t = re.replace(&t, "").into_owned();
        }
        if t.len() == before {
            break;
        }
    }
    let kept: Vec<&str> = t
        .split_whitespace()
        .filter(|tok| {
            let bare = tok.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '|' | '-' | ','));
            !QUALITY_TAGS.contains(&bare)
        })
        .collect();
    let out = squash(&kept.join(" "));
    if out.is_empty() { squash(&lower) } else { out }
}

/// Keep ASCII alphanumerics and `+`.
fn squash(s: &str) -> String { s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+').collect() }

/// Group-name normalization (lighter than [`normalize_name`]: country prefixes are KEPT so
/// "UK | News" and "US | News" stay distinct groups). Empty → "uncategorized".
pub fn normalize_group(raw: &str) -> String {
    let s = squash(&deunicode::deunicode(raw).to_lowercase());
    if s.is_empty() { "uncategorized".into() } else { s }
}

/// Group key for a raw group name.
pub fn group_key(kind: ItemKind, raw_name: &str) -> GroupKey { GroupKey(format!("{}:{}", kind.as_str(), normalize_group(raw_name))) }

/// URL identity for reattach: scheme://host[:port]/path with userinfo, query, fragment and
/// Xtream credential path segments removed (`/live/u/p/1.ts` → `/live/1.ts`,
/// `/u/p/123.ts` → `/123.ts`). `None` for unparsable URLs.
pub fn url_key(url: &str) -> Option<String> {
    let u = url::Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_lowercase();
    let mut segs: Vec<&str> = u.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    let stem_numeric = |s: &str| {
        let stem = s.split('.').next().unwrap_or("");
        !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit())
    };
    if segs.len() >= 4 && matches!(segs[0], "live" | "movie" | "series" | "timeshift") {
        segs.drain(1..3);
    } else if segs.len() == 3 && stem_numeric(segs[2]) {
        segs.drain(0..2);
    }
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}/{}", u.scheme(), segs.join("/")))
}

pub fn xtream_key(kind: ItemKind, provider_id: i64) -> ItemKey {
    let k = match kind {
        ItemKind::Channel => "live",
        ItemKind::Movie => "vod",
        ItemKind::Series => "series",
        ItemKind::Episode => "ep",
    };
    ItemKey(format!("xt:{k}:{provider_id}"))
}

/// Base (pre-collision) M3U key. `norm_group` is `normalize_group(group)`.
pub fn m3u_base_key(tvg_id: Option<&str>, norm_name: &str, norm_group: Option<&str>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(tvg_id.unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(norm_name.as_bytes());
    h.update(b"|");
    h.update(norm_group.unwrap_or("").as_bytes());
    format!("m3u:{}", &h.finalize().to_hex()[..16])
}

/// Allocates unique keys within one import, suffixing duplicates deterministically.
#[derive(Debug, Default)]
pub struct KeyAllocator {
    seen: HashMap<String, u32>,
}

impl KeyAllocator {
    pub fn alloc(&mut self, base: String) -> ItemKey {
        let n = self.seen.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 { ItemKey(base) } else { ItemKey(format!("{base}#{n}")) }
    }
}

/// Fingerprint captured with user data (see `model::Fingerprint`).
pub fn fingerprint(provider_id: Option<&str>, tvg_id: Option<&str>, name: &str, group: Option<&str>, url: Option<&str>) -> Fingerprint {
    Fingerprint {
        provider_id: provider_id.map(str::to_owned),
        tvg_id: tvg_id.map(str::to_owned),
        norm_name: normalize_name(name),
        norm_group: group.map(normalize_group),
        url_key: url.and_then(url_key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize_name("UK | BBC One HD"), "bbcone");
        assert_eq!(normalize_name("Sky Cinema +1"), "skycinema+1");
        assert_eq!(normalize_name("|EN| CNN HD (backup)"), "cnn");
        assert_eq!(normalize_name("[VIP] US: ESPN 2 4K"), "espn2");
        assert_eq!(normalize_name("DE - Das Erste HEVC"), "daserste");
        assert_eq!(normalize_name("Ça Tévé"), "cateve");
        assert_eq!(normalize_name("HD"), "hd");
        assert_ne!(group_key(ItemKind::Channel, "UK | News"), group_key(ItemKind::Channel, "US | News"));
        assert_eq!(group_key(ItemKind::Movie, "  "), GroupKey("movie:uncategorized".into()));
    }

    /// GOLDEN: these values must never change (they are stored in users' databases).
    #[test]
    fn golden_keys() {
        assert_eq!(m3u_base_key(Some("bbc1.uk"), "bbcone", Some("uknews")), m3u_base_key(Some("bbc1.uk"), "bbcone", Some("uknews")));
        let k = m3u_base_key(None, "bbcone", None);
        assert!(k.starts_with("m3u:") && k.len() == 20);
        let mut a = KeyAllocator::default();
        assert_eq!(a.alloc("m3u:x".into()).0, "m3u:x");
        assert_eq!(a.alloc("m3u:x".into()).0, "m3u:x#2");
        assert_eq!(xtream_key(ItemKind::Channel, 42).0, "xt:live:42");
    }

    #[test]
    fn url_keys_drop_credentials() {
        assert_eq!(url_key("http://User:Pw@Host:8080/live/u/p/123.ts?token=x").as_deref(), Some("http://host:8080/live/123.ts"));
        assert_eq!(url_key("http://h/u/p/123.ts").as_deref(), Some("http://h/123.ts"));
        assert_eq!(url_key("https://cdn.example/ch/index.m3u8?sig=1").as_deref(), Some("https://cdn.example/ch/index.m3u8"));
        assert_eq!(url_key("not a url"), None);
    }
}
