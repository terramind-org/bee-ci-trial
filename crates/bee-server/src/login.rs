//! Human login (SPEC §9.1): GitHub device flow, GitHub token exchange and id_tokens from an
//! external OIDC identity provider, each mapped to a human principal by its stable external
//! id (`github:<numeric id>`, `oidc:<issuer>#<sub>`), plus GitHub team sync.
//!
//! - **Device flow** (`Login{github_start}`, then `Login{github_device_code}`): the server
//!   asks GitHub for a device code with `login.github_client_id` and polls GitHub's token
//!   endpoint for the client; an unfinished flow answers with the device code and interval
//!   again (no token), `slow_down` adds 5 s.
//! - **Token exchange** (`Login{github_token}`, and the end of the device flow): `GET /user`
//!   proves the account; with `login.github_team_sync` the principal's external groups are
//!   replaced by its GitHub teams (`<org login>/<team slug>`, `GET /user/teams`), which
//!   policy and CODEOWNERS match as `@org/team`.
//! - **OIDC** (`Login{oidc_id_token}` with a JWT and `login.oidc_issuer` set): the id_token's
//!   signature is checked against the issuer's JWKS (RS256/384/512, ES256/384, EdDSA), then
//!   `iss`, `aud`, `exp`, `nbf` and `iat`; the groups claim becomes external groups (a name
//!   without `/` becomes `idp/<name>`).
//! - **GitHub down** (F49-031): a GitHub that cannot be reached or answers 5xx fails new
//!   GitHub logins with `UNAVAILABLE` (retryable). Tokens already issued are verified
//!   locally and keep working, and the groups synced at the last login stay in force.
//!
//! A login mints a session token for `auth.token_lifetime_secs` (or answers a second-factor
//! challenge first when the human enrolled TOTP) and is audited (`login.github`,
//! `login.oidc`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bee_auth::{MintRequest, Scope};
use bee_github::auth::Credential;
use bee_github::clock::SystemClock;
use bee_github::{Client, ClientConfig};
use bee_proto::v1::{self, ErrorReason};
use serde_json::{Value, json};

use crate::auth::{NS_PRINCIPAL, PrincipalRecord, TokenRecord};
use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RequestMeta};
use crate::util::timestamp_secs;

/// Server-table namespace mapping an external id to its principal id.
pub const NS_EXTERNAL_ID: &str = "principal_external";

fn nonempty(v: &str, default: &str) -> String {
    let v = v.trim().trim_end_matches('/');
    if v.is_empty() {
        default.to_string()
    } else {
        v.to_string()
    }
}

/// The GitHub client settings for login: one retry and a short timeout, so a GitHub outage
/// fails a login quickly instead of hanging it.
pub fn github_config(state: &AppState) -> ClientConfig {
    let l = &state.cfg.login;
    let api = nonempty(&l.github_api_url, "https://api.github.com");
    ClientConfig {
        graphql_url: format!("{api}/graphql"),
        api_url: api,
        web_url: nonempty(&l.github_url, "https://github.com"),
        max_wait_secs: 0,
        max_retries: 1,
        timeout: Duration::from_secs(10),
        per_page: 100,
    }
}

/// `UNAVAILABLE` (retryable) for a GitHub that cannot answer; other errors keep their reason.
fn github_error(e: bee_core::BeeError) -> ApiError {
    use bee_core::Reason;
    match e.reason {
        Reason::Unavailable | Reason::DeadlineExceeded | Reason::Internal => {
            let mut err = ApiError::new(
                ErrorReason::Unavailable,
                format!(
                    "GitHub is unavailable ({}): new GitHub logins fail until it recovers; \
                     tokens already issued keep working",
                    e.message
                ),
            );
            err.detail.retryable = true;
            err.detail.subcode = "github_unavailable".into();
            err
        }
        Reason::Unauthenticated | Reason::PermissionDenied => {
            ApiError::unauthenticated(format!("GitHub refused the token: {}", e.message))
        }
        _ => ApiError::from(e),
    }
}

/// A raw GitHub response that is a server error is GitHub being down.
fn github_ok(r: bee_github::client::Response) -> ApiResult<bee_github::client::Response> {
    if r.status >= 500 {
        return Err(github_error(bee_core::BeeError::new(
            bee_core::Reason::Unavailable,
            format!("HTTP {}", r.status),
        )));
    }
    Ok(r)
}

async fn put_principal(state: &AppState, rec: &PrincipalRecord) -> ApiResult<()> {
    state
        .kv
        .put(
            NS_PRINCIPAL,
            rec.id.as_bytes(),
            serde_json::to_vec(rec).unwrap_or_default(),
        )
        .await?;
    Ok(())
}

/// The human principal behind `external_id`, created on first login. `groups`, when given,
/// replace its external groups (team sync); `display` updates its label.
pub async fn principal_for(
    state: &AppState,
    external_id: &str,
    display: &str,
    groups: Option<Vec<String>>,
) -> ApiResult<PrincipalRecord> {
    let existing = match state.kv.get(NS_EXTERNAL_ID, external_id.as_bytes()).await? {
        Some(id) => state.principal(&String::from_utf8_lossy(&id)).await?,
        None => None,
    };
    let mut rec = match existing {
        Some(r) => r,
        None => {
            let id = crate::util::new_ulid(state.now_ms())?.to_text();
            let org = if state.cfg.login.default_org.is_empty() {
                String::new()
            } else {
                state
                    .meta
                    .org_by_name(&state.cfg.login.default_org)
                    .await?
                    .map(|o| o.id.0.to_text())
                    .unwrap_or_default()
            };
            PrincipalRecord {
                id,
                kind: "human".into(),
                display: display.to_string(),
                org,
                owner: String::new(),
                runtime: String::new(),
                purpose: String::new(),
                external_ids: vec![external_id.to_string()],
                groups: Vec::new(),
                deactivated: false,
            }
        }
    };
    if rec.deactivated {
        return Err(ApiError::unauthenticated(format!(
            "principal {} is deactivated",
            rec.id
        )));
    }
    if !display.is_empty() {
        rec.display = display.to_string();
    }
    if !rec.external_ids.iter().any(|e| e == external_id) {
        rec.external_ids.push(external_id.to_string());
    }
    if let Some(mut g) = groups {
        g.sort();
        g.dedup();
        rec.groups = g;
    }
    put_principal(state, &rec).await?;
    state
        .kv
        .put(
            NS_EXTERNAL_ID,
            external_id.as_bytes(),
            rec.id.as_bytes().to_vec(),
        )
        .await?;
    Ok(rec)
}

/// A session token for a human who just proved who they are (or a second-factor challenge).
pub async fn session(
    state: &AppState,
    rec: &PrincipalRecord,
    meta: &RequestMeta,
    how: &str,
) -> ApiResult<v1::LoginResponse> {
    let now = state.now_ms() / 1000;
    let exp = now + state.cfg.auth.token_lifetime_secs;
    if crate::services::second_factor::has_totp(state.kv.as_ref(), &rec.id).await? {
        return crate::services::second_factor::challenge(state, &rec.id, exp).await;
    }
    let token = state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &MintRequest {
                principal: rec.to_auth(),
                on_behalf_of: Vec::new(),
                scope: Scope::new().expires_at(exp),
            },
            now,
        )
        .map_err(ApiError::from)?;
    crate::services::auth::record_token(
        state.kv.as_ref(),
        &TokenRecord {
            token_id: token.id.0.clone(),
            principal: rec.id.clone(),
            parent: String::new(),
            on_behalf_of: Vec::new(),
            created_ms: state.now_ms(),
            expires_at: exp,
            description: format!("{how} login"),
            scope: None,
        },
    )
    .await?;
    state
        .audit
        .record(crate::audit::AuditEntry {
            at_ms: state.now_ms(),
            chain: vec![rec.id.clone()],
            action: format!("login.{how}"),
            target: token.id.0.clone(),
            decision: how.to_string(),
            request_id: meta.request_id.clone(),
            ..Default::default()
        })
        .await?;
    Ok(v1::LoginResponse {
        token: token.text,
        expires_at: Some(timestamp_secs(exp)),
        ..Default::default()
    })
}

// ----- GitHub ---------------------------------------------------------------------------------

fn anon(state: &AppState) -> Client {
    Client::new(
        github_config(state),
        Credential::Anonymous,
        Arc::new(SystemClock),
    )
}

fn form_header() -> Vec<(&'static str, String)> {
    vec![("accept", "application/json".to_string())]
}

/// Starts a GitHub device flow for the configured OAuth app.
pub async fn github_start(state: &AppState) -> ApiResult<v1::LoginResponse> {
    let l = &state.cfg.login;
    if l.github_client_id.is_empty() {
        return Err(ApiError::unimplemented(
            "GitHub device-flow login is not configured on this server (login.github_client_id)",
        ));
    }
    let c = anon(state);
    let url = format!("{}/login/device/code", c.config().web_url);
    let r = c
        .post_raw(
            &url,
            &json!({ "client_id": l.github_client_id, "scope": l.github_scopes }),
            &form_header(),
            false,
        )
        .await
        .map_err(github_error)?;
    let v: Value = github_ok(r)?.json().map_err(github_error)?;
    let Some(device_code) = v["device_code"].as_str() else {
        return Err(ApiError::new(
            ErrorReason::Unavailable,
            format!("GitHub returned no device code: {v}"),
        ));
    };
    Ok(v1::LoginResponse {
        verification_uri: v["verification_uri"].as_str().unwrap_or_default().into(),
        user_code: v["user_code"].as_str().unwrap_or_default().into(),
        device_code: device_code.into(),
        interval_seconds: v["interval"].as_u64().unwrap_or(5) as u32,
        expires_at: v["expires_in"]
            .as_u64()
            .map(|s| timestamp_secs(state.now_ms() / 1000 + s)),
        ..Default::default()
    })
}

/// Polls a GitHub device flow: the session once the human approved, else the device code
/// and interval to poll again.
pub async fn github_poll(
    state: &AppState,
    device_code: &str,
    meta: &RequestMeta,
) -> ApiResult<v1::LoginResponse> {
    let l = &state.cfg.login;
    if l.github_client_id.is_empty() {
        return Err(ApiError::unimplemented(
            "GitHub device-flow login is not configured on this server (login.github_client_id)",
        ));
    }
    let c = anon(state);
    let url = format!("{}/login/oauth/access_token", c.config().web_url);
    let r = c
        .post_raw(
            &url,
            &json!({
                "client_id": l.github_client_id,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }),
            &form_header(),
            false,
        )
        .await
        .map_err(github_error)?;
    let v: Value = github_ok(r)?.json().map_err(github_error)?;
    if let Some(token) = v["access_token"].as_str() {
        return github_exchange(state, token, meta).await;
    }
    let interval = v["interval"].as_u64().unwrap_or(5) as u32;
    match v["error"].as_str().unwrap_or_default() {
        "authorization_pending" => Ok(v1::LoginResponse {
            device_code: device_code.into(),
            interval_seconds: interval,
            ..Default::default()
        }),
        "slow_down" => Ok(v1::LoginResponse {
            device_code: device_code.into(),
            interval_seconds: interval.max(5) + 5,
            ..Default::default()
        }),
        "access_denied" => Err(ApiError::denied("the GitHub login was declined")),
        "expired_token" => Err(ApiError::unauthenticated(
            "the GitHub device code expired; start again",
        )),
        other => Err(ApiError::unauthenticated(format!(
            "GitHub refused the device code: {other}"
        ))),
    }
}

/// Logs in with a GitHub access token (F49-030).
pub async fn github_exchange(
    state: &AppState,
    token: &str,
    meta: &RequestMeta,
) -> ApiResult<v1::LoginResponse> {
    let token = token.trim();
    if token.is_empty() {
        return Err(ApiError::invalid("missing_field", "github_token is empty"));
    }
    let c = Client::new(
        github_config(state),
        Credential::Token(token.to_string()),
        Arc::new(SystemClock),
    );
    let user = c.get_json("/user").await.map_err(github_error)?;
    let id = user["id"]
        .as_u64()
        .ok_or_else(|| ApiError::unauthenticated("GitHub returned no account for the token"))?;
    let login = user["login"].as_str().unwrap_or_default().to_string();
    let groups = if state.cfg.login.github_team_sync {
        Some(github_teams(state, &c).await?)
    } else {
        None
    };
    let rec = principal_for(state, &format!("github:{id}"), &login, groups).await?;
    session(state, &rec, meta, "github").await
}

/// The account's GitHub teams as `<org>/<team>` groups (team sync, F49-041).
async fn github_teams(state: &AppState, c: &Client) -> ApiResult<Vec<String>> {
    let orgs = &state.cfg.login.github_team_orgs;
    let teams = c.get_all("/user/teams").await.map_err(github_error)?;
    Ok(teams
        .iter()
        .filter_map(|t| {
            let org = t["organization"]["login"].as_str()?;
            let slug = t["slug"].as_str()?;
            (orgs.is_empty() || orgs.iter().any(|o| o.eq_ignore_ascii_case(org)))
                .then(|| format!("{}/{}", org.to_lowercase(), slug.to_lowercase()))
        })
        .collect())
}

// ----- external OIDC --------------------------------------------------------------------------

/// Cached JWKS documents by URL.
#[derive(Default)]
pub struct JwksCache {
    keys: Mutex<BTreeMap<String, (u64, Vec<Value>)>>,
}

const JWKS_TTL_MS: u64 = 10 * 60 * 1000;

/// Whether `text` looks like a compact JWS.
pub fn is_jwt(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty())
}

fn b64url(s: &str) -> ApiResult<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| ApiError::unauthenticated("the id_token is not base64url"))
}

async fn fetch_json(state: &AppState, url: &str) -> ApiResult<Value> {
    let r = anon(state)
        .get_raw(url, &[("accept", "application/json".to_string())])
        .await
        .map_err(|e| {
            let mut err = ApiError::new(
                ErrorReason::Unavailable,
                format!(
                    "the identity provider is unavailable ({url}): {}",
                    e.message
                ),
            );
            err.detail.retryable = true;
            err
        })?;
    if r.status >= 400 {
        let mut err = ApiError::new(
            ErrorReason::Unavailable,
            format!("the identity provider answered HTTP {} for {url}", r.status),
        );
        err.detail.retryable = r.status >= 500;
        return Err(err);
    }
    r.json().map_err(ApiError::from)
}

async fn jwks(state: &AppState, refresh: bool) -> ApiResult<Vec<Value>> {
    let l = &state.cfg.login;
    let url = if l.oidc_jwks_url.is_empty() {
        let issuer = l.oidc_issuer.trim_end_matches('/');
        let disco =
            fetch_json(state, &format!("{issuer}/.well-known/openid-configuration")).await?;
        disco["jwks_uri"]
            .as_str()
            .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "the IdP names no jwks_uri"))?
            .to_string()
    } else {
        l.oidc_jwks_url.clone()
    };
    let now = state.now_ms();
    if !refresh
        && let Some((at, keys)) = state
            .jwks
            .keys
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&url)
        && now.saturating_sub(*at) < JWKS_TTL_MS
    {
        return Ok(keys.clone());
    }
    let doc = fetch_json(state, &url).await?;
    let keys = doc["keys"].as_array().cloned().unwrap_or_default();
    state
        .jwks
        .keys
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(url, (now, keys.clone()));
    Ok(keys)
}

/// Verifies a compact JWS signature with one JWK.
pub fn verify_jws(alg: &str, jwk: &Value, signed: &[u8], sig: &[u8]) -> bool {
    use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
    let field = |k: &str| {
        jwk[k].as_str().and_then(|s| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s.trim_end_matches('='))
                .ok()
        })
    };
    match (alg, jwk["kty"].as_str()) {
        ("RS256" | "RS384" | "RS512", Some("RSA")) => {
            let (Some(n), Some(e)) = (field("n"), field("e")) else {
                return false;
            };
            let params = match alg {
                "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                _ => &signature::RSA_PKCS1_2048_8192_SHA512,
            };
            RsaPublicKeyComponents { n: &n, e: &e }
                .verify(params, signed, sig)
                .is_ok()
        }
        ("ES256" | "ES384", Some("EC")) => {
            let (Some(x), Some(y)) = (field("x"), field("y")) else {
                return false;
            };
            let alg = if alg == "ES256" {
                &signature::ECDSA_P256_SHA256_FIXED
            } else {
                &signature::ECDSA_P384_SHA384_FIXED
            };
            let mut point = vec![4u8];
            point.extend(x);
            point.extend(y);
            UnparsedPublicKey::new(alg, point)
                .verify(signed, sig)
                .is_ok()
        }
        ("EdDSA", Some("OKP")) if jwk["crv"] == "Ed25519" => match field("x") {
            Some(x) => UnparsedPublicKey::new(&signature::ED25519, x)
                .verify(signed, sig)
                .is_ok(),
            None => false,
        },
        _ => false,
    }
}

/// The claims of an id_token from the configured IdP, after signature and claim checks.
pub async fn verify_id_token(state: &AppState, jwt: &str) -> ApiResult<Value> {
    let l = &state.cfg.login;
    let bad = |m: &str| ApiError::unauthenticated(format!("invalid id_token: {m}"));
    let mut parts = jwt.split('.');
    let (Some(h), Some(p), Some(s)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad("not a compact JWS"));
    };
    let header: Value =
        serde_json::from_slice(&b64url(h)?).map_err(|_| bad("the header is not JSON"))?;
    let claims: Value =
        serde_json::from_slice(&b64url(p)?).map_err(|_| bad("the payload is not JSON"))?;
    let sig = b64url(s)?;
    let alg = header["alg"].as_str().unwrap_or_default();
    if alg.is_empty() || alg == "none" || alg.starts_with("HS") {
        return Err(bad("the algorithm must be asymmetric"));
    }
    let kid = header["kid"].as_str();
    let signed = format!("{h}.{p}");
    let try_keys = |keys: &[Value]| {
        keys.iter()
            .filter(|k| kid.is_none() || k["kid"].as_str() == kid)
            .any(|k| verify_jws(alg, k, signed.as_bytes(), &sig))
    };
    if !try_keys(&jwks(state, false).await?) && !try_keys(&jwks(state, true).await?) {
        return Err(bad("the signature does not verify under the issuer's keys"));
    }
    let issuer = l.oidc_issuer.trim_end_matches('/');
    if claims["iss"].as_str().map(|i| i.trim_end_matches('/')) != Some(issuer) {
        return Err(bad("wrong issuer"));
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => *a == l.oidc_client_id,
        Value::Array(a) => a.iter().any(|x| *x == l.oidc_client_id.as_str()),
        _ => false,
    };
    if !aud_ok {
        return Err(bad("wrong audience"));
    }
    let now = (state.now_ms() / 1000) as i64;
    let leeway = l.oidc_leeway_secs as i64;
    match claims["exp"].as_i64() {
        Some(exp) if exp + leeway > now => {}
        _ => {
            return Err(ApiError::new(
                ErrorReason::TokenExpired,
                "the id_token expired",
            ));
        }
    }
    if claims["nbf"].as_i64().is_some_and(|n| n - leeway > now)
        || claims["iat"].as_i64().is_some_and(|i| i - leeway > now)
    {
        return Err(bad("the id_token is not valid yet"));
    }
    if claims["sub"].as_str().is_none_or(str::is_empty) {
        return Err(bad("no subject"));
    }
    Ok(claims)
}

/// Logs in with an id_token from the configured IdP (F49-021).
pub async fn oidc_login(
    state: &AppState,
    jwt: &str,
    meta: &RequestMeta,
) -> ApiResult<v1::LoginResponse> {
    let l = &state.cfg.login;
    let claims = verify_id_token(state, jwt).await?;
    let sub = claims["sub"].as_str().unwrap_or_default();
    let display = claims["preferred_username"]
        .as_str()
        .or(claims["name"].as_str())
        .or(claims["email"].as_str())
        .unwrap_or(sub)
        .to_string();
    let groups = claims[l.oidc_groups_claim.as_str()].as_array().map(|g| {
        g.iter()
            .filter_map(Value::as_str)
            .map(|n| {
                if n.contains('/') {
                    n.to_string()
                } else {
                    format!("idp/{n}")
                }
            })
            .collect()
    });
    let issuer = l.oidc_issuer.trim_end_matches('/');
    let rec = principal_for(state, &format!("oidc:{issuer}#{sub}"), &display, groups).await?;
    session(state, &rec, meta, "oidc").await
}

/// Shared login state of a server process.
pub type SharedJwks = Arc<JwksCache>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_shape() {
        assert!(is_jwt("a.b.c"));
        assert!(!is_jwt("bee-token"));
        assert!(!is_jwt("a..c"));
    }

    #[test]
    fn unknown_algorithms_never_verify() {
        let jwk = json!({"kty": "oct", "k": "AAAA"});
        assert!(!verify_jws("HS256", &jwk, b"x", b"y"));
        assert!(!verify_jws("RS256", &json!({"kty": "RSA"}), b"x", b"y"));
    }
}
