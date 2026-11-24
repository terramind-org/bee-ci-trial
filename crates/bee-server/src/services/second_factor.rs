//! Second factors for human principals (`AuthService` second-factor RPCs,
//! `docs/spec/operations.md` §6): TOTP authenticators (RFC 6238: HMAC-SHA1, 6 digits, 30 s
//! steps, one step of clock skew either way, no code accepted twice) and single-use recovery
//! codes.
//!
//! - **Enrollment.** `BeginSecondFactorEnrollment(TOTP)` creates a pending enrollment with a
//!   fresh 160-bit secret and returns its `otpauth://` URI; `FinishSecondFactorEnrollment`
//!   activates it with a current code. `RECOVERY_CODES` returns ten codes once (stored as
//!   BLAKE3 digests) and activates them on finish; a new set replaces the old one.
//! - **Login challenge.** A human with an active TOTP factor who logs in (token paste) gets a
//!   `second_factor_challenge_id` instead of a token; `VerifySecondFactor` answers it with a
//!   TOTP or recovery code and returns a short-lived token (one hour at most, never past the
//!   presented token's expiry), recorded and audited. Five wrong answers burn a challenge.
//! - **Enforcement.** [`KvTwoFactor`] is the forge's two-factor directory, so orgs and
//!   enterprises that require 2FA refuse members without a TOTP factor at admission
//!   (`TWO_FACTOR_REQUIRED`), and `DeleteSecondFactor` refuses to remove the last TOTP factor
//!   of a member of such an org.
//!
//! Passkeys and security keys (WebAuthn) are not offered: no pure-Rust WebAuthn verifier fits
//! the workspace's dependency rules, so those kinds answer `INVALID_ARGUMENT`
//! (`unsupported_factor`).

use std::sync::Arc;

use async_trait::async_trait;
use bee_auth::{MintRequest, Scope};
use bee_proto::v1::{self, ErrorReason, SecondFactorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{Caller, PrincipalRecord, TokenRecord};
use crate::errors::{ApiError, ApiResult};
use crate::kv::{Kv, KvOp};
use crate::services::support::{get_json, put_json, scan_json};
use crate::state::{AppState, RequestMeta};
use crate::util::{timestamp_ms, timestamp_secs};

/// Active factors: `<principal>/<factor id>`.
pub const NS_FACTOR: &str = "second_factor";
/// Pending enrollments: `<enrollment id>`.
pub const NS_ENROLL: &str = "second_factor_enroll";
/// Login challenges: `<challenge id>`.
pub const NS_CHALLENGE: &str = "second_factor_challenge";

/// TOTP step in seconds.
pub const STEP_SECS: u64 = 30;
/// Code length.
pub const DIGITS: u32 = 6;
/// How long an enrollment or challenge stays open.
pub const PENDING_MS: u64 = 10 * 60 * 1000;
/// Wrong answers that burn a challenge.
pub const MAX_ATTEMPTS: u32 = 5;
/// Lifetime cap of a token issued by `VerifySecondFactor`.
pub const ELEVATED_SECS: u64 = 3600;

/// A stored factor.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FactorRec {
    /// Factor id.
    pub id: String,
    /// Owner.
    pub principal: String,
    /// `SecondFactorKind`.
    pub kind: i32,
    /// Nickname.
    pub nickname: String,
    /// TOTP secret (hex).
    #[serde(default)]
    pub secret_hex: String,
    /// Last TOTP step accepted (replay guard).
    #[serde(default)]
    pub last_step: u64,
    /// Recovery code digests (hex BLAKE3) not used yet.
    #[serde(default)]
    pub code_hashes: Vec<String>,
    /// Created.
    pub created_ms: u64,
    /// Last used.
    #[serde(default)]
    pub last_used_ms: u64,
}

/// A pending enrollment.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct EnrollRec {
    /// The factor it becomes.
    pub factor: FactorRec,
    /// Expiry.
    pub expires_ms: u64,
}

/// A login challenge.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ChallengeRec {
    /// Challenge id.
    pub id: String,
    /// Who must answer it.
    pub principal: String,
    /// Expiry of the token that started the login (Unix seconds, 0 = none).
    pub token_expires_at: u64,
    /// Expiry of the challenge.
    pub expires_ms: u64,
    /// Wrong answers so far.
    pub attempts: u32,
}

// ----- RFC 4226 / 6238 ----------------------------------------------------------------------

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = sha1collisiondetection::Sha1CD::default();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 20];
    // HMAC inputs are never collision attacks; a detection would only mean a broken input,
    // and the code then simply does not verify.
    if let Ok(d) = h.finalize_cd() {
        out.copy_from_slice(&d[..20]);
    }
    out
}

/// HMAC-SHA1 (RFC 2104).
pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = sha1(&[&ipad, msg]);
    sha1(&[&opad, &inner])
}

/// The HOTP code of `counter` (RFC 4226 dynamic truncation, `DIGITS` digits).
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mac = hmac_sha1(secret, &counter.to_be_bytes());
    let off = (mac[19] & 0x0f) as usize;
    let bin = (u32::from(mac[off] & 0x7f) << 24)
        | (u32::from(mac[off + 1]) << 16)
        | (u32::from(mac[off + 2]) << 8)
        | u32::from(mac[off + 3]);
    bin % 10u32.pow(DIGITS)
}

/// The TOTP code at `unix_secs`.
pub fn totp(secret: &[u8], unix_secs: u64) -> String {
    format!(
        "{:0width$}",
        hotp(secret, unix_secs / STEP_SECS),
        width = DIGITS as usize
    )
}

/// The step at which `code` is valid around `unix_secs` (±1 step), if any.
pub fn totp_step(secret: &[u8], code: &str, unix_secs: u64) -> Option<u64> {
    let code = code.trim().replace(' ', "");
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = unix_secs / STEP_SECS;
    [now, now.saturating_sub(1), now + 1]
        .into_iter()
        .find(|s| format!("{:0width$}", hotp(secret, *s), width = DIGITS as usize) == code)
}

/// RFC 4648 base32 without padding (the `secret` of an `otpauth://` URI).
pub fn base32(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for b in bytes {
        buf = (buf << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHA[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHA[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decodes [`base32`] (case-insensitive, padding and spaces ignored).
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in text.chars().filter(|c| *c != '=' && !c.is_whitespace()) {
        let v = match c.to_ascii_uppercase() {
            'A'..='Z' => c.to_ascii_uppercase() as u32 - 'A' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

fn code_hash(code: &str) -> String {
    let norm: String = code
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    bee_core::hex::encode(blake3::hash(format!("swarm-recovery-v1\0{norm}").as_bytes()).as_bytes())
}

fn new_recovery_codes() -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for _ in 0..10 {
        let b = crate::util::random_bytes(5)?;
        let h = bee_core::hex::encode(&b);
        out.push(format!("{}-{}", &h[..5], &h[5..]));
    }
    Ok(out)
}

// ----- store --------------------------------------------------------------------------------

/// Every active factor of a principal.
pub async fn factors_of(kv: &dyn Kv, principal: &str) -> ApiResult<Vec<FactorRec>> {
    Ok(
        scan_json::<FactorRec>(kv, NS_FACTOR, &format!("{principal}/"))
            .await?
            .into_iter()
            .map(|(_, f)| f)
            .collect(),
    )
}

/// Whether a principal has an active TOTP factor.
pub async fn has_totp(kv: &dyn Kv, principal: &str) -> ApiResult<bool> {
    Ok(factors_of(kv, principal)
        .await?
        .iter()
        .any(|f| f.kind == SecondFactorKind::Totp as i32))
}

fn factor_key(f: &FactorRec) -> String {
    format!("{}/{}", f.principal, f.id)
}

fn factor_proto(f: &FactorRec) -> v1::SecondFactor {
    v1::SecondFactor {
        id: f.id.clone(),
        kind: f.kind,
        nickname: f.nickname.clone(),
        created_at: Some(timestamp_ms(f.created_ms)),
        last_used_at: (f.last_used_ms > 0).then(|| timestamp_ms(f.last_used_ms)),
    }
}

/// The forge's two-factor directory over the server store.
pub struct KvTwoFactor {
    kv: Arc<dyn Kv>,
}

impl KvTwoFactor {
    /// A directory over `kv`.
    pub fn new(kv: Arc<dyn Kv>) -> KvTwoFactor {
        KvTwoFactor { kv }
    }
}

#[async_trait]
impl bee_forge::enterprise::network::TwoFactorDirectory for KvTwoFactor {
    async fn has_two_factor(&self, principal_id: &str) -> bool {
        has_totp(self.kv.as_ref(), principal_id)
            .await
            .unwrap_or(false)
    }

    async fn has_secure_two_factor(&self, _principal_id: &str) -> bool {
        // Passkeys and security keys are not offered (see the module docs).
        false
    }
}

fn humans_only(caller: &Caller) -> ApiResult<()> {
    if caller.token.principal.kind != bee_policy::PrincipalKind::Human
        || !caller.token.on_behalf_of.is_empty()
    {
        return Err(ApiError::denied(
            "second factors belong to human principals acting for themselves",
        ));
    }
    Ok(())
}

fn forget_admissions(state: &AppState) {
    if let Some(w) = state.hub.get() {
        w.forget_admissions();
    }
}

/// `BeginSecondFactorEnrollment`.
pub async fn begin(
    state: &AppState,
    caller: &Caller,
    req: v1::BeginSecondFactorEnrollmentRequest,
    meta: &RequestMeta,
) -> ApiResult<v1::BeginSecondFactorEnrollmentResponse> {
    humans_only(caller)?;
    let kind = SecondFactorKind::try_from(req.kind).unwrap_or(SecondFactorKind::Unspecified);
    let now = state.now_ms();
    let id = crate::util::new_ulid(now)?.to_text();
    let mut factor = FactorRec {
        id: crate::util::new_ulid(now)?.to_text(),
        principal: caller.id().to_string(),
        kind: kind as i32,
        nickname: req.nickname.trim().chars().take(100).collect(),
        created_ms: now,
        ..FactorRec::default()
    };
    let mut resp = v1::BeginSecondFactorEnrollmentResponse {
        enrollment_id: id.clone(),
        ..Default::default()
    };
    match kind {
        SecondFactorKind::Totp => {
            let secret = crate::util::random_bytes(20)?;
            factor.secret_hex = bee_core::hex::encode(&secret);
            let label = caller
                .record
                .as_ref()
                .map(|r| r.display.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| caller.id().to_string());
            resp.totp_uri = format!(
                "otpauth://totp/Beehive:{}?secret={}&issuer=Beehive&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
                uri_component(&label),
                base32(&secret)
            );
        }
        SecondFactorKind::RecoveryCodes => {
            let codes = new_recovery_codes()?;
            factor.code_hashes = codes.iter().map(|c| code_hash(c)).collect();
            resp.recovery_codes = codes;
        }
        SecondFactorKind::Passkey | SecondFactorKind::SecurityKey => {
            return Err(ApiError::invalid(
                "unsupported_factor",
                "passkeys and security keys are not offered on this server; enroll TOTP",
            ));
        }
        SecondFactorKind::Unspecified => {
            return Err(ApiError::invalid("missing_field", "kind is required"));
        }
    }
    put_json(
        state.kv.as_ref(),
        NS_ENROLL,
        &id,
        &EnrollRec {
            factor,
            expires_ms: now + PENDING_MS,
        },
    )
    .await?;
    state
        .audit(
            caller,
            meta,
            None,
            &id,
            "second_factor.begin",
            json!({ "kind": kind.as_str_name() }),
        )
        .await?;
    Ok(resp)
}

fn uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `FinishSecondFactorEnrollment`.
pub async fn finish(
    state: &AppState,
    caller: &Caller,
    req: v1::FinishSecondFactorEnrollmentRequest,
    meta: &RequestMeta,
) -> ApiResult<v1::SecondFactor> {
    humans_only(caller)?;
    if req.enrollment_id.is_empty() {
        return Err(ApiError::invalid(
            "missing_field",
            "enrollment_id is required",
        ));
    }
    let raw = state
        .kv
        .get(NS_ENROLL, req.enrollment_id.as_bytes())
        .await?;
    let e: EnrollRec = raw
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok())
        .filter(|e: &EnrollRec| e.factor.principal == caller.id())
        .ok_or_else(|| ApiError::not_found("no such enrollment"))?;
    if state.now_ms() > e.expires_ms {
        state
            .kv
            .apply(vec![KvOp::delete(
                NS_ENROLL,
                req.enrollment_id.as_bytes().to_vec(),
            )])
            .await?;
        return Err(ApiError::new(
            ErrorReason::InvalidState,
            "the enrollment expired; begin again",
        ));
    }
    let mut factor = e.factor;
    let mut ops = vec![
        KvOp::expect(
            NS_ENROLL,
            req.enrollment_id.as_bytes().to_vec(),
            raw.clone(),
        ),
        KvOp::delete(NS_ENROLL, req.enrollment_id.as_bytes().to_vec()),
    ];
    if factor.kind == SecondFactorKind::Totp as i32 {
        if !req.webauthn_attestation_json.is_empty() {
            return Err(ApiError::invalid(
                "unsupported_factor",
                "WebAuthn attestations are not accepted here",
            ));
        }
        let secret = bee_core::hex::decode(&factor.secret_hex).unwrap_or_default();
        let step = totp_step(&secret, &req.totp_code, state.now_ms() / 1000).ok_or_else(|| {
            ApiError::invalid(
                "bad_code",
                "the TOTP code does not match; check the device clock",
            )
        })?;
        factor.last_step = step;
    } else {
        // A new set of recovery codes replaces the old one.
        for old in factors_of(state.kv.as_ref(), caller.id()).await? {
            if old.kind == SecondFactorKind::RecoveryCodes as i32 {
                ops.push(KvOp::delete(NS_FACTOR, factor_key(&old).into_bytes()));
            }
        }
    }
    ops.push(KvOp::put(
        NS_FACTOR,
        factor_key(&factor).into_bytes(),
        serde_json::to_vec(&factor).unwrap_or_default(),
    ));
    if !state.kv.apply(ops).await? {
        return Err(ApiError::not_found("no such enrollment"));
    }
    forget_admissions(state);
    state
        .audit(
            caller,
            meta,
            None,
            &factor.id,
            "second_factor.enroll",
            json!({ "kind": SecondFactorKind::try_from(factor.kind).map(|k| k.as_str_name()).unwrap_or("") }),
        )
        .await?;
    Ok(factor_proto(&factor))
}

/// `ListSecondFactors`.
pub async fn list(state: &AppState, caller: &Caller) -> ApiResult<v1::ListSecondFactorsResponse> {
    let mut factors: Vec<v1::SecondFactor> = factors_of(state.kv.as_ref(), caller.id())
        .await?
        .iter()
        .map(factor_proto)
        .collect();
    factors.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(v1::ListSecondFactorsResponse { factors })
}

/// Orgs of the principal that require two-factor authentication.
async fn requiring_orgs(state: &AppState, principal: &str) -> Vec<String> {
    use bee_forge::enterprise::apps::DomainCheck;
    let Some(w) = state.hub.get() else {
        return Vec::new();
    };
    let cx = bee_forge::Cx::system();
    let mut out = Vec::new();
    let mut after = String::new();
    loop {
        let Ok((orgs, next)) = w.forge.orgs.orgs_of(&cx, principal, &after, 500).await else {
            return out;
        };
        for o in orgs {
            if w.enterprise
                .network
                .two_factor_required(&cx, &o.id)
                .await
                .unwrap_or(false)
            {
                out.push(o.name);
            }
        }
        if next.is_empty() {
            return out;
        }
        after = next;
    }
}

/// `DeleteSecondFactor`.
pub async fn delete(
    state: &AppState,
    caller: &Caller,
    req: v1::DeleteSecondFactorRequest,
    meta: &RequestMeta,
) -> ApiResult<v1::DeleteSecondFactorResponse> {
    humans_only(caller)?;
    if req.id.is_empty() {
        return Err(ApiError::invalid("missing_field", "id is required"));
    }
    let all = factors_of(state.kv.as_ref(), caller.id()).await?;
    let f = all
        .iter()
        .find(|f| f.id == req.id)
        .ok_or_else(|| ApiError::not_found(format!("second factor {}", req.id)))?;
    let totps = all
        .iter()
        .filter(|x| x.kind == SecondFactorKind::Totp as i32)
        .count();
    if f.kind == SecondFactorKind::Totp as i32 && totps == 1 {
        let orgs = requiring_orgs(state, caller.id()).await;
        if !orgs.is_empty() {
            return Err(ApiError::new(
                ErrorReason::TwoFactorRequired,
                format!(
                    "{} require two-factor authentication; enroll another authenticator first",
                    orgs.join(", ")
                ),
            ));
        }
    }
    state
        .kv
        .apply(vec![KvOp::delete(NS_FACTOR, factor_key(f).into_bytes())])
        .await?;
    forget_admissions(state);
    state
        .audit(caller, meta, None, &f.id, "second_factor.delete", json!({}))
        .await?;
    Ok(v1::DeleteSecondFactorResponse {})
}

/// Opens a login challenge for `principal` (a human with an active TOTP factor).
pub async fn challenge(
    state: &AppState,
    principal: &str,
    token_expires_at: u64,
) -> ApiResult<v1::LoginResponse> {
    let id = crate::util::random_hex(16);
    let now = state.now_ms();
    put_json(
        state.kv.as_ref(),
        NS_CHALLENGE,
        &id,
        &ChallengeRec {
            id: id.clone(),
            principal: principal.to_string(),
            token_expires_at,
            expires_ms: now + PENDING_MS,
            attempts: 0,
        },
    )
    .await?;
    let mut kinds = vec![SecondFactorKind::Totp as i32];
    if factors_of(state.kv.as_ref(), principal)
        .await?
        .iter()
        .any(|f| f.kind == SecondFactorKind::RecoveryCodes as i32 && !f.code_hashes.is_empty())
    {
        kinds.push(SecondFactorKind::RecoveryCodes as i32);
    }
    Ok(v1::LoginResponse {
        second_factor_challenge_id: id,
        second_factor_kinds: kinds,
        expires_at: Some(timestamp_ms(now + PENDING_MS)),
        ..Default::default()
    })
}

/// Checks a TOTP or recovery answer against the principal's factors, consuming what it uses.
async fn check_answer(
    state: &AppState,
    principal: &str,
    totp_code: &str,
    recovery_code: &str,
) -> ApiResult<Option<FactorRec>> {
    let now = state.now_ms();
    for f in factors_of(state.kv.as_ref(), principal).await? {
        let key = factor_key(&f);
        let before = serde_json::to_vec(&f).unwrap_or_default();
        let mut used = f.clone();
        let ok = if !totp_code.is_empty() && f.kind == SecondFactorKind::Totp as i32 {
            let secret = bee_core::hex::decode(&f.secret_hex).unwrap_or_default();
            match totp_step(&secret, totp_code, now / 1000) {
                Some(step) if step > f.last_step => {
                    used.last_step = step;
                    true
                }
                _ => false,
            }
        } else if !recovery_code.is_empty() && f.kind == SecondFactorKind::RecoveryCodes as i32 {
            let h = code_hash(recovery_code);
            match f.code_hashes.iter().position(|c| *c == h) {
                Some(i) => {
                    used.code_hashes.remove(i);
                    true
                }
                None => false,
            }
        } else {
            false
        };
        if ok {
            used.last_used_ms = now;
            // Compare-and-set: a code is accepted once even under concurrent answers.
            let applied = state
                .kv
                .apply(vec![
                    KvOp::expect(NS_FACTOR, key.clone().into_bytes(), Some(before)),
                    KvOp::put(
                        NS_FACTOR,
                        key.into_bytes(),
                        serde_json::to_vec(&used).unwrap_or_default(),
                    ),
                ])
                .await?;
            return Ok(applied.then_some(used));
        }
    }
    Ok(None)
}

/// `VerifySecondFactor`: answers a login challenge and returns a short-lived token.
pub async fn verify(
    state: &AppState,
    req: v1::VerifySecondFactorRequest,
    meta: &RequestMeta,
) -> ApiResult<v1::VerifySecondFactorResponse> {
    if req.challenge_id.is_empty() {
        return Err(ApiError::invalid(
            "missing_field",
            "challenge_id is required",
        ));
    }
    if !req.webauthn_assertion_json.is_empty() {
        return Err(ApiError::invalid(
            "unsupported_factor",
            "WebAuthn assertions are not accepted here",
        ));
    }
    if req.totp_code.is_empty() && req.recovery_code.is_empty() {
        return Err(ApiError::invalid(
            "missing_field",
            "answer with totp_code or recovery_code",
        ));
    }
    let mut ch: ChallengeRec = get_json(state.kv.as_ref(), NS_CHALLENGE, &req.challenge_id)
        .await?
        .ok_or_else(|| ApiError::unauthenticated("unknown or expired challenge"))?;
    let now = state.now_ms();
    if now > ch.expires_ms || ch.attempts >= MAX_ATTEMPTS {
        state
            .kv
            .apply(vec![KvOp::delete(NS_CHALLENGE, ch.id.as_bytes().to_vec())])
            .await?;
        return Err(ApiError::unauthenticated("unknown or expired challenge"));
    }
    let Some(factor) =
        check_answer(state, &ch.principal, &req.totp_code, &req.recovery_code).await?
    else {
        ch.attempts += 1;
        put_json(state.kv.as_ref(), NS_CHALLENGE, &ch.id, &ch).await?;
        return Err(ApiError::unauthenticated(
            "the second factor does not match",
        ));
    };
    state
        .kv
        .apply(vec![KvOp::delete(NS_CHALLENGE, ch.id.as_bytes().to_vec())])
        .await?;
    let principal: PrincipalRecord = state
        .principal(&ch.principal)
        .await?
        .filter(|p| !p.deactivated)
        .ok_or_else(|| ApiError::unauthenticated("principal is gone or deactivated"))?;
    let now_s = now / 1000;
    let mut exp = now_s + ELEVATED_SECS;
    if ch.token_expires_at > 0 {
        exp = exp.min(ch.token_expires_at);
    }
    let token = state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &MintRequest {
                principal: principal.to_auth(),
                on_behalf_of: Vec::new(),
                scope: Scope::new()
                    .expires_at(exp)
                    .session(format!("2fa:{}", factor.id)),
            },
            now_s,
        )
        .map_err(ApiError::from)?;
    crate::services::auth::record_token(
        state.kv.as_ref(),
        &TokenRecord {
            token_id: token.id.0.clone(),
            principal: principal.id.clone(),
            parent: String::new(),
            on_behalf_of: Vec::new(),
            created_ms: now,
            expires_at: exp,
            description: "second factor login".into(),
            scope: None,
        },
    )
    .await?;
    state
        .audit
        .record(crate::audit::AuditEntry {
            at_ms: now,
            chain: vec![principal.id.clone()],
            action: meta.method.clone(),
            target: token.id.0.clone(),
            decision: "second_factor".into(),
            detail: json!({ "factor": factor.id }),
            request_id: meta.request_id.clone(),
            ..Default::default()
        })
        .await?;
    Ok(v1::VerifySecondFactorResponse {
        token: token.text,
        expires_at: Some(timestamp_secs(exp)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotp_matches_rfc4226_vectors() {
        // RFC 4226 appendix D: secret "12345678901234567890".
        let s = b"12345678901234567890";
        let want = [
            755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(hotp(s, i as u64), *w, "counter {i}");
        }
    }

    #[test]
    fn totp_matches_rfc6238_sha1_vectors() {
        // RFC 6238 appendix B (SHA-1, 8 digits there; the low 6 digits here).
        let s = b"12345678901234567890";
        for (t, eight) in [
            (59u64, "94287082"),
            (1_111_111_109, "07081804"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
        ] {
            assert_eq!(totp(s, t), eight[2..]);
            assert!(totp_step(s, &eight[2..], t).is_some());
        }
        assert!(totp_step(s, "000000", 59).is_none() || totp(s, 59) == "000000");
        assert!(totp_step(s, "12345", 59).is_none());
    }

    #[test]
    fn base32_round_trips_and_matches_rfc4648() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        let r = [7u8; 20];
        assert_eq!(base32_decode(&base32(&r)).unwrap(), r);
    }

    #[test]
    fn recovery_codes_normalize() {
        assert_eq!(code_hash("ABCDE-12345"), code_hash(" abcde12345 "));
        assert_ne!(code_hash("abcde-12345"), code_hash("abcde-12346"));
    }
}
