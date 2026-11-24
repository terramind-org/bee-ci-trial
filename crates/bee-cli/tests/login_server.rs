//! The real `bee` binary against an in-process `bee-server --dev` for the identity
//! leftovers: `bee auth login --github-token` (F49-030, with a fake GitHub on loopback),
//! `bee admin get-repo-health` (F416-032) and `bee security verify-object-signature`
//! (F49-004). Every `--json` document is validated against its published schema.

use crate::support;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use serde_json::{Value, json};
use support::{Bee, validate};

async fn fake_github(req: Request) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = match (req.uri().path(), auth.ends_with("gho_cli")) {
        ("/user", true) => json!({"id": 9001, "login": "cli-user"}),
        ("/user/teams", true) => json!([{"slug": "ops", "organization": {"login": "acme"}}]),
        _ => {
            return (
                http::StatusCode::UNAUTHORIZED,
                "{\"message\":\"Bad credentials\"}",
            )
                .into_response();
        }
    };
    (
        [(http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn bee(url: &str, token: &str, args: &[&str]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let run = Bee::new(&all)
        .env("BEE_ENDPOINT", url)
        .env("BEE_TOKEN", token)
        .run()
        .0;
    assert_eq!(run.code, 0, "bee {args:?}: {run:?}");
    let doc = run.json();
    let cmd: Vec<&str> = args
        .iter()
        .take_while(|a| !a.starts_with('-'))
        .copied()
        .collect();
    validate(
        &format!("schemas/v1/cli/{}.schema.json", cmd.join(".")),
        &doc,
    );
    doc["data"].clone()
}

#[test]
fn cli_logs_in_with_a_github_token_and_reads_repo_health_and_signatures() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let gh = runtime.block_on(async {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, axum::Router::new().fallback(fake_github))
                .await
                .unwrap();
        });
        format!("http://{addr}")
    });
    let mut cfg = Config::dev(tmp.path().join("server"), Some("127.0.0.1:0".into()));
    cfg.login.github_url = gh.clone();
    cfg.login.github_api_url = gh;
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    let srv = runtime.block_on(start(opts)).unwrap();
    let url = srv.url.clone();
    let dev_token = srv.dev.as_ref().unwrap().token.clone();

    let login = bee(&url, "", &["auth", "login", "--github-token", "gho_cli"]);
    let token = login["token"].as_str().expect("token").to_string();
    let who = bee(&url, &token, &["auth", "who-am-i"]);
    assert_eq!(who["principal"]["externalIds"], json!(["github:9001"]));
    assert!(who["groups"].to_string().contains("acme/ops"), "{who}");

    let health = bee(
        &url,
        &dev_token,
        &["admin", "get-repo-health", "--repo", "dev/playground"],
    );
    assert_eq!(health["ref"], "main");
    assert_eq!(health["historyDepth"], "1", "{health}");
    assert_eq!(health["largeFileBytes"], "52428800");

    let tip = bee(
        &url,
        &dev_token,
        &[
            "object",
            "resolve-ref",
            "--repo",
            "dev/playground",
            "--name",
            "main",
        ],
    );
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        tip["ref"]["target"]["raw"].as_str().unwrap(),
    )
    .unwrap();
    let id = bee_core::ObjectId::from_bytes(&raw).unwrap().to_text();
    let v = bee(
        &url,
        &dev_token,
        &[
            "security",
            "verify-object-signature",
            "--repo",
            "dev/playground",
            "--changeset",
            &id,
        ],
    );
    assert_eq!(v["reason"], "SECURITY_SIGNATURE_REASON_UNSIGNED");
    runtime.block_on(srv.shutdown());
}
