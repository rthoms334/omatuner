//! Firewall detection and the **Allow on firewall** remedy (DESIGN §40 §9.4). The signatures
//! here are frozen (CAST-0); CAST-7 owns the bodies, `ufw.rs` (the `user.rules` parser),
//! `allow.rs` (the pkexec runner and its outcomes) and `src/bin/tuner-firewall.rs` (the root
//! helper).
//!
//! What has to be open: TCP `cast.media_port` (47800), inbound, on the LAN interface, from the
//! LAN subnet. Nothing else (timing is declared PTP: no UDP). Detection needs no root: the ufw
//! files are 0644 and `systemctl is-active` works unprivileged. Tuner never runs `sudo` and never
//! sees a password; the only root code is the helper started through `pkexec`. Agents never
//! touch the firewall.

pub mod allow;
pub mod ufw;

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

/// The comment on Tuner's rule, everywhere (the button, the copy text, detection, Remove). ufw
/// stores it hex-encoded: `6f6d6174756e65722063617374`.
pub const TAG: &str = "omatuner cast";
/// The polkit action of the packaged helper (`packaging/polkit/…policy`, `auth_admin_keep`).
pub const POLKIT_ACTION: &str = "io.github.rthoms334.omatuner.firewall";
/// Where the package installs the helper.
pub const HELPER_PATH: &str = "/usr/lib/tuner/tuner-firewall";
/// The longest an Allow / Remove may wait for polkit's dialog.
pub const PKEXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// What detection concluded (Settings ▸ Cast ▸ Network's Firewall row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirewallState {
    /// ufw not installed, disabled or inactive, and neither firewalld nor nftables active.
    NoFirewall,
    /// A covering allow exists; `ours` when it carries [`TAG`].
    Allowed { ours: bool },
    /// ufw active with input policy DROP/REJECT and no covering allow; `shadowed` when an
    /// earlier deny/reject/limit matches (ufw is first-match: the fix is `prepend`).
    Blocked { shadowed: bool },
    /// Can't tell: copy text only.
    Unknown { why: UnknownWhy },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownWhy {
    Firewalld,
    /// nftables without ufw.
    Nftables,
    /// The ufw files couldn't be read.
    Unreadable,
}

/// This computer's side of the LAN toward the TV (the UDP `connect()` trick + `if-addrs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanNet {
    /// "wlan0".
    pub iface: String,
    /// This computer's address on it (the relay binds here).
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl LanNet {
    /// The network address (host bits cleared).
    pub fn network(&self) -> Ipv4Addr {
        let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(self.prefix.min(32))) };
        Ipv4Addr::from(u32::from(self.addr) & mask)
    }

    /// The rule that opens `port` for this network.
    pub fn rule(&self, port: u16) -> Rule {
        Rule { iface: self.iface.clone(), net: self.network(), prefix: self.prefix, port }
    }
}

/// The arguments of one scoped rule: TCP `port` in on `iface` from `net/prefix`. Tagged rules
/// read back from `user.rules` have this shape too (Remove uses them, so it works after a
/// network change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub iface: String,
    pub net: Ipv4Addr,
    pub prefix: u8,
    pub port: u16,
}

impl Rule {
    /// "192.168.1.0/24".
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.net, self.prefix)
    }
}

/// One detection (cached ≤ 5 s by the service; re-run on Check again, after Allow / Remove and
/// at each session start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub state: FirewallState,
    /// `/usr/bin/ufw` exists and ufw is enabled and active.
    pub ufw: bool,
    /// `pkexec` exists (with `ufw`: the button can show).
    pub pkexec: bool,
    /// The network toward the TV, when known.
    pub net: Option<LanNet>,
    /// Every rule tagged [`TAG`] (Remove; "Allowed on another network").
    pub tagged: Vec<Rule>,
    pub port: u16,
}

impl Detection {
    /// **Allow on firewall** can show: ufw blocks, and pkexec is there.
    pub fn can_allow(&self) -> bool {
        self.ufw && self.pkexec && matches!(self.state, FirewallState::Blocked { .. })
    }

    /// **Remove rule** can show.
    pub fn can_remove(&self) -> bool {
        self.ufw && self.pkexec && !self.tagged.is_empty()
    }
}

/// The programs and files detection and the runner use; tests point them at fixtures, a fake
/// `pkexec` and a recording `ufw` stub (DESIGN §40 §14.7). Never the real ones in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallEnv {
    /// `/etc/ufw` (`ufw.conf`, `user.rules`).
    pub etc_ufw: PathBuf,
    /// `/etc/default/ufw` (`DEFAULT_INPUT_POLICY`).
    pub default_ufw: PathBuf,
    pub ufw: PathBuf,
    pub pkexec: PathBuf,
    pub systemctl: PathBuf,
    /// The packaged helper; when it (or its policy) is missing, the checkout form runs `ufw`
    /// through pkexec's generic action.
    pub helper: PathBuf,
    /// The helper's polkit policy file.
    pub policy: PathBuf,
}

impl Default for FirewallEnv {
    fn default() -> Self {
        Self {
            etc_ufw: "/etc/ufw".into(),
            default_ufw: "/etc/default/ufw".into(),
            ufw: "/usr/bin/ufw".into(),
            pkexec: "/usr/bin/pkexec".into(),
            systemctl: "/usr/bin/systemctl".into(),
            helper: HELPER_PATH.into(),
            policy: format!("/usr/share/polkit-1/actions/{POLKIT_ACTION}.policy").into(),
        }
    }
}

/// Allow or Remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// `first`: `ufw prepend` (detection found a shadowing rule).
    Allow {
        first: bool,
    },
    Remove,
}

/// How an Allow / Remove ended (from pkexec's exit status, then a re-read of `user.rules`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowOutcome {
    /// Exit 0 and the tagged rule is in `user.rules` (Remove: it's gone).
    Added,
    /// Exit 0 but the re-read doesn't show it: the 10 s fetch check decides.
    AddedUnverified,
    /// 126 (dialog dismissed) or the user's Cancel.
    Cancelled,
    /// 127 and no authentication agent: the command to run instead.
    NoAgent,
    /// 127 otherwise (needs an administrator).
    NotAuthorized,
    /// 1–125: ufw's message (redacted, ≤ 2 KiB), shown only in the firewall card.
    Failed { message: String },
    /// No answer within [`PKEXEC_TIMEOUT`].
    TimedOut,
}

/// The copyable fallback (**Show command**), always available. Contains this computer's own
/// interface and subnet, never the TV's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyText {
    pub allow: String,
    pub remove: String,
    /// Copy only; not verified on this machine.
    pub firewalld: String,
}

/// Read the firewall state toward `toward` (the TV) for `port`, without root. Runs on the
/// runtime (it spawns `systemctl`).
pub async fn detect(_env: &FirewallEnv, port: u16, _toward: Ipv4Addr) -> Detection {
    // CAST-7: files + systemctl + interface/subnet + `ufw::parse` + covering/shadowing.
    Detection { state: FirewallState::Unknown { why: UnknownWhy::Unreadable }, ufw: false, pkexec: false, net: None, tagged: Vec::new(), port }
}

/// The shell-free argv for `verb` on `rule`: the packaged helper
/// (`pkexec --disable-internal-agent <helper> allow <iface> <cidr> <port> [--first]`), or the
/// checkout form (`pkexec --disable-internal-agent <ufw> …`) when the helper or its policy isn't
/// installed.
pub fn argv(_env: &FirewallEnv, _verb: Verb, _rule: &Rule) -> Vec<OsString> {
    // CAST-7
    Vec::new()
}

/// Run `verb` on `rule` through pkexec (stdin null, stderr captured, [`PKEXEC_TIMEOUT`]);
/// `cancel` kills pkexec, which closes polkit's dialog.
pub async fn run(_env: &FirewallEnv, _verb: Verb, _rule: &Rule, _cancel: CancellationToken) -> AllowOutcome {
    // CAST-7: `allow::run`.
    AllowOutcome::Failed { message: String::new() }
}

/// The commands to copy for `rule` (DESIGN §40 §9.4 "Copyable fallback").
pub fn copy_text(rule: &Rule) -> CopyText {
    let (iface, cidr, port) = (&rule.iface, rule.cidr(), rule.port);
    CopyText {
        allow: format!("sudo ufw allow in on {iface} from {cidr} to any port {port} proto tcp comment '{TAG}'"),
        remove: format!("sudo ufw delete allow in on {iface} from {cidr} to any port {port} proto tcp"),
        firewalld: format!(
            "sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address={cidr} port port={port} protocol=tcp accept' && sudo firewall-cmd --reload"
        ),
    }
}
