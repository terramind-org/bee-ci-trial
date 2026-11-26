//! Sealed boxes (libsodium `crypto_box_seal`), the encryption GitHub clients use for Actions
//! secrets: `gh secret set` and Octokit encrypt a value to the repo's public key, and the
//! gateway opens it before handing the plaintext to the forge's own secret store (which seals
//! it again with the server key).
//!
//! The construction, written from the published libsodium and NaCl descriptions:
//!
//! - `ephemeral_pk ‖ crypto_box(m, nonce, recipient_pk, ephemeral_sk)`;
//! - `nonce = BLAKE2b-192(ephemeral_pk ‖ recipient_pk)`;
//! - `crypto_box` = X25519, then HSalsa20 with a zero nonce (`beforenm`), then
//!   XSalsa20-Poly1305 (`crypto_secretbox`): the first 32 keystream bytes key Poly1305, the
//!   rest encrypt; the 16-byte tag precedes the ciphertext.
//!
//! X25519 is `curve25519-dalek`, Poly1305 the `poly1305` crate; Salsa20, HSalsa20 and
//! BLAKE2b are implemented here.

use curve25519_dalek::montgomery::MontgomeryPoint;
use poly1305::Poly1305;
use poly1305::universal_hash::KeyInit;

/// Bytes a sealed box adds to its message.
pub const SEAL_OVERHEAD: usize = 48;

const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn quarter(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[b] ^= x[a].wrapping_add(x[d]).rotate_left(7);
    x[c] ^= x[b].wrapping_add(x[a]).rotate_left(9);
    x[d] ^= x[c].wrapping_add(x[b]).rotate_left(13);
    x[a] ^= x[d].wrapping_add(x[c]).rotate_left(18);
}

fn rounds(x: &mut [u32; 16]) {
    for _ in 0..10 {
        quarter(x, 0, 4, 8, 12);
        quarter(x, 5, 9, 13, 1);
        quarter(x, 10, 14, 2, 6);
        quarter(x, 15, 3, 7, 11);
        quarter(x, 0, 1, 2, 3);
        quarter(x, 5, 6, 7, 4);
        quarter(x, 10, 11, 8, 9);
        quarter(x, 15, 12, 13, 14);
    }
}

fn initial(key: &[u8; 32], input: &[u8; 16]) -> [u32; 16] {
    [
        SIGMA[0],
        le(&key[0..]),
        le(&key[4..]),
        le(&key[8..]),
        le(&key[12..]),
        SIGMA[1],
        le(&input[0..]),
        le(&input[4..]),
        le(&input[8..]),
        le(&input[12..]),
        SIGMA[2],
        le(&key[16..]),
        le(&key[20..]),
        le(&key[24..]),
        le(&key[28..]),
        SIGMA[3],
    ]
}

/// HSalsa20: a 32-byte subkey from a key and a 16-byte input.
pub fn hsalsa20(key: &[u8; 32], input: &[u8; 16]) -> [u8; 32] {
    let mut x = initial(key, input);
    rounds(&mut x);
    let mut out = [0u8; 32];
    for (i, w) in [x[0], x[5], x[10], x[15], x[6], x[7], x[8], x[9]]
        .iter()
        .enumerate()
    {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// The Salsa20 keystream for `key` and an 8-byte nonce, `len` bytes from block 0.
fn salsa20_stream(key: &[u8; 32], nonce: &[u8; 8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    let mut counter: u64 = 0;
    while out.len() < len {
        let mut input = [0u8; 16];
        input[..8].copy_from_slice(nonce);
        input[8..].copy_from_slice(&counter.to_le_bytes());
        let start = initial(key, &input);
        let mut x = start;
        rounds(&mut x);
        for i in 0..16 {
            out.extend_from_slice(&x[i].wrapping_add(start[i]).to_le_bytes());
        }
        counter += 1;
    }
    out.truncate(len);
    out
}

/// The XSalsa20 keystream (24-byte nonce).
fn xsalsa20_stream(key: &[u8; 32], nonce: &[u8; 24], len: usize) -> Vec<u8> {
    let mut first = [0u8; 16];
    first.copy_from_slice(&nonce[..16]);
    let sub = hsalsa20(key, &first);
    let mut rest = [0u8; 8];
    rest.copy_from_slice(&nonce[16..]);
    salsa20_stream(&sub, &rest, len)
}

fn poly_tag(key: &[u8], data: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    if let Ok(mac) = Poly1305::new_from_slice(key) {
        out.copy_from_slice(&mac.compute_unpadded(data));
    }
    out
}

/// `crypto_secretbox` (XSalsa20-Poly1305): `tag ‖ ciphertext`.
pub fn secretbox_seal(key: &[u8; 32], nonce: &[u8; 24], msg: &[u8]) -> Vec<u8> {
    let ks = xsalsa20_stream(key, nonce, 32 + msg.len());
    let ct: Vec<u8> = msg.iter().zip(&ks[32..]).map(|(m, k)| m ^ k).collect();
    let mut out = poly_tag(&ks[..32], &ct).to_vec();
    out.extend_from_slice(&ct);
    out
}

/// Opens a `crypto_secretbox`; `None` when the tag does not verify.
pub fn secretbox_open(key: &[u8; 32], nonce: &[u8; 24], boxed: &[u8]) -> Option<Vec<u8>> {
    if boxed.len() < 16 {
        return None;
    }
    let (tag, ct) = boxed.split_at(16);
    let ks = xsalsa20_stream(key, nonce, 32 + ct.len());
    let want = poly_tag(&ks[..32], ct);
    let diff = want.iter().zip(tag).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if diff != 0 {
        return None;
    }
    Some(ct.iter().zip(&ks[32..]).map(|(c, k)| c ^ k).collect())
}

const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

const SIGMA_B: [[usize; 16]; 10] = [
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
];

fn compress(h: &mut [u64; 8], block: &[u8; 128], t: u128, last: bool) {
    let mut m = [0u64; 16];
    for (i, w) in m.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&block[i * 8..i * 8 + 8]);
        *w = u64::from_le_bytes(b);
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }
    let g = |v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64| {
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(32);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(24);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(63);
    };
    for r in 0..12 {
        let s = &SIGMA_B[r % 10];
        g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

/// Unkeyed BLAKE2b with an `outlen`-byte digest (1 to 64).
pub fn blake2b(outlen: usize, data: &[u8]) -> Vec<u8> {
    let outlen = outlen.clamp(1, 64);
    let mut h = IV;
    h[0] ^= 0x0101_0000 ^ outlen as u64;
    let mut t: u128 = 0;
    let mut chunks = data.chunks(128).peekable();
    if data.is_empty() {
        compress(&mut h, &[0u8; 128], 0, true);
    }
    while let Some(c) = chunks.next() {
        let mut block = [0u8; 128];
        block[..c.len()].copy_from_slice(c);
        t += c.len() as u128;
        compress(&mut h, &block, t, chunks.peek().is_none());
    }
    let mut out = Vec::with_capacity(64);
    for w in h {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.truncate(outlen);
    out
}

/// The X25519 public key of `secret`.
pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint::mul_base_clamped(*secret).to_bytes()
}

fn nonce(epk: &[u8; 32], pk: &[u8; 32]) -> [u8; 24] {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(epk);
    input[32..].copy_from_slice(pk);
    let mut n = [0u8; 24];
    n.copy_from_slice(&blake2b(24, &input));
    n
}

fn beforenm(secret: &[u8; 32], public: &[u8; 32]) -> Option<[u8; 32]> {
    let shared = MontgomeryPoint(*public).mul_clamped(*secret).to_bytes();
    if shared.iter().all(|b| *b == 0) {
        return None;
    }
    Some(hsalsa20(&shared, &[0u8; 16]))
}

/// Seals `msg` to `recipient` with the ephemeral secret `ephemeral` (callers pass fresh
/// randomness; tests pass fixed bytes).
pub fn seal(recipient: &[u8; 32], ephemeral: &[u8; 32], msg: &[u8]) -> Option<Vec<u8>> {
    let epk = public_key(ephemeral);
    let k = beforenm(ephemeral, recipient)?;
    let mut out = epk.to_vec();
    out.extend_from_slice(&secretbox_seal(&k, &nonce(&epk, recipient), msg));
    Some(out)
}

/// Opens a sealed box with the recipient's secret key; `None` when it does not verify.
pub fn open(secret: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < SEAL_OVERHEAD {
        return None;
    }
    let mut epk = [0u8; 32];
    epk.copy_from_slice(&sealed[..32]);
    let pk = public_key(secret);
    let k = beforenm(secret, &epk)?;
    secretbox_open(&k, &nonce(&epk, &pk), &sealed[32..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn arr(s: &str) -> [u8; 32] {
        hex(s).try_into().unwrap()
    }

    #[test]
    fn blake2b_matches_rfc7693() {
        assert_eq!(
            blake2b(64, b"abc"),
            hex(
                "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
            )
        );
        assert_eq!(blake2b(24, b"").len(), 24);
        let long = vec![7u8; 300];
        assert_ne!(blake2b(32, &long), blake2b(32, &long[..299]));
    }

    #[test]
    fn x25519_and_hsalsa20_match_the_nacl_vectors() {
        let alice_sk = arr("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob_pk = arr("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let shared = MontgomeryPoint(bob_pk).mul_clamped(alice_sk).to_bytes();
        assert_eq!(
            shared.to_vec(),
            hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
        assert_eq!(
            beforenm(&alice_sk, &bob_pk).unwrap().to_vec(),
            hex("1b27556473e985d462cd51197a9a46c76009549eac6474f206c4ee0844f68389")
        );
    }

    #[test]
    fn seal_open_round_trip_and_tamper() {
        let sk = [5u8; 32];
        let pk = public_key(&sk);
        let boxed = seal(&pk, &[9u8; 32], b"hunter2").unwrap();
        assert_eq!(boxed.len(), 7 + SEAL_OVERHEAD);
        assert_eq!(open(&sk, &boxed).unwrap(), b"hunter2");
        let mut bad = boxed.clone();
        bad[40] ^= 1;
        assert!(open(&sk, &bad).is_none());
        assert!(open(&[6u8; 32], &boxed).is_none());
        let big = vec![0xab; 1000];
        assert_eq!(
            open(&sk, &seal(&pk, &[1u8; 32], &big).unwrap()).unwrap(),
            big
        );
    }
}
