//! Folding for matching (B3). OWNER: F-AB.
//!
//! Folded text is lower-case ASCII letters and digits in tokens separated by single spaces
//! (non-ASCII letters `deunicode` can't spell stay as they are), so a whole-token check is a
//! look at the neighbouring bytes.

/// Fold `s` into `out` (cleared first): lower case, ASCII fast path, `deunicode` for bytes ≥
/// 0x80 (as `sources::identity::normalize_name`), "&" → "and", "vs" / "vs." / "versus" → "v",
/// punctuation → single spaces.
pub fn fold(s: &str, out: &mut String) {
    out.clear();
    fold_append(s, out);
}

/// [`fold`] appending to `out` (a space first when `out` isn't empty): several fields folded
/// into one buffer stay separate tokens.
pub fn fold_append(s: &str, out: &mut String) {
    let mut f = Folder { out, token_start: None };
    if !f.out.is_empty() && !f.out.ends_with(' ') {
        f.out.push(' ');
    }
    if s.is_ascii() {
        for b in s.bytes() {
            f.ascii(b);
        }
    } else {
        for c in s.chars() {
            if c.is_ascii() {
                f.ascii(c as u8);
            } else if let Some(spelt) = deunicode::deunicode_char(c) {
                for b in spelt.bytes() {
                    f.ascii(b);
                }
            } else if c.is_alphanumeric() {
                f.open();
                f.out.extend(c.to_lowercase());
            } else {
                f.close();
            }
        }
    }
    f.close();
    if f.out.ends_with(' ') {
        f.out.pop();
    }
}

/// `s` folded into a new string.
pub fn folded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold(s, &mut out);
    out
}

struct Folder<'a> {
    out: &'a mut String,
    /// Byte offset of the open token.
    token_start: Option<usize>,
}

impl Folder<'_> {
    fn ascii(&mut self, b: u8) {
        if b.is_ascii_alphanumeric() {
            self.open();
            self.out.push(b.to_ascii_lowercase() as char);
        } else if b == b'&' {
            self.close();
            self.out.push_str("and");
            self.token_start = Some(self.out.len() - 3);
            self.close();
        } else {
            self.close();
        }
    }

    fn open(&mut self) {
        if self.token_start.is_none() {
            self.token_start = Some(self.out.len());
        }
    }

    /// End the open token (rewriting "vs" / "versus") and leave one separating space.
    fn close(&mut self) {
        if let Some(start) = self.token_start.take() {
            let tok = &self.out[start..];
            if tok == "vs" || tok == "versus" {
                self.out.truncate(start);
                self.out.push('v');
            }
            self.out.push(' ');
        } else if !self.out.is_empty() && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
    }
}

/// `needle` (folded, non-empty) occurs in `hay` (folded) as whole tokens.
pub fn has_token(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let h = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let s = from + i;
        let e = s + needle.len();
        if (s == 0 || h[s - 1] == b' ') && (e == h.len() || h[e] == b' ') {
            return true;
        }
        from = s + 1;
        while from < hay.len() && !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_like_the_guide_writes() {
        let f = |s: &str| {
            let mut o = String::new();
            fold(s, &mut o);
            o
        };
        assert_eq!(f("Arsenal vs. Chelsea"), "arsenal v chelsea");
        assert_eq!(f("Arsenal versus Chelsea"), "arsenal v chelsea");
        assert_eq!(f("Brighton & Hove Albion"), "brighton and hove albion");
        assert_eq!(f("Atlético Madrid"), "atletico madrid");
        assert_eq!(f("ＡＲＳＥＮＡＬ"), "arsenal");
        assert_eq!(f("Live: Arsenal v Chelsea"), "live arsenal v chelsea");
        assert_eq!(f("  Arsenal's  day! "), "arsenal s day");
        assert_eq!(f("Vsevolod"), "vsevolod", "only whole tokens are rewritten");
        let mut two = String::from("taskmaster");
        fold_append("S20 E4", &mut two);
        assert_eq!(two, "taskmaster s20 e4");
    }

    #[test]
    fn whole_tokens() {
        assert!(has_token("arsenal v chelsea", "arsenal"));
        assert!(has_token("arsenal v chelsea", "v chelsea"));
        assert!(!has_token("arsenale venezia", "arsenal"));
        assert!(!has_token("the arsenal", "arsenal x"));
        assert!(has_token("x arsenale arsenal", "arsenal"), "a later whole occurrence counts");
    }
}
