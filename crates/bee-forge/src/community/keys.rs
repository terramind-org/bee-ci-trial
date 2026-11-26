//! Signing keys of user accounts (PRODUCT §4.9): OpenPGP and SSH public keys that verify
//! signatures on changesets and tags.
//!
//! Keys are parsed on upload, never trusted as given:
//!
//! - **SSH**: one OpenSSH public key line (`<type> <base64> [comment]`). The base64 blob must
//!   decode and start with its own type string. `key_id` is the `SHA256:` fingerprint
//!   `ssh-keygen -l` prints.
//! - **GPG**: an ASCII-armored public key block. Packets are walked (old and new length
//!   formats); the primary key must be a version 4 public-key packet. `key_id` is the 64-bit
//!   key id (the low 8 bytes of the v4 fingerprint, `SHA-1(0x99 ‖ len16 ‖ body)`), subkeys
//!   are listed the same way, and `emails` are the user ids' addresses that are verified on
//!   the account.
//!
//! A key id is unique across all accounts (`ALREADY_EXISTS`), so a signature names exactly
//! one owner ([`SigningKeys::owner_of`]).

use bee_proto::v1::{SubjectKind, UserSigningKey, UserSigningKeyKind};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Db, Index, Key, Row, Scan, TableDef};

use super::common::{base64_nopad, draft, max_len};
use super::users::USERS;

/// Signing keys: scope the owner, id the key's ULID, `k1` the key id (unique), `n1` added.
pub static SIGNING_KEYS: TableDef = TableDef {
    name: "forge_user_signing_keys",
    surface: USERS,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::K1, Index::ScopeN1],
};

/// Most signing keys per account.
pub const MAX_KEYS: usize = 50;
/// Longest key text accepted.
pub const MAX_KEY_TEXT: usize = 64 * 1024;

const SSH_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// A stored key.
#[derive(Clone, PartialEq, Message)]
pub struct KeyRec {
    /// The key as returned to clients.
    #[prost(message, optional, tag = "1")]
    pub key: Option<UserSigningKey>,
    /// The owner.
    #[prost(string, tag = "2")]
    pub owner: String,
}

/// What a parsed key yields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// Normalized public key text.
    pub public_key: String,
    /// Key id (GPG long id in upper-case hex, or SSH `SHA256:` fingerprint).
    pub key_id: String,
    /// GPG subkey ids.
    pub subkey_ids: Vec<String>,
    /// Addresses in GPG user ids, lower-cased.
    pub emails: Vec<String>,
}

fn bad(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("invalid_key", msg)
}

/// Standard base64 (padding optional, whitespace ignored).
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn hex_upper(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Parses an OpenSSH public key line.
pub fn parse_ssh(text: &str) -> Result<Parsed> {
    let mut parts = text.split_whitespace();
    let (Some(kind), Some(b64)) = (parts.next(), parts.next()) else {
        return Err(bad("an SSH key is `<type> <base64> [comment]`"));
    };
    if !SSH_TYPES.contains(&kind) {
        return Err(bad(format!("unsupported SSH key type {kind}")));
    }
    let blob = base64_decode(b64).ok_or_else(|| bad("the SSH key is not base64"))?;
    if blob.len() < 4 {
        return Err(bad("the SSH key is truncated"));
    }
    let n = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    if blob.len() < 4 + n || &blob[4..4 + n] != kind.as_bytes() || blob.len() == 4 + n {
        return Err(bad("the SSH key blob does not match its type"));
    }
    use sha2::Digest;
    let fp = format!("SHA256:{}", base64_nopad(&sha2::Sha256::digest(&blob)));
    Ok(Parsed {
        public_key: format!("{kind} {b64}"),
        key_id: fp,
        subkey_ids: Vec::new(),
        emails: Vec::new(),
    })
}

/// One OpenPGP packet: tag and body.
pub(crate) fn packets(mut b: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let h = b[0];
        if h & 0x80 == 0 {
            return Err(bad("not an OpenPGP packet"));
        }
        let (tag, len, hdr) = if h & 0x40 != 0 {
            let tag = h & 0x3f;
            let first = *b.get(1).ok_or_else(|| bad("truncated packet"))? as usize;
            match first {
                0..=191 => (tag, first, 2),
                192..=223 => {
                    let second = *b.get(2).ok_or_else(|| bad("truncated packet"))? as usize;
                    (tag, ((first - 192) << 8) + second + 192, 3)
                }
                255 => {
                    let l = b.get(2..6).ok_or_else(|| bad("truncated packet"))?;
                    (
                        tag,
                        u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize,
                        6,
                    )
                }
                _ => return Err(bad("partial-length packets are not supported in keys")),
            }
        } else {
            let tag = (h >> 2) & 0x0f;
            match h & 3 {
                0 => (
                    tag,
                    *b.get(1).ok_or_else(|| bad("truncated packet"))? as usize,
                    2,
                ),
                1 => {
                    let l = b.get(1..3).ok_or_else(|| bad("truncated packet"))?;
                    (tag, u16::from_be_bytes([l[0], l[1]]) as usize, 3)
                }
                2 => {
                    let l = b.get(1..5).ok_or_else(|| bad("truncated packet"))?;
                    (
                        tag,
                        u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize,
                        5,
                    )
                }
                _ => return Err(bad("indeterminate-length packets are not supported")),
            }
        };
        let body = b
            .get(hdr..hdr + len)
            .ok_or_else(|| bad("truncated packet"))?;
        out.push((tag, body));
        b = &b[hdr + len..];
    }
    Ok(out)
}

/// The v4 key id of a public-key or public-subkey packet body.
pub(crate) fn v4_key_id(body: &[u8]) -> Result<String> {
    if body.first() != Some(&4) {
        return Err(bad("only version 4 OpenPGP keys are supported"));
    }
    if body.len() < 7 || body.len() > u16::MAX as usize {
        return Err(bad("malformed OpenPGP key packet"));
    }
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(&[0x99]);
    ctx.update(&(body.len() as u16).to_be_bytes());
    ctx.update(body);
    let fp = ctx.finish();
    Ok(hex_upper(&fp.as_ref()[12..]))
}

/// Parses an ASCII-armored OpenPGP public key block.
pub fn parse_gpg(text: &str) -> Result<Parsed> {
    let begin = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
    let end = "-----END PGP PUBLIC KEY BLOCK-----";
    let start = text
        .find(begin)
        .ok_or_else(|| bad("not an armored OpenPGP public key block"))?;
    let rest = &text[start + begin.len()..];
    let stop = rest
        .find(end)
        .ok_or_else(|| bad("the key block is not closed"))?;
    let mut body = String::new();
    let mut in_headers = true;
    for line in rest[..stop].lines() {
        let line = line.trim();
        if in_headers {
            if line.is_empty() {
                in_headers = false;
            } else if !line.contains(": ") {
                // No armor headers: this line is already data.
                in_headers = false;
                body.push_str(line);
            }
            continue;
        }
        if line.starts_with('=') {
            break; // CRC-24 checksum
        }
        body.push_str(line);
    }
    let bytes = base64_decode(&body).ok_or_else(|| bad("the key block is not base64"))?;
    let pkts = packets(&bytes)?;
    let Some((6, primary)) = pkts.first().copied() else {
        return Err(bad("the key block does not start with a public key"));
    };
    let key_id = v4_key_id(primary)?;
    let mut subkey_ids = Vec::new();
    let mut emails = Vec::new();
    for (tag, b) in &pkts[1..] {
        match tag {
            14 => subkey_ids.push(v4_key_id(b)?),
            13 => {
                let uid = String::from_utf8_lossy(b);
                if let (Some(a), Some(z)) = (uid.rfind('<'), uid.rfind('>'))
                    && a < z
                {
                    let e = uid[a + 1..z].trim().to_lowercase();
                    if e.contains('@') && !emails.contains(&e) {
                        emails.push(e);
                    }
                }
            }
            6 => return Err(bad("one key per upload")),
            _ => {}
        }
    }
    Ok(Parsed {
        public_key: text[start..start + begin.len() + stop + end.len()].to_string(),
        key_id,
        subkey_ids,
        emails,
    })
}

/// The signing-key store.
pub struct SigningKeys {
    db: Db,
}

impl SigningKeys {
    /// The store over `db`.
    pub fn new(db: Db) -> SigningKeys {
        SigningKeys { db }
    }

    /// Adds a key to the caller's account. `verified` are the caller's verified emails; GPG
    /// user ids keep only those.
    pub async fn add(
        &self,
        cx: &Cx,
        kind: UserSigningKeyKind,
        title: &str,
        text: &str,
        verified: &[String],
    ) -> Result<UserSigningKey> {
        max_len("public_key", text, MAX_KEY_TEXT)?;
        max_len("title", title, super::common::MAX_TITLE)?;
        let parsed = match kind {
            UserSigningKeyKind::Ssh => parse_ssh(text.trim())?,
            UserSigningKeyKind::Gpg => parse_gpg(text)?,
            UserSigningKeyKind::Unspecified => {
                return Err(ForgeError::missing("kind"));
            }
        };
        let mut tx = self.db.begin(USERS, cx).await?;
        if tx
            .scan(
                &SIGNING_KEYS,
                &Scan::scope(cx.actor_id.clone(), MAX_KEYS + 1),
            )
            .await?
            .len()
            >= MAX_KEYS
        {
            return Err(ForgeError::too_large(
                "signing_keys",
                MAX_KEYS as u64,
                MAX_KEYS as u64 + 1,
            ));
        }
        tx.add_n1(
            &SIGNING_KEYS,
            "",
            &format!("signing_key:{}", parsed.key_id),
            0,
        )
        .await?;
        let owned_elsewhere = tx
            .scan(
                &SIGNING_KEYS,
                &Scan {
                    key: Some(Key::K1(parsed.key_id.clone())),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?;
        if !owned_elsewhere.is_empty() {
            return Err(ForgeError::already_exists(format!(
                "signing key {}",
                parsed.key_id
            )));
        }
        let id = tx.new_id()?;
        let now = tx.now_ms();
        let key = UserSigningKey {
            id: id.clone(),
            kind: kind as i32,
            title: title.to_string(),
            public_key: parsed.public_key,
            key_id: parsed.key_id.clone(),
            emails: parsed
                .emails
                .into_iter()
                .filter(|e| verified.iter().any(|v| v.eq_ignore_ascii_case(e)))
                .collect(),
            subkey_ids: parsed.subkey_ids,
            expires_at: None,
            created_at: Some(ts(now)),
        };
        tx.insert(
            &SIGNING_KEYS,
            &Row::new(cx.actor_id.clone(), id.clone())
                .k1(parsed.key_id.clone())
                .n1(now as i64)
                .msg(&KeyRec {
                    key: Some(key.clone()),
                    owner: cx.actor_id.clone(),
                }),
        )
        .await?;
        let after = serde_json::json!({"id": id, "key_id": key.key_id, "kind": kind.as_str_name()});
        tx.audit(draft(
            "user.signing_key_add",
            "",
            "",
            SubjectKind::User,
            &cx.actor_id,
            &after,
        ))
        .await?;
        tx.commit().await?;
        Ok(key)
    }

    /// A user's keys, oldest first, optionally of one kind.
    pub async fn list(
        &self,
        cx: &Cx,
        owner: &str,
        kind: UserSigningKeyKind,
    ) -> Result<Vec<UserSigningKey>> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<KeyRec>(&SIGNING_KEYS, &Scan::scope(owner, MAX_KEYS).by_n1())
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(r, _)| r.key)
            .filter(|k| kind == UserSigningKeyKind::Unspecified || k.kind == kind as i32)
            .collect())
    }

    /// Removes one of the caller's keys, by its id or key id.
    pub async fn delete(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<KeyRec>(&SIGNING_KEYS, &Scan::scope(cx.actor_id.clone(), MAX_KEYS))
            .await?;
        let Some((_, row)) = rows
            .into_iter()
            .find(|(r, row)| row.id == id || r.key.as_ref().is_some_and(|k| k.key_id == id))
        else {
            return Err(ForgeError::not_found(format!("signing key {id}")));
        };
        tx.delete(&SIGNING_KEYS, &row.scope, &row.id).await?;
        let after = serde_json::json!({"id": row.id, "key_id": row.k1});
        tx.audit(draft(
            "user.signing_key_delete",
            "",
            "",
            SubjectKind::User,
            &cx.actor_id,
            &after,
        ))
        .await?;
        tx.commit().await
    }

    /// The account a key id (primary or subkey id for GPG, fingerprint for SSH) belongs to,
    /// for signature verification.
    pub async fn owner_of(
        &self,
        cx: &Cx,
        key_id: &str,
    ) -> Result<Option<(String, UserSigningKey)>> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<KeyRec>(
                &SIGNING_KEYS,
                &Scan {
                    key: Some(Key::K1(key_id.to_string())),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?;
        if let Some((r, _)) = rows.into_iter().next() {
            return Ok(r.key.map(|k| (r.owner, k)));
        }
        let all = tx
            .scan_msgs::<KeyRec>(
                &SIGNING_KEYS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        Ok(all.into_iter().find_map(|(r, _)| {
            let k = r.key?;
            k.subkey_ids
                .iter()
                .any(|s| s == key_id)
                .then_some((r.owner, k))
        }))
    }

    /// Verifies an armored signature over `payload` against the registered keys
    /// ([`super::sigverify`]): `VALID` names the key's owner as the signer; a key id nobody
    /// registered is `UNKNOWN_KEY`, a registered key past its expiry at `now_ms`
    /// `EXPIRED_KEY`.
    pub async fn verify_signature(
        &self,
        cx: &Cx,
        payload: &[u8],
        armored: &[u8],
        now_ms: u64,
    ) -> Result<SignatureVerdict> {
        use bee_proto::v1::{SecuritySignatureReason as R, SecuritySignatureScheme as S};
        let signature = String::from_utf8_lossy(armored).into_owned();
        let mut verdict = SignatureVerdict {
            reason: R::Malformed,
            scheme: S::Unspecified,
            key_id: String::new(),
            signer: None,
            signature,
            detail: String::new(),
        };
        let sig = match super::sigverify::parse(armored) {
            Ok(s) => s,
            Err(e) => {
                verdict.detail = e.to_string();
                return Ok(verdict);
            }
        };
        verdict.scheme = match sig.scheme {
            super::sigverify::Scheme::Gpg => S::Gpg,
            super::sigverify::Scheme::Ssh => S::Ssh,
        };
        verdict.key_id = sig.issuer.clone();
        let Some((owner, key)) = self.owner_of(cx, &sig.issuer).await? else {
            verdict.reason = R::UnknownKey;
            verdict.detail = format!("no account registered key {}", sig.issuer);
            return Ok(verdict);
        };
        match super::sigverify::verify(&sig, payload, &key.public_key) {
            Ok(()) => {
                let expired = key
                    .expires_at
                    .as_ref()
                    .is_some_and(|t| t.seconds.saturating_mul(1000) <= now_ms as i64);
                verdict.reason = if expired { R::ExpiredKey } else { R::Valid };
                verdict.signer = Some(owner);
            }
            Err(super::sigverify::Failure::Bad) => {
                verdict.reason = R::BadSignature;
                verdict.detail = "the signature does not verify".into();
            }
            Err(e) => {
                verdict.reason = R::Malformed;
                verdict.detail = e.to_string();
            }
        }
        Ok(verdict)
    }
}

/// The result of [`SigningKeys::verify_signature`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureVerdict {
    /// Why it verified or not.
    pub reason: bee_proto::v1::SecuritySignatureReason,
    /// The signature format (unspecified when it could not be parsed).
    pub scheme: bee_proto::v1::SecuritySignatureScheme,
    /// The key id the signature names.
    pub key_id: String,
    /// The account owning that key, when the signature verified.
    pub signer: Option<String>,
    /// The armored signature as stored.
    pub signature: String,
    /// A human-readable explanation when it did not verify.
    pub detail: String,
}

impl SignatureVerdict {
    /// Whether the signature verified under a current key.
    pub fn verified(&self) -> bool {
        self.reason == bee_proto::v1::SecuritySignatureReason::Valid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_with_the_encoder() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0xff, 0, 0x80]] {
            assert_eq!(base64_decode(&base64_nopad(data)).unwrap(), data);
        }
        assert!(base64_decode("a*b").is_none());
    }

    #[test]
    fn ssh_keys_need_a_matching_blob() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[7u8; 32]);
        let line = format!("ssh-ed25519 {} me@host", base64_nopad(&blob));
        let p = parse_ssh(&line).unwrap();
        assert!(p.key_id.starts_with("SHA256:"));
        assert!(!p.public_key.contains("me@host"));
        let wrong = format!("ssh-rsa {}", base64_nopad(&blob));
        assert!(parse_ssh(&wrong).is_err());
        assert!(parse_ssh("ssh-ed25519").is_err());
        assert!(parse_ssh("ssh-dss AAAA").is_err());
    }
}
