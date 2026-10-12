-- 0009 Guide fixes (batch base; owner: G-core, DESIGN §31). Idempotent.
-- Columns added by the Rust step `migrations::guide_fix_columns` (add_column_if_missing):
--   item_override.shift_min    INTEGER                     -- the user's time shift (min); NULL = automatic
--   item_override.guide_ok     INTEGER NOT NULL DEFAULT 0  -- dismissed issue kinds (IssueKind::bit)
--   channel_epg_map.shift_min  INTEGER                     -- shift of a 'sibling' rule row
--   channel_epg_map.epg_name   TEXT                        -- guide channel name when chosen
--   channel_epg_map.actor      TEXT                        -- manual rows: 'you' | 'claude'
--   channel_epg.shift_min      INTEGER                     -- the winning row's shift_min
-- New channel_epg_map.method values: 'sibling', 'copy' (EpgMatchMethod; no CHECK to change).
-- Effective shift = COALESCE(item_override.shift_min, channel_epg.shift_min, channel.tvg_shift_min)
-- (store::library::load_live, B8).

CREATE TABLE IF NOT EXISTS guide_fix_log (
  id        INTEGER PRIMARY KEY,
  at        INTEGER NOT NULL,
  actor     TEXT    NOT NULL CHECK (actor IN ('you', 'claude')),
  label     TEXT    NOT NULL,          -- "Accepted 140 suggestions" (no titles at INFO)
  n         INTEGER NOT NULL,
  undone_at INTEGER
);

CREATE TABLE IF NOT EXISTS guide_fix_change (
  log_id      INTEGER NOT NULL REFERENCES guide_fix_log(id) ON DELETE CASCADE,
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key TEXT    NOT NULL,
  field       TEXT    NOT NULL CHECK (field IN ('map', 'shift', 'catchup', 'dismiss', 'rule')),
  before      TEXT,                    -- JSON
  after       TEXT,
  PRIMARY KEY (log_id, source_id, channel_key, field)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS guide_fix_change_ch ON guide_fix_change(source_id, channel_key);

-- Derived cache (not user data, no fp): top-3 fuzzy suggestions per unmapped shown channel.
CREATE TABLE IF NOT EXISTS epg_suggestion (
  source_id     INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key   TEXT    NOT NULL,
  rank          INTEGER NOT NULL,      -- 0..2
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  display_name  TEXT    NOT NULL,
  score         REAL    NOT NULL,
  PRIMARY KEY (source_id, channel_key, rank)
) WITHOUT ROWID;
-- meta 'guide_check:stamp' = blake3(active gens, catalogue generations, fuzzy threshold, rules).
