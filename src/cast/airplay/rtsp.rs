//! HTTP/1.1 + RTSP/1.0 on one TCP connection (DESIGN §40 §7.1 "Transport"). OWNER: CAST-2. Port
//! from pyatv `support/rtsp.py` (`exchange`, RTSP headers) and `support/http.py` @ b277a4c8 (MIT).
//! Exactly one request in flight; RTSP replies matched by `CSeq`, HTTP in order; a reply that
//! matches nothing pending (a late `/feedback` answer, pyatv #2893) is dropped and logged at
//! debug. Encryption (`super::hap::FrameCipher`) starts right after pair-verify. Timeouts:
//! connect 3 s, request 5 s.

// CAST-2
