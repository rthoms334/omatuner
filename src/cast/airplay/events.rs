//! The event channel (DESIGN §40 §7.3 step 3, "Events"): keys `Events-Salt` swapped, every
//! RTSP-framed request acked `RTSP/1.0 200 OK` with its CSeq (else the TV closes after ~25–30 s),
//! requests reassembled across HAP frames up to Content-Length and acked once (castcli #35),
//! `playbackState` / `notification` / `playbackInfo` responses decoded. OWNER: CAST-2.

// CAST-2
