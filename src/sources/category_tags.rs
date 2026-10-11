//! Country / language tags of category names (DESIGN §28.2): detection, display names, the
//! defaults of the category filter, and per-tag summaries for the "Choose what to show" picker.
//!
//! Detection reads the provider's RAW category name (`grp.raw_name`): smart-name cleanup drops
//! the prefix from the display name and the group key squashes the separators away.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use crate::model::{CategoryFilter, CategoryTag, ItemKind, TagKind};
use crate::sources::name_tokens::{Kind, Token, closing, deco_ascii, flag_code, is_format_word, is_service_word, quality_word, tokens};

/// Countries whose categories count as English-language by default (with `lang:en`).
pub const ENGLISH_COUNTRIES: &[&str] = &["GB", "US", "CA", "AU", "IE", "NZ"];

/// ISO 3166-1 alpha-2 codes (the tz database's `iso3166.tab`, embedded: detection must not
/// depend on tzdata being installed).
const ISO2: &str = "AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ \
     CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF GG \
     GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN KP KR KW KY KZ \
     LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA NC NE NF NG NI NL NO NP NR \
     NU NZ OM PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ \
     TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW";

/// Other tokens → canonical tag (upper case; they override [`ISO2`]: `AR` is Arabic, not
/// Argentina — IPTV convention — and `ENG` is the English language, not England).
const SYNONYMS: &[(&str, &[&str])] = &[
    ("GB", &["UK", "GBR", "BRITAIN", "BRITISH", "ENGLAND", "SCOTLAND", "WALES"]),
    ("US", &["USA", "AMERICA"]),
    ("CA", &["CAN", "CANADA"]),
    ("AU", &["AUS", "AUSTRALIA"]),
    ("IE", &["IRL", "IRELAND", "IRISH"]),
    // `NEW ZEALAND`: `NEW` is a non-tag
    ("NZ", &["NZL", "ZEALAND"]),
    ("AE", &["UAE", "EMIRATES"]),
    ("SA", &["KSA", "SAU", "SAUDI"]),
    ("DE", &["DEU", "GER", "GERMANY", "DEUTSCHLAND"]),
    ("FR", &["FRA", "FRANCE"]),
    ("IT", &["ITA", "ITALY", "ITALIA"]),
    ("ES", &["ESP", "SPA", "SPAIN", "ESPANA", "ESPAÑA"]),
    ("PT", &["POR", "PRT", "PORTUGAL"]),
    ("BR", &["BRA", "BRAZIL", "BRASIL"]),
    ("AR", &["ARG", "ARGENTINA"]),
    ("MX", &["MEX", "MEXICO"]),
    ("IN", &["IND", "INDIA", "INDIAN"]),
    ("PK", &["PAK", "PAKISTAN"]),
    ("TR", &["TUR", "TURKEY", "TURKIYE", "TÜRKIYE", "TÜRKİYE"]),
    ("RU", &["RUS", "RUSSIA"]),
    ("UA", &["UKR", "UKRAINE"]),
    ("PL", &["POL", "POLAND", "POLSKA"]),
    ("NL", &["NLD", "NED", "HOLLAND", "NETHERLANDS", "NEDERLAND"]),
    ("BE", &["BEL", "BELGIUM", "BELGIQUE"]),
    // `SW` is Sweden on IPTV panels (`SW ★ …`)
    ("SE", &["SWE", "SW", "SWEDEN", "SVERIGE", "SVENSKA", "SVENSK", "SVENSKE"]),
    ("NO", &["NOR", "NORWAY", "NORGE", "NORSK", "NORSKE"]),
    ("DK", &["DNK", "DEN", "DENMARK", "DANMARK", "DANSK", "DANSKE"]),
    ("FI", &["FIN", "FINLAND", "SUOMI", "SUOMALAISET"]),
    ("IS", &["ISL", "ICELAND", "ÍSLANDS", "ÍSLENSK", "ÍSLENSKAR"]),
    ("CH", &["CHE", "SUI", "SWITZERLAND", "SCHWEIZ", "SUISSE"]),
    ("AT", &["AUT", "AUSTRIA"]),
    ("GR", &["GRE", "GRC", "GREECE"]),
    ("CZ", &["CZE", "CZECH", "CZECHIA"]),
    ("SK", &["SVK", "SLOVAKIA"]),
    ("HU", &["HUN", "HUNGARY"]),
    ("RO", &["ROU", "ROM", "RUM", "ROMANIA"]),
    ("BG", &["BGR", "BUL", "BULGARIA"]),
    ("RS", &["SRB", "SERBIA"]),
    ("HR", &["HRV", "CRO", "CROATIA", "HRVATSKA"]),
    ("BA", &["BIH", "BOSNIA"]),
    ("SI", &["SVN", "SLO", "SLOVENIA"]),
    ("MK", &["MKD", "MACEDONIA"]),
    ("AL", &["ALB", "ALBANIA"]),
    ("CN", &["CHN", "CHINA"]),
    ("JP", &["JPN", "JAP", "JAPAN"]),
    ("KR", &["KOR", "KOREA"]),
    ("PH", &["PHI", "FIL", "PHILIPPINES", "PINOY"]),
    ("TH", &["THA", "THAILAND"]),
    ("VN", &["VIE", "VIETNAM"]),
    ("ID", &["IDN", "INDONESIA"]),
    ("MY", &["MYS", "MALAYSIA"]),
    ("SG", &["SINGAPORE"]),
    ("IL", &["ISR", "ISRAEL"]),
    ("IR", &["IRN", "IRAN"]),
    ("IQ", &["IRQ", "IRAQ"]),
    ("EG", &["EGY", "EGYPT"]),
    ("MA", &["MAR", "MOROCCO", "MAROC"]),
    ("TN", &["TUN", "TUNISIA"]),
    ("DZ", &["DZA", "ALG", "ALGERIA"]),
    ("NG", &["NGA", "NIGERIA"]),
    ("ZA", &["ZAF"]),
    ("CL", &["CHL", "CHILE"]),
    ("CO", &["COL", "COLOMBIA"]),
    ("VE", &["VEN", "VENEZUELA"]),
    ("PE", &["PERU"]),
    ("LU", &["LUX", "LUXEMBOURG"]),
    ("LT", &["LTU", "LITHUANIA"]),
    ("LV", &["LVA", "LATVIA"]),
    ("EE", &["EST", "ESTONIA"]),
    ("GE", &["GEO"]),
    ("AM", &["ARM", "ARMENIA"]),
    ("AZ", &["AZE", "AZERBAIJAN"]),
    ("KZ", &["KAZ", "KAZAKHSTAN"]),
    ("UZ", &["UZB"]),
    ("BD", &["BGD", "BANGLADESH"]),
    ("LK", &["LKA"]),
    ("NP", &["NPL", "NEPAL"]),
    // `AF` is Africa (below): panels use `AF| NIGERIA`; Afghan categories use `AFG`
    ("AF", &["AFG", "AFGHANISTAN"]),
    ("SY", &["SYR", "SYRIA"]),
    ("LB", &["LBN", "LEBANON"]),
    ("JO", &["JOR"]),
    ("KW", &["KWT", "KUWAIT"]),
    ("QA", &["QAT", "QATAR"]),
    ("BH", &["BHR", "BAHRAIN"]),
    ("OM", &["OMN", "OMAN"]),
    ("YE", &["YEM", "YEMEN"]),
    ("SO", &["SOM", "SOMALIA"]),
    ("ET", &["ETH", "ETHIOPIA"]),
    ("KE", &["KEN", "KENYA"]),
    ("GH", &["GHA", "GHANA"]),
    ("CM", &["CMR", "CAMEROON"]),
    ("SN", &["SEN", "SENEGAL"]),
    ("CI", &["CIV", "COTEDIVOIRE"]),
    ("CY", &["CYPRUS"]),
    ("MT", &["MALTA"]),
    ("JM", &["JAMAICA"]),
    // full names seen as channel / category prefixes (`URUGUAY ★ …`, `SENEGAL: …`)
    ("UY", &["URUGUAY"]),
    ("DO", &["DOMINICAN", "DOMINICANA"]),
    ("EC", &["ECUADOR"]),
    ("HN", &["HONDURAS"]),
    ("GT", &["GUATEMALA"]),
    ("PA", &["PANAMA"]),
    ("CU", &["CUBA"]),
    ("UG", &["UGANDA"]),
    ("TZ", &["TANZANIA"]),
    ("ML", &["MALI"]),
    ("GN", &["GUINEE", "GUINEA"]),
    ("BF", &["BURKINAFASO"]),
    ("TG", &["TOGO"]),
    ("BJ", &["BENIN"]),
    ("GA", &["GABON"]),
    ("TD", &["TCHAD", "CHAD"]),
    ("GM", &["GAMBIA"]),
    ("SL", &["SIERRALEONE"]),
    ("CV", &["CAPEVERDE"]),
    ("AO", &["ANGOLA"]),
    ("MZ", &["MOZAMBIQUE"]),
    ("NA", &["NAMIBIA"]),
    ("ER", &["ERITREA"]),
    ("DJ", &["DJIBOUTI"]),
    ("lang:en", &["EN", "ENG", "ENGLISH"]),
    ("lang:ar", &["AR", "ARA", "ARAB", "ARABIC"]),
    ("lang:fr", &["FRE", "FRENCH", "FRANCAIS", "FRANÇAIS"]),
    ("lang:de", &["GERMAN", "DEUTSCH"]),
    ("lang:it", &["ITALIAN"]),
    ("lang:es", &["SPANISH", "ESPANOL", "ESPAÑOL"]),
    ("lang:pt", &["PORTUGUESE"]),
    ("lang:tr", &["TURKISH"]),
    ("lang:ru", &["RUSSIAN"]),
    ("lang:pl", &["POLISH"]),
    ("lang:nl", &["DUT", "DUTCH"]),
    ("lang:el", &["GREEK"]),
    ("lang:hi", &["HIN", "HINDI"]),
    ("lang:ur", &["URD", "URDU"]),
    ("lang:ta", &["TAM", "TAMIL"]),
    ("lang:te", &["TEL", "TELUGU", "TELEGU"]),
    ("lang:or", &["ODIA", "ORIYA"]),
    ("lang:pa", &["PUNJABI"]),
    ("lang:bn", &["BANGLA", "BENGALI"]),
    ("lang:ml", &["MAL", "MALAYALAM"]),
    ("lang:kn", &["KANNADA"]),
    ("lang:mr", &["MARATHI"]),
    ("lang:gu", &["GUJARATI"]),
    ("lang:ku", &["KU", "KUR", "KURD", "KURDISH"]),
    ("lang:fa", &["FA", "PER", "PERSIAN", "FARSI"]),
    ("lang:zh", &["CHI", "CHINESE"]),
    ("lang:ja", &["JAPANESE"]),
    ("lang:ko", &["KOREAN"]),
    ("lang:he", &["HEBREW"]),
    ("lang:so", &["SOMALI"]),
    ("lang:tl", &["TAGALOG", "FILIPINO"]),
    ("lang:sq", &["ALBANIAN"]),
    ("lang:hy", &["ARMENIAN"]),
    ("region:latam", &["LAT", "LATIN", "LATINO", "LATINA", "LATAM"]),
    ("region:exyu", &["EX-YU", "EXYU", "YU", "BALKAN", "BALKANS"]),
    ("region:africa", &["AF", "AFR", "AFRICA", "AFRIKA", "AFRICAN"]),
    // `AS ❖ Singapore`: Asia in IPTV line-ups, not American Samoa
    ("region:asia", &["AS", "ASI", "ASIA", "ASIAN"]),
    ("region:caribbean", &["CAR", "CARIB", "CARIBBEAN"]),
    ("region:nordic", &["NORDIC", "NORDICS", "SCANDINAVIA", "SCANDI"]),
    ("region:europe", &["EU", "EUROPE"]),
];

/// Leading / trailing words that are not countries (access, format…): skipped when bracketed
/// or separated, never a tag (`TV` and `SD` are also ISO codes, `FM` too). The quality, format
/// and service words of `name_tokens` (`4K`, `HEVC`, `VIP`, `24/7`…) and numbers are skipped
/// the same way.
const NON_TAGS: &[&str] = &[
    // Sudan's code, but a quality word in names
    "SD",
    "VOD",
    "NEW",
    "XXX",
    "ADULT",
    "18+",
    "TV",
    "FM",
    "LIVE",
    "RADIO",
    "MULTI",
    "INT",
    "INTL",
    "INTERNATIONAL",
    "WORLD",
    "TOP",
    "PREMIUM",
    // `EX YU`: the `YU` after it is the tag
    "EX",
];

/// Two-letter codes that count when followed by a space only (`"UK SPORTS"`) or joined by `.` /
/// an unspaced `-` (`"UK.Sports"`, `"UK-DOCS"`): the usual IPTV prefixes. Any other two-letter
/// code needs a separator or a bracket — too many are words, brands or competitions (`"IT
/// Crowd"`, `"IN CINEMAS"`, `"AL JAZEERA"`, `"BT Sport"`, `"FA Cup"`, `"CL Matches"`, `"CH 5"`,
/// `"MK Dons"`, `"EE TV"`).
const SPACED_CODES: &[&str] = &[
    "UK", "GB", "US", "CA", "AU", "NZ", "IE", "EN", "AR", "DE", "FR", "ES", "PT", "NL", "TR", "GR", "RU", "UA", "RO", "BG", "HU", "CZ", "SK", "RS",
    "SI", "DK", "FI", "IL", "IR", "IQ", "EG", "LB", "JO", "KW", "QA", "AE", "PK", "BD", "LK", "NP", "PH", "TH", "VN", "JP", "KR", "CN", "TW", "HK",
    "MX", "BR", "VE", "UY", "NG", "GH", "KE", "ZA", "AZ", "KZ", "EU", "AF", "YU",
];

/// Three-letter codes that are also English words: not tags with a space only or `.` / `-`
/// joined (`"CAN SPORTS"`), still tags with a separator (`"CAN | Sports"`).
const AMBIGUOUS: &[&str] =
    &["ARM", "BRA", "CAN", "CAR", "CHI", "COL", "DEN", "EST", "FIN", "GEO", "KEN", "MAR", "PER", "ROM", "RUM", "SEN", "SPA", "TAM", "TEL", "TUN"];

/// Full names that are adjectives: with a space only, a tag only in capitals (`"INDIAN
/// MOVIES"`), not in a title (`"Indian Premier League"`, `"African Cup"`). Language names are
/// treated the same (`"German Bundesliga"`, `"French Open"`).
const DEMONYMS: &[&str] = &["INDIAN", "BRITISH", "IRISH", "AFRICAN", "ASIAN", "LATIN", "CARIBBEAN", "NORDIC"];

/// Codes not read at the end (`"Football | PL"`, `"Highlights - CL"`: Premier / Champions
/// League). At the start they stay countries (`"PL | Polsat"`).
const NOT_TRAILING: &[&str] = &["PL", "CL"];

/// Leading / trailing tokens looked at (non-tags before the tag included).
const MAX_TOKENS: usize = 3;
/// Longest bracket content read as a token list (`[UK]`, `| VIP UK |`).
const MAX_BRACKET: usize = 24;

/// Token (upper case) → `Some(canonical tag)`, or `None` for a non-tag.
fn table() -> &'static HashMap<String, Option<&'static str>> {
    static TABLE: OnceLock<HashMap<String, Option<&'static str>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t: HashMap<String, Option<&'static str>> = ISO2.split_ascii_whitespace().map(|cc| (cc.to_owned(), Some(cc))).collect();
        for (tag, tokens) in SYNONYMS {
            t.extend(tokens.iter().map(|tok| (tok.to_uppercase(), Some(*tag))));
        }
        t.extend(NON_TAGS.iter().map(|tok| ((*tok).to_owned(), None)));
        t
    })
}

/// What one word of a name is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WordTag {
    Tag(CategoryTag),
    /// A known word that is no tag: quality, format, access (`4K`, `HEVC`, `VIP`), numbers.
    NonTag,
    Unknown,
}

impl WordTag {
    fn tag(self) -> Option<CategoryTag> {
        match self {
            WordTag::Tag(t) => Some(t),
            WordTag::NonTag | WordTag::Unknown => None,
        }
    }
}

/// A word → tag (ISO codes, [`SYNONYMS`]), non-tag ([`NON_TAGS`], the shared quality /
/// format / service words of `name_tokens`, numbers) or unknown. Shared with smart names'
/// prefix stripping.
pub(crate) fn classify(word: &str) -> WordTag {
    if word.bytes().all(|b| b.is_ascii_digit()) {
        return WordTag::NonTag;
    }
    match table().get(&word.to_uppercase()) {
        Some(Some(tag)) => WordTag::Tag(CategoryTag((*tag).to_owned())),
        Some(None) => WordTag::NonTag,
        None if quality_word(word).is_some() || is_format_word(word) || is_service_word(word) => WordTag::NonTag,
        None => WordTag::Unknown,
    }
}

/// A flag's letters are an ISO code first (`🇦🇷` is Argentina, not Arabic), else a synonym
/// (`🇪🇺` → Europe); England's, Scotland's and Wales' flags are GB.
pub(crate) fn flag_tag(flag: &str) -> Option<CategoryTag> {
    let code = flag_code(flag);
    if matches!(code.as_str(), "gbeng" | "gbsct" | "gbwls") {
        return Some(CategoryTag::country("GB"));
    }
    if code.len() != 2 {
        return None;
    }
    if ISO2.split_ascii_whitespace().any(|cc| cc == code) {
        return Some(CategoryTag::country(&code));
    }
    classify(&code).tag()
}

/// The tokens of a name for detection: a word with joiners that is not itself a known
/// compound (`EX-YU`, `24/7`) is split at them (`UK-DOCS`, `UK.Sports`, `UK/US`, `Sci-Fi`), the
/// joiner becoming an unspaced separator.
fn tag_tokens(name: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    for t in tokens(name) {
        if !t.is(Kind::Word) || !matches!(classify(t.text), WordTag::Unknown) {
            out.push(t);
            continue;
        }
        let mut spaced = t.spaced;
        let mut start = 0;
        for (i, c) in t.text.char_indices() {
            if matches!(c, '-' | '–' | '.' | '/' | '_' | ':' | ',') && i > start {
                out.push(Token { kind: Kind::Word, text: &t.text[start..i], spaced });
                out.push(Token { kind: Kind::Sep, text: &t.text[i..i + c.len_utf8()], spaced: false });
                spaced = false;
                start = i + c.len_utf8();
            }
        }
        out.push(Token { kind: Kind::Word, text: &t.text[start..], spaced });
    }
    out
}

/// Byte length of tokens as written.
fn span_len(toks: &[Token<'_>]) -> usize {
    toks.iter().enumerate().map(|(i, t)| t.text.len() + usize::from(i > 0 && t.spaced)).sum()
}

/// A bracket starting at `i` (`[X]`, `(X)`, `{X}`, `|X|`…): its inner tokens and the index after
/// it. Not for empty or long contents.
fn bracket<'t, 'a>(t: &'t [Token<'a>], i: usize) -> Option<(&'t [Token<'a>], usize)> {
    let close = match t.get(i)? {
        o if o.is(Kind::Open) => closing(t, i)?,
        p if p.is(Kind::Sep) && p.text == "|" => t[i + 1..].iter().position(|x| x.is(Kind::Sep) && x.text.starts_with('|'))? + i + 1,
        _ => return None,
    };
    let inner = &t[i + 1..close];
    (inner.iter().any(|x| !x.is(Kind::Sep)) && span_len(inner) <= MAX_BRACKET).then_some((inner, close + 1))
}

/// A bracket ending at `end - 1`: the index of its opener and its inner tokens.
fn bracket_end<'t, 'a>(t: &'t [Token<'a>], end: usize) -> Option<(usize, &'t [Token<'a>])> {
    let last = t.get(end.checked_sub(1)?)?;
    let open = if last.is(Kind::Close) {
        let want = match last.text {
            ")" => "(",
            "]" => "[",
            "}" => "{",
            "⁾" => "⁽",
            "】" => "【",
            "」" => "「",
            _ => return None,
        };
        t[..end - 1].iter().rposition(|x| x.is(Kind::Open) && x.text == want)?
    } else if last.is(Kind::Sep) && last.text == "|" {
        t[..end - 1].iter().rposition(|x| x.is(Kind::Sep) && x.text.ends_with('|'))?
    } else {
        return None;
    };
    let inner = &t[open + 1..end - 1];
    (inner.iter().any(|x| !x.is(Kind::Sep)) && span_len(inner) <= MAX_BRACKET).then_some((open, inner))
}

/// The words of a bracket: a tag when every word is a tag, a flag or a non-tag (`[UK]`,
/// `[VIP UK]`, `[🇬🇧]`, first tag wins), a non-tag when all are non-tags (`[4K]`), unknown
/// otherwise (`[In Cinemas]`).
fn classify_inner(inner: &[Token<'_>]) -> WordTag {
    let mut found = WordTag::NonTag;
    for t in inner {
        let class = match t.kind {
            Kind::Word => classify(t.text),
            Kind::Deco => classify(&deco_ascii(t.text)),
            Kind::Flag => flag_tag(t.text).map_or(WordTag::NonTag, WordTag::Tag),
            Kind::Sep | Kind::Open | Kind::Close => continue,
        };
        match class {
            WordTag::Unknown => return WordTag::Unknown,
            WordTag::Tag(tag) if !matches!(found, WordTag::Tag(_)) => found = WordTag::Tag(tag),
            WordTag::Tag(_) | WordTag::NonTag => {}
        }
    }
    found
}

/// A code joined to the rest by `.` or an unspaced `-` (`"UK.Sports"`, `"UK-DOCS"`) or followed
/// by a space only: in capitals, and a usual prefix ([`SPACED_CODES`]) or a three-letter code
/// that is not a word ([`AMBIGUOUS`]).
fn short_code_ok(word: &str) -> bool {
    !word.chars().any(char::is_lowercase)
        && match word.chars().count() {
            2 => SPACED_CODES.contains(&word),
            3 => !AMBIGUOUS.contains(&word),
            _ => false,
        }
}

/// A tag followed by a space only counts when it cannot be an ordinary word: a short code per
/// [`short_code_ok`], or a full name (4+ letters) — in mixed case only a country or region
/// name, not a language or a nationality adjective (`"Latino Peliculas"` yes, `"German
/// Bundesliga"`, `"Indian Premier League"` no).
fn space_separated_ok(word: &str, tag: &CategoryTag) -> bool {
    if word.chars().count() < 4 {
        return short_code_ok(word);
    }
    !word.chars().any(char::is_lowercase) || (tag.kind() != TagKind::Language && !DEMONYMS.contains(&word.to_uppercase().as_str()))
}

/// Grammar steps 1–7 of DESIGN §28.2: the tag at the start of the name.
fn leading(t: &[Token<'_>]) -> Option<CategoryTag> {
    let mut i = 0;
    for _ in 0..MAX_TOKENS {
        // decoration (`★ ✦ ◉ • ▎ # »`, `ᴴᴰ`, a stray bracket) up to a word, flag or bracket
        while let Some(tok) = t.get(i) {
            let deco = match tok.kind {
                Kind::Sep | Kind::Open => bracket(t, i).is_none(),
                Kind::Deco | Kind::Close => true,
                Kind::Word | Kind::Flag => false,
            };
            if !deco {
                break;
            }
            i += 1;
        }
        let tok = t.get(i)?;
        match tok.kind {
            Kind::Flag => match flag_tag(tok.text) {
                Some(tag) => return Some(tag),
                None => i += 1,
            },
            Kind::Open | Kind::Sep => {
                let (inner, after) = bracket(t, i)?;
                match classify_inner(inner) {
                    WordTag::Tag(tag) => return Some(tag),
                    WordTag::NonTag => i = after,
                    WordTag::Unknown => return None,
                }
            }
            Kind::Word => {
                let word = tok.text;
                match t.get(i + 1) {
                    // the whole (remaining) name: `ENGLISH`, `VIP | UK`
                    None => return classify(word).tag(),
                    Some(n) if n.is(Kind::Sep) => {
                        // `UK-DOCS`, `UK.Sports`: only a short code joins the next word
                        // (`Sci-Fi`, `Al-Jazeera`, `No. 1` are words)
                        let glued_word = t.get(i + 2).is_some_and(|w| w.is(Kind::Word) && !w.spaced);
                        let joined = !n.spaced && (n.text == "." || (matches!(n.text, "-" | "–") && glued_word));
                        match classify(word) {
                            WordTag::Tag(tag) if !joined || short_code_ok(word) => return Some(tag),
                            WordTag::NonTag if !joined => i += 1,
                            WordTag::Tag(_) | WordTag::NonTag | WordTag::Unknown => return None,
                        }
                    }
                    Some(n) if n.spaced => match classify(word) {
                        WordTag::Tag(tag) => return space_separated_ok(word, &tag).then_some(tag),
                        // `VIP UK | Sports`, `4K UHD | UK`, `NEW ZEALAND`
                        WordTag::NonTag => i += 1,
                        WordTag::Unknown => return None,
                    },
                    Some(_) => return None,
                }
            }
            Kind::Deco | Kind::Close => unreachable!("skipped as decoration"),
        }
    }
    None
}

/// Grammar step 8: `Sports (UK)`, `Movies [EN]`, `NETFLIX - EN`, `Sports | UK` (trailing
/// non-tags and decoration skipped: `Sports | UK | HD`, `Sports | UK ◉`).
fn trailing(t: &[Token<'_>]) -> Option<CategoryTag> {
    let mut end = t.len();
    for _ in 0..MAX_TOKENS {
        while let Some(last) = end.checked_sub(1).and_then(|e| t.get(e)) {
            let deco = match last.kind {
                Kind::Sep | Kind::Close => bracket_end(t, end).is_none(),
                Kind::Deco | Kind::Open => true,
                Kind::Word | Kind::Flag => false,
            };
            if !deco {
                break;
            }
            end -= 1;
        }
        let last = t.get(end.checked_sub(1)?)?;
        if let Some((open, inner)) = bracket_end(t, end) {
            match classify_inner(inner) {
                WordTag::Tag(tag) => return Some(tag),
                WordTag::NonTag => {
                    end = open;
                    continue;
                }
                WordTag::Unknown => return None,
            }
        }
        if last.is(Kind::Flag) {
            return flag_tag(last.text);
        }
        // nothing before the word: the whole name, handled by `leading`; a word before it
        // (`Sports UK`) or a hyphen (`Movies-UK`): no separator
        let sep = t.get(end.checked_sub(2)?)?;
        let hyphen = sep.text.starts_with('-') && !sep.spaced && !last.spaced && end >= 3 && t[end - 3].is(Kind::Word);
        if !sep.is(Kind::Sep) || hyphen {
            return None;
        }
        match classify(last.text) {
            WordTag::Tag(tag) => return (!NOT_TRAILING.contains(&last.text.to_uppercase().as_str())).then_some(tag),
            WordTag::NonTag => end -= 2,
            WordTag::Unknown => return None,
        }
    }
    None
}

/// The tag of a raw category name, if it has one (grammar, synonyms and non-tags: DESIGN
/// §28.2), read through the shared name tokenizer (`name_tokens`). `"UK | Sports"`, `"|UK|
/// Movies"`, `"[UK] Kids"`, `"UK: News"`, `"UK ★ News"`, `"UK ❖ News"`, `"🇬🇧 Sports"` →
/// `GB`; `"EN ▎Movies"`, `"ENGLISH"` → `lang:en`; `"4K | Sports"`, `"VIP"`, `"CNN: Live"` →
/// none.
pub fn detect_tag(raw: &str) -> Option<CategoryTag> {
    let t = tag_tokens(raw.trim());
    leading(&t).or_else(|| trailing(&t))
}

/// Is this one of the English-language tags (pre-ticked by default)?
pub fn is_english(tag: &CategoryTag) -> bool {
    match tag.kind() {
        TagKind::Country => ENGLISH_COUNTRIES.contains(&tag.code()),
        TagKind::Language => tag.code() == "en",
        TagKind::Region => false,
    }
}

/// Does `rule` hide titles in other languages (DESIGN §28.12)? Its own choice, else
/// automatically when every tag it keeps is an English-language one.
pub fn hides_other_languages(rule: &CategoryFilter) -> bool {
    rule.hide_other_languages.unwrap_or_else(|| keeps_only_english(&rule.keep))
}

/// The automatic "Also hide titles in other languages": only English-language tags kept.
pub fn keeps_only_english(keep: &BTreeSet<CategoryTag>) -> bool {
    !keep.is_empty() && keep.iter().all(is_english)
}

/// The languages (ISO 639-1) whose titles `rule` keeps when it hides titles in other languages:
/// the language tags it keeps and the main language of each country it keeps.
/// Ex-Yugoslav titles (`EX - …`, `vod_match::market_lang`) and the countries / region that
/// keep them count as one language, Serbo-Croatian (`sh`).
pub fn rule_languages(rule: &CategoryFilter) -> BTreeSet<&'static str> {
    let mut out = BTreeSet::new();
    for t in &rule.keep {
        let lang = match t.kind() {
            TagKind::Language => LANGUAGE_NAMES.iter().find(|(c, _)| *c == t.code()).map(|(c, _)| *c),
            TagKind::Country => country_language(t.code()),
            TagKind::Region => None,
        };
        out.extend(lang);
        if t.code() == "exyu" || matches!(t.code(), "RS" | "HR" | "BA" | "ME") && t.kind() == TagKind::Country {
            out.insert(SERBO_CROATIAN);
        }
    }
    out
}

/// The language of Ex-Yugoslav markets (see [`rule_languages`]).
pub const SERBO_CROATIAN: &str = "sh";

/// Default keep-list (DESIGN §28.6): the user's country (ISO code from
/// `presets::detect_country`) and its main language, plus every English-language tag — whether
/// or not the playlist has them yet, so English categories added later are shown.
pub fn default_keep(country: Option<&str>) -> BTreeSet<CategoryTag> {
    let mut keep: BTreeSet<CategoryTag> = ENGLISH_COUNTRIES.iter().map(|cc| CategoryTag::country(cc)).collect();
    keep.insert(CategoryTag::language("en"));
    if let Some(cc) = country.filter(|cc| cc.len() == 2) {
        keep.insert(CategoryTag::country(cc));
        if let Some(lang) = country_language(cc) {
            keep.insert(CategoryTag::language(lang));
        }
    }
    keep
}

/// Main language of a country, for [`default_keep`] (`"FR"` → `"fr"`); `None` when unknown.
pub fn country_language(iso: &str) -> Option<&'static str> {
    Some(match iso.to_ascii_uppercase().as_str() {
        "GB" | "US" | "CA" | "AU" | "IE" | "NZ" => "en",
        "FR" | "BE" | "LU" | "MC" => "fr",
        "DE" | "AT" | "CH" | "LI" => "de",
        "ES" | "MX" | "AR" | "CO" | "CL" | "PE" | "VE" | "EC" | "UY" | "PY" | "BO" | "CR" | "PA" | "GT" | "HN" | "SV" | "NI" | "DO" | "CU" | "PR" => {
            "es"
        }
        "IT" | "SM" => "it",
        "PT" | "BR" | "AO" | "MZ" => "pt",
        "NL" => "nl",
        "TR" => "tr",
        "PL" => "pl",
        "RU" | "BY" => "ru",
        "UA" => "uk",
        "GR" | "CY" => "el",
        "IN" => "hi",
        "PK" => "ur",
        "BD" => "bn",
        "SA" | "AE" | "EG" | "MA" | "DZ" | "TN" | "LY" | "IQ" | "JO" | "LB" | "SY" | "KW" | "QA" | "BH" | "OM" | "YE" | "SD" | "PS" => "ar",
        "IR" | "AF" => "fa",
        "IL" => "he",
        "SE" => "sv",
        "NO" => "no",
        "DK" => "da",
        "FI" => "fi",
        "IS" => "is",
        "RO" | "MD" => "ro",
        "HU" => "hu",
        "CZ" => "cs",
        "SK" => "sk",
        "BG" => "bg",
        "RS" | "ME" => "sr",
        "HR" => "hr",
        "SI" => "sl",
        "AL" | "XK" => "sq",
        "AM" => "hy",
        "CN" | "TW" | "HK" => "zh",
        "JP" => "ja",
        "KR" => "ko",
        "TH" => "th",
        "VN" => "vi",
        "ID" => "id",
        "PH" => "tl",
        "SO" => "so",
        _ => return None,
    })
}

/// Display names of the language tags (ISO 639-1 code → English name).
const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("en", "English"),
    ("ar", "Arabic"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("es", "Spanish"),
    ("pt", "Portuguese"),
    ("tr", "Turkish"),
    ("ru", "Russian"),
    ("pl", "Polish"),
    ("nl", "Dutch"),
    ("el", "Greek"),
    ("hi", "Hindi"),
    ("ur", "Urdu"),
    ("ta", "Tamil"),
    ("or", "Odia"),
    ("te", "Telugu"),
    ("pa", "Punjabi"),
    ("bn", "Bengali"),
    ("ml", "Malayalam"),
    ("kn", "Kannada"),
    ("mr", "Marathi"),
    ("gu", "Gujarati"),
    ("ku", "Kurdish"),
    ("fa", "Persian"),
    ("zh", "Chinese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("he", "Hebrew"),
    ("so", "Somali"),
    ("tl", "Filipino"),
    ("sq", "Albanian"),
    ("hy", "Armenian"),
    ("uk", "Ukrainian"),
    ("sv", "Swedish"),
    ("no", "Norwegian"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("is", "Icelandic"),
    ("ro", "Romanian"),
    ("hu", "Hungarian"),
    ("cs", "Czech"),
    ("sk", "Slovak"),
    ("bg", "Bulgarian"),
    ("sr", "Serbian"),
    ("hr", "Croatian"),
    ("sl", "Slovenian"),
    ("th", "Thai"),
    ("vi", "Vietnamese"),
    ("id", "Indonesian"),
];

/// Display names of the region tags.
const REGION_NAMES: &[(&str, &str)] = &[
    ("latam", "Latin America"),
    ("exyu", "Ex-Yugoslavia"),
    ("africa", "Africa"),
    ("asia", "Asia"),
    ("caribbean", "Caribbean"),
    ("nordic", "Nordic"),
    ("europe", "Europe"),
];

/// Short code for chips and lists: `GB` → `"UK"`, `lang:en` → `"EN"`, `region:latam` →
/// `"LATAM"`, `region:exyu` → `"EX-YU"`.
pub fn tag_short(tag: &CategoryTag) -> String {
    match tag.kind() {
        TagKind::Country if tag.code() == "GB" => "UK".to_owned(),
        TagKind::Region if tag.code() == "exyu" => "EX-YU".to_owned(),
        _ => tag.code().to_ascii_uppercase(),
    }
}

/// Display name: `GB` → "United Kingdom", `lang:ar` → "Arabic", `region:exyu` →
/// "Ex-Yugoslavia". Falls back to [`tag_short`].
pub fn tag_label(tag: &CategoryTag) -> String {
    let named = |names: &[(&str, &'static str)]| names.iter().find(|(c, _)| *c == tag.code()).map(|(_, n)| (*n).to_owned());
    match tag.kind() {
        TagKind::Country => crate::sources::presets::country_name(tag.code()),
        TagKind::Language => named(LANGUAGE_NAMES),
        TagKind::Region => named(REGION_NAMES),
    }
    .unwrap_or_else(|| tag_short(tag))
}

/// One entry of the picker: a tag (or the untagged categories) with what it covers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagSummary {
    /// `None` = the categories without a tag ("No country tag").
    pub tag: Option<CategoryTag>,
    pub live_groups: u32,
    pub movie_groups: u32,
    pub series_groups: u32,
    /// Sums of the groups' `item_count` (an item in two groups counts twice).
    pub channels: u32,
    pub movies: u32,
    pub series: u32,
}

impl TagSummary {
    pub fn items(&self) -> u32 {
        self.channels + self.movies + self.series
    }
    pub fn groups(&self) -> u32 {
        self.live_groups + self.movie_groups + self.series_groups
    }
}

/// Per-tag summary of one source's categories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CategorySummary {
    /// Tagged entries by `items()` descending (ties: label), then the untagged entry (present
    /// only when the source has untagged categories).
    pub tags: Vec<TagSummary>,
    /// Some categories were imported before raw names were stored (smart names on): their
    /// tags are unknown until the playlist is refreshed.
    pub needs_refresh: bool,
}

impl CategorySummary {
    /// Number of distinct tags (the untagged entry not counted). The onboarding step is shown
    /// when this is ≥ 2.
    pub fn tag_count(&self) -> usize {
        self.tags.iter().filter(|t| t.tag.is_some()).count()
    }
    /// Every tag present (what `CategoryFilter::known` is set to on save).
    pub fn present(&self) -> BTreeSet<CategoryTag> {
        self.tags.iter().filter_map(|t| t.tag.clone()).collect()
    }
}

/// Summarize categories given as `(kind, raw name, item count)` (pure; the store feeds it).
pub fn summarize<'a>(groups: impl IntoIterator<Item = (ItemKind, &'a str, u32)>) -> Vec<TagSummary> {
    let mut by_tag: HashMap<Option<CategoryTag>, TagSummary> = HashMap::new();
    for (kind, raw, items) in groups {
        if kind == ItemKind::Episode {
            continue;
        }
        let tag = detect_tag(raw);
        let e = by_tag.entry(tag.clone()).or_insert_with(|| TagSummary { tag, ..TagSummary::default() });
        let (groups, count) = match kind {
            ItemKind::Channel => (&mut e.live_groups, &mut e.channels),
            ItemKind::Movie => (&mut e.movie_groups, &mut e.movies),
            ItemKind::Series | ItemKind::Episode => (&mut e.series_groups, &mut e.series),
        };
        *groups += 1;
        *count = count.saturating_add(items);
    }
    let untagged = by_tag.remove(&None);
    let mut out: Vec<TagSummary> = by_tag.into_values().collect();
    out.sort_by_cached_key(|t| (std::cmp::Reverse(t.items()), t.tag.as_ref().map(tag_label)));
    out.extend(untagged);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_tag_reads_provider_patterns_and_rejects_non_tags() {
        let cases: &[(&str, Option<&str>)] = &[
            // separators, brackets, decoration
            ("UK | Sports", Some("GB")),
            ("UK: News", Some("GB")),
            ("UK - Docs", Some("GB")),
            ("|UK| Movies", Some("GB")),
            ("| UK | Sports", Some("GB")),
            ("[UK] Kids", Some("GB")),
            ("(US) Movies", Some("US")),
            ("{DE} Filme", Some("DE")),
            ("EN ▎Movies", Some("lang:en")),
            ("UK ✦ Kids", Some("GB")),
            ("UK ✦", Some("GB")),
            ("★ FR ★ Cinéma", Some("FR")),
            ("# DE: Sport", Some("DE")),
            ("UK/US Sports", Some("GB")),
            ("NL| Kinderen", Some("NL")),
            // any symbol run separates (the shared tokenizer), decoration at either end
            ("UK ★ Sports", Some("GB")),
            ("UK ❖ General", Some("GB")),
            ("UK ⓧ Movies", Some("GB")),
            ("|UK|", Some("GB")),
            ("UK:", Some("GB")),
            ("UK\u{a0}❖\u{a0}Kids", Some("GB")),
            ("Sports | UK ◉", Some("GB")),
            ("Sports | UK ᴴᴰ", Some("GB")),
            ("ALB ★ Filma", Some("AL")),
            ("LATAM ❖ Futbol", Some("region:latam")),
            ("SW ❖ Musik", Some("SE")),
            ("URUGUAY ★ Futbol", Some("UY")),
            ("CAR ★ Music", Some("region:caribbean")),
            // flags (regional indicators, England's subdivision flag)
            ("🇬🇧 Sports", Some("GB")),
            ("🇺🇸 News", Some("US")),
            ("🇫🇷", Some("FR")),
            ("🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F} Football", Some("GB")),
            ("🇪🇺 Euro News", Some("region:europe")),
            ("🇦🇷 Futbol", Some("AR")),
            ("[🇬🇧] Sports", Some("GB")),
            // whole names, full names with a space only, upper-case codes with a space only
            ("ENGLISH", Some("lang:en")),
            ("FRANCE", Some("FR")),
            ("EX-YU", Some("region:exyu")),
            ("UK", Some("GB")),
            ("UK SPORTS", Some("GB")),
            ("ARABIC NEWS", Some("lang:ar")),
            ("Latino Peliculas", Some("region:latam")),
            ("AFRICA Movies", Some("region:africa")),
            ("USA NETWORKS", Some("US")),
            // non-tags before the tag are skipped
            ("VIP | UK | Sports", Some("GB")),
            ("[4K] UK: Movies", Some("GB")),
            ("4K | UK", Some("GB")),
            ("VIP: [AR] Sports", Some("lang:ar")),
            ("24/7 | US | Shows", Some("US")),
            ("[VIP UK] Football", Some("GB")),
            ("VIP UK | Sports", Some("GB")),
            ("NEW ZEALAND: Sport", Some("NZ")),
            // joined by `.` or an unspaced `-` (usual prefixes only)
            ("UK-DOCS", Some("GB")),
            ("UK-Sky Sports", Some("GB")),
            ("UK.Sports", Some("GB")),
            ("UK. Sports", Some("GB")),
            // trailing forms
            ("Sports (UK)", Some("GB")),
            ("Movies [EN]", Some("lang:en")),
            ("NETFLIX - EN", Some("lang:en")),
            ("Sports | UK", Some("GB")),
            ("Sports | UK | HD", Some("GB")),
            ("Kids (FR) [4K]", Some("FR")),
            // synonyms
            ("GB: News", Some("GB")),
            ("GBR | News", Some("GB")),
            ("ENGLAND | Football", Some("GB")),
            ("ENG | Movies", Some("lang:en")),
            ("AR | News", Some("lang:ar")),
            ("AS ❖ Other Asia", Some("region:asia")),
            ("ARG | Futbol", Some("AR")),
            ("ARGENTINA", Some("AR")),
            ("YU: Filmovi", Some("region:exyu")),
            ("EXYU | Sport", Some("region:exyu")),
            ("EX YU Filmovi", Some("region:exyu")),
            ("AF| NIGERIA", Some("region:africa")),
            ("PL | Polsat", Some("PL")),
            ("GER | Sport", Some("DE")),
            ("DEUTSCHLAND", Some("DE")),
            ("TÜRKIYE: Diziler", Some("TR")),
            ("FARSI | Film", Some("lang:fa")),
            ("IT: Sport", Some("IT")),
            // the playlists' own-language names (field report: "SVENSKA 2020 & GAMMAL")
            ("SVENSKA 2020 & GAMMAL", Some("SE")),
            ("ÍSLANDS MOVIES", Some("IS")),
            ("SVENSKA NYA UTGÅVAN", Some("SE")),
            ("DANSKE FILM", Some("DK")),
            ("NORSKE FILMER", Some("NO")),
            ("Svenska filmer", Some("SE")),
            // the ✪ separator (field report)
            ("NL ✪ FILMS [SUB]", Some("NL")),
            ("GR ✪ ταινίες με υπότιτλους", Some("GR")),
            ("AR ✪ اجنبي دراما", Some("lang:ar")),
            ("EN ✪ MULTI-SUB [2025/2026]", Some("lang:en")),
            ("CA ✪ QUEBEC FILMS", Some("CA")),
            ("VIP ✪ كأس العالم 2026", None),
            // negatives: non-tags, unknown words, ordinary English words, hyphenated words
            ("4K | Sports", None),
            ("24/7 Movies", None),
            ("CNN: Live", None),
            ("SKY | Movies", None),
            ("NBA: Games", None),
            ("IT Crowd", None),
            ("Us Weekly", None),
            ("In Cinemas", None),
            ("[In Cinemas] Now", None),
            ("IN CINEMAS", None),
            ("AL JAZEERA", None),
            ("Al-Jazeera", None),
            ("AL-JAZEERA", None),
            ("No. 1 Hits", None),
            // brands and competitions (a UK user's defaults would hide them)
            ("BT Sport", None),
            ("BT SPORT", None),
            ("FA Cup", None),
            ("CL Matches", None),
            ("CH 5", None),
            ("Football | PL", None),
            ("German Bundesliga", None),
            ("Indian Premier League", None),
            ("Movies: Sci-Fi", None),
            ("Re-Runs", None),
            ("UK's Best", None),
            ("Sports UK", None),
            ("HD", None),
            ("XXX", None),
            ("ADULT", None),
            ("VIP", None),
            ("PPV | Events", None),
            ("HD Movies", None),
            ("TOP 10 IT Movies", None),
            ("TV", None),
            ("NEW: Releases", None),
            ("Sports", None),
            ("", None),
            ("🏴 Pirates", None),
            ("GENERAL ᴴᴰ/", None),
            ("VIP ❖ Premier League+", None),
            ("SAT: Movies", None),
            ("UHD | Sports", None),
            ("1080p ★ Movies", None),
            ("CAR SHOWS", None),
        ];
        for (raw, want) in cases {
            assert_eq!(detect_tag(raw).as_ref().map(|t| t.0.as_str()), *want, "{raw:?}");
        }
    }

    #[test]
    fn summarize_counts_per_kind_and_orders_by_items() {
        let s = summarize([
            (ItemKind::Channel, "UK | News", 10),
            (ItemKind::Channel, "UK: Sports", 5),
            (ItemKind::Movie, "|UK| Films", 100),
            (ItemKind::Series, "AR | Series", 200),
            (ItemKind::Channel, "Sports", 7),
            (ItemKind::Channel, "FR | Info", 3),
            (ItemKind::Movie, "Kids", 4),
        ]);
        let tags: Vec<Option<&str>> = s.iter().map(|t| t.tag.as_ref().map(|t| t.0.as_str())).collect();
        assert_eq!(tags, [Some("lang:ar"), Some("GB"), Some("FR"), None], "items descending, untagged last");
        assert_eq!(
            s[1],
            TagSummary {
                tag: Some(CategoryTag::country("GB")),
                live_groups: 2,
                movie_groups: 1,
                series_groups: 0,
                channels: 15,
                movies: 100,
                series: 0
            }
        );
        assert_eq!((s[3].live_groups, s[3].movie_groups, s[3].items()), (1, 1, 11));
        let summary = CategorySummary { tags: s, needs_refresh: false };
        assert_eq!(summary.tag_count(), 3);
    }
}
