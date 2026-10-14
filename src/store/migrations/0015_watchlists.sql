-- 0015 Watchlists (DESIGN §42 §4). The Rust step adds the queue chip and moves favourites.
CREATE TABLE IF NOT EXISTS watchlist (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  note TEXT,
  play_order TEXT NOT NULL DEFAULT 'ordered' CHECK (play_order IN ('ordered', 'any')),
  sort_index INTEGER NOT NULL DEFAULT 0,
  since INTEGER NOT NULL DEFAULT 0,
  is_default INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  created_by TEXT NOT NULL DEFAULT 'app',
  last_played_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS watchlist_name ON watchlist(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS watchlist_item (
  id INTEGER PRIMARY KEY,
  list_id INTEGER NOT NULL REFERENCES watchlist(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('movie', 'series', 'episode')),
  source_id INTEGER REFERENCES source(id) ON DELETE SET NULL,
  item_key TEXT,
  any_copy INTEGER NOT NULL DEFAULT 1,
  season_from INTEGER,
  season_to INTEGER,
  title TEXT NOT NULL,
  match_title TEXT, -- provider title, independent of the display rename
  year INTEGER,
  tmdb_id INTEGER,  -- episodes: parent series identity, together with year
  series_title TEXT,
  season INTEGER,
  episode INTEGER,
  skipped INTEGER NOT NULL DEFAULT 0,
  added_at INTEGER NOT NULL,
  fp TEXT NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  CHECK ((source_id IS NULL) = (item_key IS NULL)),
  CHECK (kind = 'series' OR (season_from IS NULL AND season_to IS NULL))
);
CREATE INDEX IF NOT EXISTS watchlist_item_order ON watchlist_item(list_id, position);
CREATE INDEX IF NOT EXISTS watchlist_item_ref ON watchlist_item(source_id, kind, item_key);
-- SET NULL must clear both halves of the reference before its CHECK is evaluated.
CREATE TRIGGER IF NOT EXISTS watchlist_source_deleted BEFORE DELETE ON source BEGIN
  UPDATE watchlist_item SET source_id = NULL, item_key = NULL WHERE source_id = OLD.id;
END;
CREATE INDEX IF NOT EXISTS watchlist_movie_hash ON movie(match_hash) WHERE match_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS watchlist_series_hash ON series(match_hash) WHERE match_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS watchlist_movie_tmdb ON movie(tmdb_id);
CREATE INDEX IF NOT EXISTS watchlist_series_tmdb ON series(tmdb_id);
