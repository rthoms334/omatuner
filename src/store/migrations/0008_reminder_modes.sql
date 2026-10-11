-- 0008 reminder modes (batch base; owner: remind track, DESIGN §30 pass 2).
-- Idempotent: the columns are added by the Rust step `migrations::reminder_modes`
-- (add_column_if_missing), which also turns the old switch marker (lead_s = -1) into
-- mode = 'switch':
--   reminder.mode     TEXT    NOT NULL DEFAULT 'notify'   -- 'notify' | 'switch'
--   reminder.offset_s INTEGER NOT NULL DEFAULT 0          -- start offset ("skip the build-up")
--   reminder.open_in  TEXT                                -- NULL = reminders.open_in
-- Systemd units carry only `tuner --reminder <id>` (B6): nothing about them is stored here.
CREATE INDEX IF NOT EXISTS reminder_due ON reminder(state, start);
