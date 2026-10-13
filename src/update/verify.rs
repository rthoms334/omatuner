//! What the app and the update helper check the same way (DESIGN §41.3): the release key, a
//! package's signature, its file name and pacman's version order. The helper
//! (`src/bin/tuner-update.rs`) includes this file with `#[path]`, so it uses std and
//! `minisign-verify` only — nothing else of the crate, and the root program stays small.
//!
//! * **The release key** is `packaging/update-key.pub`, minisign's public key file as
//!   `minisign -G` wrote it, compiled in ([`RELEASE_KEY`]). A file that still carries
//!   [`PLACEHOLDER_MARK`], or one that doesn't parse, is no key: the app never installs and the
//!   helper refuses ([`release_key`]).
//! * **A signature verifies** only in minisign's pre-hashed mode (`ED`: Ed25519 over the
//!   file's BLAKE2b-512, minisign's default since 0.8), made by the release key's id, and with
//!   the global signature over its trusted comment ([`verify`]). Legacy (`Ed`) signatures are
//!   refused: release.yml never makes them.
//! * [`vercmp`] is libalpm's `alpm_pkg_vercmp`: `[epoch:]version[-release]`, `rpmvercmp`
//!   segments (numbers by value, a number beats letters, `1.0rc1` < `1.0`), the release compared
//!   only when both have one.
//! * [`package_file`] reads `omatuner-<pkgver>-<pkgrel>-<arch>.pkg.tar.zst`, the name
//!   release.yml gives the package (no epoch).

use std::cmp::Ordering;

/// The Arch package Tuner ships as.
pub const PACKAGE: &str = "omatuner";
/// The release key (minisign's `.pub` file), from the repository at build time.
pub const RELEASE_KEY: &str = include_str!("../../packaging/update-key.pub");
/// A key file holding this is the placeholder of a build without a release key (a fork that
/// hasn't made its own): nothing installs.
pub const PLACEHOLDER_MARK: &str = "REPLACE-WITH-THE-RELEASE-KEY";
/// What such a key file looks like (README "Release signing" says how to make the real one).
pub const PLACEHOLDER: &str =
    "untrusted comment: REPLACE-WITH-THE-RELEASE-KEY (minisign -G -p packaging/update-key.pub)\nREPLACE-WITH-THE-RELEASE-KEY\n";
/// The largest package either side reads (a release is about 10 MiB).
pub const MAX_PACKAGE: u64 = 256 << 20;
/// The largest signature file (minisign's are about 300 bytes).
pub const MAX_SIGNATURE: u64 = 4096;
/// The `.minisig` file of a package sits next to it under this suffix.
pub const SIGNATURE_SUFFIX: &str = ".minisig";

/// The release key in `key_file` (minisign's `.pub` format); `None` for the placeholder or a
/// file that doesn't parse — a build that must not install anything.
pub fn release_key(key_file: &str) -> Option<minisign_verify::PublicKey> {
    if key_file.contains(PLACEHOLDER_MARK) {
        return None;
    }
    minisign_verify::PublicKey::decode(key_file.trim()).ok()
}

/// Check `signature` (the text of a `.minisig` file) over `data` with `key`. `Err` says why,
/// in words for the log and Settings.
pub fn verify(key: &minisign_verify::PublicKey, data: &[u8], signature: &str) -> Result<(), String> {
    use minisign_verify::Error as E;
    let sig = minisign_verify::Signature::decode(signature.trim()).map_err(|e| match e {
        E::UnsupportedAlgorithm => "the signature uses an algorithm Tuner doesn't accept".to_owned(),
        _ => "the signature file isn't a minisign signature".to_owned(),
    })?;
    key.verify(data, &sig, false).map_err(|e| {
        match e {
            E::UnexpectedKeyId => "it was signed with another key than Tuner's release key",
            E::UnexpectedAlgorithm | E::UnsupportedLegacyMode => "it was signed in minisign's legacy mode",
            _ => "the signature doesn't match the file",
        }
        .to_owned()
    })
}

/// The parts of a package file name, `omatuner-0.2.6-1-x86_64.pkg.tar.zst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageFile<'a> {
    pub pkgver: &'a str,
    pub pkgrel: &'a str,
    pub arch: &'a str,
}

impl PackageFile<'_> {
    /// pacman's full version, `0.2.6-1`.
    pub fn version(&self) -> String {
        format!("{}-{}", self.pkgver, self.pkgrel)
    }
}

/// `omatuner-<pkgver>-<pkgrel>-<arch>.pkg.tar.zst`: pkgver letters, digits, `.`, `_` and `+`
/// starting with a digit (no epoch), pkgrel digits and dots, arch letters, digits and `_`.
pub fn package_file(name: &str) -> Option<PackageFile<'_>> {
    let rest = name.strip_prefix(PACKAGE)?.strip_prefix('-')?.strip_suffix(".pkg.tar.zst")?;
    let mut parts = rest.rsplitn(3, '-');
    let (arch, pkgrel, pkgver) = (parts.next()?, parts.next()?, parts.next()?);
    let ok = pkgver.starts_with(|c: char| c.is_ascii_digit())
        && pkgver.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+'))
        && !pkgrel.is_empty()
        && pkgrel.starts_with(|c: char| c.is_ascii_digit())
        && pkgrel.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && !arch.is_empty()
        && arch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    ok.then_some(PackageFile { pkgver, pkgrel, arch })
}

/// libalpm's `alpm_pkg_vercmp(a, b)`: how `a` orders against `b` (pacman's `vercmp`).
pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (e1, v1, r1) = parse_evr(a);
    let (e2, v2, r2) = parse_evr(b);
    rpmvercmp(e1, e2).then_with(|| rpmvercmp(v1, v2)).then_with(|| match (r1, r2) {
        (Some(x), Some(y)) => rpmvercmp(x, y),
        _ => Ordering::Equal,
    })
}

/// `[epoch:]version[-release]` (libalpm's `parseEVR`): the epoch is the leading digits before a
/// `:` (empty or missing = `0`), the release follows the last `-`.
fn parse_evr(evr: &str) -> (&str, &str, Option<&str>) {
    let digits = evr.bytes().take_while(u8::is_ascii_digit).count();
    let (epoch, rest) = match evr[digits..].strip_prefix(':') {
        Some(rest) => (if digits == 0 { "0" } else { &evr[..digits] }, rest),
        None => ("0", evr),
    };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

/// libalpm's `rpmvercmp`: alternating runs of digits and letters compared one by one (numbers
/// by value, letters bytewise; a number beats letters), separators (anything else) only by
/// their length; when one side runs out, a trailing letter run is older (`1.0rc` < `1.0`) and
/// anything else newer.
fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // `one`/`two` walk the strings, `ptr1`/`ptr2` mark where the last segment ended
    let (mut one, mut two, mut ptr1, mut ptr2) = (0, 0, 0, 0);
    while one < a.len() && two < b.len() {
        while one < a.len() && !a[one].is_ascii_alphanumeric() {
            one += 1;
        }
        while two < b.len() && !b[two].is_ascii_alphanumeric() {
            two += 1;
        }
        if one >= a.len() || two >= b.len() {
            break;
        }
        // separators of different lengths decide
        if one - ptr1 != two - ptr2 {
            return (one - ptr1).cmp(&(two - ptr2));
        }
        ptr1 = one;
        ptr2 = two;
        let numeric = a[ptr1].is_ascii_digit();
        let class: fn(&u8) -> bool = if numeric { u8::is_ascii_digit } else { u8::is_ascii_alphabetic };
        while ptr1 < a.len() && class(&a[ptr1]) {
            ptr1 += 1;
        }
        while ptr2 < b.len() && class(&b[ptr2]) {
            ptr2 += 1;
        }
        // a number against letters: the number is newer
        if two == ptr2 {
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let (mut s1, mut s2) = (&a[one..ptr1], &b[two..ptr2]);
        if numeric {
            let zeros = |s: &[u8]| s.iter().take_while(|&&c| c == b'0').count();
            s1 = &s1[zeros(s1)..];
            s2 = &s2[zeros(s2)..];
            match s1.len().cmp(&s2.len()) {
                Ordering::Equal => {}
                longer => return longer,
            }
        }
        match s1.cmp(s2) {
            Ordering::Equal => {}
            other => return other,
        }
        one = ptr1;
        two = ptr2;
    }
    let (c1, c2) = (a.get(one).copied(), b.get(two).copied());
    match (c1, c2) {
        (None, None) => Ordering::Equal,
        // what's left over: letters lose to the end, the end loses to anything but letters
        (None, Some(c)) if !c.is_ascii_alphabetic() => Ordering::Less,
        (Some(c), _) if c.is_ascii_alphabetic() => Ordering::Less,
        _ => Ordering::Greater,
    }
}
