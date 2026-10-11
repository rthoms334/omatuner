-- Several playlists P2: copies of one title stack in "All playlists" (DESIGN §29.10). Owner:
-- P2 core. Safe to re-run (it was 0005 before explicit content took that number): the
-- columns below are added by the Rust step of this migration (`migrations::backfill` →
-- `add_title_columns`) only when missing, then `store::titles::refresh_all` fills them from
-- the stored provider names, so an existing database stacks without a refresh.
--
--   movie.match_key / series.match_key      TEXT    — the folded title (+ version markers) of
--                                                     the RAW name (`sources::vod_match`);
--                                                     diagnostics and P3 "Same title as…"
--   movie.match_hash / series.match_hash    INTEGER — blake3-64 of (kind, key, year): what the
--                                                     library build groups on; NULL = the key
--                                                     can't stack (no year, nothing left)
--   movie.match_lang / series.match_lang    TEXT    — market / language tag + dub marker
--                                                     (`EN`, `AR-SUBS`, `FR+SUB`, `+MULTI`)
--   movie.match_tags / series.match_tags    INTEGER — `VersionTags` bits (quality word, HDR,
--                                                     DV, CAM, 3D); NULL = not computed yet
--                                                     (new row, or its name / year changed)
--   movie.match_origin / series.match_origin TEXT   — `(GB)`-style origin country
--   series.tmdb_id                           INTEGER — from the series list (`tmdb`) or its
--                                                     details (movies already have `tmdb_id`)
--
-- `meta` row `vod_match:<source id>` = "<rules version>:<prefix fingerprint>": when either
-- changes the source's keys are recomputed (`titles::refresh_source`).

-- The user's corrections, per copy (keyed like user data: source, kind, item key):
--   split — "Not the same title": the copy never joins a stack (Undo deletes the row);
--   pin   — "Always play from <playlist>": this copy plays first in its stack.
-- User data like a favourite: `fp` + `orphaned_since` let a refresh re-attach it when the
-- provider re-keys the copy (`store::user` HOLDERS), else it is listed in Settings ▸ Orphans.
CREATE TABLE IF NOT EXISTS title_link (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind      TEXT    NOT NULL CHECK (kind IN ('movie', 'series')),
  item_key  TEXT    NOT NULL,
  action    TEXT    NOT NULL CHECK (action IN ('split', 'pin')),
  at        INTEGER NOT NULL,
  fp        TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  PRIMARY KEY (source_id, kind, item_key, action)
) WITHOUT ROWID;
