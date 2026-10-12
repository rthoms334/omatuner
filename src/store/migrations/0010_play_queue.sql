-- 0010 Up next, the play queue (batch base; owner: Q-core, DESIGN §32). Idempotent.
-- User data: fp + orphaned_since like favourites (store::user::HOLDERS "queue").
CREATE TABLE IF NOT EXISTS queue_item (
  id          INTEGER PRIMARY KEY,
  position    INTEGER NOT NULL,                 -- dense 0..n-1, rewritten in one tx on move (≤ 200 rows)
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL CHECK (kind IN ('movie', 'episode', 'channel')),
  item_key    TEXT    NOT NULL,
  any_copy    INTEGER NOT NULL DEFAULT 1,       -- 1: the title's copy plan picks at play time; 0: this copy
  prog_start  INTEGER,                          -- channel rows: the programme (displayed UTC times;
  prog_stop   INTEGER,                          --   programme.id changes every EPG import)
  catchup_id  TEXT,
  title       TEXT    NOT NULL,                 -- captured for display if the item vanishes; never logged at INFO
  subtitle    TEXT,                             -- "S02E05 · …" / "BBC One · Sat 07:10"
  duration_s  REAL,                             -- ETA; NULL = unknown
  from_start  INTEGER NOT NULL DEFAULT 0,
  state       TEXT    NOT NULL DEFAULT 'pending'
              CHECK (state IN ('pending', 'current', 'played', 'skipped', 'failed')),
  added_at    INTEGER NOT NULL,
  added_by    TEXT    NOT NULL DEFAULT 'app' CHECK (added_by IN ('app', 'agent')),
  done_at     INTEGER,
  fp          TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  CHECK ((kind = 'channel') = (prog_start IS NOT NULL AND prog_stop IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS queue_item_order ON queue_item(position);
CREATE UNIQUE INDEX IF NOT EXISTS queue_item_one_current ON queue_item(state) WHERE state = 'current';
