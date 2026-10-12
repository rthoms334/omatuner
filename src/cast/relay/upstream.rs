//! The loopback upstream proxy (DESIGN §40 §8.1.1): the only code that talks to the provider
//! (reqwest with `Playable.headers`), one upstream request per token at a time, real HTTP
//! statuses and provider text reported, a ≤ 16 MiB head buffer replayed to ffprobe then ffmpeg,
//! HLS upstream reduced to one variant with URIs rewritten to `/u/<tok>/r/<n>`. Never puts a
//! provider URL on a command line. OWNER: CAST-4.

// CAST-4
