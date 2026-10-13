//! Tuner's own updates (DESIGN §41): stable releases on GitHub, checked at start and every 6 h;
//! this computer's package downloaded, its minisign signature checked against the release key
//! compiled in, installed through the root helper (`tuner-update`, started by pkexec), and Tuner
//! restarted once nothing is going on.
//!
//! * [`verify`] — shared with the helper: the release key, signatures, package file names,
//!   pacman's version order.
//! * [`github`] — the Releases API (conditional requests, rate limits) and which release is
//!   this computer's ([`github::pick`]).
//! * [`service`] — [`Updater`], in `Services`: the check loop, download and verify, install,
//!   the passwordless switch and the idle restart, with a snapshot ([`UpdateStatus`]) the UI
//!   reads each frame.
//! * [`helper`] — the helper through pkexec: argv, timeout, what its exit code means.
//! * [`restart`] — the idle gate, the re-exec of `/usr/bin/tuner` and the marker the new
//!   process reads.
//!
//! Only a Tuner that runs `/usr/bin/tuner` of pacman's `omatuner`, with the helper, its polkit
//! policy, pkexec and a real release key installs ([`Mode::Managed`]); any other (cargo
//! install, the tarball, a checkout) checks and tells, and Settings says why ([`Why`]).

pub mod github;
pub mod helper;
pub mod restart;
pub mod service;
pub mod verify;

#[cfg(test)]
pub(crate) mod test_signer;

use std::path::PathBuf;

pub use service::Updater;

/// This Tuner's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where the package puts Tuner (the only Tuner that installs updates).
pub const EXE: &str = "/usr/bin/tuner";
/// Where the package puts the helper; the polkit policy names this exact path.
pub const HELPER: &str = "/usr/lib/tuner/tuner-update";
/// The helper's polkit policy (`packaging/polkit/…`).
pub const POLICY: &str = "/usr/share/polkit-1/actions/io.github.rthoms334.omatuner.update.policy";
/// Stable and pre-releases, newest first (one page is plenty: releases come weeks apart).
pub const RELEASES_API: &str = "https://api.github.com/repos/rthoms334/omatuner/releases?per_page=20";
/// Where people read about a release (Settings, the notification of a Tuner that can't
/// install).
pub const RELEASES_PAGE: &str = "https://github.com/rthoms334/omatuner/releases";

/// A release asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// A stable release, and its signed package for this computer when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// "0.2.6" (the tag without its `v`).
    pub version: String,
    /// The release's page.
    pub page: String,
    pub package: Option<Package>,
}

/// A release's package for this computer's architecture, with its signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// pacman's version, "0.2.6-1".
    pub version: String,
    pub file: Asset,
    /// The file's `.minisig`.
    pub signature: Asset,
}

/// Whether this Tuner installs updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// pacman's `omatuner` runs, the helper is there: installs. `installed` is pacman's
    /// version ("0.2.5-1"), which may be newer than what runs (installed meanwhile).
    Managed { installed: String },
    /// Checks and tells only.
    TellOnly(Why),
}

/// Why a Tuner only tells (Settings ▸ About & logs ▸ Updates says it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Not `/usr/bin/tuner`, or pacman doesn't own it (cargo install, the tarball, a checkout).
    NotPackaged(PathBuf),
    /// The package's helper or its polkit policy is missing.
    NoHelper,
    /// polkit's pkexec isn't installed.
    NoPkexec,
    /// This build's release key is the placeholder.
    NoKey,
}

impl Why {
    /// The sentence Settings shows.
    pub fn text(&self) -> String {
        match self {
            Why::NotPackaged(exe) => format!(
                "This Tuner runs from {} and wasn't installed with pacman's omatuner package, so it tells you about new versions but doesn't install them. Update it the way you installed it.",
                exe.display()
            ),
            Why::NoHelper => format!(
                "The update helper ({HELPER}) or its polkit policy is missing: reinstall the omatuner package. Until then Tuner only tells you about new versions."
            ),
            Why::NoPkexec => {
                "Installing updates needs polkit (pkexec), which isn't installed. Until then Tuner only tells you about new versions.".into()
            }
            Why::NoKey => {
                "This build of Tuner has no release key, so it can't check a download and installs none. It tells you about new versions.".into()
            }
        }
    }
}

/// Where an update stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Phase {
    /// Not checked yet.
    #[default]
    Idle,
    UpToDate,
    /// A newer release this Tuner won't install by itself: it only tells, or automatic
    /// installs are off ("Install now" downloads it).
    Available(Release),
    Downloading(Release),
    /// Downloaded and verified, waiting for the password prompt's moment or "Install now".
    /// `declined`: the prompt was dismissed (or failed); no new prompt by itself this session.
    Ready {
        release: Release,
        path: PathBuf,
        declined: bool,
    },
    Installing(Release),
    /// A newer Tuner is installed (by Tuner, or by pacman meanwhile): restarts when idle.
    Installed {
        version: String,
    },
}

/// The passwordless switch while the helper changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchOp {
    /// Waiting for the password (turning it on, or off).
    Working(bool),
    /// It didn't change: why (shown under the switch until the next try).
    Failed(String),
}

/// What the UI shows of the updates (a snapshot, read each frame).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateStatus {
    pub phase: Phase,
    /// `None` until the first check found out.
    pub mode: Option<Mode>,
    pub checking: bool,
    /// When the last check finished (unix seconds).
    pub last_check: Option<crate::model::Timestamp>,
    /// The last thing that went wrong (redacted, one line), until the next success.
    pub problem: Option<String>,
    pub switch: Option<SwitchOp>,
    /// Off the desktop (tests, `TUNER_NO_DESKTOP`, a profile with another home): no checks.
    pub inert: bool,
}

/// "0.2.6" for a stable tag `v0.2.6` (three numbers, nothing after: `v0.3.0-rc1` and
/// `v0.3.0rc1` are pre-releases whatever GitHub says).
pub fn stable_version(tag: &str) -> Option<&str> {
    tag.strip_prefix('v').filter(|v| verify::is_stable(v))
}

/// pkgver of pacman's full version: "0.2.6" of "0.2.6-1" (and of "1:0.2.6-1").
pub fn pkgver(full: &str) -> &str {
    let v = full.split_once(':').map_or(full, |(_, v)| v);
    v.rsplit_once('-').map_or(v, |(v, _)| v)
}

/// `release` is newer than this Tuner: than pacman's installed package when pacman manages it
/// (by the package's full version when the release has one for this computer), else than the
/// version that runs.
pub fn is_newer(release: &Release, mode: &Mode, running: &str) -> bool {
    use std::cmp::Ordering::Greater;
    match (mode, &release.package) {
        (Mode::Managed { installed }, Some(p)) => verify::vercmp(&p.version, installed) == Greater,
        (Mode::Managed { installed }, None) => verify::vercmp(&release.version, pkgver(installed)) == Greater,
        (Mode::TellOnly(_), _) => verify::vercmp(&release.version, running) == Greater,
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering::{self, Equal, Greater, Less};

    use super::verify::{self, vercmp};
    use super::*;

    /// pacman's own table (`test/util/vercmptest.sh`), both ways round.
    #[test]
    fn vercmp_matches_pacman() {
        let cases: &[(&str, &str, Ordering)] = &[
            ("1.5.0", "1.5.0", Equal),
            ("1.5.1", "1.5.0", Greater),
            ("1.5.1", "1.5", Greater),
            ("1.5.0-1", "1.5.0-1", Equal),
            ("1.5.0-1", "1.5.0-2", Less),
            ("1.5.0-1", "1.5.1-1", Less),
            ("1.5.0-2", "1.5.1-1", Less),
            ("1.5-1", "1.5.1-1", Less),
            ("1.5-2", "1.5.1-1", Less),
            ("1.5-2", "1.5.1-2", Less),
            ("1.5", "1.5-1", Equal),
            ("1.5-1", "1.5", Equal),
            ("1.1-1", "1.1", Equal),
            ("1.0-1", "1.1", Less),
            ("1.1-1", "1.0", Greater),
            ("1.5b-1", "1.5-1", Less),
            ("1.5b", "1.5", Less),
            ("1.5b-1", "1.5", Less),
            ("1.5b", "1.5.1", Less),
            ("1.0a", "1.0alpha", Less),
            ("1.0alpha", "1.0b", Less),
            ("1.0b", "1.0beta", Less),
            ("1.0beta", "1.0rc", Less),
            ("1.0rc", "1.0", Less),
            ("1.5.a", "1.5", Greater),
            ("1.5.b", "1.5.a", Greater),
            ("1.5.1", "1.5.b", Greater),
            ("1.5.b-1", "1.5.b", Equal),
            ("1.5-1", "1.5.b", Less),
            ("2.0", "2_0", Equal),
            ("2.0_a", "2_0.a", Equal),
            ("2.0a", "2.0.a", Less),
            ("2___a", "2_a", Greater),
            ("0:1.0", "0:1.0", Equal),
            ("0:1.0", "0:1.1", Less),
            ("1:1.0", "0:1.0", Greater),
            ("1:1.0", "0:1.1", Greater),
            ("1:1.0", "2:1.1", Less),
            ("1:1.0", "0:1.0-1", Greater),
            ("1:1.0-1", "0:1.1-1", Greater),
            ("0:1.0", "1.0", Equal),
            ("0:1.0", "1.1", Less),
            ("0:1.1", "1.0", Greater),
            ("1:1.0", "1.0", Greater),
            ("1:1.0", "1.1", Greater),
            ("1:1.1", "1.1", Greater),
            // Tuner's own: numbers by value, a pre-release before its release
            ("0.2.10-1", "0.2.9-1", Greater),
            ("0.3.0rc4-1", "0.3.0-1", Less),
            ("0.2.6-1", "0.2.5-2", Greater),
            ("0.2.5-2", "0.2.5-1", Greater),
        ];
        for &(a, b, want) in cases {
            assert_eq!(vercmp(a, b), want, "vercmp {a} {b}");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp {b} {a}");
        }
    }

    #[test]
    fn only_plain_version_tags_are_stable() {
        assert_eq!(stable_version("v0.2.6"), Some("0.2.6"));
        assert_eq!(stable_version("v10.20.300"), Some("10.20.300"));
        for tag in ["v0.3.0-rc1", "v0.3.0rc1", "0.2.6", "v0.2", "v0.2.6.1", "v0..6", "v0.2.6 ", "vx.y.z", "v0.2.6+build"] {
            assert_eq!(stable_version(tag), None, "{tag}");
        }
        assert_eq!(pkgver("0.2.6-1"), "0.2.6");
        assert_eq!(pkgver("1:0.2.6-1"), "0.2.6");
        assert_eq!(pkgver("0.2.6"), "0.2.6");
    }

    /// The committed key file is the maintainer's real key (id 7FC95FA61A0134E8, as minisign
    /// prints it); a placeholder or a broken file is no key at all.
    #[test]
    fn the_release_key_is_real_and_a_placeholder_is_no_key() {
        use base64::Engine;
        assert!(verify::release_key(verify::RELEASE_KEY).is_some(), "packaging/update-key.pub doesn't parse");
        let line = verify::RELEASE_KEY.lines().nth(1).unwrap();
        let bin = base64::engine::general_purpose::STANDARD.decode(line.trim()).unwrap();
        assert_eq!(&bin[..2], b"Ed");
        assert_eq!(format!("{:016X}", u64::from_le_bytes(bin[2..10].try_into().unwrap())), "7FC95FA61A0134E8");
        assert!(verify::release_key(verify::PLACEHOLDER).is_none());
        assert!(verify::release_key("untrusted comment: x\nRWQgarbage\n").is_none());
        assert!(verify::release_key("").is_none());
        // a real-looking key with the mark anywhere is still the placeholder
        let marked = format!("untrusted comment: {}\n{line}\n", verify::PLACEHOLDER_MARK);
        assert!(verify::release_key(&marked).is_none());
    }

    /// minisign's pre-hashed signature by the release key verifies; any changed byte, another
    /// key or legacy mode doesn't. The tests' BLAKE2b is RFC 7693's.
    #[test]
    fn signatures_verify_only_for_the_exact_bytes_and_key() {
        use super::test_signer::{TestKey, blake2b_512};
        let abc: String = blake2b_512(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            abc,
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        let key = TestKey::new(9);
        let public = verify::release_key(&key.public_file()).unwrap();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(verify::verify(&public, &data, &key.sign(&data)), Ok(()));
        let mut changed = data.clone();
        changed[123_456] ^= 0x40;
        assert!(verify::verify(&public, &changed, &key.sign(&data)).unwrap_err().contains("doesn't match"));
        assert!(verify::verify(&public, &data, &TestKey::new(8).sign(&data)).unwrap_err().contains("another key"));
        assert!(verify::verify(&public, &data, &key.sign_legacy(&data)).unwrap_err().contains("legacy"));
        assert!(verify::verify(&public, &data, "nonsense").is_err());
        // the trusted comment is signed too
        let forged = key.sign(&data).replace("timestamp:1791000000", "timestamp:1791000001");
        assert!(verify::verify(&public, &data, &forged).is_err());
    }

    #[test]
    fn package_file_names_are_read_strictly() {
        let f = verify::package_file("omatuner-0.2.6-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!((f.pkgver, f.pkgrel, f.arch, f.version()), ("0.2.6", "1", "x86_64", "0.2.6-1".to_owned()));
        assert_eq!(verify::package_file("omatuner-0.3.0rc4-2-aarch64.pkg.tar.zst").unwrap().version(), "0.3.0rc4-2");
        for bad in [
            "omatuner-0.2.6-1-x86_64.pkg.tar.xz",
            "omatuner-0.2.6-x86_64.pkg.tar.zst",
            "omatuner-debug-0.2.6-1-x86_64.pkg.tar.zst",
            "omatuner-1:0.2.6-1-x86_64.pkg.tar.zst",
            "omatuner-0.2.6-1-x86/64.pkg.tar.zst",
            "omatuner-v0.2.6-1-x86_64.pkg.tar.zst",
            "other-0.2.6-1-x86_64.pkg.tar.zst",
            "omatuner-0.2.6-1-.pkg.tar.zst",
        ] {
            assert!(verify::package_file(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn newer_means_newer_than_installed_or_running() {
        let asset = Asset { name: String::new(), url: String::new(), size: 0 };
        let package = Package { version: "0.2.6-1".into(), file: asset.clone(), signature: asset };
        let r = Release { version: "0.2.6".into(), page: String::new(), package: Some(package) };
        let managed = |v: &str| Mode::Managed { installed: v.into() };
        assert!(is_newer(&r, &managed("0.2.5-1"), "0.2.5"));
        assert!(!is_newer(&r, &managed("0.2.6-1"), "0.2.5"), "already installed, waiting for the restart");
        assert!(is_newer(&r, &managed("0.2.6rc1-1"), "0.2.6"), "a pre-release package moves on to its release");
        let tell = Mode::TellOnly(Why::NoKey);
        assert!(is_newer(&r, &tell, "0.2.5"));
        assert!(!is_newer(&r, &tell, "0.2.6") && !is_newer(&r, &tell, "0.3.0"));
        // no package for this computer: by the release's version
        let bare = Release { package: None, ..r };
        assert!(is_newer(&bare, &managed("0.2.5-3"), "0.2.5") && !is_newer(&bare, &managed("0.2.6-1"), "0.2.5"));
    }
}
