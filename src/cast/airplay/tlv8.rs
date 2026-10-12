//! TLV8 (HAP pairing messages; DESIGN §40 §7.1). OWNER: CAST-1. Port from pyatv
//! `pyatv/auth/hap_tlv8.py` @ b277a4c8 (MIT): tags 0x00–0x0D, 0x11 Name, 0x13 Flags, plus 0x12
//! for the HKP 5 ACL. Values over 255 bytes are split on encode and joined on decode; unknown
//! tags are kept (readers ignore them: a 159-byte type-17 TLV in M6 never fails a pairing).

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
pub fn encode(_items: &[(u8, &[u8])]) -> Vec<u8> {
    // CAST-1
    Vec::new()
}

pub fn decode(_buf: &[u8]) -> Result<Tlv, Tlv8Error> {
    // CAST-1
    Err(Tlv8Error)
}
