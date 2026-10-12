//! Binary property lists (`bplist00`; DESIGN §40 §7.1). OWNER: CAST-1. Hand-rolled (the `plist`
//! crate would bring `time` and maybe a second `quick-xml`). The input is untrusted: the reader
//! caps size ([`MAX_SIZE`]), nesting ([`MAX_DEPTH`]) and offsets, and never panics.
//!
//! The writer lays objects out as Python's `plistlib` does (what pyatv's `encode_plist_body`
//! sends, golden bytes in `tests/fixtures/cast/vectors/pyatv_0_18_0.json`): objects numbered
//! depth-first with a dict's keys before its values, equal scalars (strings, data, numbers,
//! booleans, dates) written once and shared, containers never shared; the smallest integer
//! encoding (negative numbers as 8 bytes); ASCII strings as type 5, others as UTF-16BE type 6;
//! references and offsets in the fewest bytes that fit.
//!
//! The reader takes every `bplist00` object type the TVs send: booleans, integers of 1–16 bytes
//! (16-byte ones keep their low 64 bits, so an unsigned 64-bit feature mask survives as
//! `Int(x as i64)`), 4- and 8-byte reals, dates, data, ASCII / UTF-16 strings, UIDs, arrays,
//! sets (read as arrays) and dicts with string keys. Besides [`MAX_SIZE`] and [`MAX_DEPTH`], the
//! decoded tree is capped at [`MAX_DECODED`] bytes of cost (each value 32 + its payload), so a
//! small file that references one big object many times can't balloon in memory. A `null`, a
//! non-string dict key, a reference cycle (caught by the depth cap) or any offset outside the
//! object area is [`BplistError::Malformed`].

pub const MAX_SIZE: usize = 1 << 20;
pub const MAX_DEPTH: usize = 32;
/// Decoded-tree budget (see the module docs).
pub const MAX_DECODED: usize = 16 << 20;

/// A plist value. Dicts keep their order (the golden bytes depend on it).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Real(f64),
    String(String),
    Data(Vec<u8>),
    /// Seconds since 2001-01-01.
    Date(f64),
    Array(Vec<Value>),
    Dict(Vec<(String, Value)>),
    Uid(u64),
}

impl Value {
    /// `dict[key]`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Malformed or over a cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BplistError {
    Malformed,
    TooLarge,
    TooDeep,
}

const MAGIC: &[u8; 8] = b"bplist00";
const TRAILER_LEN: usize = 32;

// ---- writer

/// A scalar's identity for sharing (plistlib's `(type(value), value)`; reals by bit pattern).
#[derive(PartialEq)]
enum Scalar<'a> {
    Bool(bool),
    Int(i64),
    Real(u64),
    Date(u64),
    String(&'a str),
    Data(&'a [u8]),
}

/// One object to write: a scalar (shared) or a container (its children's object numbers).
enum Obj<'a> {
    Scalar(&'a Value),
    Key(&'a str),
    Array(Vec<usize>),
    Dict(Vec<usize>, Vec<usize>),
}

struct Writer<'a> {
    objs: Vec<Obj<'a>>,
    scalars: Vec<(Scalar<'a>, usize)>,
}

impl<'a> Writer<'a> {
    /// The object number of a scalar, adding it when it's new.
    fn scalar(&mut self, key: Scalar<'a>, obj: Obj<'a>) -> usize {
        if let Some((_, n)) = self.scalars.iter().find(|(k, _)| *k == key) {
            return *n;
        }
        self.objs.push(obj);
        self.scalars.push((key, self.objs.len() - 1));
        self.objs.len() - 1
    }

    /// Number `v` and everything under it, depth-first (plistlib `_flatten`).
    fn flatten(&mut self, v: &'a Value) -> usize {
        match v {
            Value::Bool(b) => self.scalar(Scalar::Bool(*b), Obj::Scalar(v)),
            Value::Int(i) => self.scalar(Scalar::Int(*i), Obj::Scalar(v)),
            Value::Real(r) => self.scalar(Scalar::Real(r.to_bits()), Obj::Scalar(v)),
            Value::Date(d) => self.scalar(Scalar::Date(d.to_bits()), Obj::Scalar(v)),
            Value::String(s) => self.scalar(Scalar::String(s), Obj::Scalar(v)),
            Value::Data(d) => self.scalar(Scalar::Data(d), Obj::Scalar(v)),
            Value::Uid(_) => {
                self.objs.push(Obj::Scalar(v));
                self.objs.len() - 1
            }
            Value::Array(items) => {
                let me = self.objs.len();
                self.objs.push(Obj::Array(Vec::new()));
                let refs = items.iter().map(|i| self.flatten(i)).collect();
                self.objs[me] = Obj::Array(refs);
                me
            }
            Value::Dict(entries) => {
                let me = self.objs.len();
                self.objs.push(Obj::Dict(Vec::new(), Vec::new()));
                let keys = entries.iter().map(|(k, _)| self.scalar(Scalar::String(k), Obj::Key(k))).collect();
                let vals = entries.iter().map(|(_, v)| self.flatten(v)).collect();
                self.objs[me] = Obj::Dict(keys, vals);
                me
            }
        }
    }
}

/// Bytes needed for `n` (plistlib `_count_to_size`).
fn width(n: usize) -> usize {
    match n {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    }
}

fn put_uint(out: &mut Vec<u8>, v: u64, width: usize) {
    out.extend_from_slice(&v.to_be_bytes()[8 - width..]);
}

/// An object marker with its count (plistlib `_write_size`): in the low nibble below 15, else
/// `0xF` and an integer object.
fn put_marker(out: &mut Vec<u8>, kind: u8, count: usize) {
    if count < 15 {
        out.push(kind | count as u8);
    } else {
        out.push(kind | 0xF);
        let w = width(count);
        out.push(0x10 | w.trailing_zeros() as u8);
        put_uint(out, count as u64, w);
    }
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        put_marker(out, 0x50, s.len());
        out.extend_from_slice(s.as_bytes());
    } else {
        let units: Vec<u16> = s.encode_utf16().collect();
        put_marker(out, 0x60, units.len());
        out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
    }
}

fn put_scalar(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Bool(b) => out.push(if *b { 0x09 } else { 0x08 }),
        Value::Int(i) if *i < 0 => {
            out.push(0x13);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Int(i) => {
            let w = match *i as u64 {
                0..=0xFF => 1,
                0x100..=0xFFFF => 2,
                0x1_0000..=0xFFFF_FFFF => 4,
                _ => 8,
            };
            out.push(0x10 | (w as u32).trailing_zeros() as u8);
            put_uint(out, *i as u64, w);
        }
        Value::Real(r) => {
            out.push(0x23);
            out.extend_from_slice(&r.to_be_bytes());
        }
        Value::Date(d) => {
            out.push(0x33);
            out.extend_from_slice(&d.to_be_bytes());
        }
        Value::String(s) => put_string(out, s),
        Value::Data(d) => {
            put_marker(out, 0x40, d.len());
            out.extend_from_slice(d);
        }
        Value::Uid(u) => {
            let w = width(usize::try_from(*u).unwrap_or(usize::MAX));
            out.push(0x80 | (w - 1) as u8);
            put_uint(out, *u, w);
        }
        Value::Array(_) | Value::Dict(_) => {}
    }
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut w = Writer { objs: Vec::new(), scalars: Vec::new() };
    w.flatten(v);
    let ref_width = width(w.objs.len());
    let mut out = MAGIC.to_vec();
    let mut offsets = Vec::with_capacity(w.objs.len());
    for obj in &w.objs {
        offsets.push(out.len());
        match obj {
            Obj::Scalar(v) => put_scalar(&mut out, v),
            Obj::Key(k) => put_string(&mut out, k),
            Obj::Array(refs) => {
                put_marker(&mut out, 0xA0, refs.len());
                refs.iter().for_each(|r| put_uint(&mut out, *r as u64, ref_width));
            }
            Obj::Dict(keys, vals) => {
                put_marker(&mut out, 0xD0, keys.len());
                keys.iter().chain(vals).for_each(|r| put_uint(&mut out, *r as u64, ref_width));
            }
        }
    }
    let table = out.len();
    let offset_width = width(table);
    offsets.iter().for_each(|o| put_uint(&mut out, *o as u64, offset_width));
    out.extend_from_slice(&[0; 6]);
    out.push(offset_width as u8);
    out.push(ref_width as u8);
    out.extend_from_slice(&(w.objs.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes()); // the top object is always the first
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out
}

// ---- reader

struct Reader<'a> {
    buf: &'a [u8],
    offsets: Vec<usize>,
    ref_width: usize,
    /// What's left of [`MAX_DECODED`].
    budget: usize,
}

fn uint_be(b: &[u8]) -> u64 {
    b.iter().fold(0, |acc, x| (acc << 8) | u64::from(*x))
}

impl Reader<'_> {
    fn bytes(&self, at: usize, len: usize) -> Result<&[u8], BplistError> {
        at.checked_add(len).and_then(|end| self.buf.get(at..end)).ok_or(BplistError::Malformed)
    }

    fn spend(&mut self, payload: usize) -> Result<(), BplistError> {
        self.budget = self.budget.checked_sub(32 + payload).ok_or(BplistError::TooLarge)?;
        Ok(())
    }

    /// An object's count and where its payload starts: the low nibble, or the integer object
    /// after an `0xF` nibble.
    fn count(&self, at: usize, nibble: u8) -> Result<(usize, usize), BplistError> {
        if nibble != 0xF {
            return Ok((usize::from(nibble), at + 1));
        }
        let marker = *self.bytes(at + 1, 1)?.first().ok_or(BplistError::Malformed)?;
        if marker >> 4 != 0x1 || marker & 0xF > 3 {
            return Err(BplistError::Malformed);
        }
        let w = 1usize << (marker & 0xF);
        let n = usize::try_from(uint_be(self.bytes(at + 2, w)?)).map_err(|_| BplistError::Malformed)?;
        Ok((n, at + 2 + w))
    }

    fn refs(&self, at: usize, n: usize) -> Result<Vec<usize>, BplistError> {
        let len = n.checked_mul(self.ref_width).ok_or(BplistError::Malformed)?;
        let raw = self.bytes(at, len)?;
        Ok(raw.chunks(self.ref_width).map(|c| usize::try_from(uint_be(c)).unwrap_or(usize::MAX)).collect())
    }

    fn object(&mut self, index: usize, depth: usize) -> Result<Value, BplistError> {
        if depth > MAX_DEPTH {
            return Err(BplistError::TooDeep);
        }
        let at = *self.offsets.get(index).ok_or(BplistError::Malformed)?;
        let marker = *self.bytes(at, 1)?.first().ok_or(BplistError::Malformed)?;
        let (kind, nibble) = (marker >> 4, marker & 0xF);
        self.spend(0)?;
        Ok(match (kind, nibble) {
            (0x0, 0x8) => Value::Bool(false),
            (0x0, 0x9) => Value::Bool(true),
            (0x1, 0..=4) => {
                let raw = self.bytes(at + 1, 1 << nibble)?;
                Value::Int(match raw.len() {
                    16 => uint_be(&raw[8..]) as i64,
                    8 => i64::from_be_bytes(raw.try_into().map_err(|_| BplistError::Malformed)?),
                    _ => uint_be(raw) as i64,
                })
            }
            (0x2, 2) => Value::Real(f64::from(f32::from_be_bytes(self.bytes(at + 1, 4)?.try_into().map_err(|_| BplistError::Malformed)?))),
            (0x2, 3) => Value::Real(f64::from_be_bytes(self.bytes(at + 1, 8)?.try_into().map_err(|_| BplistError::Malformed)?)),
            (0x3, 3) => Value::Date(f64::from_be_bytes(self.bytes(at + 1, 8)?.try_into().map_err(|_| BplistError::Malformed)?)),
            (0x4, _) => {
                let (n, start) = self.count(at, nibble)?;
                self.spend(n)?;
                Value::Data(self.bytes(start, n)?.to_vec())
            }
            (0x5, _) => {
                let (n, start) = self.count(at, nibble)?;
                self.spend(n)?;
                // ASCII by definition; a stray high byte is kept as its Latin-1 character
                Value::String(self.bytes(start, n)?.iter().map(|b| char::from(*b)).collect())
            }
            (0x6, _) => {
                let (n, start) = self.count(at, nibble)?;
                let len = n.checked_mul(2).ok_or(BplistError::Malformed)?;
                self.spend(len)?;
                let units: Vec<u16> = self.bytes(start, len)?.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                Value::String(String::from_utf16(&units).map_err(|_| BplistError::Malformed)?)
            }
            (0x8, 0..=7) => Value::Uid(uint_be(self.bytes(at + 1, usize::from(nibble) + 1)?)),
            (0xA | 0xC, _) => {
                let (n, start) = self.count(at, nibble)?;
                let refs = self.refs(start, n)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for r in refs {
                    items.push(self.object(r, depth + 1)?);
                }
                Value::Array(items)
            }
            (0xD, _) => {
                let (n, start) = self.count(at, nibble)?;
                let keys = self.refs(start, n)?;
                let vals = self.refs(start + n * self.ref_width, n)?;
                let mut entries = Vec::with_capacity(n.min(1024));
                for (k, v) in keys.into_iter().zip(vals) {
                    let Value::String(key) = self.object(k, depth + 1)? else { return Err(BplistError::Malformed) };
                    entries.push((key, self.object(v, depth + 1)?));
                }
                Value::Dict(entries)
            }
            _ => return Err(BplistError::Malformed),
        })
    }
}

pub fn decode(buf: &[u8]) -> Result<Value, BplistError> {
    if buf.len() > MAX_SIZE {
        return Err(BplistError::TooLarge);
    }
    if buf.len() < MAGIC.len() + TRAILER_LEN || !buf.starts_with(MAGIC) {
        return Err(BplistError::Malformed);
    }
    let trailer = &buf[buf.len() - TRAILER_LEN..];
    let (offset_width, ref_width) = (usize::from(trailer[6]), usize::from(trailer[7]));
    let num = usize::try_from(uint_be(&trailer[8..16])).map_err(|_| BplistError::Malformed)?;
    let top = usize::try_from(uint_be(&trailer[16..24])).map_err(|_| BplistError::Malformed)?;
    let table = usize::try_from(uint_be(&trailer[24..32])).map_err(|_| BplistError::Malformed)?;
    let object_end = buf.len() - TRAILER_LEN;
    let valid_width = |w: usize| matches!(w, 1 | 2 | 4 | 8);
    let table_len = num.checked_mul(offset_width).ok_or(BplistError::Malformed)?;
    if !valid_width(offset_width)
        || !valid_width(ref_width)
        || num == 0
        || top >= num
        || table < MAGIC.len()
        || table.checked_add(table_len) != Some(object_end)
    {
        return Err(BplistError::Malformed);
    }
    let offsets = buf[table..object_end]
        .chunks(offset_width)
        .map(|c| usize::try_from(uint_be(c)).ok().filter(|o| (MAGIC.len()..table).contains(o)).ok_or(BplistError::Malformed))
        .collect::<Result<Vec<_>, _>>()?;
    Reader { buf: &buf[..table], offsets, ref_width, budget: MAX_DECODED }.object(top, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::airplay::pair::golden::{self, unhex};

    /// The vectors' tagged JSON (`{"dict": [[k, v], …]}`, `{"int": n}`, …) as a [`Value`].
    fn value(j: &serde_json::Value) -> Value {
        let (tag, v) = j.as_object().unwrap().iter().next().unwrap();
        match tag.as_str() {
            "bool" => Value::Bool(v.as_bool().unwrap()),
            "int" => Value::Int(v.as_i64().unwrap()),
            "real" => Value::Real(v.as_f64().unwrap()),
            "date" => Value::Date(v.as_f64().unwrap()),
            "string" => Value::String(v.as_str().unwrap().to_owned()),
            "data" => Value::Data(unhex(v)),
            "uid" => Value::Uid(v.as_u64().unwrap()),
            "array" => Value::Array(v.as_array().unwrap().iter().map(value).collect()),
            "dict" => Value::Dict(v.as_array().unwrap().iter().map(|e| (e[0].as_str().unwrap().to_owned(), value(&e[1]))).collect()),
            other => panic!("unknown tag {other}"),
        }
    }

    /// pyatv's `encode_plist_body` (plistlib) bytes, both ways: `{streams: []}`, a nested
    /// `{params: {data: <bplist>}}` command, and every scalar width and type.
    #[test]
    fn pyatv_bodies_both_ways() {
        for case in golden::get()["bplist"].as_array().unwrap() {
            let v = value(&case["value"]);
            let wire = unhex(&case["encoded"]);
            assert_eq!(encode(&v), wire, "{v:?}");
            assert_eq!(decode(&wire), Ok(v));
        }
    }

    #[test]
    fn round_trips_what_casting_sends() {
        let item = Value::Dict(vec![
            ("uuid".into(), Value::String("6F1C2A3B-0000-4000-8000-0000000000AA".into())),
            ("Start-Position".into(), Value::Dict(vec![("flags".into(), Value::Int(1)), ("value".into(), Value::Int(0))])),
            ("rate".into(), Value::Real(-0.5)),
            ("big".into(), Value::Int(i64::MAX)),
            ("neg".into(), Value::Int(i64::MIN)),
            ("name".into(), Value::String("Wohnzimmer – 4K ✓".into())),
            ("blob".into(), Value::Data(vec![9; 300])),
            ("list".into(), Value::Array((0..40).map(Value::Int).collect())),
        ]);
        assert_eq!(decode(&encode(&item)), Ok(item));
    }

    /// The two scrubbed LAN captures (Apple's own writer).
    #[test]
    fn real_info_plists_decode() {
        let atv = decode(include_bytes!("../../../tests/fixtures/cast/info_appletv_26_6.plist")).unwrap();
        assert_eq!(atv.get("model"), Some(&Value::String("AppleTV14,1".into())));
        assert_eq!(atv.get("statusFlags"), Some(&Value::Int(0x118644)));
        assert_eq!(atv.get("supportedFormats").and_then(|f| f.get("bufferStream")), Some(&Value::Int(-577021992844656640)));
        let his = decode(include_bytes!("../../../tests/fixtures/cast/info_hisense.plist")).unwrap();
        let Some(Value::Array(displays)) = his.get("displays") else { panic!("displays") };
        assert_eq!(displays[0].get("widthPixels"), Some(&Value::Int(1920)));
    }

    #[test]
    fn hostile_input_is_refused_without_panicking() {
        let good = encode(&Value::Dict(vec![("k".into(), Value::Array(vec![Value::String("v".into())]))]));
        assert_eq!(decode(&vec![0; MAX_SIZE + 1]), Err(BplistError::TooLarge));
        assert_eq!(decode(b"bplist00"), Err(BplistError::Malformed));
        // every truncation and every single-byte corruption: an error or a value, never a panic
        for n in 0..good.len() {
            assert!(decode(&good[..n]).is_err());
            for flip in [0x01, 0x80, 0xFF] {
                let mut bad = good.clone();
                bad[n] ^= flip;
                let _ = decode(&bad);
            }
        }
        // an array that contains itself: the depth cap
        let mut cyclic = encode(&Value::Array(vec![Value::Bool(true)]));
        let marker = cyclic.iter().position(|b| *b == 0xA1).unwrap();
        cyclic[marker + 1] = 0; // its one reference → object 0, itself
        assert_eq!(decode(&cyclic), Err(BplistError::TooDeep));
    }

    /// A DAG that doubles at every level: 2^31 leaves from a few hundred bytes. The budget
    /// stops it long before the memory does.
    #[test]
    fn shared_references_cannot_balloon() {
        // object i (< 31) is [i+1, i+1]; object 31 is 64 KiB of data
        let mut buf = MAGIC.to_vec();
        let mut offsets = Vec::new();
        for i in 0..31u8 {
            offsets.push(buf.len());
            buf.extend_from_slice(&[0xA2, i + 1, i + 1]);
        }
        offsets.push(buf.len());
        put_marker(&mut buf, 0x40, 1 << 16);
        buf.extend(std::iter::repeat_n(7, 1 << 16));
        let table = buf.len();
        offsets.iter().for_each(|o| put_uint(&mut buf, *o as u64, 4));
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 4, 1]);
        buf.extend_from_slice(&32u64.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&(table as u64).to_be_bytes());
        assert_eq!(decode(&buf), Err(BplistError::TooLarge));
    }
}
