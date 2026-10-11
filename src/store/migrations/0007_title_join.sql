-- Several playlists P3: "Same title as…" (DESIGN §29.16). The user combines two titles the
-- matcher kept apart. `title_link` gains the action `join` (its CHECK constraint can only
-- change by rebuilding the table) and `join_group`: copies with the same `join_group` stack
-- together, with the stacks they are in. A join is user data like a split or a pin (`fp` +
-- `orphaned_since`: a refresh that re-keys a copy re-attaches its row).
--
-- The rebuild is the Rust backfill `rebuild_title_link` (migrations.rs): a re-run must copy
-- `join_group` when the table already has it, which plain SQL can't decide.
SELECT 1;
