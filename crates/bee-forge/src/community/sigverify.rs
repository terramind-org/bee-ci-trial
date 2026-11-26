//! Verification of commit and tag signatures (PRODUCT §4.9, "required signed commits").
//!
//! Two formats, both as `git` writes them into a `gpgsig` header or after a tag message:
//!
//! - **OpenPGP** (`-----BEGIN PGP SIGNATURE-----`): one version 4 signature packet over the
//!   payload (signature type 0x00 binary or 0x01 canonical text). Hashes SHA-256, SHA-384 and
//!   SHA-512 (SHA-1 and SHA-224 are refused). Public-key algorithms RSA (1 and 3, at least
//!   2048 bits), EdDSA over Ed25519 (22 and 27) and ECDSA over P-256 with SHA-256 or P-384
//!   with SHA-384 (19). The issuer is the issuer-fingerprint subpacket (33) or, without it,
//!   the issuer key id subpacket (16); it names the primary key or a subkey of a registered
//!   key ([`super::keys::SigningKeys::owner_of`]).
//! - **SSH** (`-----BEGIN SSH SIGNATURE-----`): an `SSHSIG` blob (version 1) in the `git`
//!   namespace with hash `sha256` or `sha512`. Keys `ssh-ed25519`, `ecdsa-sha2-nistp256`,
//!   `ecdsa-sha2-nistp384` and `ssh-rsa` with `rsa-sha2-256`/`rsa-sha2-512` signatures. The
//!   issuer is the `SHA256:` fingerprint of the public key the blob embeds.
//!
//! Everything here is a pure function of its inputs; the cryptography is `ring`'s. Owner
//! lookup and the verdict live in [`super::keys::SigningKeys::verify_signature`].

use ring::digest;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};

use super::common::base64_nopad;
use super::keys::{base64_decode, packets, v4_key_id};

/// The signature format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// OpenPGP.
    Gpg,
    /// SSH (`SSHSIG`).
    Ssh,
}

/// Why a signature did not verify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The signature (or the key) cannot be parsed.
    Malformed(String),
    /// A well-formed signature in an algorithm or hash this verifier refuses.
    Unsupported(String),
    /// The signature does not verify under the key.
    Bad,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Malformed(m) => write!(f, "malformed signature: {m}"),
            Failure::Unsupported(m) => write!(f, "unsupported signature: {m}"),
            Failure::Bad => f.write_str("the signature does not verify"),
        }
    }
}

fn malformed(m: impl Into<String>) -> Failure {
    Failure::Malformed(m.into())
}

/// A parsed signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Its format.
    pub scheme: Scheme,
    /// The key id it names, in the form keys are registered under: a GPG long key id in
    /// upper-case hex, or an SSH `SHA256:` fingerprint.
    pub issuer: String,
    body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Body {
    Pgp(PgpSig),
    Ssh(SshSig),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PgpSig {
    sig_type: u8,
    pk_algo: u8,
    hash_algo: u8,
    /// The hashed part of the packet: version through the hashed subpackets.
    hashed: Vec<u8>,
    left16: [u8; 2],
    /// Signature values (MPIs, or the raw 64 bytes of algorithm 27).
    values: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshSig {
    public_key: Vec<u8>,
    namespace: Vec<u8>,
    reserved: Vec<u8>,
    hash_alg: String,
    sig_alg: String,
    sig: Vec<u8>,
}

/// The base64 body of an ASCII-armored block labelled `label` (`PGP SIGNATURE`, `SSH
/// SIGNATURE`, …): armor headers and the CRC-24 line are skipped.
pub fn armor_body(text: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin)? + begin.len();
    let stop = text[start..].find(&end)? + start;
    let mut body = String::new();
    let mut lines = text[start..stop].lines().map(str::trim).peekable();
    // Armor headers (`Key: value`) end at an empty line.
    if lines.peek().is_some_and(|l| l.is_empty()) {
        lines.next();
    }
    let mut in_headers = true;
    for line in lines {
        if in_headers && line.contains(": ") {
            continue;
        }
        if in_headers && line.is_empty() {
            in_headers = false;
            continue;
        }
        in_headers = false;
        if line.starts_with('=') && line.len() == 5 {
            break; // CRC-24 checksum
        }
        body.push_str(line);
    }
    base64_decode(&body)
}

/// Parses an armored signature (OpenPGP or SSH).
pub fn parse(armored: &[u8]) -> Result<Signature, Failure> {
    let text = std::str::from_utf8(armored).map_err(|_| malformed("not UTF-8"))?;
    if text.contains("-----BEGIN SSH SIGNATURE-----") {
        let blob = armor_body(text, "SSH SIGNATURE").ok_or_else(|| malformed("bad SSH armor"))?;
        return parse_sshsig(&blob);
    }
    if text.contains("-----BEGIN PGP SIGNATURE-----") {
        let bytes = armor_body(text, "PGP SIGNATURE").ok_or_else(|| malformed("bad PGP armor"))?;
        return parse_pgp(&bytes);
    }
    if text.contains("-----BEGIN SIGNED MESSAGE-----") {
        return Err(Failure::Unsupported("X.509 (S/MIME) signatures".into()));
    }
    Err(malformed("no armored signature"))
}

/// Verifies `sig` over `payload` with a registered key (`public_key` as stored: an
/// armored OpenPGP key block or an OpenSSH line).
pub fn verify(sig: &Signature, payload: &[u8], public_key: &str) -> Result<(), Failure> {
    match &sig.body {
        Body::Pgp(p) => verify_pgp(p, &sig.issuer, payload, public_key),
        Body::Ssh(s) => verify_ssh(s, payload, public_key),
    }
}

// ----- OpenPGP ----------------------------------------------------------------------------

/// Reads one MPI: a 2-byte bit count, then the bytes.
fn mpi(b: &[u8]) -> Result<(Vec<u8>, &[u8]), Failure> {
    let bits = u16::from_be_bytes([
        *b.first().ok_or_else(|| malformed("truncated MPI"))?,
        *b.get(1).ok_or_else(|| malformed("truncated MPI"))?,
    ]) as usize;
    let n = bits.div_ceil(8);
    let v = b.get(2..2 + n).ok_or_else(|| malformed("truncated MPI"))?;
    Ok((v.to_vec(), &b[2 + n..]))
}

/// Subpackets: `(type without the critical bit, body)`.
fn subpackets(mut b: &[u8]) -> Result<Vec<(u8, &[u8])>, Failure> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let first = b[0] as usize;
        let (len, hdr) = match first {
            0..=191 => (first, 1),
            192..=254 => {
                let second = *b.get(1).ok_or_else(|| malformed("truncated subpacket"))? as usize;
                (((first - 192) << 8) + second + 192, 2)
            }
            _ => {
                let l = b
                    .get(1..5)
                    .ok_or_else(|| malformed("truncated subpacket"))?;
                (u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize, 5)
            }
        };
        let body = b
            .get(hdr..hdr + len)
            .ok_or_else(|| malformed("truncated subpacket"))?;
        let (ty, rest) = body
            .split_first()
            .ok_or_else(|| malformed("empty subpacket"))?;
        out.push((ty & 0x7f, rest));
        b = &b[hdr + len..];
    }
    Ok(out)
}

fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn parse_pgp(bytes: &[u8]) -> Result<Signature, Failure> {
    let pkts = packets(bytes).map_err(|e| malformed(e.to_string()))?;
    let body = match pkts.as_slice() {
        [(2, body)] => *body,
        _ => return Err(malformed("expected exactly one signature packet")),
    };
    match body.first() {
        Some(4) => {}
        Some(v) => return Err(Failure::Unsupported(format!("version {v} signature"))),
        None => return Err(malformed("empty signature packet")),
    }
    let get = |r: std::ops::Range<usize>| body.get(r).ok_or_else(|| malformed("truncated"));
    let head = get(0..6)?;
    let (sig_type, pk_algo, hash_algo) = (head[1], head[2], head[3]);
    let hashed_len = u16::from_be_bytes([head[4], head[5]]) as usize;
    let hashed_sub = get(6..6 + hashed_len)?;
    let at = 6 + hashed_len;
    let ul = get(at..at + 2)?;
    let unhashed_len = u16::from_be_bytes([ul[0], ul[1]]) as usize;
    let unhashed_sub = get(at + 2..at + 2 + unhashed_len)?;
    let at = at + 2 + unhashed_len;
    let l16 = get(at..at + 2)?;
    let mut rest = &body[at + 2..];
    let mut issuer = None;
    let mut issuer_fp = None;
    for (ty, b) in subpackets(hashed_sub)?
        .into_iter()
        .chain(subpackets(unhashed_sub)?)
    {
        match ty {
            16 if b.len() == 8 => issuer = issuer.or(Some(hex_upper(b))),
            33 if b.len() == 21 && b[0] == 4 => issuer_fp = issuer_fp.or(Some(hex_upper(&b[13..]))),
            _ => {}
        }
    }
    let issuer = issuer_fp
        .or(issuer)
        .ok_or_else(|| malformed("the signature names no issuer"))?;
    let mut values = Vec::new();
    if pk_algo == 27 {
        let raw = rest
            .get(..64)
            .ok_or_else(|| malformed("truncated Ed25519 signature"))?;
        values.push(raw.to_vec());
    } else {
        while !rest.is_empty() {
            let (v, r) = mpi(rest)?;
            values.push(v);
            rest = r;
        }
    }
    Ok(Signature {
        scheme: Scheme::Gpg,
        issuer,
        body: Body::Pgp(PgpSig {
            sig_type,
            pk_algo,
            hash_algo,
            hashed: body[..6 + hashed_len].to_vec(),
            left16: [l16[0], l16[1]],
            values,
        }),
    })
}

/// Public key material of one OpenPGP key packet.
enum PgpKey {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ed25519([u8; 32]),
    Ecdsa { curve: Curve, point: Vec<u8> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Curve {
    P256,
    P384,
}

const OID_ED25519: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01];
const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];

fn pgp_key(body: &[u8]) -> Result<PgpKey, Failure> {
    if body.first() != Some(&4) || body.len() < 6 {
        return Err(malformed("not a version 4 key packet"));
    }
    let algo = body[5];
    let rest = &body[6..];
    let oid = |b: &[u8]| -> Result<(Vec<u8>, Vec<u8>), Failure> {
        let n = *b.first().ok_or_else(|| malformed("truncated key"))? as usize;
        let oid = b.get(1..1 + n).ok_or_else(|| malformed("truncated key"))?;
        let (point, _) = mpi(&b[1 + n..])?;
        Ok((oid.to_vec(), point))
    };
    match algo {
        1..=3 => {
            let (n, rest) = mpi(rest)?;
            let (e, _) = mpi(rest)?;
            Ok(PgpKey::Rsa { n, e })
        }
        22 => {
            let (o, point) = oid(rest)?;
            if o != OID_ED25519 || point.len() != 33 || point[0] != 0x40 {
                return Err(Failure::Unsupported(
                    "EdDSA key on a curve other than Ed25519".into(),
                ));
            }
            let mut k = [0u8; 32];
            k.copy_from_slice(&point[1..]);
            Ok(PgpKey::Ed25519(k))
        }
        27 => {
            let raw = rest.get(..32).ok_or_else(|| malformed("truncated key"))?;
            let mut k = [0u8; 32];
            k.copy_from_slice(raw);
            Ok(PgpKey::Ed25519(k))
        }
        19 => {
            let (o, point) = oid(rest)?;
            let curve = if o == OID_P256 {
                Curve::P256
            } else if o == OID_P384 {
                Curve::P384
            } else {
                return Err(Failure::Unsupported(
                    "ECDSA key on an unsupported curve".into(),
                ));
            };
            Ok(PgpKey::Ecdsa { curve, point })
        }
        a => Err(Failure::Unsupported(format!("public-key algorithm {a}"))),
    }
}

/// The key packet (primary or subkey) of an armored key block whose id is `issuer`.
fn find_pgp_key(public_key: &str, issuer: &str) -> Result<PgpKey, Failure> {
    let bytes = armor_body(public_key, "PGP PUBLIC KEY BLOCK")
        .ok_or_else(|| malformed("the registered key is not an armored key block"))?;
    let pkts = packets(&bytes).map_err(|e| malformed(e.to_string()))?;
    for (tag, body) in pkts {
        if (tag == 6 || tag == 14) && v4_key_id(body).ok().as_deref() == Some(issuer) {
            return pgp_key(body);
        }
    }
    Err(malformed("the registered key does not contain the issuer"))
}

/// Left-pads `v` with zeros to `n` bytes (MPIs drop leading zeros).
fn pad(v: &[u8], n: usize) -> Result<Vec<u8>, Failure> {
    let v = match v.iter().position(|&b| b != 0) {
        Some(i) => &v[i..],
        None => &[][..],
    };
    if v.len() > n {
        return Err(Failure::Bad);
    }
    let mut out = vec![0u8; n - v.len()];
    out.extend_from_slice(v);
    Ok(out)
}

/// Converts lone `\n` to `\r\n` (canonical text, signature type 0x01).
fn canonical_text(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len() + b.len() / 32);
    let mut prev = 0u8;
    for &c in b {
        if c == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

fn verify_pgp(sig: &PgpSig, issuer: &str, payload: &[u8], public_key: &str) -> Result<(), Failure> {
    let data = match sig.sig_type {
        0x00 => payload.to_vec(),
        0x01 => canonical_text(payload),
        t => return Err(Failure::Unsupported(format!("signature type {t:#04x}"))),
    };
    let (dig_alg, rsa_alg): (
        &'static digest::Algorithm,
        &'static signature::RsaParameters,
    ) = match sig.hash_algo {
        8 => (&digest::SHA256, &signature::RSA_PKCS1_2048_8192_SHA256),
        9 => (&digest::SHA384, &signature::RSA_PKCS1_2048_8192_SHA384),
        10 => (&digest::SHA512, &signature::RSA_PKCS1_2048_8192_SHA512),
        h => return Err(Failure::Unsupported(format!("hash algorithm {h}"))),
    };
    // The signed message: data ‖ hashed part ‖ 0x04 0xFF ‖ u32be(len(hashed part)).
    let mut msg = data;
    msg.extend_from_slice(&sig.hashed);
    msg.extend_from_slice(&[0x04, 0xFF]);
    msg.extend_from_slice(&(sig.hashed.len() as u32).to_be_bytes());
    let dig = digest::digest(dig_alg, &msg);
    if dig.as_ref()[..2] != sig.left16 {
        return Err(Failure::Bad);
    }
    let key = find_pgp_key(public_key, issuer)?;
    match (key, sig.pk_algo) {
        (PgpKey::Rsa { n, e }, 1 | 3) => {
            let n = pad(&n, n.len())?;
            let s = sig
                .values
                .first()
                .ok_or_else(|| malformed("no RSA value"))?;
            let s = pad(s, n.len())?;
            RsaPublicKeyComponents { n: &n, e: &e }
                .verify(rsa_alg, &msg, &s)
                .map_err(|_| Failure::Bad)
        }
        (PgpKey::Ed25519(k), 22) => {
            let [r, s] = sig.values.as_slice() else {
                return Err(malformed("EdDSA needs two values"));
            };
            let mut rs = pad(r, 32)?;
            rs.extend(pad(s, 32)?);
            UnparsedPublicKey::new(&signature::ED25519, k)
                .verify(dig.as_ref(), &rs)
                .map_err(|_| Failure::Bad)
        }
        (PgpKey::Ed25519(k), 27) => {
            let rs = sig
                .values
                .first()
                .ok_or_else(|| malformed("no Ed25519 value"))?;
            UnparsedPublicKey::new(&signature::ED25519, k)
                .verify(dig.as_ref(), rs)
                .map_err(|_| Failure::Bad)
        }
        (PgpKey::Ecdsa { curve, point }, 19) => {
            let [r, s] = sig.values.as_slice() else {
                return Err(malformed("ECDSA needs two values"));
            };
            let (alg, n): (&'static signature::EcdsaVerificationAlgorithm, usize) =
                match (curve, sig.hash_algo) {
                    (Curve::P256, 8) => (&signature::ECDSA_P256_SHA256_FIXED, 32),
                    (Curve::P384, 9) => (&signature::ECDSA_P384_SHA384_FIXED, 48),
                    _ => {
                        return Err(Failure::Unsupported(
                            "ECDSA with a hash that does not match the curve".into(),
                        ));
                    }
                };
            let mut rs = pad(r, n)?;
            rs.extend(pad(s, n)?);
            UnparsedPublicKey::new(alg, &point)
                .verify(&msg, &rs)
                .map_err(|_| Failure::Bad)
        }
        _ => Err(Failure::Bad),
    }
}

// ----- SSH --------------------------------------------------------------------------------

/// Reads one SSH `string`.
fn ssh_string(b: &[u8]) -> Result<(&[u8], &[u8]), Failure> {
    let l = b
        .get(..4)
        .ok_or_else(|| malformed("truncated SSH string"))?;
    let n = u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize;
    let v = b
        .get(4..4 + n)
        .ok_or_else(|| malformed("truncated SSH string"))?;
    Ok((v, &b[4 + n..]))
}

fn put_string(out: &mut Vec<u8>, v: &[u8]) {
    out.extend_from_slice(&(v.len() as u32).to_be_bytes());
    out.extend_from_slice(v);
}

/// The `SHA256:` fingerprint of an SSH public key blob, as `ssh-keygen -l` prints it.
pub fn ssh_fingerprint(blob: &[u8]) -> String {
    format!(
        "SHA256:{}",
        base64_nopad(digest::digest(&digest::SHA256, blob).as_ref())
    )
}

fn parse_sshsig(blob: &[u8]) -> Result<Signature, Failure> {
    let rest = blob
        .strip_prefix(b"SSHSIG")
        .ok_or_else(|| malformed("not an SSHSIG blob"))?;
    let v = rest.get(..4).ok_or_else(|| malformed("truncated SSHSIG"))?;
    if v != [0, 0, 0, 1] {
        return Err(Failure::Unsupported("SSHSIG version".into()));
    }
    let (public_key, rest) = ssh_string(&rest[4..])?;
    let (namespace, rest) = ssh_string(rest)?;
    let (reserved, rest) = ssh_string(rest)?;
    let (hash_alg, rest) = ssh_string(rest)?;
    let (sigblob, _) = ssh_string(rest)?;
    let (sig_alg, rest) = ssh_string(sigblob)?;
    let (sig, _) = ssh_string(rest)?;
    Ok(Signature {
        scheme: Scheme::Ssh,
        issuer: ssh_fingerprint(public_key),
        body: Body::Ssh(SshSig {
            public_key: public_key.to_vec(),
            namespace: namespace.to_vec(),
            reserved: reserved.to_vec(),
            hash_alg: String::from_utf8_lossy(hash_alg).into_owned(),
            sig_alg: String::from_utf8_lossy(sig_alg).into_owned(),
            sig: sig.to_vec(),
        }),
    })
}

fn verify_ssh(sig: &SshSig, payload: &[u8], public_key: &str) -> Result<(), Failure> {
    // The registered key must be the key the signature embeds.
    let mut parts = public_key.split_whitespace();
    let registered = parts
        .nth(1)
        .and_then(base64_decode)
        .ok_or_else(|| malformed("the registered key is not an OpenSSH line"))?;
    if registered != sig.public_key {
        return Err(Failure::Bad);
    }
    if sig.namespace != b"git" {
        return Err(Failure::Bad);
    }
    let h = match sig.hash_alg.as_str() {
        "sha256" => digest::digest(&digest::SHA256, payload),
        "sha512" => digest::digest(&digest::SHA512, payload),
        other => return Err(Failure::Unsupported(format!("SSHSIG hash {other}"))),
    };
    let mut signed = b"SSHSIG".to_vec();
    put_string(&mut signed, &sig.namespace);
    put_string(&mut signed, &sig.reserved);
    put_string(&mut signed, sig.hash_alg.as_bytes());
    put_string(&mut signed, h.as_ref());
    let (key_type, rest) = ssh_string(&sig.public_key)?;
    match (key_type, sig.sig_alg.as_str()) {
        (b"ssh-ed25519", "ssh-ed25519") => {
            let (k, _) = ssh_string(rest)?;
            UnparsedPublicKey::new(&signature::ED25519, k)
                .verify(&signed, &sig.sig)
                .map_err(|_| Failure::Bad)
        }
        (b"ecdsa-sha2-nistp256", "ecdsa-sha2-nistp256")
        | (b"ecdsa-sha2-nistp384", "ecdsa-sha2-nistp384") => {
            let (_, rest) = ssh_string(rest)?;
            let (point, _) = ssh_string(rest)?;
            let (r, s_rest) = ssh_string(&sig.sig)?;
            let (s, _) = ssh_string(s_rest)?;
            let (alg, n): (&'static signature::EcdsaVerificationAlgorithm, usize) =
                if key_type == b"ecdsa-sha2-nistp256" {
                    (&signature::ECDSA_P256_SHA256_FIXED, 32)
                } else {
                    (&signature::ECDSA_P384_SHA384_FIXED, 48)
                };
            let mut rs = pad(r, n)?;
            rs.extend(pad(s, n)?);
            UnparsedPublicKey::new(alg, point)
                .verify(&signed, &rs)
                .map_err(|_| Failure::Bad)
        }
        (b"ssh-rsa", alg @ ("rsa-sha2-256" | "rsa-sha2-512")) => {
            let (e, rest) = ssh_string(rest)?;
            let (n, _) = ssh_string(rest)?;
            let n = match n.iter().position(|&b| b != 0) {
                Some(i) => &n[i..],
                None => return Err(Failure::Bad),
            };
            let params = if alg == "rsa-sha2-256" {
                &signature::RSA_PKCS1_2048_8192_SHA256
            } else {
                &signature::RSA_PKCS1_2048_8192_SHA512
            };
            let s = pad(&sig.sig, n.len())?;
            RsaPublicKeyComponents { n, e }
                .verify(params, &signed, &s)
                .map_err(|_| Failure::Bad)
        }
        (_, "ssh-rsa") => Err(Failure::Unsupported("SHA-1 RSA signatures".into())),
        (t, _) => Err(Failure::Unsupported(format!(
            "SSH key type {}",
            String::from_utf8_lossy(t)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armor_skips_headers_and_checksum() {
        let text = "-----BEGIN PGP SIGNATURE-----\nComment: x\n\naGVsbG8=\n=abcd\n-----END PGP SIGNATURE-----\n";
        assert_eq!(armor_body(text, "PGP SIGNATURE").unwrap(), b"hello");
        let bare = "-----BEGIN SSH SIGNATURE-----\naGVs\nbG8=\n-----END SSH SIGNATURE-----";
        assert_eq!(armor_body(bare, "SSH SIGNATURE").unwrap(), b"hello");
    }

    #[test]
    fn canonical_text_adds_carriage_returns_once() {
        assert_eq!(canonical_text(b"a\nb\r\nc"), b"a\r\nb\r\nc");
    }

    #[test]
    fn garbage_is_malformed() {
        assert!(matches!(parse(b"nope"), Err(Failure::Malformed(_))));
        assert!(matches!(
            parse(b"-----BEGIN SSH SIGNATURE-----\nAAAA\n-----END SSH SIGNATURE-----\n"),
            Err(Failure::Malformed(_))
        ));
    }
}
