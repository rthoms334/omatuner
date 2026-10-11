-- Quality badges (DESIGN §6, §13.0). Owner: WP-02.
-- Written as 0002_quality, renumbered after 0002_category_filter; safe to re-run (a pre-merge
-- build's database is at v2 with this applied, see `migrations::reconcile_renumbered`): the
-- two `channel` columns are added by the Rust step of this
-- migration (`migrations::backfill`) only when missing, and it fills them from the stored
-- provider names, so an existing database needs no refresh:
--   channel.quality INTEGER — the quality word of the provider name (smart_names: 1 SD, 2 HD,
--                             3 FHD, 4 4K; NULL = none), written by every import;
--   channel.family  TEXT    — with a quality word: `normalize_name` of the smart-cleaned name,
--                             shared by the SD / HD / 2160p duplicates of one channel (NULL =
--                             same as `norm_name`; best-first ordering, DESIGN §13.2).

-- What mpv decoded the last time a channel played (video-params w × h); wins over the name.
-- Keyed by stable identity like `stream_failure`, so a refresh keeps it, and valid only while
-- the channel at that key still has the provider `name` (and `url_key`) it was measured
-- under: the quality duplicates of an M3U channel share a base key and are numbered by
-- playlist position (`KeyAllocator`), so a reordered playlist moves keys between them.
-- `finish_catalogue` re-keys a row to the channel that now has its name, else drops it.
-- It is not user data: no `orphaned_since`, no fingerprint.
CREATE TABLE IF NOT EXISTS channel_video (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  item_key  TEXT    NOT NULL,
  name      TEXT    NOT NULL,   -- channel.name (raw provider name) when measured
  url_key   TEXT,               -- channel.url_key when measured
  width     INTEGER NOT NULL,
  height    INTEGER NOT NULL,
  at        INTEGER NOT NULL,
  PRIMARY KEY (source_id, item_key)
) WITHOUT ROWID;
