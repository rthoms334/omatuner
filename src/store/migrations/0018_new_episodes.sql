-- 0018 New episodes (DESIGN §44). The ledger is per series COPY; alerts are per TITLE episode.
-- Rows exist only for tracked titles (watchlists + binged), so the tables stay small.
-- User data: newep_arrival and newep_mute carry fp + orphaned_since (HOLDERS "new episode",
-- "new-episode mute"); the ledger is derived and never backed up (a restore re-baselines).
CREATE TABLE IF NOT EXISTS newep_copy (
  source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  series_key   TEXT    NOT NULL,
  baselined_at INTEGER,               -- NULL: tracked, episodes not loaded yet (baseline at first load)
  frontier     INTEGER NOT NULL DEFAULT 0,  -- 1: found after its title was tracked: its first
                                            -- load alerts beyond the title's frontier (§3.2)
  lm_seen      INTEGER,               -- series.last_modified at the last diff
  checked_at   INTEGER,               -- the last details fetch this feature made
  PRIMARY KEY (source_id, series_key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS newep_known (  -- every (season, episode) a tracked copy has had
  source_id  INTEGER NOT NULL,
  series_key TEXT    NOT NULL,
  season     INTEGER NOT NULL,
  episode    INTEGER NOT NULL,
  first_seen INTEGER NOT NULL,
  PRIMARY KEY (source_id, series_key, season, episode),
  FOREIGN KEY (source_id, series_key) REFERENCES newep_copy(source_id, series_key) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS newep_arrival (  -- one row per title episode found new
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER REFERENCES source(id) ON DELETE SET NULL,  -- the copy it arrived in first
  series_key   TEXT,
  season       INTEGER NOT NULL,
  episode      INTEGER NOT NULL,
  series_title TEXT    NOT NULL,      -- display only; never logged at INFO
  why          TEXT    NOT NULL DEFAULT '[]',  -- JSON: ["list:3", "binge"]
  arrived_at   INTEGER NOT NULL,
  notified_at  INTEGER,               -- NULL: not announced (notifications off, quiet, headless)
  quiet        TEXT,                  -- 'following' | 'seen' | NULL
  opened_at    INTEGER,               -- series page / entry opened: the chip goes
  cleared_at   INTEGER,               -- off the Home row
  cleared_how  TEXT CHECK (cleared_how IN ('watched', 'dismissed', 'expired')),
  fp           TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER
);
CREATE INDEX IF NOT EXISTS newep_arrival_open ON newep_arrival(cleared_at, arrived_at DESC);
CREATE INDEX IF NOT EXISTS newep_arrival_ref  ON newep_arrival(source_id, series_key, season, episode);

-- "Stop alerts for this series" (Q1): one row per copy of the muted title. A muted title stays
-- tracked (its ledger keeps up), so unmuting doesn't replay what arrived meanwhile.
CREATE TABLE IF NOT EXISTS newep_mute (
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  series_key     TEXT    NOT NULL,
  muted_at       INTEGER NOT NULL,
  fp             TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, series_key)
) WITHOUT ROWID;
-- meta rows: 'newep:baselined_at', 'newep:card', 'newep:watchlists_opened_at', 'newep:checked_at',
-- 'newep:lm_unreliable:<source id>' (§5, Fallback).
