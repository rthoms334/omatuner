//! Discovery (DESIGN §40 §6): a `Discovery` trait with `MdnsDiscovery` (mdns-sd `=0.21.4`,
//! `_airplay._tcp.local.` and `_googlecast._tcp.local.`, plus SSDP for DLNA renderers; on demand
//! only), `NullDiscovery` (off the desktop: tests never touch the LAN) and `FakeDiscovery`
//! (tests); plus the manual address probe (`GET /info` on 7000, then a Cast device's
//! `eureka_info` on 8008, then a unicast M-SEARCH). OWNER: CAST-6.
//!
//! DLNA renderers answer SSDP ([`dlna::ssdp`]): the browse listens for `NOTIFY` on
//! 239.255.255.250:1900, sends the multicast M-SEARCH, and one unicast M-SEARCH to each address
//! of this computer's subnet (their answers get through ufw; the module docs there say why),
//! then reads each renderer's description over HTTP.
//!
//! mdns-sd co-binds 5353 with avahi and Chromium (`SO_REUSEADDR`/`SO_REUSEPORT`) and, as a
//! browser, queries and listens by multicast, which ufw's `before.rules` allow; no avahi
//! dependency. It browses only while someone asks: the picker or Settings ▸ Cast is open, the
//! 5 s first-run pass, a 2 s refresh before casting to a remembered TV (the service decides;
//! there is no continuous scan). Its daemon thread is the only OS thread casting adds, and it
//! exists only while a browse runs.
//!
//! Everything a receiver says is data: [`sighting`] classifies it (`airplay::features`), derives
//! the stable [`DeviceId`] (never from an address) and sanitizes every string; the address stays
//! inside the crate (`Device::addr`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::airplay::{self, features};
use super::dlna::{self, description, ssdp};
use super::{CastErrorKind, DeviceId, Protocol, ReceiverKind, Usability, sanitize_name};

/// The AirPlay service type.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
/// The Google Cast service type.
pub const GOOGLECAST_SERVICE: &str = "_googlecast._tcp.local.";
/// `ca` bit 0: video out (Chromium `cast_device_capability.h`); audio-only speakers and groups
/// lack it.
pub const CAST_VIDEO_OUT: u64 = 0x01;
/// The Cast device's unauthenticated info page (the manual probe).
pub const CAST_INFO_PORT: u16 = 8008;
/// A receiver's name when it gives none (or one that looks like a URL).
pub const FALLBACK_NAME: &str = "AirPlay TV";
/// How often the browse loop looks at its stop token between mDNS events.
const POLL: Duration = Duration::from_millis(250);

/// One receiver as discovery (or "Add by address…") saw it: classified, sanitized, with its
/// address (crate-private once it reaches a `Device`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub id: DeviceId,
    /// What removes it again: the mDNS instance's full name, or `manual:<ip>`.
    pub key: String,
    pub name: String,
    pub kind: ReceiverKind,
    pub protocol: Protocol,
    pub model: Option<String>,
    /// "Hisense" (sanitized; kept in memory only: the store has no column for it, and the
    /// picker browses whenever it is open).
    pub manufacturer: Option<String>,
    /// "26.6 (23L773)".
    pub os_version: Option<String>,
    /// "960.13.1".
    pub source_version: Option<String>,
    pub pairing_required: bool,
    pub usability: Usability,
    pub manual: bool,
    pub addr: SocketAddr,
    /// The TXT record's `psi`/`pi`/`gid` (empty for a manual probe: the session reads `/info`
    /// itself).
    pub ids: features::ReceiverIds,
    /// A DLNA renderer's control URLs.
    pub dlna: Option<dlna::Control>,
}

/// What a browse reports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // a few per browse; boxing buys nothing
pub enum Seen {
    Found(Sighting),
    /// The instance with this [`Sighting::key`] left the network.
    Gone(String),
}

/// A receiver from its TXT record or `/info` plist (`None`: not a video receiver, or no stable
/// id).
pub fn sighting(info: &features::AirPlayInfo, addr: SocketAddr, key: String, manual: bool) -> Option<Sighting> {
    let class = features::classify(info)?;
    let id = features::device_id(info)?;
    let clean = |s: &Option<String>| s.as_deref().map(|v| sanitize_name(v, "")).filter(|v| !v.is_empty());
    let os_version = crate::cast::os_version_text(clean(&info.os_version), clean(&info.os_build));
    Some(Sighting {
        id,
        key,
        name: sanitize_name(info.name.as_deref().unwrap_or_default(), FALLBACK_NAME),
        kind: class.kind,
        protocol: Protocol::AirPlay,
        model: clean(&info.model),
        manufacturer: clean(&info.manufacturer),
        os_version,
        source_version: clean(&info.source_version),
        pairing_required: class.pairing_required,
        usability: class.usability,
        manual,
        addr,
        ids: if manual { features::ReceiverIds::default() } else { info.ids() },
        dlna: None,
    })
}

/// A Google Cast device from its TXT record (`id`, `fn`, `md`, `ca`) at `addr` (`None`: no
/// video out, or no id).
pub fn cast_sighting(txt: &[(String, String)], addr: SocketAddr, key: String, manual: bool) -> Option<Sighting> {
    let get = |k: &str| txt.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_owned()).filter(|v| !v.is_empty());
    let id = get("id")?;
    let ca: u64 = get("ca").and_then(|c| c.parse().ok()).unwrap_or(0);
    if ca & CAST_VIDEO_OUT == 0 {
        return None;
    }
    let clean = |s: Option<String>| s.map(|v| sanitize_name(&v, "")).filter(|v| !v.is_empty());
    Some(Sighting {
        id: DeviceId::derive(Protocol::GoogleCast.as_str(), &id.to_lowercase().replace('-', "")),
        key,
        name: sanitize_name(&get("fn").unwrap_or_default(), "Google Cast"),
        kind: ReceiverKind::Chromecast,
        protocol: Protocol::GoogleCast,
        model: clean(get("md")),
        manufacturer: None,
        os_version: None,
        source_version: clean(get("ve")),
        pairing_required: false,
        usability: Usability::Usable,
        manual,
        addr,
        ids: features::ReceiverIds::default(),
        dlna: None,
    })
}

/// A DLNA renderer from its description (fetched from `location`).
pub fn dlna_sighting(r: &description::Renderer, location: &url::Url, manual: bool) -> Option<Sighting> {
    let host: Ipv4Addr = location.host_str()?.parse().ok()?;
    let addr = SocketAddr::from((host, location.port_or_known_default()?));
    let clean = |s: &Option<String>| s.as_deref().map(|v| sanitize_name(v, "")).filter(|v| !v.is_empty());
    Some(Sighting {
        id: DeviceId::derive(Protocol::Dlna.as_str(), &r.udn.to_lowercase()),
        key: format!("dlna:{}", r.udn),
        name: sanitize_name(&r.name, "DLNA TV"),
        kind: ReceiverKind::Dlna,
        protocol: Protocol::Dlna,
        model: clean(&r.model),
        manufacturer: clean(&r.manufacturer),
        os_version: None,
        source_version: None,
        pairing_required: false,
        usability: Usability::Usable,
        manual,
        addr,
        ids: features::ReceiverIds::default(),
        dlna: Some(dlna::Control { av_transport: r.av_transport.clone(), rendering: r.rendering.clone() }),
    })
}

/// Where the service finds receivers.
pub trait Discovery: Send + Sync {
    /// Browse until `stop` is cancelled, reporting to `tx`. Returns at once.
    fn browse(&self, rt: &tokio::runtime::Handle, tx: UnboundedSender<Seen>, stop: CancellationToken);
    /// "Add by address…": `GET http://<ip>:7000/info` (unauthenticated), then the same
    /// classification as a TXT record.
    fn probe(&self, ip: Ipv4Addr) -> BoxFuture<'static, Result<Sighting, CastErrorKind>>;
}

/// The LAN through mdns-sd (production on the desktop).
#[derive(Debug, Default)]
pub struct MdnsDiscovery;

impl Discovery for MdnsDiscovery {
    fn browse(&self, rt: &tokio::runtime::Handle, tx: UnboundedSender<Seen>, stop: CancellationToken) {
        // the daemon's receivers block: the loops run on the blocking pool, only while browsing
        let (mdns_tx, mdns_stop) = (tx.clone(), stop.clone());
        rt.spawn_blocking(move || {
            let daemon = match mdns_sd::ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::info!("cast discovery: mDNS unavailable: {e}");
                    return;
                }
            };
            match (daemon.browse(AIRPLAY_SERVICE), daemon.browse(GOOGLECAST_SERVICE)) {
                (Ok(airplay), Ok(cast)) => pump(&[&airplay, &cast], &mdns_tx, &mdns_stop),
                (Ok(airplay), Err(e)) => {
                    tracing::info!("cast discovery: Google Cast browse failed: {e}");
                    pump(&[&airplay], &mdns_tx, &mdns_stop)
                }
                (Err(e), _) => tracing::info!("cast discovery: browse failed: {e}"),
            }
            let _ = daemon.stop_browse(AIRPLAY_SERVICE);
            let _ = daemon.stop_browse(GOOGLECAST_SERVICE);
            let _ = daemon.shutdown();
        });
        rt.spawn(ssdp_browse(tx, stop));
    }

    fn probe(&self, ip: Ipv4Addr) -> BoxFuture<'static, Result<Sighting, CastErrorKind>> {
        Box::pin(async move {
            let addr = SocketAddr::from((ip, airplay::CONTROL_PORT));
            // AirPlay first (the receivers Tuner has always taken), then Google Cast, then DLNA
            let airplay = match airplay::probe_info(addr).await {
                // a receiver that isn't a video TV is "unsupported", not "unreachable": it answered
                Ok(info) => return sighting(&info, addr, format!("manual:{ip}"), true).ok_or(CastErrorKind::Unsupported),
                Err(e) => e,
            };
            if let Some(s) = probe_cast(ip).await {
                return Ok(s);
            }
            if let Some(s) = probe_dlna(ip).await {
                return Ok(s);
            }
            Err(airplay)
        })
    }
}

/// "Add by address…" for a Cast device: its `eureka_info` (name, model, id; unauthenticated on
/// 8008), then the control port.
async fn probe_cast(ip: Ipv4Addr) -> Option<Sighting> {
    let http = lan_http()?;
    let url = format!("http://{ip}:{CAST_INFO_PORT}/setup/eureka_info?params=name,device_info,ssdp_udn");
    let v: serde_json::Value = http.get(url).send().await.ok()?.json().await.ok()?;
    // the SSDP UDN is the TXT record's `id` with dashes: the same device as discovery's
    let id = v["ssdp_udn"].as_str().or(v["device_info"]["ssdp_udn"].as_str()).or(v["device_info"]["cloud_device_id"].as_str())?;
    let txt = vec![
        ("id".to_owned(), id.to_owned()),
        ("fn".to_owned(), v["name"].as_str().unwrap_or_default().to_owned()),
        ("md".to_owned(), v["device_info"]["model_name"].as_str().unwrap_or_default().to_owned()),
        // the page is served by devices with a screen and by speakers alike: a speaker fails at
        // LOAD (as it would from the picker), there is no capability field to tell
        ("ca".to_owned(), CAST_VIDEO_OUT.to_string()),
    ];
    cast_sighting(&txt, SocketAddr::from((ip, crate::cast::googlecast::PORT)), format!("manual:{ip}"), true)
}

/// "Add by address…" for a DLNA renderer: a unicast M-SEARCH to it (its answer gets through
/// ufw: it matches the request's conntrack entry), then its description.
async fn probe_dlna(ip: Ipv4Addr) -> Option<Sighting> {
    let sock = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    let to = SocketAddr::from((ip, ssdp::PORT));
    let msg = ssdp::m_search(to, false);
    let mut buf = vec![0u8; 2048];
    for _ in 0..2 {
        sock.send_to(msg.as_bytes(), to).await.ok()?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
            if from.ip() != IpAddr::V4(ip) {
                continue;
            }
            if let Some(ssdp::Heard::Alive { location, .. }) = ssdp::parse(&buf[..n]) {
                return describe(&location, Some(ip), true).await;
            }
        }
    }
    None
}

/// A client for the LAN: no proxy (a renderer is never behind one), short timeouts, no redirects.
fn lan_http() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .connect_timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
}

/// Fetch and read the description at `location` (`from`: the host that announced it, which it
/// must name; a description elsewhere is ignored).
async fn describe(location: &str, from: Option<Ipv4Addr>, manual: bool) -> Option<Sighting> {
    let url = url::Url::parse(location).ok()?;
    let host: Ipv4Addr = url.host_str()?.parse().ok()?;
    if url.scheme() != "http" || !host.is_private() || from.is_some_and(|f| f != host) {
        return None;
    }
    let mut resp = lan_http()?.get(url.clone()).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if body.len() + chunk.len() > description::MAX_DESCRIPTION {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let r = description::parse(&String::from_utf8_lossy(&body), &url)?;
    dlna_sighting(&r, &url, manual)
}

/// The SSDP side of a browse (module docs): NOTIFY on the group, the multicast M-SEARCH, and a
/// paced unicast sweep of this computer's subnet; each new renderer's description is fetched
/// once per browse.
async fn ssdp_browse(tx: UnboundedSender<Seen>, stop: CancellationToken) {
    let search = match tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::info!("cast discovery: SSDP unavailable: {e}");
            return;
        }
    };
    let notify = notify_socket();
    let group = SocketAddr::from((ssdp::GROUP, ssdp::PORT));
    let _ = search.send_to(ssdp::m_search(group, true).as_bytes(), group).await;
    let mut sweep: std::collections::VecDeque<Ipv4Addr> = local_subnet().map(|(ip, prefix)| ssdp::sweep(ip, prefix)).unwrap_or_default().into();
    let mut pace = tokio::time::interval(Duration::from_millis(4));
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let (mut a, mut b) = (vec![0u8; 2048], vec![0u8; 2048]);
    let (desc_tx, mut desc_rx) = tokio::sync::mpsc::unbounded_channel::<Option<Sighting>>();
    loop {
        let heard = tokio::select! {
            _ = stop.cancelled() => return,
            _ = pace.tick(), if !sweep.is_empty() => {
                if let Some(ip) = sweep.pop_front() {
                    let to = SocketAddr::from((ip, ssdp::PORT));
                    let _ = search.send_to(ssdp::m_search(to, false).as_bytes(), to).await;
                }
                continue;
            }
            r = search.recv_from(&mut a) => r.ok().map(|(n, from)| (ssdp::parse(&a[..n]), from)),
            r = recv(notify.as_ref(), &mut b) => r.map(|(n, from)| (ssdp::parse(&b[..n]), from)),
            Some(s) = desc_rx.recv() => {
                if let Some(s) = s
                    && tx.send(Seen::Found(s)).is_err()
                {
                    return;
                }
                continue;
            }
        };
        let Some((Some(heard), from)) = heard else { continue };
        let IpAddr::V4(from) = from.ip() else { continue };
        match heard {
            ssdp::Heard::Alive { location, usn } => {
                if seen.insert(usn, location.clone()).is_some() {
                    continue;
                }
                let desc_tx = desc_tx.clone();
                tokio::spawn(async move {
                    let _ = desc_tx.send(describe(&location, Some(from), false).await);
                });
            }
            ssdp::Heard::Byebye { usn } => {
                let udn = usn.split("::").next().unwrap_or(&usn).to_owned();
                if tx.send(Seen::Gone(format!("dlna:{udn}"))).is_err() {
                    return;
                }
            }
        }
    }
}

async fn recv(sock: Option<&tokio::net::UdpSocket>, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
    match sock {
        Some(s) => s.recv_from(buf).await.ok(),
        None => std::future::pending().await,
    }
}

/// A socket on 0.0.0.0:1900 in the SSDP group, shared with whatever else listens there
/// (`SO_REUSEADDR`/`SO_REUSEPORT`, as mdns-sd shares 5353); `None` when the system won't.
fn notify_socket() -> Option<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol as SockProto, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(SockProto::UDP)).ok()?;
    s.set_reuse_address(true).ok()?;
    s.set_reuse_port(true).ok()?;
    s.set_nonblocking(true).ok()?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, ssdp::PORT)).into()).ok()?;
    s.join_multicast_v4(&ssdp::GROUP, &Ipv4Addr::UNSPECIFIED).ok()?;
    tokio::net::UdpSocket::from_std(s.into()).ok()
}

/// This computer's private IPv4 address and prefix on the LAN (the interface the default route
/// would use toward the SSDP group).
fn local_subnet() -> Option<(Ipv4Addr, u8)> {
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((Ipv4Addr::new(192, 168, 0, 1), ssdp::PORT)).ok()?;
    let IpAddr::V4(me) = probe.local_addr().ok()?.ip() else { return None };
    if_addrs::get_if_addrs().ok()?.into_iter().find_map(|i| match i.addr {
        if_addrs::IfAddr::V4(v4) if v4.ip == me => Some((me, u32::from(v4.netmask).count_ones() as u8)),
        _ => None,
    })
}

/// Forward resolved `_airplay._tcp` and `_googlecast._tcp` instances until `stop` (or the daemon
/// ends).
fn pump(receivers: &[&mdns_sd::Receiver<mdns_sd::ServiceEvent>], tx: &UnboundedSender<Seen>, stop: &CancellationToken) {
    let mut open = vec![true; receivers.len()];
    while !stop.is_cancelled() && open.contains(&true) {
        // each open receiver gets a share of the poll's wait
        let wait = POLL / open.iter().filter(|o| **o).count().max(1) as u32;
        for (events, open) in receivers.iter().zip(open.iter_mut()).filter(|(_, o)| **o) {
            match events.recv_timeout(wait) {
                Err(mdns_sd::RecvTimeoutError::Disconnected) => *open = false,
                ev => {
                    if !forward(ev, tx) {
                        return;
                    }
                }
            }
        }
    }
}

/// One mDNS event; false when the browse should end (nobody listens any more).
fn forward(ev: Result<mdns_sd::ServiceEvent, mdns_sd::RecvTimeoutError>, tx: &UnboundedSender<Seen>) -> bool {
    use mdns_sd::ServiceEvent;
    {
        match ev {
            Ok(ServiceEvent::ServiceResolved(svc)) => {
                let Some(ip) = svc.addresses.iter().find_map(|a| match a.to_ip_addr() {
                    IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
                    _ => None,
                }) else {
                    return true; // IPv4 only
                };
                let txt: Vec<(String, String)> = svc.txt_properties.iter().map(|p| (p.key().to_owned(), p.val_str().to_owned())).collect();
                let addr = SocketAddr::from((ip, svc.port));
                let found = if svc.ty_domain.eq_ignore_ascii_case(GOOGLECAST_SERVICE) {
                    cast_sighting(&txt, addr, svc.fullname.clone(), false)
                } else {
                    let mut info = features::from_txt(&txt);
                    if info.name.is_none() {
                        // the instance name is the receiver's name ("Living Room._airplay._tcp.local.")
                        info.name = svc.fullname.strip_suffix(&format!(".{AIRPLAY_SERVICE}")).map(str::to_owned);
                    }
                    sighting(&info, addr, svc.fullname.clone(), false)
                };
                if let Some(s) = found {
                    tracing::debug!("cast discovery: found device {}", s.id.short());
                    if tx.send(Seen::Found(s)).is_err() {
                        return false;
                    }
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                if tx.send(Seen::Gone(fullname)).is_err() {
                    return false;
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    true
}

/// Off the desktop: finds nothing, probes nothing (tests never touch the LAN, DESIGN §40 §14.9).
#[derive(Debug, Default)]
pub struct NullDiscovery;

impl Discovery for NullDiscovery {
    fn browse(&self, _rt: &tokio::runtime::Handle, _tx: UnboundedSender<Seen>, _stop: CancellationToken) {}

    fn probe(&self, _ip: Ipv4Addr) -> BoxFuture<'static, Result<Sighting, CastErrorKind>> {
        Box::pin(async { Err(CastErrorKind::Unreachable) })
    }
}

/// Scripted discovery (tests): every browse reports the current sightings at once; probes
/// answer from a table. Counts the browses it was asked for.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct FakeDiscovery {
    state: Mutex<FakeDiscoveryState>,
}

#[doc(hidden)]
#[derive(Debug, Default)]
pub struct FakeDiscoveryState {
    pub sightings: Vec<Sighting>,
    pub probes: HashMap<Ipv4Addr, Result<Sighting, CastErrorKind>>,
    pub browses: usize,
}

impl FakeDiscovery {
    pub fn with<T>(&self, f: impl FnOnce(&mut FakeDiscoveryState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Discovery for FakeDiscovery {
    fn browse(&self, _rt: &tokio::runtime::Handle, tx: UnboundedSender<Seen>, _stop: CancellationToken) {
        let found = self.with(|s| {
            s.browses += 1;
            s.sightings.clone()
        });
        for s in found {
            let _ = tx.send(Seen::Found(s));
        }
    }

    fn probe(&self, ip: Ipv4Addr) -> BoxFuture<'static, Result<Sighting, CastErrorKind>> {
        let answer = self.with(|s| s.probes.get(&ip).cloned()).unwrap_or(Err(CastErrorKind::Unreachable));
        Box::pin(async move { answer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// A Chromecast's record (pychromecast's and Chromium's keys): the id is its UUID however it
    /// is spelled, the name is sanitized, a speaker or a group (no video out) isn't listed.
    #[test]
    fn a_cast_device_from_its_txt_record() {
        let addr: SocketAddr = "192.168.1.50:8009".parse().unwrap();
        let record = txt(&[
            ("id", "8f2e1c0a4b5d6e7f8091a2b3c4d5e6f7"),
            ("fn", "Bedroom TV\u{202E}"),
            ("md", "Chromecast"),
            ("ca", "201221"),
            ("ve", "05"),
            ("rs", "Netflix"),
        ]);
        let s = cast_sighting(&record, addr, "Chromecast-8f2e._googlecast._tcp.local.".into(), false).unwrap();
        assert_eq!(
            (s.kind, s.protocol, s.name.as_str(), s.model.as_deref()),
            (ReceiverKind::Chromecast, Protocol::GoogleCast, "Bedroom TV", Some("Chromecast"))
        );
        assert!(!s.pairing_required && s.usability == Usability::Usable && s.addr == addr);
        let dashed = txt(&[("id", "8F2E1C0A-4B5D-6E7F-8091-A2B3C4D5E6F7"), ("ca", "5"), ("fn", "x")]);
        assert_eq!(cast_sighting(&dashed, addr, "k".into(), true).unwrap().id, s.id, "the same device found by address");
        let speaker = txt(&[("id", "aa"), ("fn", "Kitchen speaker"), ("md", "Google Home"), ("ca", "199172")]);
        assert_eq!(cast_sighting(&speaker, addr, "k".into(), false), None, "no video out (bit 0)");
        assert_eq!(cast_sighting(&txt(&[("fn", "no id"), ("ca", "5")]), addr, "k".into(), false), None);
        let unnamed = cast_sighting(&txt(&[("id", "bb"), ("ca", "1"), ("fn", "http://evil/")]), addr, "k".into(), false).unwrap();
        assert_eq!(unnamed.name, "Google Cast");
    }

    /// A DLNA renderer by its UDN, at the host its description is on; its control URLs go with
    /// it (crate only).
    #[test]
    fn a_dlna_renderer_from_its_description() {
        let location = url::Url::parse("http://192.168.1.40:9197/dmr").unwrap();
        let r = description::Renderer {
            name: "[TV] Samsung".into(),
            manufacturer: Some("Samsung Electronics".into()),
            model: Some("QE55Q60R".into()),
            udn: "uuid:0BF4".into(),
            av_transport: location.join("/upnp/control/AVTransport1").unwrap(),
            rendering: None,
        };
        let s = dlna_sighting(&r, &location, false).unwrap();
        assert_eq!((s.kind, s.protocol, s.key.as_str()), (ReceiverKind::Dlna, Protocol::Dlna, "dlna:uuid:0BF4"));
        assert_eq!(s.id, DeviceId::derive("dlna", "uuid:0bf4"));
        assert_eq!(s.addr, "192.168.1.40:9197".parse().unwrap());
        assert_eq!(s.dlna.unwrap().av_transport.as_str(), "http://192.168.1.40:9197/upnp/control/AVTransport1");
        let named = url::Url::parse("http://tv.local:9197/dmr").unwrap();
        assert_eq!(dlna_sighting(&r, &named, false), None, "IPv4 addresses only");
    }
}
