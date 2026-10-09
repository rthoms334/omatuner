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
//!
//! Rules (no regexes, no allocation beyond the output):
//! * Leading prefixes, repeatedly: `[XX]` / `[tag]`, `|XX|`, `XX:` / `XX |` / `XX|`, and
//!   upper-case `XX - `. `XX` is a country/language code: any two letters, or a known
//!   three-letter code (`USA`, `ENG`, `ARA`…) so that `CNN: Live` stays intact.
//! * Quality words anywhere (`SD HD FHD UHD 4K 8K 480p…2160p 1080i`, also in `(…)`/`[…]` and as
//!   superscript/small-caps `ᴴᴰ` / `ꜰʜᴅ`, attached or not) → [`Quality`] (best one wins).
//! * Codec/format words (`HEVC H.265 H264 x265 HDR 50fps…`), any `[…]` group and `(…)` groups
//!   that are tags (`backup`, `backup 2`, `multi-sub`, `geo-blocked`…) → `tags`. Other `(…)`
//!   groups are part of the name (`BBC One (Scotland)`).
//! * Dangling separators (`|`, `-`, `:`…) at either end are dropped; whitespace collapses.
//! * Non-Latin text is untouched (no transliteration: display, not identity).

// Implemented: opt back into the lints that `lib.rs` silences for the still-stubbed `sources`.
#![warn(dead_code, unused_variables, unused_imports)]

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
pub fn clean_channel_name(raw: &str) -> CleanName {
    let c = clean(raw, false);
    if c.words.is_empty() {
        return CleanName { name: raw.trim().to_owned(), country: None, quality: None, tags: Vec::new() };
    }
    CleanName { name: c.words.join(" "), country: c.country, quality: c.quality, tags: c.tags }
}

/// Clean a movie/series title; extracts a trailing `(YYYY)` / `- YYYY` year.
pub fn clean_vod_name(raw: &str) -> (String, Option<u16>) {
    let c = clean(raw, true);
    if c.words.is_empty() {
        return (raw.trim().to_owned(), None);
    }
    (c.words.join(" "), c.year)
}

/// Clean a group/category name (`"UK| SPORTS ᴴᴰ"` → `"Sports"`, keeps country as prefix tag).
///
/// The country only survives in the group KEY (`identity::normalize_group` keeps it); the
/// display drops it like for channels. All-caps names are title-cased, keeping short
/// acronyms (`TV`, `NFL`) and lower-casing joining words (`NEWS & INFO` → `News & Info`).
pub fn clean_group_name(raw: &str) -> String {
    let c = clean(raw, false);
    if c.words.is_empty() {
        return raw.trim().to_owned();
    }
    let name = c.words.join(" ");
    let has_upper = name.chars().any(char::is_uppercase);
    let has_lower = name.chars().any(char::is_lowercase);
    if has_upper && !has_lower { title_case(&name) } else { name }
}

// ---------------------------------------------------------------------------------------------

/// Result of the shared cleanup pass.
#[derive(Default)]
struct Cleaned<'a> {
    words: Vec<&'a str>,
    country: Option<String>,
    quality: Option<Quality>,
    tags: Vec<String>,
    year: Option<u16>,
}

impl Cleaned<'_> {
    fn add_quality(&mut self, q: Quality) {
        if self.quality.is_none_or(|old| rank(q) > rank(old)) {
            self.quality = Some(q);
        }
    }
    fn add_country(&mut self, code: &str) {
        if self.country.is_none() {
            self.country = Some(code.to_ascii_uppercase());
        }
    }
}

/// Quality words → [`Quality`] (compared lower-case).
const QUALITY_WORDS: &[(&str, Quality)] = &[
    ("sd", Quality::Sd),
    ("480p", Quality::Sd),
    ("576p", Quality::Sd),
    ("hd", Quality::Hd),
    ("720p", Quality::Hd),
    ("fhd", Quality::Fhd),
    ("1080p", Quality::Fhd),
    ("1080i", Quality::Fhd),
    ("uhd", Quality::Uhd),
    ("4k", Quality::Uhd),
    ("2160p", Quality::Uhd),
    ("8k", Quality::Uhd),
];

/// Codec / format words stripped into `tags` (compared lower-case).
const CODEC_WORDS: &[&str] =
    &["hevc", "h265", "h.265", "h264", "h.264", "x265", "x264", "avc", "hdr", "hdr10", "hdr10+", "dolby", "50fps", "60fps", "25fps"];

/// `(…)` contents that are tags rather than part of the name (compared lower-case; `backup`
/// matches `backup 2`, `backup-1`…).
const PAREN_TAGS: &[&str] = &[
    "backup", "multi-sub", "multisub", "multi-audio", "multi audio", "multi", "vip", "geo-blocked", "not 24/7", "24/7", "opt-in", "dubbed",
    "sub", "subbed", "vostfr", "vo", "ppv", "test", "new", "alt",
];

/// Three-letter codes accepted as country/language prefixes (two letters are always codes).
const COUNTRY3: &[&str] = &[
    "usa", "uae", "ksa", "gbr", "deu", "ger", "fra", "ita", "esp", "spa", "por", "prt", "bra", "arg", "mex", "can", "aus", "ind", "pak", "tur",
    "rus", "ukr", "pol", "nld", "bel", "swe", "nor", "dnk", "den", "fin", "isl", "irl", "che", "sui", "aut", "gre", "grc", "cze", "svk", "hun",
    "rou", "rom", "bgr", "bul", "srb", "hrv", "cro", "bih", "slo", "svn", "mkd", "alb", "ara", "eng", "lat", "afr", "asi", "chn", "jpn", "kor",
    "phi", "tha", "vie", "idn", "mys", "isr", "irn", "irq", "egy", "mar", "tun", "dza", "alg", "nga", "zaf", "kur", "per", "chl", "col",
    "ven", "lux", "ltu", "lva", "est", "geo", "arm", "aze", "kaz", "uzb", "bgd", "lka", "npl", "afg", "syr", "lbn", "jor", "kwt", "qat", "bhr",
    "omn", "yem", "sau", "som", "eth", "ken", "gha", "cmr", "sen", "civ", "hin", "urd", "tam", "tel", "fre", "dut", "chi", "jap", "rum", "fil",
];

/// Separators that are noise when left dangling at either end. Not `+`: `Canal +` and
/// `Disney +` are channels of their own.
fn is_separator(w: &str) -> bool { !w.is_empty() && w.chars().all(|c| matches!(c, '|' | '-' | '–' | '—' | ':' | '·' | '•' | '/' | '~' | '*' | '_')) }

fn quality_of(w: &str) -> Option<Quality> {
    let w = w.trim();
    QUALITY_WORDS.iter().find(|(k, _)| w.eq_ignore_ascii_case(k)).map(|(_, q)| *q)
}

fn is_codec(w: &str) -> bool { CODEC_WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)) }

fn is_paren_tag(inner: &str) -> bool {
    let l = inner.trim().to_lowercase();
    PAREN_TAGS.contains(&l.as_str())
        || l.strip_prefix("backup").is_some_and(|rest| rest.trim_start_matches([' ', '-', '_']).chars().all(|c| c.is_ascii_digit()))
        || is_codec(&l)
}

fn is_country(code: &str) -> bool {
    match code.len() {
        2 => code.bytes().all(|b| b.is_ascii_alphabetic()),
        3 => code.bytes().all(|b| b.is_ascii_alphabetic()) && COUNTRY3.contains(&code.to_ascii_lowercase().as_str()),
        _ => false,
    }
}

fn year_of(w: &str) -> Option<u16> {
    let w = w.trim();
    if w.len() != 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse::<u16>().ok().filter(|y| (1900..=2100).contains(y))
}

fn rank(q: Quality) -> u8 {
    match q {
        Quality::Sd => 0,
        Quality::Hd => 1,
        Quality::Fhd => 2,
        Quality::Uhd => 3,
    }
}

/// Superscript / modifier / small-capital letters used as decoration (`ᴴᴰ`, `ꜰʜᴅ`, `⁴ᴷ`).
fn deco_to_ascii(c: char) -> Option<char> {
    Some(match c {
        'ᴬ' => 'A',
        'ᴮ' => 'B',
        'ᴰ' | 'ᵈ' | 'ᴅ' => 'D',
        'ᴱ' | 'ᴇ' => 'E',
        'ᶠ' | 'ꜰ' => 'F',
        'ᴳ' | 'ɢ' => 'G',
        'ᴴ' | 'ʰ' | 'ʜ' => 'H',
        'ᴵ' | 'ⁱ' | 'ɪ' => 'I',
        'ᴷ' | 'ᵏ' | 'ᴋ' => 'K',
        'ᴸ' | 'ʟ' => 'L',
        'ᴹ' | 'ᴍ' => 'M',
        'ᴺ' | 'ⁿ' | 'ɴ' => 'N',
        'ᴼ' | 'ᴏ' => 'O',
        'ᴾ' | 'ᵖ' | 'ᴘ' => 'P',
        'ᴿ' | 'ʀ' => 'R',
        'ˢ' | 'ꜱ' => 'S',
        'ᵀ' | 'ᴛ' => 'T',
        'ᵁ' | 'ᴜ' => 'U',
        'ⱽ' | 'ᵛ' | 'ᴠ' => 'V',
        'ᵂ' | 'ʷ' | 'ᴡ' => 'W',
        '⁰' => '0',
        '¹' => '1',
        '²' => '2',
        '³' => '3',
        '⁴' => '4',
        '⁵' => '5',
        '⁶' => '6',
        '⁷' => '7',
        '⁸' => '8',
        '⁹' => '9',
        '⁺' => '+',
        _ => return None,
    })
}

/// Split a trailing run of decoration characters off a word: `TF1ᴴᴰ` → (`TF1`, `HD`).
/// Returns `None` when the word has no decoration suffix.
fn split_deco(w: &str) -> Option<(&str, String)> {
    let start = w.char_indices().rev().take_while(|(_, c)| deco_to_ascii(*c).is_some()).last()?.0;
    Some((&w[..start], w[start..].chars().filter_map(deco_to_ascii).collect()))
}

/// A leading prefix of a name (see [`split_prefix`]).
enum Prefix<'a> {
    /// `[…]` — country, quality or tag.
    Bracket(&'a str),
    /// `|XX|`, `XX:`, `XX |`, `XX - ` — only accepted when `XX` is a country code.
    Code(&'a str),
}

fn split_prefix(s: &str) -> Option<(Prefix<'_>, &str)> {
    let b = s.as_bytes();
    if b.first() == Some(&b'[') {
        let close = s.find(']')?;
        return Some((Prefix::Bracket(&s[1..close]), &s[close + 1..]));
    }
    if b.first() == Some(&b'|') {
        let close = s[1..].find('|')? + 1;
        let code = s[1..close].trim();
        return is_country(code).then_some((Prefix::Code(code), &s[close + 1..]));
    }
    let letters = b.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    if !(2..=3).contains(&letters) {
        return None;
    }
    let code = &s[..letters];
    let after = &s[letters..];
    let trimmed = after.trim_start();
    if (trimmed.starts_with(':') || trimmed.starts_with('|')) && is_country(code) {
        return Some((Prefix::Code(code), &trimmed[1..]));
    }
    // `DE - Das Erste`: two upper-case letters and a spaced dash (avoids `Al - …` names).
    if letters == 2
        && code.bytes().all(|c| c.is_ascii_uppercase())
        && after.starts_with(char::is_whitespace)
        && let Some(rest) = trimmed.strip_prefix('-').or_else(|| trimmed.strip_prefix('–'))
        && rest.starts_with(char::is_whitespace)
    {
        return Some((Prefix::Code(code), rest));
    }
    None
}

/// Strip leading separators/whitespace left after a prefix.
fn trim_lead(s: &str) -> &str { s.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '|' | ':' | '-' | '–' | '·' | '•')) }

/// The shared pass: prefixes, then words and bracket groups.
fn clean(raw: &str, vod: bool) -> Cleaned<'_> {
    let mut out = Cleaned::default();
    let mut s = raw.trim();
    while let Some((prefix, rest)) = split_prefix(s) {
        let rest = trim_lead(rest);
        if rest.is_empty() {
            break; // the prefix is the whole name
        }
        match prefix {
            Prefix::Code(code) => out.add_country(code),
            Prefix::Bracket(inner) => {
                let inner = inner.trim();
                if let Some(q) = quality_of(inner) {
                    out.add_quality(q);
                } else if is_country(inner) {
                    out.add_country(inner);
                } else if !inner.is_empty() {
                    out.tags.push(inner.to_owned());
                }
            }
        }
        s = rest;
    }

    let mut rest = s;
    loop {
        rest = rest.trim_start();
        let Some(first) = rest.chars().next() else { break };
        let close = match first {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(close) = close
            && let Some(end) = rest[1..].find(close).map(|i| i + 1)
        {
            let inner = rest[1..end].trim();
            let group = &rest[..=end];
            rest = &rest[end + 1..];
            if let Some(q) = quality_of(inner) {
                out.add_quality(q);
            } else if let (true, Some(y)) = (vod, year_of(inner)) {
                out.year = Some(y);
            } else if first == '[' || is_paren_tag(inner) {
                if !inner.is_empty() {
                    out.tags.push(inner.to_owned());
                }
            } else {
                out.words.push(group);
            }
            continue;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = &rest[end..];
        let (word, deco) = match split_deco(word) {
            Some((w, d)) => (w, Some(d)),
            None => (word, None),
        };
        if let Some(d) = deco {
            if let Some(q) = quality_of(&d) {
                out.add_quality(q);
            } else if !d.is_empty() {
                out.tags.push(d);
            }
        }
        if word.is_empty() {
            continue;
        }
        if let Some(q) = quality_of(word) {
            out.add_quality(q);
        } else if is_codec(word) {
            out.tags.push(word.to_owned());
        } else {
            out.words.push(word);
        }
    }

    // Dangling separators, then a VOD `- 1999` suffix (and separators before it).
    loop {
        while out.words.first().is_some_and(|w| is_separator(w)) {
            out.words.remove(0);
        }
        while out.words.last().is_some_and(|w| is_separator(w)) {
            out.words.pop();
        }
        let n = out.words.len();
        if vod
            && out.year.is_none()
            && n >= 3
            && matches!(out.words[n - 2], "-" | "–")
            && let Some(y) = year_of(out.words[n - 1])
        {
            out.year = Some(y);
            out.words.truncate(n - 2);
            continue;
        }
        break;
    }
    out
}

/// Title-case an all-caps name, keeping acronyms and lower-casing joining words. Acronyms are
/// two-letter words (`TV`, `UK`) that are not common words, letter+digit words (`F1`, `MBC1`)
/// and a list of well-known longer ones; everything else (`NEW`, `ALL`, `TOP`) is a word.
fn title_case(s: &str) -> String {
    const SMALL: &[&str] = &["and", "the", "for", "of", "in", "on", "at", "to", "by", "with", "a", "an", "or", "de", "la", "le", "et"];
    const WORDS2: &[&str] = &["my", "go", "up", "no", "so", "do", "be", "he", "if", "is", "it", "we", "me"];
    const ACRONYMS: &[&str] = &[
        "nfl", "nba", "nhl", "mlb", "mls", "ufc", "wwe", "bbc", "itv", "hbo", "mtv", "cnn", "espn", "ppv", "vod", "usa", "vip", "xxx", "rtl",
        "ard", "zdf", "dazn", "bt", "mbc", "osn", "tnt", "amc", "abc", "nbc", "cbs", "fox", "pbs", "dstv", "iptv",
    ];
    let mut out = String::with_capacity(s.len());
    for (i, w) in s.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let lower = w.to_lowercase();
        let letters = w.chars().filter(|c| c.is_alphabetic()).count();
        let digits = w.chars().any(|c| c.is_ascii_digit());
        let core = lower.trim_matches(|c: char| !c.is_alphanumeric());
        let acronym = !SMALL.contains(&core)
            && ((letters == 2 && !digits && !WORDS2.contains(&core)) || (digits && letters <= 3) || ACRONYMS.contains(&core));
        if i > 0 && SMALL.contains(&lower.as_str()) {
            out.push_str(&lower);
        } else if acronym || letters == 0 {
            out.push_str(w);
        } else {
            let mut chars = lower.chars();
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
                out.push_str(chars.as_str());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(raw: &str) -> (String, Option<String>, Option<Quality>, Vec<String>) {
        let c = clean_channel_name(raw);
        (c.name, c.country, c.quality, c.tags)
    }

    #[test]
    fn channel_examples() {
        assert_eq!(ch("|EN| CNN HD (backup)"), ("CNN".into(), Some("EN".into()), Some(Quality::Hd), vec!["backup".into()]));
        assert_eq!(ch("[VIP] US: ESPN 2 4K"), ("ESPN 2".into(), Some("US".into()), Some(Quality::Uhd), vec!["VIP".into()]));
        assert_eq!(ch("DE - Das Erste HEVC"), ("Das Erste".into(), Some("DE".into()), None, vec!["HEVC".into()]));
        assert_eq!(ch("Sky Cinema +1 HD").0, "Sky Cinema +1");
        assert_eq!(ch("FR: TF1ᴴᴰ"), ("TF1".into(), Some("FR".into()), Some(Quality::Hd), vec![]));
        assert_eq!(ch("UK | ꜰʜᴅ Sky Sports Main Event").0, "Sky Sports Main Event");
        assert_eq!(ch("BBC News (720p) [Geo-blocked]"), ("BBC News".into(), None, Some(Quality::Hd), vec!["Geo-blocked".into()]));
        assert_eq!(ch("BBC One (Scotland) HD").0, "BBC One (Scotland)");
        assert_eq!(ch("CNN: Live").0, "CNN: Live");
        assert_eq!(ch("USA: CNN").0, "CNN");
        assert_eq!(ch("Al - Jazeera").0, "Al - Jazeera");
        assert_eq!(ch("UK | BBC One | FHD").0, "BBC One");
        assert_eq!(ch("Channel HD 4K SD").2, Some(Quality::Uhd));
        assert_eq!(ch("  Plain   Name  ").0, "Plain Name");
    }

    #[test]
    fn never_empty() {
        assert_eq!(ch("HD").0, "HD");
        assert_eq!(ch("[VIP]").0, "[VIP]");
        assert_eq!(ch("UK |").0, "UK");
        assert_eq!(ch(" UK: HD ").0, "UK: HD");
        assert_eq!(ch("").0, "");
        assert_eq!(ch("|").0, "|");
    }

    #[test]
    fn vod_names() {
        assert_eq!(clean_vod_name("The Matrix (1999) [MULTI-SUB]"), ("The Matrix".into(), Some(1999)));
        assert_eq!(clean_vod_name("EN - Inception - 2010 4K"), ("Inception".into(), Some(2010)));
        assert_eq!(clean_vod_name("Blade Runner 2049 (2017)"), ("Blade Runner 2049".into(), Some(2017)));
        assert_eq!(clean_vod_name("1917"), ("1917".into(), None));
        assert_eq!(clean_vod_name("Dune [2021]"), ("Dune".into(), Some(2021)));
        assert_eq!(clean_vod_name("Space (3000)"), ("Space (3000)".into(), None));
        assert_eq!(clean_vod_name("(2001)"), ("(2001)".into(), None));
    }

    #[test]
    fn group_names() {
        assert_eq!(clean_group_name("UK| SPORTS ᴴᴰ"), "Sports");
        assert_eq!(clean_group_name("NEWS & INFO"), "News & Info");
        assert_eq!(clean_group_name("US | NFL GAMES"), "NFL Games");
        assert_eq!(clean_group_name("TV SHOWS OF THE WORLD"), "TV Shows of the World");
        assert_eq!(clean_group_name("Documentaries"), "Documentaries");
        assert_eq!(clean_group_name("  "), "");
        assert_eq!(clean_group_name("ÉMISSIONS"), "Émissions");
        // short words are words, not acronyms, unless they look like one
        assert_eq!(clean_group_name("NEW RELEASES"), "New Releases");
        assert_eq!(clean_group_name("ALL MOVIES"), "All Movies");
        assert_eq!(clean_group_name("TOP 100"), "Top 100");
        assert_eq!(clean_group_name("MY LIST"), "My List");
        assert_eq!(clean_group_name("F1 & MOTOGP"), "F1 & Motogp");
        assert_eq!(clean_group_name("BBC CHANNELS (UK)"), "BBC Channels (UK)");
        assert_eq!(clean_group_name("ESPN PPV EVENTS"), "ESPN PPV Events");
    }

    #[test]
    fn plus_is_part_of_the_name() {
        assert_eq!(ch("Canal +").0, "Canal +");
        assert_eq!(ch("CANAL + HD"), ("CANAL +".into(), None, Some(Quality::Hd), vec![]));
        assert_eq!(ch("FR: Disney + 4K").0, "Disney +");
        assert_eq!(ch("UK | BBC One |").0, "BBC One");
    }
}
