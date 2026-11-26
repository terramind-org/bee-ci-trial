//! OCI distribution: multi-arch push and pull, chunked uploads, mounts, referrers, tags,
//! deletion and digest verification.

use bytes::Bytes;
use serde_json::{Value, json};

use super::support::*;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// Pushes a blob in two chunks; returns its digest.
async fn push_blob(
    w: &World,
    tok: &str,
    name: &str,
    data: &[u8],
    extra: &[(&str, &str)],
) -> String {
    let start = w
        .http(rq(
            "POST",
            &format!("/v2/{name}/blobs/uploads/"),
            Some(tok),
            extra,
            Bytes::new(),
        ))
        .await;
    assert_eq!(start.status(), 202, "{}", body_str(&start));
    let loc = hdr(&start, "location").to_string();
    let mid = data.len() / 2;
    let p1 = w
        .http(rq(
            "PATCH",
            &loc,
            Some(tok),
            &[("content-range", &format!("0-{}", mid - 1))],
            data[..mid].to_vec(),
        ))
        .await;
    assert_eq!(p1.status(), 202);
    assert_eq!(hdr(&p1, "range"), format!("0-{}", mid - 1));
    // An out-of-order chunk is refused.
    let bad = w
        .http(rq(
            "PATCH",
            &loc,
            Some(tok),
            &[("content-range", "999-1000")],
            vec![1u8, 2],
        ))
        .await;
    assert_eq!(bad.status(), 416);
    let p2 = w
        .http(rq(
            "PATCH",
            &loc,
            Some(tok),
            &[("content-range", &format!("{mid}-{}", data.len() - 1))],
            data[mid..].to_vec(),
        ))
        .await;
    assert_eq!(p2.status(), 202);
    let digest = sha256_digest(data);
    let done = w
        .http(rq(
            "PUT",
            &format!("{loc}?digest={digest}"),
            Some(tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(done.status(), 201, "{}", body_str(&done));
    assert_eq!(hdr(&done, "docker-content-digest"), digest);
    digest
}

struct Image {
    manifest: Vec<u8>,
    digest: String,
}

async fn push_image(w: &World, tok: &str, name: &str, arch: &str, extra: &[(&str, &str)]) -> Image {
    let config = json!({"architecture": arch, "os": "linux", "config": {"Labels": {}}})
        .to_string()
        .into_bytes();
    let layer = format!("layer for {arch} ").repeat(50).into_bytes();
    let cd = push_blob(w, tok, name, &config, extra).await;
    let ld = push_blob(w, tok, name, &layer, extra).await;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": ld, "size": layer.len()}],
    }))
    .unwrap();
    let digest = sha256_digest(&manifest);
    let r = w
        .http(rq(
            "PUT",
            &format!("/v2/{name}/manifests/{digest}"),
            Some(tok),
            &[("content-type", OCI_MANIFEST)],
            manifest.clone(),
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    Image { manifest, digest }
}

#[tokio::test]
async fn multi_arch_index_pushes_and_pulls() {
    let w = world().await;
    let tok = w.tok("owner");
    let amd = push_image(&w, &tok, "acme/app", "amd64", &[]).await;
    let arm = push_image(&w, &tok, "acme/app", "arm64", &[]).await;
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": OCI_INDEX,
        "manifests": [
            {"mediaType": OCI_MANIFEST, "digest": amd.digest, "size": amd.manifest.len(), "platform": {"architecture": "amd64", "os": "linux"}},
            {"mediaType": OCI_MANIFEST, "digest": arm.digest, "size": arm.manifest.len(), "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}},
        ],
    }))
    .unwrap();
    let idx_digest = sha256_digest(&index);
    // An index naming an unknown child is refused.
    let missing = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": OCI_INDEX,
        "manifests": [{"mediaType": OCI_MANIFEST, "digest": sha256_digest(b"nope"), "size": 4}],
    }))
    .unwrap();
    let r = w
        .http(rq(
            "PUT",
            "/v2/acme/app/manifests/bad",
            Some(&tok),
            &[("content-type", OCI_INDEX)],
            missing,
        ))
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(body_json(&r)["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
    // A wrong digest reference is refused.
    let r = w
        .http(rq(
            "PUT",
            &format!("/v2/acme/app/manifests/{}", sha256_digest(b"other")),
            Some(&tok),
            &[("content-type", OCI_INDEX)],
            index.clone(),
        ))
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(body_json(&r)["errors"][0]["code"], "DIGEST_INVALID");
    // The real push, by tag.
    let r = w
        .http(rq(
            "PUT",
            "/v2/acme/app/manifests/v1.0",
            Some(&tok),
            &[("content-type", OCI_INDEX)],
            index.clone(),
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    assert_eq!(hdr(&r, "docker-content-digest"), idx_digest);
    // Pull by tag and by digest; the platform manifests resolve.
    let g = w
        .http(rq(
            "GET",
            "/v2/acme/app/manifests/v1.0",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 200);
    assert_eq!(hdr(&g, "content-type"), OCI_INDEX);
    assert_eq!(g.body().as_ref(), index.as_slice());
    let doc: Value = body_json(&g);
    let child = doc["manifests"][1]["digest"].as_str().unwrap().to_string();
    let c = w
        .http(rq(
            "GET",
            &format!("/v2/acme/app/manifests/{child}"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(c.status(), 200);
    assert_eq!(hdr(&c, "docker-content-digest"), arm.digest);
    let m: Value = body_json(&c);
    let layer_digest = m["layers"][0]["digest"].as_str().unwrap();
    let b = w
        .http(rq(
            "GET",
            &format!("/v2/acme/app/blobs/{layer_digest}"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(b.status(), 200);
    assert_eq!(sha256_digest(b.body()), layer_digest);
    let h = w
        .http(rq(
            "HEAD",
            &format!("/v2/acme/app/blobs/{layer_digest}"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(h.status(), 200);
    assert_eq!(hdr(&h, "content-length"), b.body().len().to_string());
    // Tags list and pagination.
    let _ = w
        .http(rq(
            "PUT",
            "/v2/acme/app/manifests/latest",
            Some(&tok),
            &[("content-type", OCI_INDEX)],
            index.clone(),
        ))
        .await;
    let t = w
        .http(rq(
            "GET",
            "/v2/acme/app/tags/list",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&t)["tags"], json!(["latest", "v1.0"]));
    let t = w
        .http(rq(
            "GET",
            "/v2/acme/app/tags/list?n=1",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&t)["tags"], json!(["latest"]));
    assert!(hdr(&t, "link").contains("last=latest"));
    let t = w
        .http(rq(
            "GET",
            "/v2/acme/app/tags/list?last=latest",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&t)["tags"], json!(["v1.0"]));
    // The package model shows one index with two platforms and downloads were counted.
    let v = w.s.clone();
    use bee_proto::v1::package_service_server::PackageService;
    let got = v
        .get_package_version(req(
            &tok,
            GetPackageVersionRequest {
                owner: "acme".into(),
                ecosystem: PackageEcosystem::Container as i32,
                name: "app".into(),
                version: "v1.0".into(),
                include_download_urls: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.platforms.len(), 2);
    assert_eq!(got.tags, vec!["latest".to_string(), "v1.0".to_string()]);
    assert_eq!(got.download_count, 1);
}

#[tokio::test]
async fn auth_challenges_and_visibility() {
    let w = world().await;
    let owner = w.tok("owner");
    let r = w.http(rq("GET", "/v2/", None, &[], Bytes::new())).await;
    assert_eq!(r.status(), 401);
    assert!(hdr(&r, "www-authenticate").starts_with("Basic"));
    assert_eq!(body_json(&r)["errors"][0]["code"], "UNAUTHORIZED");
    let r = w
        .http(rq("GET", "/v2/", Some(&owner), &[], Bytes::new()))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "docker-distribution-api-version"), "registry/2.0");
    // Docker logs in with HTTP Basic: the token is the password.
    let basic = format!(
        "Basic {}",
        bee_forge::packages::hash::b64_encode(format!("owner:{owner}").as_bytes())
    );
    let r = w
        .http(rq(
            "GET",
            "/v2/",
            None,
            &[("authorization", &basic)],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 200);

    let img = push_image(&w, &owner, "acme/app", "amd64", &[]).await;
    let u = format!("/v2/acme/app/manifests/{}", img.digest);
    // Anonymous and unprivileged callers cannot read a private package.
    assert_eq!(
        w.http(rq("GET", &u, None, &[], Bytes::new()))
            .await
            .status(),
        401
    );
    let outsider = w.tok("outsider");
    assert_eq!(
        w.http(rq("GET", &u, Some(&outsider), &[], Bytes::new()))
            .await
            .status(),
        404
    );
    // A member without a role on the unlinked package is hidden from it too.
    let reader = w.tok("reader");
    assert_eq!(
        w.http(rq("GET", &u, Some(&reader), &[], Bytes::new()))
            .await
            .status(),
        404
    );
    // A writer cannot create packages under the org, and cannot push to this one.
    let writer = w.tok("writer");
    let r = w
        .http(rq(
            "POST",
            "/v2/acme/fresh/blobs/uploads/",
            Some(&writer),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 403);
    let r = w
        .http(rq(
            "POST",
            "/v2/acme/app/blobs/uploads/",
            Some(&writer),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 404);
    // Making it public lets anyone pull.
    use bee_proto::v1::package_service_server::PackageService;
    w.s.set_package_visibility(req(
        &owner,
        SetPackageVisibilityRequest {
            owner: "acme".into(),
            ecosystem: PackageEcosystem::Container as i32,
            name: "app".into(),
            visibility: PackageVisibility::Public as i32,
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.http(rq("GET", &u, None, &[], Bytes::new()))
            .await
            .status(),
        200
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/v2/nosuch/app/tags/list",
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
}

#[tokio::test]
async fn mounting_deleting_and_referrers() {
    let w = world().await;
    let tok = w.tok("owner");
    let amd = push_image(&w, &tok, "acme/app", "amd64", &[]).await;
    let m: Value = serde_json::from_slice(&amd.manifest).unwrap();
    let layer = m["layers"][0]["digest"].as_str().unwrap().to_string();
    // Cross-repo mount: no upload needed.
    let r = w
        .http(rq(
            "POST",
            &format!("/v2/acme/other/blobs/uploads/?mount={layer}&from=acme/app"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    assert_eq!(hdr(&r, "location"), format!("/v2/acme/other/blobs/{layer}"));
    assert_eq!(
        w.http(rq(
            "HEAD",
            &format!("/v2/acme/other/blobs/{layer}"),
            Some(&tok),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        200
    );
    // Mounting a blob the source lacks falls back to a normal upload session.
    let r = w
        .http(rq(
            "POST",
            &format!(
                "/v2/acme/other/blobs/uploads/?mount={}&from=acme/app",
                sha256_digest(b"zzz")
            ),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 202);
    // Monolithic upload with ?digest=.
    let data = b"monolithic blob".to_vec();
    let d = sha256_digest(&data);
    let r = w
        .http(rq(
            "POST",
            &format!("/v2/acme/other/blobs/uploads/?digest={d}"),
            Some(&tok),
            &[],
            data.clone(),
        ))
        .await;
    assert_eq!(r.status(), 201);
    let bad = w
        .http(rq(
            "POST",
            &format!(
                "/v2/acme/other/blobs/uploads/?digest={}",
                sha256_digest(b"x")
            ),
            Some(&tok),
            &[],
            data,
        ))
        .await;
    assert_eq!(bad.status(), 400);
    // Referrers: an artifact manifest whose subject is the image.
    let empty = b"{}".to_vec();
    let ed = push_blob(
        &w,
        &tok,
        "acme/app",
        &[empty.clone(), vec![b' ', b' ']].concat(),
        &[],
    )
    .await;
    let sbom = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST, "artifactType": "application/spdx+json",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": ed, "size": empty.len() + 2},
        "layers": [], "subject": {"mediaType": OCI_MANIFEST, "digest": amd.digest, "size": amd.manifest.len()},
    }))
    .unwrap();
    let sd = sha256_digest(&sbom);
    let r = w
        .http(rq(
            "PUT",
            &format!("/v2/acme/app/manifests/{sd}"),
            Some(&tok),
            &[("content-type", OCI_MANIFEST)],
            sbom,
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    assert_eq!(hdr(&r, "oci-subject"), amd.digest);
    let refs = w
        .http(rq(
            "GET",
            &format!("/v2/acme/app/referrers/{}", amd.digest),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(refs.status(), 200);
    assert_eq!(
        body_json(&refs)["manifests"][0]["artifactType"],
        "application/spdx+json"
    );
    let none = w
        .http(rq(
            "GET",
            &format!(
                "/v2/acme/app/referrers/{}?artifactType=other/type",
                amd.digest
            ),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&none)["manifests"].as_array().unwrap().len(), 0);
    assert_eq!(hdr(&none, "oci-filters-applied"), "artifactType");
    // Delete by digest: gone, and the referrer index is empty again after deleting the sbom.
    let r = w
        .http(rq(
            "DELETE",
            &format!("/v2/acme/app/manifests/{sd}"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 202);
    let refs = w
        .http(rq(
            "GET",
            &format!("/v2/acme/app/referrers/{}", amd.digest),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&refs)["manifests"].as_array().unwrap().len(), 0);
    let r = w
        .http(rq(
            "DELETE",
            &format!("/v2/acme/app/manifests/{}", amd.digest),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 202);
    let g = w
        .http(rq(
            "GET",
            &format!("/v2/acme/app/manifests/{}", amd.digest),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 404);
    assert_eq!(body_json(&g)["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

#[tokio::test]
async fn workflow_job_token_pushes_to_a_linked_repo() {
    let w = world().await;
    let job = w.job_token(&w.app);
    // Without the repo hint a restricted token cannot create the package.
    let r = w
        .http(rq(
            "POST",
            "/v2/acme/svc/blobs/uploads/",
            Some(&job),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 403);
    let hint = [("x-bee-repo", "acme/app")];
    let img = push_image(&w, &job, "acme/svc", "amd64", &hint).await;
    // The package is linked, inherits repo access: a member of the org can pull it.
    let reader = w.tok("reader");
    let g = w
        .http(rq(
            "GET",
            &format!("/v2/acme/svc/manifests/{}", img.digest),
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 200);
    // An outsider cannot.
    let g = w
        .http(rq(
            "GET",
            &format!("/v2/acme/svc/manifests/{}", img.digest),
            Some(&w.tok("outsider")),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 404);
    // The job token cannot touch a package linked to another repo.
    let other = w.job_token(&w.open);
    let r = w
        .http(rq(
            "POST",
            "/v2/acme/svc/blobs/uploads/",
            Some(&other),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn image_source_label_links_the_package() {
    let w = world().await;
    let tok = w.tok("owner");
    let config = json!({"architecture": "amd64", "os": "linux", "config": {"Labels": {"org.opencontainers.image.source": "https://beehive.test/acme/app"}}})
        .to_string()
        .into_bytes();
    let cd = push_blob(&w, &tok, "acme/lbl", &config, &[]).await;
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": config.len()},
        "layers": [],
    }))
    .unwrap();
    let r = w
        .http(rq(
            "PUT",
            "/v2/acme/lbl/manifests/latest",
            Some(&tok),
            &[("content-type", OCI_MANIFEST)],
            manifest,
        ))
        .await;
    assert_eq!(r.status(), 201);
    use bee_proto::v1::package_service_server::PackageService;
    let p =
        w.s.get_package(req(
            &tok,
            GetPackageRequest {
                owner: "acme".into(),
                ecosystem: PackageEcosystem::Container as i32,
                name: "lbl".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(p.inherit_access_from_repo);
    assert!(matches!(p.repo.unwrap().repo, Some(repo_ref::Repo::FullName(n)) if n == "acme/app"));
}
