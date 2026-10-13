//! The `/etc/ufw/user.rules` parser (DESIGN §40 §9.4 "Detection"). OWNER: CAST-7.
//!
//! Reads the `### tuple ###` lines (format from ufw's own reader, `backend_iptables.py`
//! L697–790): action, proto, dport, dst, sport, src, then (apps only) two app fields, then
//! `dir[_iface]` last, then `comment=<hex>` (stripped first). Actions `allow|deny|reject|limit`
//! with `_log` / `_log-all`; `route:` rules ignored; port lists and ranges; proto `any`. Finds the
//! covering allow for the media port, an earlier shadowing deny (first match), and the rules tagged
//! `omatuner cast`. Pure: tested on fixture files (`tests/fixtures/cast/ufw/`).
//!
//! The app fields precede the direction in ufw's reader (a 9-field line is
//! `… src dapp sapp dir`), not follow it as DESIGN §9.4's prose has it; the reader is the
//! authority. Only IPv4 `user.rules` is read: a `from <v4 net>` rule never lands in `user6.rules`.

use std::net::Ipv4Addr;

use super::{Rule, TAG};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
    Reject,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    /// ufw's `any`: every protocol (a rule with no `proto`).
    Any,
    /// `esp`, `gre`, … : never TCP.
    Other,
}

/// An IPv4 address with a prefix (`0.0.0.0/0` anywhere; a bare address is a /32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Net {
    fn parse(s: &str) -> Option<Net> {
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip, p.parse::<u8>().ok().filter(|p| *p <= 32)?),
            None => (s, 32),
        };
        Some(Net { addr: ip.parse().ok()?, prefix })
    }

    fn mask(self) -> u32 {
        if self.prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(self.prefix)) }
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & self.mask()) == (u32::from(self.addr) & self.mask())
    }

    fn network(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) & self.mask())
    }
}

/// One inbound IPv4 rule read from `user.rules`, in file order (ufw is first-match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tuple {
    pub action: Action,
    pub proto: Proto,
    /// Inclusive `(low, high)` ranges; `None` is `any` (no `port`).
    pub ports: Option<Vec<(u16, u16)>>,
    pub dst: Net,
    /// The source port is `any`. A rule on a fixed source port never matches the TV's ephemeral one.
    pub sport_any: bool,
    pub src: Net,
    /// `in_<iface>`; `None` is every interface.
    pub iface: Option<String>,
    /// The decoded `comment=` (None when absent or not hex).
    pub comment: Option<String>,
}

/// The traffic Tuner needs through: the TV connecting to `local:port` on `iface`.
#[derive(Debug, Clone, Copy)]
pub struct Flow<'a> {
    pub port: u16,
    pub iface: &'a str,
    pub local: Ipv4Addr,
    pub tv: Ipv4Addr,
}

/// How `user.rules` treats a [`Flow`] (first matching rule wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The first match is an allow; `ours` when it carries the tag.
    Allowed { ours: bool },
    /// The first match is a deny, reject or limit.
    Denied,
    /// No rule matches: the input policy decides.
    Unmatched,
}

impl Tuple {
    /// Does this rule apply to `flow` (any action)?
    pub fn matches(&self, flow: &Flow) -> bool {
        matches!(self.proto, Proto::Tcp | Proto::Any)
            && self.ports.as_ref().is_none_or(|r| r.iter().any(|&(lo, hi)| (lo..=hi).contains(&flow.port)))
            && self.dst.contains(flow.local)
            && self.src.contains(flow.tv)
            && self.sport_any
            && self.iface.as_deref().is_none_or(|i| i == flow.iface)
    }

    /// Carries Tuner's tag.
    pub fn is_ours(&self) -> bool {
        self.comment.as_deref() == Some(TAG)
    }

    /// The scoped rule Tuner adds (allow in on `<iface>` from `<net>` to any port `<port>` proto
    /// tcp), when this tuple is exactly that shape and tagged: what Remove needs.
    fn tagged_rule(&self) -> Option<Rule> {
        let port = match self.ports.as_deref() {
            Some([(lo, hi)]) if lo == hi => *lo,
            _ => return None,
        };
        let ours = self.is_ours() && self.action == Action::Allow && self.proto == Proto::Tcp && self.sport_any && self.dst.prefix == 0;
        let iface = self.iface.clone().filter(|_| ours)?;
        Some(Rule { iface, net: self.src.network(), prefix: self.src.prefix, port })
    }
}

/// Every inbound IPv4 tuple of `text` (the file), in order. Malformed, outbound, `route:` and
/// two-interface lines are skipped, as ufw's reader skips what it can't use.
pub fn parse(text: &str) -> Vec<Tuple> {
    text.lines().filter_map(parse_line).collect()
}

/// The first rule that matches `flow` decides.
pub fn evaluate(tuples: &[Tuple], flow: &Flow) -> Verdict {
    match tuples.iter().find(|t| t.matches(flow)) {
        Some(t) if t.action == Action::Allow => Verdict::Allowed { ours: t.is_ours() },
        Some(_) => Verdict::Denied,
        None => Verdict::Unmatched,
    }
}

/// Every rule tagged [`TAG`] in Tuner's shape, de-duplicated, in file order.
pub fn tagged(tuples: &[Tuple]) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for rule in tuples.iter().filter_map(Tuple::tagged_rule) {
        if !out.contains(&rule) {
            out.push(rule);
        }
    }
    out
}

fn parse_line(line: &str) -> Option<Tuple> {
    let rest = line.strip_prefix("### tuple ###")?;
    // `comment=` is always last: ufw strips it first.
    let (body, comment) = match rest.split_once(" comment=") {
        Some((body, hex)) => (body, hex_decode(hex.trim())),
        None => (rest, None),
    };
    let f: Vec<&str> = body.split_whitespace().collect();
    if !(6..=9).contains(&f.len()) {
        return None;
    }
    // 7 and 9 fields end in the direction; 6 and 8 are the pre-direction format ("in").
    let iface = match f.len() {
        7 | 9 => direction(f[f.len() - 1])?,
        _ => None,
    };
    let (action, log) = f[0].split_once('_').map_or((f[0], ""), |(a, l)| (a, l));
    if !matches!(log, "" | "log" | "log-all") {
        return None;
    }
    let action = match action {
        "allow" => Action::Allow,
        "deny" => Action::Deny,
        "reject" => Action::Reject,
        "limit" => Action::Limit,
        _ => return None, // `route:allow` and anything newer
    };
    let proto = match f[1] {
        "tcp" => Proto::Tcp,
        "udp" => Proto::Udp,
        "any" => Proto::Any,
        _ => Proto::Other,
    };
    Some(Tuple { action, proto, ports: ports(f[2])?, dst: Net::parse(f[3])?, sport_any: f[4] == "any", src: Net::parse(f[5])?, iface, comment })
}

/// The `dir[_iface]` field: `Some(iface)` for `in_<iface>`, `None` for plain `in`; the outer
/// `None` for anything that isn't an input rule (`out…`, or ufw's `in_a!out_b`).
fn direction(field: &str) -> Option<Option<String>> {
    match field.split_once('_') {
        None => (field == "in").then_some(None),
        Some(("in", iface)) if !iface.is_empty() && !iface.contains('!') => Some(Some(iface.to_owned())),
        Some(_) => None,
    }
}

/// `any`, `80`, `80,443`, `8080:8090` or a mix. The outer `None` is a malformed field.
fn ports(field: &str) -> Option<Option<Vec<(u16, u16)>>> {
    if field == "any" {
        return Some(None);
    }
    let mut out = Vec::new();
    for part in field.split(',') {
        let (lo, hi) = part.split_once(':').unwrap_or((part, part));
        let (lo, hi) = (lo.parse::<u16>().ok()?, hi.parse::<u16>().ok()?);
        out.push((lo.min(hi), lo.max(hi)));
    }
    Some(Some(out))
}

/// ufw stores comments as the hex of their UTF-8 bytes.
fn hex_decode(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}
