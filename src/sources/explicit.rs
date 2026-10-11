//! Explicit (adult) content detection (DESIGN §20.3). Two signals, both conservative:
//!
//! * **The provider's flag**: Xtream panels send `is_adult` on categories and on live, movie
//!   and series streams (`"1"`, `1`, `true`; `"0"`, `0`, missing = not adult) — read by
//!   [`crate::sources::xtream::types::lenient_flag`] and stored as `grp.explicit = 2` /
//!   `channel|movie|series.explicit = 1`.
//! * **The category name** ([`is_explicit_name`]): whole words of the provider's raw category
//!   name, read with the shared tokenizer (`name_tokens`), so `|XXX|`, `★ 18+ ★`, `ᴬᴰᵁᴸᵀ`,
//!   `XXX-Movies` and `Adults (VOD)` read like `XXX`, `18+` and `Adults`. Words: `XXX`, `18+`
//!   / `+18`, `Porn` / `Porno`, `Erotic` / `Erotica` / `Erotik` / `Érotique` / `Erotico`, and
//!   `Adult` / `Adults` (and `Adulto(s)`, `Adulte(s)`) — except in `Young Adult(s)`, `New
//!   Adult`, `Adult Swim`, `Adult Animation` / `Animated` / `Cartoon(s)` / `Education` /
//!   `Learning` / `Contemporary`. Item names are never classified (only categories).
//!
//! The user's word wins over both: a category marked "Not explicit" stays visible, one marked
//! "Mark as explicit" is hidden ([`Explicit`](crate::model::Explicit)).

use crate::sources::name_tokens::{self, Kind};

/// Words that make a category explicit wherever they appear.
const ALWAYS: &[&str] = &[
    "XXX",
    "18+",
    "+18",
    "PORN",
    "PORNO",
    "PORNOS",
    "PORNOGRAPHY",
    "EROTIC",
    "EROTICA",
    "EROTIK",
    "EROTICO",
    "EROTIQUE",
    "ÉROTIQUE",
    "ЭРОТИКА",
    "ПОРНО",
];

/// "Adult" in the languages providers use; explicit unless an innocent phrase surrounds it.
/// (`Für Erwachsene`, `Для взрослых`.)
const ADULT: &[&str] = &["ADULT", "ADULTS", "ADULTO", "ADULTOS", "ADULTE", "ADULTES", "ADULTI", "ERWACHSENE", "ВЗРОСЛЫЕ", "ВЗРОСЛЫХ"];
/// `<word> Adult` that is not adult content (book / TV genres).
const ADULT_AFTER: &[&str] = &["YOUNG", "NEW"];
/// `Adult <word>` that is not adult content.
const ADULT_BEFORE: &[&str] = &["SWIM", "ANIMATION", "ANIMATED", "CARTOON", "CARTOONS", "EDUCATION", "LEARNING", "CONTEMPORARY"];

/// A styled letter, digit or plus read as plain ASCII: circled, parenthesised-free squared,
/// fullwidth and the mathematical alphabets (`ⓍⓍⓍ`, `🅇🅇🅇`, `ＸＸＸ`, `𝐗𝐗𝐗`, `𝟭𝟴+`).
fn fold_styled(c: char) -> char {
    let u = c as u32;
    let letter = |base: u32, upper: bool| char::from_u32(u - base + if upper { 'A' as u32 } else { 'a' as u32 }).unwrap_or(c);
    match u {
        0x24B6..=0x24CF => letter(0x24B6, true),
        0x24D0..=0x24E9 => letter(0x24D0, false),
        0x1F130..=0x1F149 => letter(0x1F130, true),
        0x1F150..=0x1F169 => letter(0x1F150, true),
        0x1F170..=0x1F189 => letter(0x1F170, true),
        0xFF21..=0xFF3A => letter(0xFF21, true),
        0xFF41..=0xFF5A => letter(0xFF41, false),
        0xFF10..=0xFF19 => char::from_u32(u - 0xFF10 + '0' as u32).unwrap_or(c),
        0xFF0B => '+',
        // bold, italic, script, fraktur, double-struck, sans-serif and monospace: 52 letters each
        0x1D400..=0x1D6A3 => {
            let i = (u - 0x1D400) % 52;
            char::from_u32(if i < 26 { 'A' as u32 + i } else { 'a' as u32 + i - 26 }).unwrap_or(c)
        }
        0x1D7CE..=0x1D7FF => char::from_u32('0' as u32 + (u - 0x1D7CE) % 10).unwrap_or(c),
        _ => c,
    }
}

/// Upper-cased whole words of a name: styled letters read as ASCII (`𝐗𝐗𝐗`), word tokens split
/// at their joiners (`XXX-Movies`, `Adult/18+`, `18+XXX`), decorations read as ASCII
/// (`ᴬᴰᵁᴸᵀ`), a word of Cyrillic `Х`s read as `XXX` (`ХХХ`).
fn words(name: &str) -> Vec<String> {
    let name: String = name.chars().map(fold_styled).collect();
    let mut out = Vec::new();
    let mut push = |w: &str| {
        let up = w.to_uppercase();
        out.push(if up.chars().all(|c| c == 'Х' || c == 'X') { up.replace('Х', "X") } else { up });
    };
    for t in name_tokens::tokens(&name) {
        let text = match t.kind {
            Kind::Word => t.text.to_owned(),
            Kind::Deco => name_tokens::deco_ascii(t.text),
            _ => continue,
        };
        for w in text.split(|c: char| !(c.is_alphanumeric() || c == '+')) {
            if w.is_empty() {
                continue;
            }
            let parts: Vec<&str> = w.split('+').collect();
            if parts.len() == 1 || parts.iter().all(|p| p.is_empty()) || matches!(w, "18+" | "+18") {
                push(w);
                continue;
            }
            // a plus joining words (`18+XXX`, `XXX+18`, `ADULTS+`): each part, an `18` beside
            // the plus kept as `18+` unless a number is on its other side (`18+1`)
            let numeric = |p: Option<&&str>| p.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
            for (k, p) in parts.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
                let (before, after) = (k.checked_sub(1).and_then(|j| parts.get(j)), parts.get(k + 1));
                match *p {
                    "18" if after.is_some() && !numeric(after) => push("18+"),
                    "18" if before.is_some() && !numeric(before) => push("+18"),
                    p => push(p),
                }
            }
        }
    }
    out
}

/// The provider's category name reads as adult content (see the module docs). Pass the raw
/// provider name (`grp.raw_name`), not the smart-cleaned one.
pub fn is_explicit_name(name: &str) -> bool {
    let w = words(name);
    w.iter().enumerate().any(|(i, word)| {
        if ALWAYS.contains(&word.as_str()) {
            return true;
        }
        // `18 +` written apart
        if word == "18" && w.get(i + 1).is_some_and(|n| n == "+") {
            return true;
        }
        ADULT.contains(&word.as_str())
            && !i.checked_sub(1).and_then(|p| w.get(p)).is_some_and(|p| ADULT_AFTER.contains(&p.as_str()))
            && !w.get(i + 1).is_some_and(|n| ADULT_BEFORE.contains(&n.as_str()))
    })
}

/// `grp.explicit` codes (what the import found; the user's override lives in
/// `group_override.explicit`).
pub const GROUP_NOT_EXPLICIT: i64 = 0;
/// The category name reads as adult content.
pub const GROUP_BY_NAME: i64 = 1;
/// The provider flags the category (`is_adult`).
pub const GROUP_BY_PROVIDER: i64 = 2;

/// `grp.explicit` for a category: the provider's flag beats the name.
pub fn group_code(provider_adult: bool, raw_name: &str) -> i64 {
    if provider_adult {
        GROUP_BY_PROVIDER
    } else if is_explicit_name(raw_name) {
        GROUP_BY_NAME
    } else {
        GROUP_NOT_EXPLICIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adult_category_names_are_explicit() {
        for name in [
            "XXX",
            "xxx",
            "|XXX| Movies",
            "XXX-Movies",
            "VOD | XXX",
            "★ 18+ ★",
            "18+",
            "(18+) Hot",
            "+18 Adultos",
            "18 + Only",
            "Adult",
            "ADULTS",
            "For Adults",
            "Adults Only",
            "ᴬᴰᵁᴸᵀ",
            "ᴬᴰᵁᴸᵀ Channels",
            "Adult/18+",
            "UK: Adult",
            "PORN",
            "Porno HD",
            "Erotic Movies",
            "Erotica",
            "Erotik",
            "Érotique",
            "Canales Adultos",
            "ХХХ",
            "18+XXX",
            "XXX+18",
            "ADULTS+",
            "Film per Adulti",
            "Für Erwachsene",
            "ЭРОТИКА",
            "Для взрослых",
            "ⓍⓍⓍ",
            "𝐗𝐗𝐗 Movies",
            "ＸＸＸ",
            "🅇🅇🅇",
            "𝟭𝟴+",
        ] {
            assert!(is_explicit_name(name), "{name:?} should be explicit");
        }
    }

    #[test]
    fn innocent_category_names_are_not_explicit() {
        for name in [
            "Adult Swim",
            "US | Adult Swim HD",
            "Young Adult",
            "Young Adults",
            "New Adult",
            "Adult Animation",
            "Adult Cartoons",
            "Adult Education",
            "Sports",
            "UK | News",
            "Essex",
            "Middlesex",
            "Sussex TV",
            "XXXL",
            "Exotic Places",
            "Top 18",
            "U18 Football",
            "Under 18s",
            "Adultery Dramas",
            "Pornichet Live",
            "Kids",
            "",
            "Movies 2018",
            "Disney+",
            "Apple TV+",
            "U18+Juniors",
            "Top 18+1",
            "Хит FM",
            "𝐒𝐩𝐨𝐫𝐭𝐬",
        ] {
            assert!(!is_explicit_name(name), "{name:?} should not be explicit");
        }
    }

    #[test]
    fn the_provider_flag_beats_the_name() {
        assert_eq!(group_code(true, "Movies"), GROUP_BY_PROVIDER);
        assert_eq!(group_code(true, "XXX"), GROUP_BY_PROVIDER);
        assert_eq!(group_code(false, "XXX"), GROUP_BY_NAME);
        assert_eq!(group_code(false, "Adult Swim"), GROUP_NOT_EXPLICIT);
    }
}
