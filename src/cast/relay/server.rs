//! The LAN media server (DESIGN §40 §9.1): axum (`http1`, `tokio`) bound to the address toward
//! the TV only (never 0.0.0.0), on `cast.media_port`, listening only while a session (or Test on
//! TV) exists. One fallback handler: `GET`/`HEAD` `/c/<tok>/(index.m3u8|init.mp4|s<n>.ts|.m4s)`;
//! every miss (path, method, token, peer) is the same 404. Correct MIME types, `Content-Length`,
//! `Range` → 206, `Cache-Control: no-cache` on playlists, CORS, no redirects, ≤ 8 connections,
//! 30 s idle. OWNER: CAST-4.

// CAST-4
