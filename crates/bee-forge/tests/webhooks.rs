//! Webhook delivery against a local HTTP receiver: HMAC signatures, retries with backoff,
//! the delivery log, redelivery, ping and the GitHub-compatible format.

use crate::common;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use bee_forge::Cx;
use bee_forge::checks::Report;
use bee_forge::events::delivery::verify_signature;
use bee_forge::events::webhooks::{HookOwner, NewHook};
use bee_forge::orgs::repos::NewRepo;
use bee_proto::v1::{CheckStatus, LeaseMode, ObjectId, PrincipalKind, Repo, WebhookFormat};

#[derive(Default)]
struct Receiver {
    fail_next: usize,
    got: Vec<(BTreeMap<String, String>, String)>,
}

type Shared = Arc<Mutex<Receiver>>;

async fn hook(State(s): State<Shared>, headers: HeaderMap, body: String) -> StatusCode {
    let mut r = s.lock().unwrap();
    let h = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    r.got.push((h, body));
    if r.fail_next > 0 {
        r.fail_next -= 1;
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    }
}

async fn receiver() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/hook", post(hook))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/hook"), shared)
}

fn owner_cx() -> Cx {
    Cx::new("owner", PrincipalKind::Human)
}

async fn repo(env: &common::Env) -> Repo {
    let cx = owner_cx();
    let p = env.forge.hooks.directory.get("owner").await.unwrap();
    let org = env.forge.orgs.create_org(&cx, "acme", "", p).await.unwrap();
    let id = env.forge.db.ids().ulid().unwrap();
    env.forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id,
                name: "app".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap()
}

fn lease_req(repo: &Repo, path: &str) -> bee_forge::leases::AcquireRequest {
    bee_forge::leases::AcquireRequest {
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        ref_name: "main".into(),
        paths: vec![path.as_bytes().to_vec()],
        mode: LeaseMode::Advisory,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deliveries_are_signed_retried_logged_and_redeliverable() {
    let env = common::env().await;
    let repo = repo(&env).await;
    let (url, rx) = receiver().await;
    let owner = HookOwner::repo(&repo.id, &repo.org_id);
    let cx = owner_cx();
    let hook = env
        .forge
        .webhooks
        .create(
            &cx,
            &owner,
            NewHook {
                url,
                kinds: vec!["lease.*".into()],
                secret: "s3cret".into(),
                format: WebhookFormat::Bee,
                active: true,
                insecure_ssl: false,
            },
        )
        .await
        .unwrap();

    // First delivery succeeds and is signed.
    env.forge
        .leases
        .acquire(&cx, lease_req(&repo, "src"))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    {
        let r = rx.lock().unwrap();
        assert_eq!(r.got.len(), 1);
        let (h, body) = &r.got[0];
        assert_eq!(h["x-bee-event"], "lease.acquired");
        assert_eq!(h["x-bee-hook-id"], hook.id);
        assert!(verify_signature(
            "s3cret",
            body.as_bytes(),
            &h["x-bee-signature-256"]
        ));
        assert!(!verify_signature(
            "wrong",
            body.as_bytes(),
            &h["x-bee-signature-256"]
        ));
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["kind"], "lease.acquired");
    }

    // A failing receiver gets retried with backoff until it answers 2xx.
    rx.lock().unwrap().fail_next = 2;
    env.forge
        .leases
        .acquire(&cx, lease_req(&repo, "docs"))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        env.forge.webhooks.deliver_due(&Cx::system()).await.unwrap();
    }
    assert_eq!(rx.lock().unwrap().got.len(), 4);
    assert_eq!(env.forge.webhooks.queued(&Cx::system()).await.unwrap(), 0);
    let (log, _) = env
        .forge
        .webhooks
        .deliveries(&cx, &owner, &hook.id, None, "", 50)
        .await
        .unwrap();
    let attempts: Vec<(u32, bool)> = log.iter().map(|d| (d.attempt, d.success)).collect();
    assert_eq!(attempts, vec![(3, true), (2, false), (1, false), (1, true)]);
    assert_eq!(log[1].status_code, 500);
    let (failed, _) = env
        .forge
        .webhooks
        .deliveries(&cx, &owner, &hook.id, Some(false), "", 50)
        .await
        .unwrap();
    assert_eq!(failed.len(), 2);

    // A receiver that never recovers stops after max_attempts.
    rx.lock().unwrap().fail_next = 100;
    env.forge
        .leases
        .acquire(&cx, lease_req(&repo, "ci"))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        env.forge.webhooks.deliver_due(&Cx::system()).await.unwrap();
    }
    assert_eq!(
        rx.lock().unwrap().got.len(),
        7,
        "three attempts, then given up"
    );
    assert_eq!(env.forge.webhooks.queued(&Cx::system()).await.unwrap(), 0);
    rx.lock().unwrap().fail_next = 0;

    // Redelivery re-sends the same body, re-signed, flagged as a redelivery.
    let first = log.last().unwrap();
    let full = env
        .forge
        .webhooks
        .delivery(&cx, &owner, &hook.id, &first.id)
        .await
        .unwrap();
    assert!(!full.request_body.is_empty());
    let again = env
        .forge
        .webhooks
        .redeliver(&cx, &owner, &hook.id, &first.id)
        .await
        .unwrap();
    assert!(again.success && again.redelivery);
    {
        let r = rx.lock().unwrap();
        let (h, body) = r.got.last().unwrap();
        assert_eq!(*body, full.request_body);
        assert!(verify_signature(
            "s3cret",
            body.as_bytes(),
            &h["x-bee-signature-256"]
        ));
        assert_ne!(h["x-bee-delivery"], r.got[0].0["x-bee-delivery"]);
    }

    // Ping.
    let ping = env
        .forge
        .webhooks
        .ping(&cx, &owner, &hook.id)
        .await
        .unwrap();
    assert!(ping.success);
    assert_eq!(ping.event_kind, "ping");
    let h = env.forge.webhooks.get(&cx, &owner, &hook.id).await.unwrap();
    assert_eq!(h.last_delivery.unwrap().event_kind, "ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_format_and_kind_filters() {
    let env = common::env().await;
    let repo = repo(&env).await;
    let (url, rx) = receiver().await;
    let owner = HookOwner::repo(&repo.id, &repo.org_id);
    let cx = owner_cx();
    // Unknown kinds are refused.
    let bad = env
        .forge
        .webhooks
        .create(
            &cx,
            &owner,
            NewHook {
                url: url.clone(),
                kinds: vec!["nope.*".into()],
                active: true,
                ..NewHook::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad.subcode, "unknown_event_kind");
    env.forge
        .webhooks
        .create(
            &cx,
            &owner,
            NewHook {
                url,
                kinds: vec!["check.updated".into()],
                secret: "k".into(),
                format: WebhookFormat::Github,
                active: true,
                insecure_ssl: false,
            },
        )
        .await
        .unwrap();
    env.forge
        .leases
        .acquire(&cx, lease_req(&repo, "src"))
        .await
        .unwrap();
    let ci = Cx::new("ci", PrincipalKind::Service);
    env.forge
        .checks
        .report(
            &ci,
            &repo.org_id,
            &repo.id,
            Report {
                changeset: Some(ObjectId { raw: vec![1; 33] }),
                name: "test".into(),
                status: CheckStatus::Passed,
                ..Report::default()
            },
        )
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    let r = rx.lock().unwrap();
    assert_eq!(r.got.len(), 1, "the lease event is filtered out");
    let (h, body) = &r.got[0];
    assert_eq!(h["x-github-event"], "check_run");
    assert_eq!(h["x-github-hook-installation-target-type"], "repository");
    assert!(verify_signature(
        "k",
        body.as_bytes(),
        &h["x-hub-signature-256"]
    ));
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["action"], "completed");
    assert_eq!(v["check_run"]["name"], "test");
    assert_eq!(v["repository"]["id"], repo.id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_hooks_see_every_repo_of_the_org() {
    let env = common::env().await;
    let repo = repo(&env).await;
    let (url, rx) = receiver().await;
    let cx = owner_cx();
    env.forge
        .webhooks
        .create(
            &cx,
            &HookOwner::org(&repo.org_id),
            NewHook {
                url,
                kinds: vec!["lease.acquired".into()],
                active: true,
                ..NewHook::default()
            },
        )
        .await
        .unwrap();
    env.forge
        .leases
        .acquire(&cx, lease_req(&repo, "src"))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    assert_eq!(rx.lock().unwrap().got.len(), 1);
}
