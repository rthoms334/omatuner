-- 0011 Following (batch base; owner: F-AB, DESIGN §33). Idempotent.
-- Rust step `migrations::follow_columns`: reminder.rule_id INTEGER REFERENCES follow_rule(id)
-- ON DELETE SET NULL (add_column_if_missing).
CREATE TABLE IF NOT EXISTS follow_rule (
  id           INTEGER PRIMARY KEY,
  name         TEXT    NOT NULL,                       -- "Arsenal"
  kind         TEXT    NOT NULL CHECK (kind IN ('team', 'show', 'event', 'custom', 'news')),
  spec         TEXT    NOT NULL,                       -- JSON follow::spec::FollowSpec {"v":1,…}
  mode         TEXT    NOT NULL DEFAULT 'notify' CHECK (mode IN ('notify', 'switch', 'list', 'flash')),
  lead_s       INTEGER NOT NULL DEFAULT 120,
  offset_s     INTEGER NOT NULL DEFAULT 0,             -- e.g. +900 = skip build-up
  spoiler_safe INTEGER NOT NULL DEFAULT 0,             -- also a spoiler-guard topic (B4)
  max_per_week INTEGER NOT NULL DEFAULT 10,
  priority     INTEGER NOT NULL,                       -- list order; lower wins clashes
  enabled      INTEGER NOT NULL DEFAULT 1,
  origin       TEXT    NOT NULL DEFAULT 'user' CHECK (origin IN ('user', 'agent')),
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

-- The ledger: one row per airing a rule matched. It survives guide generations (programme ids
-- don't) and drives dedupe, dismissals, moves, new-only and Upcoming.
CREATE TABLE IF NOT EXISTS follow_match (
  id          INTEGER PRIMARY KEY,
  rule_id     INTEGER NOT NULL REFERENCES follow_rule(id) ON DELETE CASCADE,
  event_key   TEXT    NOT NULL,   -- textmatch::event_key (shared with spoiler_seen)
  start       INTEGER NOT NULL,   -- displayed UTC (effective shift of the chosen channel)
  stop        INTEGER NOT NULL,
  title       TEXT    NOT NULL,
  sub_title   TEXT,
  source_id   INTEGER,            -- chosen channel; NULL = nothing watchable
  channel_key TEXT,
  alternates  TEXT    NOT NULL DEFAULT '[]',   -- ≤ 3 fallback ItemRefs
  state       TEXT    NOT NULL CHECK (state IN
              ('scheduled', 'adopted', 'listed', 'over-cap', 'skipped', 'gone', 'aired')),
  note        TEXT,               -- 'clash:<rule id>' | 'locked' | 'no-interrupt'
  reminder_id INTEGER REFERENCES reminder(id) ON DELETE SET NULL,
  seen_at     INTEGER NOT NULL,
  announced   INTEGER NOT NULL DEFAULT 0,
  UNIQUE (rule_id, event_key, start)
);
CREATE INDEX IF NOT EXISTS follow_match_start    ON follow_match(start);
CREATE INDEX IF NOT EXISTS follow_match_reminder ON follow_match(reminder_id);
