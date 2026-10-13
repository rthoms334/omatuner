// Ported from pyatv pyatv/auth/hap_session.py + pyatv/support/chacha20.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! HAP keys and frame encryption (DESIGN §40 §7.1). OWNER: CAST-1. Port from pyatv
//! `pyatv/auth/hap_session.py` + `pyatv/support/chacha20.py` @ b277a4c8 (MIT), on aws-lc-rs.
//!
//! A frame is a 2-byte little-endian plaintext length (also the AAD), the ChaCha20-Poly1305
//! ciphertext and a 16-byte tag; the nonce is 4 zero bytes + a little-endian u64 counter per
//! direction. Outgoing plaintext ≤ [`MAX_OUT_FRAME`] bytes per frame (pyatv); incoming frames
//! may be any length up to [`MAX_IN_FRAME`] (lenient: tvOS 27 is claimed to send more than 1024).
//! Hardening caps: [`MAX_IN_FRAME`] per frame, [`MAX_MESSAGE`] per reassembled message (the
//! reader in `rtsp` / `events` applies the latter).
//!
//! The pairing messages use the same cipher one-shot with a fixed nonce label instead of a
//! counter (pyatv `Chacha20Cipher8byteNonce.encrypt(…, nonce=b"PS-Msg05")`): 4 zero bytes ‖ the
//! 8-byte label, no AAD ([`seal_labelled`] / [`open_labelled`]).

use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::hkdf::{self, HKDF_SHA512};

use crate::redact::Secret;

pub const MAX_OUT_FRAME: usize = 1024;
pub const MAX_IN_FRAME: usize = 65_535;
pub const MAX_MESSAGE: usize = 1 << 20;

/// The Poly1305 tag after every frame's ciphertext.
const TAG_LEN: usize = 16;

/// A frame that can't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HapError {
    /// The tag doesn't verify (a forged or corrupted frame: the session is lost).
    BadTag,
    /// A length over the cap.
    TooLong,
}

/// HKDF output length: every HAP key is 32 bytes.
struct Len32;

impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

/// `HKDF-SHA512(ikm, salt, info)` → 32 bytes (every HAP key: pair-setup, pair-verify, control,
/// events).
pub fn hkdf_sha512(ikm: &[u8], salt: &[u8], info: &[u8]) -> Secret<[u8; 32]> {
    let mut out = [0; 32];
    hkdf::Salt::new(HKDF_SHA512, salt).extract(ikm).expand(&[info], Len32).and_then(|okm| okm.fill(&mut out)).expect("HKDF-SHA512 yields 32 bytes");
    Secret::new(out)
}

fn aead_key(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("a 32-byte ChaCha20-Poly1305 key"))
}

/// 4 zero bytes ‖ `tail` (the counter or the label).
fn nonce(tail: [u8; 8]) -> Nonce {
    let mut n = [0; NONCE_LEN];
    n[4..].copy_from_slice(&tail);
    Nonce::assume_unique_for_key(n)
}

/// Seal one pairing message under a fixed nonce label (`PS-Msg05`, `PV-Msg03`, …).
pub(crate) fn seal_labelled(key: &Secret<[u8; 32]>, label: &[u8; 8], plain: &[u8]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    aead_key(key.expose()).seal_in_place_append_tag(nonce(*label), Aad::empty(), &mut buf).expect("ChaCha20-Poly1305 seals any pairing message");
    buf
}

/// Open one pairing message sealed under `label`; `None` when the tag doesn't verify.
pub(crate) fn open_labelled(key: &Secret<[u8; 32]>, label: &[u8; 8], sealed: &[u8]) -> Option<Vec<u8>> {
    let mut buf = sealed.to_vec();
    let len = aead_key(key.expose()).open_in_place(nonce(*label), Aad::empty(), &mut buf).ok()?.len();
    buf.truncate(len);
    Some(buf)
}

/// Both directions of an encrypted connection.
pub struct FrameCipher {
    write: LessSafeKey,
    read: LessSafeKey,
    write_count: u64,
    read_count: u64,
}

impl FrameCipher {
    /// `write_key` seals what we send, `read_key` opens what we receive (the control channel:
    /// `Control-Write-…` / `Control-Read-…`; the event channel swaps them).
    pub fn new(write_key: &Secret<[u8; 32]>, read_key: &Secret<[u8; 32]>) -> Self {
        Self { write: aead_key(write_key.expose()), read: aead_key(read_key.expose()), write_count: 0, read_count: 0 }
    }

    /// Seal `plain` into one or more frames.
    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let frames = plain.len().div_ceil(MAX_OUT_FRAME);
        let mut out = Vec::with_capacity(plain.len() + frames * (2 + TAG_LEN));
        for chunk in plain.chunks(MAX_OUT_FRAME) {
            let len = (chunk.len() as u16).to_le_bytes();
            let mut buf = chunk.to_vec();
            self.write
                .seal_in_place_append_tag(nonce(self.write_count.to_le_bytes()), Aad::from(len), &mut buf)
                .expect("ChaCha20-Poly1305 seals a 1024-byte frame");
            self.write_count += 1;
            out.extend_from_slice(&len);
            out.extend_from_slice(&buf);
        }
        out
    }

    /// Open the first complete frame in `input` (consumed from it); `Ok(None)`: need more bytes.
    pub fn open(&mut self, input: &mut Vec<u8>) -> Result<Option<Vec<u8>>, HapError> {
        let Some(&[lo, hi]) = input.first_chunk::<2>() else { return Ok(None) };
        let len = usize::from(u16::from_le_bytes([lo, hi]));
        if len > MAX_IN_FRAME {
            return Err(HapError::TooLong);
        }
        let end = 2 + len + TAG_LEN;
        if input.len() < end {
            return Ok(None);
        }
        let mut buf = input[2..end].to_vec();
        input.drain(..end);
        let plain_len =
            self.read.open_in_place(nonce(self.read_count.to_le_bytes()), Aad::from([lo, hi]), &mut buf).map_err(|_| HapError::BadTag)?.len();
        self.read_count += 1;
        buf.truncate(plain_len);
        Ok(Some(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::airplay::pair::golden::{self, pattern, unhex};

    fn key(v: &serde_json::Value) -> Secret<[u8; 32]> {
        Secret::new(unhex(v).try_into().unwrap())
    }

    #[test]
    fn hkdf_labels_match_pyatv() {
        let g = golden::get();
        let ikm = |name: &str| match name {
            "srp_k" => unhex(&g["srp"]["K"]),
            _ => unhex(&g["pair_verify"]["shared"]),
        };
        for case in g["hkdf"].as_array().unwrap() {
            let s = |k: &str| case[k].as_str().unwrap();
            let out = hkdf_sha512(&ikm(s("ikm")), s("salt").as_bytes(), s("info").as_bytes());
            assert_eq!(out.expose().as_slice(), unhex(&case["out"]), "{} / {}", s("salt"), s("info"));
        }
    }

    /// Our 1500-byte write is pyatv's 1024 + 476 frames; the TV's 1500-byte and 16 KiB single
    /// frames (longer than pyatv ever writes) open in order.
    #[test]
    fn frames_match_pyatv_both_ways() {
        let hap = &golden::get()["hap"];
        let (write, read) = (key(&hap["write_key"]), key(&hap["read_key"]));
        let mut ours = FrameCipher::new(&write, &read);
        assert_eq!(ours.seal(&pattern(1500, 0)), unhex(&hap["sealed"]));

        let mut input = unhex(&hap["inbound"]);
        let tail = input.split_off(input.len() - 100);
        assert_eq!(ours.open(&mut input), Ok(Some(pattern(1500, 1500))));
        assert_eq!(ours.open(&mut input), Ok(None), "the second frame is incomplete");
        input.extend_from_slice(&tail);
        assert_eq!(ours.open(&mut input), Ok(Some(pattern(16384, 16384))));
        assert!(input.is_empty());

        // the TV's side of the same connection reads our frames
        let mut tv = FrameCipher::new(&read, &write);
        let mut sealed = unhex(&hap["sealed"]);
        assert_eq!(tv.open(&mut sealed).unwrap().unwrap().len(), MAX_OUT_FRAME);
        assert_eq!(tv.open(&mut sealed).unwrap().unwrap().len(), 476);
    }

    #[test]
    fn a_tampered_frame_is_a_bad_tag() {
        let (a, b) = (Secret::new([1; 32]), Secret::new([2; 32]));
        let mut frames = FrameCipher::new(&a, &b).seal(b"POST /command RTSP/1.0\r\n\r\n");
        frames[5] ^= 1;
        assert_eq!(FrameCipher::new(&b, &a).open(&mut frames), Err(HapError::BadTag));
        // a length header claiming more than the frame: waits for bytes, never reads past them
        let mut short = vec![0xFF, 0xFF, 0, 0];
        assert_eq!(FrameCipher::new(&b, &a).open(&mut short), Ok(None));
    }

    #[test]
    fn labelled_messages_round_trip_and_reject_another_label() {
        let k = Secret::new([5; 32]);
        let sealed = seal_labelled(&k, b"PS-Msg05", b"tlv");
        assert_eq!(open_labelled(&k, b"PS-Msg05", &sealed).as_deref(), Some(&b"tlv"[..]));
        assert_eq!(open_labelled(&k, b"PS-Msg06", &sealed), None);
    }
}
