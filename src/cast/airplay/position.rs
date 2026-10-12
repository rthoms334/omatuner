//! Position (DESIGN §40 §7.3 "Position"): a `playbackInfo` request (PR 2846; reply keys from
//! LIVI, facts only), else an estimate for the rest of the session (advances while `playing`,
//! freezes on `paused`/`loading`, bounded by the media the TV fetched). Never `GET /playback-info`
//! (500 on 26.x and 27.0). OWNER: CAST-2.

// CAST-2
