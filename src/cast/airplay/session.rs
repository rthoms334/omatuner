//! The session sequence (DESIGN §40 §7.3): pair-verify → SETUP (declared PTP, no timing port) →
//! event channel → RECORD (skipped on `skipRecord`; errors non-fatal) → `GET /info` → type-130
//! SETUP → `/command` → `/feedback` every 2 s after the commands, never in flight with a
//! `/command`; TEARDOWN. OWNER: CAST-2. From pyatv PR 2899 (MIT) with foxCast's order as the
//! cross-check (facts only).

// CAST-2
