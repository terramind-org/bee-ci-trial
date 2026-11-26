//! The gateway's metrics (`bee_git_*`, `docs/spec/git-gateway.md` §10) after real `git`
//! traffic, read from the server's `/metrics` and through `OpsService.QueryMetrics`.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::process::Command;

use common::*;
use serde_json::json;

/// The server's Prometheus exposition.
async fn exposition(env: &Env) -> String {
    let url = format!("{}/metrics", env.srv.url);
    let out = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .args(["-sf", &url])
            .output()
            .expect("curl runs")
    })
    .await
    .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The value of the sample `name{labels}`.
fn sample(text: &str, name_and_labels: &str) -> f64 {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{name_and_labels} ")))
        .unwrap_or_else(|| panic!("no sample {name_and_labels}"))
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_metrics_count_requests_protocols_bytes_cache_and_pushes() {
    let env = start_env().await;
    imported(&env, "r").await;
    let a = env.path("a");
    env.git(
        env.dir.path(),
        &[
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            &env.url("r"),
            a.to_str().unwrap(),
        ],
    )
    .await;
    // The same clone again is served from the pack cache, this time in protocol v0.
    env.git(
        env.dir.path(),
        &[
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            &env.url("r"),
            "b",
        ],
    )
    .await;
    env.git(
        env.dir.path(),
        &["-c", "protocol.version=0", "ls-remote", &env.url("r")],
    )
    .await;
    // A push that lands and one that is refused (deleting a protected ref).
    write(&a.join("m.txt"), b"metrics\n");
    env.git(&a, &["add", "-A"]).await;
    env.git(&a, &["commit", "-q", "-m", "metrics"]).await;
    env.git(&a, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    env.git_fails(&a, &["push", "-q", "origin", ":main"]).await;
    // A refused credential.
    env.git_fails(env.dir.path(), &["ls-remote", &env.url_as("nope", "r")])
        .await;
    let t = exposition(&env).await;
    for m in [
        "bee_git_requests_total",
        "bee_git_request_duration_seconds",
        "bee_git_fetch_protocol_total",
        "bee_git_upload_pack_bytes_total",
        "bee_git_receive_pack_bytes_total",
        "bee_git_pack_cache_total",
        "bee_git_push_ref_updates_total",
        "bee_git_lfs_bytes_total",
    ] {
        assert!(t.contains(&format!("# TYPE {m} ")), "{m} is declared\n{t}");
    }
    let req = |service: &str, outcome: &str| {
        sample(
            &t,
            &format!(
                "bee_git_requests_total{{transport=\"http\",service=\"{service}\",outcome=\"{outcome}\"}}"
            ),
        )
    };
    assert!(req("upload-pack", "ok") >= 2.0);
    assert!(req("receive-pack", "ok") >= 1.0);
    assert!(req("info-refs", "ok") >= 4.0);
    assert!(req("info-refs", "error") >= 1.0, "the refused credential");
    assert!(
        sample(
            &t,
            "bee_git_request_duration_seconds_count{transport=\"http\",service=\"upload-pack\"}"
        ) >= 2.0
    );
    assert!(
        sample(
            &t,
            "bee_git_fetch_protocol_total{transport=\"http\",version=\"2\"}"
        ) >= 2.0
    );
    assert!(sample(&t, "bee_git_upload_pack_bytes_total{transport=\"http\"}") > 0.0);
    assert!(sample(&t, "bee_git_receive_pack_bytes_total{transport=\"http\"}") > 0.0);
    assert!(sample(&t, "bee_git_pack_cache_total{result=\"miss\"}") >= 1.0);
    assert!(sample(&t, "bee_git_pack_cache_total{result=\"hit\"}") >= 1.0);
    assert_eq!(
        sample(&t, "bee_git_push_ref_updates_total{result=\"ok\"}"),
        1.0
    );
    assert_eq!(
        sample(&t, "bee_git_push_ref_updates_total{result=\"rejected\"}"),
        1.0
    );
    // The same names through the operator API.
    let m = env
        .rpc(
            "OpsService",
            "QueryMetrics",
            json!({ "name": "bee_git_push_ref_updates_total",
                    "aggregation": "METRIC_AGGREGATION_SUM" }),
        )
        .await
        .expect("query");
    assert_eq!(
        m["series"][0]["samples"][0]["value"].as_f64(),
        Some(2.0),
        "{m}"
    );
    let d = env
        .rpc(
            "OpsService",
            "ListMetricDescriptors",
            json!({ "prefix": "bee_git_" }),
        )
        .await
        .expect("descriptors");
    assert_eq!(d["descriptors"].as_array().unwrap().len(), 8, "{d}");
}

/// `curl` with `args`; stdout.
async fn curl(args: Vec<String>) -> Vec<u8> {
    let out = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .args(&args)
            .output()
            .expect("curl runs")
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "curl failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One LFS batch call for `oid`/`size`; the action's href and headers.
async fn lfs_action(env: &Env, op: &str, oid: &str, size: usize) -> (String, Vec<String>) {
    let body = json!({ "operation": op, "transfers": ["basic"],
                       "objects": [{ "oid": oid, "size": size }] })
    .to_string();
    let resp = curl(vec![
        "-sf".into(),
        "-H".into(),
        format!("Authorization: Bearer {}", env.token),
        "-H".into(),
        "Content-Type: application/vnd.git-lfs+json".into(),
        "--data-binary".into(),
        body,
        format!("http://{}/dev/r.git/info/lfs/objects/batch", env.addr()),
    ])
    .await;
    let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    let action = &v["objects"][0]["actions"][op];
    let headers = action["header"]
        .as_object()
        .map(|h| {
            h.iter()
                .flat_map(|(k, v)| ["-H".to_string(), format!("{k}: {}", v.as_str().unwrap())])
                .collect()
        })
        .unwrap_or_default();
    (
        action["href"].as_str().unwrap_or_default().to_string(),
        headers,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_transfers_are_counted_in_bytes() {
    let env = start_env().await;
    imported(&env, "r").await;
    let data = b"large file content for git lfs\n".repeat(40);
    let file = env.path("lfs-object");
    std::fs::write(&file, &data).unwrap();
    let sum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(&file)
        .output()
        .expect("shasum runs");
    let oid = String::from_utf8_lossy(&sum.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let (href, headers) = lfs_action(&env, "upload", &oid, data.len()).await;
    assert!(!href.is_empty());
    let mut args = vec!["-sf".to_string(), "-X".into(), "PUT".into()];
    args.extend(headers);
    args.extend(["--data-binary".into(), format!("@{}", file.display()), href]);
    curl(args).await;
    let (href, headers) = lfs_action(&env, "download", &oid, data.len()).await;
    let mut args = vec!["-sf".to_string()];
    args.extend(headers);
    args.push(href);
    assert_eq!(curl(args).await, data);
    let t = exposition(&env).await;
    let n = data.len() as f64;
    assert_eq!(
        sample(&t, "bee_git_lfs_bytes_total{direction=\"upload\"}"),
        n
    );
    assert_eq!(
        sample(&t, "bee_git_lfs_bytes_total{direction=\"download\"}"),
        n
    );
    assert!(
        sample(
            &t,
            "bee_git_requests_total{transport=\"http\",service=\"lfs-transfer\",outcome=\"ok\"}"
        ) >= 2.0
    );
    assert!(
        sample(
            &t,
            "bee_git_requests_total{transport=\"http\",service=\"lfs-batch\",outcome=\"ok\"}"
        ) >= 2.0
    );
}
