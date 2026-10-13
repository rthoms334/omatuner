// Ported from pyatv pyatv/auth/hap_tlv8.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! TLV8 (HAP pairing messages; DESIGN §40 §7.1). OWNER: CAST-1. Port from pyatv
//! `pyatv/auth/hap_tlv8.py` @ b277a4c8 (MIT): tags 0x00–0x0D, 0x11 Name, 0x13 Flags, plus 0x12
//! for the HKP 5 ACL. Values over 255 bytes are split on encode and joined on decode; unknown
//! tags are kept (readers ignore them: a 159-byte type-17 TLV in M6 never fails a pairing).
//!
//! Details fixed by the port (golden bytes in `tests/fixtures/cast/vectors/pyatv_0_18_0.json`):
//! * `encode` writes items in the given order, each value in 255-byte chunks under its tag
//!   (pyatv `write_tlv`). An empty value is written as `tag 0` (HAP); pyatv drops it, but no
//!   message Tuner sends has one.
//! * `decode` joins CONSECUTIVE items with the same tag into one value (fragments of one long
//!   value); pyatv's dict joins any repeat, which differs only for interleaved duplicates, which
//!   HAP separates with a 0xFF item anyway. A truncated item is an error, never a panic.

/// Tag numbers (pyatv `TlvValue`).
pub mod tag {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const BACK_OFF: u8 = 0x08;
    pub const CERTIFICATE: u8 = 0x09;
    pub const SIGNATURE: u8 = 0x0A;
    pub const PERMISSIONS: u8 = 0x0B;
    pub const FRAGMENT_DATA: u8 = 0x0C;
    pub const FRAGMENT_LAST: u8 = 0x0D;
    pub const NAME: u8 = 0x11;
    /// HKP 5: the ScreenCapture ACL (OPACK `{"com.apple.ScreenCapture": true}`).
    pub const ACL: u8 = 0x12;
    pub const FLAGS: u8 = 0x13;
}

/// A decoded message: (tag, value) in wire order, fragments joined, unknown tags kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tlv(pub Vec<(u8, Vec<u8>)>);

impl Tlv {
    /// The first value with `tag`.
    pub fn get(&self, tag: u8) -> Option<&[u8]> {
        self.0.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_slice())
    }
}

/// Malformed TLV8 (a truncated item).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv8Error;

/// Encode in order, splitting values over 255 bytes.
pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|(_, v)| v.len() + 2 * (v.len() / 255 + 1)).sum());
    for &(tag, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[tag, 0]);
        }
        for chunk in value.chunks(255) {
            out.push(tag);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

pub fn decode(buf: &[u8]) -> Result<Tlv, Tlv8Error> {
    let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut rest = buf;
    while let [tag, len, tail @ ..] = rest {
        let len = usize::from(*len);
        let value = tail.get(..len).ok_or(Tlv8Error)?;
        match items.last_mut() {
            Some((last, joined)) if last == tag => joined.extend_from_slice(value),
            _ => items.push((*tag, value.to_vec())),
        }
        rest = &tail[len..];
    }
    if rest.is_empty() { Ok(Tlv(items)) } else { Err(Tlv8Error) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::airplay::pair::golden;

    #[test]
    fn pyatv_encodings_round_trip() {
        for case in golden::get()["tlv8"].as_array().unwrap() {
            let items: Vec<(u8, Vec<u8>)> =
                case["items"].as_array().unwrap().iter().map(|i| (i[0].as_u64().unwrap() as u8, golden::unhex(&i[1]))).collect();
            let borrowed: Vec<(u8, &[u8])> = items.iter().map(|(t, v)| (*t, v.as_slice())).collect();
            let wire = golden::unhex(&case["encoded"]);
            assert_eq!(encode(&borrowed), wire);
            assert_eq!(decode(&wire).unwrap(), Tlv(items));
        }
    }

    #[test]
    fn fragments_join_and_unknown_tags_stay() {
        let long = vec![7u8; 600];
        let wire = encode(&[(tag::STATE, &[6]), (tag::ENCRYPTED_DATA, &long), (0x11, &[1; 159]), (0xFE, &[])]);
        // 600 bytes: three items of 255 + 255 + 90 under one tag
        assert_eq!(wire.len(), 3 + (600 + 6) + (159 + 2) + 2);
        let tlv = decode(&wire).unwrap();
        assert_eq!(tlv.get(tag::ENCRYPTED_DATA), Some(long.as_slice()));
        assert_eq!(tlv.get(0x11).map(<[u8]>::len), Some(159));
        assert_eq!(tlv.get(0xFE), Some(&[][..]));
        assert_eq!(tlv.get(tag::ERROR), None);
    }

    #[test]
    fn truncated_items_are_errors() {
        assert_eq!(decode(&[]), Ok(Tlv::default()));
        assert_eq!(decode(&[tag::STATE]), Err(Tlv8Error));
        assert_eq!(decode(&[tag::STATE, 2, 1]), Err(Tlv8Error));
    }
}
