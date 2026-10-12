//! Discovery (DESIGN §40 §6): a `Discovery` trait with `MdnsDiscovery` (mdns-sd `=0.21.4`,
//! `_airplay._tcp.local.`, on demand only), `NullDiscovery` (off the desktop: tests never touch
//! the LAN) and `FakeDiscovery` (tests); plus the manual address probe (`GET /info` on 7000).
//! OWNER: CAST-6.
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
use super::{CastErrorKind, DeviceId, Protocol, ReceiverKind, Usability, sanitize_name};

/// The AirPlay service type (Phase 2 adds `_googlecast._tcp`).
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
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
    /// "26.6 (23L773)".
    pub os_version: Option<String>,
    /// "960.13.1".
    pub source_version: Option<String>,
    pub pairing_required: bool,
    pub usability: Usability,
    pub experimental: bool,
    pub manual: bool,
    pub addr: SocketAddr,
}

/// What a browse reports.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    let os_version = match (clean(&info.os_version), clean(&info.os_build)) {
        (Some(v), Some(b)) => Some(format!("{v} ({b})")),
        (v, b) => v.or(b),
    };
    Some(Sighting {
        id,
        key,
        name: sanitize_name(info.name.as_deref().unwrap_or_default(), FALLBACK_NAME),
        kind: class.kind,
        protocol: Protocol::AirPlay,
        model: clean(&info.model),
        os_version,
        source_version: clean(&info.source_version),
        pairing_required: class.pairing_required,
        usability: class.usability,
        experimental: class.experimental,
        manual,
        addr,
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
        // the daemon's receiver blocks: the loop runs on the blocking pool, only while browsing
        rt.spawn_blocking(move || {
            let daemon = match mdns_sd::ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::info!("cast discovery: mDNS unavailable: {e}");
                    return;
                }
            };
            match daemon.browse(AIRPLAY_SERVICE) {
                Ok(events) => pump(&events, &tx, &stop),
                Err(e) => tracing::info!("cast discovery: browse failed: {e}"),
            }
            let _ = daemon.stop_browse(AIRPLAY_SERVICE);
            let _ = daemon.shutdown();
        });
    }

    fn probe(&self, ip: Ipv4Addr) -> BoxFuture<'static, Result<Sighting, CastErrorKind>> {
        Box::pin(async move {
            let addr = SocketAddr::from((ip, airplay::CONTROL_PORT));
            let info = airplay::probe_info(addr).await?;
            // a receiver that isn't a video TV is "unsupported", not "unreachable": it answered
            sighting(&info, addr, format!("manual:{ip}"), true).ok_or(CastErrorKind::Unsupported)
        })
    }
}

/// Forward resolved `_airplay._tcp` instances until `stop` (or the daemon ends).
fn pump(events: &mdns_sd::Receiver<mdns_sd::ServiceEvent>, tx: &UnboundedSender<Seen>, stop: &CancellationToken) {
    use mdns_sd::ServiceEvent;
    while !stop.is_cancelled() {
        match events.recv_timeout(POLL) {
            Ok(ServiceEvent::ServiceResolved(svc)) => {
                let Some(ip) = svc.addresses.iter().find_map(|a| match a.to_ip_addr() {
                    IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
                    _ => None,
                }) else {
                    continue; // IPv4 only in Phase 1
                };
                let txt: Vec<(String, String)> = svc.txt_properties.iter().map(|p| (p.key().to_owned(), p.val_str().to_owned())).collect();
                let mut info = features::from_txt(&txt);
                if info.name.is_none() {
                    // the instance name is the receiver's name ("Living Room._airplay._tcp.local.")
                    info.name = svc.fullname.strip_suffix(&format!(".{AIRPLAY_SERVICE}")).map(str::to_owned);
                }
                if let Some(s) = sighting(&info, SocketAddr::from((ip, svc.port)), svc.fullname.clone(), false) {
                    tracing::debug!("cast discovery: found device {}", s.id.short());
                    if tx.send(Seen::Found(s)).is_err() {
                        return;
                    }
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                if tx.send(Seen::Gone(fullname)).is_err() {
                    return;
                }
            }
            Ok(_) | Err(mdns_sd::RecvTimeoutError::Timeout) => {}
            Err(mdns_sd::RecvTimeoutError::Disconnected) => return,
        }
    }
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
