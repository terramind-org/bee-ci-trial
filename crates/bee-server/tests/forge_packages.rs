//! The package registries mounted on the running dev server: an OCI image (blobs and a
//! manifest) and an npm package are published over HTTP, then pulled back.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    format!("sha256:{:x}", sha2::Sha256::digest(data))
}

async fn req(
    t: &TestServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, http::HeaderMap, Vec<u8>) {
    let auth = format!("Bearer {}", t.token);
    let mut h: Vec<(&str, &str)> = vec![("authorization", auth.as_str())];
    h.extend_from_slice(headers);
    http(method, &format!("{}{path}", t.url()), &h, body).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oci_image_and_an_npm_package_publish_and_pull_over_http() {
    let t = start_default().await;

    // OCI: `/v2/` challenges anonymous clients and answers authenticated ones.
    let (st, h, _) = http("GET", &format!("{}/v2/", t.url()), &[], Vec::new()).await;
    assert_eq!(st, 401);
    assert!(h.contains_key("www-authenticate"));
    let (st, _, _) = req(&t, "GET", "/v2/", &[], Vec::new()).await;
    assert_eq!(st, 200);
    let config = br#"{"architecture":"arm64","os":"linux"}"#.to_vec();
    let layer = b"layer bytes ".repeat(100);
    for blob in [&config, &layer] {
        let path = format!("/v2/dev/app/blobs/uploads/?digest={}", sha256(blob));
        let (st, _, body) = req(&t, "POST", &path, &[], blob.to_vec()).await;
        assert_eq!(st, 201, "{}", String::from_utf8_lossy(&body));
    }
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": sha256(&config), "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": sha256(&layer), "size": layer.len()}],
    }))
    .unwrap();
    let (st, _, body) = req(
        &t,
        "PUT",
        "/v2/dev/app/manifests/v1.0",
        &[("content-type", "application/vnd.oci.image.manifest.v1+json")],
        manifest.clone(),
    )
    .await;
    assert_eq!(st, 201, "{}", String::from_utf8_lossy(&body));
    let (st, h, body) = req(
        &t,
        "GET",
        "/v2/dev/app/manifests/v1.0",
        &[("accept", "application/vnd.oci.image.manifest.v1+json")],
        Vec::new(),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(body, manifest);
    assert_eq!(
        h.get("docker-content-digest").and_then(|v| v.to_str().ok()),
        Some(sha256(&manifest).as_str())
    );
    let (st, _, body) = req(
        &t,
        "GET",
        &format!("/v2/dev/app/blobs/{}", sha256(&layer)),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!((st, body), (200, layer.clone()));
    let (_, _, tags) = req(&t, "GET", "/v2/dev/app/tags/list", &[], Vec::new()).await;
    let tags: Value = serde_json::from_slice(&tags).unwrap();
    assert_eq!(tags["tags"], json!(["v1.0"]));

    // npm: publish, then read the packument and the tarball.
    let tgz = bee_forge::packages::archive::gzip(b"package/index.js\0module.exports = 1;\n");
    let doc = json!({
        "_id": "widget", "name": "widget", "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "widget", "version": "1.0.0", "description": "a widget"}},
        "_attachments": {"widget-1.0.0.tgz": {
            "content_type": "application/octet-stream",
            "data": B64.encode(&tgz), "length": tgz.len(),
        }},
    });
    let (st, _, body) = req(
        &t,
        "PUT",
        "/npm/dev/widget",
        &[("content-type", "application/json")],
        doc.to_string().into_bytes(),
    )
    .await;
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    let (st, _, body) = req(&t, "GET", "/npm/dev/widget", &[], Vec::new()).await;
    assert_eq!(st, 200);
    let pk: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(pk["dist-tags"]["latest"], "1.0.0");
    let tarball = pk["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .expect("tarball url")
        .to_string();
    let path = tarball.trim_start_matches(t.url());
    let (st, _, body) = req(&t, "GET", path, &[], Vec::new()).await;
    assert_eq!((st, body), (200, tgz));
    // The packages are forge records too.
    let l = t
        .call("PackageService", "ListPackages", json!({ "owner": "dev" }))
        .await
        .expect("list packages");
    let names: Vec<&str> = l["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"app") && names.contains(&"widget"), "{l}");
}

/// Regression (three-form conformance): the upload URLs `BeginPackageUpload` hands out are
/// served, so a native version can be published and its files fetched back; a tampered
/// signature is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_package_uploads_go_to_the_signed_urls() {
    let t = start_default().await;
    let begin = t
        .call(
            "PackageService",
            "BeginPackageUpload",
            json!({"owner": "dev", "ecosystem": "PACKAGE_ECOSYSTEM_NPM", "name": "signed-upload",
                   "version": "1.0.0", "files": [{"name": "signed-upload-1.0.0.tgz", "sizeBytes": "5",
                   "digest": sha256(b"hello"), "mediaType": "application/octet-stream"}]}),
        )
        .await
        .expect("begin");
    let url = begin["targets"][0]["uploadUrl"]
        .as_str()
        .expect("url")
        .to_string();
    let (st, _, _) = http("PUT", &format!("{url}0"), &[], b"hello".to_vec()).await;
    assert_eq!(st, 403, "a tampered signature is refused");
    let (st, _, body) = http("PUT", &url, &[], b"hello".to_vec()).await;
    assert_eq!(st, 201, "{}", String::from_utf8_lossy(&body));
    let v = t
        .call(
            "PackageService",
            "FinalizePackageVersion",
            json!({"owner": "dev", "uploadId": begin["uploadId"]}),
        )
        .await
        .expect("finalize after the upload");
    assert_eq!(v["version"], "1.0.0", "{v}");
}
