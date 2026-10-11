//! Matching copies of one movie or series across playlists (DESIGN §29.10–§29.12, P2): the
//! match key of a provider's RAW name, the language / market of a copy, and the clustering
//! rules that turn matching copies into one stack in "All playlists".
//!
//! Display names never enter: smart names and the user's renames don't change a key.
//!
//! **Name → [`MatchInfo`]** ([`parse`]):
//! * Leading line-up prefixes, up to three: `EN ★ `, `4K-NF - `, `NF: `, `DE-TOP - `,
//!   `AR-SUBS - `, `UK/US | `. A code is taken when every part is a known quality / brand /
//!   label / marker word, or a known market code (not with `:` — `IT: Welcome to Derry` is a
//!   title), or when (code, separator) opens at least [`FREQUENT_PREFIX`] names of the
//!   playlist ([`PrefixCodes`]). A chained prefix uses the separator of the first (`EN - IT:
//!   Welcome to Derry`, `IN - KD – The Devil` keep their title words). Parts are split at `-`
//!   and `/`.
//! * The code minus quality, brand and label parts is the copy's **market / language tag**,
//!   spelled one way through [`MARKET_ALIASES`] (`ALB` = `AL`, `EAR` = `AR-SUBS`, `TAM` =
//!   `IN-TL`…). Dub / subtitle markers (`[مدبلج]`, `Dubbed`, `VOSTFR`, `Multi-Audio`, `VO`)
//!   join the tag as [`Dub`] instead of being dropped.
//! * `NN.` rank numbers after a `TOP` label come off (`DE-TOP - 38. Prinzessin Mononoke`).
//! * Bracket groups: a year is the name's year (`( 2024 )` too); quality / format-only groups
//!   (`[4K]`, `(HDR)`) and tags (`[Multi-Sub]`, `(CAM)`) go; a round `(XX)` ISO country is the
//!   copy's **origin** (`Lewis (2007) (GB)`), kept out of the key; every other bracket text
//!   stays in the key (`[Extended]`, `(and Everybody Else)`).
//! * A trailing `- 1999` is the name's year. Quality words come off only at the end of the
//!   title (`The Vision` keeps its word; `Dolby Vision` at the end goes).
//! * Dotted acronyms mark the key (`S.W.A.T.` never meets `Swat`); a `3D` prefix part too.
//! * The key is the title folded to lower-case letters and digits (Latin diacritics folded,
//!   other scripts kept).
//! * Year: the `year` column, else the name's.
//!
//! **Stacking** ([`cluster`]), per kind:
//! * Copies with the same key and the same known year stack — across languages and markets
//!   (user decision, §29.1): the tag only orders and labels copies inside the stack. A missing
//!   year never stacks by key; years are never fuzzy (off-by-one stays apart).
//! * Short, all-digit and stop-list titles (`It`, `1917`, `Live`) stack only with a year —
//!   which every key-stack has.
//! * **TMDB ids decide when present**: copies with the same id stack whatever their names or
//!   years; copies with different ids never do. Two different explicit origins (`(US)`,
//!   `(GB)`) never stack either (remakes: *The Office*).
//! * A copy the user split off ("Not the same title") stands alone.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::model::{ItemKind, Quality};
use crate::sources::category_tags::{self, WordTag};

/// Version of these rules: stored with the match columns (`meta`), a change recomputes them.
pub const RULES_VERSION: u32 = 1;

/// (code, separator) pairs that open at least this many names of one playlist are prefixes
/// even when the code is unknown (`EAR ★`, `KD ★`, `AR-AS-S - `).
pub const FREQUENT_PREFIX: usize = 20;

/// Spellings of the same market by different providers → one spelling (data: extend here).
pub const MARKET_ALIASES: &[(&str, &str)] = &[
    ("ALB", "AL"),
    ("SW", "SE"),
    ("LAT", "LA"),
    ("EXYU", "EX"),
    ("EX-YU", "EX"),
    ("QFR", "QC"),
    ("PER", "IR"),
    ("TAM", "IN-TL"),
    ("TEL", "IN-TU"),
    ("MAL", "IN-MM"),
    ("ML", "IN-MM"),
    ("KAN", "IN-KN"),
    ("PUN", "IN-PI"),
    ("GUJ", "IN-GU"),
    ("BL", "IN"),
    ("EAR", "AR-SUBS"),
    ("EG", "AR-EG"),
    ("MA", "AR-MA"),
    ("KH", "AR-KH"),
    ("TN", "AR-TN"),
    ("DZ", "AR-DZ"),
    ("LY", "AR-LY"),
    ("ENG", "EN"),
    ("UK", "EN"),
];

/// Prefix parts that are a quality / format claim.
const QUALITY_PARTS: &[&str] = &["4K", "UHD", "FHD", "HD", "SD", "HDR", "HDR10", "DV", "HEVC", "8K", "1080P", "720P", "2160P"];
/// Prefix parts that are a brand, service or line-up slot, not a language (`NF`, `AMZ`, `D+`).
const BRAND_PARTS: &[&str] = &[
    "NF", "AMZ", "D+", "A+", "P+", "PRMT", "UNV", "MAX", "NICK", "VIU", "DSC+", "DSC+S", "DSC+D", "VP", "SKY", "PCOK", "SHWT", "CR", "WIT", "TOD",
    "SH", "OSN+", "MRVL", "DWA", "FIFA+", "007", "JO", "MSH", "AJ", "F1", "UFC", "SOC", "XXX", "MV", "MULTI", "HBO", "HULU", "FB",
];
/// Line-up labels (`EN-TOP`, `AR-DOC-S`, `AR-XMAS`): not a language.
const LABEL_PARTS: &[&str] = &[
    "TOP", "DOC", "ANM", "KID", "KIDS", "XMAS", "CAM", "POD", "PTV", "ISL", "BDY", "HARA", "SHR", "GEO", "CHR", "MS", "ST", "E", "CH", "SHAM", "DO",
];
/// Prefix parts that make another version (kept in the key).
const MARKER_PARTS: &[&str] = &["3D"];

/// Title words (lower-case) that are a quality / format claim at the end of a title or alone
/// in brackets (`vision` only after `dolby`).
const QUALITY_WORDS: &[&str] = &[
    "sd", "hd", "fhd", "uhd", "4k", "8k", "480p", "576p", "720p", "1080p", "1080i", "1440p", "2160p", "3840p", "qhd", "hevc", "h265", "h.265",
    "h264", "h.264", "x265", "x264", "hdr", "hdr10", "hdr10+", "dv", "bluray", "blu-ray", "web-dl", "webrip", "remux", "dolby",
];
/// Bracket contents / trailing words that are dub markers (audio in the tag's language).
const DUB_WORDS: &[&str] = &["dubbed", "dub", "dubbing", "مدبلج", "دوبله", "vf", "vff", "doblada", "doblado", "synchro"];
/// Subtitle / original-audio markers.
const SUB_WORDS: &[&str] =
    &["vostfr", "vost", "vo", "sub", "subs", "subbed", "multi-sub", "multi-subs", "multisub", "multisubs", "multi sub", "مترجم", "زیرنویس"];
/// Several audio tracks.
const MULTI_WORDS: &[&str] = &["multi audio", "multi-audio", "multiaudio", "multi"];
/// Camera / telesync copies (played last).
const CAM_WORDS: &[&str] = &["cam", "ts", "hdcam", "hdts", "telesync", "camrip"];
/// Markers that end titles too (`Rub-a-Dub`, `The Sub`, `Spy Cam`): recognised in brackets and
/// prefixes only, never as the last word.
const AMBIGUOUS_AT_END: &[&str] = &["dub", "sub", "subs", "vo", "multi", "ts", "cam", "doblada", "doblado", "synchro"];
/// Other bracket tags that are no part of a title.
const OTHER_TAGS: &[&str] = &["backup", "new"];
/// Titles too generic to stack without a year.
const STOP: &[&str] = &[
    "movie",
    "movies",
    "film",
    "films",
    "trailer",
    "trailers",
    "test",
    "sample",
    "untitled",
    "unknown",
    "tba",
    "tbd",
    "comingsoon",
    "soon",
    "teaser",
    "promo",
    "intro",
    "preview",
    "episode",
    "series",
    "season",
    "show",
    "tv",
    "live",
    "vod",
    "channel",
    "null",
    "none",
    "new",
    "the",
];

/// Dub / subtitle marker of a copy (part of its language).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum Dub {
    #[default]
    None,
    /// Dubbed into the tag's language (`[مدبلج]`, `Dubbed`, `VF`).
    Dubbed,
    /// Original audio with subtitles (`VOSTFR`, `[Multi-Sub]`, `VO`).
    Subbed,
    /// Several audio tracks (`[Multi-Audio]`).
    Multi,
}

impl Dub {
    fn suffix(self) -> &'static str {
        match self {
            Dub::None => "",
            Dub::Dubbed => "+DUB",
            Dub::Subbed => "+SUB",
            Dub::Multi => "+MULTI",
        }
    }

    /// Short label for chips ("dub", "sub", "multi").
    pub fn label(self) -> Option<&'static str> {
        match self {
            Dub::None => None,
            Dub::Dubbed => Some("dub"),
            Dub::Subbed => Some("sub"),
            Dub::Multi => Some("multi"),
        }
    }
}

/// Version facts of a copy from its name (bit set, stored as `match_tags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VersionTags(pub u16);

impl VersionTags {
    const QUALITY: u16 = 0b111;
    pub const HDR: u16 = 1 << 3;
    pub const DV: u16 = 1 << 4;
    /// Camera / telesync copy.
    pub const CAM: u16 = 1 << 5;
    pub const THREE_D: u16 = 1 << 6;

    /// The best quality word of the name (`4K`, `FHD`…).
    pub fn quality(self) -> Option<Quality> {
        Quality::from_code(i64::from(self.0 & Self::QUALITY))
    }
    pub fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
    pub fn is_cam(self) -> bool {
        self.has(Self::CAM)
    }
    fn add_quality(&mut self, q: Quality) {
        if self.quality().is_none_or(|old| q > old) {
            self.0 = (self.0 & !Self::QUALITY) | (q.code() as u16 & Self::QUALITY);
        }
    }
    /// Read a quality / format word (`4k`, `2160p`, `hdr`, `dv`) into the set.
    fn add_word(&mut self, w: &str) {
        let w = w.to_ascii_lowercase();
        if let Some(q) = crate::sources::name_tokens::quality_word(&w) {
            self.add_quality(q);
        }
        match w.as_str() {
            "hdr" | "hdr10" | "hdr10+" => self.0 |= Self::HDR,
            "dv" | "vision" => self.0 |= Self::DV,
            _ => {}
        }
    }
}

/// Why a copy can't stack by its key (it may still stack by TMDB id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Neither the `year` column nor the name has a year.
    NoYear,
    /// Nothing is left of the title.
    Empty,
}

/// What a copy's name says for matching.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchInfo {
    /// The folded title plus version markers (`thematrix`, `avatar|3d`); empty when nothing is
    /// left.
    pub key: String,
    /// The `year` column, else the year in the name.
    pub year: Option<u16>,
    /// Market / language code of the prefix after [`MARKET_ALIASES`] (`EN`, `AR-SUBS`,
    /// `IN-TL`); `None` = no tag (no prefix, or brands only).
    pub lang: Option<String>,
    pub dub: Dub,
    /// Origin country in round brackets (`(GB)`), kept out of the key.
    pub origin: Option<[u8; 2]>,
    pub tags: VersionTags,
    /// The title is short (< 3), all digits or on the stop list (stacks only with a year).
    pub generic: bool,
}

impl MatchInfo {
    /// Why this copy can't stack by key, if it can't.
    pub fn refusal(&self) -> Option<Refusal> {
        if self.key.is_empty() {
            Some(Refusal::Empty)
        } else if self.year.is_none() {
            Some(Refusal::NoYear)
        } else {
            None
        }
    }

    /// The stored stacking hash (`match_hash`): blake3 over kind, key and year, separated, cut
    /// to 64 bits (strong: two unrelated titles never share it by accident). `None` = refused.
    pub fn stack_hash(&self, kind: ItemKind) -> Option<i64> {
        self.refusal().is_none().then(|| stack_hash(kind, &self.key, self.year.unwrap_or(0)))
    }

    /// The stored language text (`match_lang`): `FR`, `FR+SUB`, `+MULTI`; `None` without either.
    pub fn lang_text(&self) -> Option<String> {
        lang_text(self.lang.as_deref(), self.dub)
    }

    /// `(XX)` origin as text.
    pub fn origin_text(&self) -> Option<String> {
        self.origin.map(|o| String::from_utf8_lossy(&o).into_owned())
    }
}

/// `FR` + Subbed → `FR+SUB`; `None` + Multi → `+MULTI`.
pub fn lang_text(lang: Option<&str>, dub: Dub) -> Option<String> {
    match (lang, dub) {
        (None, Dub::None) => None,
        (l, d) => Some(format!("{}{}", l.unwrap_or(""), d.suffix())),
    }
}

/// `FR+SUB` → (`Some("FR")`, Subbed); the inverse of [`lang_text`].
pub fn split_lang_text(s: &str) -> (Option<&str>, Dub) {
    let (code, dub) = match s.rfind('+') {
        Some(i) if matches!(&s[i..], "+DUB" | "+SUB" | "+MULTI") => (
            &s[..i],
            match &s[i..] {
                "+DUB" => Dub::Dubbed,
                "+SUB" => Dub::Subbed,
                _ => Dub::Multi,
            },
        ),
        _ => (s, Dub::None),
    };
    ((!code.is_empty()).then_some(code), dub)
}

/// The stacking hash of (kind, key, year).
pub fn stack_hash(kind: ItemKind, key: &str, year: u16) -> i64 {
    let mut h = blake3::Hasher::new();
    h.update(kind.as_str().as_bytes());
    h.update(&[0]);
    h.update(key.as_bytes());
    h.update(&[0]);
    h.update(&year.to_le_bytes());
    let b = h.finalize();
    i64::from_le_bytes(b.as_bytes()[..8].try_into().unwrap_or_default())
}

/// The prefixes one playlist uses often enough to be read as prefixes even when the code is
/// unknown (`EAR ★`, `KD ★`): (code, separator class) opening ≥ [`FREQUENT_PREFIX`] names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrefixCodes {
    frequent: HashSet<(String, &'static str)>,
}

impl PrefixCodes {
    /// Count the first prefix of each name of a playlist (movies and series together).
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut counts: HashMap<(String, &'static str), usize> = HashMap::new();
        for name in names {
            let name = name.trim();
            if let Some(c) = prefix_re().captures(name) {
                let code = &c[1];
                if code.chars().any(|ch| ch.is_ascii_alphabetic()) {
                    *counts.entry((code.to_owned(), sep_class(&c[2]))).or_default() += 1;
                }
            }
        }
        Self { frequent: counts.into_iter().filter(|(_, n)| *n >= FREQUENT_PREFIX).map(|(k, _)| k).collect() }
    }

    fn is_frequent(&self, code: &str, sep: &'static str) -> bool {
        self.frequent.contains(&(code.to_owned(), sep))
    }

    /// A stable fingerprint (stored per playlist: a change recomputes its keys).
    pub fn fingerprint(&self) -> u64 {
        let mut v: Vec<String> = self.frequent.iter().map(|(c, s)| format!("{c}\u{1}{s}")).collect();
        v.sort_unstable();
        let h = blake3::hash(v.join("\u{2}").as_bytes());
        u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or_default())
    }

    pub fn len(&self) -> usize {
        self.frequent.len()
    }
    pub fn is_empty(&self) -> bool {
        self.frequent.is_empty()
    }
}

fn prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*([A-Z0-9][A-Z0-9+]*(?:[-/][A-Z0-9+]+)*)(\s*[★✪◉❖|]\s*|\s*:\s+|\s+[-–]\s+)").expect("prefix regex"))
}

fn bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\(\[]([^\(\)\[\]]*)[\)\]]").expect("bracket regex"))
}

fn trail_year_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*[-–]\s*((?:19|20)\d\d)\s*$").expect("year regex"))
}

fn broken_quality_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\[\s*(4k|hd|fhd|uhd)\s*\[?\s*$").expect("broken bracket regex"))
}

fn rank_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,3}\.\s*").expect("rank regex"))
}

fn acronym_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:\b\w\.){2,}").expect("acronym regex"))
}

/// Separator class: the decorations are one class (`★`, `✪`, `|`…), else the trimmed text
/// (`-`, `–`, `:`), so `-` and `–` differ.
fn sep_class(sep: &str) -> &'static str {
    match sep.trim() {
        "-" => "-",
        "–" => "–",
        ":" => ":",
        _ => "star",
    }
}

fn year_in(s: &str) -> Option<u16> {
    let s = s.trim();
    (s.len() == 4 && (s.starts_with("19") || s.starts_with("20")) && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

fn in_list(list: &[&str], w: &str) -> bool {
    list.contains(&w)
}

fn is_known_part(p: &str) -> bool {
    in_list(QUALITY_PARTS, p) || in_list(BRAND_PARTS, p) || in_list(LABEL_PARTS, p) || in_list(MARKER_PARTS, p)
}

/// A market / language code of a prefix: the alias table, or a country / language / region
/// code of the category tags (`EN`, `DE`, `AR`, `LATAM`).
fn is_market_part(p: &str) -> bool {
    MARKET_ALIASES.iter().any(|(a, b)| *a == p || b.split('-').any(|x| x == p)) || matches!(category_tags::classify(p), WordTag::Tag(_))
}

/// Does `after` (the text after a prefix) still name something: a word outside brackets and a
/// trailing year?
fn has_title_word(after: &str) -> bool {
    let no_brackets = bracket_re().replace_all(after, "");
    let no_year = trail_year_re().replace(&no_brackets, "");
    no_year.chars().any(char::is_alphanumeric)
}

/// Strip leading prefixes; returns (codes, rest).
fn split_prefixes<'a>(name: &'a str, codes: &PrefixCodes) -> (Vec<&'a str>, &'a str) {
    let mut found = Vec::new();
    let mut rest = name;
    let mut first_sep: Option<&'static str> = None;
    for _ in 0..3 {
        let Some(c) = prefix_re().captures(rest) else { break };
        let (Some(code), Some(sep), Some(all)) = (c.get(1), c.get(2), c.get(0)) else { break };
        let code = code.as_str();
        let sep = sep_class(sep.as_str());
        let after = &rest[all.end()..];
        if !has_title_word(after) || !code.chars().any(|ch| ch.is_ascii_alphabetic()) || first_sep.is_some_and(|f| f != sep) {
            break;
        }
        let parts = code.split(['-', '/']);
        let known = parts.clone().all(is_known_part);
        // a market code with `:` is a title (`IT: Welcome to Derry`) unless the playlist uses it
        let market = sep != ":" && parts.clone().all(|p| is_known_part(p) || is_market_part(p));
        if !(known || market || codes.is_frequent(code, sep)) {
            break;
        }
        first_sep = Some(sep);
        found.push(code);
        rest = after;
    }
    (found, rest)
}

/// Fold a title to its key: lower-case letters and digits, Latin diacritics folded, marks and
/// punctuation dropped, other scripts kept.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
            }
            continue;
        }
        if matches!(get_general_category(c), GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark) {
            continue;
        }
        if !c.is_alphanumeric() {
            continue;
        }
        let u = u32::from(c);
        let latin = (0xC0..0x250).contains(&u) || (0x1E00..0x1F00).contains(&u);
        if latin {
            for d in deunicode::deunicode_char(c).unwrap_or("").chars() {
                if d.is_ascii_alphanumeric() {
                    out.push(d.to_ascii_lowercase());
                }
            }
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Classify one bracket's contents; returns what replaces it (`None` = drop, `Some` = keep).
fn bracket(inner: &str, round: bool, info: &mut MatchInfo, name_year: &mut Option<u16>) -> bool {
    let t = inner.trim();
    if let Some(y) = year_in(t) {
        name_year.get_or_insert(y);
        return false;
    }
    let lower = t.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| c.is_whitespace() || "/@,+-_.".contains(c)).filter(|w| !w.is_empty()).collect();
    let fps = |w: &str| w.strip_suffix("fps").is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()));
    if words.is_empty() || words.iter().all(|w| in_list(QUALITY_WORDS, w) || *w == "vision" || fps(w)) {
        for w in &words {
            info.tags.add_word(w);
        }
        return false;
    }
    if marker(&lower, info) {
        return false;
    }
    if in_list(OTHER_TAGS, &lower) {
        return false;
    }
    // `(GB)`, `(UK)` → origin GB (a language `(EN)` is no origin)
    if round
        && t.len() == 2
        && t.bytes().all(|b| b.is_ascii_uppercase())
        && let WordTag::Tag(tag) = category_tags::classify(t)
        && tag.kind() == crate::model::TagKind::Country
        && let [a, b] = tag.code().as_bytes()
    {
        info.origin = Some([*a, *b]);
        return false;
    }
    true
}

/// A dub / sub / multi / cam marker word (lower-case): recorded, true.
fn marker(lower: &str, info: &mut MatchInfo) -> bool {
    let dub = if in_list(DUB_WORDS, lower) {
        Dub::Dubbed
    } else if in_list(SUB_WORDS, lower) {
        Dub::Subbed
    } else if in_list(MULTI_WORDS, lower) {
        Dub::Multi
    } else if in_list(CAM_WORDS, lower) {
        info.tags.0 |= VersionTags::CAM;
        return true;
    } else {
        return false;
    };
    // the strongest statement wins: a dub over multi-audio over subtitles
    let rank = |d: Dub| match d {
        Dub::None => 0,
        Dub::Subbed => 1,
        Dub::Multi => 2,
        Dub::Dubbed => 3,
    };
    if rank(dub) > rank(info.dub) {
        info.dub = dub;
    }
    true
}

/// Read a provider name (see the module docs). `year_col` is the stored `year`.
pub fn parse(raw: &str, year_col: Option<u16>, codes: &PrefixCodes) -> MatchInfo {
    let name = raw.replace('\u{a0}', " ");
    let (prefixes, rest) = split_prefixes(name.trim(), codes);
    let mut info = MatchInfo::default();
    let mut markers: Vec<&str> = Vec::new();
    let mut top = false;
    for code in &prefixes {
        let mut kept: Vec<&str> = Vec::new();
        for p in code.split(['-', '/']) {
            if in_list(QUALITY_PARTS, p) {
                info.tags.add_word(p);
            } else if in_list(LABEL_PARTS, p) {
                top |= p == "TOP";
                if p == "CAM" {
                    info.tags.0 |= VersionTags::CAM;
                }
            } else if in_list(MARKER_PARTS, p) {
                markers.push("3d");
                info.tags.0 |= VersionTags::THREE_D;
            } else if !in_list(BRAND_PARTS, p) {
                kept.push(p);
            }
        }
        if !kept.is_empty() {
            info.lang = Some(kept.join("-"));
        }
    }
    if let Some(l) = info.lang.take() {
        info.lang = Some(MARKET_ALIASES.iter().find(|(a, _)| *a == l).map_or(l, |(_, b)| (*b).to_owned()));
    }
    let mut rest = rest.to_owned();
    if top && let Some(m) = rank_re().find(&rest) {
        rest.replace_range(..m.end(), "");
    }
    if let Some(m) = broken_quality_re().captures(&rest) {
        info.tags.add_word(&m[1]);
        let at = m.get(0).map_or(rest.len(), |g| g.start());
        rest.truncate(at);
    }
    let mut name_year = None;
    let mut plain = String::with_capacity(rest.len());
    let mut last = 0;
    for c in bracket_re().captures_iter(&rest) {
        let (Some(all), Some(inner)) = (c.get(0), c.get(1)) else { continue };
        plain.push_str(&rest[last..all.start()]);
        if bracket(inner.as_str(), all.as_str().starts_with('('), &mut info, &mut name_year) {
            plain.push(' ');
            plain.push_str(inner.as_str());
        }
        plain.push(' ');
        last = all.end();
    }
    plain.push_str(&rest[last..]);
    if let Some(c) = trail_year_re().captures(&plain) {
        if let Some(y) = c.get(1).and_then(|y| y.as_str().parse().ok()) {
            name_year.get_or_insert(y);
        }
        let at = c.get(0).map_or(plain.len(), |g| g.start());
        plain.truncate(at);
    }
    // quality, dub / sub and cam words at the end of the title only
    let mut words: Vec<&str> = plain.split_whitespace().collect();
    while let Some(w) = words.last() {
        let bare = w.trim_matches(|c: char| ".,:;!?-–|".contains(c)).to_lowercase();
        if bare.is_empty() {
            words.pop();
            continue;
        }
        let dolby_vision = bare == "vision" && words.len() > 1 && words[words.len() - 2].eq_ignore_ascii_case("dolby");
        if in_list(QUALITY_WORDS, &bare) || dolby_vision {
            info.tags.add_word(&bare);
            words.pop();
        } else if words.len() > 1 && !in_list(AMBIGUOUS_AT_END, &bare) && marker(&bare, &mut info) {
            words.pop();
        } else {
            break;
        }
    }
    let title = words.join(" ");
    if acronym_re().is_match(&title) {
        markers.push("acr");
    }
    let base = fold(&title);
    info.generic = base.chars().count() < 3 || base.bytes().all(|b| b.is_ascii_digit()) || in_list(STOP, &base);
    info.key = if base.is_empty() || markers.is_empty() { base } else { format!("{base}|{}", markers.join("|")) };
    info.year = year_col.or(name_year);
    info
}

// ---------------------------------------------------------------------------------------------
// Languages

/// ISO 639-2 → 639-1 for the codes of `player.audio_langs` (`eng` → `en`).
pub fn norm_lang(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    let two = match c.as_str() {
        "eng" => "en",
        "ger" | "deu" => "de",
        "fre" | "fra" => "fr",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "dut" | "nld" => "nl",
        "ara" => "ar",
        "tur" => "tr",
        "pol" => "pl",
        "rus" => "ru",
        "gre" | "ell" => "el",
        "swe" => "sv",
        "nor" | "nob" | "nno" => "no",
        "dan" => "da",
        "fin" => "fi",
        "rum" | "ron" => "ro",
        "hun" => "hu",
        "cze" | "ces" => "cs",
        "slo" | "slk" => "sk",
        "slv" => "sl",
        "bul" => "bg",
        "alb" | "sqi" => "sq",
        "hin" => "hi",
        "per" | "fas" => "fa",
        "heb" => "he",
        "jpn" => "ja",
        "kor" => "ko",
        "chi" | "zho" => "zh",
        "kur" => "ku",
        "tam" => "ta",
        "tel" => "te",
        "mal" => "ml",
        "kan" => "kn",
        "pan" => "pa",
        "guj" => "gu",
        "srp" => "sr",
        "hrv" => "hr",
        "ukr" => "uk",
        "tha" => "th",
        "vie" => "vi",
        "ind" => "id",
        "fil" | "tgl" => "tl",
        "som" => "so",
        "urd" => "ur",
        "ben" => "bn",
        other => return other.to_owned(),
    };
    two.to_owned()
}

/// The audio language a copy's tag promises (ISO 639-1), if it says one: `EN` → en, `DE+DUB`
/// → de, `GR` → el, `IN-TL` → ta; subtitled markets (`AR-SUBS`, `KU-S`, `+SUB`: original
/// audio) and regions without one language (`SC`, `EX`) → `None`.
pub fn audio_lang(code: &str, dub: Dub) -> Option<&'static str> {
    if dub == Dub::Subbed {
        return None;
    }
    let mut parts = code.split('-');
    let first = parts.next()?;
    let rest: Vec<&str> = parts.collect();
    if rest.iter().any(|p| matches!(*p, "SUBS" | "SUB" | "S")) {
        return None;
    }
    if first == "IN" {
        return Some(match rest.first().copied() {
            Some("TL") | Some("TM") => "ta",
            Some("TU") => "te",
            Some("MM") => "ml",
            Some("KN") => "kn",
            Some("PI") => "pa",
            Some("GU") => "gu",
            // "IN-EN" is the Hindi-dubbed shelf ("EN Hindi", "India EN Dubbed"), not English
            _ => "hi",
        });
    }
    match first {
        "LA" => return Some("es"),
        "QC" => return Some("fr"),
        "GR" => return Some("el"),
        "SE" => return Some("sv"),
        "BR" => return Some("pt"),
        "KU" => return Some("ku"),
        "IR" => return Some("fa"),
        _ => {}
    }
    match category_tags::classify(first) {
        WordTag::Tag(t) => match t.kind() {
            crate::model::TagKind::Language => LANGS.iter().find(|l| **l == t.code()).copied(),
            crate::model::TagKind::Country => category_tags::country_language(t.code()),
            crate::model::TagKind::Region => None,
        },
        _ => None,
    }
}

/// The language a copy's tag is FOR (ISO 639-1): its audio language, or for a subtitled market
/// (`AR-SUBS`, `KU-S`, `NL+SUB`) the market's language — its subtitles and its shelf. `None`
/// for multi-audio copies and regions without one language (`SC`); Ex-Yugoslav (`EX`) is
/// `sh` ([`category_tags::SERBO_CROATIAN`]). What "Also hide titles
/// in other languages" reads (DESIGN §28.12).
pub fn market_lang(code: &str, dub: Dub) -> Option<&'static str> {
    if dub == Dub::Multi {
        return None;
    }
    let base: Vec<&str> = code.split('-').filter(|p| !matches!(*p, "SUBS" | "SUB" | "S")).collect();
    match base.first() {
        Some(&"EX") => Some(category_tags::SERBO_CROATIAN),
        _ => audio_lang(&base.join("-"), Dub::None),
    }
}

/// A subtitle mark with no market (`SUBS`, `SUB`): the copy is for its category's language
/// ("Also hide titles in other languages", DESIGN §28.12).
pub fn is_bare_subtitles(code: &str) -> bool {
    !code.is_empty() && code.split('-').all(|p| matches!(p, "SUBS" | "SUB" | "S"))
}

/// Language codes [`audio_lang`] can return from a language tag.
const LANGS: &[&str] = &[
    "en", "ar", "fr", "de", "it", "es", "pt", "tr", "ru", "pl", "nl", "el", "hi", "ur", "ta", "or", "te", "pa", "bn", "ml", "kn", "mr", "gu", "ku",
    "fa", "zh", "ja", "ko", "he", "so", "tl", "sq", "hy",
];

/// How well a copy's language suits the user (lower first): the index of its audio language in
/// `prefs` (normalised [`norm_lang`] codes), then untagged or multi-audio copies, then copies
/// whose audio the tag doesn't say (subtitled markets), then other languages.
pub fn lang_rank(lang: Option<&str>, dub: Dub, prefs: &[String]) -> u8 {
    let n = prefs.len().min(200) as u8;
    let Some(code) = lang.filter(|c| !c.is_empty()) else {
        return if dub == Dub::Subbed { n + 1 } else { n };
    };
    match audio_lang(code, dub) {
        Some(a) => prefs.iter().position(|p| p == a).map_or(n + 2, |i| i as u8),
        None if dub == Dub::Multi => n,
        None => n + 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Clustering

/// One copy as the clustering sees it (only copies that may stack: visible, in All, not
/// split off by the user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterItem {
    /// [`MatchInfo::stack_hash`] (`None` = refused: stacks only by TMDB id).
    pub hash: Option<i64>,
    pub tmdb: Option<u32>,
    pub origin: Option<[u8; 2]>,
}

/// Stacks of two or more copies: indices into `items`, each stack in ascending order, stacks
/// ordered by their first member. Rules: module docs. O(n log n) (one sort of the hashes); no
/// allocation per copy or per stack.
pub fn cluster_flat(items: &[ClusterItem]) -> Clusters {
    // 1. same key + year, split where TMDB ids or origins conflict
    let mut by_hash: Vec<(i64, u32)> = items.iter().enumerate().filter_map(|(i, it)| Some((it.hash?, i as u32))).collect();
    by_hash.sort_unstable();
    let mut parent: Vec<u32> = (0..items.len() as u32).collect();
    let mut run: Vec<u32> = Vec::new();
    let mut start = 0;
    while start < by_hash.len() {
        let h = by_hash[start].0;
        let mut end = start + 1;
        while end < by_hash.len() && by_hash[end].0 == h {
            end += 1;
        }
        if end - start > 1 {
            run.clear();
            run.extend(by_hash[start..end].iter().map(|(_, i)| *i));
            if conflicts(items, &run) {
                for part in split_conflicts(items, &run) {
                    for w in part.windows(2) {
                        union(&mut parent, w[0], w[1]);
                    }
                }
            } else {
                for w in run.windows(2) {
                    union(&mut parent, w[0], w[1]);
                }
            }
        }
        start = end;
    }
    // 2. the same TMDB id stacks, whatever the names say
    let mut by_tmdb: Vec<(u32, u32)> = items.iter().enumerate().filter_map(|(i, it)| Some((it.tmdb?, i as u32))).collect();
    by_tmdb.sort_unstable();
    for w in by_tmdb.windows(2) {
        if w[0].0 == w[1].0 {
            union(&mut parent, w[0].1, w[1].1);
        }
    }
    // 3. collect (a merge through TMDB never joins two different ids: each key part has ≤ 1);
    // a root is its stack's smallest index, so stacks come out ordered by their first member
    let n = items.len();
    let mut size = vec![0u32; n];
    for i in 0..n as u32 {
        let r = find(&mut parent, i);
        parent[i as usize] = r;
        size[r as usize] += 1;
    }
    // stack starts in `flat`, in root order (prefix sums over the roots of real stacks)
    let mut slot = vec![u32::MAX; n];
    let mut spans: Vec<(u32, u32)> = Vec::new();
    let mut total = 0u32;
    for r in 0..n {
        if parent[r] as usize == r && size[r] > 1 {
            slot[r] = spans.len() as u32;
            spans.push((total, total));
            total += size[r];
        }
    }
    let mut flat = vec![0u32; total as usize];
    for (i, &r) in parent.iter().enumerate() {
        let r = r as usize;
        if size[r] > 1 {
            let span = &mut spans[slot[r] as usize];
            flat[span.1 as usize] = i as u32;
            span.1 += 1;
        }
    }
    Clusters { flat, spans }
}

/// Stacks as one flat list: `flat[a..b]` for each `(a, b)` of `spans` (ascending members,
/// stacks ordered by their first member).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Clusters {
    pub flat: Vec<u32>,
    pub spans: Vec<(u32, u32)>,
}

impl Clusters {
    pub fn iter(&self) -> impl Iterator<Item = &[u32]> {
        self.spans.iter().map(|&(a, b)| &self.flat[a as usize..b as usize])
    }
    pub fn len(&self) -> usize {
        self.spans.len()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// [`cluster_flat`] as one vector per stack (tests, reports).
pub fn cluster(items: &[ClusterItem]) -> Vec<Vec<u32>> {
    cluster_flat(items).iter().map(<[u32]>::to_vec).collect()
}

/// Does a same-key run hold two different TMDB ids or two different origins?
fn conflicts(items: &[ClusterItem], run: &[u32]) -> bool {
    let (mut tmdb, mut origin) = (None, None);
    for &i in run {
        let it = &items[i as usize];
        if let Some(t) = it.tmdb {
            if tmdb.is_some_and(|x| x != t) {
                return true;
            }
            tmdb = Some(t);
        }
        if let Some(o) = it.origin {
            if origin.is_some_and(|x| x != o) {
                return true;
            }
            origin = Some(o);
        }
    }
    false
}

/// Split one same-key run where copies conflict: two or more TMDB ids → one part per id (and
/// one for the copies without); else two or more origins → one part per origin (and one for
/// the copies without); else the whole run.
fn split_conflicts(items: &[ClusterItem], run: &[u32]) -> Vec<Vec<u32>> {
    fn parts<K: Eq + std::hash::Hash + Copy>(run: &[u32], key: impl Fn(u32) -> Option<K>) -> Option<Vec<Vec<u32>>> {
        let distinct: HashSet<K> = run.iter().filter_map(|i| key(*i)).collect();
        if distinct.len() < 2 {
            return None;
        }
        let mut by: HashMap<Option<K>, Vec<u32>> = HashMap::new();
        for &i in run {
            by.entry(key(i)).or_default().push(i);
        }
        Some(by.into_values().collect())
    }
    if let Some(p) = parts(run, |i| items[i as usize].tmdb) {
        // inside each TMDB part an origin conflict can't happen by TMDB, but can between the
        // id-less copies
        return p.into_iter().flat_map(|part| parts(&part, |i| items[i as usize].origin).unwrap_or_else(|| vec![part])).collect();
    }
    parts(run, |i| items[i as usize].origin).unwrap_or_else(|| vec![run.to_vec()])
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        let p = parent[i as usize];
        parent[i as usize] = parent[p as usize];
        i = p;
    }
    i
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[hi as usize] = lo;
    }
}

/// A TMDB id as sent by a provider (`"603"`, `603`, `"0"`, `""`, `"tt0133093"`) → a positive
/// number, else `None`.
pub fn tmdb_id(s: &str) -> Option<u32> {
    s.trim().parse::<u64>().ok().filter(|n| *n > 0).and_then(|n| u32::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(raw: &str, year: Option<u16>) -> MatchInfo {
        parse(raw, year, &PrefixCodes::default())
    }

    /// (raw name, year column, key, year, lang, origin).
    type Case<'a> = (&'a str, Option<u16>, &'a str, Option<u16>, Option<&'a str>, Option<&'a str>);

    /// (raw name, year column) → (key, year, lang text, origin): the provider spellings of the
    /// dedup report, one row per rule.
    #[test]
    fn names_to_keys() {
        let cases: &[Case<'_>] = &[
            // prefixes: star / spaced dash / brand colon, chained codes, quality and brand parts
            ("EN ★ Captain America  Civil War - 2016 [4k]", None, "captainamericacivilwar", Some(2016), Some("EN"), None),
            ("EN - Captain America: Civil War (2016)", None, "captainamericacivilwar", Some(2016), Some("EN"), None),
            ("4K-MRVL - Captain America: Civil War (2016)", None, "captainamericacivilwar", Some(2016), None, None),
            ("NF: Code 8 Part II (2024)", None, "code8partii", Some(2024), None, None),
            ("EN ★Incoming - 2024", None, "incoming", Some(2024), Some("EN"), None),
            ("AS★ Jolly LLB 3 - 2025", None, "jollyllb3", Some(2025), Some("AS"), None),
            // market aliases: one spelling per market
            ("ALB ★ Jigra - 2024", None, "jigra", Some(2024), Some("AL"), None),
            ("EAR ★ Upgrade - 2018", None, "upgrade", Some(2018), Some("AR-SUBS"), None),
            ("TAM ★ Leo - 2023", None, "leo", Some(2023), Some("IN-TL"), None),
            // '/' in prefix codes
            ("UK/US - The Crown (2016)", None, "thecrown", Some(2016), Some("UK-US"), None),
            // a chained prefix must use the first separator: the second code is title text
            ("EN - IT: Welcome to Derry (2025)", None, "itwelcometoderry", Some(2025), Some("EN"), None),
            ("IN - KD – The Devil (2025)", None, "kdthedevil", Some(2025), Some("IN"), None),
            // a market code with `:` is a title unless the playlist uses it
            ("IT: Welcome to Derry (2025)", None, "itwelcometoderry", Some(2025), None, None),
            // TOP rank numbers
            ("DE-TOP - 38. Prinzessin Mononoke (1997)", None, "prinzessinmononoke", Some(1997), Some("DE"), None),
            // a trailing (XX) origin leaves the key; other bracket text stays
            ("EN - Lewis (2007) (GB)", None, "lewis", Some(2007), Some("EN"), Some("GB")),
            ("EN - The Office (UK) (2005)", None, "theoffice", Some(2005), Some("EN"), Some("GB")),
            ("Hello (EN) (2005)", None, "helloen", Some(2005), None, None),
            ("EN ★ Blade Runner [Final Cut] - 1982", None, "bladerunnerfinalcut", Some(1982), Some("EN"), None),
            ("EN - Me (and Everybody Else) (2020)", None, "meandeverybodyelse", Some(2020), Some("EN"), None),
            // the year: the column first, else `( 2024 )`, a broken `[4k[`, a trailing `- 2024`
            ("FR - Pillion ( 2024 )", None, "pillion", Some(2024), Some("FR"), None),
            ("EN ★ The Order - 2024 [4k[", None, "theorder", Some(2024), Some("EN"), None),
            ("EN ★ Arrival", Some(2016), "arrival", Some(2016), Some("EN"), None),
            // quality words only at the end or alone in brackets
            ("EN - The Vision (2020)", None, "thevision", Some(2020), Some("EN"), None),
            ("EN - HD Mapping Explained (2020)", None, "hdmappingexplained", Some(2020), Some("EN"), None),
            ("EN - Nature Dolby Vision (2021)", None, "nature", Some(2021), Some("EN"), None),
            ("EN - Sin City HD (2005)", None, "sincity", Some(2005), Some("EN"), None),
            // dotted acronyms and 3D versions mark the key
            ("EN - S.W.A.T. (2017)", None, "swat|acr", Some(2017), Some("EN"), None),
            ("EN - SWAT (2017)", None, "swat", Some(2017), Some("EN"), None),
            ("3D-EN - Avatar (2009)", None, "avatar|3d", Some(2009), Some("EN"), None),
            // diacritics fold, other scripts stay
            ("FR ★ Opération Dragon - 1973", None, "operationdragon", Some(1973), Some("FR"), None),
            ("AR ★ ابطال الكرة", Some(2008), "ابطالالكرة", Some(2008), Some("AR"), None),
        ];
        for (raw, year, key, want_year, lang, origin) in cases {
            let m = p(raw, *year);
            assert_eq!(m.key, *key, "{raw}");
            assert_eq!(m.year, *want_year, "{raw}");
            assert_eq!(m.lang.as_deref(), *lang, "{raw}");
            assert_eq!(m.origin_text().as_deref(), *origin, "{raw}");
        }
    }

    /// Dub and subtitle markers are part of the language (not dropped); CAM copies, quality and
    /// HDR are recorded as version tags.
    #[test]
    fn dub_sub_and_version_tags() {
        let lang = |raw: &str| p(raw, Some(2020)).lang_text();
        assert_eq!(lang("AR ★ Ice Age The Meltdown - 2006 [مدبلج]").as_deref(), Some("AR+DUB"));
        assert_eq!(lang("FR ★ A Working Man - 2025  [VOSTFR]").as_deref(), Some("FR+SUB"));
        assert_eq!(lang("Time Hustler [Multi-Audio]").as_deref(), Some("+MULTI"));
        assert_eq!(lang("Chilling Adventures of Sabrina [Multi-Sub]").as_deref(), Some("+SUB"));
        assert_eq!(lang("IR - Lucky Strike (2026) - دوبله").as_deref(), Some("IR+DUB"));
        assert_eq!(lang("FR - Le Cercle VOSTFR (2020)").as_deref(), Some("FR+SUB"));
        // words that also end titles stay title words
        assert_eq!(p("EN - Spy Cam (2020)", None).key, "spycam");
        assert_eq!(p("EN - The Sub (2020)", None).key, "thesub");
        assert_eq!(p("AR ★ Ice Age The Meltdown - 2006 [مدبلج]", None).key, p("AR ★ Ice Age The Meltdown - 2006", None).key, "same title");
        assert_eq!(split_lang_text("FR+SUB"), (Some("FR"), Dub::Subbed));
        assert_eq!(split_lang_text("+MULTI"), (None, Dub::Multi));
        assert_eq!(split_lang_text("D+"), (Some("D+"), Dub::None));

        let t = p("4K-FR-HDR - Ballerina (2025)", None).tags;
        assert_eq!((t.quality(), t.has(VersionTags::HDR), t.is_cam()), (Some(Quality::Uhd), true, false));
        assert!(p("EN - Fast X (2023) [CAM]", None).tags.is_cam());
        assert!(p("EN-CAM - Fast X (2023)", None).tags.is_cam());
        assert_eq!(p("EN - Fast X (2023) (1080p)", None).tags.quality(), Some(Quality::Fhd));
    }

    /// Missing years never stack by key; short / all-digit / stop-list titles stack only with
    /// a year; an empty title never.
    #[test]
    fn refusals() {
        assert_eq!(p("SE - Gold", None).refusal(), Some(Refusal::NoYear));
        assert_eq!(p("SE - Gold", None).stack_hash(ItemKind::Movie), None);
        for raw in ["EN ★ F1 - 2025 [4K]", "FR ★ 300 - 2007", "ES ★ Up - 2009 [4k]", "4K-IN - Live (2023)"] {
            let m = p(raw, None);
            assert!(m.generic, "{raw}");
            assert!(m.stack_hash(ItemKind::Movie).is_some(), "{raw} has a year: it stacks");
        }
        assert!(p("FR ★ 300", None).stack_hash(ItemKind::Movie).is_none());
        assert_eq!(p("[4K]", Some(2020)).refusal(), Some(Refusal::Empty));
        // movies and series never share a hash; nor do two years
        let m = p("EN - Fargo (1996)", None);
        assert_ne!(m.stack_hash(ItemKind::Movie), m.stack_hash(ItemKind::Series));
        assert_ne!(m.stack_hash(ItemKind::Movie), p("EN - Fargo (1997)", None).stack_hash(ItemKind::Movie));
    }

    /// Unknown codes become prefixes once a playlist opens ≥ 20 names with them (and with that
    /// separator), never below.
    #[test]
    fn frequent_prefixes() {
        let names: Vec<String> = (0..FREQUENT_PREFIX).map(|i| format!("KD ★ Title {i} - 2020")).collect();
        let codes = PrefixCodes::from_names(names.iter().map(String::as_str));
        assert_eq!(parse("KD ★ Ice Age - 2006", None, &codes).key, "iceage");
        assert_eq!(parse("KD ★ Ice Age - 2006", None, &codes).lang.as_deref(), Some("KD"));
        assert_eq!(parse("KD - Ice Age (2006)", None, &codes).key, "kdiceage", "another separator");
        let few = PrefixCodes::from_names(names[1..].iter().map(String::as_str));
        assert_eq!(parse("KD ★ Ice Age - 2006", None, &few).key, "kdiceage");
        assert_ne!(codes.fingerprint(), few.fingerprint());
    }

    /// The audio language a tag promises, and how copies rank for an English (then German)
    /// user: English, then untagged / multi-audio, then subtitled markets, then other languages.
    #[test]
    fn languages_rank_copies() {
        assert_eq!(audio_lang("EN", Dub::None), Some("en"));
        assert_eq!(audio_lang("UK-US", Dub::None), Some("en"));
        assert_eq!(audio_lang("GR", Dub::None), Some("el"));
        assert_eq!(audio_lang("IN-TL", Dub::None), Some("ta"));
        assert_eq!(audio_lang("IN-EN", Dub::None), Some("hi"), "India's English shelf is Hindi-dubbed");
        assert_eq!(audio_lang("AR-SUBS", Dub::None), None);
        assert_eq!(audio_lang("FR", Dub::Subbed), None, "VOSTFR is original audio");
        assert_eq!(audio_lang("AR", Dub::Dubbed), Some("ar"));
        // the market a copy is for (subtitled shelves count as their market's language)
        assert_eq!(market_lang("AR-SUBS", Dub::None), Some("ar"));
        assert_eq!(market_lang("KU-S", Dub::None), Some("ku"));
        assert_eq!(market_lang("NL", Dub::Subbed), Some("nl"));
        assert_eq!(market_lang("EN", Dub::Subbed), Some("en"));
        assert_eq!(market_lang("QC", Dub::None), Some("fr"));
        assert_eq!(market_lang("IN-EN", Dub::None), Some("hi"));
        assert_eq!(market_lang("DE", Dub::Multi), None, "multi-audio may carry the user's language");
        assert_eq!(market_lang("SC", Dub::None), None);
        assert_eq!(market_lang("EX", Dub::None), Some("sh"));
        assert_eq!(market_lang("IS", Dub::None), Some("is"));
        assert_eq!(market_lang("SUBS", Dub::None), None);
        assert!(is_bare_subtitles("SUBS") && is_bare_subtitles("SUB"));
        assert!(!is_bare_subtitles("AR-SUBS") && !is_bare_subtitles("") && !is_bare_subtitles("EN"));
        assert_eq!(norm_lang("eng"), "en");
        assert_eq!(norm_lang("ger"), "de");
        let prefs = vec!["en".to_owned(), "de".to_owned()];
        let rank = |l: Option<&str>, d| lang_rank(l, d, &prefs);
        assert_eq!(rank(Some("EN"), Dub::None), 0);
        assert_eq!(rank(Some("DE"), Dub::Dubbed), 1);
        assert_eq!(rank(None, Dub::None), 2);
        assert_eq!(rank(None, Dub::Multi), 2);
        assert_eq!(rank(Some("AR-SUBS"), Dub::None), 3);
        assert_eq!(rank(Some("FR"), Dub::None), 4);
    }

    fn item(hash: Option<i64>, tmdb: Option<u32>, origin: Option<&str>) -> ClusterItem {
        ClusterItem { hash, tmdb, origin: origin.map(|o| [o.as_bytes()[0], o.as_bytes()[1]]) }
    }

    /// Same key + year stacks across languages; TMDB ids decide when present (same id joins
    /// different names and years, different ids never stack); two different origins never
    /// stack; refused keys stack only by id.
    #[test]
    fn clustering_rules() {
        let h = |raw: &str| p(raw, None).stack_hash(ItemKind::Movie);
        // one film in several languages: same key and year, one stack
        let opp_en = h("EN ★ Oppenheimer - 2023");
        let opp_tl = h("IN-TL - Oppenheimer (2023)");
        assert_eq!(opp_en, opp_tl, "one stack across languages");
        let items = [
            item(opp_en, None, None),                       // 0 English
            item(opp_tl, None, None),                       // 1 Tamil dub
            item(h("EN - Oppenheimer (2024)"), None, None), // 2 another year: apart
        ];
        assert_eq!(cluster(&items), vec![vec![0, 1]]);
        // namesakes (the Netflix "Leo" and the Tamil "Leo", both 2023) share key and year:
        // only TMDB ids tell them apart — different ids split them; an id-less copy goes with
        // neither
        let (leo_en, leo_tl) = (h("EN ★ Leo - 2023"), h("IN-TL - Leo (2023)"));
        assert_eq!(leo_en, leo_tl);
        let items =
            [item(leo_en, Some(1075794), None), item(leo_tl, Some(1000837), None), item(leo_en, Some(1075794), None), item(leo_en, None, None)];
        assert_eq!(cluster(&items), vec![vec![0, 2]]);
        // one id among id-less copies: they join it
        let items = [item(leo_en, Some(1075794), None), item(leo_tl, None, None)];
        assert_eq!(cluster(&items), vec![vec![0, 1]]);
        // the same id joins different names and years, and a copy without a year
        let incendies = h("FR - Incendies (2010)");
        let die_frau = h("DE - Die Frau die singt (2011)");
        let items = [item(incendies, Some(46738), None), item(die_frau, Some(46738), None), item(None, Some(46738), None), item(None, None, None)];
        assert_eq!(cluster(&items), vec![vec![0, 1, 2]]);
        // remakes: The Office (US) vs (GB), same key and year; untagged copies stack apart
        let office = h("EN - The Office (2005)");
        let items = [
            item(office, None, Some("US")),
            item(office, None, Some("GB")),
            item(office, None, Some("US")),
            item(office, None, None),
            item(office, None, None),
        ];
        assert_eq!(cluster(&items), vec![vec![0, 2], vec![3, 4]]);
        // one origin among untagged copies: one stack
        let items = [item(office, None, Some("US")), item(office, None, None)];
        assert_eq!(cluster(&items), vec![vec![0, 1]]);
        // no year: refused by key
        let gold = h("SE - Gold");
        assert_eq!(cluster(&[item(gold, None, None), item(gold, None, None)]), Vec::<Vec<u32>>::new());
        assert_eq!(tmdb_id(" 603 "), Some(603));
        assert_eq!(tmdb_id("0"), None);
        assert_eq!(tmdb_id("tt0133093"), None);
    }
}
