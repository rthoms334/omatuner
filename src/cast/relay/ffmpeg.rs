//! The ffmpeg supervisor (DESIGN §40 §8.1.3, §8.4): `tokio::process` with `kill_on_drop` and
//! `PR_SET_PDEATHSIG` (spawned from a runtime worker, never the blocking pool), environment
//! cleared but `LC_ALL=C`, argv with only the loopback URL and the output directory, stderr
//! redacted at debug, `-progress pipe:1` speed → `CpuTooSlow`, encoder check once per run.
//! OWNER: CAST-4.

// CAST-4
