//! The tests' own release key (DESIGN §41.8): an Ed25519 key pair made from a seed in the
//! test, never the product key, and minisign files written the way `minisign -S` writes them
//! (pre-hashed: Ed25519 over the file's BLAKE2b-512, then the global signature over the
//! signature and the trusted comment). Test builds only: the library's unit tests and the
//! helper's (`#[path]`) both include it. minisign-verify checking these files is what proves
//! the BLAKE2b below; the RFC 7693 vector pins it on its own.

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use base64::Engine;

/// A key pair and its minisign key id.
pub struct TestKey {
    pair: Ed25519KeyPair,
    id: [u8; 8],
}

impl TestKey {
    /// The key made from `seed` (tests use a few fixed seeds: deterministic files).
    pub fn new(seed: u8) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("an Ed25519 seed");
        Self { pair, id: [seed, 0x7e, 0x57, 0x4b, 0x45, 0x59, 0x00, 0x01] }
    }

    /// minisign's `.pub` file for this key.
    pub fn public_file(&self) -> String {
        let mut bin = b"Ed".to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(self.pair.public_key().as_ref());
        format!("untrusted comment: minisign public key {:016X}\n{}\n", u64::from_le_bytes(self.id), b64(&bin))
    }

    /// The `.minisig` file of `data` (pre-hashed, as `minisign -S` makes it).
    pub fn sign(&self, data: &[u8]) -> String {
        self.sign_as(b"ED", &blake2b_512(data))
    }

    /// A legacy (`Ed`) signature: Ed25519 over the bytes themselves.
    pub fn sign_legacy(&self, data: &[u8]) -> String {
        self.sign_as(b"Ed", data)
    }

    fn sign_as(&self, alg: &[u8; 2], message: &[u8]) -> String {
        let sig = self.pair.sign(message);
        let mut bin = alg.to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(sig.as_ref());
        let trusted = "timestamp:1791000000\tfile:test.pkg.tar.zst\thashed";
        let mut global = sig.as_ref().to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let global = self.pair.sign(&global);
        format!("untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {trusted}\n{}\n", b64(&bin), b64(global.as_ref()))
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// BLAKE2b-512 (RFC 7693), unkeyed.
pub fn blake2b_512(data: &[u8]) -> [u8; 64] {
    const IV: [u64; 8] = [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];
    const SIGMA: [[usize; 16]; 12] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
        [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
        [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
        [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
        [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
        [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
        [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
        [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
        [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    ];
    fn g(v: &mut [u64; 16], (a, b, c, d): (usize, usize, usize, usize), x: u64, y: u64) {
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(32);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(24);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(63);
    }
    let compress = |h: &mut [u64; 8], block: &[u8], t: u128, last: bool| {
        let mut padded = [0u8; 128];
        padded[..block.len()].copy_from_slice(block);
        let m: Vec<u64> = padded.chunks(8).map(|w| u64::from_le_bytes(w.try_into().expect("8 bytes"))).collect();
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= t as u64;
        v[13] ^= (t >> 64) as u64;
        if last {
            v[14] = !v[14];
        }
        for s in &SIGMA {
            g(&mut v, (0, 4, 8, 12), m[s[0]], m[s[1]]);
            g(&mut v, (1, 5, 9, 13), m[s[2]], m[s[3]]);
            g(&mut v, (2, 6, 10, 14), m[s[4]], m[s[5]]);
            g(&mut v, (3, 7, 11, 15), m[s[6]], m[s[7]]);
            g(&mut v, (0, 5, 10, 15), m[s[8]], m[s[9]]);
            g(&mut v, (1, 6, 11, 12), m[s[10]], m[s[11]]);
            g(&mut v, (2, 7, 8, 13), m[s[12]], m[s[13]]);
            g(&mut v, (3, 4, 9, 14), m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            h[i] ^= v[i] ^ v[i + 8];
        }
    };
    let mut h = IV;
    h[0] ^= 0x0101_0000 ^ 64;
    let blocks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(128).collect() };
    let mut t = 0u128;
    for (i, block) in blocks.iter().enumerate() {
        t += block.len() as u128;
        compress(&mut h, block, t, i + 1 == blocks.len());
    }
    let mut out = [0u8; 64];
    for (i, w) in h.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}
