//! Provider protocols: parsing, fetching, matching, URL building. No database writes and no UI
//! here — parsers yield `store::import::*` / `store::epg::Import*` rows that `jobs` persist.
//!
//! | file              | owner | contents |
//! |-------------------|-------|----------|
//! | `http.rs`         | WP-04 | reqwest client, streamed downloads (`sniff_compression` frozen) |
//! | `xtream/*`        | WP-04 | player_api client, lenient serde types (`urls.rs` frozen); `detect.rs` P5 (§27.2) |
//! | `m3u.rs`          | WP-03 | streaming extended-M3U parser |
//! | `m3u_vod.rs`      | P5 sources | M3U movies/series fallback classifier (DESIGN §27.5) |
//! | `presets.rs`      | P5 sources | free-channel playlists + country detection (§27.8) |
//! | `codes.rs`       | playlist codes | short playlist codes shown on cards and rows (§13.0) |
//! | `category_tags.rs`| P6 core | country / language tags of category names (§28.2) |
//! | `explicit.rs`     | P14 | adult-content detection of category names (§20.3) |
//! | `smart_names.rs`  | WP-03 | name cleanup ("UK | BBC One FHD" → "BBC One") |
//! | `vod_match.rs`    | P2 core | match keys, languages and stacking rules of movies / series (§29.10) |
//! | `name_tokens.rs`  | P6 names | the name tokenizer shared by smart names and category tags (§13.1) |
//! | `identity.rs`     | skeleton (frozen) | ItemKey / normalization / fingerprints (WP-03 owns its tests) |
//! | `xmltv.rs`        | WP-05a | streaming XMLTV parser |
//! | `epg_match.rs`    | WP-05a | channel ↔ EPG matching |
//! | `catchup.rs`      | WP-05a (`parse_kind` frozen) | catch-up URL templates |
//! | `resolve.rs`      | WP-05b | MediaRef → Playable (URLs, headers, titles) |

pub mod catchup;
pub mod category_tags;
pub mod codes;
pub mod epg_match;
pub mod explicit;
pub mod http;
pub mod identity;
pub mod m3u;
pub mod m3u_vod;
pub mod name_tokens;
pub mod presets;
pub mod resolve;
pub mod smart_names;
pub mod vod_match;
pub mod xmltv;
pub mod xtream;

/// What a downloaded payload looks like (first non-empty bytes), used to reject HTML error
/// pages served with 200 and to route pasted URLs in onboarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// `#EXTM3U` / `#EXTINF` / bare URL lines.
    M3u,
    /// `#EXT-X-*` — a single HLS stream, not a channel list.
    HlsMedia,
    /// `<?xml` / `<tv` — XMLTV.
    Xmltv,
    /// `{` / `[` — JSON (Xtream API).
    Json,
    /// `<!DOCTYPE html` / `<html` — error/captcha page.
    Html,
    Unknown,
}

/// Classify the first bytes of a (decompressed) payload (pass ≥ 4 KiB when available). Skips
/// BOM and leading whitespace. Implemented in the skeleton (used by WP-03/04/06).
pub fn sniff_payload(first: &[u8]) -> PayloadKind {
    let b = first.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(first);
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let head = String::from_utf8_lossy(&b[start..b.len().min(start + 4096)]).to_ascii_lowercase();
    if head.starts_with("#extm3u") || head.starts_with("#extinf") {
        let hls = ["#ext-x-targetduration", "#ext-x-stream-inf", "#ext-x-media-sequence", "#ext-x-version", "#ext-x-playlist-type"];
        return if hls.iter().any(|t| head.contains(t)) { PayloadKind::HlsMedia } else { PayloadKind::M3u };
    }
    if head.starts_with("#ext-x-") {
        return PayloadKind::HlsMedia;
    }
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        return PayloadKind::Html;
    }
    if head.starts_with("<?xml") || head.starts_with("<tv") || head.starts_with("<!doctype tv") {
        return if head.contains("<html") { PayloadKind::Html } else { PayloadKind::Xmltv };
    }
    if head.starts_with('{') || head.starts_with('[') {
        return PayloadKind::Json;
    }
    if head.starts_with("http://") || head.starts_with("https://") || head.starts_with("rtmp://") || head.starts_with("udp://") {
        return PayloadKind::M3u;
    }
    PayloadKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(sniff_payload(b"\xEF\xBB\xBF  #EXTM3U url-tvg=\"x\"\n#EXTINF:-1,A\nhttp://a"), PayloadKind::M3u);
        assert_eq!(sniff_payload(b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6"), PayloadKind::HlsMedia);
        assert_eq!(sniff_payload(b"<!DOCTYPE html><html>"), PayloadKind::Html);
        assert_eq!(sniff_payload(b"<?xml version=\"1.0\"?><tv>"), PayloadKind::Xmltv);
        assert_eq!(sniff_payload(b" {\"user_info\":{}}"), PayloadKind::Json);
        assert_eq!(sniff_payload(b"http://h/1.ts\n"), PayloadKind::M3u);
        assert_eq!(sniff_payload(b"garbage"), PayloadKind::Unknown);
    }
}
