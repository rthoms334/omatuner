//! Casting to TVs (DESIGN §40): AirPlay (Apple TVs, third-party AirPlay TVs, Macs), Google Cast
//! and DLNA, each a [`receiver::Backend`] on the one relay. The skeleton (CAST-0) froze this
//! file, the value types in `model::cast`, the player contract in `player::remote`, and the APIs
//! of `airplay`, `relay` and `firewall`; the work items fill the bodies.
//!
//! ```text
//!  UI / agent ──CastHandle (devices, pairing, firewall, test on TV; polled per frame)──┐
//!  controller A ──CastConnector::open──▶ CastService (tokio) ◀─────────────────────────┘
//!     ▲ RemoteEvent (crossbeam)   │ RemoteCmd (tokio mpsc)
//!     └──────── session task (one per cast): receiver::Backend + relay::RelaySession
//!                 ├─ AirPlay client: :7000 control connection, event channel, feedback
//!                 ├─ (or) Google Cast client: TLS :8009 / DLNA client: SOAP polls
//!                 └─ relay: loopback upstream proxy → ffprobe / ffmpeg → LAN server :47800
//! ```
//! OWNERS (DESIGN §40 §15.2):
//! * CAST-0: this file, `receiver.rs`, `testing.rs` (all frozen: no work item edits them); the
//!   signatures in `airplay/mod.rs`, `relay/mod.rs`, `firewall/mod.rs` (frozen).
//! * CAST-1: `airplay/{tlv8,srp,hap,pair,bplist,features}.rs`.
//! * CAST-2: `airplay/{rtsp,session,queue,events,position,trace}.rs` and the bodies in
//!   `airplay/mod.rs`.
//! * CAST-4: `relay/*` and the bodies in `relay/mod.rs`.
//! * CAST-6: `service.rs` (implements [`CastApi`] and `player::remote::CastConnector`),
//!   `session.rs`, `discovery.rs`, `registry.rs`, `inhibit.rs`, `store::cast`.
//! * CAST-7: `firewall/{ufw,allow}.rs` and the bodies in `firewall/mod.rs`.
//! * Revision 5 (cast design Appendix E): `googlecast/`, `dlna/`, the Mac's transient pairing in
//!   `airplay/`, and their seams (the `Backend` variants, `LoadReq::fmp4`, `RelayConfig::stream`,
//!   `LoadSpec::model`, the relay's `stream.ts`).
//!
//! Rules: the TV never gets a provider URL (Tuner always relays); casting never moves Tuner's
//! window; device text is data (sanitized, never in error copy); no address leaves this module
//! except to the protocol code that connects to it.

pub mod airplay;
pub mod discovery;
pub mod dlna;
pub mod firewall;
pub mod googlecast;
pub mod inhibit;
pub mod receiver;
pub mod registry;
pub mod relay;
pub mod service;
pub mod session;
#[doc(hidden)]
pub mod testing;

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::model::Timestamp;
pub use crate::model::cast::{CastErrorKind, CastHints, CastLevel, CastPhase, CastView, DeviceId, PositionSource, ReceiverKind};
use crate::player::remote::{CastConnector, RemoteOutput};
use crate::player::{PlayerError, PlayerErrorKind};
use crate::redact::Secret;

/// Casting shows in the UI (the picker, `C`, the OSD control, Settings ▸ Cast, the first-run
/// card, the agent's tools). On since the combined gate (DESIGN §40 §15.0 step 4).
pub const ENABLED: bool = true;

/// Casting is on: built in ([`ENABLED`]) and not switched off in Settings ▸ Cast
/// (`cast.enabled`, which stops the service). Off, nothing offers a TV — the OSD control, `C`,
/// the player menu — rather than a picker with no service behind it; a cast that still runs
/// keeps its way off.
pub fn on(cfg: &crate::config::Config) -> bool {
    ENABLED && cfg.cast.enabled
}

/// The wire protocol a device speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    AirPlay,
    GoogleCast,
    Dlna,
}

impl Protocol {
    /// The store's `cast_device.protocol` and [`DeviceId::derive`]'s prefix.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::AirPlay => "airplay",
            Protocol::GoogleCast => "googlecast",
            Protocol::Dlna => "dlna",
        }
    }
}

/// The AirPlay pairing variant that worked (`cast_device.pair_kind`; DESIGN §40 §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairVariant {
    /// `X-Apple-HKP: 3` with `X-Apple-Client-Name` (tried first).
    Hkp3,
    /// `X-Apple-HKP: 5` with the ScreenCapture ACL (the fallback; tvOS 27 senders use it).
    Hkp5,
}

impl PairVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            PairVariant::Hkp3 => "hkp3",
            PairVariant::Hkp5 => "hkp5",
        }
    }

    /// The variant **Show a new code** moves to (the ladder: HKP 3 → HKP 5, then stays).
    pub fn next(self) -> Self {
        PairVariant::Hkp5
    }
}

/// Whether a device needs pairing before it casts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// The receiver doesn't ask for it (no PIN bit).
    NotNeeded,
    /// "Pair first": one-time PIN pairing.
    Required,
    Paired(PairVariant),
}

/// Whether a listed device can be chosen (the picker greys out the others).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usability {
    Usable,
    /// A Mac whose AirPlay Receiver allows only its own user (`act=2`, "Current User"): no other
    /// sender can cast to it. "Only this Mac's user", with the setting hint.
    CurrentUser,
    /// A Mac whose AirPlay Receiver asks for a password (status flag 0x80): Tuner has no
    /// password entry yet. "Password required", with the setting hint.
    Password,
    /// `acl=1` (Allow Access: Home only): "Same Home only", with the setting hint.
    HomeOnly,
}

impl Usability {
    /// Why a greyed device can't be chosen (the picker's and Settings' second line); `None` when
    /// it can.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Usability::Usable => None,
            Usability::CurrentUser => Some("Only its own user"),
            Usability::Password => Some("Password required"),
            Usability::HomeOnly => Some("Same Home only"),
        }
    }

    /// The setting that would let a greyed device through (the picker's tooltip).
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Usability::Usable => None,
            Usability::CurrentUser => {
                Some("On the Mac: System Settings ▸ General ▸ AirDrop & Handoff ▸ Allow AirPlay for ▸ Anyone on the Same Network.")
            }
            Usability::Password => Some("On the Mac: System Settings ▸ General ▸ AirDrop & Handoff ▸ turn off Require Password."),
            Usability::HomeOnly => Some("On the Apple TV: Settings ▸ AirPlay and HomeKit ▸ Allow Access ▸ Anyone on the Same Network."),
        }
    }
}

/// One receiver as the UI and the agent see it: no address (DESIGN §40 §6). Device strings are
/// sanitized ([`sanitize_name`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub kind: ReceiverKind,
    pub protocol: Protocol,
    /// "AppleTV14,1".
    pub model: Option<String>,
    /// "Hisense": a third-party TV's maker from its TXT record or `/info` (sanitized; this
    /// run's discovery only, never stored).
    pub manufacturer: Option<String>,
    /// The receiver's OS version for logs, `diagnose` and Settings only: "26.6 (23L773)".
    pub os_version: Option<String>,
    pub pairing: Pairing,
    pub usability: Usability,
    /// Seen on the network by the latest discovery (else remembered from the store).
    pub available: bool,
    /// Last cast to it (the picker lists the most recent first).
    pub last_used: Option<Timestamp>,
    /// Where it was last found. Private to the crate: never shown, logged at INFO or sent to an
    /// agent.
    pub(crate) addr: Option<SocketAddr>,
    /// Its TXT identifiers (a third-party TV's `channelID` and `ClockPorts` key, §7.7), from this
    /// run's discovery. Private to the crate, like the address.
    pub(crate) ids: airplay::features::ReceiverIds,
    /// A DLNA renderer's control URLs, from this run's discovery (crate only, like the address).
    pub(crate) dlna: Option<dlna::Control>,
}

impl Device {
    /// A device by identity; the rest defaults to usable, not paired (pairing required),
    /// available, never used, no model or version. `name` is sanitized.
    pub fn new(id: DeviceId, name: &str, kind: ReceiverKind, protocol: Protocol) -> Self {
        Self {
            id,
            name: sanitize_name(name, "TV"),
            kind,
            protocol,
            model: None,
            manufacturer: None,
            os_version: None,
            pairing: Pairing::Required,
            usability: Usability::Usable,
            available: true,
            last_used: None,
            addr: None,
            ids: airplay::features::ReceiverIds::default(),
            dlna: None,
        }
    }

    /// A device at `addr` (discovery, the store, tests that point a client at a fake receiver).
    #[doc(hidden)]
    pub fn at(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Where to connect (crate only).
    pub(crate) fn addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// The picker can start a cast to it (perhaps after pairing).
    pub fn selectable(&self) -> bool {
        self.usability == Usability::Usable
    }

    /// What the TV is, for the picker's and Settings' second line (DESIGN §40 §3.1): a
    /// third-party TV by maker and model ("Hisense 85E75LE": "AirPlay TV" alone read as a
    /// generic entry next to the Apple TV), an Apple TV by the name of its model identifier
    /// when Tuner knows it ("Apple TV 4K (3rd generation)"), else the kind. Both strings are the
    /// device's own, sanitized at discovery ([`sanitize_name`]) and shown as plain text.
    pub fn make(&self) -> String {
        match self.kind {
            ReceiverKind::AppleTv => self.model.as_deref().and_then(apple_tv_name).unwrap_or("Apple TV").to_owned(),
            ReceiverKind::AirPlayTv => match (self.manufacturer.as_deref(), self.model.as_deref()) {
                // a model that already names its maker isn't prefixed twice
                (Some(m), Some(model)) if model.to_lowercase().starts_with(&m.to_lowercase()) => model.to_owned(),
                (Some(m), Some(model)) => format!("{m} {model}"),
                (Some(m), None) => format!("{m} AirPlay TV"),
                (None, Some(model)) => format!("AirPlay TV {model}"),
                (None, None) => "AirPlay TV".to_owned(),
            },
            ReceiverKind::Mac => self.model.as_deref().and_then(mac_name).unwrap_or("Mac").to_owned(),
            ReceiverKind::Chromecast => match self.model.as_deref() {
                // a Chromecast says so; a TV with Cast built in is named by its model
                Some(m) if m.to_lowercase().contains("chromecast") || m.starts_with("Google") => m.to_owned(),
                Some(m) => format!("{m} (Google Cast)"),
                None => "Google Cast".to_owned(),
            },
            ReceiverKind::Dlna => match (self.manufacturer.as_deref(), self.model.as_deref()) {
                (Some(m), Some(model)) if model.to_lowercase().starts_with(&m.to_lowercase()) => format!("{model} (DLNA)"),
                (Some(m), Some(model)) => format!("{m} {model} (DLNA)"),
                (Some(m), None) => format!("{m} (DLNA)"),
                (None, Some(model)) => format!("{model} (DLNA)"),
                (None, None) => "DLNA TV".to_owned(),
            },
        }
    }
}

/// A Mac's family from its model identifier ("MacBookPro18,3" → "MacBook Pro"); `None` for the
/// newer "Mac14,7" style, which names no family (it is then just "Mac").
fn mac_name(model: &str) -> Option<&'static str> {
    let family = model.split(|c: char| c.is_ascii_digit()).next()?;
    Some(match family {
        "MacBookPro" => "MacBook Pro",
        "MacBookAir" => "MacBook Air",
        "MacBook" => "MacBook",
        "iMac" => "iMac",
        "iMacPro" => "iMac Pro",
        "Macmini" => "Mac mini",
        "MacPro" => "Mac Pro",
        _ => return None,
    })
}

/// The Apple TV models by identifier (Apple's own names); `None` for one Tuner doesn't know
/// (it is then just "Apple TV", never a guess).
fn apple_tv_name(model: &str) -> Option<&'static str> {
    Some(match model {
        "AppleTV2,1" => "Apple TV (2nd generation)",
        "AppleTV3,1" | "AppleTV3,2" => "Apple TV (3rd generation)",
        "AppleTV5,3" => "Apple TV HD",
        "AppleTV6,2" => "Apple TV 4K",
        "AppleTV11,1" => "Apple TV 4K (2nd generation)",
        "AppleTV14,1" => "Apple TV 4K (3rd generation)",
        _ => return None,
    })
}

/// A 4-digit PIN as typed in the pairing overlay. Used once, then dropped: never logged, stored
/// or `Debug`-printed, and deliberately not a registered secret (it would mask every 4-digit
/// number in the logs). The digits are zeroed on drop.
pub struct Pin([u8; 4]);

impl Pin {
    /// Exactly four ASCII digits.
    pub fn new(digits: &str) -> Option<Self> {
        let b = digits.as_bytes();
        (b.len() == 4 && b.iter().all(u8::is_ascii_digit)).then(|| Self([b[0], b[1], b[2], b[3]]))
    }

    /// The SRP password (the digit string). Grep for `expose(` when auditing.
    pub fn expose(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(****)")
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

/// This install's long-term AirPlay identity (`cast_identity`): one per install.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The pairing identifier (a UUID string).
    pub pairing_id: String,
    /// Ed25519 seed of the long-term key.
    pub seed: Secret<[u8; 32]>,
    /// Random, locally administered MAC: SETUP `deviceID`, fallback `channelID`.
    pub sender_mac: [u8; 6],
}

impl Identity {
    /// A throwaway identity for a session that stores no pairing (a Mac's transient pairing,
    /// before any TV was ever paired): random, locally administered MAC; never written.
    pub fn ephemeral() -> Self {
        let mut seed = [0u8; 32];
        let mut id = [0u8; 16];
        let mut mac = [0u8; 6];
        for buf in [&mut seed[..], &mut id[..], &mut mac[..]] {
            aws_lc_rs::rand::fill(buf).expect("the system random number generator");
        }
        mac[0] = (mac[0] | 0x02) & 0xfe;
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        let pairing_id = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        Self { pairing_id, seed: Secret::new(seed), sender_mac: mac }
    }
}

/// What pairing with one TV stored (`cast_device.peer_id`, `peer_ltpk`, `pair_kind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCredentials {
    /// The TV's pairing identifier (pair-verify checks it).
    pub peer_id: String,
    /// The TV's long-term Ed25519 public key.
    pub peer_ltpk: [u8; 32],
    pub variant: PairVariant,
}

/// Everything a session needs to verify with a TV.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub identity: Identity,
    /// The stored pairing (pair-verify); `None`: none is needed — a Mac pairs transiently each
    /// session (DESIGN §40 Appendix E), and Google Cast and DLNA have no pairing (`identity` is
    /// then unused).
    pub peer: Option<PeerCredentials>,
}

/// Why pairing stopped (DESIGN §40 §3.2: each has its own copy in the PIN overlay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// Error TLV 2 at M4: "That code didn't match. The TV is showing a new one."
    WrongPin,
    /// Error TLV 3: the TV wants a break (`BackOff` seconds, when it said).
    Backoff { secs: Option<u32> },
    /// HTTP 470 on both variants, another error TLV, or (pair-verify) an empty 200:
    /// `home_only` when "Everyone in your Home" is the likely cause.
    Refused { home_only: bool },
    /// The pairing connection closed while waiting for the code: "The code expired."
    Expired,
    /// No answer: "Living Room isn't answering. Is it on?"
    Unreachable,
    /// 403 at pin-start (a Mac).
    NotSupported,
    /// The TV's proof or signature didn't verify, or its reply was malformed.
    Failed,
}

impl PairError {
    /// The cast error this becomes when it stops a cast.
    pub fn kind(self) -> CastErrorKind {
        match self {
            PairError::Unreachable | PairError::Expired => CastErrorKind::Unreachable,
            PairError::NotSupported => CastErrorKind::Unsupported,
            PairError::Refused { .. } => CastErrorKind::PairingRefused,
            PairError::WrongPin | PairError::Backoff { .. } | PairError::Failed => CastErrorKind::PairingRequired,
        }
    }
}

/// A pairing in progress (the PIN overlay holds one while the user types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PairJob(pub u64);

/// Where a [`PairJob`] is (polled per frame by the PIN overlay).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// Asking the TV to show a code.
    Starting,
    /// The TV shows a code; type it in.
    EnterCode {
        variant: PairVariant,
    },
    /// The code is being checked (M1–M6).
    Checking,
    Paired,
    Failed(PairError),
    /// No such job (cancelled, or the service restarted).
    Gone,
}

/// The firewall work Settings and the firewall card poll ([`CastApi::firewall_job`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FirewallJob {
    #[default]
    Idle,
    /// polkit's dialog is (about to be) up: "Waiting for your password…" with Cancel.
    Waiting {
        verb: firewall::Verb,
    },
    Done {
        verb: firewall::Verb,
        outcome: firewall::AllowOutcome,
    },
}

/// Test on TV (DESIGN §40 §8.5): a 15 s Tuner test card through the real relay and pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestState {
    #[default]
    Idle,
    Running,
    Passed,
    Failed(CastErrorKind),
}

/// A TV the first-run card offers to set up (a usable, unpaired AirPlay video receiver: an Apple
/// TV or a third-party AirPlay TV).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOffer {
    pub device: DeviceId,
    pub name: String,
    pub kind: ReceiverKind,
}

/// The first-run discovery pass finished ([`CastApi::take_pass_outcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassOutcome {
    /// `None`: nothing to offer (the app records `cast.checked_version`).
    pub offer: Option<SetupOffer>,
}

/// What casting has going on now ([`CastApi::activity`]): Tuner's updates never restart, nor
/// install by themselves, in the middle of it (DESIGN §41.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CastActivity {
    /// A session is starting, casting, or still letting go of the TV.
    pub session: bool,
    /// Test on TV runs.
    pub test: bool,
    /// A pairing is open: from asking the TV for a code until the PIN overlay closes.
    pub pairing: bool,
    /// The firewall's Allow / Remove waits on polkit's dialog.
    pub firewall: bool,
}

impl CastActivity {
    /// Anything at all: the restart waits.
    pub fn any(self) -> bool {
        self.session || self.test || self.pairing || self.firewall
    }
}

/// The agent's `diagnose` cast section (DESIGN §40 §12): no addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CastDiagnose {
    /// The device last cast to (or being cast to).
    pub device: Option<DeviceId>,
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub source_version: Option<String>,
    pub pairing: Option<PairVariant>,
    /// Always `"ptp-declared"` in Phase 1.
    pub timing: &'static str,
    pub firewall: Option<firewall::FirewallState>,
    pub last_error: Option<CastErrorKind>,
}

/// What the UI, the agent and the player ask of the cast service. Every method returns at once
/// (state is polled, work runs on the runtime). The defaults are the inert answers ("no TVs,
/// nothing running"): [`NullCast`] is the service while casting is off, off the desktop, or
/// before `Services::start_integrations`. CAST-6's service implements every method.
pub trait CastApi: Send + Sync {
    // ---- devices (Settings ▸ Cast, the picker, the agent's `cast_devices`)
    /// Known devices: remembered ones and those the latest discovery found, merged, most
    /// recently used first. Never starts a scan.
    fn devices(&self) -> Vec<Device> {
        Vec::new()
    }
    /// Keep discovery running: call it every frame while the picker or Settings ▸ Cast is open;
    /// the browse stops shortly after the last call (on demand only, DESIGN §40 §6).
    fn browse(&self) {}
    /// A browse is running (the picker's "Searching…").
    fn searching(&self) -> bool {
        false
    }
    /// "Add by address…": probe `GET /info` on port 7000 of `ip`; the result arrives through
    /// [`CastApi::take_add_outcome`].
    fn add_by_address(&self, _ip: Ipv4Addr) {}
    fn take_add_outcome(&self) -> Option<Result<DeviceId, CastErrorKind>> {
        None
    }
    /// Forget a device (its pairing and hints too).
    fn forget(&self, _device: DeviceId) {}

    // ---- pairing (the PIN overlay; DESIGN §40 §3.2, §7.2)
    /// Ask `device` to show a code (HKP 3 first). The connection stays open while the user types.
    fn pair_start(&self, _device: DeviceId) -> PairJob {
        PairJob(0)
    }
    fn pair_state(&self, _job: PairJob) -> PairState {
        PairState::Gone
    }
    /// The typed code (used once, then dropped).
    fn pair_submit(&self, _job: PairJob, _pin: Pin) {}
    /// **Show a new code**: ask again with the next variant.
    fn pair_new_code(&self, _job: PairJob) {}
    fn pair_cancel(&self, _job: PairJob) {}

    // ---- firewall (Settings ▸ Cast ▸ Network, the firewall card; DESIGN §40 §9.4)
    /// The detection toward "the TV" (the one asked about or cast to last, else the default,
    /// else any on the network; cached ≤ 5 s per address; `None` until one toward it finished).
    fn firewall(&self) -> Option<firewall::Detection> {
        None
    }
    /// A fresh detection made toward `device`'s own address, the only one that may say the
    /// firewall blocks that TV (the agent's gate, the wizard). `None` while it runs (it starts
    /// one), or when the TV has no known address. `device` becomes what [`CastApi::firewall`],
    /// Check again and Allow are about.
    fn firewall_for(&self, _device: DeviceId) -> Option<firewall::Detection> {
        None
    }
    /// Detect again now ("Check again").
    fn firewall_check(&self) {}
    /// **Allow on firewall** through pkexec (prepending when detection found a shadowing rule).
    /// After `cast.media_port` changed, it also removes Tuner's tagged rule for the old port on
    /// this network (a move: one remove and one add under the same polkit keep).
    fn firewall_allow(&self) {}
    /// **Remove rule**: every rule tagged `omatuner cast`.
    fn firewall_remove(&self) {}
    /// Cancel a pending Allow / Remove (kills pkexec, which closes its dialog).
    fn firewall_cancel(&self) {}
    fn firewall_job(&self) -> FirewallJob {
        FirewallJob::Idle
    }

    // ---- setup (the first-run card and wizard; DESIGN §40 §3.5)
    /// The one 5 s first-run browse.
    fn start_pass(&self) {}
    fn take_pass_outcome(&self) -> Option<PassOutcome> {
        None
    }
    /// Test on TV: the test card through the real relay (no provider, no controller).
    fn test_on_tv(&self, _device: DeviceId) {}
    fn test_state(&self) -> TestState {
        TestState::Idle
    }

    // ---- the session now casting (the player menu, the casting card; DESIGN §40 §3.3, §8.3)
    /// **Convert this channel**: remember L3 for the item now casting on its TV
    /// (`cast_hint.forced`) and restart its pipeline at L3 (a replace on the same session, the
    /// same load generation). Nothing when nothing casts.
    fn convert_current(&self) {}
    /// **Send as is** (shown instead of Convert this channel when `CastView::as_is`): forget
    /// the item's remembered conversion on its TV (`cast_hint.forced` and a remembered L2
    /// failure) and load it again without converting, where the TV is (§8.3). Nothing when
    /// nothing casts.
    fn send_as_is(&self) {}
    /// Where the positions of the item casting now come from (§7.3: the TV's `playbackInfo`
    /// answers, or the estimate once one went unanswered): the info panel's Position line.
    /// `None` while nothing casts, before the first request, and for live (never asked).
    fn position_source(&self) -> Option<PositionSource> {
        None
    }

    // ---- Tuner's updates (the restart and install gates; DESIGN §41.6)
    /// What casting has going on (sessions, Test on TV, pairing, the firewall's dialog).
    fn activity(&self) -> CastActivity {
        CastActivity::default()
    }

    // ---- the agent (`status.casting.error`, `diagnose`)
    /// The last cast failure (kept after casting ended, e.g. `StoppedOnTv`).
    fn last_error(&self) -> Option<CastErrorKind> {
        None
    }
    fn diagnose(&self) -> CastDiagnose {
        CastDiagnose { timing: "ptp-declared", ..CastDiagnose::default() }
    }

    // ---- the player (picture A's controller, through `AppShared`)
    /// Start a session to `device` (non-blocking; it connects on the runtime and reports
    /// `Phase` / `Lost`).
    fn open(&self, _device: DeviceId) -> Result<RemoteOutput, PlayerError> {
        Err(PlayerError { kind: PlayerErrorKind::Cast(CastErrorKind::Unreachable), message: "Casting isn't available.".into() })
    }
}

/// The inert service: no devices, nothing to do.
#[derive(Debug, Default)]
pub struct NullCast;

impl CastApi for NullCast {}

/// The cloneable facade the UI and the agent hold (`AppShared::cast`). Derefs to [`CastApi`].
#[derive(Clone)]
pub struct CastHandle(Arc<dyn CastApi>);

impl CastHandle {
    pub fn new(api: Arc<dyn CastApi>) -> Self {
        Self(api)
    }
}

impl Default for CastHandle {
    fn default() -> Self {
        Self(Arc::new(NullCast))
    }
}

impl fmt::Debug for CastHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CastHandle")
    }
}

impl std::ops::Deref for CastHandle {
    type Target = dyn CastApi;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl CastConnector for CastHandle {
    fn open(&self, device: DeviceId) -> Result<RemoteOutput, PlayerError> {
        self.0.open(device)
    }
}

/// Longest device string kept (names, models, versions).
pub const MAX_NAME_CHARS: usize = 64;

/// A device-supplied string made safe to show (DESIGN §40 §6 "Untrusted strings"): control and
/// bidi-override characters dropped, whitespace runs collapsed, trimmed, capped at
/// [`MAX_NAME_CHARS`]; empty, or anything that looks like a URL (`://`), becomes `fallback`
/// (`"AirPlay TV"`).
pub fn sanitize_name(raw: &str, fallback: &str) -> String {
    if raw.contains("://") {
        return fallback.to_owned();
    }
    let bidi = |c: char| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}');
    let mut out = String::new();
    let mut space = false;
    for c in raw.chars().filter(|c| !c.is_control() && !bidi(*c)) {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if out.chars().count() >= MAX_NAME_CHARS {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    if out.is_empty() { fallback.to_owned() } else { out }
}

/// The OS version Settings, `diagnose` and the session's log line show: "26.6 (23L773)".
/// The TXT record carries the version (`osvers`) and the Apple TV's `/info` only the build
/// (`osBuildVersion`, no `osVersion`), so a build alone is "(23L773)" and
/// [`merge_os_version`] joins the halves.
pub fn os_version_text(version: Option<String>, build: Option<String>) -> Option<String> {
    match (version, build) {
        (Some(v), Some(b)) => Some(format!("{v} ({b})")),
        (Some(v), None) => Some(v),
        (None, Some(b)) => Some(format!("({b})")),
        (None, None) => None,
    }
}

/// `new` ([`os_version_text`]) over what was known: each half that `new` lacks is kept, except
/// that a new version drops the old build (the TV updated; its build comes with the next
/// `/info`).
pub fn merge_os_version(old: Option<String>, new: Option<String>) -> Option<String> {
    fn split(s: Option<String>) -> (Option<String>, Option<String>) {
        let Some(s) = s else { return (None, None) };
        match s.strip_suffix(')').and_then(|s| s.rsplit_once('(')) {
            Some((v, b)) => (Some(v.trim().to_owned()).filter(|v| !v.is_empty()), Some(b.to_owned())),
            None => (Some(s), None),
        }
    }
    let ((ov, ob), (nv, nb)) = (split(old), split(new));
    let keep_build = nv.is_none() || nv == ov;
    let build = nb.or(if keep_build { ob } else { None });
    os_version_text(nv.or(ov), build)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a TV is, as the picker and Settings say it: a third-party TV by maker and model
    /// from its own TXT record (the Hisense was taken for a generic "AirPlay TV" entry), an
    /// Apple TV by its model's name only when Tuner knows the identifier. The maker is the
    /// device's text: sanitized and capped at discovery like its name.
    #[test]
    fn a_tv_is_named_by_maker_and_model() {
        use airplay::features::{self, AirPlayInfo};
        let tv = |kind, model: Option<&str>, maker: Option<&str>| Device {
            model: model.map(Into::into),
            manufacturer: maker.map(Into::into),
            ..Device::new(DeviceId::derive("airplay", "x"), "TV", kind, Protocol::AirPlay)
        };
        let cases = [
            (tv(ReceiverKind::AirPlayTv, Some("85E75LE"), Some("Hisense")), "Hisense 85E75LE"),
            (tv(ReceiverKind::AirPlayTv, Some("Hisense 85E75LE"), Some("hisense")), "Hisense 85E75LE"),
            (tv(ReceiverKind::AirPlayTv, None, Some("Hisense")), "Hisense AirPlay TV"),
            (tv(ReceiverKind::AirPlayTv, Some("85E75LE"), None), "AirPlay TV 85E75LE"),
            (tv(ReceiverKind::AirPlayTv, None, None), "AirPlay TV"),
            (tv(ReceiverKind::AppleTv, Some("AppleTV14,1"), None), "Apple TV 4K (3rd generation)"),
            (tv(ReceiverKind::AppleTv, Some("AppleTV6,2"), None), "Apple TV 4K"),
            (tv(ReceiverKind::AppleTv, Some("AppleTV99,9"), Some("Apple Inc.")), "Apple TV"),
            (tv(ReceiverKind::AppleTv, None, None), "Apple TV"),
        ];
        for (d, want) in cases {
            assert_eq!(d.make(), want, "{:?} {:?}", d.model, d.manufacturer);
        }

        // from the wire: control and bidi characters go, the length is capped, a URL isn't kept
        let info = |maker: &str| AirPlayInfo {
            name: Some("Living Room TV".into()),
            model: Some("85E75LE".into()),
            manufacturer: Some(maker.into()),
            pi: Some("02:00:00:00:00:70".into()),
            features: features::VIDEO_V2 | features::VIDEO_PLAY_QUEUE,
            ..AirPlayInfo::default()
        };
        let seen = |maker: &str| discovery::sighting(&info(maker), "192.168.1.6:7000".parse().unwrap(), "k".into(), false).unwrap().manufacturer;
        assert_eq!(seen("Hisense").as_deref(), Some("Hisense"));
        assert_eq!(seen("\u{202E}Hi\u{0}sense\n").as_deref(), Some("Hisense"));
        assert_eq!(seen(&"H".repeat(300)).map(|m| m.chars().count()), Some(MAX_NAME_CHARS));
        assert_eq!(seen("http://evil.example/"), None);
    }

    /// The Apple TV's TXT version and its `/info` build meet in one line, in either order; an
    /// update drops the old build.
    #[test]
    fn os_version_halves_merge() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(merge_os_version(s("27.0"), s("(23L773)")), s("27.0 (23L773)"));
        assert_eq!(merge_os_version(s("27.0 (23L773)"), s("27.0")), s("27.0 (23L773)"));
        assert_eq!(merge_os_version(s("26.6 (23L773)"), s("27.0")), s("27.0"));
        assert_eq!(merge_os_version(s("27.0"), s("26.6 (23L773)")), s("26.6 (23L773)"));
        assert_eq!(merge_os_version(None, s("(23L773)")), s("(23L773)"));
        assert_eq!(merge_os_version(s("26.5"), None), s("26.5"));
    }

    #[test]
    fn device_strings_are_sanitized() {
        assert_eq!(sanitize_name("  Living\u{0}  Room\n", "AirPlay TV"), "Living Room");
        assert_eq!(sanitize_name("http://evil.example/x", "AirPlay TV"), "AirPlay TV");
        assert_eq!(sanitize_name("\u{202E}\u{7}", "AirPlay TV"), "AirPlay TV");
        assert_eq!(sanitize_name(&"x".repeat(200), "AirPlay TV").chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn a_pin_is_four_digits_and_never_printed() {
        assert!(Pin::new("123").is_none() && Pin::new("12a4").is_none());
        let pin = Pin::new("4711").unwrap();
        assert_eq!(pin.expose(), "4711");
        assert_eq!(format!("{pin:?}"), "Pin(****)");
    }
}
