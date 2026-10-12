//! HAP keys and frame encryption (DESIGN §40 §7.1). OWNER: CAST-1. Port from pyatv
//! `pyatv/auth/hap_session.py` + `pyatv/support/chacha20.py` @ b277a4c8 (MIT), on aws-lc-rs.
//!
//! A frame is a 2-byte little-endian plaintext length (also the AAD), the ChaCha20-Poly1305
//! ciphertext and a 16-byte tag; the nonce is 4 zero bytes + a little-endian u64 counter per
//! direction. Outgoing plaintext ≤ [`MAX_OUT_FRAME`] bytes per frame (pyatv); incoming frames
//! may be any length up to [`MAX_IN_FRAME`] (lenient: tvOS 27 is claimed to send more than 1024).
//! Hardening caps: [`MAX_IN_FRAME`] per frame, [`MAX_MESSAGE`] per reassembled message.

use crate::redact::Secret;

pub const MAX_OUT_FRAME: usize = 1024;
pub const MAX_IN_FRAME: usize = 65_535;
pub const MAX_MESSAGE: usize = 1 << 20;

/// A frame that can't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapError {
    /// The tag doesn't verify (a forged or corrupted frame: the session is lost).
    BadTag,
    /// A length over the cap.
    TooLong,
}

/// `HKDF-SHA512(ikm, salt, info)` → 32 bytes (every HAP key: pair-setup, pair-verify, control,
/// events).
pub fn hkdf_sha512(_ikm: &[u8], _salt: &[u8], _info: &[u8]) -> Secret<[u8; 32]> {
    // CAST-1
    Secret::new([0; 32])
}

/// Both directions of an encrypted connection.
pub struct FrameCipher {
    // CAST-1: keys and counters.
}

impl FrameCipher {
    /// `write_key` seals what we send, `read_key` opens what we receive (the control channel:
    /// `Control-Write-…` / `Control-Read-…`; the event channel swaps them).
    pub fn new(_write_key: &Secret<[u8; 32]>, _read_key: &Secret<[u8; 32]>) -> Self {
        // CAST-1
        Self {}
    }

    /// Seal `plain` into one or more frames.
    pub fn seal(&mut self, _plain: &[u8]) -> Vec<u8> {
        // CAST-1
        Vec::new()
    }

    /// Open the first complete frame in `input` (consumed from it); `Ok(None)`: need more bytes.
    pub fn open(&mut self, _input: &mut Vec<u8>) -> Result<Option<Vec<u8>>, HapError> {
        // CAST-1
        Err(HapError::BadTag)
    }
}
