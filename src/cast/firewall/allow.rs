//! The Allow / Remove runner (DESIGN §40 §9.4 "Remedy"). OWNER: CAST-7.
//!
//! Spawns `pkexec --disable-internal-agent …` (`super::argv`) with no shell, stdin null and
//! stderr captured, on the runtime, capped at `super::PKEXEC_TIMEOUT`; Cancel, the timeout and
//! quitting kill it (closing polkit's dialog). Maps the exit status (`man pkexec`: 126 dismissed,
//! 127 not authorised / no agent; the helper's 2 bad arguments, 3 ufw failed) and then a re-read
//! of `user.rules` to an `AllowOutcome`. Tests use a fake `pkexec` script, never the real one.

// CAST-7
