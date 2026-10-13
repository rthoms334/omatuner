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
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// The comment on Tuner's rule, everywhere (the button, the copy text, detection, Remove). ufw
/// stores it hex-encoded: `6f6d6174756e65722063617374`.
pub const TAG: &str = "omatuner cast";
/// The polkit action of the packaged helper (`packaging/polkit/…policy`, `auth_admin_keep`).
pub const POLKIT_ACTION: &str = "io.github.rthoms334.omatuner.firewall";
/// Where the package installs the helper.
pub const HELPER_PATH: &str = "/usr/lib/tuner/tuner-firewall";
/// The longest an Allow / Remove may wait for polkit's dialog.
pub const PKEXEC_TIMEOUT: Duration = Duration::from_secs(120);

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
    /// No TV to judge toward (the detection was made toward the mDNS group: no TV known yet).
    /// ufw's verdict depends on the TV's address (a rule is scoped to a subnet or a host), so
    /// "blocked" toward anything but the TV itself would be a guess (DESIGN §40 §9.4).
    NoTv,
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

/// One detection, made toward one address (cached ≤ 5 s per address by the service; re-run on
/// Check again, after Allow / Remove and at each session start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The state for a TV at [`Detection::toward`] only: ufw rules are scoped by source, so
    /// another address can get another answer (DESIGN §40 §9.4 "Toward which TV").
    pub state: FirewallState,
    /// The address it was made toward: the TV, or the mDNS group when no TV is known.
    pub toward: Ipv4Addr,
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
    /// The state for the TV at `tv`, when this detection can say: it was made toward that very
    /// address and found the network toward it. `None` otherwise (made toward another TV or the
    /// mDNS group, or no route): nothing may be concluded about `tv` from it, least of all that
    /// the firewall blocks it (DESIGN §40 §12, the agent's gate).
    pub fn state_for(&self, tv: Ipv4Addr) -> Option<FirewallState> {
        (self.toward == tv && self.net.is_some()).then_some(self.state)
    }

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
pub async fn detect(env: &FirewallEnv, port: u16, toward: Ipv4Addr) -> Detection {
    let (ufw_unit, firewalld, nftables) = tokio::join!(unit_active(env, "ufw"), unit_active(env, "firewalld"), unit_active(env, "nftables"));
    let net = lan_net(toward);
    let mut det = Detection { state: FirewallState::NoFirewall, toward, ufw: false, pkexec: env.pkexec.exists(), net, tagged: Vec::new(), port };
    let unreadable = FirewallState::Unknown { why: UnknownWhy::Unreadable };
    // ufw is in force when its binary is there, its unit is active and ufw.conf says ENABLED=yes.
    let mut ufw_on = false;
    if env.ufw.exists() && ufw_unit {
        match tokio::fs::read_to_string(env.etc_ufw.join("ufw.conf")).await {
            Ok(conf) => ufw_on = conf_value(&conf, "ENABLED").is_some_and(|v| v.eq_ignore_ascii_case("yes")),
            Err(_) => {
                det.state = unreadable;
                return det;
            }
        }
    }
    if !ufw_on {
        det.state = if firewalld {
            FirewallState::Unknown { why: UnknownWhy::Firewalld }
        } else if nftables {
            FirewallState::Unknown { why: UnknownWhy::Nftables }
        } else {
            FirewallState::NoFirewall
        };
        return det;
    }
    det.ufw = true;
    let (policy, rules) = tokio::join!(tokio::fs::read_to_string(&env.default_ufw), tokio::fs::read_to_string(env.etc_ufw.join("user.rules")));
    let (Ok(policy), Ok(rules)) = (policy, rules) else {
        det.state = unreadable;
        return det;
    };
    let policy_blocks = input_policy_blocks(&policy);
    match det.net.clone() {
        Some(net) => (det.state, det.tagged) = assess(&rules, policy_blocks, &net, toward, port),
        // No route toward the TV: the interface (and so any rule's scope) is unknown.
        None => {
            det.tagged = ufw::tagged(&ufw::parse(&rules));
            det.state = unreadable;
        }
    }
    det
}

/// The state of `rules` (the text of `user.rules`) for the TV at `tv` reaching `net.addr:port`
/// on `net.iface`, and every rule tagged [`TAG`]. Pure: [`detect`] without the files and
/// `systemctl`.
///
/// ufw is first-match: the first rule matching the traffic decides; with none, the input policy
/// does (`policy_blocks`: `DEFAULT_INPUT_POLICY` is DROP or REJECT).
///
/// `tv` that isn't a host (the mDNS group the service falls back to before it knows a TV, a
/// broadcast) never sends the flow: a rule scoped to the LAN (`from 192.168.1.0/24`) doesn't
/// contain it, so the policy would answer "blocked" for a port the LAN's TVs reach. Toward such
/// an address a block is [`UnknownWhy::NoTv`]; an allow that covers it (an unscoped rule) still
/// counts.
pub fn assess(rules: &str, policy_blocks: bool, net: &LanNet, tv: Ipv4Addr, port: u16) -> (FirewallState, Vec<Rule>) {
    let tuples = ufw::parse(rules);
    let flow = ufw::Flow { port, iface: &net.iface, local: net.addr, tv };
    let state = match ufw::evaluate(&tuples, &flow) {
        ufw::Verdict::Allowed { ours } => FirewallState::Allowed { ours },
        ufw::Verdict::Unmatched if !policy_blocks => FirewallState::NoFirewall,
        _ if !is_host(tv) => FirewallState::Unknown { why: UnknownWhy::NoTv },
        ufw::Verdict::Denied => FirewallState::Blocked { shadowed: true },
        ufw::Verdict::Unmatched => FirewallState::Blocked { shadowed: false },
    };
    (state, ufw::tagged(&tuples))
}

/// An address one machine sends from (not multicast, broadcast or unspecified; the loopback is
/// one: `tests/cast_firewall.rs` detects toward it).
fn is_host(ip: Ipv4Addr) -> bool {
    !(ip.is_multicast() || ip.is_broadcast() || ip.is_unspecified())
}

/// `DEFAULT_INPUT_POLICY` of `/etc/default/ufw`: DROP and REJECT block, ACCEPT doesn't; a missing
/// key is ufw's own default, DROP.
pub fn input_policy_blocks(default_ufw: &str) -> bool {
    conf_value(default_ufw, "DEFAULT_INPUT_POLICY").is_none_or(|v| !v.eq_ignore_ascii_case("accept"))
}

/// The last `KEY=value` of a shell-style config file, unquoted.
fn conf_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(key)?.trim_start().strip_prefix('='))
        .next_back()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\''))
}

/// Does `systemctl is-active <unit>` say active? (Works without root; a missing `systemctl` or
/// one that doesn't answer in 5 s is "no".)
async fn unit_active(env: &FirewallEnv, unit: &str) -> bool {
    let mut cmd = tokio::process::Command::new(&env.systemctl);
    cmd.args(["is-active", "--quiet", unit]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    matches!(tokio::time::timeout(Duration::from_secs(5), cmd.status()).await, Ok(Ok(st)) if st.success())
}

/// This computer's side of the LAN toward `toward`: the UDP `connect()` trick names the source
/// address the kernel would use (no packet is sent), `if-addrs` the interface and prefix that own it.
pub fn lan_net(toward: Ipv4Addr) -> Option<LanNet> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((toward, 9)).ok()?;
    let SocketAddr::V4(local) = sock.local_addr().ok()? else { return None };
    if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| match i.addr {
        if_addrs::IfAddr::V4(v4) if v4.ip == *local.ip() => Some(LanNet { iface: i.name, addr: v4.ip, prefix: v4.prefixlen }),
        _ => None,
    })
}

/// The shell-free argv for `verb` on `rule`: the packaged helper
/// (`pkexec --disable-internal-agent <helper> allow <iface> <cidr> <port> [--first]`), or the
/// checkout form (`pkexec --disable-internal-agent <ufw> …`) when the helper or its policy isn't
/// installed. Element 0 is the `pkexec` program itself.
pub fn argv(env: &FirewallEnv, verb: Verb, rule: &Rule) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![env.pkexec.clone().into(), "--disable-internal-agent".into()];
    let (iface, cidr, port) = (rule.iface.as_str(), rule.cidr(), rule.port.to_string());
    if env.helper.exists() && env.policy.exists() {
        a.push(env.helper.clone().into());
        a.push(if matches!(verb, Verb::Remove) { "remove" } else { "allow" }.into());
        a.extend([iface.into(), cidr.into(), port.into()]);
        if matches!(verb, Verb::Allow { first: true }) {
            a.push("--first".into());
        }
    } else {
        // `man ufw`: `ufw [prepend] allow|delete allow …`; the comment is one argument, no quotes.
        a.push(env.ufw.clone().into());
        match verb {
            Verb::Allow { first } => {
                if first {
                    a.push("prepend".into());
                }
                a.push("allow".into());
            }
            Verb::Remove => a.extend(["delete".into(), "allow".into()]),
        }
        a.extend(["in", "on", iface, "from", &cidr, "to", "any", "port", &port, "proto", "tcp"].map(OsString::from));
        if !matches!(verb, Verb::Remove) {
            a.extend(["comment", TAG].map(OsString::from));
        }
    }
    a
}

/// Run `verb` on `rule` through pkexec (stdin null, stderr captured, [`PKEXEC_TIMEOUT`]);
/// `cancel` kills pkexec, which closes polkit's dialog.
pub async fn run(env: &FirewallEnv, verb: Verb, rule: &Rule, cancel: CancellationToken) -> AllowOutcome {
    allow::run_with(env, verb, rule, cancel, PKEXEC_TIMEOUT).await
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
