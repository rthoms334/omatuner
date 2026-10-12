//! Session tokens (DESIGN §40 §9.2): 128 random bits (aws-lc-rs `SystemRandom`), base32 lower
//! case, compared in constant time, one per load generation (a zap, seek or reconnect revokes the
//! old one; a playlist view's token maps to the same directory), registered with
//! `redact::register_secret`, revoked at session end and 60 s after the last fetch once stopped.
//! The loopback proxy has its own. OWNER: CAST-4.

// CAST-4
