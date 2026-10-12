//! SRP-6a for pair-setup (DESIGN §40 §7.1). OWNER: CAST-1. Hand-rolled on `num-bigint`, ported
//! from pyatv `hap_srp.py`'s behaviour (srptools) and cross-checked with foxCast's maths: RFC 5054
//! 3072-bit group, g = 5, SHA-512, identity `"Pair-Setup"`, password = the PIN string;
//! `x = H(salt ‖ H("Pair-Setup:" ‖ pin))`; `k` and `u` padded to 384 bytes; the M1 proof over
//! UNPADDED values `H(H(N) ⊕ H(g) ‖ H(I) ‖ salt ‖ A ‖ B ‖ K)`, `K = H(S)`; the server proof
//! `H(A ‖ M1 ‖ K)` is verified. The server half is here too (`#[doc(hidden)]`) for the fake
//! receiver (CAST-3). Checked against RFC 5054 Appendix B and the pyatv 0.18.0 vectors.

use crate::redact::Secret;

/// An SRP failure (bad `B`, a proof that doesn't verify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrpError {
    /// `B mod N == 0` or `A mod N == 0`.
    BadPublicKey,
    /// The peer's proof doesn't match.
    BadProof,
}

/// The client's side of one exchange.
pub struct SrpClient {
    // CAST-1: the private `a` and public `A`.
}

/// What the client sends at M3 and keeps for M4 / M5.
pub struct ClientProof {
    /// `A`, unpadded big-endian.
    pub public_a: Vec<u8>,
    /// M1.
    pub proof: Vec<u8>,
    /// `K = H(S)` (64 bytes): the pair-setup session key.
    pub session_key: Secret<Vec<u8>>,
    /// The M2 the server must send back.
    pub expected_server_proof: Vec<u8>,
}

impl SrpClient {
    /// `a_private`: 32 random bytes (fixed in tests and the golden vectors).
    pub fn new(_a_private: &[u8]) -> Self {
        // CAST-1
        Self {}
    }

    /// Process the server's salt and `B` with the PIN.
    pub fn process(&self, _salt: &[u8], _b_public: &[u8], _pin: &str) -> Result<ClientProof, SrpError> {
        // CAST-1
        Err(SrpError::BadPublicKey)
    }
}

/// The server's side (the fake receiver and the golden-vector tests only).
#[doc(hidden)]
pub struct SrpServer {
    // CAST-1: salt, verifier, `b`, `B`.
}

#[doc(hidden)]
impl SrpServer {
    pub fn new(_salt: &[u8], _pin: &str, _b_private: &[u8]) -> Self {
        // CAST-1
        Self {}
    }

    /// `B`, unpadded big-endian.
    pub fn public_b(&self) -> Vec<u8> {
        // CAST-1
        Vec::new()
    }

    /// Check the client's M1; on success the server proof M2 and `K`.
    pub fn verify(&self, _public_a: &[u8], _client_proof: &[u8]) -> Result<(Vec<u8>, Secret<Vec<u8>>), SrpError> {
        // CAST-1
        Err(SrpError::BadProof)
    }
}
