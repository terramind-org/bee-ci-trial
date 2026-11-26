//! Beehive as an OpenID Connect provider (F49-012): the OAuth code and device flows of
//! `/login/oauth/*` return an `id_token` when the grant includes the `openid` scope, signed
//! with an ES256 key and published as a JWKS. The key is `auth.oidc_key_pkcs8_hex` when set
//! (every replica of a scaled-out deployment must hold the same one), else the one this server
//! keeps at `<data_dir>/oidc-signing-key.p8` (generated on first use, mode 0600).
//!
//! | Route | |
//! |---|---|
//! | `GET /.well-known/openid-configuration` | discovery: issuer (the public URL), endpoints, ES256, PKCE S256 |
//! | `GET /.well-known/jwks.json`, `GET /login/oauth/jwks` | the signing key as a JWK set |
//! | `GET`/`POST /login/oauth/userinfo` | `sub`, `name`, `preferred_username` for a bearer access token |
//!
//! The id_token carries `iss`, `sub` (the human's principal id; for an app's user-to-server
//! token, the user it acts for), `aud` (the client id), `iat`, `exp`
//! (`login.id_token_ttl_secs`), `auth_time`, the authorization request's `nonce`, and with
//! the `profile` scope `name` and `preferred_username`.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use bee_policy::PrincipalKind;
use bee_proto::v1::ErrorReason;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

use crate::errors::{ApiError, ApiResult};
use crate::state::AppState;

/// The id_token signing key.
pub struct OidcKey {
    pair: EcdsaKeyPair,
    /// Key id: the first 16 hex digits of SHA-256 over the public point.
    pub kid: String,
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn key_error(path: &Path, what: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ErrorReason::Internal,
        format!("OIDC signing key {}: {what}", path.display()),
    )
}

impl OidcKey {
    /// The key at `path`, generated (PKCS#8, mode 0600) when absent.
    pub fn load_or_create(path: &Path) -> ApiResult<OidcKey> {
        let pkcs8 = read_or_create_pkcs8(path)?;
        OidcKey::from_pkcs8(&pkcs8).map_err(|e| key_error(path, e))
    }

    /// The key in a PKCS#8 document (P-256).
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<OidcKey, &'static str> {
        let pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| "not a P-256 PKCS#8 key")?;
        let digest = ring::digest::digest(&ring::digest::SHA256, pair.public_key().as_ref());
        let kid = bee_core::hex::encode(&digest.as_ref()[..8]);
        Ok(OidcKey { pair, kid })
    }

    /// The public key as a JWK.
    pub fn jwk(&self) -> Value {
        let point = self.pair.public_key().as_ref();
        json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": self.kid,
            "x": b64(&point[1..33]), "y": b64(&point[33..65]),
        })
    }

    /// A compact ES256 JWS over `claims`.
    pub fn sign(&self, claims: &Value) -> ApiResult<String> {
        let header = json!({ "alg": "ES256", "typ": "JWT", "kid": self.kid });
        let input = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(claims.to_string().as_bytes())
        );
        let sig = self
            .pair
            .sign(&SystemRandom::new(), input.as_bytes())
            .map_err(|_| ApiError::new(ErrorReason::Internal, "id_token signing failed"))?;
        Ok(format!("{input}.{}", b64(sig.as_ref())))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

/// The PKCS#8 file at `path`, generated (mode 0600) when absent.
fn read_or_create_pkcs8(path: &Path) -> ApiResult<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let doc = EcdsaKeyPair::generate_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &SystemRandom::new(),
            )
            .map_err(|_| key_error(path, "cannot generate"))?;
            write_private(path, doc.as_ref()).map_err(|e| key_error(path, e))?;
            Ok(doc.as_ref().to_vec())
        }
        Err(e) => Err(key_error(path, e)),
    }
}

/// Where the server keeps its ES256 signing key (PKCS#8) when `auth.oidc_key_pkcs8_hex` is
/// not set.
pub fn key_path(state: &AppState) -> std::path::PathBuf {
    key_path_for(&state.cfg)
}

fn key_path_for(cfg: &crate::config::Config) -> std::path::PathBuf {
    cfg.server.data_dir.join("oidc-signing-key.p8")
}

/// The server's ES256 signing key as a PKCS#8 document: `auth.oidc_key_pkcs8_hex` when set,
/// else the file at [`key_path`] (generated when absent). Every user of the key (the OpenID
/// provider, Actions OIDC tokens and attestations) reads it here, so they all sign with the
/// key the JWKS publishes.
pub fn key_pkcs8(state: &AppState) -> ApiResult<Vec<u8>> {
    key_pkcs8_for(&state.cfg)
}

fn key_pkcs8_for(cfg: &crate::config::Config) -> ApiResult<Vec<u8>> {
    let hex = cfg.auth.oidc_key_pkcs8_hex.trim();
    if hex.is_empty() {
        return read_or_create_pkcs8(&key_path_for(cfg));
    }
    bee_core::hex::decode(hex)
        .ok_or_else(|| ApiError::new(ErrorReason::Internal, "auth.oidc_key_pkcs8_hex is not hex"))
}

/// The server's id_token key, loaded on first use.
pub fn key(state: &AppState) -> ApiResult<Arc<OidcKey>> {
    if let Some(k) = state.oidc_key.get() {
        return Ok(k.clone());
    }
    let k = Arc::new(
        OidcKey::from_pkcs8(&key_pkcs8(state)?)
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("OIDC signing key: {e}")))?,
    );
    Ok(state.oidc_key.get_or_init(|| k).clone())
}

/// The issuer: the server's public URL.
pub fn issuer(state: &AppState) -> String {
    state.public_url().trim_end_matches('/').to_string()
}

/// The discovery document.
pub fn discovery(state: &AppState) -> Value {
    let base = issuer(state);
    json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/login/oauth/authorize"),
        "token_endpoint": format!("{base}/login/oauth/access_token"),
        "userinfo_endpoint": format!("{base}/login/oauth/userinfo"),
        "jwks_uri": format!("{base}/.well-known/jwks.json"),
        "device_authorization_endpoint": format!("{base}/login/device/code"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "scopes_supported": ["openid", "profile", "read:user", "read:org", "repo"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
        "code_challenge_methods_supported": ["S256"],
        "claims_supported": ["iss", "sub", "aud", "iat", "exp", "auth_time", "nonce", "name", "preferred_username"],
    })
}

/// The JWK set.
pub fn jwks(state: &AppState) -> ApiResult<Value> {
    Ok(json!({ "keys": [key(state)?.jwk()] }))
}

/// The human an access token speaks for and its display name: the principal itself when it
/// is a human, else the delegation root (an app acting for a user).
async fn subject_of(state: &AppState, access_token: &str) -> ApiResult<(String, String)> {
    let (v, record) = state.verify_token(access_token).await?;
    if v.principal.kind == PrincipalKind::Human {
        let display = record.map(|r| r.display).unwrap_or_default();
        return Ok((v.principal.id.clone(), display));
    }
    let root = v.delegation_root().id.clone();
    let display = state
        .principal(&root)
        .await?
        .map(|r| r.display)
        .unwrap_or_default();
    Ok((root, display))
}

/// The id_token for a token response, when its scopes include `openid`.
pub async fn id_token(
    state: &AppState,
    access_token: &str,
    client_id: &str,
    scopes: &[String],
    nonce: &str,
) -> ApiResult<Option<String>> {
    if !scopes.iter().any(|s| s == "openid") {
        return Ok(None);
    }
    let (sub, display) = subject_of(state, access_token).await?;
    let now = state.now_ms() / 1000;
    let mut claims = json!({
        "iss": issuer(state),
        "sub": sub,
        "aud": client_id,
        "iat": now,
        "auth_time": now,
        "exp": now + state.cfg.login.id_token_ttl_secs,
    });
    if !nonce.is_empty() {
        claims["nonce"] = json!(nonce);
    }
    if scopes.iter().any(|s| s == "profile") {
        claims["name"] = json!(display);
        claims["preferred_username"] = json!(display);
    }
    key(state)?.sign(&claims).map(Some)
}

/// The userinfo response for a bearer access token.
pub async fn userinfo(state: &AppState, access_token: &str) -> ApiResult<Value> {
    let (sub, display) = subject_of(state, access_token).await?;
    Ok(json!({ "sub": sub, "name": display, "preferred_username": display }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_tokens_verify_with_the_published_jwk() {
        let dir = tempfile_dir();
        let path = dir.join("k.p8");
        let k = OidcKey::load_or_create(&path).unwrap();
        let again = OidcKey::load_or_create(&path).unwrap();
        assert_eq!(k.kid, again.kid, "the key persists");
        let jwt = k.sign(&json!({"sub": "x"})).unwrap();
        let (signed, sig) = jwt.rsplit_once('.').unwrap();
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig)
            .unwrap();
        assert!(crate::login::verify_jws(
            "ES256",
            &k.jwk(),
            signed.as_bytes(),
            &sig
        ));
        assert!(!crate::login::verify_jws("ES256", &k.jwk(), b"other", &sig));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replicas_with_the_configured_key_publish_the_same_one() {
        // Two replicas: one store, their own data directories.
        let (d1, d2) = (tempfile_dir(), tempfile_dir());
        let mut a = crate::config::Config::default();
        a.server.data_dir = d1.clone();
        let mut b = a.clone();
        b.server.data_dir = d2.clone();
        let kid = |cfg: &crate::config::Config| {
            OidcKey::from_pkcs8(&key_pkcs8_for(cfg).unwrap())
                .unwrap()
                .kid
        };
        // Without the setting each generates its own key: the bug the setting fixes.
        assert_ne!(kid(&a), kid(&b));
        assert_eq!(kid(&a), kid(&a), "the generated key persists");
        let doc =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .unwrap();
        let hex = bee_core::hex::encode(doc.as_ref());
        a.auth.oidc_key_pkcs8_hex = hex.clone();
        b.auth.oidc_key_pkcs8_hex = hex;
        let expected = OidcKey::from_pkcs8(doc.as_ref()).unwrap().kid;
        assert_eq!(kid(&a), expected);
        assert_eq!(kid(&b), expected);
        // A configured key that is not a P-256 PKCS#8 document is refused, never replaced.
        a.auth.oidc_key_pkcs8_hex = "00ff".into();
        assert!(OidcKey::from_pkcs8(&key_pkcs8_for(&a).unwrap()).is_err());
        a.auth.oidc_key_pkcs8_hex = "zz".into();
        assert!(key_pkcs8_for(&a).is_err());
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bee-oidc-{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
