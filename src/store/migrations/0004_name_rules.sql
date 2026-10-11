-- Name rules and quality-label trust (DESIGN §13.0.1, §13.1). Owner: WP-02.
--
-- What each measured channel's provider name claimed and what it played at: the evidence of
-- how far a source's quality words can be trusted (`LabelTrust`, aggregated per source at
-- library load). One row per channel NAME of a source, upserted by `store::live::record_video`
-- (a channel measured again replaces its row: zapping one channel fifty times is one
-- channel). Keyed by name, not item key, so the evidence outlives refreshes that move keys
-- between the quality duplicates of a channel. Not user data: no fingerprint.
CREATE TABLE IF NOT EXISTS quality_check (
  source_id INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  name      TEXT    NOT NULL,   -- channel.name (raw provider name) when measured
  claimed   INTEGER,            -- channel.quality then: the name's class (1 SD … 4 4K; NULL = none)
  measured  INTEGER NOT NULL,   -- Quality::of_video of the decoded size (same codes)
  at        INTEGER NOT NULL,
  PRIMARY KEY (source_id, name)
) WITHOUT ROWID;

-- The Rust step of this migration (`migrations::backfill`) seeds it from `channel_video`, and
-- re-cleans the stored display names with the shared name tokenizer
-- (`migrations::reclean_names`: `clean_name` / `grp.name` from the stored provider names,
-- `channel.quality` / `family` again), so existing databases get the new rules without a
-- refresh. Safe to re-run.
