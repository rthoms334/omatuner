-- Several playlists P3: "Same title as…" (DESIGN §29.16). The user combines two titles the
-- matcher kept apart. `title_link` gains the action `join` (its CHECK constraint can only
-- change by rebuilding the table) and `join_group`: copies with the same `join_group` stack
-- together, with the stacks they are in. A join is user data like a split or a pin (`fp` +
-- `orphaned_since`: a refresh that re-keys a copy re-attaches its row).
CREATE TABLE title_link_new (
  source_id  INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL CHECK (kind IN ('movie', 'series')),
  item_key   TEXT    NOT NULL,
  action     TEXT    NOT NULL CHECK (action IN ('split', 'pin', 'join')),
  at         INTEGER NOT NULL,
  fp         TEXT    NOT NULL DEFAULT '{}',
  orphaned_since INTEGER,
  join_group INTEGER,
  PRIMARY KEY (source_id, kind, item_key, action)
) WITHOUT ROWID;
INSERT INTO title_link_new(source_id, kind, item_key, action, at, fp, orphaned_since)
  SELECT source_id, kind, item_key, action, at, fp, orphaned_since FROM title_link;
DROP TABLE title_link;
ALTER TABLE title_link_new RENAME TO title_link;
