//! Every RPC of the proto descriptor is served: the test calls each one on the HTTP/JSON
//! surface of a dev server (unary methods as Connect unary, streaming methods as Connect
//! streams) and collects the ones that answer `UNIMPLEMENTED`. The result must equal the
//! documented allow-list (`docs/spec/server.md` §8.3): services other workers are building,
//! and RPCs the forge modules deliberately serve elsewhere.

mod common;

use std::time::Duration;

use bee_proto::methods::MethodDescriptor;
use common::*;
use serde_json::Value;

/// Services that are not registered yet (other tasks own them; they plug in through
/// `ServerOptions::extra`). Every service of the descriptor is registered today.
pub const UNREGISTERED: &[&str] = &[];

/// Single RPCs of registered services that answer `UNIMPLEMENTED` even to an empty request
/// (each one documented in `docs/spec/server.md` §8.3).
pub const UNIMPLEMENTED_RPCS: &[&str] = &[];

fn frame(json: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json);
    out
}

/// True when the call answered a bare `UNIMPLEMENTED` (Connect code `unimplemented` without a
/// subcode). An `UNIMPLEMENTED` with a subcode is a deliberate answer of a served RPC on this
/// deployment (`served_elsewhere`, `no_comment_source`; see the conformance fixtures), not a
/// missing handler.
async fn unimplemented(url: &str, token: &str, path: &str, unary: bool) -> bool {
    let auth = format!("Bearer {token}");
    let (ct, body) = if unary {
        ("application/json", b"{}".to_vec())
    } else {
        ("application/connect+json", frame(b"{}"))
    };
    let target = format!("{url}{path}");
    let headers = [("content-type", ct), ("authorization", auth.as_str())];
    let call = http("POST", &target, &headers, body);
    // Streams that stay open (inbox streams, subscriptions) are served, not unimplemented.
    let Ok((_, _, body)) = tokio::time::timeout(Duration::from_secs(5), call).await else {
        return false;
    };
    let text = String::from_utf8_lossy(&body);
    if unary {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .is_some_and(|v| {
                v["code"] == "unimplemented"
                    && v["details"][0]["debug"]["subcode"]
                        .as_str()
                        .is_none_or(str::is_empty)
            })
    } else {
        text.contains("\"code\":\"unimplemented\"") && !text.contains("\"subcode\":\"")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_rpc_is_served_except_the_documented_allow_list() {
    let t = start_default().await;
    let mut got: Vec<String> = Vec::new();
    // `AuthService.Logout` revokes the calling token, after which every call answers
    // `UNAUTHENTICATED` and an unimplemented RPC would pass for served: call it last.
    let logout = "/bee.v1.AuthService/Logout";
    let (last, rest): (Vec<&MethodDescriptor>, Vec<&MethodDescriptor>) =
        bee_proto::methods::METHODS
            .iter()
            .partition(|m| m.path == logout);
    for m in rest.into_iter().chain(last) {
        let unary = m.kind.is_unary();
        if unimplemented(t.url(), &t.token, m.path, unary).await {
            got.push(m.path.to_string());
        }
    }
    let mut expected: Vec<String> = bee_proto::methods::METHODS
        .iter()
        .filter(|m| UNREGISTERED.contains(&m.service))
        .map(|m| m.path.to_string())
        .chain(UNIMPLEMENTED_RPCS.iter().map(|s| s.to_string()))
        .collect();
    expected.sort();
    got.sort();
    let extra: Vec<&String> = got.iter().filter(|g| !expected.contains(g)).collect();
    let missing: Vec<&String> = expected.iter().filter(|e| !got.contains(e)).collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "unexpected UNIMPLEMENTED: {extra:#?}\nexpected UNIMPLEMENTED but served: {missing:#?}"
    );
    // A service the server does not register still answers UNIMPLEMENTED.
    assert!(unimplemented(t.url(), &t.token, "/bee.v1.NoSuchService/Nope", true).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_services_answer_on_grpc_too() {
    let t = start_default().await;
    let mut c =
        bee_proto::v1::lease_service_client::LeaseServiceClient::new(t.grpc_channel().await);
    let r = c
        .list_leases(t.bearer(
            &t.token,
            bee_proto::v1::ListLeasesRequest {
                repo: prepo(),
                ..Default::default()
            },
        ))
        .await
        .expect("leases on gRPC");
    assert!(r.into_inner().leases.is_empty());
    let mut ch =
        bee_proto::v1::change_service_client::ChangeServiceClient::new(t.grpc_channel().await);
    let list = ch
        .list_changes(t.bearer(
            &t.token,
            bee_proto::v1::ListChangesRequest {
                repo: prepo(),
                ..Default::default()
            },
        ))
        .await
        .expect("changes on gRPC");
    assert!(list.into_inner().changes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_services_trust_a_rotated_signing_key() {
    let t = start_default().await;
    t.call("AdminService", "RotateSigningKey", serde_json::json!({}))
        .await
        .expect("rotate");
    // A root token signed by the new key verifies in the forge too.
    let fresh = t.token_for(&bee_auth::Principal::human(t.dev_user(), "dev"), &[]);
    t.call_as(
        &fresh,
        "IssueService",
        "ListIssues",
        serde_json::json!({ "repo": repo() }),
    )
    .await
    .expect("forge accepts the new key");
}
