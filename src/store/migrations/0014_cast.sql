-- 0014 Casting (DESIGN §40 §7.5, §13; skeleton CAST-0, repository code in store::cast, CAST-6).
-- Idempotent. Machine-local: backup.rs exports an explicit table list, so none of these tables
-- is ever in a backup (pairings are per machine; after a restore the user pairs again).

-- One long-term pairing identity per install (row id 1). `seed` is the Ed25519 seed (32 bytes,
-- a credential: never logged, never sent to an agent); `sender_mac` a random, locally
-- administered MAC ("02:…") used as the SETUP deviceID and the fallback channelID.
CREATE TABLE IF NOT EXISTS cast_identity (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  pairing_id  TEXT    NOT NULL,                  -- UUID string, the pair-setup/verify identifier
  seed        BLOB    NOT NULL CHECK (length(seed) = 32),
  sender_mac  TEXT    NOT NULL,
  created_at  INTEGER NOT NULL
);

-- Receivers seen or paired. `id` is the DeviceId (32 hex). Names, models and versions are
-- sanitized device strings (display only); `last_addr` is private to the cast module (manual
-- devices, quick reconnects) and never shown, logged at INFO or given to an agent.
CREATE TABLE IF NOT EXISTS cast_device (
  id             TEXT    PRIMARY KEY,
  protocol       TEXT    NOT NULL CHECK (protocol IN ('airplay', 'googlecast', 'dlna')),
  name           TEXT    NOT NULL,
  model          TEXT,                            -- "AppleTV14,1"
  os_version     TEXT,                            -- TXT osvers / /info osBuildVersion ("26.6 (23L773)")
  source_version TEXT,                            -- AirPlay srcvers / sourceVersion ("960.13.1")
  manual         INTEGER NOT NULL DEFAULT 0,      -- added by address (not found by mDNS)
  last_addr      TEXT,                            -- "192.168.1.5:7000"
  last_seen      INTEGER,
  peer_id        TEXT,                            -- the TV's pairing identifier (NULL = not paired)
  peer_ltpk      BLOB CHECK (peer_ltpk IS NULL OR length(peer_ltpk) = 32),
  pair_kind      TEXT CHECK (pair_kind IS NULL OR pair_kind IN ('hkp3', 'hkp5')),
  paired_at      INTEGER,
  last_used      INTEGER,
  CHECK ((peer_id IS NULL) = (peer_ltpk IS NULL))
);

-- What worked for an item on a TV (DESIGN §40 §8.2–8.3): the level, whether it is interlaced,
-- its codecs (re-probed 30 days after the last picture). `forced` is the user's "Convert this
-- channel". A remembered L2 failure (→ L3 next time) is `l2_failed_at` with the relay logic
-- version that judged it (`store::cast::L2_FAILURE_LOGIC`: another version's verdict is
-- ignored), `irregular` when the playlist guard found it; the first L2 picture clears it.
-- Databases made before the two columns get them from `migrations::cast_hint_failures`.
CREATE TABLE IF NOT EXISTS cast_hint (
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL CHECK (kind IN ('channel', 'movie', 'episode')),
  item_key    TEXT    NOT NULL,
  device      TEXT    NOT NULL REFERENCES cast_device(id) ON DELETE CASCADE,
  level       TEXT    CHECK (level IS NULL OR level IN ('original', 'repackaged', 'converted')),
  forced      INTEGER NOT NULL DEFAULT 0,
  interlaced  INTEGER,
  vcodec      TEXT,
  acodec      TEXT,
  irregular   INTEGER NOT NULL DEFAULT 0,
  l2_failed_at    INTEGER,
  l2_failed_logic INTEGER,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (source_id, kind, item_key, device)
);
