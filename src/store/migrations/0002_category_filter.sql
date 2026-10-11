-- Phase 6 (DESIGN §28): category filter by country / language.
-- The provider's category name as imported, before smart-name cleanup (tag detection reads
-- it). NULL for groups imported before this migration: filled by the next refresh.
ALTER TABLE grp ADD COLUMN raw_name TEXT;
-- The user showed this group although the source's category filter hides its tag.
ALTER TABLE group_override ADD COLUMN shown INTEGER NOT NULL DEFAULT 0;
