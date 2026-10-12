//! Binary property lists (`bplist00`; DESIGN §40 §7.1). OWNER: CAST-1. Hand-rolled (the `plist`
//! crate would bring `time` and maybe a second `quick-xml`). The input is untrusted: the reader
//! caps size ([`MAX_SIZE`]), nesting ([`MAX_DEPTH`]) and offsets, and never panics.

pub const MAX_SIZE: usize = 1 << 20;
pub const MAX_DEPTH: usize = 32;

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

pub fn encode(_v: &Value) -> Vec<u8> {
    // CAST-1
    Vec::new()
}

pub fn decode(_buf: &[u8]) -> Result<Value, BplistError> {
    // CAST-1
    Err(BplistError::Malformed)
}
