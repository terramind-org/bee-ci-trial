//! GitHub credentials: a personal access token, a GitHub App installation (JWT exchanged for
//! an installation token, refreshed before it expires), or discovery from the environment
//! (`GH_TOKEN`, `GITHUB_TOKEN`) and the `gh` CLI (`gh auth token`).
//!
//! An App installation gets its own, higher rate limit and webhook access, which is why
//! SPEC §17.1 prefers it; a token works but is slower.

use std::sync::Arc;

use base64::Engine as _;
use ring::signature::RsaKeyPair;

use crate::error::{Result, internal, invalid, unauthenticated};

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A GitHub credential.
#[derive(Clone)]
pub enum Credential {
    /// No credential (public data only, 60 requests an hour).
    Anonymous,
    /// A personal access token or any bearer token.
    Token(String),
    /// A GitHub App installation.
    App(AppCredential),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Anonymous => f.write_str("Anonymous"),
            Credential::Token(_) => f.write_str("Token(..)"),
            Credential::App(a) => write!(
                f,
                "App(app_id={}, installation={})",
                a.app_id, a.installation_id
            ),
        }
    }
}

/// A GitHub App's identity and the installation to act as.
#[derive(Clone)]
pub struct AppCredential {
    /// The App id (the JWT issuer).
    pub app_id: String,
    /// The installation id on the source org or user.
    pub installation_id: u64,
    key: Arc<RsaKeyPair>,
}

impl AppCredential {
    /// An App credential from its id, PEM private key (PKCS#1 `RSA PRIVATE KEY` as GitHub
    /// issues them, or PKCS#8 `PRIVATE KEY`) and installation id.
    pub fn new(app_id: impl Into<String>, pem: &str, installation_id: u64) -> Result<Self> {
        let (label, der) = pem_decode(pem)?;
        let key = match label.as_str() {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
            other => {
                return Err(invalid(
                    "invalid_app_key",
                    format!("unsupported PEM block `{other}`; expected an RSA private key"),
                ));
            }
        }
        .map_err(|e| invalid("invalid_app_key", format!("bad RSA private key: {e}")))?;
        Ok(AppCredential {
            app_id: app_id.into(),
            installation_id,
            key: Arc::new(key),
        })
    }

    /// The RSA public key (DER `RSAPublicKey`), for verifying JWTs in tests.
    pub fn public_key_der(&self) -> Vec<u8> {
        use ring::signature::KeyPair as _;
        self.key.public_key().as_ref().to_vec()
    }

    /// A JWT for the App, valid from a minute ago (clock skew) for nine minutes.
    pub fn jwt(&self, now_secs: u64) -> Result<String> {
        let header = B64URL.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": now_secs.saturating_sub(60),
            "exp": now_secs + 540,
            "iss": self.app_id,
        });
        let payload = B64URL.encode(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{payload}");
        let rng = ring::rand::SystemRandom::new();
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &rng,
                signing_input.as_bytes(),
                &mut sig,
            )
            .map_err(|_| internal("RSA signing failed"))?;
        Ok(format!("{signing_input}.{}", B64URL.encode(sig)))
    }
}

/// Verifies an RS256 JWT against a DER `RSAPublicKey` and returns its claims.
pub fn verify_jwt(token: &str, public_key_der: &[u8]) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    let sig = B64URL.decode(s).ok()?;
    let key = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        public_key_der,
    );
    key.verify(format!("{h}.{p}").as_bytes(), &sig).ok()?;
    serde_json::from_slice(&B64URL.decode(p).ok()?).ok()
}

/// Decodes the first PEM block: its label and bytes.
fn pem_decode(pem: &str) -> Result<(String, Vec<u8>)> {
    let begin = pem
        .find("-----BEGIN ")
        .ok_or_else(|| invalid("invalid_app_key", "no PEM block found"))?;
    let rest = &pem[begin + 11..];
    let label_end = rest
        .find("-----")
        .ok_or_else(|| invalid("invalid_app_key", "malformed PEM header"))?;
    let label = rest[..label_end].to_string();
    let body_start = label_end + 5;
    let end_marker = format!("-----END {label}-----");
    let body_end = rest
        .find(&end_marker)
        .ok_or_else(|| invalid("invalid_app_key", "PEM block has no end line"))?;
    let b64: String = rest[body_start..body_end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = B64
        .decode(b64)
        .map_err(|e| invalid("invalid_app_key", format!("PEM body is not base64: {e}")))?;
    Ok((label, der))
}

/// Where a discovered credential came from (shown to the user, never the secret).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// Passed explicitly.
    Explicit,
    /// `GH_TOKEN`.
    EnvGhToken,
    /// `GITHUB_TOKEN`.
    EnvGithubToken,
    /// `gh auth token`.
    GhCli,
}

/// Finds a token: `GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token` for `host`.
pub fn discover_token(host: &str) -> Result<(String, CredentialSource)> {
    for (var, src) in [
        ("GH_TOKEN", CredentialSource::EnvGhToken),
        ("GITHUB_TOKEN", CredentialSource::EnvGithubToken),
    ] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Ok((v, src));
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            unauthenticated(format!(
                "no GitHub credential: set GH_TOKEN, pass --token, or install and log in to the gh CLI ({e})"
            ))
        })?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || token.is_empty() {
        return Err(unauthenticated(format!(
            "no GitHub credential: `gh auth token` failed ({}); run `gh auth login` or pass --token",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok((token, CredentialSource::GhCli))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_errors_are_typed() {
        let e = AppCredential::new("1", "garbage", 1).err().unwrap();
        assert_eq!(e.reason, bee_core::Reason::InvalidArgument);
        let e = AppCredential::new(
            "1",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            1,
        )
        .err()
        .unwrap();
        assert!(e.message.contains("unsupported PEM block"));
    }

    #[test]
    fn debug_hides_secrets() {
        let c = Credential::Token("ghp_secret".into());
        assert!(!format!("{c:?}").contains("secret"));
    }
}
