//! The provider-name tokenizer (DESIGN §13.1, §28.2), shared by smart names (channel, VOD and
//! category display names) and category tag detection, so both read a name the same way:
//! `"UK ★ BBC One 4K◉"`, `"|UK| BBC One"`, `"UK: BBC One"` and `"UK ❖ BBC One"` have the same
//! tag, the same name and the same quality word.
//!
//! A name is a list of [`Token`]s:
//! * **Word**: letters, digits and marks (any script) and the characters that belong to names
//!   (`+ & ' ’` anywhere, `! %` after a letter or digit: `E! News`, `Sky Sports+`, `AT&T`,
//!   `+1`, `100%`), with joiners between two word characters (`- – . / _`; `: ,` between
//!   digits): `Sci-Fi`, `Canela.TV`, `24/7`, `EX-YU`, `15:55`.
//! * **Sep**: a run of any other punctuation or symbol (Unicode P* and S*, private-use icon
//!   glyphs): `| : - ★ ✦ ❖ ◉ ● • ▎ ┃ ⓧ # » ⚽`… — a separator or a decoration.
//! * **Deco**: superscript / modifier / small-capital letters and digits (`ᴴᴰ`, `⁴ᵏ`, `ꜰʜᴅ`,
//!   `ᴿᴬᵂ`, `³⁸⁴⁰ᴾ`), split off the word they are glued to (`TF1ᴴᴰ`).
//! * **Flag**: a flag emoji (a regional-indicator pair, or 🏴 with its subdivision tag).
//! * **Open** / **Close**: one bracket, `( [ { ⁽ 【 「` / `) ] } ⁾ 】 」`.
//!
//! Whitespace (NBSP included) and control / format characters only end tokens; each token
//! records whether whitespace came before it, so kept text is put back as it was written.

use unicode_general_category::{GeneralCategory, get_general_category};

use crate::model::Quality;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Word,
    Sep,
    Deco,
    Flag,
    Open,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: Kind,
    pub text: &'a str,
    /// Whitespace came before it (never for the first token).
    pub spaced: bool,
}

impl Token<'_> {
    pub fn is(&self, kind: Kind) -> bool {
        self.kind == kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    /// Continues a word, never starts one (`!`, `%`).
    Tail,
    Join,
    Sep,
    Deco,
    Regional,
    BlackFlag,
    /// Subdivision-flag tag characters.
    FlagTag,
    /// ZWJ / ZWNJ / variation selectors: part of whatever token they follow.
    Continue,
    Open,
    Close,
}

const BLACK_FLAG: char = '\u{1F3F4}';

fn class(c: char) -> Class {
    match c {
        '\u{200C}' | '\u{200D}' | '\u{FE0E}' | '\u{FE0F}' => return Class::Continue,
        '\u{E0020}'..='\u{E007F}' => return Class::FlagTag,
        '+' | '&' | '\'' | '’' => return Class::Word,
        '!' | '%' => return Class::Tail,
        '-' | '–' | '.' | '/' | '_' | ':' | ',' => return Class::Join,
        '(' | '[' | '{' | '⁽' | '【' | '「' => return Class::Open,
        ')' | ']' | '}' | '⁾' | '】' | '」' => return Class::Close,
        BLACK_FLAG => return Class::BlackFlag,
        _ => {}
    }
    if c.is_whitespace() {
        return Class::Space;
    }
    if is_deco(c) {
        return Class::Deco;
    }
    if regional(c).is_some() {
        return Class::Regional;
    }
    match get_general_category(c) {
        GeneralCategory::PrivateUse | GeneralCategory::Unassigned => Class::Sep,
        g => match g.abbreviation().as_bytes()[0] {
            b'L' | b'M' | b'N' => Class::Word,
            b'Z' | b'C' => Class::Space,
            _ => Class::Sep,
        },
    }
}

/// Does the joiner at `i` join the word characters around it?
fn joins(chars: &[(usize, char, Class)], i: usize) -> bool {
    let (Some(prev), Some(next)) = (i.checked_sub(1).and_then(|p| chars.get(p)), chars.get(i + 1)) else { return false };
    let word = |c: &(usize, char, Class)| matches!(c.2, Class::Word | Class::Tail);
    match chars[i].1 {
        ':' | ',' => prev.1.is_ascii_digit() && next.1.is_ascii_digit(),
        _ => word(prev) && next.2 == Class::Word,
    }
}

/// Split a name into tokens (see the module docs).
pub fn tokens(s: &str) -> Vec<Token<'_>> {
    let chars: Vec<(usize, char, Class)> = s.char_indices().map(|(i, c)| (i, c, class(c))).collect();
    let class_at = |j: usize| chars.get(j).map(|c| c.2);
    let mut out = Vec::new();
    let mut spaced = false;
    let mut i = 0;
    while i < chars.len() {
        let (start, _, cl) = chars[i];
        let mut j = i + 1;
        let kind = match cl {
            Class::Space => {
                spaced = !out.is_empty();
                i += 1;
                continue;
            }
            Class::Word => {
                loop {
                    match class_at(j) {
                        Some(Class::Word | Class::Tail | Class::Continue) => j += 1,
                        Some(Class::Join) if joins(&chars, j) => j += 1,
                        _ => break,
                    }
                }
                Kind::Word
            }
            Class::Deco => {
                while matches!(class_at(j), Some(Class::Deco | Class::Continue)) {
                    j += 1;
                }
                Kind::Deco
            }
            Class::Regional => {
                if class_at(j) == Some(Class::Regional) {
                    j += 1;
                }
                while class_at(j) == Some(Class::Continue) {
                    j += 1;
                }
                Kind::Flag
            }
            Class::BlackFlag => {
                while matches!(class_at(j), Some(Class::FlagTag | Class::Continue)) {
                    j += 1;
                }
                Kind::Flag
            }
            Class::Open => Kind::Open,
            Class::Close => Kind::Close,
            Class::Tail | Class::Join | Class::Sep | Class::FlagTag | Class::Continue => {
                while matches!(class_at(j), Some(Class::Tail | Class::Join | Class::Sep | Class::FlagTag | Class::Continue)) {
                    j += 1;
                }
                Kind::Sep
            }
        };
        let end = chars.get(j).map_or(s.len(), |c| c.0);
        out.push(Token { kind, text: &s[start..end], spaced });
        spaced = false;
        i = j;
    }
    out
}

/// The `Close` token matching the `Open` at `open` (the first closer of the same pair).
pub fn closing(toks: &[Token<'_>], open: usize) -> Option<usize> {
    let want = match toks.get(open)?.text {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        "⁽" => "⁾",
        "【" => "】",
        "「" => "」",
        _ => return None,
    };
    toks[open + 1..].iter().position(|t| t.kind == Kind::Close && t.text == want).map(|p| open + 1 + p)
}

/// Regional-indicator symbol → its letter (`🇬` → `G`).
pub fn regional(c: char) -> Option<char> {
    let n = u32::from(c).checked_sub(0x1F1E6).filter(|n| *n < 26)?;
    char::from_u32(u32::from('A') + n)
}

/// The letters of a flag token: `"GB"` for 🇬🇧, `"gbeng"` for England's subdivision flag,
/// `""` for a lone indicator or a bare 🏴.
pub fn flag_code(flag: &str) -> String {
    let mut code = String::new();
    for c in flag.chars() {
        if let Some(l) = regional(c) {
            code.push(l);
        } else if ('\u{E0061}'..='\u{E007A}').contains(&c) {
            code.extend(char::from_u32(u32::from(c) - 0xE0000));
        }
    }
    if code.chars().count() == 1 { String::new() } else { code }
}

/// Superscript / modifier / small-capital letters and digits used as decoration (`ᴴᴰ`, `ꜰʜᴅ`,
/// `⁴ᵏ`, `ʰᵉᵛᶜ`) → their ASCII letter.
pub fn deco_to_ascii(c: char) -> Option<char> {
    Some(match c {
        'ᴬ' | 'ᵃ' | 'ᴀ' => 'A',
        'ᴮ' | 'ᵇ' | 'ʙ' => 'B',
        'ᶜ' | 'ᴄ' => 'C',
        'ᴰ' | 'ᵈ' | 'ᴅ' => 'D',
        'ᴱ' | 'ᵉ' | 'ᴇ' => 'E',
        // `ғ` (Cyrillic ghe with stroke) stands in for a small-capital F (`ғʜᴅ`)
        'ᶠ' | 'ꜰ' | 'ғ' => 'F',
        'ᴳ' | 'ᵍ' | 'ɢ' => 'G',
        'ᴴ' | 'ʰ' | 'ʜ' => 'H',
        'ᴵ' | 'ⁱ' | 'ᶦ' | 'ɪ' => 'I',
        'ᴶ' | 'ʲ' | 'ᴊ' => 'J',
        'ᴷ' | 'ᵏ' | 'ᴋ' => 'K',
        'ᴸ' | 'ˡ' | 'ᶫ' | 'ʟ' => 'L',
        'ᴹ' | 'ᵐ' | 'ᴍ' => 'M',
        'ᴺ' | 'ⁿ' | 'ᶰ' | 'ɴ' => 'N',
        'ᴼ' | 'ᵒ' | 'ᴏ' => 'O',
        'ᴾ' | 'ᵖ' | 'ᴘ' => 'P',
        'ᴿ' | 'ʳ' | 'ʀ' => 'R',
        'ˢ' | 'ꜱ' => 'S',
        'ᵀ' | 'ᵗ' | 'ᴛ' => 'T',
        'ᵁ' | 'ᵘ' | 'ᶸ' | 'ᴜ' => 'U',
        'ⱽ' | 'ᵛ' | 'ᴠ' => 'V',
        'ᵂ' | 'ʷ' | 'ᴡ' => 'W',
        'ˣ' => 'X',
        'ʸ' | 'ʏ' => 'Y',
        'ᶻ' | 'ᴢ' => 'Z',
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

/// Decoration: [`deco_to_ascii`]'s letters, and any other modifier / superscript letter
/// (`ᶿ`, `⁼`; Phonetic Extensions, Spacing Modifier Letters, Superscripts and Subscripts).
fn is_deco(c: char) -> bool {
    deco_to_ascii(c).is_some() || matches!(u32::from(c), 0x1D2C..=0x1D6A | 0x1D9B..=0x1DBF | 0x2070..=0x209C | 0x02B0..=0x02B8 | 0x02E0..=0x02E4)
}

/// A Deco token's text in ASCII (`ᴴᴰ` → `HD`).
pub fn deco_ascii(s: &str) -> String {
    s.chars().filter_map(deco_to_ascii).collect()
}

/// Small capitals only (`ꜱᴘᴏʀᴛꜱ`): stylised text when it is no known word, unlike superscripts,
/// which are always decoration.
pub fn is_small_caps(s: &str) -> bool {
    const SMALL_CAPS: &str = "ᴀʙᴄᴅᴇꜰғɢʜɪᴊᴋʟᴍɴᴏᴘʀꜱᴛᴜᴠᴡʏᴢ";
    s.chars().all(|c| SMALL_CAPS.contains(c))
}

/// Quality words → [`Quality`] (compared case-insensitively). 1440p / QHD count as FHD, like a
/// measured 2560×1440 picture (`Quality::of_video`); 8K as 4K (the best class).
const QUALITY_WORDS: &[(&str, Quality)] = &[
    ("sd", Quality::Sd),
    ("480p", Quality::Sd),
    ("480i", Quality::Sd),
    ("576p", Quality::Sd),
    ("576i", Quality::Sd),
    ("hd", Quality::Hd),
    ("720p", Quality::Hd),
    ("fhd", Quality::Fhd),
    ("1080p", Quality::Fhd),
    ("1080i", Quality::Fhd),
    ("1440p", Quality::Fhd),
    ("qhd", Quality::Fhd),
    ("uhd", Quality::Uhd),
    ("4k", Quality::Uhd),
    ("2160p", Quality::Uhd),
    ("3840p", Quality::Uhd),
    ("8k", Quality::Uhd),
    ("4320p", Quality::Uhd),
];

/// The quality class a word names (`"FHD"`, `"2160p"`, `"4K"`), if any.
pub fn quality_word(w: &str) -> Option<Quality> {
    QUALITY_WORDS.iter().find(|(k, _)| w.eq_ignore_ascii_case(k)).map(|(_, q)| *q)
}

/// Codec / picture-format words (`HEVC`, `H.265`, `HDR`, `50fps`, `⁵⁰ FPS`): never part of a
/// name.
pub fn is_format_word(w: &str) -> bool {
    const FORMAT: &[&str] = &["hevc", "h265", "h.265", "h264", "h.264", "x265", "x264", "avc", "hdr", "hdr10", "hdr10+", "dolby", "fps"];
    FORMAT.iter().any(|k| w.eq_ignore_ascii_case(k))
        || w.get(w.len().saturating_sub(3)..).is_some_and(|s| s.eq_ignore_ascii_case("fps")) && w[..w.len() - 3].bytes().all(|b| b.is_ascii_digit())
}

/// Access / service tags of a provider's line-up (`VIP`, `SAT`, `RAW`, `PPV`, `24/7`): a
/// leading tag, not a name (`"VIP: ESPN"`), though `"ESPN+ PPV 175"` keeps its `PPV`.
pub fn is_service_word(w: &str) -> bool {
    const SERVICE: &[&str] = &["vip", "sat", "raw", "ppv", "24/7", "24-7"];
    SERVICE.iter().any(|k| w.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(s: &str) -> String {
        tokens(s)
            .iter()
            .map(|t| {
                let k = match t.kind {
                    Kind::Word => "w",
                    Kind::Sep => "s",
                    Kind::Deco => "d",
                    Kind::Flag => "f",
                    Kind::Open => "o",
                    Kind::Close => "c",
                };
                format!("{}{k}:{}", if t.spaced { " " } else { "" }, t.text)
            })
            .collect()
    }

    /// Tricky provider spellings: glued decorations, joiners inside words vs. separators,
    /// characters that belong to names, any-script words with marks, flags, NBSP.
    #[test]
    fn tokenizes_provider_names() {
        let cases: &[(&str, &str)] = &[
            ("UK ★ ITV 4 4K ◉", "w:UK s:★ w:ITV w:4 w:4K s:◉"),
            ("UK: ITV 4 4K◉", "w:UKs:: w:ITV w:4 w:4Ks:◉"),
            ("|UK| BBC One", "s:|w:UKs:| w:BBC w:One"),
            ("★UK★BBC", "s:★w:UKs:★w:BBC"),
            ("TF1ᴴᴰ News", "w:TF1d:ᴴᴰ w:News"),
            ("SKY ⁽ᴴᴰ⁾", "w:SKY o:⁽d:ᴴᴰc:⁾"),
            ("E! News", "w:E! w:News"),
            ("!!! NEW !!!", "s:!!! w:NEW s:!!!"),
            ("AT&T: 5-MINUTE CRAFTS", "w:AT&Ts:: w:5-MINUTE w:CRAFTS"),
            ("Channel 4 +1", "w:Channel w:4 w:+1"),
            ("Canal +", "w:Canal w:+"),
            ("Mr. Bean 24/7 15:55", "w:Mrs:. w:Bean w:24/7 w:15:55"),
            ("UK.Sports UK-DOCS EX-YU", "w:UK.Sports w:UK-DOCS w:EX-YU"),
            ("MADISON, WI", "w:MADISONs:, w:WI"),
            ("(FHD@60FPS)", "o:(w:FHDs:@w:60FPSc:)"),
            ("beIN Sp⚽rts", "w:beIN w:Sps:⚽w:rts"),
            ("US\u{a0}★\u{a0}CNN", "w:US s:★ w:CNN"),
            ("🇬🇧 Sports", "f:🇬🇧 w:Sports"),
            ("नमस्ते टीवी", "w:नमस्ते w:टीवी"),
            ("ⓧ GENERAL ᴴᴰ/", "s:ⓧ w:GENERAL d:ᴴᴰs:/"),
            ("-5", "s:-w:5"),
        ];
        for (raw, want) in cases {
            assert_eq!(show(raw), *want, "{raw:?}");
        }
        assert_eq!(flag_code("🇬🇧"), "GB");
        assert_eq!(flag_code("🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}"), "gbeng");
        assert_eq!(deco_ascii("³⁸⁴⁰ᴾ"), "3840P");
        assert!(is_format_word("60FPS") && is_format_word("fps") && !is_format_word("HD60fps"));
        assert_eq!(show("SEATTLE ᶜᶦᵗʸ"), "w:SEATTLE d:ᶜᶦᵗʸ");
        assert_eq!(deco_ascii("ᶜᶦᵗʸ"), "CITY");
    }
}
