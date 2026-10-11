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
//! | `UK ★ ITV 4 4K ◉`                | `ITV 4`        | UK      | Uhd     |
//!
//! Rules (DESIGN §13.1), on the tokens of the shared tokenizer (`name_tokens`: words, symbol /
//! punctuation runs, superscript decoration, flags, brackets):
//! * Leading prefixes, repeatedly, after any decoration (`★ ◉ ## -`) and flags: `[…]` (any
//!   tag), `(XX)` / `|XX|`, and up to three prefix words followed by any separator run (`XX:`,
//!   `XX |`, `XX ★`, `XX ❖`, `XX ⓧ`; a dash only spaced after upper-case codes, `DE - Das
//!   Erste`, never `Al - Jazeera`). A prefix word is a country / language / region of the
//!   category tags (`UK`, `ALB`, `EXYU`, `LATAM`, `HINDI`, `URUGUAY`), a known three-letter
//!   code, a service tag (`VIP SAT RAW PPV 24/7`), a quality word, or — alone — any two
//!   letters (`RK:`) or a code with a short line-up label (`IN-PREM:`, `BE-VIP:`), so `CNN:
//!   Live`, `AT&T: …` and `MY TV: …` stay intact. A prefix is never all that is left.
//! * Quality words anywhere (`SD HD FHD UHD 4K 8K 480p…2160p 3840p 1080i`, glued to symbols
//!   `4K◉`, in `(…)` / `[…]` / `⁽…⁾`, as superscript / small caps `ᴴᴰ` `ꜰʜᴅ` `ᶠ ᴴᴰ`) →
//!   [`Quality`] (best one wins).
//! * Codec / format words (`HEVC H.265 x265 HDR 50fps ⁵⁰ FPS`), `RAW` (not in `WWE RAW`), any
//!   `[…]` group, superscript groups (`⁽ᴮᴷ⁾`) and `(…)` groups that are tags (`backup`,
//!   `multi-sub`, `geo-blocked`…) → `tags`. Other `(…)` groups are part of the name (`BBC One
//!   (Scotland)`); superscript text is decoration, stylised small capitals that are no known
//!   word stay.
//! * Separators and decoration at either end are dropped (`◉`, `★`, `|`, `:`, `##`, flags);
//!   inside the name they stay as written (`E! News`, `AT&T`, `Sky Sports+`, `Channel 4 +1`,
//!   `Parijs-Tours: Mannen`); whitespace collapses.
//! * Non-Latin text is untouched (no transliteration: display, not identity).

// Implemented: opt back into the lints that `lib.rs` silences for the still-stubbed `sources`.
#![warn(dead_code, unused_variables, unused_imports)]

/// The quality word of a name is the channel's name-derived badge (stored at import,
/// `channel.quality`); the type lives in the model next to the measured one.
pub use crate::model::Quality;
use crate::sources::category_tags::{self, WordTag};
use crate::sources::name_tokens::{Kind, Token, closing, deco_ascii, is_format_word, is_service_word, is_small_caps, quality_word, tokens};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanName {
    pub name: String,
    /// Country / language / region prefix that was stripped, upper-case as written (`UK`,
    /// `USA`, `EN`, `ALB`, `URUGUAY`; a flag's letters).
    pub country: Option<String>,
    pub quality: Option<Quality>,
    /// Other stripped tags: "VIP", "backup", "HEVC", "MULTI-SUB"...
    pub tags: Vec<String>,
}

/// Clean a live channel name. Never returns an empty name (falls back to the trimmed raw).
pub fn clean_channel_name(raw: &str) -> CleanName {
    let c = clean(raw, false);
    if c.name.is_empty() {
        return CleanName { name: raw.trim().to_owned(), country: None, quality: None, tags: Vec::new() };
    }
    CleanName { name: c.name, country: c.country, quality: c.quality, tags: c.tags }
}

/// Clean a movie/series title; extracts a trailing `(YYYY)` / `- YYYY` year.
pub fn clean_vod_name(raw: &str) -> (String, Option<u16>) {
    let c = clean(raw, true);
    if c.name.is_empty() {
        return (raw.trim().to_owned(), None);
    }
    (c.name, c.year)
}

/// Clean a group/category name (`"UK| SPORTS ᴴᴰ"` → `"Sports"`, `"UK ❖ General"` →
/// `"General"`).
///
/// The country only survives in the group KEY (`identity::normalize_group` keeps it) and in
/// `grp.raw_name` (tag detection); the display drops it like for channels. All-caps names are
/// title-cased, keeping short acronyms (`TV`, `NFL`) and lower-casing joining words
/// (`NEWS & INFO` → `News & Info`).
pub fn clean_group_name(raw: &str) -> String {
    let c = clean(raw, false);
    if c.name.is_empty() {
        return raw.trim().to_owned();
    }
    let has_upper = c.name.chars().any(char::is_uppercase);
    let has_lower = c.name.chars().any(char::is_lowercase);
    if has_upper && !has_lower { title_case(&c.name) } else { c.name }
}

/// Display name of a group as imported (`grp.name`): smart cleanup or the trimmed raw name,
/// never empty ("Uncategorized").
pub fn group_display(raw: &str, smart: bool) -> String {
    let name = if smart { clean_group_name(raw) } else { raw.trim().to_owned() };
    if name.trim().is_empty() { "Uncategorized".to_owned() } else { name }
}

// ---------------------------------------------------------------------------------------------

/// Result of the shared cleanup pass (`name` empty = nothing left: callers fall back).
#[derive(Default)]
struct Cleaned {
    name: String,
    country: Option<String>,
    quality: Option<Quality>,
    tags: Vec<String>,
    year: Option<u16>,
}

impl Cleaned {
    fn add_quality(&mut self, q: Quality) {
        if self.quality.is_none_or(|old| q > old) {
            self.quality = Some(q);
        }
    }
    fn add_country(&mut self, code: &str) {
        if self.country.is_none() && !code.is_empty() {
            self.country = Some(code.to_uppercase());
        }
    }
}

/// `(…)` contents that are tags rather than part of the name (compared lower-case; `backup`
/// matches `backup 2`, `backup-1`…).
const PAREN_TAGS: &[&str] = &[
    "backup",
    "multi-sub",
    "multisub",
    "multi-audio",
    "multi audio",
    "multi",
    "vip",
    "geo-blocked",
    "not 24/7",
    "24/7",
    "opt-in",
    "dubbed",
    "sub",
    "subbed",
    "vostfr",
    "vo",
    "ppv",
    "test",
    "new",
    "alt",
];

/// Three-letter codes accepted as country/language prefixes besides the ones the category
/// tags know (two letters are always codes).
const COUNTRY3: &[&str] = &[
    "usa", "uae", "ksa", "gbr", "deu", "ger", "fra", "ita", "esp", "spa", "por", "prt", "bra", "arg", "mex", "can", "aus", "ind", "pak", "tur",
    "rus", "ukr", "pol", "nld", "bel", "swe", "nor", "dnk", "den", "fin", "isl", "irl", "che", "sui", "aut", "gre", "grc", "cze", "svk", "hun",
    "rou", "rom", "bgr", "bul", "srb", "hrv", "cro", "bih", "slo", "svn", "mkd", "alb", "ara", "eng", "lat", "afr", "asi", "chn", "jpn", "kor",
    "phi", "tha", "vie", "idn", "mys", "isr", "irn", "irq", "egy", "mar", "tun", "dza", "alg", "nga", "zaf", "kur", "per", "chl", "col", "ven",
    "lux", "ltu", "lva", "est", "geo", "arm", "aze", "kaz", "uzb", "bgd", "lka", "npl", "afg", "syr", "lbn", "jor", "kwt", "qat", "bhr", "omn",
    "yem", "sau", "som", "eth", "ken", "gha", "cmr", "sen", "civ", "hin", "urd", "tam", "tel", "fre", "dut", "chi", "jap", "rum", "fil", "ban",
];

fn is_paren_tag(inner: &str) -> bool {
    let l = inner.trim().to_lowercase();
    PAREN_TAGS.contains(&l.as_str())
        || l.strip_prefix("backup").is_some_and(|rest| rest.trim_start_matches([' ', '-', '_']).chars().all(|c| c.is_ascii_digit()))
        || is_format_word(&l)
}

fn year_of(w: &str) -> Option<u16> {
    let w = w.trim();
    if w.len() != 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse::<u16>().ok().filter(|y| (1900..=2100).contains(y))
}

/// What a leading word is, if it can be part of a prefix.
enum PrefixWord<'a> {
    /// A country / language / region (`UK`, `ALB`, `HINDI`), or any two-letter code alone
    /// (`RK:`, `ZG:` — provider line-up codes). `tag` is a sub-label (`IN-PREM` → `PREM`).
    Country {
        code: &'a str,
        tag: Option<&'a str>,
    },
    Quality(Quality),
    /// `VIP`, `SAT`, `RAW`, `PPV`, `24/7`, `HEVC`.
    Tag(&'a str),
}

/// `w` as a prefix word. `alone` = the only word before the separator: unknown two-letter
/// codes and `XX-YYY` line-up labels count only then (`"MY TV: …"` is a name).
fn prefix_word(w: &str, alone: bool) -> Option<PrefixWord<'_>> {
    if let Some(q) = quality_word(w) {
        return Some(PrefixWord::Quality(q));
    }
    if is_service_word(w) || is_format_word(w) {
        return Some(PrefixWord::Tag(w));
    }
    let ascii_letters = |s: &str, n: std::ops::RangeInclusive<usize>| n.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphabetic());
    if matches!(category_tags::classify(w), WordTag::Tag(_))
        || (ascii_letters(w, 3..=3) && COUNTRY3.contains(&w.to_ascii_lowercase().as_str()))
        || (alone && ascii_letters(w, 2..=2))
    {
        return Some(PrefixWord::Country { code: w, tag: None });
    }
    // `BE/FR:`, `UK/US |`: codes only
    if alone && w.contains('/') && w.split('/').all(|p| matches!(prefix_word(p, true), Some(PrefixWord::Country { tag: None, .. }))) {
        return Some(PrefixWord::Country { code: w.split('/').next().unwrap_or(w), tag: None });
    }
    // `IN-PREM:`, `BE-VIP:`, `FR-REU:`: a code and a short label of the provider's line-up
    let (head, label) = w.split_once('-').filter(|_| alone)?;
    let short_label = label.len() <= 4 && label.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    match prefix_word(head, true)? {
        PrefixWord::Country { code, .. } if ascii_letters(code, 2..=3) && (short_label || is_service_word(label)) => {
            Some(PrefixWord::Country { code, tag: Some(label) })
        }
        _ => None,
    }
}

/// The text from the first to the last of `toks` as written (they are slices of `raw`).
fn span<'a>(raw: &'a str, toks: &[Token<'a>]) -> &'a str {
    let (Some(first), Some(last)) = (toks.first(), toks.last()) else { return "" };
    let at = |t: &Token<'_>| t.text.as_ptr() as usize - raw.as_ptr() as usize;
    &raw[at(first)..at(last) + last.text.len()]
}

/// Leading prefixes, repeatedly: `[XX]` / `[tag]`, `(XX)`, `|XX|`, and up to three prefix
/// words followed by a separator (`XX:`, `XX |`, `XX ★`, `XX ❖`, `PL VIP:`, `IN-PREM:`; a dash
/// only spaced after upper-case codes: `DE - Das Erste`, not `Al - Jazeera`), after any
/// decoration and flags. Never the whole name: something must follow. Returns the index of
/// the first token of the rest.
fn strip_prefixes(raw: &str, t: &[Token<'_>], out: &mut Cleaned) -> usize {
    // something must be left to name the channel (`"UK: HD"` stays `UK`)
    let has_word =
        |from: usize| t.get(from..).is_some_and(|r| r.iter().any(|x| x.is(Kind::Word) && quality_word(x.text).is_none() && !is_format_word(x.text)));
    let mut i = 0;
    loop {
        let mut j = i;
        while let Some(tok) = t.get(j) {
            match tok.kind {
                Kind::Sep if tok.text != "|" => {}
                Kind::Close => {}
                Kind::Flag if has_word(j + 1) => out.add_country(&crate::sources::name_tokens::flag_code(tok.text)),
                _ => break,
            }
            j += 1;
        }
        let Some(tok) = t.get(j) else { return i };
        // `[…]` (any tag), `(XX)` / `{XX}` / `|XX|` (prefix words only)
        let group = match tok.kind {
            Kind::Open => closing(t, j),
            Kind::Sep => t[j + 1..].iter().position(|x| x.is(Kind::Sep) && x.text.starts_with('|')).map(|p| j + 1 + p),
            _ => None,
        };
        if let Some(close) = group {
            let inner = &t[j + 1..close];
            let words: Vec<&str> = inner.iter().filter(|x| x.is(Kind::Word)).map(|x| x.text).collect();
            let all_prefix = !words.is_empty() && words.iter().all(|w| prefix_word(w, words.len() == 1).is_some());
            if !has_word(close + 1) || !(all_prefix || matches!(tok.text, "[" | "【")) {
                return i;
            }
            if all_prefix {
                record_prefix(&words, out);
            } else {
                let text = span(raw, inner).trim();
                match quality_word(text) {
                    Some(q) => out.add_quality(q),
                    None if !text.is_empty() => out.tags.push(text.to_owned()),
                    None => {}
                }
            }
            i = close + 1;
            continue;
        }
        if tok.is(Kind::Sep) {
            // a lone `|` that opens nothing: decoration
            i = j + 1;
            continue;
        }
        // prefix words, then a separator
        let words = t[j..].iter().take(4).take_while(|x| x.is(Kind::Word)).count().min(3);
        let Some(sep) = t.get(j + words).filter(|x| x.is(Kind::Sep) && words > 0) else { return i };
        let phrase: Vec<&str> = t[j..j + words].iter().map(|x| x.text).collect();
        let dash = matches!(sep.text, "-" | "–");
        let ok = phrase.iter().all(|w| prefix_word(w, words == 1).is_some())
            && !matches!(sep.text, "." | "," | "/" | "'" | "_")
            && (!dash || (sep.spaced && t.get(j + words + 1).is_some_and(|n| n.spaced) && phrase.iter().all(|w| !w.chars().any(char::is_lowercase))))
            && has_word(j + words + 1);
        if !ok {
            return i;
        }
        record_prefix(&phrase, out);
        i = j + words + 1;
    }
}

fn record_prefix(words: &[&str], out: &mut Cleaned) {
    for w in words {
        match prefix_word(w, words.len() == 1) {
            Some(PrefixWord::Country { code, tag }) => {
                out.add_country(code);
                out.tags.extend(tag.map(str::to_owned));
            }
            Some(PrefixWord::Quality(q)) => out.add_quality(q),
            Some(PrefixWord::Tag(tag)) => out.tags.push(tag.to_owned()),
            None => {}
        }
    }
}

/// What a bracket group in the name is.
enum Group {
    /// Quality / format words only (`(720p)`, `⁽ᴴᴰ⁾`, `(FHD@60FPS)`, `[4K]`) or decoration.
    Drop,
    Year(u16),
    Tag,
    /// Part of the name (`BBC One (Scotland)`).
    Name,
}

fn group_class(raw: &str, open: &str, inner: &[Token<'_>], vod: bool, out: &mut Cleaned) -> Group {
    let text = span(raw, inner).trim();
    // superscript only (`⁽ᴮᴷ⁾`): decoration
    if inner.iter().any(|x| x.is(Kind::Deco)) && inner.iter().all(|x| matches!(x.kind, Kind::Deco | Kind::Sep)) {
        for x in inner.iter().filter(|x| x.is(Kind::Deco)) {
            let ascii = deco_ascii(x.text);
            match quality_word(&ascii) {
                Some(q) => out.add_quality(q),
                None => out.tags.push(ascii),
            }
        }
        return Group::Drop;
    }
    let words: Vec<String> = inner
        .iter()
        .filter_map(|x| match x.kind {
            Kind::Word => Some(x.text.to_owned()),
            Kind::Deco => Some(deco_ascii(x.text)),
            _ => None,
        })
        .collect();
    if words.iter().all(|w| quality_word(w).is_some() || is_format_word(w)) {
        for w in &words {
            match quality_word(w) {
                Some(q) => out.add_quality(q),
                None => out.tags.push(w.clone()),
            }
        }
        return Group::Drop;
    }
    if let (true, Some(y)) = (vod, year_of(text)) {
        return Group::Year(y);
    }
    if matches!(open, "[" | "【") || (open == "(" && is_paren_tag(text)) {
        out.tags.push(text.to_owned());
        return Group::Tag;
    }
    Group::Name
}

/// A run of decoration tokens (`ᶠ ᴴᴰ`, `ᴴᴰ ⱽᴵᴾ`, `ᵁᴴᴰ ³⁸⁴⁰ᴾ`): quality words, tags, or kept
/// when it is stylised small-capital text that is no known word.
fn deco_run(run: &[Token<'_>], out: &mut Cleaned, keep: &mut [bool]) {
    let joined: String = run.iter().map(|t| deco_ascii(t.text)).collect();
    if let Some(q) = quality_word(&joined) {
        out.add_quality(q);
        keep.fill(false);
        return;
    }
    for (t, k) in run.iter().zip(keep.iter_mut()) {
        let ascii = deco_ascii(t.text);
        if let Some(q) = quality_word(&ascii) {
            out.add_quality(q);
        } else if is_small_caps(t.text)
            && !is_format_word(&ascii)
            && !is_service_word(&ascii)
            && !matches!(category_tags::classify(&ascii), WordTag::Tag(_))
        {
            continue;
        } else if !ascii.is_empty() {
            out.tags.push(ascii);
        }
        *k = false;
    }
}

/// The shared pass: prefixes, then quality / format words and bracket groups anywhere, then
/// decoration at either end.
fn clean(raw: &str, vod: bool) -> Cleaned {
    let raw = raw.trim();
    let t = tokens(raw);
    let mut out = Cleaned::default();
    let start = strip_prefixes(raw, &t, &mut out);
    let mut keep = vec![true; t.len()];
    keep[..start].fill(false);
    // matched brackets (a stray one is decoration)
    let mut pair = vec![None; t.len()];

    let mut i = start;
    while i < t.len() {
        let tok = t[i];
        match tok.kind {
            Kind::Word => {
                let raw_word = tok.text.eq_ignore_ascii_case("raw") && !(i > 0 && t[i - 1].text.eq_ignore_ascii_case("wwe"));
                if let Some(q) = quality_word(tok.text) {
                    out.add_quality(q);
                    keep[i] = false;
                } else if is_format_word(tok.text) || raw_word {
                    out.tags.push(tok.text.to_owned());
                    keep[i] = false;
                }
            }
            Kind::Deco => {
                let end = t[i..].iter().position(|x| !x.is(Kind::Deco)).map_or(t.len(), |p| i + p);
                // inside a word (a letter glued after it): part of the word
                if t.get(end).is_some_and(|n| n.is(Kind::Word) && !n.spaced && n.text.starts_with(char::is_alphabetic)) {
                    i = end;
                    continue;
                }
                // `sᴅ`: a plain letter glued before small capitals
                let lead = (i > start && !tok.spaced && keep[i - 1])
                    .then(|| t[i - 1].text)
                    .filter(|w| w.len() == 1 && w.chars().all(|c| c.is_ascii_alphabetic()));
                if let Some(q) = lead.and_then(|l| quality_word(&format!("{l}{}", deco_ascii(tok.text)))) {
                    out.add_quality(q);
                    keep[i - 1] = false;
                    keep[i] = false;
                    i += 1;
                    continue;
                }
                deco_run(&t[i..end], &mut out, &mut keep[i..end]);
                // `³⁸⁴⁰ᴾ+`: a plus glued to dropped decoration goes with it
                if !keep[end - 1] && t.get(end).is_some_and(|n| n.text == "+" && !n.spaced) {
                    keep[end] = false;
                    i = end + 1;
                    continue;
                }
                i = end;
                continue;
            }
            Kind::Open => {
                if let Some(close) = closing(&t, i) {
                    match group_class(raw, tok.text, &t[i + 1..close], vod, &mut out) {
                        Group::Drop | Group::Tag => keep[i..=close].fill(false),
                        Group::Year(y) => {
                            out.year = Some(y);
                            keep[i..=close].fill(false);
                        }
                        Group::Name => {
                            // its words are cleaned like the rest (`(HYPER ᴴᴰ)` → `(HYPER)`)
                            pair[i] = Some(close);
                            pair[close] = Some(i);
                            i += 1;
                            continue;
                        }
                    }
                    i = close + 1;
                    continue;
                }
            }
            Kind::Sep | Kind::Flag | Kind::Close => {}
        }
        i += 1;
    }

    // decoration at either end: separators, flags, stray brackets; then a VOD `- 1999` suffix
    // a bracket at the front stays when it opens a group of the name, at the end when it closes one
    let removable = |front: bool, i: usize| match t[i].kind {
        Kind::Sep | Kind::Flag => true,
        Kind::Open => !front || pair[i].is_none(),
        Kind::Close => front || pair[i].is_none(),
        Kind::Word | Kind::Deco => false,
    };
    loop {
        let kept: Vec<usize> = (0..t.len()).filter(|&i| keep[i]).collect();
        let (Some(&first), Some(&last)) = (kept.first(), kept.last()) else { break };
        if removable(true, first) {
            if t[first].is(Kind::Flag) {
                out.add_country(&crate::sources::name_tokens::flag_code(t[first].text));
            }
            keep[first] = false;
            continue;
        }
        // a tag word set off at the end (`… ◉ *MULTI*`, `… | backup`)
        if kept.len() > 2
            && t[last].is(Kind::Word)
            && t[kept[kept.len() - 2]].is(Kind::Sep)
            && PAREN_TAGS.contains(&t[last].text.to_lowercase().as_str())
        {
            out.tags.push(t[last].text.to_owned());
            keep[last] = false;
            continue;
        }
        // the full stop of an abbreviation stays (`T.I.M.`)
        let abbreviation = t[last].text == "." && !t[last].spaced && kept.len() > 1 && t[kept[kept.len() - 2]].text.contains('.');
        if removable(false, last) && !abbreviation {
            keep[last] = false;
            continue;
        }
        let n = kept.len();
        if vod
            && out.year.is_none()
            && n >= 3
            && matches!(t[kept[n - 2]].text, "-" | "–")
            && let Some(y) = year_of(t[last].text)
        {
            out.year = Some(y);
            keep[last] = false;
            keep[kept[n - 2]] = false;
            continue;
        }
        break;
    }
    out.name = render(&t, &keep);
    out
}

/// The kept tokens as written: a space where the name had one, or where dropped tokens with
/// a space left a gap (`ITV 4 4K ◉ News` → `ITV 4 News`) or sat between two words
/// (`SKY(HD)NEWS` → `SKY NEWS`) — never inside brackets or before a glued separator
/// (`(HYPER ᴴᴰ)` → `(HYPER)`).
fn render(t: &[Token<'_>], keep: &[bool]) -> String {
    let mut s = String::new();
    let mut prev: Option<usize> = None;
    for (i, tok) in t.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if let Some(p) = prev {
            let dropped = &t[p + 1..i];
            let gap = !dropped.is_empty()
                && !matches!(tok.kind, Kind::Sep | Kind::Close)
                && !t[p].is(Kind::Open)
                && (dropped.iter().chain([tok]).any(|x| x.spaced) || (t[p].is(Kind::Word) && tok.is(Kind::Word)));
            if tok.spaced || gap {
                s.push(' ');
            }
        }
        s.push_str(tok.text);
        prev = Some(i);
    }
    s
}

/// Title-case an all-caps name, keeping acronyms and lower-casing joining words. Acronyms are
/// two-letter words (`TV`, `UK`) that are not common words, letter+digit words (`F1`, `MBC1`)
/// and a list of well-known longer ones; everything else (`NEW`, `ALL`, `TOP`) is a word.
fn title_case(s: &str) -> String {
    const SMALL: &[&str] = &["and", "the", "for", "of", "in", "on", "at", "to", "by", "with", "a", "an", "or", "de", "la", "le", "et"];
    const WORDS2: &[&str] = &["my", "go", "up", "no", "so", "do", "be", "he", "if", "is", "it", "we", "me"];
    const ACRONYMS: &[&str] = &[
        "nfl", "nba", "nhl", "mlb", "mls", "ufc", "wwe", "bbc", "itv", "hbo", "mtv", "cnn", "espn", "ppv", "vod", "usa", "vip", "xxx", "rtl", "ard",
        "zdf", "dazn", "bt", "mbc", "osn", "tnt", "amc", "abc", "nbc", "cbs", "fox", "pbs", "dstv", "iptv",
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
        let acronym =
            !SMALL.contains(&core) && ((letters == 2 && !digits && !WORDS2.contains(&core)) || (digits && letters <= 3) || ACRONYMS.contains(&core));
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

        // provider decorations: any symbol run separates a prefix, quality words glued to
        // symbols or in superscript, decoration at either end; names keep their own symbols
        let q = |s: &str| match s {
            "SD" => Some(Quality::Sd),
            "HD" => Some(Quality::Hd),
            "FHD" => Some(Quality::Fhd),
            "4K" => Some(Quality::Uhd),
            _ => None,
        };
        let cases: &[(&str, &str, &str, &str)] = &[
            ("UK: ITV 4 4K ◉", "ITV 4", "UK", "4K"),
            ("UK ★ ITV 4 4K◉", "ITV 4", "UK", "4K"),
            ("US ★ HGTV HD EAST ◉", "HGTV EAST", "US", "HD"),
            ("US\u{a0}★\u{a0}Flo 31 :", "Flo 31", "US", ""),
            ("FR ★ FRANCE 2 FHD ◉", "FRANCE 2", "FR", "FHD"),
            ("ALB ★ TOP CHANNEL", "TOP CHANNEL", "ALB", ""),
            ("EXYU ★ ARENA SPORT 1", "ARENA SPORT 1", "EXYU", ""),
            ("LATAM | Canal 5", "Canal 5", "LATAM", ""),
            ("URUGUAY ★ TELEDOCE", "TELEDOCE", "URUGUAY", ""),
            ("UK ❖ Sky Atlantic ⓧ", "Sky Atlantic", "UK", ""),
            ("DE ┃ Das Erste ●", "Das Erste", "DE", ""),
            ("VIP: SKY SPORTS F1 ⁴ᵏ", "SKY SPORTS F1", "", "4K"),
            ("SAT: PRO TV ᴿᴬᵂ", "PRO TV", "", ""),
            ("PL VIP: ELEVEN 1 RAW 4K 3840P", "ELEVEN 1", "PL", "4K"),
            ("IN-PREM: COMEDY CINEMA HD.", "COMEDY CINEMA", "IN", "HD"),
            ("24/7: THE SIMPSONS", "THE SIMPSONS", "", ""),
            ("BE/FR: TF1", "TF1", "BE", ""),
            ("(JM) ESPN PLAY 7 (D)", "ESPN PLAY 7 (D)", "JM", ""),
            ("## TR| BEIN SPORTS 1 ##", "BEIN SPORTS 1", "TR", ""),
            ("DVB-T: MMN DRAMA ᶠ ᴴᴰ", "DVB-T: MMN DRAMA", "", "FHD"),
            ("ES: LIGA 1 ᴴᴰ ⁽ᴮᴷ⁾", "LIGA 1", "ES", "HD"),
            ("NF: NETFLIX 02 (FHD@60FPS) EVENTS", "NETFLIX 02 EVENTS", "NF", "FHD"),
            ("GOLD: SKY ⁽ᴴᴰ⁾", "GOLD: SKY", "", "HD"),
            ("TNT SPORTS 1 ᴿᴬᵂ ⁵⁰ FPS", "TNT SPORTS 1", "", ""),
            ("🇬🇧 BBC One", "BBC One", "GB", ""),
            // kept: names with their own symbols, unknown prefixes, PPV inside a name
            ("AT&T: 5-MINUTE CRAFTS ᴿᴬᵂ", "AT&T: 5-MINUTE CRAFTS", "", ""),
            ("E! News", "E! News", "", ""),
            ("Sky Sports+ HD", "Sky Sports+", "", "HD"),
            ("UK: Channel 4 +1", "Channel 4 +1", "UK", ""),
            ("UK: ITV 4+1 ◉", "ITV 4+1", "UK", ""),
            ("5 USA", "5 USA", "", ""),
            ("WWE RAW", "WWE RAW", "", ""),
            ("US: ESPN+ PPV 175", "ESPN+ PPV 175", "US", ""),
            ("MY TV: News", "MY TV: News", "", ""),
            ("NEXT | Lyon vs Nice | 15:55 (CEST) | 8K EXCLUSIVE", "NEXT | Lyon vs Nice | 15:55 (CEST) | EXCLUSIVE", "", "4K"),
            ("beIN Sp⚽rts 8 4K", "beIN Sp⚽rts 8", "", "4K"),
            ("ꜱᴘᴏʀᴛꜱ 1", "ꜱᴘᴏʀᴛꜱ 1", "", ""),
            ("CH: BLUE SPORT 1 ᴅᴇ ғʜᴅ", "BLUE SPORT 1", "CH", "FHD"),
            ("GOLD: EXXEN SPORTS 1 sᴅ", "GOLD: EXXEN SPORTS 1", "", "SD"),
            ("SE: Nick Jr ᴴᴰ ◉ *MULTI* ⁵⁰ᶠᵖˢ", "Nick Jr", "SE", "HD"),
            ("KZ: Қазақстан ғылым", "Қазақстан ғылым", "KZ", ""),
            ("PH: ONE SPORTS (HYPER ᴴᴰ)", "ONE SPORTS (HYPER)", "PH", "HD"),
            ("TNT ULTIMATE ᵁᴴᴰ ³⁸⁴⁰ᴾ+", "TNT ULTIMATE", "", "4K"),
        ];
        for (raw, name, country, quality) in cases {
            let c = clean_channel_name(raw);
            assert_eq!((c.name.as_str(), c.country.as_deref().unwrap_or(""), c.quality), (*name, *country, q(quality)), "{raw:?}");
        }
    }

    #[test]
    fn never_empty() {
        assert_eq!(ch("HD").0, "HD");
        assert_eq!(ch("[VIP]").0, "[VIP]");
        assert_eq!(ch("UK |").0, "UK");
        assert_eq!(ch(" UK: HD ").0, "UK", "a prefix is never all that is left");
        assert_eq!(ch("CAR ★ FX | HD").0, "FX");
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
        assert_eq!(clean_vod_name("EN ★ The Lion King - 1994"), ("The Lion King".into(), Some(1994)));
        assert_eq!(clean_vod_name("DE - T.I.M. (2023)"), ("T.I.M.".into(), Some(2023)));
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
        // decorated categories
        assert_eq!(clean_group_name("UK ❖ General"), "General");
        assert_eq!(clean_group_name("GENERAL ᴴᴰ/"), "General");
        assert_eq!(clean_group_name("★ FR ★ CINÉMA"), "Cinéma");
        assert_eq!(clean_group_name("VIP ❖ Premier League+"), "Premier League+");
    }

    #[test]
    fn plus_is_part_of_the_name() {
        assert_eq!(ch("Canal +").0, "Canal +");
        assert_eq!(ch("CANAL + HD"), ("CANAL +".into(), None, Some(Quality::Hd), vec![]));
        assert_eq!(ch("FR: Disney + 4K").0, "Disney +");
        assert_eq!(ch("UK | BBC One |").0, "BBC One");
    }
}
