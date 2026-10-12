//! Casting rows (DESIGN §40 §7.5, §13): `cast_identity`, `cast_device`, `cast_hint` (migration
//! 0014). OWNER: CAST-6. Free functions over `&Connection` like every repository module; writes
//! go through the `db-writer` (`Writer::submit` closures). The identity seed and the peer keys
//! are credentials: they leave this module only as `redact::Secret`s, never in a `Debug` print,
//! a log line or an agent reply. None of these tables is exported by `backup.rs`.

// CAST-6: identity (create once per install), device upsert / forget / pairing columns, hints.
