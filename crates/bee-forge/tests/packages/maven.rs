//! Maven repository layout: deploy, resolve, checksums, generated metadata, snapshots.

use bee_forge::packages::hash::{md5_hex, sha1_hex, sha256_hex};
use bytes::Bytes;

use super::support::*;

async fn put(w: &World, tok: &str, path: &str, body: &[u8]) -> http::Response<Bytes> {
    w.http(rq(
        "PUT",
        path,
        Some(tok),
        &[("x-bee-repo", "acme/app")],
        body.to_vec(),
    ))
    .await
}

#[tokio::test]
async fn deploy_resolve_and_metadata() {
    let w = world().await;
    let tok = w.tok("owner");
    let base = "/maven/acme/com/acme/lib";
    let jar = b"jar-bytes-1.0".to_vec();
    for v in ["1.0", "1.1", "1.10", "1.2-SNAPSHOT"] {
        let jar = format!("jar for {v}").into_bytes();
        assert_eq!(
            put(&w, &tok, &format!("{base}/{v}/lib-{v}.jar"), &jar)
                .await
                .status(),
            201
        );
        assert_eq!(
            put(
                &w,
                &tok,
                &format!("{base}/{v}/lib-{v}.jar.sha1"),
                sha1_hex(&jar).as_bytes()
            )
            .await
            .status(),
            201
        );
        assert_eq!(
            put(&w, &tok, &format!("{base}/{v}/lib-{v}.pom"), b"<project/>")
                .await
                .status(),
            201
        );
        assert_eq!(
            put(
                &w,
                &tok,
                &format!("{base}/{v}/lib-{v}.pom.md5"),
                md5_hex(b"<project/>").as_bytes()
            )
            .await
            .status(),
            201
        );
    }
    // A wrong checksum is refused.
    let r = put(&w, &tok, &format!("{base}/1.0/lib-1.0.jar.sha1"), b"0000").await;
    assert_eq!(r.status(), 400);
    // Resolve the artifact, its checksums (stored and computed) and HEAD.
    let jar10 = b"jar for 1.0".to_vec();
    let g = w
        .http(rq(
            "GET",
            &format!("{base}/1.0/lib-1.0.jar"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(g.status(), 200);
    assert_eq!(g.body().as_ref(), jar10.as_slice());
    for (ext, want) in [
        ("sha1", sha1_hex(&jar10)),
        ("md5", md5_hex(&jar10)),
        ("sha256", sha256_hex(&jar10)),
    ] {
        let c = w
            .http(rq(
                "GET",
                &format!("{base}/1.0/lib-1.0.jar.{ext}"),
                Some(&tok),
                &[],
                Bytes::new(),
            ))
            .await;
        assert_eq!(body_str(&c), want, "{ext}");
    }
    let h = w
        .http(rq(
            "HEAD",
            &format!("{base}/1.0/lib-1.0.jar"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(hdr(&h, "content-length"), jar10.len().to_string());
    // Releases are immutable, identical re-deploys are accepted.
    assert_eq!(
        put(&w, &tok, &format!("{base}/1.0/lib-1.0.jar"), &jar10)
            .await
            .status(),
        201
    );
    assert_eq!(
        put(&w, &tok, &format!("{base}/1.0/lib-1.0.jar"), &jar)
            .await
            .status(),
        409
    );
    // Snapshots can be overwritten.
    assert_eq!(
        put(
            &w,
            &tok,
            &format!("{base}/1.2-SNAPSHOT/lib-1.2-SNAPSHOT.jar"),
            b"newer"
        )
        .await
        .status(),
        201
    );
    // maven-metadata.xml at the artifact level: versions in Maven order, latest and release.
    let m = w
        .http(rq(
            "GET",
            &format!("{base}/maven-metadata.xml"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(m.status(), 200);
    let xml = body_str(&m);
    let doc = roxmltree::Document::parse(&xml).unwrap();
    let text = |tag: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name(tag))
            .and_then(|n| n.text())
            .unwrap()
            .to_string()
    };
    assert_eq!(text("groupId"), "com.acme");
    assert_eq!(text("artifactId"), "lib");
    assert_eq!(text("latest"), "1.10");
    assert_eq!(text("release"), "1.10");
    let versions: Vec<&str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("version"))
        .filter_map(|n| n.text())
        .collect();
    assert_eq!(versions, ["1.0", "1.1", "1.2-SNAPSHOT", "1.10"]);
    assert_eq!(text("lastUpdated").len(), 14);
    let sum = w
        .http(rq(
            "GET",
            &format!("{base}/maven-metadata.xml.sha1"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(body_str(&sum), sha1_hex(xml.as_bytes()));
    // The org's reader can resolve (package linked to repo acme/app by the hint).
    let r = w
        .http(rq(
            "GET",
            &format!("{base}/1.0/lib-1.0.pom"),
            Some(&w.tok("reader")),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        w.http(rq(
            "GET",
            &format!("{base}/1.0/lib-1.0.pom"),
            None,
            &[],
            Bytes::new()
        ))
        .await
        .status(),
        401
    );
    // Wrong file names are rejected.
    assert_eq!(
        put(&w, &tok, &format!("{base}/1.0/other-1.0.jar"), b"x")
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn snapshot_metadata_lists_the_newest_timestamped_build() {
    let w = world().await;
    let tok = w.tok("owner");
    let base = "/maven/acme/org/demo/app/2.0-SNAPSHOT";
    for (ts, build) in [("20260101.100000", 1), ("20260102.110000", 2)] {
        for (ext, cls) in [("jar", ""), ("pom", ""), ("jar", "-sources")] {
            let f = format!("{base}/app-2.0-{ts}-{build}{cls}.{ext}");
            assert_eq!(put(&w, &tok, &f, f.as_bytes()).await.status(), 201);
        }
    }
    let m = w
        .http(rq(
            "GET",
            &format!("{base}/maven-metadata.xml"),
            Some(&tok),
            &[],
            Bytes::new(),
        ))
        .await;
    let xml = body_str(&m);
    let doc = roxmltree::Document::parse(&xml).unwrap();
    let ts = doc
        .descendants()
        .find(|n| n.has_tag_name("timestamp"))
        .and_then(|n| n.text())
        .unwrap();
    assert_eq!(ts, "20260102.110000");
    let build = doc
        .descendants()
        .find(|n| n.has_tag_name("buildNumber"))
        .and_then(|n| n.text())
        .unwrap();
    assert_eq!(build, "2");
    let values: Vec<&str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("value"))
        .filter_map(|n| n.text())
        .collect();
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|v| *v == "2.0-20260102.110000-2"));
    let classifiers: Vec<&str> = doc
        .descendants()
        .filter(|n| n.has_tag_name("classifier"))
        .filter_map(|n| n.text())
        .collect();
    assert_eq!(classifiers, ["sources"]);
}
