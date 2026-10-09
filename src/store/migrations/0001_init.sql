-- Tuner schema v1. FROZEN at tag `skeleton-v1`; later changes are appended as 0002_*.sql
-- (owner WP-02, architect sign-off) — never edit this file after the tag.
-- Conventions: timestamps = unix seconds UTC; JSON columns hold serde_json of model types;
-- `seen_gen` / `gen` implement mark-and-sweep imports (DESIGN §7.2, §9.3).
-- User data is keyed by (source_id, kind, item_key) — NEVER by catalogue row ids — so it
-- survives refreshes; `fp` holds the JSON Fingerprint used for reattach (DESIGN §8). Every
-- user-data table that is keyed by an item carries `fp` + `orphaned_since`.
-- WRITE RULES (DESIGN §6.3/§7.2): all upserts are `INSERT … ON CONFLICT(…) DO UPDATE`;
-- `INSERT OR REPLACE` is FORBIDDEN on any table referenced by a foreign key (it deletes the
-- parent row, fires cascades and changes row ids).

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---------------------------------------------------------------- sources
CREATE TABLE source (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  kind          TEXT    NOT NULL CHECK (kind IN ('m3u-url','m3u-file','xtream')),
  icon          TEXT    NOT NULL DEFAULT '󰐹',
  color         TEXT    NOT NULL DEFAULT 'accent',
  url           TEXT,                 -- M3U URL (secret: may embed credentials)
  path          TEXT,                 -- M3U file
  xt_server     TEXT,
  xt_username   TEXT,
  xt_password   TEXT,                 -- plain; DB file is 0600 (DESIGN §20)
  settings      TEXT    NOT NULL DEFAULT '{}',   -- JSON SourceSettings
  enabled       INTEGER NOT NULL DEFAULT 1,
  sort_index    INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  last_refresh  INTEGER,
  last_attempt  INTEGER,
  last_error    TEXT,                 -- redacted
  etag          TEXT,
  last_modified TEXT,
  account       TEXT,                 -- JSON AccountInfo
  generation    INTEGER NOT NULL DEFAULT 0,      -- last *completed* catalogue import
  n_channels    INTEGER NOT NULL DEFAULT 0,
  n_movies      INTEGER NOT NULL DEFAULT 0,
  n_series      INTEGER NOT NULL DEFAULT 0,
  n_groups      INTEGER NOT NULL DEFAULT 0
);

-- ---------------------------------------------------------------- catalogue
CREATE TABLE grp (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL CHECK (kind IN ('channel','movie','series')),
  key            TEXT    NOT NULL,     -- GroupKey: normalized name
  name           TEXT    NOT NULL,
  provider_id    TEXT,                 -- Xtream category_id
  provider_order INTEGER NOT NULL,
  item_count     INTEGER NOT NULL DEFAULT 0,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, kind, key)
);

CREATE TABLE channel (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,     -- ItemKey
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,     -- identity::normalize_name
  number         INTEGER,
  logo           TEXT,
  tvg_id         TEXT,
  tvg_name       TEXT,
  tvg_shift_min  INTEGER NOT NULL DEFAULT 0,
  stream         TEXT    NOT NULL,     -- JSON StreamLocator
  url_key        TEXT,                 -- identity::url_key (reattach)
  headers        TEXT,                 -- JSON HttpHeaders
  catchup        TEXT,                 -- JSON CatchupSpec
  radio          INTEGER NOT NULL DEFAULT 0,
  unsupported    TEXT,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX channel_tvg      ON channel(tvg_id);
CREATE INDEX channel_norm     ON channel(source_id, norm_name);
CREATE INDEX channel_provider ON channel(source_id, provider_id);
CREATE INDEX channel_sweep    ON channel(source_id, seen_gen);
CREATE INDEX channel_urlkey   ON channel(source_id, url_key);   -- reattach step 4

CREATE TABLE channel_group (
  group_id   INTEGER NOT NULL REFERENCES grp(id)     ON DELETE CASCADE,
  channel_id INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
  position   INTEGER NOT NULL,
  is_primary INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (group_id, channel_id)
) WITHOUT ROWID;
CREATE INDEX channel_group_ch ON channel_group(channel_id);

CREATE TABLE movie (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,
  year           INTEGER,
  poster         TEXT,
  backdrop       TEXT,
  rating         REAL,
  plot           TEXT,
  genre          TEXT,
  cast_          TEXT,
  director       TEXT,
  duration_s     INTEGER,
  release_date   TEXT,
  tmdb_id        TEXT,
  trailer        TEXT,
  stream         TEXT    NOT NULL,
  url_key        TEXT,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  details_at     INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX movie_added ON movie(source_id, added_at DESC);
CREATE INDEX movie_norm  ON movie(source_id, norm_name);
CREATE INDEX movie_sweep ON movie(source_id, seen_gen);
CREATE INDEX movie_urlkey   ON movie(source_id, url_key);
CREATE INDEX movie_provider ON movie(source_id, provider_id);

CREATE TABLE movie_group (
  group_id INTEGER NOT NULL REFERENCES grp(id)   ON DELETE CASCADE,
  movie_id INTEGER NOT NULL REFERENCES movie(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  PRIMARY KEY (group_id, movie_id)
) WITHOUT ROWID;
CREATE INDEX movie_group_m ON movie_group(movie_id);

CREATE TABLE series (
  id             INTEGER PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key            TEXT    NOT NULL,
  provider_id    TEXT,
  name           TEXT    NOT NULL,
  clean_name     TEXT    NOT NULL,
  norm_name      TEXT    NOT NULL,
  year           INTEGER,
  cover          TEXT,
  backdrop       TEXT,
  rating         REAL,
  plot           TEXT,
  genre          TEXT,
  cast_          TEXT,
  director       TEXT,
  release_date   TEXT,
  last_modified  INTEGER,
  provider_order INTEGER NOT NULL,
  added_at       INTEGER,
  details_at     INTEGER,
  seen_gen       INTEGER NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX series_added ON series(source_id, added_at DESC);
CREATE INDEX series_norm  ON series(source_id, norm_name);
CREATE INDEX series_sweep ON series(source_id, seen_gen);
CREATE INDEX series_provider ON series(source_id, provider_id);

CREATE TABLE series_group (
  group_id  INTEGER NOT NULL REFERENCES grp(id)    ON DELETE CASCADE,
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  PRIMARY KEY (group_id, series_id)
) WITHOUT ROWID;
CREATE INDEX series_group_s ON series_group(series_id);

CREATE TABLE season (
  series_id     INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  number        INTEGER NOT NULL,
  name          TEXT,
  cover         TEXT,
  air_date      TEXT,
  overview      TEXT,
  episode_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (series_id, number)
) WITHOUT ROWID;

CREATE TABLE episode (
  id          INTEGER PRIMARY KEY,
  series_id   INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  key         TEXT    NOT NULL,
  provider_id TEXT,
  season      INTEGER NOT NULL,
  episode     INTEGER NOT NULL,
  title       TEXT    NOT NULL,
  plot        TEXT,
  duration_s  INTEGER,
  still       TEXT,
  air_date    TEXT,
  rating      REAL,
  stream      TEXT    NOT NULL,
  UNIQUE (source_id, key)
);
CREATE INDEX episode_series ON episode(series_id, season, episode);

-- ---------------------------------------------------------------- EPG
CREATE TABLE epg_source (
  id                 INTEGER PRIMARY KEY,
  name               TEXT    NOT NULL,
  kind               TEXT    NOT NULL CHECK (kind IN ('url','file','provider')),
  url                TEXT,                -- secret (xmltv.php carries credentials)
  path               TEXT,
  playlist_source_id INTEGER REFERENCES source(id) ON DELETE CASCADE,  -- kind='provider'
  priority           INTEGER NOT NULL DEFAULT 100,
  offset_min         INTEGER NOT NULL DEFAULT 0,
  enabled            INTEGER NOT NULL DEFAULT 1,
  refresh_h          INTEGER,
  active_gen         INTEGER NOT NULL DEFAULT 0,
  last_refresh       INTEGER,
  last_attempt       INTEGER,
  last_error         TEXT,
  etag               TEXT,
  last_modified      TEXT,
  n_channels         INTEGER NOT NULL DEFAULT 0,
  n_programmes       INTEGER NOT NULL DEFAULT 0,
  max_stop           INTEGER
);

CREATE TABLE epg_channel (
  id            INTEGER PRIMARY KEY,
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  icon          TEXT,
  seen_gen      INTEGER NOT NULL,
  UNIQUE (epg_source_id, xmltv_id)
);
CREATE INDEX epg_channel_nocase ON epg_channel(xmltv_id COLLATE NOCASE);

CREATE TABLE epg_display_name (
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id) ON DELETE CASCADE,
  name           TEXT    NOT NULL,
  norm           TEXT    NOT NULL,     -- identity::normalize_name
  PRIMARY KEY (epg_channel_id, name)
) WITHOUT ROWID;
CREATE INDEX epg_display_norm ON epg_display_name(norm);

-- Rowid table ON PURPOSE: programme.id is the FTS5 rowid. AUTOINCREMENT so a deleted id is
-- never reused (a stale FTS row can never match a new programme). The FK to epg_channel has NO
-- cascade: programme deletes are always explicit and chunked, FTS rows first
-- (store::epg::delete_programmes_of); deleting an epg_channel / epg_source / source that still
-- has programmes fails loudly instead of silently desynchronising the FTS index.
CREATE TABLE programme (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  epg_source_id  INTEGER NOT NULL,     -- denormalized for per-generation sweeps
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id),
  gen            INTEGER NOT NULL,
  start          INTEGER NOT NULL,
  stop           INTEGER NOT NULL,
  title          TEXT    NOT NULL,
  sub_title      TEXT,
  descr          TEXT,
  categories     TEXT,                 -- '\x1f'-joined
  icon           TEXT,
  season         INTEGER,
  episode        INTEGER,
  rating         TEXT,
  star_rating    TEXT,
  catchup_id     TEXT,
  flags          INTEGER NOT NULL DEFAULT 0   -- bit0 new, bit1 live, bit2 previously-shown
);
-- UNIQUE: duplicate (channel, start) inside one generation → first wins
-- (`INSERT … ON CONFLICT(epg_channel_id, gen, start) DO NOTHING`).
CREATE UNIQUE INDEX programme_grid ON programme(epg_channel_id, gen, start);
CREATE INDEX programme_sweep ON programme(epg_source_id, gen);
CREATE INDEX programme_stop  ON programme(stop);

-- Contentless FTS5 (SQLite ≥ 3.43 `contentless_delete`) over title/sub_title, keyed by
-- programme.id. Rows are inserted in the SAME transaction as their programme batch and deleted
-- by rowid (`DELETE FROM programme_fts WHERE rowid IN (…)`) before the programmes are deleted.
-- Hits are joined back to `programme` (and filtered to the active generation) for display.
-- No triggers (they would slow the 400k-row bulk import several-fold); no 'rebuild' needed.
CREATE VIRTUAL TABLE programme_fts USING fts5(
  title, sub_title,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '2 3'
);

-- Channel → EPG mappings, keyed by STABLE channel identity (survives refresh).
CREATE TABLE channel_epg_map (
  source_id     INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key   TEXT    NOT NULL,
  epg_source_id INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
  xmltv_id      TEXT    NOT NULL,
  method        TEXT    NOT NULL,      -- EpgMatchMethod (kebab)
  confidence    REAL    NOT NULL DEFAULT 1.0,
  fp            TEXT    NOT NULL DEFAULT '{}',   -- channel Fingerprint (manual rows reattach)
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, channel_key, epg_source_id)
) WITHOUT ROWID;

-- Resolved mapping per channel row (derived; rebuilt by store::epg::resolve_mappings).
CREATE TABLE channel_epg (
  channel_id     INTEGER PRIMARY KEY REFERENCES channel(id) ON DELETE CASCADE,
  epg_channel_id INTEGER NOT NULL REFERENCES epg_channel(id) ON DELETE CASCADE
);
CREATE INDEX channel_epg_rev ON channel_epg(epg_channel_id);

-- ---------------------------------------------------------------- user data
CREATE TABLE fav_list (
  id         INTEGER PRIMARY KEY,
  name       TEXT    NOT NULL,
  icon       TEXT    NOT NULL DEFAULT '',
  sort_index INTEGER NOT NULL DEFAULT 0,
  is_default INTEGER NOT NULL DEFAULT 0
);
INSERT INTO fav_list (name, icon, sort_index, is_default) VALUES ('Favourites', '', 0, 1);

CREATE TABLE fav_item (
  list_id        INTEGER NOT NULL REFERENCES fav_list(id) ON DELETE CASCADE,
  source_id      INTEGER NOT NULL REFERENCES source(id)   ON DELETE CASCADE,
  kind           TEXT    NOT NULL,
  item_key       TEXT    NOT NULL,
  position       INTEGER NOT NULL,
  added_at       INTEGER NOT NULL,
  fp             TEXT    NOT NULL,     -- JSON Fingerprint
  orphaned_since INTEGER,
  PRIMARY KEY (list_id, source_id, kind, item_key)
) WITHOUT ROWID;
CREATE INDEX fav_item_ref ON fav_item(source_id, kind, item_key);

CREATE TABLE item_override (
  source_id        INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind             TEXT    NOT NULL,
  item_key         TEXT    NOT NULL,
  rename           TEXT,
  hidden           INTEGER NOT NULL DEFAULT 0,
  locked           INTEGER NOT NULL DEFAULT 0,
  user_order       INTEGER,
  catchup_offset_s INTEGER,
  fp               TEXT    NOT NULL,
  orphaned_since   INTEGER,
  PRIMARY KEY (source_id, kind, item_key)
) WITHOUT ROWID;

CREATE TABLE group_override (
  source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind        TEXT    NOT NULL,
  group_key   TEXT    NOT NULL,
  rename      TEXT,
  hidden      INTEGER NOT NULL DEFAULT 0,
  locked      INTEGER NOT NULL DEFAULT 0,
  user_order  INTEGER,
  pinned_home INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (source_id, kind, group_key)
) WITHOUT ROWID;

CREATE TABLE watch_progress (
  source_id  INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,
  item_key   TEXT    NOT NULL,
  position_s REAL    NOT NULL,
  duration_s REAL,
  finished   INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  title      TEXT    NOT NULL,
  fp         TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, kind, item_key)
) WITHOUT ROWID;
CREATE INDEX watch_progress_recent ON watch_progress(updated_at DESC);

CREATE TABLE history (
  id        INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind      TEXT    NOT NULL,
  item_key  TEXT    NOT NULL,
  title     TEXT    NOT NULL,
  at        INTEGER NOT NULL
);
CREATE INDEX history_at ON history(at DESC);

CREATE TABLE reminder (
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  channel_key  TEXT    NOT NULL,
  channel_name TEXT    NOT NULL,
  title        TEXT    NOT NULL,
  start        INTEGER NOT NULL,
  stop         INTEGER NOT NULL,
  lead_s       INTEGER NOT NULL DEFAULT 120,
  state        TEXT    NOT NULL DEFAULT 'pending',
  fp           TEXT    NOT NULL DEFAULT '{}',   -- channel Fingerprint
  orphaned_since INTEGER,
  UNIQUE (source_id, channel_key, start)
);
CREATE INDEX reminder_due ON reminder(state, start);

-- Per-channel stream failure cache (critique gap 11): dims dead channels, skips them when zapping.
CREATE TABLE stream_failure (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  item_key  TEXT    NOT NULL,
  reason    TEXT    NOT NULL,          -- PlayerErrorKind (kebab)
  at        INTEGER NOT NULL,
  count     INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (source_id, item_key)
) WITHOUT ROWID;
