//! Casting rows (DESIGN §40 §7.5, §13): `cast_identity`, `cast_device`, `cast_hint` (migration
//! 0014). OWNER: CAST-6. Free functions over `&Connection` like every repository module; writes
//! go through the `db-writer` (`Writer::submit` closures). The identity seed and the peer keys
//! are credentials: they leave this module only as `redact::Secret`s, never in a `Debug` print,
//! a log line or an agent reply. None of these tables is exported by `backup.rs`.
//!
//! * `cast_identity`: one long-term pairing identity per install, created by the first pairing
//!   ([`ensure_identity`]): a UUID pairing id (pyatv's `str(uuid.uuid4())`), a 32-byte Ed25519
//!   seed and a random, locally administered sender MAC (SETUP `deviceID`, fallback
//!   `channelID`; §7.3).
//! * `cast_device`: what discovery last saw ([`upsert_device`]: names, models and versions are
//!   sanitized device strings; `last_addr` stays inside the cast module) plus the pairing
//!   ([`set_paired`]) and the last cast ([`mark_used`]). [`forget`] removes a TV with its hints.
//! * `cast_hint`: what worked for an item on a TV (§8.2–8.3): the level, the codecs, whether it
//!   is interlaced, the playlist guard's "irregular" mark and the user's **Convert this
//!   channel** (`forced`). Codecs and the cached level count for [`HINT_FRESH_S`]; after that
//!   the relay probes again. `forced` always counts.

use std::net::SocketAddr;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::cast::{Identity, PairVariant, PeerCredentials, Protocol};
use crate::error::{Error, Result};
use crate::model::{CastHints, CastLevel, DeviceId, ItemKind, ItemRef, Timestamp};
use crate::redact::Secret;
use crate::store::sql::{enum_parse, enum_str};

/// A cached level or codec is re-probed after this long (DESIGN §40 §8.2: "less than 30 days
/// old").
pub const HINT_FRESH_S: i64 = 30 * 86_400;

// ------------------------------------------------------------------ identity

/// This install's identity, if a pairing created one.
pub fn identity(conn: &Connection) -> Result<Option<Identity>> {
    let row = conn
        .query_row("SELECT pairing_id, seed, sender_mac FROM cast_identity WHERE id = 1", [], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?))
        })
        .optional()?;
    let Some((pairing_id, seed, mac)) = row else { return Ok(None) };
    let seed: [u8; 32] = seed.try_into().map_err(|_| Error::Other("cast identity: bad seed length".into()))?;
    let sender_mac = parse_mac(&mac).ok_or_else(|| Error::Other("cast identity: bad sender MAC".into()))?;
    Ok(Some(Identity { pairing_id, seed: Secret::new(seed), sender_mac }))
}

/// The identity, created on first use (once per install; concurrent callers get the same row).
pub fn ensure_identity(conn: &Connection, now: Timestamp) -> Result<Identity> {
    if let Some(id) = identity(conn)? {
        return Ok(id);
    }
    let mut seed = [0u8; 32];
    let mut uuid = [0u8; 16];
    let mut mac = [0u8; 6];
    for buf in [&mut seed[..], &mut uuid[..], &mut mac[..]] {
        aws_lc_rs::rand::fill(buf).map_err(|_| Error::Other("cast identity: no system randomness".into()))?;
    }
    // locally administered (bit 1), unicast (bit 0 clear): never a real vendor's address
    mac[0] = (mac[0] | 0x02) & 0xfe;
    conn.execute(
        "INSERT INTO cast_identity(id, pairing_id, seed, sender_mac, created_at) VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO NOTHING",
        params![uuid_v4(uuid), &seed[..], format_mac(&mac), now],
    )?;
    seed.fill(0);
    std::hint::black_box(&seed);
    identity(conn)?.ok_or_else(|| Error::Other("cast identity: not stored".into()))
}

/// A random (version 4, RFC 4122 variant) UUID in pyatv's lower-case `str(uuid.uuid4())` form.
fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// "02:AB:CD:EF:01:23".
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut out {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

// ------------------------------------------------------------------ devices

/// One `cast_device` row.
#[derive(Clone, PartialEq)]
pub struct StoredDevice {
    pub id: DeviceId,
    pub protocol: Protocol,
    /// Sanitized device strings.
    pub name: String,
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub source_version: Option<String>,
    /// Added by address, not found by mDNS.
    pub manual: bool,
    /// Private to the cast module.
    pub last_addr: Option<SocketAddr>,
    pub last_seen: Option<Timestamp>,
    /// The pairing (`None`: not paired).
    pub peer: Option<Secret<PeerCredentials>>,
    pub paired_at: Option<Timestamp>,
    pub last_used: Option<Timestamp>,
}

impl std::fmt::Debug for StoredDevice {
    /// No address, no keys.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredDevice")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("paired", &self.peer.is_some())
            .finish_non_exhaustive()
    }
}

impl StoredDevice {
    /// A device as discovery saw it (no pairing, never used).
    pub fn seen(id: DeviceId, protocol: Protocol, name: &str) -> Self {
        Self {
            id,
            protocol,
            name: name.to_owned(),
            model: None,
            os_version: None,
            source_version: None,
            manual: false,
            last_addr: None,
            last_seen: None,
            peer: None,
            paired_at: None,
            last_used: None,
        }
    }
}

const DEVICE_COLS: &str =
    "id, protocol, name, model, os_version, source_version, manual, last_addr, last_seen, peer_id, peer_ltpk, pair_kind, paired_at, last_used";

fn device_from_row(r: &Row<'_>) -> rusqlite::Result<Option<StoredDevice>> {
    let id: String = r.get(0)?;
    let protocol: String = r.get(1)?;
    let (Ok(id), Some(protocol)) = (id.parse::<DeviceId>(), protocol_of(&protocol)) else { return Ok(None) };
    let peer = match (r.get::<_, Option<String>>(9)?, r.get::<_, Option<Vec<u8>>>(10)?, r.get::<_, Option<String>>(11)?) {
        (Some(peer_id), Some(key), kind) => key.try_into().ok().map(|peer_ltpk| {
            let variant = kind.as_deref().and_then(variant_of).unwrap_or(PairVariant::Hkp3);
            Secret::new(PeerCredentials { peer_id, peer_ltpk, variant })
        }),
        _ => None,
    };
    Ok(Some(StoredDevice {
        id,
        protocol,
        name: r.get(2)?,
        model: r.get(3)?,
        os_version: r.get(4)?,
        source_version: r.get(5)?,
        manual: r.get(6)?,
        last_addr: r.get::<_, Option<String>>(7)?.and_then(|a| a.parse().ok()),
        last_seen: r.get(8)?,
        peer,
        paired_at: r.get(12)?,
        last_used: r.get(13)?,
    }))
}

fn protocol_of(s: &str) -> Option<Protocol> {
    [Protocol::AirPlay, Protocol::GoogleCast, Protocol::Dlna].into_iter().find(|p| p.as_str() == s)
}

fn variant_of(s: &str) -> Option<PairVariant> {
    [PairVariant::Hkp3, PairVariant::Hkp5].into_iter().find(|v| v.as_str() == s)
}

/// Every stored device, most recently used first (rows this version can't read are skipped).
pub fn devices(conn: &Connection) -> Result<Vec<StoredDevice>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {DEVICE_COLS} FROM cast_device ORDER BY last_used DESC NULLS LAST, name"))?;
    let rows = stmt.query_map([], device_from_row)?;
    Ok(rows.filter_map(|r| r.transpose()).collect::<rusqlite::Result<_>>()?)
}

pub fn device(conn: &Connection, id: DeviceId) -> Result<Option<StoredDevice>> {
    Ok(conn.query_row(&format!("SELECT {DEVICE_COLS} FROM cast_device WHERE id = ?1"), [id.to_string()], device_from_row).optional()?.flatten())
}

/// Record what discovery (or "Add by address…", or a session's `/info`) saw. The pairing and
/// `last_used` are kept; a missing model, version, address or time keeps the stored one.
pub fn upsert_device(conn: &Connection, d: &StoredDevice) -> Result<()> {
    conn.execute(
        "INSERT INTO cast_device(id, protocol, name, model, os_version, source_version, manual, last_addr, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           name = excluded.name,
           model = COALESCE(excluded.model, model),
           os_version = COALESCE(excluded.os_version, os_version),
           source_version = COALESCE(excluded.source_version, source_version),
           manual = excluded.manual,
           last_addr = COALESCE(excluded.last_addr, last_addr),
           last_seen = COALESCE(excluded.last_seen, last_seen)",
        params![
            d.id.to_string(),
            d.protocol.as_str(),
            d.name,
            d.model,
            d.os_version,
            d.source_version,
            d.manual,
            d.last_addr.map(|a| a.to_string()),
            d.last_seen,
        ],
    )?;
    Ok(())
}

/// Store a pairing (pair-setup's M6 verified). The device row must exist.
pub fn set_paired(conn: &Connection, id: DeviceId, peer: &PeerCredentials, now: Timestamp) -> Result<()> {
    let n = conn.execute(
        "UPDATE cast_device SET peer_id = ?2, peer_ltpk = ?3, pair_kind = ?4, paired_at = ?5 WHERE id = ?1",
        params![id.to_string(), peer.peer_id, &peer.peer_ltpk[..], peer.variant.as_str(), now],
    )?;
    if n == 0 {
        return Err(crate::store::sql::not_found("cast device", id.short()));
    }
    Ok(())
}

/// A cast to `id` started (the picker lists the most recent first).
pub fn mark_used(conn: &Connection, id: DeviceId, now: Timestamp) -> Result<()> {
    conn.execute("UPDATE cast_device SET last_used = ?2 WHERE id = ?1", params![id.to_string(), now])?;
    Ok(())
}

/// Forget a TV: its pairing and its hints go too.
pub fn forget(conn: &Connection, id: DeviceId) -> Result<()> {
    let id = id.to_string();
    conn.execute("DELETE FROM cast_hint WHERE device = ?1", [&id])?;
    conn.execute("DELETE FROM cast_device WHERE id = ?1", [&id])?;
    Ok(())
}

// ------------------------------------------------------------------ hints

/// What worked for an item on a TV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub level: Option<CastLevel>,
    /// The user's **Convert this channel**.
    pub forced: bool,
    pub interlaced: Option<bool>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    /// The playlist guard tripped (L3 next time).
    pub irregular: bool,
    pub updated_at: Timestamp,
}

impl Hint {
    fn fresh(&self, now: Timestamp) -> bool {
        now - self.updated_at < HINT_FRESH_S
    }

    /// The level a start must use (DESIGN §40 §8.2 items 2–3): the user's override always, a
    /// fresh cache's L3 (an earlier escalation, or an irregular playlist).
    pub fn force(&self, now: Timestamp) -> Option<CastLevel> {
        let cached = self.fresh(now) && (self.irregular || self.level == Some(CastLevel::Converted));
        (self.forced || cached).then_some(CastLevel::Converted)
    }

    /// Fill what `hints` doesn't know from a fresh row (§8.2 item 3; mpv's own view of the item
    /// that moves to the TV wins).
    pub fn fill(&self, hints: &mut CastHints, now: Timestamp) {
        if !self.fresh(now) {
            return;
        }
        if hints.vcodec.is_none() {
            hints.vcodec.clone_from(&self.vcodec);
        }
        if hints.acodec.is_none() {
            hints.acodec.clone_from(&self.acodec);
        }
        if hints.interlaced.is_none() {
            hints.interlaced = self.interlaced;
        }
    }
}

/// The hint's `kind` column: series aren't played (their episodes are).
fn hint_kind(kind: ItemKind) -> Option<&'static str> {
    match kind {
        ItemKind::Channel => Some("channel"),
        ItemKind::Movie => Some("movie"),
        ItemKind::Episode => Some("episode"),
        ItemKind::Series => None,
    }
}

pub fn hint(conn: &Connection, item: &ItemRef, device: DeviceId) -> Result<Option<Hint>> {
    let Some(kind) = hint_kind(item.kind) else { return Ok(None) };
    Ok(conn
        .query_row(
            "SELECT level, forced, interlaced, vcodec, acodec, irregular, updated_at FROM cast_hint
             WHERE source_id = ?1 AND kind = ?2 AND item_key = ?3 AND device = ?4",
            params![item.source, kind, item.key.as_str(), device.to_string()],
            |r| {
                Ok(Hint {
                    level: r.get::<_, Option<String>>(0)?.and_then(|l| enum_parse(&l)),
                    forced: r.get(1)?,
                    interlaced: r.get(2)?,
                    vcodec: r.get(3)?,
                    acodec: r.get(4)?,
                    irregular: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// `level` played on `device` (the first picture): cache it with the item's codecs. `forced`
/// and `irregular` are kept.
pub fn save_hint(conn: &Connection, item: &ItemRef, device: DeviceId, level: CastLevel, hints: &CastHints, now: Timestamp) -> Result<()> {
    let Some(kind) = hint_kind(item.kind) else { return Ok(()) };
    conn.execute(
        "INSERT INTO cast_hint(source_id, kind, item_key, device, level, interlaced, vcodec, acodec, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(source_id, kind, item_key, device) DO UPDATE SET
           level = excluded.level,
           interlaced = COALESCE(excluded.interlaced, interlaced),
           vcodec = COALESCE(excluded.vcodec, vcodec),
           acodec = COALESCE(excluded.acodec, acodec),
           updated_at = excluded.updated_at",
        params![item.source, kind, item.key.as_str(), device.to_string(), enum_str(&level), hints.interlaced, hints.vcodec, hints.acodec, now],
    )?;
    Ok(())
}

/// **Convert this channel**: L3 for this item on this TV from now on.
pub fn force_convert(conn: &Connection, item: &ItemRef, device: DeviceId, now: Timestamp) -> Result<()> {
    flag(conn, item, device, "forced", now)
}

/// The playlist guard tripped twice in a session (§8.1 item 4): the next start uses L3.
pub fn mark_irregular(conn: &Connection, item: &ItemRef, device: DeviceId, now: Timestamp) -> Result<()> {
    flag(conn, item, device, "irregular", now)
}

fn flag(conn: &Connection, item: &ItemRef, device: DeviceId, column: &str, now: Timestamp) -> Result<()> {
    let Some(kind) = hint_kind(item.kind) else { return Ok(()) };
    conn.execute(
        &format!(
            "INSERT INTO cast_hint(source_id, kind, item_key, device, {column}, updated_at) VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(source_id, kind, item_key, device) DO UPDATE SET {column} = 1, updated_at = excluded.updated_at"
        ),
        params![item.source, kind, item.key.as_str(), device.to_string(), now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_strings_have_their_wire_shapes() {
        let u = uuid_v4([0xff; 16]);
        assert_eq!(u, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(parse_mac(&format_mac(&[0x02, 0xab, 0, 1, 2, 3])), Some([0x02, 0xab, 0, 1, 2, 3]));
        assert_eq!(parse_mac("02:AB:00:01:02"), None);
    }

    #[test]
    fn a_hint_forces_l3_only_when_the_user_asked_or_a_fresh_cache_says_so() {
        let now = 100 * 86_400;
        let h = |level, forced, irregular, age| Hint {
            level,
            forced,
            interlaced: Some(true),
            vcodec: Some("h264".into()),
            acodec: None,
            irregular,
            updated_at: now - age,
        };
        assert_eq!(h(Some(CastLevel::Repackaged), false, false, 0).force(now), None);
        assert_eq!(h(Some(CastLevel::Converted), false, false, 0).force(now), Some(CastLevel::Converted));
        assert_eq!(h(None, false, true, 0).force(now), Some(CastLevel::Converted));
        assert_eq!(h(Some(CastLevel::Converted), false, true, HINT_FRESH_S).force(now), None, "stale: probe again");
        assert_eq!(h(None, true, false, HINT_FRESH_S * 3).force(now), Some(CastLevel::Converted), "the user's choice stays");

        let mut hints = CastHints { acodec: Some("mp2".into()), ..CastHints::default() };
        h(None, false, false, 0).fill(&mut hints, now);
        assert_eq!((hints.vcodec.as_deref(), hints.acodec.as_deref(), hints.interlaced), (Some("h264"), Some("mp2"), Some(true)));
        let mut stale = CastHints::default();
        h(None, false, false, HINT_FRESH_S).fill(&mut stale, now);
        assert_eq!(stale, CastHints::default());
    }
}
