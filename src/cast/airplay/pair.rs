// Ported from pyatv pyatv/protocols/airplay/auth/hap.py + pyatv/auth/hap_srp.py + pyatv/protocols/airplay/server_auth.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
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
//!
//! Messages, byte for byte pyatv's (checked against `tests/fixtures/cast/vectors/pyatv_0_18_0.json`):
//! * pair-setup M1 `{Method 0, State 1}`; M3 `{State 3, PublicKey A, Proof M1}`; M5 `{State 5,
//!   EncryptedData}` sealed with `HKDF(K, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")`
//!   under nonce `PS-Msg05`, holding `{Identifier, PublicKey LTPK, Signature, Name [, 0x12 ACL]}`,
//!   the signature over `HKDF(K, "Pair-Setup-Controller-Sign-Salt", "…-Info") ‖ id ‖ LTPK`. The
//!   0x11 `Name` (OPACK `{"name": "Tuner"}`, [`NAME_TLV`]) is always sent, as pyatv's AirPlay
//!   pairing handler does (it passes its name, `pyatv` by default, to `step3`; PR 2894 sends
//!   the same name as `X-Apple-Client-Name`), so the TV's paired-controller record is named.
//! * M4 must carry the server proof `H(A ‖ M1 ‖ K)`; M6 (nonce `PS-Msg06`) the TV's id, LTPK
//!   and a signature over `HKDF(K, "Pair-Setup-Accessory-Sign-Salt", "…-Info") ‖ id ‖ LTPK`.
//! * pair-verify M1 `{State 1, PublicKey}`; M2's sealed part (nonce `PV-Msg02`, key
//!   `HKDF(shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info")`) must name the stored
//!   `peer_id` and be signed by `peer_ltpk` over `serverPub ‖ id ‖ clientPub`; M3 `{State 3,
//!   EncryptedData}` (nonce `PV-Msg03`) signs `clientPub ‖ clientId ‖ serverPub`. M4 is checked
//!   for an `Error` TLV (pyatv's TODO); an empty M4 is accepted, as foxCast does.
//!
//! pyatv draws the SRP `a` from its per-pairing key; [`PairSetup::new`] draws a fresh random
//! one per attempt (see `srp`), [`PairSetup::with_srp_private`] pins it for the vectors.

use aws_lc_rs::agreement::{self, PrivateKey, X25519};
use aws_lc_rs::constant_time;
use aws_lc_rs::rand;
use aws_lc_rs::signature::{self, ED25519, Ed25519KeyPair, KeyPair};

use super::CLIENT_NAME;
use super::hap::{hkdf_sha512, open_labelled, seal_labelled};
use super::srp::{ClientProof, SrpClient};
use super::tlv8::{self, Tlv, tag};
use crate::cast::{Identity, PairError, PairVariant, PeerCredentials, Pin};
use crate::redact::Secret;

/// `User-Agent` of every pairing request (pyatv `_AIRPLAY_HEADERS`).
pub const USER_AGENT: &str = "AirPlay/320.20";
/// The pairing bodies' `Content-Type`.
pub const CONTENT_TYPE: &str = "application/octet-stream";
/// HKP 5's M5 extra, TLV 0x12: OPACK `{"com.apple.ScreenCapture": true}` (foxCast; pyatv's
/// `opack.pack` gives the same bytes, checked by `scripts/cast/pyatv_vectors.py`).
pub const SCREEN_CAPTURE_ACL: &[u8] = b"\xE1\x57com.apple.ScreenCapture\x01";
/// M5's 0x11 Name: OPACK `{"name": CLIENT_NAME}` (pyatv `hap_srp.step3(name=…)`; `opack.pack`
/// gives the same bytes, checked by `scripts/cast/pyatv_vectors.py`).
pub const NAME_TLV: &[u8] = b"\xE1\x44name\x45Tuner";

/// HAP error codes (pyatv `ErrorCode`).
mod code {
    pub const UNKNOWN: u8 = 0x01;
    pub const AUTHENTICATION: u8 = 0x02;
    pub const BACK_OFF: u8 = 0x03;
}

/// The headers every pairing request of `variant` carries (pin-start adds
/// `X-Apple-SupportedPINLengths: 4` on HKP 5; pair-verify adds `X-Apple-PD: 1` on HKP 5).
pub fn headers(variant: PairVariant, identity: &Identity) -> Vec<(&'static str, String)> {
    let mut h =
        vec![("X-Apple-HKP", if variant == PairVariant::Hkp5 { "5" } else { "3" }.to_owned()), ("X-Apple-Client-Name", CLIENT_NAME.to_owned())];
    if variant == PairVariant::Hkp5 {
        h.push(("X-Apple-Client-ID", identity.pairing_id.clone()));
    }
    h.push(("Content-Type", CONTENT_TYPE.to_owned()));
    h.push(("User-Agent", USER_AGENT.to_owned()));
    h
}

/// [`headers`] for `/pair-pin-start` (HKP 5 asks for a four-digit code).
pub fn pin_start_headers(variant: PairVariant, identity: &Identity) -> Vec<(&'static str, String)> {
    let mut h = headers(variant, identity);
    if variant == PairVariant::Hkp5 {
        h.push(("X-Apple-SupportedPINLengths", "4".to_owned()));
    }
    h
}

/// [`headers`] for `/pair-verify` (HKP 5 marks a paired device).
pub fn verify_headers(variant: PairVariant, identity: &Identity) -> Vec<(&'static str, String)> {
    let mut h = headers(variant, identity);
    if variant == PairVariant::Hkp5 {
        h.push(("X-Apple-PD", "1".to_owned()));
    }
    h
}

/// The long-term Ed25519 public key of `seed` (this install's LTPK; the fake's accessory key).
#[doc(hidden)]
pub fn ltpk(seed: &[u8; 32]) -> [u8; 32] {
    let mut pk = [0; 32];
    if let Ok(kp) = Ed25519KeyPair::from_seed_unchecked(seed) {
        pk.copy_from_slice(kp.public_key().as_ref());
    }
    pk
}

fn sign(seed: &[u8; 32], parts: &[&[u8]]) -> Result<Vec<u8>, PairError> {
    let kp = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| PairError::Failed)?;
    Ok(kp.sign(&parts.concat()).as_ref().to_vec())
}

fn signed_by(ltpk: &[u8], parts: &[&[u8]], sig: &[u8]) -> bool {
    signature::UnparsedPublicKey::new(&ED25519, ltpk).verify(&parts.concat(), sig).is_ok()
}

/// An X25519 secret and its public key.
fn x25519(secret: &[u8; 32]) -> Option<(PrivateKey, [u8; 32])> {
    let private = PrivateKey::from_private_key(&X25519, secret).ok()?;
    let public = private.compute_public_key().ok()?.as_ref().try_into().ok()?;
    Some((private, public))
}

fn agree(private: &PrivateKey, peer: &[u8]) -> Option<Secret<[u8; 32]>> {
    agreement::agree(private, agreement::UnparsedPublicKey::new(&X25519, peer), (), |s| s.try_into().map_err(|_| ())).ok().map(Secret::new)
}

/// A 32-byte value of a TLV (an X25519 or Ed25519 public key).
fn key32(tlv: &Tlv, t: u8) -> Result<[u8; 32], PairError> {
    tlv.get(t).and_then(|v| v.try_into().ok()).ok_or(PairError::Failed)
}

fn decode(body: &[u8]) -> Result<Tlv, PairError> {
    tlv8::decode(body).map_err(|_| PairError::Failed)
}

/// An `Error` TLV in a pair-setup reply (checked first: TVs send it with HTTP 200, pyatv PR
/// 2920). `wrong_pin`: at M4, where error 2 means the code didn't match.
fn setup_error(tlv: &Tlv, wrong_pin: bool) -> Result<(), PairError> {
    let Some(err) = tlv.get(tag::ERROR) else { return Ok(()) };
    Err(match err.first() {
        Some(&code::BACK_OFF) => PairError::Backoff { secs: tlv.get(tag::BACK_OFF).map(le_secs) },
        Some(&code::AUTHENTICATION) if wrong_pin => PairError::WrongPin,
        _ => PairError::Refused { home_only: false },
    })
}

/// A little-endian `BackOff` value in seconds (saturating).
fn le_secs(v: &[u8]) -> u32 {
    v.iter().rev().fold(0u64, |acc, b| acc.saturating_mul(256).saturating_add(u64::from(*b))).min(u64::from(u32::MAX)) as u32
}

fn hkdf(ikm: &[u8], salt: &str, info: &str) -> Secret<[u8; 32]> {
    hkdf_sha512(ikm, salt.as_bytes(), info.as_bytes())
}

fn setup_encrypt_key(k: &[u8]) -> Secret<[u8; 32]> {
    hkdf(k, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")
}

fn controller_sign_key(k: &[u8]) -> Secret<[u8; 32]> {
    hkdf(k, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info")
}

fn accessory_sign_key(k: &[u8]) -> Secret<[u8; 32]> {
    hkdf(k, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info")
}

fn verify_encrypt_key(shared: &[u8]) -> Secret<[u8; 32]> {
    hkdf(shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info")
}

/// The client's pair-setup (M1, M3, M5; checks M2, M4, M6).
pub struct PairSetup {
    pairing_id: String,
    seed: Secret<[u8; 32]>,
    variant: PairVariant,
    srp: SrpClient,
    /// After M3.
    proof: Option<ClientProof>,
    /// M4's proof verified.
    verified: bool,
}

impl PairSetup {
    pub fn new(identity: &Identity, variant: PairVariant) -> Self {
        let mut a = [0; 32];
        rand::fill(&mut a).expect("the system RNG");
        Self::with_srp_private(identity, variant, &a)
    }

    /// With a fixed SRP `a` (the golden vectors use pyatv's: the controller seed).
    #[doc(hidden)]
    pub fn with_srp_private(identity: &Identity, variant: PairVariant, a: &[u8; 32]) -> Self {
        Self { pairing_id: identity.pairing_id.clone(), seed: identity.seed.clone(), variant, srp: SrpClient::new(a), proof: None, verified: false }
    }

    /// M1 `{Method 0, State 1}`.
    pub fn m1(&mut self) -> Vec<u8> {
        tlv8::encode(&[(tag::METHOD, &[0]), (tag::STATE, &[1])])
    }

    /// From M2 `{Salt, PublicKey}` and the PIN: M3 `{State 3, PublicKey, Proof}`.
    pub fn m3(&mut self, m2: &[u8], pin: &Pin) -> Result<Vec<u8>, PairError> {
        let tlv = decode(m2)?;
        setup_error(&tlv, false)?;
        let (Some(salt), Some(b_pub)) = (tlv.get(tag::SALT), tlv.get(tag::PUBLIC_KEY)) else { return Err(PairError::Failed) };
        let proof = self.srp.process(salt, b_pub, pin.expose()).map_err(|_| PairError::Failed)?;
        let m3 = tlv8::encode(&[(tag::STATE, &[3]), (tag::PUBLIC_KEY, &proof.public_a), (tag::PROOF, &proof.proof)]);
        self.proof = Some(proof);
        Ok(m3)
    }

    /// Verify M4's proof; M5 `{State 5, EncryptedData{Identifier, PublicKey, Signature, Name[,
    /// ACL]}}` (pyatv's order: the name, then `additional_data`).
    pub fn m5(&mut self, m4: &[u8]) -> Result<Vec<u8>, PairError> {
        let proof = self.proof.as_ref().ok_or(PairError::Failed)?;
        let tlv = decode(m4)?;
        setup_error(&tlv, true)?;
        let server_proof = tlv.get(tag::PROOF).ok_or(PairError::Failed)?;
        constant_time::verify_slices_are_equal(server_proof, &proof.expected_server_proof).map_err(|_| PairError::Failed)?;
        self.verified = true;

        let k = proof.session_key.expose();
        let id = self.pairing_id.as_bytes();
        let ltpk = ltpk(self.seed.expose());
        let sig = sign(self.seed.expose(), &[controller_sign_key(k).expose(), id, &ltpk])?;
        let mut inner: Vec<(u8, &[u8])> = vec![(tag::IDENTIFIER, id), (tag::PUBLIC_KEY, &ltpk), (tag::SIGNATURE, &sig), (tag::NAME, NAME_TLV)];
        if self.variant == PairVariant::Hkp5 {
            inner.push((tag::ACL, SCREEN_CAPTURE_ACL));
        }
        let sealed = seal_labelled(&setup_encrypt_key(k), b"PS-Msg05", &tlv8::encode(&inner));
        Ok(tlv8::encode(&[(tag::STATE, &[5]), (tag::ENCRYPTED_DATA, &sealed)]))
    }

    /// Verify M6's signature: the TV's long-term credentials.
    pub fn finish(self, m6: &[u8]) -> Result<PeerCredentials, PairError> {
        let proof = self.proof.as_ref().filter(|_| self.verified).ok_or(PairError::Failed)?;
        let tlv = decode(m6)?;
        setup_error(&tlv, false)?;
        let k = proof.session_key.expose();
        let sealed = tlv.get(tag::ENCRYPTED_DATA).ok_or(PairError::Failed)?;
        let inner = decode(&open_labelled(&setup_encrypt_key(k), b"PS-Msg06", sealed).ok_or(PairError::Failed)?)?;
        let id = inner.get(tag::IDENTIFIER).filter(|id| !id.is_empty()).ok_or(PairError::Failed)?;
        let peer_ltpk = key32(&inner, tag::PUBLIC_KEY)?;
        let sig = inner.get(tag::SIGNATURE).ok_or(PairError::Failed)?;
        if !signed_by(&peer_ltpk, &[accessory_sign_key(k).expose(), id, &peer_ltpk], sig) {
            return Err(PairError::Failed);
        }
        let peer_id = String::from_utf8(id.to_vec()).map_err(|_| PairError::Failed)?;
        Ok(PeerCredentials { peer_id, peer_ltpk, variant: self.variant })
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
        let s = self.shared.expose();
        (hkdf(s, "Control-Salt", "Control-Write-Encryption-Key"), hkdf(s, "Control-Salt", "Control-Read-Encryption-Key"))
    }

    /// Event channel `(write, read)` for us: `"Events-Salt"`, SWAPPED — we write with
    /// `"Events-Read-Encryption-Key"` and read with `"Events-Write-Encryption-Key"`.
    pub fn events(&self) -> (Secret<[u8; 32]>, Secret<[u8; 32]>) {
        let s = self.shared.expose();
        (hkdf(s, "Events-Salt", "Events-Read-Encryption-Key"), hkdf(s, "Events-Salt", "Events-Write-Encryption-Key"))
    }
}

/// The client's pair-verify (every session).
pub struct PairVerify {
    pairing_id: String,
    seed: Secret<[u8; 32]>,
    peer: PeerCredentials,
    /// The ephemeral X25519 key (`None` only if aws-lc refused it: M3 then fails).
    ephemeral: Option<(PrivateKey, [u8; 32])>,
    /// After M2 verified.
    shared: Option<Secret<[u8; 32]>>,
}

impl PairVerify {
    /// `ephemeral`: 32 random bytes (fixed in tests).
    pub fn new(identity: &Identity, peer: &PeerCredentials, ephemeral: &[u8; 32]) -> Self {
        Self { pairing_id: identity.pairing_id.clone(), seed: identity.seed.clone(), peer: peer.clone(), ephemeral: x25519(ephemeral), shared: None }
    }

    /// M1 `{State 1, PublicKey}`.
    pub fn m1(&mut self) -> Vec<u8> {
        let public = self.ephemeral.as_ref().map(|(_, p)| *p).unwrap_or_default();
        tlv8::encode(&[(tag::STATE, &[1]), (tag::PUBLIC_KEY, &public)])
    }

    /// Check M2 (id and signature); M3 `{State 3, EncryptedData}`.
    pub fn m3(&mut self, m2: &[u8]) -> Result<Vec<u8>, PairError> {
        if m2.is_empty() {
            // tvOS 27.2 with Allow Access "Everyone in Your Home" answers 200 with no body (#2885)
            return Err(PairError::Refused { home_only: true });
        }
        let tlv = decode(m2)?;
        if tlv.get(tag::ERROR).is_some() {
            return Err(PairError::Failed);
        }
        let (private, our_pub) = self.ephemeral.as_ref().ok_or(PairError::Failed)?;
        let tv_pub = key32(&tlv, tag::PUBLIC_KEY)?;
        let sealed = tlv.get(tag::ENCRYPTED_DATA).ok_or(PairError::Failed)?;
        let shared = agree(private, &tv_pub).ok_or(PairError::Failed)?;
        let key = verify_encrypt_key(shared.expose());
        let inner = decode(&open_labelled(&key, b"PV-Msg02", sealed).ok_or(PairError::Failed)?)?;
        let id = inner.get(tag::IDENTIFIER).ok_or(PairError::Failed)?;
        let sig = inner.get(tag::SIGNATURE).ok_or(PairError::Failed)?;
        if id != self.peer.peer_id.as_bytes() || !signed_by(&self.peer.peer_ltpk, &[&tv_pub, id, our_pub], sig) {
            return Err(PairError::Failed);
        }
        let client_id = self.pairing_id.as_bytes();
        let sig = sign(self.seed.expose(), &[our_pub, client_id, &tv_pub])?;
        let sealed = seal_labelled(&key, b"PV-Msg03", &tlv8::encode(&[(tag::IDENTIFIER, client_id), (tag::SIGNATURE, &sig)]));
        self.shared = Some(shared);
        Ok(tlv8::encode(&[(tag::STATE, &[3]), (tag::ENCRYPTED_DATA, &sealed)]))
    }

    /// Check M4's status: the session keys.
    pub fn finish(self, m4: &[u8]) -> Result<SessionKeys, PairError> {
        let shared = self.shared.ok_or(PairError::Failed)?;
        let tlv = decode(m4)?;
        let state_ok = tlv.get(tag::STATE).is_none_or(|s| s == [4]);
        if tlv.get(tag::ERROR).is_some() || !state_ok {
            return Err(PairError::Failed);
        }
        Ok(SessionKeys { shared })
    }
}

/// The receiver's halves (the fake receiver, the interop and golden-vector tests). Frozen
/// signatures (CAST-0) so the fake receiver (CAST-3) builds in parallel with CAST-1, which fills
/// the bodies (strict: the SRP proof and the controller's signature are checked). Bodies reply
/// with TLV8 (an `Error` TLV on failure, as the TV does).
///
/// Ported from pyatv `protocols/airplay/server_auth.py` (`_m1_setup` … `_m5_setup`,
/// `_m1_verify`, `_m3_verify`), with the same reply layouts (`{State 2, Salt, PublicKey}`,
/// `{Proof, State 4}` / `{Error 2, State 4}`, `{State 6, EncryptedData}`, `{State 2, PublicKey,
/// EncryptedData}`, `{State 4}`), plus what pyatv's server skips: M5's controller signature and
/// M3's identity and signature are verified (`{Error 2, State n}` when they don't).
#[doc(hidden)]
pub mod server {

    use super::*;
    use crate::cast::airplay::srp::SrpServer;

    /// The TV's long-term identity in a test: its pairing id and Ed25519 seed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Accessory {
        pub id: String,
        pub seed: [u8; 32],
    }

    /// A reply carrying only an error and the state it answers with.
    fn error_reply(code: u8, state: u8) -> Vec<u8> {
        tlv8::encode(&[(tag::ERROR, &[code]), (tag::STATE, &[state])])
    }

    /// The TV's pair-setup (M1 → M2, M3 → M4, M5 → M6). Fixed `salt` and `b_private` make the
    /// golden-vector tests deterministic.
    pub struct PairSetupServer {
        accessory: Accessory,
        salt: [u8; 16],
        srp: SrpServer,
        /// `K` once M3's proof verified.
        key: Option<Secret<Vec<u8>>>,
        paired: Option<PeerCredentials>,
        /// Extra TLVs inside M6's sealed part (a tvOS 26.6 TV adds a 159-byte type 17, §7.6).
        m6_extra: Vec<(u8, Vec<u8>)>,
    }

    impl PairSetupServer {
        /// `pin`: the code the TV "shows".
        pub fn new(accessory: &Accessory, pin: &str, salt: &[u8; 16], b_private: &[u8; 32]) -> Self {
            Self {
                accessory: accessory.clone(),
                salt: *salt,
                srp: SrpServer::new(salt, pin, b_private),
                key: None,
                paired: None,
                m6_extra: Vec::new(),
            }
        }

        /// Add a TLV to M6's sealed part after the signature (the fake's `m6_tlv17` mode).
        pub fn m6_extra(&mut self, tag: u8, value: Vec<u8>) {
            self.m6_extra.push((tag, value));
        }

        /// One `/pair-setup` request body → the reply body (M2, M4 or M6; an `Error` TLV — 2
        /// for a wrong PIN at M4 — when it fails).
        pub fn handle(&mut self, request: &[u8]) -> Vec<u8> {
            let Ok(tlv) = tlv8::decode(request) else { return error_reply(code::UNKNOWN, 2) };
            match tlv.get(tag::STATE) {
                Some([1]) => {
                    self.key = None;
                    self.paired = None;
                    tlv8::encode(&[(tag::STATE, &[2]), (tag::SALT, &self.salt), (tag::PUBLIC_KEY, &self.srp.public_b())])
                }
                Some([3]) => {
                    let verified = match (tlv.get(tag::PUBLIC_KEY), tlv.get(tag::PROOF)) {
                        (Some(a_pub), Some(proof)) => self.srp.verify(a_pub, proof).ok(),
                        _ => None,
                    };
                    match verified {
                        Some((m2, key)) => {
                            self.key = Some(key);
                            tlv8::encode(&[(tag::PROOF, &m2), (tag::STATE, &[4])])
                        }
                        None => error_reply(code::AUTHENTICATION, 4),
                    }
                }
                Some([5]) => self.m5(&tlv).unwrap_or_else(|| error_reply(code::AUTHENTICATION, 6)),
                Some([n]) => error_reply(code::UNKNOWN, n.saturating_add(1)),
                _ => error_reply(code::UNKNOWN, 2),
            }
        }

        fn m5(&mut self, tlv: &Tlv) -> Option<Vec<u8>> {
            let k = self.key.as_ref()?.expose().clone();
            let enc = setup_encrypt_key(&k);
            let inner = tlv8::decode(&open_labelled(&enc, b"PS-Msg05", tlv.get(tag::ENCRYPTED_DATA)?)?).ok()?;
            let id = inner.get(tag::IDENTIFIER).filter(|id| !id.is_empty())?;
            let controller_ltpk: [u8; 32] = inner.get(tag::PUBLIC_KEY)?.try_into().ok()?;
            let sig = inner.get(tag::SIGNATURE)?;
            if !signed_by(&controller_ltpk, &[controller_sign_key(&k).expose(), id, &controller_ltpk], sig) {
                return None;
            }
            let variant = if inner.get(tag::ACL).is_some() { PairVariant::Hkp5 } else { PairVariant::Hkp3 };
            self.paired = Some(PeerCredentials { peer_id: String::from_utf8(id.to_vec()).ok()?, peer_ltpk: controller_ltpk, variant });

            let acc_id = self.accessory.id.as_bytes();
            let acc_ltpk = ltpk(&self.accessory.seed);
            let acc_sig = sign(&self.accessory.seed, &[accessory_sign_key(&k).expose(), acc_id, &acc_ltpk]).ok()?;
            let mut items: Vec<(u8, &[u8])> = vec![(tag::IDENTIFIER, acc_id), (tag::PUBLIC_KEY, &acc_ltpk), (tag::SIGNATURE, &acc_sig)];
            items.extend(self.m6_extra.iter().map(|(t, v)| (*t, v.as_slice())));
            let sealed = seal_labelled(&enc, b"PS-Msg06", &tlv8::encode(&items));
            Some(tlv8::encode(&[(tag::STATE, &[6]), (tag::ENCRYPTED_DATA, &sealed)]))
        }

        /// The controller that paired (its id and long-term key, `variant` as the request
        /// said), once M5 verified. The variant is read from the request body: the ACL TLV is
        /// sent only by HKP 5.
        pub fn paired(&self) -> Option<PeerCredentials> {
            self.paired.clone()
        }
    }

    /// The TV's pair-verify (M1 → M2, M3 → M4) against one known controller.
    pub struct PairVerifyServer {
        accessory: Accessory,
        controller: PeerCredentials,
        ephemeral: Option<(PrivateKey, [u8; 32])>,
        /// The controller's ephemeral public key and the shared secret, after M1.
        exchange: Option<([u8; 32], Secret<[u8; 32]>)>,
        verified: bool,
    }

    impl PairVerifyServer {
        /// `controller`: who paired (`peer_id` / `peer_ltpk` here are the controller's);
        /// `ephemeral`: the TV's X25519 secret (fixed in tests).
        pub fn new(accessory: &Accessory, controller: &PeerCredentials, ephemeral: &[u8; 32]) -> Self {
            Self { accessory: accessory.clone(), controller: controller.clone(), ephemeral: x25519(ephemeral), exchange: None, verified: false }
        }

        /// One `/pair-verify` request body → the reply body (M2 or M4).
        pub fn handle(&mut self, request: &[u8]) -> Vec<u8> {
            let Ok(tlv) = tlv8::decode(request) else { return error_reply(code::UNKNOWN, 2) };
            match tlv.get(tag::STATE) {
                Some([1]) => self.m1(&tlv).unwrap_or_else(|| error_reply(code::AUTHENTICATION, 2)),
                Some([3]) => {
                    self.verified = self.m3(&tlv).is_some();
                    if self.verified { tlv8::encode(&[(tag::STATE, &[4])]) } else { error_reply(code::AUTHENTICATION, 4) }
                }
                _ => error_reply(code::UNKNOWN, 2),
            }
        }

        fn m1(&mut self, tlv: &Tlv) -> Option<Vec<u8>> {
            self.verified = false;
            let (private, tv_pub) = self.ephemeral.as_ref()?;
            let client_pub: [u8; 32] = tlv.get(tag::PUBLIC_KEY)?.try_into().ok()?;
            let shared = agree(private, &client_pub)?;
            let id = self.accessory.id.as_bytes();
            let sig = sign(&self.accessory.seed, &[tv_pub, id, &client_pub]).ok()?;
            let sealed =
                seal_labelled(&verify_encrypt_key(shared.expose()), b"PV-Msg02", &tlv8::encode(&[(tag::IDENTIFIER, id), (tag::SIGNATURE, &sig)]));
            let reply = tlv8::encode(&[(tag::STATE, &[2]), (tag::PUBLIC_KEY, tv_pub), (tag::ENCRYPTED_DATA, &sealed)]);
            self.exchange = Some((client_pub, shared));
            Some(reply)
        }

        fn m3(&self, tlv: &Tlv) -> Option<()> {
            let (_, tv_pub) = self.ephemeral.as_ref()?;
            let (client_pub, shared) = self.exchange.as_ref()?;
            let inner = tlv8::decode(&open_labelled(&verify_encrypt_key(shared.expose()), b"PV-Msg03", tlv.get(tag::ENCRYPTED_DATA)?)?).ok()?;
            let id = inner.get(tag::IDENTIFIER)?;
            let sig = inner.get(tag::SIGNATURE)?;
            (id == self.controller.peer_id.as_bytes() && signed_by(&self.controller.peer_ltpk, &[client_pub, id, tv_pub], sig)).then_some(())
        }

        /// The session's keys once M3 verified. The receiver uses them swapped: it reads with
        /// the controller's write key and writes with its read key.
        pub fn keys(&self) -> Option<SessionKeys> {
            let (_, shared) = self.exchange.as_ref().filter(|_| self.verified)?;
            Some(SessionKeys { shared: shared.clone() })
        }
    }
}

/// The pyatv 0.18.0 golden vectors (`tests/fixtures/cast/vectors/pyatv_0_18_0.json`) for the
/// CAST-1 unit tests.
#[cfg(test)]
pub(crate) mod golden {
    use std::sync::LazyLock;

    static VECTORS: LazyLock<serde_json::Value> =
        LazyLock::new(|| serde_json::from_str(include_str!("../../../tests/fixtures/cast/vectors/pyatv_0_18_0.json")).expect("valid vectors"));

    pub(crate) fn get() -> &'static serde_json::Value {
        &VECTORS
    }

    /// A hex string of the vectors as bytes.
    pub(crate) fn unhex(v: &serde_json::Value) -> Vec<u8> {
        let s = v.as_str().expect("a hex string");
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
    }

    /// The script's `pattern(n, seed)`: `(i·7 + seed) mod 251`.
    pub(crate) fn pattern(n: usize, seed: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + seed) % 251) as u8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::golden::{self, unhex};
    use super::server::{Accessory, PairSetupServer, PairVerifyServer};
    use super::*;
    use crate::cast::airplay::hap::FrameCipher;

    fn seed(v: &serde_json::Value) -> [u8; 32] {
        unhex(v).try_into().unwrap()
    }

    fn controller() -> Identity {
        let i = &golden::get()["inputs"];
        Identity {
            pairing_id: i["controller_id"].as_str().unwrap().to_owned(),
            seed: Secret::new(seed(&i["controller_seed"])),
            sender_mac: [2, 0, 0, 0, 0, 1],
        }
    }

    fn accessory() -> Accessory {
        let i = &golden::get()["inputs"];
        Accessory { id: i["accessory_id"].as_str().unwrap().to_owned(), seed: seed(&i["accessory_seed"]) }
    }

    fn pin() -> Pin {
        Pin::new(golden::get()["inputs"]["pin"].as_str().unwrap()).unwrap()
    }

    fn setup_server() -> PairSetupServer {
        let i = &golden::get()["inputs"];
        PairSetupServer::new(&accessory(), pin().expose(), &unhex(&i["salt"]).try_into().unwrap(), &seed(&i["srp_b"]))
    }

    fn setup_client(variant: PairVariant) -> PairSetup {
        PairSetup::with_srp_private(&controller(), variant, &seed(&golden::get()["inputs"]["srp_a"]))
    }

    fn tv_credentials() -> PeerCredentials {
        let a = accessory();
        PeerCredentials { peer_id: a.id, peer_ltpk: ltpk(&a.seed), variant: PairVariant::Hkp3 }
    }

    /// Tamper with a sealed TLV message: flip one ciphertext byte of its `EncryptedData`.
    fn tamper(msg: &[u8]) -> Vec<u8> {
        let mut tlv = tlv8::decode(msg).unwrap();
        for (t, v) in &mut tlv.0 {
            if *t == tag::ENCRYPTED_DATA {
                v[0] ^= 1;
            }
        }
        let items: Vec<(u8, &[u8])> = tlv.0.iter().map(|(t, v)| (*t, v.as_slice())).collect();
        tlv8::encode(&items)
    }

    /// pyatv's client against pyatv's accessory, byte for byte: our client yields its requests
    /// from its replies, and our strict server yields its replies from its requests.
    #[test]
    fn pair_setup_matches_pyatv_both_sides() {
        let ps = &golden::get()["pair_setup"];
        let m = |k: &str| unhex(&ps[k]);
        let mut client = setup_client(PairVariant::Hkp3);
        let mut server = setup_server();
        assert_eq!(client.m1(), m("m1"));
        assert_eq!(server.handle(&m("m1")), m("m2"));
        assert_eq!(client.m3(&m("m2"), &pin()).unwrap(), m("m3"));
        assert_eq!(server.handle(&m("m3")), m("m4"));
        assert_eq!(client.m5(&m("m4")).unwrap(), m("m5"));
        assert_eq!(server.handle(&m("m5")), m("m6"));
        let tv = client.finish(&m("m6")).unwrap();
        assert_eq!(tv, tv_credentials());
        assert_eq!(tv.peer_ltpk.as_slice(), m("accessory_ltpk"));
        let ours = server.paired().unwrap();
        assert_eq!(
            (ours.peer_id.as_str(), ours.peer_ltpk.as_slice(), ours.variant),
            (controller().pairing_id.as_str(), &m("controller_ltpk")[..], PairVariant::Hkp3)
        );

        // HKP 5: the same exchange, M5 adds the ScreenCapture ACL
        let mut client = setup_client(PairVariant::Hkp5);
        client.m1();
        client.m3(&m("m2"), &pin()).unwrap();
        assert_eq!(client.m5(&m("m4")).unwrap(), m("m5_hkp5"));
        assert_eq!(unhex(&golden::get()["inputs"]["acl"]), SCREEN_CAPTURE_ACL);
        assert_eq!(unhex(&golden::get()["inputs"]["name"]), NAME_TLV);
        let mut server = setup_server();
        server.handle(&m("m1"));
        server.handle(&m("m3"));
        assert_eq!(server.handle(&m("m5_hkp5")), m("m6"));
        assert_eq!(server.paired().unwrap().variant, PairVariant::Hkp5);
    }

    #[test]
    fn pair_verify_matches_pyatv_both_sides() {
        let g = golden::get();
        let pv = &g["pair_verify"];
        let m = |k: &str| unhex(&pv[k]);
        let mut client = PairVerify::new(&controller(), &tv_credentials(), &seed(&g["inputs"]["client_ephemeral"]));
        let us = PeerCredentials { peer_id: controller().pairing_id, peer_ltpk: ltpk(controller().seed.expose()), variant: PairVariant::Hkp3 };
        let mut server = PairVerifyServer::new(&accessory(), &us, &seed(&g["inputs"]["accessory_ephemeral"]));
        assert_eq!(client.m1(), m("m1"));
        assert_eq!(server.handle(&m("m1")), m("m2"));
        assert_eq!(client.m3(&m("m2")).unwrap(), m("m3"));
        assert_eq!(server.handle(&m("m3")), m("m4"));
        let keys = client.finish(&m("m4")).unwrap();
        assert_eq!(keys.shared.expose().as_slice(), m("shared"));
        let (w, r) = keys.control();
        assert_eq!((w.expose().to_vec(), r.expose().to_vec()), (m("control_write"), m("control_read")));
        let (w, r) = keys.events();
        assert_eq!((w.expose().to_vec(), r.expose().to_vec()), (m("events_write"), m("events_read")));

        // the TV reads what we write on the control channel (its keys swapped)
        let (tv_w, tv_r) = server.keys().unwrap().control();
        let (our_w, our_r) = keys.control();
        let mut frames = FrameCipher::new(&our_w, &our_r).seal(b"GET /info HTTP/1.1\r\n\r\n");
        assert_eq!(FrameCipher::new(&tv_r, &tv_w).open(&mut frames).unwrap().as_deref(), Some(&b"GET /info HTTP/1.1\r\n\r\n"[..]));
    }

    #[test]
    fn setup_error_tlvs_come_first() {
        let mut client = setup_client(PairVariant::Hkp3);
        client.m1();
        let backoff = tlv8::encode(&[(tag::STATE, &[2]), (tag::ERROR, &[3]), (tag::BACK_OFF, &[0x2C, 0x01])]);
        assert_eq!(client.m3(&backoff, &pin()), Err(PairError::Backoff { secs: Some(300) }));
        let other = tlv8::encode(&[(tag::STATE, &[2]), (tag::ERROR, &[6])]);
        assert_eq!(client.m3(&other, &pin()), Err(PairError::Refused { home_only: false }));

        // a wrong PIN: the strict server answers {Error 2, State 4}, which is WrongPin only at M4
        let mut server = setup_server();
        let mut client = setup_client(PairVariant::Hkp3);
        let m2 = server.handle(&client.m1());
        let m3 = client.m3(&m2, &Pin::new("1112").unwrap()).unwrap();
        let m4 = server.handle(&m3);
        assert_eq!(tlv8::decode(&m4).unwrap().get(tag::ERROR), Some(&[2][..]));
        assert_eq!(client.m5(&m4), Err(PairError::WrongPin));
    }

    #[test]
    fn forged_proofs_and_signatures_are_rejected() {
        let ps = &golden::get()["pair_setup"];
        let m = |k: &str| unhex(&ps[k]);

        // M4 with another proof (a TV that doesn't know the PIN's verifier)
        let mut client = setup_client(PairVariant::Hkp3);
        client.m3(&m("m2"), &pin()).unwrap();
        let mut forged = m("m4");
        forged[10] ^= 1;
        assert_eq!(client.m5(&forged), Err(PairError::Failed));

        // M6 sealed under the right key but signed by another key
        let mut client = setup_client(PairVariant::Hkp3);
        client.m3(&m("m2"), &pin()).unwrap();
        client.m5(&m("m4")).unwrap();
        let k = unhex(&golden::get()["srp"]["K"]);
        let liar = [0x55; 32];
        let id = accessory().id;
        let sig = sign(&liar, &[accessory_sign_key(&k).expose(), id.as_bytes(), &ltpk(&accessory().seed)]).unwrap();
        let inner = tlv8::encode(&[(tag::IDENTIFIER, id.as_bytes()), (tag::PUBLIC_KEY, &ltpk(&accessory().seed)), (tag::SIGNATURE, &sig)]);
        let m6 = tlv8::encode(&[(tag::STATE, &[6]), (tag::ENCRYPTED_DATA, &seal_labelled(&setup_encrypt_key(&k), b"PS-Msg06", &inner))]);
        assert_eq!(client.finish(&m6), Err(PairError::Failed));

        // the strict server refuses an M5 whose ciphertext was altered
        let mut server = setup_server();
        server.handle(&m("m1"));
        server.handle(&m("m3"));
        assert_eq!(tlv8::decode(&server.handle(&tamper(&m("m5")))).unwrap().get(tag::ERROR), Some(&[2][..]));
        assert!(server.paired().is_none());
    }

    /// A tvOS 26.6 TV adds a 159-byte type-17 TLV to M6: ignored, the signature still checked.
    #[test]
    fn unknown_m6_tlvs_are_ignored() {
        let mut server = setup_server();
        server.m6_extra(0x11, vec![0xAB; 159]);
        let mut client = setup_client(PairVariant::Hkp3);
        let m2 = server.handle(&client.m1());
        let m4 = server.handle(&client.m3(&m2, &pin()).unwrap());
        let m6 = server.handle(&client.m5(&m4).unwrap());
        assert_eq!(client.finish(&m6).unwrap(), tv_credentials());
    }

    #[test]
    fn verify_failures_map_to_lost_or_refused() {
        let g = golden::get();
        let m = |k: &str| unhex(&g["pair_verify"][k]);
        let ephemeral = seed(&g["inputs"]["client_ephemeral"]);
        let fresh = |peer: &PeerCredentials| PairVerify::new(&controller(), peer, &ephemeral);

        assert_eq!(fresh(&tv_credentials()).m3(&[]), Err(PairError::Refused { home_only: true }));
        // the TV was reset: a new long-term key, or another id
        let new_key = PeerCredentials { peer_ltpk: ltpk(&[0x66; 32]), ..tv_credentials() };
        assert_eq!(fresh(&new_key).m3(&m("m2")), Err(PairError::Failed));
        let other_id = PeerCredentials { peer_id: "another-tv".into(), ..tv_credentials() };
        assert_eq!(fresh(&other_id).m3(&m("m2")), Err(PairError::Failed));
        assert_eq!(fresh(&tv_credentials()).m3(&tamper(&m("m2"))), Err(PairError::Failed));
        let err = tlv8::encode(&[(tag::STATE, &[2]), (tag::ERROR, &[2])]);
        assert_eq!(fresh(&tv_credentials()).m3(&err), Err(PairError::Failed));

        // M4: an Error TLV fails; an empty body is accepted (foxCast)
        let mut client = fresh(&tv_credentials());
        client.m3(&m("m2")).unwrap();
        assert!(client.finish(&error_m4()).is_err());
        let mut client = fresh(&tv_credentials());
        client.m3(&m("m2")).unwrap();
        assert!(client.finish(&[]).is_ok());

        // the strict server refuses a controller it doesn't know
        let stranger = PeerCredentials { peer_id: controller().pairing_id, peer_ltpk: ltpk(&[0x77; 32]), variant: PairVariant::Hkp3 };
        let mut server = PairVerifyServer::new(&accessory(), &stranger, &seed(&g["inputs"]["accessory_ephemeral"]));
        server.handle(&m("m1"));
        assert_eq!(tlv8::decode(&server.handle(&m("m3"))).unwrap().get(tag::ERROR), Some(&[2][..]));
        assert!(server.keys().is_none());
    }

    fn error_m4() -> Vec<u8> {
        tlv8::encode(&[(tag::STATE, &[4]), (tag::ERROR, &[2])])
    }

    #[test]
    fn variant_headers() {
        let id = controller();
        let names = |h: Vec<(&'static str, String)>| h.into_iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>();
        assert_eq!(
            names(pin_start_headers(PairVariant::Hkp3, &id)),
            ["X-Apple-HKP: 3", "X-Apple-Client-Name: Tuner", "Content-Type: application/octet-stream", "User-Agent: AirPlay/320.20"]
        );
        let hkp5 = names(verify_headers(PairVariant::Hkp5, &id));
        assert!(hkp5.contains(&"X-Apple-HKP: 5".to_owned()) && hkp5.contains(&"X-Apple-PD: 1".to_owned()));
        assert!(hkp5.contains(&format!("X-Apple-Client-ID: {}", id.pairing_id)));
        assert!(names(pin_start_headers(PairVariant::Hkp5, &id)).contains(&"X-Apple-SupportedPINLengths: 4".to_owned()));
    }
}
