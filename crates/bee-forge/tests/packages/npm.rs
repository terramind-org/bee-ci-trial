//! npm registry: publish with a tarball attachment, packument and install metadata, dist-tags,
//! deprecation, unpublish rules.

use bee_forge::packages::hash::{b64_encode, sha1_hex, sha512_b64};
use bytes::Bytes;
use serde_json::{Value, json};

use super::support::*;

fn tarball(name: &str, version: &str) -> Vec<u8> {
    gzip(&tar(&[
        (
            "package/package.json",
            format!("{{\"name\":\"{name}\",\"version\":\"{version}\"}}").as_bytes(),
        ),
        ("package/index.js", b"module.exports = 1;"),
    ]))
}

fn publish_doc(name: &str, version: &str, tag: &str, extra: Value) -> (Value, Vec<u8>) {
    let tgz = tarball(name, version);
    let unscoped = name.rsplit('/').next().unwrap();
    let file = format!("{unscoped}-{version}.tgz");
    let mut v = json!({"name": name, "version": version, "description": "a widget", "dependencies": {"left-pad": "^1.0.0"}});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    let doc = json!({
        "_id": name, "name": name, "dist-tags": {tag: version},
        "versions": {version: v},
        "_attachments": {file: {"content_type": "application/octet-stream", "data": b64_encode(&tgz), "length": tgz.len()}},
    });
    (doc, tgz)
}

async fn put(w: &World, tok: &str, path: &str, doc: &Value) -> http::Response<Bytes> {
    w.http(rq(
        "PUT",
        path,
        Some(tok),
        &[("content-type", "application/json")],
        doc.to_string().into_bytes(),
    ))
    .await
}

#[tokio::test]
async fn publish_then_install_metadata() {
    let w = world().await;
    let tok = w.tok("owner");
    let (mut doc, tgz) = publish_doc(
        "@acme/widget",
        "1.0.0",
        "latest",
        json!({"repository": {"type": "git", "url": "git+https://beehive.test/acme/app.git"}}),
    );
    doc["access"] = json!("restricted");
    let r = put(&w, &tok, "/npm/acme/@acme%2Fwidget", &doc).await;
    assert_eq!(r.status(), 200, "{}", body_str(&r));
    // The repository field linked it to acme/app, so members of the org can read it.
    let reader = w.tok("reader");
    let g = w
        .http(rq(
            "GET",
            "/npm/acme/@acme%2Fwidget",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 200, "{}", body_str(&g));
    let p = body_json(&g);
    assert_eq!(p["dist-tags"]["latest"], "1.0.0");
    let v = &p["versions"]["1.0.0"];
    assert_eq!(v["dist"]["shasum"], sha1_hex(&tgz));
    assert_eq!(
        v["dist"]["integrity"],
        format!("sha512-{}", sha512_b64(&tgz))
    );
    let tarball_url = v["dist"]["tarball"].as_str().unwrap().to_string();
    assert_eq!(
        tarball_url,
        "https://beehive.test/npm/acme/@acme/widget/-/widget-1.0.0.tgz"
    );
    assert_eq!(v["dependencies"]["left-pad"], "^1.0.0");
    // Abbreviated install metadata.
    let a = w
        .http(rq(
            "GET",
            "/npm/acme/@acme%2Fwidget",
            Some(&reader),
            &[("accept", "application/vnd.npm.install-v1+json")],
            Bytes::new(),
        ))
        .await;
    assert_eq!(
        hdr(&a, "content-type"),
        "application/vnd.npm.install-v1+json"
    );
    let a = body_json(&a);
    assert!(a["versions"]["1.0.0"].get("description").is_none());
    assert_eq!(a["versions"]["1.0.0"]["dist"]["shasum"], sha1_hex(&tgz));
    // Install: fetch the tarball by the advertised path (unencoded scope slash).
    let t = w
        .http(rq(
            "GET",
            "/npm/acme/@acme/widget/-/widget-1.0.0.tgz",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(t.status(), 200);
    assert_eq!(t.body().as_ref(), tgz.as_slice());
    // Version and tag documents.
    let one = w
        .http(rq(
            "GET",
            "/npm/acme/@acme%2Fwidget/latest",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&one)["version"], "1.0.0");
    // Publishing over an existing version is forbidden.
    let (again, _) = publish_doc("@acme/widget", "1.0.0", "latest", json!({}));
    let r = put(&w, &tok, "/npm/acme/@acme%2Fwidget", &again).await;
    assert_eq!(r.status(), 403);
    assert!(body_str(&r).contains("version"));
    // A bad attachment length is refused; a missing attachment too.
    let (mut bad, _) = publish_doc("@acme/widget", "1.0.1", "latest", json!({}));
    bad["_attachments"]["widget-1.0.1.tgz"]["length"] = json!(3);
    assert_eq!(
        put(&w, &tok, "/npm/acme/@acme%2Fwidget", &bad)
            .await
            .status(),
        400
    );
    let (mut none, _) = publish_doc("@acme/widget", "1.0.1", "latest", json!({}));
    none["_attachments"] = json!({});
    assert_eq!(
        put(&w, &tok, "/npm/acme/@acme%2Fwidget", &none)
            .await
            .status(),
        400
    );
    // Invalid semver and mismatched names.
    let (bad_ver, _) = publish_doc("@acme/widget", "1.0", "latest", json!({}));
    assert_eq!(
        put(&w, &tok, "/npm/acme/@acme%2Fwidget", &bad_ver)
            .await
            .status(),
        400
    );
    let (other, _) = publish_doc("@acme/other", "1.0.0", "latest", json!({}));
    assert_eq!(
        put(&w, &tok, "/npm/acme/@acme%2Fwidget", &other)
            .await
            .status(),
        400
    );
    // An anonymous caller is asked to authenticate; an outsider sees nothing.
    assert_eq!(
        w.http(rq(
            "GET",
            "/npm/acme/@acme%2Fwidget",
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
    assert_eq!(
        w.http(rq(
            "GET",
            "/npm/acme/@acme%2Fwidget",
            Some(&w.tok("outsider")),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        404
    );
    let who = w
        .http(rq(
            "GET",
            "/npm/acme/-/whoami",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&who)["username"], "owner");
}

#[tokio::test]
async fn dist_tags_deprecate_and_unpublish() {
    let w = world().await;
    let tok = w.tok("owner");
    for (v, tag) in [
        ("1.0.0", "latest"),
        ("1.1.0", "latest"),
        ("2.0.0-beta.1", "next"),
    ] {
        let (doc, _) = publish_doc("plain", v, tag, json!({}));
        assert_eq!(put(&w, &tok, "/npm/acme/plain", &doc).await.status(), 200);
    }
    let p = body_json(
        &w.http(rq("GET", "/npm/acme/plain", Some(&tok), &[], Bytes::new()))
            .await,
    );
    assert_eq!(
        p["dist-tags"],
        json!({"latest": "1.1.0", "next": "2.0.0-beta.1"})
    );
    assert_eq!(p["versions"].as_object().unwrap().len(), 3);
    // dist-tag endpoints.
    let r = w
        .http(rq(
            "PUT",
            "/npm/acme/-/package/plain/dist-tags/stable",
            Some(&tok),
            &[],
            "\"1.0.0\"",
        ))
        .await;
    assert_eq!(r.status(), 200);
    let r = w
        .http(rq(
            "GET",
            "/npm/acme/-/package/plain/dist-tags",
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_json(&r)["stable"], "1.0.0");
    assert_eq!(
        w.http(rq(
            "DELETE",
            "/npm/acme/-/package/plain/dist-tags/stable",
            Some(&tok),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        200
    );
    assert_eq!(
        w.http(rq(
            "DELETE",
            "/npm/acme/-/package/plain/dist-tags/latest",
            Some(&tok),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        400
    );
    // Deprecate: the client PUTs the packument back with a `deprecated` message.
    let mut dep = p.clone();
    dep["versions"]["1.0.0"]["deprecated"] = json!("use 1.1.0");
    dep.as_object_mut().unwrap().remove("_attachments");
    assert_eq!(put(&w, &tok, "/npm/acme/plain", &dep).await.status(), 200);
    let p = body_json(
        &w.http(rq("GET", "/npm/acme/plain", Some(&tok), &[], Bytes::new()))
            .await,
    );
    assert_eq!(p["versions"]["1.0.0"]["deprecated"], "use 1.1.0");
    assert!(p["versions"]["1.1.0"].get("deprecated").is_none());
    // Unpublish a fresh version through the -rev route (allowed inside the 72 hour window).
    let rev = p["_rev"].as_str().unwrap();
    let mut smaller = p.clone();
    smaller["versions"]
        .as_object_mut()
        .unwrap()
        .remove("2.0.0-beta.1");
    smaller["dist-tags"].as_object_mut().unwrap().remove("next");
    assert_eq!(
        put(&w, &tok, &format!("/npm/acme/plain/-rev/{rev}"), &smaller)
            .await
            .status(),
        200
    );
    let p = body_json(
        &w.http(rq("GET", "/npm/acme/plain", Some(&tok), &[], Bytes::new()))
            .await,
    );
    assert!(p["versions"].get("2.0.0-beta.1").is_none());
    // A republish of the removed version stays refused: npm never reuses a version.
    let (doc, _) = publish_doc("plain", "2.0.0-beta.1", "next", json!({}));
    assert_eq!(put(&w, &tok, "/npm/acme/plain", &doc).await.status(), 403);
    // Old and popular: unpublish is refused. Age the version and count 300 downloads.
    w.clock.advance(73 * 3_600_000);
    let tok = w.tok("owner");
    for _ in 0..300 {
        let t = w
            .http(rq(
                "GET",
                "/npm/acme/plain/-/plain-1.1.0.tgz",
                Some(&tok),
                &[],
                Bytes::new(),
            ))
            .await;
        assert_eq!(t.status(), 200);
    }
    let rev = body_json(
        &w.http(rq("GET", "/npm/acme/plain", Some(&tok), &[], Bytes::new()))
            .await,
    )["_rev"]
        .as_str()
        .unwrap()
        .to_string();
    let mut gone = p.clone();
    gone["versions"].as_object_mut().unwrap().remove("1.1.0");
    let r = put(&w, &tok, &format!("/npm/acme/plain/-rev/{rev}"), &gone).await;
    assert_eq!(r.status(), 403, "{}", body_str(&r));
    assert!(body_str(&r).contains("deprecate"));
    // The unpopular old 1.0.0 can still go.
    let mut gone = p.clone();
    gone["versions"].as_object_mut().unwrap().remove("1.0.0");
    assert_eq!(
        put(&w, &tok, &format!("/npm/acme/plain/-rev/{rev}"), &gone)
            .await
            .status(),
        200
    );
    // Whole-package unpublish is refused while a popular version remains.
    assert_eq!(
        w.http(rq(
            "DELETE",
            &format!("/npm/acme/plain/-rev/{rev}"),
            Some(&tok),
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        403
    );
}

#[tokio::test]
async fn unlinked_publish_needs_org_admin() {
    let w = world().await;
    let (doc, _) = publish_doc("lib", "1.0.0", "latest", json!({}));
    assert_eq!(
        put(&w, &w.tok("writer"), "/npm/acme/lib", &doc)
            .await
            .status(),
        403
    );
    assert_eq!(
        put(&w, &w.tok("admin"), "/npm/acme/lib", &doc)
            .await
            .status(),
        200
    );
    // The job token of repo app cannot create an unlinked package.
    let (doc2, _) = publish_doc("lib2", "1.0.0", "latest", json!({}));
    assert_eq!(
        put(&w, &w.job_token(&w.app), "/npm/acme/lib2", &doc2)
            .await
            .status(),
        403
    );
    // With a repository field naming its repo it can.
    let (doc3, _) = publish_doc(
        "lib3",
        "1.0.0",
        "latest",
        json!({"repository": "https://beehive.test/acme/app"}),
    );
    assert_eq!(
        put(&w, &w.job_token(&w.app), "/npm/acme/lib3", &doc3)
            .await
            .status(),
        200
    );
}
