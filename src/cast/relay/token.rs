//! Session tokens (DESIGN §40 §9.2): 128 random bits (aws-lc-rs `SystemRandom`), base32 lower
//! case, compared in constant time, one per load generation (a zap, seek or reconnect revokes the
//! old one; a playlist view's token maps to the same directory), registered with
//! `redact::register_secret`, revoked at session end and 60 s after the last fetch once stopped.
//! The loopback proxy has its own. OWNER: CAST-4.
//!
//! A LAN token URL is a bearer credential to a live stream: whoever has it (and the TV's
//! address) can watch. So a token is a [`Secret`], never `Debug`-printed, masked in every log line
//! through the redaction registry, and looked up without an early exit ([`Grants::find`]).

use std::time::{Duration, Instant};

use crate::redact::{Secret, register_secret};

/// Random bits per token.
const TOKEN_BYTES: usize = 16;
/// RFC 4648 base32, lower case (no padding): 26 characters for 128 bits, safe in a URL path and
/// unambiguous when read aloud from a log.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// The length of every token.
pub const TOKEN_LEN: usize = (TOKEN_BYTES * 8).div_ceil(5);
/// A finished generation's token lives this long after the TV's last fetch (§9.2).
pub const STOPPED_TTL: Duration = Duration::from_secs(60);

/// One bearer token (LAN: one per load generation; loopback: one per feed).
#[derive(Clone, PartialEq, Eq)]
pub struct Token(Secret<String>);

impl Token {
    /// 128 bits from the system CSPRNG, registered with the redaction registry before it can
    /// reach any log line or URL.
    pub fn mint() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        // The kernel CSPRNG doesn't fail on Linux once booted; an error here means no randomness
        // at all, and a predictable token is worse than a crash.
        aws_lc_rs::rand::SecureRandom::fill(&aws_lc_rs::rand::SystemRandom::new(), &mut bytes).expect("system random");
        let s = base32_lower(&bytes);
        register_secret(&s);
        Self(Secret::new(s))
    }

    /// The token text (for building the URL handed to the TV or ffmpeg). Grep for `expose(`
    /// when auditing.
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// `candidate` is this token: constant time over the token's bytes (a wrong length is
    /// rejected at once; every token has the same length).
    pub fn matches(&self, candidate: &str) -> bool {
        aws_lc_rs::constant_time::verify_slices_are_equal(self.0.expose().as_bytes(), candidate.as_bytes()).is_ok()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// RFC 4648 base32 with the lower-case alphabet and no padding.
pub fn base32_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// A token and what it unlocks.
#[derive(Debug)]
pub struct Grant<T> {
    pub token: Token,
    pub value: T,
    /// The TV's last fetch with this token (the §9.2 60 s rule).
    pub last_fetch: Option<Instant>,
}

/// The live tokens of one server (a handful at most: the current generation, and nothing else
/// once a newer one is minted).
#[derive(Debug)]
pub struct Grants<T> {
    entries: Vec<Grant<T>>,
}

impl<T> Default for Grants<T> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<T> Grants<T> {
    pub fn insert(&mut self, token: Token, value: T) {
        self.entries.push(Grant { token, value, last_fetch: None });
    }

    /// The grant for `candidate`. Every entry is compared (no early exit), so the time taken
    /// doesn't tell how many tokens exist or which one nearly matched.
    pub fn find_mut(&mut self, candidate: &str) -> Option<&mut Grant<T>> {
        let mut hit = None;
        for (i, g) in self.entries.iter().enumerate() {
            if g.token.matches(candidate) && hit.is_none() {
                hit = Some(i);
            }
        }
        hit.map(|i| &mut self.entries[i])
    }

    /// Revoke every grant `drop` selects.
    pub fn revoke(&mut self, mut drop: impl FnMut(&Grant<T>) -> bool) {
        self.entries.retain(|g| !drop(g));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        // RFC 4648 §10, lower-cased, padding dropped
        for (input, want) in
            [("", ""), ("f", "my"), ("fo", "mzxq"), ("foo", "mzxw6"), ("foob", "mzxw6yq"), ("fooba", "mzxw6ytb"), ("foobar", "mzxw6ytboi")]
        {
            assert_eq!(base32_lower(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn tokens_are_26_url_safe_chars_registered_and_compared_whole() {
        let t = Token::mint();
        assert_eq!(t.expose().len(), TOKEN_LEN);
        assert_eq!(TOKEN_LEN, 26);
        assert!(t.expose().bytes().all(|b| ALPHABET.contains(&b)));
        assert_ne!(t, Token::mint());
        assert_eq!(crate::redact::redact(&format!("/c/{}/index.m3u8", t.expose())), "/c/***/index.m3u8");
        assert_eq!(format!("{t:?}"), "Token(***)");
        assert!(t.matches(t.expose()));
        assert!(!t.matches(&t.expose()[..25]) && !t.matches(""));

        let mut g = Grants::default();
        g.insert(t.clone(), 1);
        g.insert(Token::mint(), 2);
        assert_eq!(g.find_mut(t.expose()).map(|g| g.value), Some(1));
        g.revoke(|g| g.value == 1);
        assert!(g.find_mut(t.expose()).is_none());
        assert_eq!(g.len(), 1);
    }
}
