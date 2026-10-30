//! CASTV2 framing (DESIGN §40 §15.3, Phase 2): the 7-field `CastMessage` protobuf, hand-coded
//! (no prost), behind a 4-byte big-endian length on the TLS connection to port 8009.
//!
//! ```text
//! message CastMessage {                       // cast_channel.proto (Chromium)
//!   required ProtocolVersion protocol_version = 1;   // CASTV2_1_0 = 0
//!   required string source_id = 2;
//!   required string destination_id = 3;
//!   required string namespace = 4;
//!   required PayloadType payload_type = 5;           // STRING = 0, BINARY = 1
//!   optional string payload_utf8 = 6;
//!   optional bytes payload_binary = 7;
//! }
//! ```
//! Every payload Tuner sends or reads is JSON text (`payload_utf8`); a binary payload (device
//! auth, which a sender may skip) is decoded and then ignored by the client. Replies are
//! untrusted: a frame over [`MAX_FRAME`], a truncated varint or field, or a wire type the message
//! doesn't use is [`Malformed`] and ends the connection.

/// The longest frame read or written (the receivers' own limit is 64 KiB).
pub const MAX_FRAME: usize = 64 * 1024;

/// A frame or message that doesn't decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// One decoded message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    /// The JSON text (`None`: a binary payload).
    pub payload: Option<String>,
}

impl CastMessage {
    pub fn json(source: &str, destination: &str, namespace: &str, payload: &serde_json::Value) -> Self {
        Self { source: source.to_owned(), destination: destination.to_owned(), namespace: namespace.to_owned(), payload: Some(payload.to_string()) }
    }

    /// The payload as JSON (`Value::Null` for a binary one or bad JSON).
    pub fn value(&self) -> serde_json::Value {
        self.payload.as_deref().and_then(|p| serde_json::from_str(p).ok()).unwrap_or(serde_json::Value::Null)
    }

    /// The message's bytes, with the length prefix.
    pub fn frame(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64 + self.payload.as_ref().map_or(0, String::len));
        varint_field(&mut body, 1, 0);
        bytes_field(&mut body, 2, self.source.as_bytes());
        bytes_field(&mut body, 3, self.destination.as_bytes());
        bytes_field(&mut body, 4, self.namespace.as_bytes());
        match &self.payload {
            Some(p) => {
                varint_field(&mut body, 5, 0);
                bytes_field(&mut body, 6, p.as_bytes());
            }
            None => {
                varint_field(&mut body, 5, 1);
                bytes_field(&mut body, 7, &[]);
            }
        }
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// One message body (the length prefix already taken off).
    pub fn decode(mut b: &[u8]) -> Result<Self, Malformed> {
        let mut m = CastMessage::default();
        let mut binary = false;
        while !b.is_empty() {
            let key = varint(&mut b)?;
            let (field, wire) = (key >> 3, key & 7);
            match wire {
                0 => {
                    let v = varint(&mut b)?;
                    if field == 5 {
                        binary = v == 1;
                    }
                }
                2 => {
                    let len = usize::try_from(varint(&mut b)?).map_err(|_| Malformed)?;
                    if len > b.len() {
                        return Err(Malformed);
                    }
                    let (v, rest) = b.split_at(len);
                    b = rest;
                    let text = || String::from_utf8(v.to_vec()).map_err(|_| Malformed);
                    match field {
                        2 => m.source = text()?,
                        3 => m.destination = text()?,
                        4 => m.namespace = text()?,
                        6 => m.payload = Some(text()?),
                        _ => {}
                    }
                }
                _ => return Err(Malformed),
            }
        }
        if binary {
            m.payload = None;
        }
        Ok(m)
    }
}

/// The length a frame header announces (`Err` beyond [`MAX_FRAME`]).
pub fn frame_len(header: [u8; 4]) -> Result<usize, Malformed> {
    let n = u32::from_be_bytes(header) as usize;
    if n > MAX_FRAME { Err(Malformed) } else { Ok(n) }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_field(out: &mut Vec<u8>, field: u64, v: u64) {
    put_varint(out, field << 3);
    put_varint(out, v);
}

fn bytes_field(out: &mut Vec<u8>, field: u64, v: &[u8]) {
    put_varint(out, field << 3 | 2);
    put_varint(out, v.len() as u64);
    out.extend_from_slice(v);
}

fn varint(b: &mut &[u8]) -> Result<u64, Malformed> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&byte, rest) = b.split_first().ok_or(Malformed)?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes pychromecast's protobuf build writes for a CONNECT (checked by hand against the
    /// proto's field numbers: `08 00 12 08 "sender-0" 1a 0a "receiver-0" 22 … 28 00 32 …`).
    #[test]
    fn a_message_frames_like_protobuf_and_decodes_back() {
        let ns = "urn:x-cast:com.google.cast.tp.connection";
        let m = CastMessage::json("sender-0", "receiver-0", ns, &serde_json::json!({"type": "CONNECT"}));
        let f = m.frame();
        let len = frame_len(f[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, f.len() - 4);
        let body = &f[4..];
        assert_eq!(&body[..4], &[0x08, 0x00, 0x12, 0x08]);
        assert_eq!(&body[4..12], b"sender-0");
        assert_eq!(&body[12..14], &[0x1a, 0x0a]);
        assert_eq!(CastMessage::decode(body), Ok(m.clone()));
        assert_eq!(m.value()["type"], "CONNECT");
    }

    #[test]
    fn untrusted_frames_are_refused() {
        assert_eq!(frame_len([0, 1, 0, 1]), Err(Malformed), "over 64 KiB");
        assert_eq!(CastMessage::decode(&[0x12, 0x05, b'a']), Err(Malformed), "a field longer than the frame");
        assert_eq!(CastMessage::decode(&[0x08, 0xff]), Err(Malformed), "a truncated varint");
        assert_eq!(CastMessage::decode(&[0x0d, 0, 0, 0, 0]), Err(Malformed), "a fixed32 wire type");
        assert_eq!(CastMessage::decode(&[0x12, 0x01, 0xff]), Err(Malformed), "not UTF-8");
        // unknown fields are skipped, a binary payload reads as none
        let m = CastMessage::decode(&[0x52, 0x01, b'x', 0x28, 0x01, 0x3a, 0x01, 0x00]).unwrap();
        assert_eq!((m.payload.clone(), m.value()), (None, serde_json::Value::Null));
    }
}
