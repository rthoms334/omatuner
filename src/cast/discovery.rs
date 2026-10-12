//! Discovery (DESIGN §40 §6): a `Discovery` trait with `MdnsDiscovery` (mdns-sd `=0.21.4`,
//! `_airplay._tcp.local.`, on demand only), `NullDiscovery` (off the desktop: tests never touch
//! the LAN) and `FakeDiscovery` (tests); plus the manual address probe (`GET /info` on 7000).
//! OWNER: CAST-6.

// CAST-6
