//! The level decision (DESIGN §40 §8.3): pure and table-tested. DRM → refuse; override or hint
//! L3 → L3; video outside the receiver profile → L3 (or refuse when `convert = never`);
//! interlaced on AirPlay with `interlaced = auto` → L3 + yadif (Apple HLS rule 1.14); HEVC → L2
//! fMP4; else L2 TS. Audio: AAC ≤ 2 ch copied, everything else AAC stereo 160k. OWNER: CAST-4.

// CAST-4
