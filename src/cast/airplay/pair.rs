//! Pair-setup and pair-verify as sans-I/O state machines (DESIGN §40 §7.2). OWNER: CAST-1. Port
//! from pyatv `protocols/airplay/auth/hap.py` + `auth/hap_srp.py` @ b277a4c8 (MIT); the
//! `X-Apple-Client-Name` header from PR 2894; the M4 proof / M6 signature checks (TODO in pyatv)
//! and the HKP 5 ACL from foxCast's protocol notes (facts only). The HTTP exchange is CAST-2's
//! (`super::PairingSession`, `super::rtsp`).
//!
//! Replies: an `Error` TLV is checked first (it comes with HTTP 200): 2 at M4 → `WrongPin`,
//! 3 → `Backoff`, others → `Refused`. Unknown TLVs are ignored. Pair-verify: an empty M2 →
//! `Refused { home_only: true }`; an id or signature mismatch → `Failed` (the caller maps it to
//! `PairingLost`). The server halves (`#[doc(hidden)]`) are for the fake receiver (CAST-3),
//! ported from pyatv `server_auth.py` and made strict.

use crate::cast::{Identity, PairError, PairVariant, PeerCredentials, Pin};
use crate::redact::Secret;

/// The headers every pairing request of `variant` carries (pin-start adds
/// `X-Apple-SupportedPINLengths: 4` on HKP 5; pair-verify adds `X-Apple-PD: 1` on HKP 5).
pub fn headers(_variant: PairVariant, _identity: &Identity) -> Vec<(&'static str, String)> {
    // CAST-1
    Vec::new()
}

/// The client's pair-setup (M1, M3, M5; checks M2, M4, M6).
pub struct PairSetup {
    // CAST-1
}

impl PairSetup {
    pub fn new(_identity: &Identity, _variant: PairVariant) -> Self {
        // CAST-1
        Self {}
    }

    /// M1 `{Method 0, State 1}`.
    pub fn m1(&mut self) -> Vec<u8> {
        // CAST-1
        Vec::new()
    }

    /// From M2 `{Salt, PublicKey}` and the PIN: M3 `{State 3, PublicKey, Proof}`.
    pub fn m3(&mut self, _m2: &[u8], _pin: &Pin) -> Result<Vec<u8>, PairError> {
        // CAST-1
        Err(PairError::Failed)
    }

    /// Verify M4's proof; M5 `{State 5, EncryptedData{Identifier, PublicKey, Signature[, ACL]}}`.
    pub fn m5(&mut self, _m4: &[u8]) -> Result<Vec<u8>, PairError> {
        // CAST-1
        Err(PairError::Failed)
    }

    /// Verify M6's signature: the TV's long-term credentials.
    pub fn finish(self, _m6: &[u8]) -> Result<PeerCredentials, PairError> {
        // CAST-1
        Err(PairError::Failed)
    }
}

/// The keys a verified session derives from.
pub struct SessionKeys {
    /// The X25519 shared secret.
    pub shared: Secret<[u8; 32]>,
}

impl SessionKeys {
    /// Control channel `(write, read)`: `HKDF(shared, "Control-Salt", "Control-Write-Encryption-Key")`
    /// / `"Control-Read-Encryption-Key"`.
    pub fn control(&self) -> (Secret<[u8; 32]>, Secret<[u8; 32]>) {
        // CAST-1
        (Secret::new([0; 32]), Secret::new([0; 32]))
    }

    /// Event channel `(write, read)` for us: `"Events-Salt"`, SWAPPED — we write with
    /// `"Events-Read-Encryption-Key"` and read with `"Events-Write-Encryption-Key"`.
    pub fn events(&self) -> (Secret<[u8; 32]>, Secret<[u8; 32]>) {
        // CAST-1
        (Secret::new([0; 32]), Secret::new([0; 32]))
    }
}

/// The client's pair-verify (every session).
pub struct PairVerify {
    // CAST-1
}

impl PairVerify {
    /// `ephemeral`: 32 random bytes (fixed in tests).
    pub fn new(_identity: &Identity, _peer: &PeerCredentials, _ephemeral: &[u8; 32]) -> Self {
        // CAST-1
        Self {}
    }

    /// M1 `{State 1, PublicKey}`.
    pub fn m1(&mut self) -> Vec<u8> {
        // CAST-1
        Vec::new()
    }

    /// Check M2 (id and signature); M3 `{State 3, EncryptedData}`.
    pub fn m3(&mut self, _m2: &[u8]) -> Result<Vec<u8>, PairError> {
        // CAST-1
        Err(PairError::Failed)
    }

    /// Check M4's status: the session keys.
    pub fn finish(self, _m4: &[u8]) -> Result<SessionKeys, PairError> {
        // CAST-1
        Err(PairError::Failed)
    }
}

/// The receiver's halves (the fake receiver, the interop and golden-vector tests). Frozen
/// signatures (CAST-0) so the fake receiver (CAST-3) builds in parallel with CAST-1, which fills
/// the bodies (strict: the SRP proof and the controller's signature are checked). Bodies reply
/// with TLV8 (an `Error` TLV on failure, as the TV does).
#[doc(hidden)]
pub mod server {

    use super::SessionKeys;
    use crate::cast::PeerCredentials;

    /// The TV's long-term identity in a test: its pairing id and Ed25519 seed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Accessory {
        pub id: String,
        pub seed: [u8; 32],
    }

    /// The TV's pair-setup (M1 → M2, M3 → M4, M5 → M6). Fixed `salt` and `b_private` make the
    /// golden-vector tests deterministic.
    pub struct PairSetupServer {
        // CAST-1
    }

    impl PairSetupServer {
        /// `pin`: the code the TV "shows".
        pub fn new(_accessory: &Accessory, _pin: &str, _salt: &[u8; 16], _b_private: &[u8; 32]) -> Self {
            // CAST-1
            Self {}
        }

        /// One `/pair-setup` request body → the reply body (M2, M4 or M6; an `Error` TLV — 2
        /// for a wrong PIN at M4 — when it fails).
        pub fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
            // CAST-1
            Vec::new()
        }

        /// The controller that paired (its id and long-term key, `variant` as the request
        /// said), once M5 verified.
        pub fn paired(&self) -> Option<PeerCredentials> {
            // CAST-1
            None
        }
    }

    /// The TV's pair-verify (M1 → M2, M3 → M4) against one known controller.
    pub struct PairVerifyServer {
        // CAST-1
    }

    impl PairVerifyServer {
        /// `controller`: who paired (`peer_id` / `peer_ltpk` here are the controller's);
        /// `ephemeral`: the TV's X25519 secret (fixed in tests).
        pub fn new(_accessory: &Accessory, _controller: &PeerCredentials, _ephemeral: &[u8; 32]) -> Self {
            // CAST-1
            Self {}
        }

        /// One `/pair-verify` request body → the reply body (M2 or M4).
        pub fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
            // CAST-1
            Vec::new()
        }

        /// The session's keys once M3 verified. The receiver uses them swapped: it reads with
        /// the controller's write key and writes with its read key.
        pub fn keys(&self) -> Option<SessionKeys> {
            // CAST-1
            None
        }
    }
}
