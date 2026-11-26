//! Cargo sparse registry: config, publish, index files, yank, download.

use bytes::Bytes;
use serde_json::{Value, json};

use super::support::*;

fn publish_body(meta: &Value, krate: &[u8]) -> Vec<u8> {
    let j = meta.to_string().into_bytes();
    let mut out = (j.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&j);
    out.extend_from_slice(&(krate.len() as u32).to_le_bytes());
    out.extend_from_slice(krate);
    out
}

fn meta(name: &str, vers: &str) -> Value {
    json!({
        "name": name, "vers": vers, "description": "demo", "license": "MIT",
        "repository": "https://beehive.test/acme/app",
        "deps": [
            {"name": "serde", "version_req": "^1", "features": ["derive"], "optional": false, "default_features": true, "target": null, "kind": "normal", "registry": null},
            {"name": "serde_json", "version_req": "^1", "features": [], "optional": true, "default_features": true, "target": null, "kind": "dev", "explicit_name_in_toml": "sj"},
        ],
        "features": {"default": [], "extra": ["dep:serde_json"]},
    })
}

#[tokio::test]
async fn publish_index_yank_and_download() {
    let w = world().await;
    let tok = w.tok("owner");
    let cfg = w
        .http(rq(
            "GET",
            "/cargo/acme/index/config.json",
            None,
            &[],
            Bytes::new(),
        ))
        .await;
    let cfg = body_json(&cfg);
    assert_eq!(
        cfg["dl"],
        "https://beehive.test/cargo/acme/api/v1/crates/{crate}/{version}/download"
    );
    assert_eq!(cfg["api"], "https://beehive.test/cargo/acme");
    assert_eq!(cfg["auth-required"], true);
    // Cargo sends the bare token in `Authorization`.
    let krate = gzip(&tar(&[("my_crate-0.1.0/Cargo.toml", b"[package]")]));
    let r = w
        .http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/new",
            None,
            &[("authorization", &tok)],
            publish_body(&meta("my_crate", "0.1.0"), &krate),
        ))
        .await;
    assert_eq!(r.status(), 200, "{}", body_str(&r));
    assert!(body_json(&r)["warnings"]["other"].is_array());
    let r = w
        .http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/new",
            None,
            &[("authorization", &tok)],
            publish_body(&meta("my_crate", "0.1.0"), &krate),
        ))
        .await;
    assert_eq!(r.status(), 409);
    assert!(
        body_json(&r)["errors"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("version")
    );
    let krate2 = gzip(&tar(&[("my_crate-0.2.0/Cargo.toml", b"[package] v2")]));
    assert_eq!(
        w.http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/new",
            None,
            &[("authorization", &tok)],
            publish_body(&meta("my_crate", "0.2.0"), &krate2)
        ))
        .await
        .status(),
        200
    );
    // Truncated bodies and bad names are refused.
    assert_eq!(
        w.http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/new",
            None,
            &[("authorization", &tok)],
            vec![1u8, 0, 0]
        ))
        .await
        .status(),
        400
    );
    assert_eq!(
        w.http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/new",
            None,
            &[("authorization", &tok)],
            publish_body(&meta("1bad", "0.1.0"), &krate)
        ))
        .await
        .status(),
        400
    );
    // The sparse index file for `my_crate` lives at my/_c/my_crate: two + two letters.
    let reader = w.tok("reader");
    let idx = w
        .http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[("authorization", &reader)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(idx.status(), 200, "{}", body_str(&idx));
    let lines: Vec<Value> = body_str(&idx)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["name"], "my_crate");
    assert_eq!(lines[0]["vers"], "0.1.0");
    assert_eq!(
        lines[0]["cksum"],
        sha256_digest(&krate).trim_start_matches("sha256:")
    );
    assert_eq!(lines[0]["yanked"], false);
    assert_eq!(lines[0]["deps"][0]["name"], "serde");
    assert_eq!(lines[0]["deps"][0]["req"], "^1");
    assert_eq!(lines[0]["deps"][0]["features"], json!(["derive"]));
    // A renamed dependency keeps its local name and records the real package.
    assert_eq!(lines[0]["deps"][1]["name"], "sj");
    assert_eq!(lines[0]["deps"][1]["package"], "serde_json");
    assert_eq!(lines[0]["deps"][1]["kind"], "dev");
    // `dep:` features move to features2 with index version 2.
    assert_eq!(lines[0]["v"], 2);
    assert_eq!(lines[0]["features2"]["extra"], json!(["dep:serde_json"]));
    // ETag revalidation.
    let etag = hdr(&idx, "etag").to_string();
    let nm = w
        .http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[("authorization", &reader), ("if-none-match", &etag)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(nm.status(), 304);
    // Wrong layout path is a 404.
    assert_eq!(
        w.http(rq(
            "GET",
            "/cargo/acme/index/xx/yy/my_crate",
            None,
            &[("authorization", &reader)],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    // Download, counted.
    let d = w
        .http(rq(
            "GET",
            "/cargo/acme/api/v1/crates/my_crate/0.1.0/download",
            None,
            &[("authorization", &reader)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(d.status(), 200);
    assert_eq!(d.body().as_ref(), krate.as_slice());
    use bee_proto::v1::package_service_server::PackageService;
    let u =
        w.s.get_package_usage(req(
            &tok,
            GetPackageUsageRequest {
                owner: "acme".into(),
                ecosystem: 6,
                name: "my_crate".into(),
                since: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u.download_count, 1);
    // Yank and unyank.
    let y = w
        .http(rq(
            "DELETE",
            "/cargo/acme/api/v1/crates/my_crate/0.1.0/yank",
            None,
            &[("authorization", &tok)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&y)["ok"], true);
    let idx = body_str(
        &w.http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[("authorization", &reader)],
            Bytes::new(),
        ))
        .await,
    );
    let first: Value = serde_json::from_str(idx.lines().next().unwrap()).unwrap();
    assert_eq!(first["yanked"], true);
    // Yanked crates stay downloadable for existing lockfiles.
    assert_eq!(
        w.http(rq(
            "GET",
            "/cargo/acme/api/v1/crates/my_crate/0.1.0/download",
            None,
            &[("authorization", &reader)],
            Bytes::new()
        ))
        .await
        .status(),
        200
    );
    let y = w
        .http(rq(
            "PUT",
            "/cargo/acme/api/v1/crates/my_crate/0.1.0/unyank",
            None,
            &[("authorization", &tok)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(y.status(), 200);
    let idx = body_str(
        &w.http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[("authorization", &reader)],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(
        serde_json::from_str::<Value>(idx.lines().next().unwrap()).unwrap()["yanked"],
        false
    );
    // Permissions: readers cannot yank or publish, outsiders see nothing, no token is 401.
    assert_eq!(
        w.http(rq(
            "DELETE",
            "/cargo/acme/api/v1/crates/my_crate/0.1.0/yank",
            None,
            &[("authorization", &reader)],
            Bytes::new()
        ))
        .await
        .status(),
        403
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[("authorization", &w.tok("outsider"))],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/cargo/acme/index/my/_c/my_crate",
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
    // Search.
    let s = body_json(
        &w.http(rq(
            "GET",
            "/cargo/acme/api/v1/crates?q=my",
            None,
            &[("authorization", &reader)],
            Bytes::new(),
        ))
        .await,
    );
    assert_eq!(s["meta"]["total"], 1);
}
