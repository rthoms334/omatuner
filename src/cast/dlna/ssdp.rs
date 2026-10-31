//! SSDP (DESIGN §40 §15.3, Phase 3): finding MediaRenderers. Pure message building and parsing;
//! the browse in `cast::discovery` sends and listens.
//!
//! **Why three ways.** Omarchy's ufw drops unsolicited inbound UDP, and an answer to a multicast
//! M-SEARCH is a unicast packet that no conntrack entry matches (the request went to
//! 239.255.255.250), so a firewalled computer never hears them [NE]. What does get through:
//! * `NOTIFY` announcements, multicast to 239.255.255.250:1900, which ufw's `before.rules` allow;
//!   a renderer sends them when it starts and then every few minutes;
//! * answers to a **unicast** M-SEARCH (UDA 1.1 §1.3.3): the request to `<ip>:1900` leaves a
//!   conntrack entry its answer matches. The browse sends one to every address of this
//!   computer's subnet (at most [`MAX_SWEEP`]), paced, once per browse;
//! * the multicast M-SEARCH, for computers without a firewall.

use std::net::{Ipv4Addr, SocketAddr};

/// The SSDP group and port.
pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const PORT: u16 = 1900;
/// What Tuner looks for.
pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// The most addresses a unicast sweep asks (a /23; a bigger subnet is swept around this
/// computer's own address).
pub const MAX_SWEEP: u32 = 510;

/// An M-SEARCH for MediaRenderers, to the group (`MAN`, `MX` 2) or to one host (no `MX`, UDA 1.1).
pub fn m_search(to: SocketAddr, multicast: bool) -> String {
    let mut m = format!("M-SEARCH * HTTP/1.1\r\nHOST: {to}\r\nMAN: \"ssdp:discover\"\r\n");
    if multicast {
        m.push_str("MX: 2\r\n");
    }
    m.push_str(&format!("ST: {MEDIA_RENDERER}\r\nUSER-AGENT: Linux/1.0 UPnP/1.1 Tuner/1.0\r\n\r\n"));
    m
}

/// A renderer announced itself or answered: where its description is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// An answer, or a `NOTIFY ssdp:alive`, for a MediaRenderer: its `LOCATION` and `USN`.
    Alive { location: String, usn: String },
    /// `NOTIFY ssdp:byebye`.
    Byebye { usn: String },
}

/// One SSDP datagram (`None`: not about a MediaRenderer, or not SSDP).
pub fn parse(datagram: &[u8]) -> Option<Heard> {
    let text = std::str::from_utf8(datagram).ok()?;
    let start = text.lines().next()?.trim();
    let answer = start.starts_with("HTTP/1.1 200") || start.starts_with("HTTP/1.0 200");
    let notify = start.starts_with("NOTIFY ");
    if !answer && !notify {
        return None;
    }
    let get = |want: &str| header(text, want);
    let kind = if answer { get("ST")? } else { get("NT")? };
    if !kind.eq_ignore_ascii_case(MEDIA_RENDERER) && !kind.starts_with("urn:schemas-upnp-org:device:MediaRenderer:") {
        return None;
    }
    let usn = get("USN")?;
    if notify && get("NTS").is_some_and(|n| n.eq_ignore_ascii_case("ssdp:byebye")) {
        return Some(Heard::Byebye { usn });
    }
    let location = get("LOCATION")?;
    Some(Heard::Alive { location, usn })
}

fn header(text: &str, want: &str) -> Option<String> {
    text.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(want).then(|| v.trim().to_owned())
    })
}

/// The addresses a unicast sweep asks: this computer's subnet (`ip`/`prefix`) without the
/// network, broadcast and own addresses, at most [`MAX_SWEEP`] around `ip`. Nothing for a /31
/// or /32, or a subnet that isn't private (RFC 1918): the sweep never leaves the LAN.
pub fn sweep(ip: Ipv4Addr, prefix: u8) -> Vec<Ipv4Addr> {
    if prefix > 30 || !ip.is_private() {
        return Vec::new();
    }
    let me = u32::from(ip);
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    let (net, bcast) = (me & mask, (me & mask) | !mask);
    let (first, last) = (net + 1, bcast - 1);
    let half = MAX_SWEEP / 2;
    let lo = me.saturating_sub(half).max(first);
    let hi = (lo + MAX_SWEEP).min(last);
    let lo = hi.saturating_sub(MAX_SWEEP).max(first);
    (lo..=hi).filter(|a| *a != me).map(Ipv4Addr::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m_searches() {
        let group = SocketAddr::from((GROUP, PORT));
        let m = m_search(group, true);
        assert!(m.starts_with("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n"));
        assert!(m.contains("ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n") && m.ends_with("\r\n\r\n"));
        let unicast = m_search(SocketAddr::from(([192, 168, 1, 40], PORT)), false);
        assert!(unicast.contains("HOST: 192.168.1.40:1900") && !unicast.contains("MX:"));
    }

    #[test]
    fn answers_and_announcements() {
        let answer = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLocation: http://192.168.1.40:9197/dmr\r\n\
SERVER: SHP, UPnP/1.0, Samsung UPnP SDK/1.0\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:0bf4::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(
            parse(answer),
            Some(Heard::Alive {
                location: "http://192.168.1.40:9197/dmr".into(),
                usn: "uuid:0bf4::urn:schemas-upnp-org:device:MediaRenderer:1".into()
            })
        );
        let alive = b"NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: urn:schemas-upnp-org:device:MediaRenderer:1\r\nNTS: ssdp:alive\r\n\
LOCATION: http://192.168.1.41:1400/xml/device_description.xml\r\nUSN: uuid:lg::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert!(matches!(parse(alive), Some(Heard::Alive { .. })));
        let bye = b"NOTIFY * HTTP/1.1\r\nNT: urn:schemas-upnp-org:device:MediaRenderer:1\r\nNTS: ssdp:byebye\r\nUSN: uuid:lg\r\n\r\n";
        assert_eq!(parse(bye), Some(Heard::Byebye { usn: "uuid:lg".into() }));
        // another device type, a search, garbage
        let server =
            b"NOTIFY * HTTP/1.1\r\nNT: urn:schemas-upnp-org:device:MediaServer:1\r\nNTS: ssdp:alive\r\nLOCATION: http://x/\r\nUSN: u\r\n\r\n";
        assert_eq!(parse(server), None);
        assert_eq!(parse(m_search(SocketAddr::from((GROUP, PORT)), true).as_bytes()), None);
        assert_eq!(parse(&[0xff, 0xfe]), None);
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: u\r\n\r\n"), None, "no LOCATION");
    }

    #[test]
    fn the_sweep_stays_in_the_private_subnet() {
        let me = Ipv4Addr::new(192, 168, 1, 88);
        let s = sweep(me, 24);
        assert_eq!(s.len(), 253);
        assert_eq!((s[0], *s.last().unwrap()), (Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 254)));
        assert!(!s.contains(&me));
        let big = sweep(Ipv4Addr::new(10, 1, 2, 3), 8);
        assert_eq!(big.len(), MAX_SWEEP as usize);
        assert!(big.iter().all(|a| a.octets()[0] == 10));
        assert!(sweep(Ipv4Addr::new(8, 8, 8, 8), 24).is_empty(), "not a LAN");
        assert!(sweep(me, 31).is_empty() && sweep(me, 32).is_empty());
        assert_eq!(sweep(Ipv4Addr::new(192, 168, 1, 1), 30), vec![Ipv4Addr::new(192, 168, 1, 2)]);
    }
}
