//! Plain-HTTP enterprise endpoints on the server's listener:
//!
//! | Route | Serves |
//! |---|---|
//! | `/scim/v2/{organizations,enterprises}/<name>/…` | SCIM 2.0 (`Scim::handle`), bearer SCIM tokens |
//! | `GET /saml/{orgs,enterprises}/<name>` | SAML SP metadata (the SP entity id is this URL) |
//! | `GET /saml/{orgs,enterprises}/<name>/login` | SP-initiated sign-in: redirect to the IdP (a bearer token links the identity) |
//! | `POST /saml/{orgs,enterprises}/<name>/acs` | Assertion consumer service (`SAMLResponse`, `RelayState` form fields) |
//! | `GET /login/oauth/authorize` | OAuth consent for the bearer's user: redirect with a code |
//! | `POST /login/oauth/access_token` | Code exchange (PKCE), refresh, and device-code grants |
//! | `POST /login/device/code` | Starts a device flow |
//! | `GET /.well-known/openid-configuration` | OIDC discovery (`crate::oidc`): Beehive as an identity provider |
//! | `GET /.well-known/jwks.json`, `GET /login/oauth/jwks` | The ES256 key that signs id_tokens |
//! | `GET`/`POST /login/oauth/userinfo` | OIDC userinfo for a bearer access token |
//! | `/_ci/…` | CI's signed transfer URLs: artifact and cache upload/download, job and run logs, one-time job secrets (`Ci::serve_transfer`) |
//!
//! Responses follow GitHub's shapes where GitHub has the endpoint (`access_token`,
//! `token_type`, `scope`, `error`, `error_description`; device `device_code`, `user_code`,
//! `verification_uri`, `expires_in`, `interval`), with JSON bodies; a grant with the `openid`
//! scope also carries an `id_token`.

// Helpers return early with a ready axum `Response` as their error.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use bee_forge::enterprise::apps::AuthorizationRequest;
use bee_forge::enterprise::common::{ScopeKind, ScopeResolver};
use bee_forge::enterprise::scim::ScimRequest;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::{EnterpriseScope, OAuthDeviceFlowStatus, OAuthTokenSet, enterprise_scope};
use http::{StatusCode, header};
use serde_json::{Value, json};

use super::ForgeWiring;
use crate::state::AppState;

/// Largest request body these endpoints read.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// The routes, to merge into the HTTP router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/scim/v2/{*rest}", any(scim))
        .route("/saml/{kind}/{name}", get(saml_metadata))
        .route("/saml/{kind}/{name}/login", get(saml_login))
        .route("/saml/{kind}/{name}/acs", post(saml_acs))
        .route("/login/oauth/authorize", get(oauth_authorize))
        .route("/login/oauth/access_token", post(oauth_token))
        .route("/login/device/code", post(device_code))
        .route("/.well-known/openid-configuration", get(oidc_discovery))
        .route("/.well-known/jwks.json", get(oidc_jwks))
        .route("/login/oauth/jwks", get(oidc_jwks))
        .route(
            "/login/oauth/userinfo",
            get(oidc_userinfo).post(oidc_userinfo),
        )
        .route("/webhooks/github", post(github_webhook))
        .route("/_ci/{*rest}", any(ci_transfer))
        .with_state(state)
}

/// The GitHub REST/GraphQL compatibility gateway (`bee-compat`, `docs/spec/github-compat.md`)
/// over this forge: `/api/v3`, `/api/graphql`, `/api/uploads`, and the root aliases (`/user`,
/// `/repos/...`, `/graphql`) that `gh` uses for an `api.<host>` origin. Credentials are the
/// server's own tokens; git ids come from the VCS stores through `bee-git`'s exporter, and
/// every port is the server's ([`super::compat`]), including the signed release asset URLs.
pub fn compat_router(state: &Arc<AppState>, w: &ForgeWiring) -> crate::errors::ApiResult<Router> {
    let compat = super::compat::build(state, w)?;
    let assets = Arc::new(super::compat::ServerAssets::new(state)?);
    Ok(compat
        .router()
        .merge(compat.root_router())
        .merge(super::compat::asset_router(assets))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            forget_access_after_writes,
        )))
}

/// The gateway reaches the forge services in-process (not through the request layer), so
/// its writes (repo updates, archive, transfer, collaborators, teams) drop cached
/// repo-access decisions here, before the answer leaves (`crate::access`).
async fn forget_access_after_writes(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let write = !matches!(
        *req.method(),
        http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
    );
    let resp = next.run(req).await;
    if write {
        state.forget_access();
    }
    resp
}

/// CI transfer URLs: the signature in the query is the authorization (`Ci::serve_transfer`).
async fn ci_transfer(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let method = req.method().as_str().to_string();
    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let body = match to_bytes(req.into_body(), bee_forge::ci::transfer::MAX_TRANSFER_BYTES).await {
        Ok(b) => b.to_vec(),
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    match w.ci.serve_transfer(&method, &target, body).await {
        Ok(r) => ([(header::CONTENT_TYPE, r.content_type)], r.body).into_response(),
        Err(e) => (status_of(&e), e.message).into_response(),
    }
}

/// GitHub live sync: verifies `X-Hub-Signature-256` with `import.github_webhook_secret`,
/// finds the import of the payload's repository and lets its importer place the change
/// (`bee_github::live`: `200` with the sync result, `401` bad signature, `404` no such
/// import, `409` frozen, `503` GitHub or the target failed, so GitHub redelivers).
async fn github_webhook(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let secret = state.cfg.import.github_webhook_secret.clone();
    if secret.is_empty() {
        return (StatusCode::NOT_FOUND, "GitHub webhooks are not configured").into_response();
    }
    let (parts, body) = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let get = |k: &str| {
        parts
            .headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let delivery = match bee_github::live::verify(secret.as_bytes(), &get, &body) {
        Ok(d) => d,
        Err(e) => return (StatusCode::UNAUTHORIZED, e.message).into_response(),
    };
    let repo = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v["repository"]["full_name"].as_str().map(str::to_string))
        .unwrap_or_default();
    let Some(entry) = w.github.by_github_repo(&repo) else {
        return (StatusCode::NOT_FOUND, format!("no live import of {repo:?}")).into_response();
    };
    match entry.importer.webhook(&delivery).await {
        Ok(r) => Json(serde_json::to_value(&r).unwrap_or(Value::Null)).into_response(),
        Err(e) if e.reason == bee_core::Reason::InvalidState => {
            (StatusCode::CONFLICT, e.message).into_response()
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `a=b&c=d` (form bodies and query strings).
pub fn parse_form(s: &str) -> BTreeMap<String, String> {
    s.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn wiring(state: &AppState) -> Option<Arc<ForgeWiring>> {
    state.hub.get().cloned()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "unavailable", "error_description": "the forge is not running"})),
    )
        .into_response()
}

struct Json(Value);

impl IntoResponse for Json {
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, "application/json")],
            self.0.to_string(),
        )
            .into_response()
    }
}

fn ip_of(req: &Request) -> String {
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn body_of(req: Request) -> Result<(http::request::Parts, Vec<u8>), Response> {
    let (parts, body) = req.into_parts();
    match to_bytes(body, MAX_BODY).await {
        Ok(b) => Ok((parts, b.to_vec())),
        Err(_) => Err((StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response()),
    }
}

fn status_of(e: &ForgeError) -> StatusCode {
    bee_proto::errors::reason_info(e.reason)
        .and_then(|i| StatusCode::from_u16(i.http_status()).ok())
        .unwrap_or(StatusCode::BAD_REQUEST)
}

// ----- SCIM ----------------------------------------------------------------------------------

async fn scim(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let ip = ip_of(&req);
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = parse_form(req.uri().query().unwrap_or(""));
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| crate::util::random_hex(8));
    let (_, body) = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let write = method != "GET";
    let resp = w
        .enterprise
        .scim
        .handle(ScimRequest {
            method,
            path,
            query,
            body,
            authorization,
            ip,
            request_id,
        })
        .await;
    if write {
        // Deprovisioning blocks at admission, and drops repo access, at once.
        w.forget_admissions();
        state.forget_access();
    }
    let mut out = Response::builder()
        .status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/scim+json");
    if let Some(l) = &resp.location {
        out = out.header(header::LOCATION, l);
    }
    let body = if resp.body.is_null() {
        Body::empty()
    } else {
        Body::from(resp.body.to_string())
    };
    out.body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ----- SAML ----------------------------------------------------------------------------------

async fn saml_scope(
    w: &ForgeWiring,
    kind: &str,
    name: &str,
) -> Result<bee_forge::enterprise::common::ResolvedScope, Response> {
    let scope = match kind {
        "orgs" => enterprise_scope::Scope::Org(name.to_string()),
        "enterprises" => enterprise_scope::Scope::Enterprise(name.to_string()),
        _ => return Err((StatusCode::NOT_FOUND, "unknown SAML scope kind").into_response()),
    };
    let resolver: &dyn ScopeResolver = w.enterprise.accounts.as_ref();
    match resolver
        .resolve(&Cx::system(), &Some(EnterpriseScope { scope: Some(scope) }))
        .await
    {
        Ok(s) if (s.kind == ScopeKind::Org) == (kind == "orgs") => Ok(s),
        Ok(_) => Err((StatusCode::NOT_FOUND, "unknown scope").into_response()),
        Err(e) => Err((status_of(&e), e.message).into_response()),
    }
}

async fn saml_metadata(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((kind, name)): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    match saml_scope(&w, &kind, &name).await {
        Ok(scope) => (
            [(header::CONTENT_TYPE, "application/samlmetadata+xml")],
            w.enterprise.sso.sp_metadata(&scope),
        )
            .into_response(),
        Err(r) => r,
    }
}

/// The forge caller of a request with a bearer token, if any.
fn bearer_caller(w: &ForgeWiring, req: &Request) -> Option<bee_forge::auth::Caller> {
    let auth = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut r = tonic::Request::new(());
    r.metadata_mut().insert("authorization", auth.parse().ok()?);
    if let Some(ip) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        r.metadata_mut().insert("x-forwarded-for", ip);
    }
    w.forge.authn.authenticate(&r).ok()
}

async fn saml_login(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((kind, name)): axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let scope = match saml_scope(&w, &kind, &name).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let principal = bearer_caller(&w, &req)
        .map(|c| c.cx.actor_id)
        .unwrap_or_default();
    match w
        .enterprise
        .sso
        .start_sign_in(&Cx::system(), &scope, "login", &principal, "")
        .await
    {
        Ok((url, _)) => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, url)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(e) => (status_of(&e), e.message).into_response(),
    }
}

async fn saml_acs(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((kind, name)): axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let scope = match saml_scope(&w, &kind, &name).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let (_, body) = match body_of(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let form = parse_form(&String::from_utf8_lossy(&body));
    let response = form.get("SAMLResponse").cloned().unwrap_or_default();
    let relay = form.get("RelayState").cloned().unwrap_or_default();
    match w
        .enterprise
        .sso
        .consume_response(&Cx::system(), &scope, &response, &relay)
        .await
    {
        Ok(s) => {
            w.forget_admissions();
            Json(json!({
                "principal_id": s.principal_id,
                "session_expires_ms": s.session_expires_ms,
                "linked": s.linked,
                "test": s.test,
            }))
            .into_response()
        }
        Err(e) => (
            status_of(&e),
            Json(json!({"error": e.reason_name(), "error_description": e.message})),
        )
            .into_response(),
    }
}

// ----- OAuth ---------------------------------------------------------------------------------

fn oauth_error(e: &ForgeError) -> Response {
    let code = match e.subcode.as_str() {
        "" => match e.reason {
            bee_proto::v1::ErrorReason::Unauthenticated => "invalid_client",
            bee_proto::v1::ErrorReason::NotFound => "invalid_client",
            _ => "invalid_request",
        },
        s => s,
    };
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": code, "error_description": e.message})),
    )
        .into_response()
}

fn token_json(t: &OAuthTokenSet, now_ms: u64) -> Value {
    let secs = |ts: &Option<pbjson_types::Timestamp>| {
        ts.as_ref()
            .map(|t| (t.seconds.max(0) as u64).saturating_sub(now_ms / 1000))
    };
    let mut v = json!({
        "access_token": t.access_token,
        "token_type": if t.token_type.is_empty() { "bearer" } else { t.token_type.as_str() },
        "scope": t.scopes.join(","),
    });
    if let Some(s) = secs(&t.expires_at) {
        v["expires_in"] = json!(s);
    }
    if !t.refresh_token.is_empty() {
        v["refresh_token"] = json!(t.refresh_token);
        if let Some(s) = secs(&t.refresh_token_expires_at) {
            v["refresh_token_expires_in"] = json!(s);
        }
    }
    v
}

async fn form_or_json(req: Request) -> Result<BTreeMap<String, String>, Response> {
    let mut fields = parse_form(req.uri().query().unwrap_or(""));
    let json_body = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let (_, body) = body_of(req).await?;
    if json_body {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&body) {
            for (k, v) in m {
                let s = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                fields.insert(k, s);
            }
        }
    } else {
        fields.extend(parse_form(&String::from_utf8_lossy(&body)));
    }
    Ok(fields)
}

async fn oauth_authorize(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let Some(caller) = bearer_caller(&w, &req) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "login_required", "error_description": "sign in first"})),
        )
            .into_response();
    };
    let q = parse_form(req.uri().query().unwrap_or(""));
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let scopes = get("scope")
        .split([' ', ','])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    match w
        .apps
        .create_authorization(
            &caller.cx,
            AuthorizationRequest {
                client_id: get("client_id"),
                user_code: get("user_code"),
                redirect_uri: get("redirect_uri"),
                scopes,
                state: get("state"),
                code_challenge: get("code_challenge"),
                code_challenge_method: get("code_challenge_method"),
                deny: get("deny") == "true",
                nonce: get("nonce"),
            },
        )
        .await
    {
        Ok(o) if !o.redirect_url.is_empty() => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, o.redirect_url)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Ok(_) => Json(json!({"approved": true})).into_response(),
        Err(e) => oauth_error(&e),
    }
}

async fn oauth_token(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let f = match form_or_json(req).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    let get = |k: &str| f.get(k).cloned().unwrap_or_default();
    let cx = Cx::system();
    let now = state.now_ms();
    let grant = get("grant_type");
    let nonce = if get("code").is_empty() {
        String::new()
    } else {
        w.apps
            .code_nonce(&cx, &get("code"))
            .await
            .unwrap_or_default()
    };
    let out = match grant.as_str() {
        "" | "authorization_code" if !get("code").is_empty() => {
            w.apps
                .exchange_code(
                    &cx,
                    &get("client_id"),
                    &get("client_secret"),
                    &get("code"),
                    &get("redirect_uri"),
                    &get("code_verifier"),
                )
                .await
        }
        "refresh_token" => {
            w.apps
                .refresh_token(
                    &cx,
                    &get("client_id"),
                    &get("client_secret"),
                    &get("refresh_token"),
                )
                .await
        }
        "urn:ietf:params:oauth:grant-type:device_code" => {
            match w
                .apps
                .poll_device_flow(&cx, &get("client_id"), &get("device_code"))
                .await
            {
                Ok(p) => match (p.status, p.tokens) {
                    (OAuthDeviceFlowStatus::OauthDeviceFlowStatusApproved, Some(t)) => Ok(t),
                    (status, _) => {
                        let code = match status {
                            OAuthDeviceFlowStatus::OauthDeviceFlowStatusSlowDown => "slow_down",
                            OAuthDeviceFlowStatus::OauthDeviceFlowStatusDenied => "access_denied",
                            OAuthDeviceFlowStatus::OauthDeviceFlowStatusExpired => "expired_token",
                            _ => "authorization_pending",
                        };
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": code, "interval": p.interval_secs})),
                        )
                            .into_response();
                    }
                },
                Err(e) => Err(e),
            }
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "unsupported_grant_type",
                    "error_description": format!("grant_type {other:?} is not supported"),
                })),
            )
                .into_response();
        }
    };
    match out {
        Ok(t) => {
            let mut body = token_json(&t, now);
            match crate::oidc::id_token(
                &state,
                &t.access_token,
                &get("client_id"),
                &t.scopes,
                &nonce,
            )
            .await
            {
                Ok(Some(id)) => body["id_token"] = json!(id),
                Ok(None) => {}
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": "server_error", "error_description": e.detail.message}),
                        ),
                    )
                        .into_response();
                }
            }
            Json(body).into_response()
        }
        Err(e) => oauth_error(&e),
    }
}

// ----- OpenID Connect provider ---------------------------------------------------------------

async fn oidc_discovery(State(state): State<Arc<AppState>>) -> Response {
    Json(crate::oidc::discovery(&state)).into_response()
}

async fn oidc_jwks(State(state): State<Arc<AppState>>) -> Response {
    match crate::oidc::jwks(&state) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "server_error", "error_description": e.detail.message})),
        )
            .into_response(),
    }
}

async fn oidc_userinfo(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")],
            Json(json!({"error": "invalid_token"})),
        )
            .into_response()
    };
    if token.is_empty() {
        return unauthorized();
    }
    match crate::oidc::userinfo(&state, &token).await {
        Ok(v) => Json(v).into_response(),
        Err(_) => unauthorized(),
    }
}

async fn device_code(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let Some(w) = wiring(&state) else {
        return unavailable();
    };
    let f = match form_or_json(req).await {
        Ok(f) => f,
        Err(r) => return r,
    };
    let scopes: Vec<String> = f
        .get("scope")
        .map(|s| {
            s.split([' ', ','])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let client = f.get("client_id").cloned().unwrap_or_default();
    match w
        .apps
        .start_device_flow(&Cx::system(), &client, &scopes)
        .await
    {
        Ok(d) => Json(json!({
            "device_code": d.device_code,
            "user_code": d.user_code,
            "verification_uri": d.verification_uri,
            "verification_uri_complete": d.verification_uri_complete,
            "expires_in": d.expires_ms.saturating_sub(state.now_ms()) / 1000,
            "interval": d.interval_secs,
        }))
        .into_response(),
        Err(e) => oauth_error(&e),
    }
}

/// `redirect_uri` with query parameters appended (helpers for tests and clients).
pub fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    let q: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}{}", q.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_round_trip() {
        let f = parse_form("a=b+c&d=%2Fe%3D&empty=&flag");
        assert_eq!(f["a"], "b c");
        assert_eq!(f["d"], "/e=");
        assert_eq!(f["empty"], "");
        assert_eq!(f["flag"], "");
        let url = with_query("https://x.test/cb", &[("code", "a b"), ("state", "s/1")]);
        assert_eq!(url, "https://x.test/cb?code=a%20b&state=s%2F1");
        assert_eq!(parse_form(url.split_once('?').unwrap().1)["code"], "a b");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }
}
