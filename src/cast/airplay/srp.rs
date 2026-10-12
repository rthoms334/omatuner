// Ported from pyatv pyatv/auth/hap_srp.py @ b277a4c8 (MIT, © 2020 Pierre Ståhl).
//! SRP-6a for pair-setup (DESIGN §40 §7.1). OWNER: CAST-1. Hand-rolled on `num-bigint`, ported
//! from pyatv `hap_srp.py`'s behaviour (srptools) and cross-checked with foxCast's maths: RFC 5054
//! 3072-bit group, g = 5, SHA-512, identity `"Pair-Setup"`, password = the PIN string;
//! `x = H(salt ‖ H("Pair-Setup:" ‖ pin))`; `k` and `u` padded to 384 bytes; the M1 proof over
//! UNPADDED values `H(H(N) ⊕ H(g) ‖ H(I) ‖ salt ‖ A ‖ B ‖ K)`, `K = H(S)`; the server proof
//! `H(A ‖ M1 ‖ K)` is verified. The server half is here too (`#[doc(hidden)]`) for the fake
//! receiver (CAST-3). Checked against RFC 5054 Appendix B and the pyatv 0.18.0 vectors.
//!
//! srptools details kept exactly (they decide the bytes the TV checks):
//! * Integers are hashed as minimal big-endian bytes (`int_to_bytes`): `N`, `g` (one byte),
//!   `A`, `B`, `S` and the `H(N) ⊕ H(g)` integer. Only `k` and `u` pad (`PAD()` = 384 bytes).
//!   A padded `H(g)` in M1 gets a 470 at M3 that looks like a policy refusal (AirPlayDroid).
//! * The salt is hashed as the bytes the TV sent; `S = (B − k·v)^(a + u·x) mod N` and
//!   `S = (A·v^u)^b mod N`.
//! * `B mod N == 0` (client) and `A mod N == 0` (server) are refused (srptools), and so is
//!   `u == 0` (RFC 5054 §2.6), which srptools leaves unchecked.
//!
//! pyatv uses the controller's long-term Ed25519 seed as `a` (a fresh key per pairing there);
//! Tuner's seed is per install, so `pair::PairSetup` draws a fresh random `a` instead and the
//! golden-vector tests pass pyatv's.

use std::sync::LazyLock;

use aws_lc_rs::constant_time;
use aws_lc_rs::digest::{self, SHA512};
use num_bigint::BigUint;

use crate::redact::Secret;

/// An SRP failure (bad `B`, a proof that doesn't verify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrpError {
    /// `B mod N == 0` or `A mod N == 0`.
    BadPublicKey,
    /// The peer's proof doesn't match.
    BadProof,
}

/// The username SRP hashes (HAP's fixed identity).
const IDENTITY: &[u8] = b"Pair-Setup";

/// RFC 5054 Appendix A, the 3072-bit group (srptools `PRIME_3072`), g = 5.
const N_3072: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DD\
EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F\
83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA0510\
15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200C\
BBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

/// A group and hash: HAP's (3072-bit, SHA-512) in use; the tests also run RFC 5054's
/// Appendix B vectors (1024-bit, SHA-1) through the same maths.
struct Group {
    n: BigUint,
    g: BigUint,
    /// `PAD()` width: the byte length of `N`.
    len: usize,
    hash: &'static digest::Algorithm,
}

static HAP: LazyLock<Group> = LazyLock::new(|| Group::new(N_3072, 5, &SHA512));

impl Group {
    fn new(n_hex: &str, g: u32, hash: &'static digest::Algorithm) -> Self {
        let n = BigUint::parse_bytes(n_hex.as_bytes(), 16).expect("an RFC 5054 group prime");
        let len = n.to_bytes_be().len();
        Self { n, g: BigUint::from(g), len, hash }
    }

    /// `H(a ‖ b ‖ …)`.
    fn h(&self, parts: &[&[u8]]) -> Vec<u8> {
        let mut ctx = digest::Context::new(self.hash);
        for p in parts {
            ctx.update(p);
        }
        ctx.finish().as_ref().to_vec()
    }

    /// `H(…)` as an integer.
    fn h_int(&self, parts: &[&[u8]]) -> BigUint {
        BigUint::from_bytes_be(&self.h(parts))
    }

    /// `PAD(v)`: left-padded with zeros to the length of `N`.
    fn pad(&self, v: &BigUint) -> Vec<u8> {
        let b = v.to_bytes_be();
        let mut out = vec![0; self.len.saturating_sub(b.len())];
        out.extend_from_slice(&b);
        out
    }

    /// `k = H(N ‖ PAD(g))`.
    fn k(&self) -> BigUint {
        self.h_int(&[&self.n.to_bytes_be(), &self.pad(&self.g)])
    }

    /// `x = H(salt ‖ H(I ‖ ":" ‖ P))`.
    fn x(&self, salt: &[u8], identity: &[u8], password: &[u8]) -> BigUint {
        let inner = self.h(&[identity, b":", password]);
        self.h_int(&[salt, &inner])
    }

    /// `u = H(PAD(A) ‖ PAD(B))`.
    fn u(&self, a_pub: &BigUint, b_pub: &BigUint) -> BigUint {
        self.h_int(&[&self.pad(a_pub), &self.pad(b_pub)])
    }

    /// `v = g^x mod N`.
    fn verifier(&self, x: &BigUint) -> BigUint {
        self.g.modpow(x, &self.n)
    }

    /// `A = g^a mod N`.
    fn client_public(&self, a: &BigUint) -> BigUint {
        self.g.modpow(a, &self.n)
    }

    /// `B = (k·v + g^b) mod N`.
    fn server_public(&self, v: &BigUint, b: &BigUint) -> BigUint {
        (self.k() * v + self.g.modpow(b, &self.n)) % &self.n
    }

    /// The client's `S = (B − k·v)^(a + u·x) mod N` (the base reduced into `[0, N)`).
    fn client_premaster(&self, b_pub: &BigUint, x: &BigUint, a: &BigUint, u: &BigUint) -> BigUint {
        let kv = self.k() * self.verifier(x) % &self.n;
        let base = (b_pub % &self.n + &self.n - kv) % &self.n;
        base.modpow(&(a + u * x), &self.n)
    }

    /// The server's `S = (A·v^u)^b mod N`.
    fn server_premaster(&self, a_pub: &BigUint, v: &BigUint, u: &BigUint, b: &BigUint) -> BigUint {
        (a_pub * v.modpow(u, &self.n) % &self.n).modpow(b, &self.n)
    }

    /// `M1 = H(H(N) ⊕ H(g) ‖ H(I) ‖ salt ‖ A ‖ B ‖ K)`, every integer unpadded (srptools).
    fn client_proof(&self, identity: &[u8], salt: &[u8], a_pub: &BigUint, b_pub: &BigUint, key: &[u8]) -> Vec<u8> {
        let hn = self.h_int(&[&self.n.to_bytes_be()]);
        let hg = self.h_int(&[&self.g.to_bytes_be()]);
        let xor = (hn ^ hg).to_bytes_be();
        self.h(&[&xor, &self.h(&[identity]), salt, &a_pub.to_bytes_be(), &b_pub.to_bytes_be(), key])
    }

    /// `M2 = H(A ‖ M1 ‖ K)`.
    fn server_proof(&self, a_pub: &BigUint, m1: &[u8], key: &[u8]) -> Vec<u8> {
        self.h(&[&a_pub.to_bytes_be(), m1, key])
    }

    fn is_zero_mod_n(&self, v: &BigUint) -> bool {
        (v % &self.n).bits() == 0
    }
}

/// The client's side of one exchange.
pub struct SrpClient {
    a: BigUint,
    a_pub: BigUint,
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
    pub fn new(a_private: &[u8]) -> Self {
        let a = BigUint::from_bytes_be(a_private);
        let a_pub = HAP.client_public(&a);
        Self { a, a_pub }
    }

    /// Process the server's salt and `B` with the PIN.
    pub fn process(&self, salt: &[u8], b_public: &[u8], pin: &str) -> Result<ClientProof, SrpError> {
        let grp = &*HAP;
        let b_pub = BigUint::from_bytes_be(b_public);
        if grp.is_zero_mod_n(&b_pub) {
            return Err(SrpError::BadPublicKey);
        }
        let u = grp.u(&self.a_pub, &b_pub);
        if u.bits() == 0 {
            return Err(SrpError::BadPublicKey);
        }
        let x = grp.x(salt, IDENTITY, pin.as_bytes());
        let key = grp.h(&[&grp.client_premaster(&b_pub, &x, &self.a, &u).to_bytes_be()]);
        let proof = grp.client_proof(IDENTITY, salt, &self.a_pub, &b_pub, &key);
        let expected_server_proof = grp.server_proof(&self.a_pub, &proof, &key);
        Ok(ClientProof { public_a: self.a_pub.to_bytes_be(), proof, session_key: Secret::new(key), expected_server_proof })
    }
}

/// The server's side (the fake receiver and the golden-vector tests only).
#[doc(hidden)]
pub struct SrpServer {
    salt: Vec<u8>,
    v: BigUint,
    b: BigUint,
    b_pub: BigUint,
}

#[doc(hidden)]
impl SrpServer {
    pub fn new(salt: &[u8], pin: &str, b_private: &[u8]) -> Self {
        let grp = &*HAP;
        let v = grp.verifier(&grp.x(salt, IDENTITY, pin.as_bytes()));
        let b = BigUint::from_bytes_be(b_private);
        let b_pub = grp.server_public(&v, &b);
        Self { salt: salt.to_vec(), v, b, b_pub }
    }

    /// `B`, unpadded big-endian.
    pub fn public_b(&self) -> Vec<u8> {
        self.b_pub.to_bytes_be()
    }

    /// Check the client's M1; on success the server proof M2 and `K`.
    pub fn verify(&self, public_a: &[u8], client_proof: &[u8]) -> Result<(Vec<u8>, Secret<Vec<u8>>), SrpError> {
        let grp = &*HAP;
        let a_pub = BigUint::from_bytes_be(public_a);
        if grp.is_zero_mod_n(&a_pub) {
            return Err(SrpError::BadPublicKey);
        }
        let u = grp.u(&a_pub, &self.b_pub);
        if u.bits() == 0 {
            return Err(SrpError::BadPublicKey);
        }
        let key = grp.h(&[&grp.server_premaster(&a_pub, &self.v, &u, &self.b).to_bytes_be()]);
        let expected = grp.client_proof(IDENTITY, &self.salt, &a_pub, &self.b_pub, &key);
        constant_time::verify_slices_are_equal(&expected, client_proof).map_err(|_| SrpError::BadProof)?;
        Ok((grp.server_proof(&a_pub, client_proof, &key), Secret::new(key)))
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY;

    use super::*;
    use crate::cast::airplay::pair::golden::{self, unhex};

    fn int(v: &serde_json::Value) -> BigUint {
        BigUint::from_bytes_be(&unhex(v))
    }

    fn hex_int(s: &str) -> BigUint {
        BigUint::parse_bytes(s.replace(' ', "").as_bytes(), 16).unwrap()
    }

    /// RFC 5054 Appendix B (1024-bit group, SHA-1): the same maths, the RFC's numbers.
    #[test]
    fn rfc5054_appendix_b() {
        let grp = Group::new(
            "EEAF0AB9ADB38DD69C33F80AFA8FC5E86072618775FF3C0B9EA2314C9C256576D674DF7496EA81D3383B4813D692C6E0\
             E0D5D8E250B98BE48E495C1D6089DAD15DC7D7B46154D6B6CE8EF4AD69B15D4982559B297BCF1885C529F566660E57EC\
             68EDBC3C05726CC02FD4CBF4976EAA9AFD5138FE8376435B9FC61D2FC0EB06E3",
            2,
            &SHA1_FOR_LEGACY_USE_ONLY,
        );
        let salt = hex_int("BEB25379 D1A8581E B5A72767 3A2441EE").to_bytes_be();
        let a = hex_int("60975527 035CF2AD 1989806F 0407210B C81EDC04 E2762A56 AFD529DD DA2D4393");
        let b = hex_int("E487CB59 D31AC550 471E81F0 0F6928E0 1DDA08E9 74A004F4 9E61F5D1 05284D20");
        assert_eq!(grp.k(), hex_int("7556AA04 5AEF2CDD 07ABAF0F 665C3E81 8913186F"));
        let x = grp.x(&salt, b"alice", b"password123");
        assert_eq!(x, hex_int("94B7555A ABE9127C C58CCF49 93DB6CF8 4D16C124"));
        let v = grp.verifier(&x);
        assert_eq!(
            v,
            hex_int(
                "7E273DE8 696FFC4F 4E337D05 B4B375BE B0DDE156 9E8FA00A 9886D812 9BADA1F1 822223CA 1A605B53 0E379BA4 \
                 729FDC59 F105B478 7E5186F5 C671085A 1447B52A 48CF1970 B4FB6F84 00BBF4CE BFBB1681 52E08AB5 EA53D15C \
                 1AFF87B2 B9DA6E04 E058AD51 CC72BFC9 033B564E 26480D78 E955A5E2 9E7AB245 DB2BE315 E2099AFB"
            )
        );
        let a_pub = grp.client_public(&a);
        assert_eq!(
            a_pub,
            hex_int(
                "61D5E490 F6F1B795 47B0704C 436F523D D0E560F0 C64115BB 72557EC4 4352E890 3211C046 92272D8B 2D1A5358 \
                 A2CF1B6E 0BFCF99F 921530EC 8E393561 79EAE45E 42BA92AE ACED8251 71E1E8B9 AF6D9C03 E1327F44 BE087EF0 \
                 6530E69F 66615261 EEF54073 CA11CF58 58F0EDFD FE15EFEA B349EF5D 76988A36 72FAC47B 0769447B"
            )
        );
        let b_pub = grp.server_public(&v, &b);
        assert_eq!(
            b_pub,
            hex_int(
                "BD0C6151 2C692C0C B6D041FA 01BB152D 4916A1E7 7AF46AE1 05393011 BAF38964 DC46A067 0DD125B9 5A981652 \
                 236F99D9 B681CBF8 7837EC99 6C6DA044 53728610 D0C6DDB5 8B318885 D7D82C7F 8DEB75CE 7BD4FBAA 37089E6F \
                 9C6059F3 88838E7A 00030B33 1EB76840 910440B1 B27AAEAE EB4012B7 D7665238 A8E3FB00 4B117B58"
            )
        );
        let u = grp.u(&a_pub, &b_pub);
        assert_eq!(u, hex_int("CE38B959 3487DA98 554ED47D 70A7AE5F 462EF019"));
        let premaster = hex_int(
            "B0DC82BA BCF30674 AE450C02 87745E79 90A3381F 63B387AA F271A10D 233861E3 59B48220 F7C4693C 9AE12B0A \
             6F67809F 0876E2D0 13800D6C 41BB59B6 D5979B5C 00A172B4 A2A5903A 0BDCAF8A 709585EB 2AFAFA8F 3499B200 \
             210DCC1F 10EB3394 3CD67FC8 8A2F39A4 BE5BEC4E C0A3212D C346D7E4 74B29EDE 8A469FFE CA686E5A",
        );
        assert_eq!(grp.client_premaster(&b_pub, &x, &a, &u), premaster);
        assert_eq!(grp.server_premaster(&a_pub, &v, &u, &b), premaster);
    }

    /// pyatv 0.18.0 (srptools) with PIN 1111: every intermediate value and both proofs, on both
    /// halves.
    #[test]
    fn pyatv_exchange() {
        let g = golden::get();
        let (inputs, srp) = (&g["inputs"], &g["srp"]);
        let salt = unhex(&inputs["salt"]);
        let pin = inputs["pin"].as_str().unwrap();
        let grp = &*HAP;
        assert_eq!(grp.k(), int(&srp["k"]));
        let x = grp.x(&salt, IDENTITY, pin.as_bytes());
        assert_eq!(x, int(&srp["x"]));
        assert_eq!(grp.verifier(&x), int(&srp["verifier"]));

        let server = SrpServer::new(&salt, pin, &unhex(&inputs["srp_b"]));
        assert_eq!(server.public_b(), unhex(&srp["B"]));
        let client = SrpClient::new(&unhex(&inputs["srp_a"]));
        let proof = client.process(&salt, &server.public_b(), pin).unwrap();
        assert_eq!(proof.public_a, unhex(&srp["A"]));
        assert_eq!(grp.u(&client.a_pub, &server.b_pub), int(&srp["u"]));
        assert_eq!(grp.client_premaster(&server.b_pub, &x, &client.a, &int(&srp["u"])), int(&srp["S"]));
        assert_eq!(proof.session_key.expose(), &unhex(&srp["K"]));
        assert_eq!(proof.proof, unhex(&srp["M1"]));
        assert_eq!(proof.expected_server_proof, unhex(&srp["M2"]));

        let (m2, key) = server.verify(&proof.public_a, &proof.proof).unwrap();
        assert_eq!(m2, unhex(&srp["M2"]));
        assert_eq!(key.expose(), proof.session_key.expose());
    }

    #[test]
    fn wrong_pins_and_degenerate_keys_are_refused() {
        let salt = [7u8; 16];
        let server = SrpServer::new(&salt, "1111", &[9; 32]);
        let wrong = SrpClient::new(&[3; 32]).process(&salt, &server.public_b(), "1112").unwrap();
        assert_eq!(server.verify(&wrong.public_a, &wrong.proof).err(), Some(SrpError::BadProof));

        let n = HAP.n.to_bytes_be();
        assert_eq!(SrpClient::new(&[3; 32]).process(&salt, &n, "1111").err(), Some(SrpError::BadPublicKey));
        assert_eq!(SrpClient::new(&[3; 32]).process(&salt, &[0], "1111").err(), Some(SrpError::BadPublicKey));
        assert_eq!(server.verify(&n, &wrong.proof).err(), Some(SrpError::BadPublicKey));
    }
}
