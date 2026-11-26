//! Keys, JWTs and PKCE for the apps platform.
//!
//! - App private keys are Ed25519 (PKCS#8, PEM). Only the public key and its SHA-256
//!   fingerprint are stored.
//! - App JWTs are `EdDSA` JWTs: header `{"alg":"EdDSA","typ":"JWT","kid":<key id>}`, claims
//!   `iss` (the app's client id or id), `iat`, `exp` (at most ten minutes after `iat`).
//! - PKCE uses the `S256` method only.

use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};

use crate::enterprise::common::ct_eq;
use crate::error::{ForgeError, Result};

/// Longest allowed distance between a JWT's `iat` and `exp`, in seconds.
pub const MAX_JWT_LIFETIME_SECS: u64 = 600;
/// How far in the future a JWT's `iat` may be (clock skew), in seconds.
pub const JWT_SKEW_SECS: u64 = 60;

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode(alphabet: &[u8; 64], data: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        let keep = chunk.len() + 1;
        for (i, ix) in idx.iter().enumerate() {
            if i < keep {
                out.push(alphabet[*ix as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 with padding.
pub fn b64(data: &[u8]) -> String {
    encode(STD, data, true)
}

/// URL-safe base64 without padding.
pub fn b64url(data: &[u8]) -> String {
    encode(URL, data, false)
}

fn decode_with(s: &str, std: bool) -> Option<Vec<u8>> {
    let mut buf: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' if std => 62,
            b'/' if std => 63,
            b'-' if !std => 62,
            b'_' if !std => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Decodes standard base64 (padding optional).
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    decode_with(s, true)
}

/// Decodes URL-safe base64 (padding optional).
pub fn unb64url(s: &str) -> Option<Vec<u8>> {
    decode_with(s, false)
}

/// A freshly generated app key.
pub struct NewKey {
    /// PKCS#8 PEM of the private key (returned to the caller once).
    pub pem: String,
    /// The 32-byte public key.
    pub public_key: Vec<u8>,
    /// SHA-256 of the public key, base64.
    pub fingerprint: String,
}

/// SHA-256 fingerprint (base64) of a public key.
pub fn fingerprint(public_key: &[u8]) -> String {
    b64(&Sha256::digest(public_key))
}

/// Generates an Ed25519 key pair.
pub fn generate_key() -> Result<NewKey> {
    let rng = SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| ForgeError::internal("cannot generate an Ed25519 key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref())
        .map_err(|_| ForgeError::internal("generated key does not parse"))?;
    let public_key = pair.public_key().as_ref().to_vec();
    let body = b64(doc.as_ref());
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for line in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).unwrap_or(""));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    Ok(NewKey {
        fingerprint: fingerprint(&public_key),
        public_key,
        pem,
    })
}

fn parse_pem(pem: &str) -> Result<Ed25519KeyPair> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    let der = unb64(&body)
        .ok_or_else(|| ForgeError::invalid("bad_private_key", "private key is not base64 PEM"))?;
    Ed25519KeyPair::from_pkcs8(&der)
        .map_err(|_| ForgeError::invalid("bad_private_key", "not an Ed25519 PKCS#8 key"))
}

/// Signs an app JWT with a PKCS#8 PEM private key (what a client of the apps API does).
pub fn sign_app_jwt(
    private_key_pem: &str,
    key_id: &str,
    iss: &str,
    iat: u64,
    exp: u64,
) -> Result<String> {
    let pair = parse_pem(private_key_pem)?;
    let header = serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": key_id });
    let claims = serde_json::json!({ "iss": iss, "iat": iat, "exp": exp });
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let sig = pair.sign(input.as_bytes());
    Ok(format!("{input}.{}", b64url(sig.as_ref())))
}

/// The unverified parts of an app JWT.
#[derive(Clone, Debug)]
pub struct JwtParts {
    /// `kid` header, if any.
    pub kid: Option<String>,
    /// `iss` claim.
    pub iss: String,
    /// `iat` claim.
    pub iat: u64,
    /// `exp` claim.
    pub exp: u64,
    /// The signed input (`header.claims`).
    pub signing_input: String,
    /// The signature bytes.
    pub signature: Vec<u8>,
}

fn bad_jwt(m: &str) -> ForgeError {
    ForgeError::new(
        bee_proto::v1::ErrorReason::Unauthenticated,
        format!("invalid app JWT: {m}"),
    )
}

/// Whether a bearer credential looks like a JWT (two dots) rather than a Biscuit (none).
pub fn looks_like_jwt(bearer: &str) -> bool {
    bearer.bytes().filter(|b| *b == b'.').count() == 2
}

/// Parses a JWT and checks its header and claim shapes (not the signature).
pub fn parse_jwt(jwt: &str) -> Result<JwtParts> {
    let mut it = jwt.split('.');
    let (Some(h), Some(c), Some(s), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err(bad_jwt("expected three parts"));
    };
    let header: serde_json::Value = unb64url(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| bad_jwt("header is not JSON"))?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("EdDSA") {
        return Err(bad_jwt("alg must be EdDSA"));
    }
    let claims: serde_json::Value = unb64url(c)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| bad_jwt("claims are not JSON"))?;
    let iss = match claims.get("iss") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => return Err(bad_jwt("missing iss")),
    };
    let num = |k: &str| {
        claims
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| bad_jwt(&format!("missing {k}")))
    };
    Ok(JwtParts {
        kid: header
            .get("kid")
            .and_then(|k| k.as_str())
            .map(str::to_string),
        iss,
        iat: num("iat")?,
        exp: num("exp")?,
        signing_input: format!("{h}.{c}"),
        signature: unb64url(s).ok_or_else(|| bad_jwt("signature is not base64url"))?,
    })
}

/// Checks the time claims at `now` (unix seconds).
pub fn check_jwt_times(p: &JwtParts, now: u64) -> Result<()> {
    if p.exp <= p.iat {
        return Err(bad_jwt("exp is not after iat"));
    }
    if p.exp - p.iat > MAX_JWT_LIFETIME_SECS {
        return Err(bad_jwt("exp is more than 10 minutes after iat"));
    }
    if p.iat > now + JWT_SKEW_SECS {
        return Err(bad_jwt("iat is in the future"));
    }
    if now >= p.exp {
        return Err(ForgeError::new(
            bee_proto::v1::ErrorReason::TokenExpired,
            "app JWT expired",
        ));
    }
    Ok(())
}

/// Verifies the signature of parsed JWT parts against a public key.
pub fn verify_jwt_signature(p: &JwtParts, public_key: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(p.signing_input.as_bytes(), &p.signature)
        .is_ok()
}

/// Whether a PKCE verifier matches a stored S256 challenge (constant-time).
pub fn pkce_matches(challenge: &str, verifier: &str) -> bool {
    if verifier.len() < 43 || verifier.len() > 128 {
        return false;
    }
    let computed = b64url(&Sha256::digest(verifier.as_bytes()));
    ct_eq(&computed, challenge)
}

/// The S256 challenge of a verifier (what a client computes).
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(unb64(&b64(&data)).unwrap(), data);
            assert_eq!(unb64url(&b64url(&data)).unwrap(), data);
        }
        assert_eq!(b64(b"Man"), "TWFu");
        assert_eq!(b64(b"Ma"), "TWE=");
        assert_eq!(b64url(b"\xfb\xff"), "-_8");
    }

    #[test]
    fn jwt_signs_and_verifies() {
        let k = generate_key().unwrap();
        let jwt = sign_app_jwt(&k.pem, "K1", "Iv1.abc", 1000, 1500).unwrap();
        assert!(looks_like_jwt(&jwt));
        let p = parse_jwt(&jwt).unwrap();
        assert_eq!(p.kid.as_deref(), Some("K1"));
        assert!(verify_jwt_signature(&p, &k.public_key));
        check_jwt_times(&p, 1100).unwrap();
        assert!(check_jwt_times(&p, 1500).is_err());
        let other = generate_key().unwrap();
        assert!(!verify_jwt_signature(&p, &other.public_key));
        let long = parse_jwt(&sign_app_jwt(&k.pem, "K1", "x", 1000, 1700).unwrap()).unwrap();
        assert!(check_jwt_times(&long, 1100).is_err());
    }

    #[test]
    fn pkce_s256() {
        let v = "a".repeat(50);
        let c = pkce_challenge(&v);
        assert!(pkce_matches(&c, &v));
        assert!(!pkce_matches(&c, &"b".repeat(50)));
        assert!(!pkce_matches(&c, "short"));
    }
}
