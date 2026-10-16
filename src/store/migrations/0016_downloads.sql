-- 0016 Downloads and recordings (DESIGN §43). User data: fp + orphaned_since (HOLDERS "download").
CREATE TABLE IF NOT EXISTS download (
  id          INTEGER PRIMARY KEY,
  position    INTEGER NOT NULL,                 -- queue order among waiting jobs
  kind        TEXT NOT NULL CHECK (kind IN ('movie', 'episode', 'live', 'catchup')),
  source_id   INTEGER REFERENCES source(id) ON DELETE SET NULL,   -- NULL after a playlist delete: still plays
  item_key    TEXT,                             -- movie / episode key; channel key for live / catchup
  any_copy    INTEGER NOT NULL DEFAULT 1,
  prog_start  INTEGER, prog_stop INTEGER,       -- live / catchup: the programme (or the set time)
  pad_before_s INTEGER NOT NULL DEFAULT 0, pad_after_s INTEGER NOT NULL DEFAULT 0,
  catchup_id  TEXT,
  title       TEXT NOT NULL, subtitle TEXT,     -- display only; never logged at INFO, never in file names
  state       TEXT NOT NULL CHECK (state IN ('scheduled', 'waiting', 'running', 'finishing', 'paused', 'done',
                                             'partial', 'failed', 'missed', 'cancelled')),
  reason      TEXT,                             -- 'busy', 'space', 'http-403', 'watching', 'gaps:2:40' …
  store_name  TEXT,                             -- '<id>.mkv' or '<id>/' (HLS folder); counters only
  bytes_done  INTEGER NOT NULL DEFAULT 0, bytes_total INTEGER,
  etag        TEXT, last_modified TEXT,         -- Range resume validation
  attempts    INTEGER NOT NULL DEFAULT 0,
  added_at    INTEGER NOT NULL, added_by TEXT NOT NULL DEFAULT 'app',  -- or the agent's name
  done_at     INTEGER, watched_at INTEGER,
  fp          TEXT NOT NULL DEFAULT '{}', orphaned_since INTEGER
);
CREATE INDEX IF NOT EXISTS download_item ON download(source_id, kind, item_key);
CREATE INDEX IF NOT EXISTS download_state ON download(state, position);

CREATE TABLE IF NOT EXISTS download_rule (     -- "Keep the next N unwatched downloaded"
  id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  series_key TEXT NOT NULL, keep INTEGER NOT NULL CHECK (keep BETWEEN 1 AND 10),
  added_at INTEGER NOT NULL, fp TEXT NOT NULL DEFAULT '{}', orphaned_since INTEGER
);
-- Series recording: follow_rule gains action 'record' (a Rust rebuild step if its CHECK lists the actions).

