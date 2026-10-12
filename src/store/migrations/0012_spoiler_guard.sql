-- 0012 Spoiler guard (batch base; owner: S1, DESIGN §34). Idempotent.
-- Following rules with spoiler_safe = 1 are guard topics by data (B4): no follow_rule_id or
-- origin 'follow' here.
CREATE TABLE IF NOT EXISTS spoiler_rule (
  id             INTEGER PRIMARY KEY,
  kind           TEXT    NOT NULL CHECK (kind IN ('topic', 'series', 'blackout')),
  label          TEXT    NOT NULL,                 -- "Arsenal" (Settings, tooltips)
  terms          TEXT    NOT NULL DEFAULT '[]',    -- JSON folded phrases incl. aliases (topic/blackout)
  series_norm    TEXT,                             -- title-stack norm name (series)
  categories     TEXT    NOT NULL DEFAULT '[]',    -- JSON guide-category scope; [] = any
  mode           TEXT    NOT NULL DEFAULT 'until-seen' CHECK (mode IN ('until-seen', 'always')),
  window_h       INTEGER NOT NULL DEFAULT 48,      -- cap after the event ends; 0 = none
  until_at       INTEGER,                          -- blackout end
  sport_descr    INTEGER NOT NULL DEFAULT 1,       -- also hide sport sub/descr while embargoed
  shield         INTEGER NOT NULL DEFAULT 1,       -- zap shield for this rule
  enabled        INTEGER NOT NULL DEFAULT 1,       -- 0 on a series row = "don't guard" exception
  origin         TEXT    NOT NULL DEFAULT 'user' CHECK (origin IN ('user', 'agent')),
  created_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS spoiler_rule_series ON spoiler_rule(series_norm) WHERE kind = 'series';

-- Only guarded subjects are ever recorded (no general programme watch log).
CREATE TABLE IF NOT EXISTS spoiler_seen (
  event_key   TEXT    NOT NULL,                    -- textmatch::event_key (shared with follow_match)
  first_start INTEGER NOT NULL,                    -- EPG time of the first airing
  title       TEXT    NOT NULL DEFAULT '',         -- as the guide wrote it (Settings "Seen recently")
  series_norm TEXT,                                -- normalize_name(series) when S/E known
  season      INTEGER,
  episode     INTEGER,
  seen_at     INTEGER NOT NULL,
  how         TEXT    NOT NULL CHECK (how IN ('live', 'catchup', 'vod', 'marked', 'agent')),
  PRIMARY KEY (event_key, first_start)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS spoiler_seen_at ON spoiler_seen(seen_at);
