-- Quality badges (DESIGN §6, §13.0). Owner: WP-02.
-- `channel.quality`: the quality word of the provider name (smart_names: 1 SD, 2 HD, 3 FHD,
-- 4 4K; NULL = none), written by every import. Existing rows are filled by the Rust backfill
-- that runs with this migration (`migrations::backfill`), from the stored provider name.
ALTER TABLE channel ADD COLUMN quality INTEGER;

-- What mpv decoded the last time a channel played (video-params w × h); wins over the name.
-- Keyed by stable identity like `stream_failure`, so a refresh keeps it; `fp` lets
-- `finish_catalogue` re-key it when the channel's key changes (reattach rules, DESIGN §8.3).
-- It is not user data: no `orphaned_since` — a row whose channel is gone for good is dropped.
CREATE TABLE channel_video (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  item_key  TEXT    NOT NULL,
  width     INTEGER NOT NULL,
  height    INTEGER NOT NULL,
  at        INTEGER NOT NULL,
  fp        TEXT    NOT NULL DEFAULT '{}',   -- channel Fingerprint at measurement time
  PRIMARY KEY (source_id, item_key)
) WITHOUT ROWID;
