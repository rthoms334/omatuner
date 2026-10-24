//! The merged device list (DESIGN §40 §6): stable `DeviceId`s, last seen, versions (the TV's
//! current tvOS for Settings), paired devices from the store, addresses private to the crate.
//! OWNER: CAST-6.
//!
//! Pure bookkeeping (no I/O): the service feeds it the store's rows at start, each discovery
//! [`Sighting`], what a session learned from `/info`, pairings and forgets, and reads it every
//! frame (`CastApi::devices`), for the agent's `cast_devices` and for the first-run card's
//! offer. A remembered TV that this run hasn't seen yet is listed as not available; a sighting
//! makes it available and refreshes its address, name and versions (a TV that updated to tvOS
//! 27 overnight shows 27 in Settings ▸ Cast). One physical TV that speaks several protocols (an
//! LG with AirPlay and DLNA, a Sony with Cast and DLNA) is listed once: the devices at one
//! address are one TV, shown as the one used before, else the one paired, else by protocol
//! (Cast > DLNA > third-party AirPlay; DESIGN §40 §6 "Merging").

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use super::discovery::Sighting;
use super::receiver::ReceiverInfo;
use super::{Device, DeviceId, Pairing, PeerCredentials, Protocol, ReceiverKind, SetupOffer, Usability, sanitize_name};
use crate::model::Timestamp;
use crate::redact::Secret;
use crate::store::cast::StoredDevice;

/// One device and what only the crate sees.
struct Entry {
    device: Device,
    source_version: Option<String>,
    manual: bool,
    /// When this run's discovery last saw it.
    seen: Option<Instant>,
    /// The stored pairing (credentials: never printed).
    peer: Option<Secret<PeerCredentials>>,
}

/// Every known receiver.
#[derive(Default)]
pub struct Registry {
    entries: HashMap<DeviceId, Entry>,
    /// A sighting's key (mDNS instance) → its device, for `Gone`.
    keys: HashMap<String, DeviceId>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("devices", &self.entries.len()).finish_non_exhaustive()
    }
}

/// Which of one TV's devices is its row (lowest first): one used before, then one paired, then
/// Apple's own receivers and Google Cast, DLNA, a third-party AirPlay TV.
fn merge_rank(d: &Device) -> (bool, bool, u8) {
    let protocol = match d.kind {
        ReceiverKind::AppleTv | ReceiverKind::Mac | ReceiverKind::Chromecast => 0,
        ReceiverKind::Dlna => 1,
        ReceiverKind::AirPlayTv => 2,
    };
    (d.last_used.is_none(), !matches!(d.pairing, Pairing::Paired(_)), protocol)
}

/// The kind of a stored device (the table keeps the model, not the kind): an Apple TV by its
/// model (VioletRelay's detection, §7.7), any other AirPlay receiver is a third-party TV.
fn stored_kind(protocol: Protocol, model: Option<&str>) -> ReceiverKind {
    match protocol {
        Protocol::AirPlay if model.is_some_and(|m| m.starts_with("AppleTV")) => ReceiverKind::AppleTv,
        Protocol::AirPlay if model.is_some_and(crate::cast::airplay::features::is_mac_model) => ReceiverKind::Mac,
        Protocol::AirPlay => ReceiverKind::AirPlayTv,
        Protocol::GoogleCast => ReceiverKind::Chromecast,
        Protocol::Dlna => ReceiverKind::Dlna,
    }
}

impl Registry {
    /// Merge the store's rows (at service start). A device this run already saw keeps its live
    /// state; the stored pairing and last use are adopted either way.
    pub fn load(&mut self, rows: Vec<StoredDevice>) {
        for row in rows {
            let pairing = row.peer.as_ref().map(|p| Pairing::Paired(p.expose().variant));
            if let Some(e) = self.entries.get_mut(&row.id) {
                if let Some(p) = pairing {
                    e.device.pairing = p;
                }
                e.device.last_used = e.device.last_used.max(row.last_used);
                e.peer = e.peer.take().or(row.peer);
                continue;
            }
            let kind = stored_kind(row.protocol, row.model.as_deref());
            let mut device = Device::new(row.id, &row.name, kind, row.protocol);
            device.model = row.model;
            device.os_version = row.os_version;
            // only AirPlay pairs (and a Mac pairs transiently, each session)
            let needs_pin = row.protocol == Protocol::AirPlay && kind != ReceiverKind::Mac;
            device.pairing = pairing.unwrap_or(if needs_pin { Pairing::Required } else { Pairing::NotNeeded });
            device.available = false;
            device.last_used = row.last_used;
            device.addr = row.last_addr;
            self.entries.insert(row.id, Entry { device, source_version: row.source_version, manual: row.manual, seen: None, peer: row.peer });
        }
    }

    /// Discovery (or a manual probe) saw a receiver. True when what the store keeps changed
    /// (new, renamed, moved, updated): the service then writes the row.
    pub fn found(&mut self, s: Sighting, at: Instant) -> bool {
        self.keys.insert(s.key.clone(), s.id);
        let e = self.entries.entry(s.id).or_insert_with(|| Entry {
            device: Device::new(s.id, &s.name, s.kind, s.protocol),
            source_version: None,
            manual: s.manual,
            seen: None,
            peer: None,
        });
        let before = (e.device.name.clone(), e.device.model.clone(), e.device.os_version.clone(), e.source_version.clone(), e.device.addr, e.manual);
        let d = &mut e.device;
        d.name = sanitize_name(&s.name, super::discovery::FALLBACK_NAME);
        d.kind = s.kind;
        d.model = s.model.or(d.model.take());
        d.manufacturer = s.manufacturer.or(d.manufacturer.take());
        d.os_version = crate::cast::merge_os_version(d.os_version.take(), s.os_version);
        d.usability = s.usability;
        d.available = true;
        d.addr = Some(s.addr);
        if !s.ids.is_empty() {
            d.ids = s.ids;
        }
        if s.dlna.is_some() {
            d.dlna = s.dlna;
        }
        if !matches!(d.pairing, Pairing::Paired(_)) {
            d.pairing = if s.pairing_required { Pairing::Required } else { Pairing::NotNeeded };
        }
        e.source_version = s.source_version.or(e.source_version.take());
        e.manual = s.manual;
        let first = e.seen.is_none();
        e.seen = Some(at);
        first || before != (d.name.clone(), d.model.clone(), d.os_version.clone(), e.source_version.clone(), d.addr, e.manual)
    }

    /// The mDNS instance left (a manual device never leaves this way).
    pub fn gone(&mut self, key: &str) {
        if let Some(e) = self.keys.remove(key).and_then(|id| self.entries.get_mut(&id)) {
            e.device.available = false;
        }
    }

    /// The list the picker, Settings and the agent show, every kind alike: most recently used
    /// first, then the ones on the network, then by name; one row per TV (module docs).
    pub fn list(&self) -> Vec<Device> {
        let mut out: Vec<Device> = self.entries.values().map(|e| e.device.clone()).collect();
        // the row kept for a TV with several protocols sorts first among them
        out.sort_by_key(merge_rank);
        let mut seen = std::collections::HashSet::new();
        out.retain(|d| match d.addr.filter(|_| d.available) {
            Some(a) => seen.insert(a.ip()),
            None => true,
        });
        out.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(b.available.cmp(&a.available)).then_with(|| a.name.cmp(&b.name)).then(a.id.cmp(&b.id)));
        out
    }

    pub fn get(&self, id: DeviceId) -> Option<&Device> {
        self.entries.get(&id).map(|e| &e.device)
    }

    /// AirPlay `srcvers` / `sourceVersion` (`diagnose`).
    pub fn source_version(&self, id: DeviceId) -> Option<String> {
        self.entries.get(&id).and_then(|e| e.source_version.clone())
    }

    /// Added by address (no mDNS refresh before a cast: it isn't announced).
    pub fn manual(&self, id: DeviceId) -> bool {
        self.entries.get(&id).is_some_and(|e| e.manual)
    }

    /// The stored pairing, for a session's pair-verify.
    pub(crate) fn peer(&self, id: DeviceId) -> Option<PeerCredentials> {
        self.entries.get(&id).and_then(|e| e.peer.as_ref()).map(|p| p.expose().clone())
    }

    /// Seen by discovery at or after `since` (its address is current).
    pub fn seen_since(&self, id: DeviceId, since: Instant) -> bool {
        self.entries.get(&id).and_then(|e| e.seen).is_some_and(|t| t >= since)
    }

    /// Pairing finished.
    pub fn paired(&mut self, id: DeviceId, peer: PeerCredentials) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.device.pairing = Pairing::Paired(peer.variant);
            e.peer = Some(Secret::new(peer));
        }
    }

    pub fn used(&mut self, id: DeviceId, at: Timestamp) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.device.last_used = Some(at);
        }
    }

    /// What a session's `/info` said (the version Settings shows; DESIGN §40 §7.6).
    pub fn learned(&mut self, id: DeviceId, info: &ReceiverInfo) {
        if let Some(e) = self.entries.get_mut(&id) {
            let clean = |s: &Option<String>| s.as_deref().map(|v| sanitize_name(v, "")).filter(|v| !v.is_empty());
            e.device.model = clean(&info.model).or(e.device.model.take());
            e.device.os_version = crate::cast::merge_os_version(e.device.os_version.take(), clean(&info.os_version));
            e.source_version = clean(&info.source_version).or(e.source_version.take());
        }
    }

    pub fn forget(&mut self, id: DeviceId) {
        self.entries.remove(&id);
        self.keys.retain(|_, v| *v != id);
    }

    /// The row the store keeps for `id` (discovery columns only).
    pub fn stored(&self, id: DeviceId, now: Timestamp) -> Option<StoredDevice> {
        let e = self.entries.get(&id)?;
        let d = &e.device;
        Some(StoredDevice {
            model: d.model.clone(),
            os_version: d.os_version.clone(),
            source_version: e.source_version.clone(),
            manual: e.manual,
            last_addr: d.addr,
            last_seen: e.seen.map(|_| now),
            ..StoredDevice::seen(id, d.protocol, &d.name)
        })
    }

    /// The first-run card's offer (DESIGN §40 §3.5): the first usable, unpaired AirPlay video
    /// receiver on the network now, an Apple TV or a third-party AirPlay TV.
    pub fn offer(&self) -> Option<SetupOffer> {
        self.list()
            .into_iter()
            .find(|d| {
                // a Mac pairs by itself each time: nothing to set up
                d.available
                    && d.protocol == Protocol::AirPlay
                    && d.kind != ReceiverKind::Mac
                    && d.usability == Usability::Usable
                    && !matches!(d.pairing, Pairing::Paired(_))
            })
            .map(|d| SetupOffer { device: d.id, name: d.name, kind: d.kind })
    }

    /// The address to reach `id` at (the latest sighting, else the stored one).
    pub(crate) fn addr(&self, id: DeviceId) -> Option<SocketAddr> {
        self.entries.get(&id).and_then(|e| e.device.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::PairVariant;

    fn sighting(stable: &str, name: &str, kind: ReceiverKind) -> Sighting {
        Sighting {
            id: DeviceId::derive("airplay", stable),
            key: format!("{name}._airplay._tcp.local."),
            name: name.into(),
            kind,
            protocol: Protocol::AirPlay,
            model: Some(if kind == ReceiverKind::AppleTv { "AppleTV14,1" } else { "85E75LE" }.into()),
            manufacturer: (kind == ReceiverKind::AirPlayTv).then(|| "Hisense".into()),
            os_version: Some("26.6 (23L773)".into()),
            source_version: Some("960.13.1".into()),
            pairing_required: true,
            usability: Usability::Usable,
            manual: false,
            // one address per TV (a shared one is one TV with several protocols)
            addr: std::net::SocketAddr::from(([192, 168, 1, stable.bytes().fold(0u8, |a, b| a.wrapping_add(b))], 7000)),
            ids: Default::default(),
            dlna: None,
        }
    }

    /// One TV seen over AirPlay (a Hisense, third-party) and DLNA at one address is one row:
    /// DLNA's, until the AirPlay one is paired or used; a Google Cast device beats DLNA; another
    /// TV isn't touched, nor a TV that isn't on the network.
    #[test]
    fn one_tv_with_several_protocols_is_one_row() {
        let at = |s: Sighting, ip: [u8; 4]| Sighting { addr: std::net::SocketAddr::from((ip, 7000)), ..s };
        let airplay = at(sighting("pi-lg", "Kitchen TV", ReceiverKind::AirPlayTv), [192, 168, 1, 40]);
        let dlna = Sighting {
            id: DeviceId::derive("dlna", "uuid:lg"),
            key: "dlna:uuid:lg".into(),
            kind: ReceiverKind::Dlna,
            protocol: Protocol::Dlna,
            pairing_required: false,
            ..at(sighting("x", "[LG] webOS TV", ReceiverKind::AirPlayTv), [192, 168, 1, 40])
        };
        let cast = Sighting {
            id: DeviceId::derive("googlecast", "abc"),
            kind: ReceiverKind::Chromecast,
            protocol: Protocol::GoogleCast,
            ..at(sighting("y", "Kitchen TV (Cast)", ReceiverKind::AirPlayTv), [192, 168, 1, 40])
        };
        let other = at(sighting("pi-den", "Den", ReceiverKind::AppleTv), [192, 168, 1, 41]);
        let mut r = Registry::default();
        let t0 = Instant::now();
        for s in [airplay.clone(), dlna.clone(), other.clone()] {
            r.found(s, t0);
        }
        let ids = |r: &Registry| r.list().into_iter().map(|d| d.id).collect::<Vec<_>>();
        assert_eq!(ids(&r), [other.id, dlna.id], "DLNA over a third-party AirPlay TV");
        r.paired(airplay.id, PeerCredentials { peer_id: "lg".into(), peer_ltpk: [1; 32], variant: PairVariant::Hkp3 });
        assert_eq!(ids(&r), [other.id, airplay.id], "a paired AirPlay TV keeps its row");
        r.found(cast.clone(), t0);
        assert_eq!(ids(&r), [other.id, airplay.id], "paired still wins");
        r.used(cast.id, 5);
        assert_eq!(ids(&r), [cast.id, other.id], "the one used before, listed first");
        r.gone(&cast.key);
        assert_eq!(ids(&r).len(), 3, "a TV not on the network merges with nothing");
    }

    /// A paired TV from the store stays paired when discovery finds it again, becomes available
    /// with its new version, and is listed first (most recently used); a third-party TV is
    /// listed like the Apple TVs; the first-run card offers an unpaired TV on the network, the
    /// third-party one too.
    #[test]
    fn stored_and_discovered_devices_merge() {
        let tv = sighting("pi-tv", "Living Room", ReceiverKind::AppleTv);
        let other = sighting("pi-den", "Den", ReceiverKind::AppleTv);
        let hisense = sighting("pi-hisense", "Living Room TV", ReceiverKind::AirPlayTv);
        let mut stored = StoredDevice::seen(tv.id, Protocol::AirPlay, "Living Room");
        stored.model = Some("AppleTV14,1".into());
        stored.os_version = Some("26.5".into());
        stored.last_used = Some(100);
        stored.peer = Some(Secret::new(PeerCredentials { peer_id: "tv".into(), peer_ltpk: [7; 32], variant: PairVariant::Hkp5 }));

        let mut r = Registry::default();
        r.load(vec![stored]);
        assert!(!r.get(tv.id).unwrap().available, "remembered, not seen yet");
        assert_eq!(r.get(tv.id).unwrap().kind, ReceiverKind::AppleTv);

        let t0 = Instant::now();
        assert!(r.found(tv.clone(), t0), "first sighting this run: the row is written");
        assert!(!r.found(tv.clone(), t0), "nothing changed");
        r.found(other.clone(), t0);
        r.found(hisense.clone(), t0);
        let d = r.get(tv.id).unwrap();
        assert_eq!((d.pairing, d.available, d.os_version.as_deref()), (Pairing::Paired(PairVariant::Hkp5), true, Some("26.6 (23L773)")));
        assert_eq!(r.peer(tv.id).map(|p| p.peer_id), Some("tv".into()));

        let names = r.list().into_iter().map(|d| d.name).collect::<Vec<_>>();
        assert_eq!(names, ["Living Room", "Den", "Living Room TV"]);
        let makes = r.list().iter().map(Device::make).collect::<Vec<_>>();
        assert_eq!(makes, ["Apple TV 4K (3rd generation)", "Apple TV 4K (3rd generation)", "Hisense 85E75LE"], "the maker from discovery");
        assert_eq!(r.offer().map(|o| o.device), Some(other.id), "unpaired, the first in the list");

        r.gone(&other.key);
        assert!(!r.get(other.id).unwrap().available);
        assert_eq!(r.offer().map(|o| (o.device, o.kind)), Some((hisense.id, ReceiverKind::AirPlayTv)), "offered like an Apple TV");
        r.gone(&hisense.key);
        assert_eq!(r.offer(), None, "not on the network");
        r.forget(tv.id);
        assert!(r.get(tv.id).is_none() && r.peer(tv.id).is_none());
    }
}
