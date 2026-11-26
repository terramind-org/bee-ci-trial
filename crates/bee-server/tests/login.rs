//! Human login over HTTP/JSON against an in-process `bee-server --dev`, with a fake GitHub
//! and a fake OIDC identity provider on loopback: GitHub device flow and token exchange with
//! team sync (F49-021, F49-030, F49-041), what happens when GitHub is down (F49-031), id_tokens
//! from an external IdP (F49-021), and Beehive as an OIDC provider on `/login/oauth/*`
//! (F49-012).

#![allow(clippy::result_large_err)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use common::*;
use http::StatusCode;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

/// A JSON response (the server builds axum without its `json` feature).
struct J(Value);

impl IntoResponse for J {
    fn into_response(self) -> Response {
        (
            [(http::header::CONTENT_TYPE, "application/json")],
            self.0.to_string(),
        )
            .into_response()
    }
}

// ----- fake GitHub ------------------------------------------------------------------------

/// An account: numeric id, login and teams as (org, slug).
type Account = (u64, String, Vec<(String, String)>);

#[derive(Default)]
struct FakeGithub {
    down: AtomicBool,
    polls: AtomicU32,
    /// token -> (numeric id, login, teams as (org, slug))
    users: Mutex<BTreeMap<String, Account>>,
}

async fn github(State(f): State<Arc<FakeGithub>>, req: Request) -> Response {
    if f.down.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "unicorn").into_response();
    }
    let path = req.uri().path().to_string();
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_whitespace().last())
        .unwrap_or_default()
        .to_string();
    let user = f.users.lock().unwrap().get(&auth).cloned();
    match path.as_str() {
        "/login/device/code" => J(json!({
            "device_code": "dc-1", "user_code": "ABCD-1234",
            "verification_uri": "https://github.example/login/device",
            "expires_in": 900, "interval": 5,
        }))
        .into_response(),
        "/login/oauth/access_token" => {
            if f.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                J(json!({"error": "authorization_pending", "interval": 5})).into_response()
            } else {
                J(json!({"access_token": "gho_alice", "token_type": "bearer", "scope": "read:user,read:org"}))
                    .into_response()
            }
        }
        "/user" => match user {
            Some((id, login, _)) => J(json!({"id": id, "login": login})).into_response(),
            None => (
                StatusCode::UNAUTHORIZED,
                J(json!({"message": "Bad credentials"})),
            )
                .into_response(),
        },
        "/user/teams" => match user {
            Some((_, _, teams)) => J(Value::Array(
                teams
                    .iter()
                    .map(|(org, slug)| json!({"slug": slug, "organization": {"login": org}}))
                    .collect(),
            ))
            .into_response(),
            None => StatusCode::UNAUTHORIZED.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve(router: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn fake_github() -> (Arc<FakeGithub>, String) {
    let f = Arc::new(FakeGithub::default());
    f.users.lock().unwrap().insert(
        "gho_alice".into(),
        (
            4242,
            "alice".into(),
            vec![("Acme".into(), "core".into()), ("other".into(), "x".into())],
        ),
    );
    f.users
        .lock()
        .unwrap()
        .insert("gho_bob".into(), (7, "bob".into(), Vec::new()));
    let router = axum::Router::new().fallback(github).with_state(f.clone());
    (f, serve(router).await)
}

async fn login(t: &TestServer, req: Value) -> Result<Value, bee_proto::v1::ErrorDetail> {
    t.client("")
        .call_rpc_json("AuthService", "Login", &req)
        .await
}

async fn whoami(t: &TestServer, token: &str) -> Result<Value, bee_proto::v1::ErrorDetail> {
    t.call_as(token, "AuthService", "WhoAmI", json!({})).await
}

async fn github_server() -> (TestServer, Arc<FakeGithub>) {
    let (f, base) = fake_github().await;
    let t = start_with(|c| {
        c.login.github_url = base.clone();
        c.login.github_api_url = base.clone();
        c.login.github_client_id = "Iv1.beetest".into();
        c.login.github_team_orgs = vec!["acme".into()];
    })
    .await;
    (t, f)
}

/// F49-021 (device flow) and F49-041 (GitHub team sync): the server runs GitHub's device
/// flow, polls until approved, logs the GitHub account in as a human with the stable id
/// `github:<id>` and syncs its teams of the allowed orgs as policy groups.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_device_flow_logs_in_and_syncs_teams() {
    let (t, _) = github_server().await;
    let start = login(&t, json!({ "githubStart": true }))
        .await
        .expect("start");
    assert_eq!(start["userCode"], "ABCD-1234");
    assert_eq!(
        start["verificationUri"],
        "https://github.example/login/device"
    );
    assert_eq!(start["deviceCode"], "dc-1");
    assert_eq!(start["intervalSeconds"], 5);
    // Not approved yet: no token, poll again.
    let pending = login(&t, json!({ "githubDeviceCode": "dc-1" }))
        .await
        .expect("poll");
    assert!(
        pending["token"].as_str().unwrap_or("").is_empty(),
        "{pending}"
    );
    assert_eq!(pending["deviceCode"], "dc-1");
    let done = login(&t, json!({ "githubDeviceCode": "dc-1" }))
        .await
        .expect("poll");
    let token = done["token"].as_str().unwrap().to_string();
    let who = whoami(&t, &token).await.expect("whoami");
    assert_eq!(who["principal"]["kind"], "PRINCIPAL_KIND_HUMAN");
    assert_eq!(who["principal"]["display"], "alice");
    assert_eq!(who["principal"]["externalIds"], json!(["github:4242"]));
    let groups = who["groups"].to_string();
    assert!(groups.contains("\"acme/core\""), "{who}");
    assert!(
        !groups.contains("other/x"),
        "only github_team_orgs are synced: {who}"
    );
}

/// F49-030: a GitHub token logs in its account; the principal is keyed by the numeric id, so
/// a renamed login keeps its principal; a bad token is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_token_exchange_logs_in_by_numeric_id() {
    let (t, f) = github_server().await;
    let first = login(&t, json!({ "githubToken": "gho_bob" }))
        .await
        .expect("exchange");
    let who = whoami(&t, first["token"].as_str().unwrap()).await.unwrap();
    let id = who["principal"]["id"].as_str().unwrap().to_string();
    assert_eq!(who["principal"]["externalIds"], json!(["github:7"]));
    f.users
        .lock()
        .unwrap()
        .insert("gho_bob2".into(), (7, "robert".into(), Vec::new()));
    let second = login(&t, json!({ "githubToken": "gho_bob2" }))
        .await
        .unwrap();
    let who = whoami(&t, second["token"].as_str().unwrap()).await.unwrap();
    assert_eq!(
        who["principal"]["id"],
        id.as_str(),
        "same account, same principal"
    );
    assert_eq!(who["principal"]["display"], "robert");
    let e = login(&t, json!({ "githubToken": "nope" }))
        .await
        .expect_err("bad token");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
}

/// F49-031: while GitHub is down new GitHub logins fail with a retryable `UNAVAILABLE`, and
/// tokens already issued keep working with the groups synced at their login.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn when_github_is_down_new_logins_fail_and_existing_tokens_work() {
    let (t, f) = github_server().await;
    let ok = login(&t, json!({ "githubToken": "gho_alice" }))
        .await
        .unwrap();
    let token = ok["token"].as_str().unwrap().to_string();
    f.down.store(true, Ordering::SeqCst);
    for req in [
        json!({ "githubToken": "gho_alice" }),
        json!({ "githubStart": true }),
        json!({ "githubDeviceCode": "dc-1" }),
    ] {
        let e = login(&t, req.clone()).await.expect_err("GitHub down");
        assert_eq!(reason(&e), "UNAVAILABLE", "{req}: {e:?}");
        assert!(e.retryable, "{req}");
        assert_eq!(e.subcode, "github_unavailable");
    }
    let who = whoami(&t, &token).await.expect("existing token works");
    assert!(who["groups"].to_string().contains("acme/core"), "{who}");
    f.down.store(false, Ordering::SeqCst);
    login(&t, json!({ "githubToken": "gho_alice" }))
        .await
        .expect("recovered");
}

// ----- fake OIDC identity provider ---------------------------------------------------------

struct Idp {
    key: EcdsaKeyPair,
    issuer: String,
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

impl Idp {
    fn jwk(&self) -> Value {
        let p = self.key.public_key().as_ref();
        json!({"kty": "EC", "crv": "P-256", "kid": "idp-1", "alg": "ES256",
               "x": b64(&p[1..33]), "y": b64(&p[33..65])})
    }

    fn token(&self, claims: Value) -> String {
        let h = b64(json!({"alg": "ES256", "kid": "idp-1", "typ": "JWT"})
            .to_string()
            .as_bytes());
        let p = b64(claims.to_string().as_bytes());
        let sig = self
            .key
            .sign(
                &ring::rand::SystemRandom::new(),
                format!("{h}.{p}").as_bytes(),
            )
            .unwrap();
        format!("{h}.{p}.{}", b64(sig.as_ref()))
    }
}

async fn idp_routes(State(idp): State<Arc<Mutex<Option<Arc<Idp>>>>>, req: Request) -> Response {
    let idp = idp.lock().unwrap().clone().unwrap();
    match req.uri().path() {
        "/.well-known/openid-configuration" => J(json!({
            "issuer": idp.issuer, "jwks_uri": format!("{}/keys", idp.issuer),
        }))
        .into_response(),
        "/keys" => J(json!({ "keys": [idp.jwk()] })).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// F49-021 (OIDC): an id_token from the configured IdP logs in its subject after the
/// signature (via discovery and JWKS), issuer, audience and expiry checks; its groups claim
/// becomes external groups. A bee token pasted into the same field still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_oidc_id_tokens_log_in_after_signature_and_claim_checks() {
    let slot: Arc<Mutex<Option<Arc<Idp>>>> = Arc::new(Mutex::new(None));
    let base = serve(
        axum::Router::new()
            .fallback(idp_routes)
            .with_state(slot.clone()),
    )
    .await;
    let rng = ring::rand::SystemRandom::new();
    let pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let idp = Arc::new(Idp {
        key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pk8.as_ref(), &rng)
            .unwrap(),
        issuer: base.clone(),
    });
    *slot.lock().unwrap() = Some(idp.clone());
    let t = start_with(|c| {
        c.login.oidc_issuer = base.clone();
        c.login.oidc_client_id = "beehive".into();
    })
    .await;
    let now = (t.srv.state.now_ms() / 1000) as i64;
    let claims = |aud: &str, exp: i64| {
        json!({"iss": base, "sub": "user-1", "aud": aud, "iat": now, "exp": exp,
               "preferred_username": "carol", "groups": ["eng", "acme/ops"]})
    };
    let ok = login(
        &t,
        json!({ "oidcIdToken": idp.token(claims("beehive", now + 300)) }),
    )
    .await
    .expect("oidc login");
    let who = whoami(&t, ok["token"].as_str().unwrap()).await.unwrap();
    assert_eq!(who["principal"]["display"], "carol");
    assert_eq!(
        who["principal"]["externalIds"],
        json!([format!("oidc:{base}#user-1")])
    );
    let groups = who["groups"].to_string();
    assert!(
        groups.contains("idp/eng") && groups.contains("acme/ops"),
        "{who}"
    );

    let e = login(
        &t,
        json!({ "oidcIdToken": idp.token(claims("someone-else", now + 300)) }),
    )
    .await
    .expect_err("wrong audience");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    let e = login(
        &t,
        json!({ "oidcIdToken": idp.token(claims("beehive", now - 3600)) }),
    )
    .await
    .expect_err("expired");
    assert_eq!(reason(&e), "TOKEN_EXPIRED");
    let mut forged = idp.token(claims("beehive", now + 300));
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    let e = login(&t, json!({ "oidcIdToken": forged }))
        .await
        .expect_err("forged");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    // Another IdP's key does not verify.
    let other = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let stranger = Idp {
        key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, other.as_ref(), &rng)
            .unwrap(),
        issuer: base.clone(),
    };
    let e = login(
        &t,
        json!({ "oidcIdToken": stranger.token(claims("beehive", now + 300)) }),
    )
    .await
    .expect_err("unknown key");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    // Token paste keeps working.
    let pasted = login(&t, json!({ "oidcIdToken": t.token }))
        .await
        .expect("paste");
    assert_eq!(pasted["token"], t.token.as_str());
}

// ----- Beehive as an OIDC provider ----------------------------------------------------------

fn query_of(location: &str) -> BTreeMap<String, String> {
    location
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn verify_id_token(jwks: &Value, jwt: &str) -> Value {
    let (signed, sig) = jwt.rsplit_once('.').unwrap();
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig)
        .unwrap();
    let key = &jwks["keys"][0];
    assert!(
        bee_server::login::verify_jws("ES256", key, signed.as_bytes(), &sig),
        "the id_token verifies with the published key"
    );
    let payload = signed.split('.').nth(1).unwrap();
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap()
}

async fn get_json(url: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let (s, _, b) = http("GET", url, headers, Vec::new()).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// Scale-out (docs/scaling.md): with `auth.oidc_key_pkcs8_hex` set, the JWKS publishes exactly
/// that key and the server never generates one of its own in its data directory, so every
/// replica given the same setting publishes the same JWKS and verifies the others' tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_configured_oidc_key_is_the_published_one() {
    let rng = ring::rand::SystemRandom::new();
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, doc.as_ref(), &rng).unwrap();
    let point = pair.public_key().as_ref().to_vec();
    let hex: String = doc.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let t = start_with(|c| c.auth.oidc_key_pkcs8_hex = hex).await;
    let base = t.url().trim_end_matches('/').to_string();
    let (s, jwks) = get_json(&format!("{base}/.well-known/jwks.json"), &[]).await;
    assert_eq!(s, 200);
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    assert_eq!(jwks["keys"].as_array().map(Vec::len), Some(1));
    assert_eq!(jwks["keys"][0]["x"], b64(&point[1..33]).as_str());
    assert_eq!(jwks["keys"][0]["y"], b64(&point[33..65]).as_str());
    assert!(
        !t.cfg.server.data_dir.join("oidc-signing-key.p8").exists(),
        "no key of its own was generated"
    );
}

/// F49-012: the code flow (PKCE) and device flow of `/login/oauth/*` return an ES256
/// id_token for the `openid` scope, verifiable with the JWKS the discovery document names;
/// userinfo answers for the access token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn beehive_is_an_oidc_provider() {
    let t = start_default().await;
    let base = t.url().trim_end_matches('/').to_string();
    let (s, disco) = get_json(&format!("{base}/.well-known/openid-configuration"), &[]).await;
    assert_eq!(s, 200);
    assert_eq!(disco["issuer"], base.as_str());
    assert_eq!(
        disco["id_token_signing_alg_values_supported"],
        json!(["ES256"])
    );
    let jwks_uri = disco["jwks_uri"].as_str().unwrap().to_string();
    let (_, jwks) = get_json(&jwks_uri, &[]).await;
    assert_eq!(jwks["keys"][0]["kty"], "EC");

    let app = t
        .call(
            "AppService",
            "CreateOAuthApp",
            json!({ "name": "relying-party", "callbackUrls": ["https://rp.example/cb"], "deviceFlowEnabled": true }),
        )
        .await
        .expect("oauth app");
    let client_id = app["app"]["clientId"].as_str().unwrap().to_string();
    let secret = app["clientSecret"].as_str().unwrap().to_string();

    // Code flow with PKCE and a nonce.
    let verifier = "v".repeat(48);
    let challenge = b64(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
    let auth = format!("Bearer {}", t.token);
    let url = format!(
        "{base}/login/oauth/authorize?client_id={client_id}&redirect_uri=https%3A%2F%2Frp.example%2Fcb\
         &scope=openid%20profile&state=s1&nonce=n-123&code_challenge={challenge}&code_challenge_method=S256"
    );
    let (s, h, _) = http("GET", &url, &[("authorization", auth.as_str())], Vec::new()).await;
    assert_eq!(s, 302);
    let q = query_of(h["location"].to_str().unwrap());
    let code = q["code"].clone();
    let body = format!(
        "grant_type=authorization_code&client_id={client_id}&client_secret={secret}&code={code}\
         &redirect_uri=https%3A%2F%2Frp.example%2Fcb&code_verifier={verifier}"
    );
    let (s, _, b) = http(
        "POST",
        &format!("{base}/login/oauth/access_token"),
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("accept", "application/json"),
        ],
        body.into_bytes(),
    )
    .await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    let tok: Value = serde_json::from_slice(&b).unwrap();
    let claims = verify_id_token(&jwks, tok["id_token"].as_str().expect("id_token"));
    assert_eq!(claims["iss"], base.as_str());
    assert_eq!(claims["aud"], client_id.as_str());
    assert_eq!(claims["sub"], t.dev_user().as_str());
    assert_eq!(claims["nonce"], "n-123");
    assert!(claims["exp"].as_i64().unwrap() > claims["iat"].as_i64().unwrap());
    assert!(claims["preferred_username"].is_string());
    let access = format!("Bearer {}", tok["access_token"].as_str().unwrap());
    let (s, info) = get_json(
        &format!("{base}/login/oauth/userinfo"),
        &[("authorization", access.as_str())],
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(info["sub"], t.dev_user().as_str());
    let (s, _) = get_json(&format!("{base}/login/oauth/userinfo"), &[]).await;
    assert_eq!(s, 401);

    // Without `openid` there is no id_token.
    let url = format!(
        "{base}/login/oauth/authorize?client_id={client_id}&redirect_uri=https%3A%2F%2Frp.example%2Fcb&scope=read%3Auser&state=s2"
    );
    let (_, h, _) = http("GET", &url, &[("authorization", auth.as_str())], Vec::new()).await;
    let code = query_of(h["location"].to_str().unwrap())["code"].clone();
    let body = format!(
        "grant_type=authorization_code&client_id={client_id}&client_secret={secret}&code={code}\
         &redirect_uri=https%3A%2F%2Frp.example%2Fcb"
    );
    let (_, _, b) = http(
        "POST",
        &format!("{base}/login/oauth/access_token"),
        &[("content-type", "application/x-www-form-urlencoded")],
        body.into_bytes(),
    )
    .await;
    let tok: Value = serde_json::from_slice(&b).unwrap();
    assert!(
        tok["access_token"].is_string() && tok.get("id_token").is_none(),
        "{tok}"
    );

    // Device flow with `openid`.
    let (_, _, b) = http(
        "POST",
        &format!("{base}/login/device/code"),
        &[("content-type", "application/x-www-form-urlencoded")],
        format!("client_id={client_id}&scope=openid").into_bytes(),
    )
    .await;
    let dev: Value = serde_json::from_slice(&b).unwrap();
    let user_code = dev["user_code"].as_str().unwrap();
    let (s, _, _) = http(
        "GET",
        &format!("{base}/login/oauth/authorize?user_code={user_code}"),
        &[("authorization", auth.as_str())],
        Vec::new(),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _, b) = http(
        "POST",
        &format!("{base}/login/oauth/access_token"),
        &[("content-type", "application/x-www-form-urlencoded")],
        format!(
            "client_id={client_id}&device_code={}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
            dev["device_code"].as_str().unwrap()
        )
        .into_bytes(),
    )
    .await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    let tok: Value = serde_json::from_slice(&b).unwrap();
    let claims = verify_id_token(&jwks, tok["id_token"].as_str().expect("device id_token"));
    assert_eq!(claims["sub"], t.dev_user().as_str());
    assert!(claims.get("nonce").is_none());
}
