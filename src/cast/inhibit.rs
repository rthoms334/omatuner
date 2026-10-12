//! The logind sleep inhibitor while casting (DESIGN §40 §11.5): `Inhibit("sleep", "Tuner",
//! "Casting to <TV>", "block")` through zbus, held only while a session exists and
//! `cast.keep_awake` is on; lid close still suspends. OWNER: CAST-6.

// CAST-6
