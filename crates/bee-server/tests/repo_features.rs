//! Repo features over HTTP/JSON against an in-process `bee-server --dev`: social preview
//! images (F416-011), repository size and health (F416-032), pull mirrors of external git
//! remotes on a schedule with status (F416-031), and RenameRef carrying the version line and
//! ref log and leaving a redirect.

#![allow(clippy::result_large_err)]

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
];

async fn put(url: &str, body: &[u8]) -> u16 {
    http("PUT", url, &[("content-type", "image/png")], body.to_vec())
        .await
        .0
}

/// F416-011: a signed upload URL takes a PNG or JPEG for the repo's social preview; once the
/// bytes arrive the repo's `social_preview_url` serves them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn social_preview_images_upload_through_a_signed_url() {
    let t = start_default().await;
    let up = t
        .call(
            "RepoService",
            "CreateSocialPreviewUpload",
            json!({ "repo": repo(), "contentType": "image/png", "sizeBytes": PNG.len() }),
        )
        .await
        .expect("upload url");
    let url = up["uploadUrl"].as_str().unwrap().to_string();
    assert!(
        url.contains("/social-previews/") && url.contains("signature="),
        "{url}"
    );
    assert!(
        up["uploadUrlExpiresUnix"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            > 0
    );
    // A tampered signature and a non-image are refused; the real image is stored.
    assert_eq!(
        put(&url.replace("signature=", "signature=0"), PNG).await,
        403
    );
    assert_eq!(put(&url, b"plain text").await, 415);
    assert_eq!(put(&url, PNG).await, 200);
    let r = t
        .call("RepoService", "GetRepo", json!({ "repo": repo() }))
        .await
        .unwrap();
    let preview = r["socialPreviewUrl"]
        .as_str()
        .expect("preview url")
        .to_string();
    assert!(
        !preview.contains("signature="),
        "the preview is public: {preview}"
    );
    let (s, h, b) = http("GET", &preview, &[], Vec::new()).await;
    assert_eq!(s, 200);
    assert_eq!(h["content-type"], "image/png");
    assert_eq!(b, PNG);
    // Wrong type, size limits and authorization.
    let e = t
        .call(
            "RepoService",
            "CreateSocialPreviewUpload",
            json!({ "repo": repo(), "contentType": "image/gif", "sizeBytes": 10 }),
        )
        .await
        .expect_err("gif");
    assert_eq!(reason(&e), "INVALID_ARGUMENT");
    let e = t
        .call(
            "RepoService",
            "CreateSocialPreviewUpload",
            json!({ "repo": repo(), "contentType": "image/png", "sizeBytes": 2 << 20 }),
        )
        .await
        .expect_err("too large");
    assert!(!reason(&e).is_empty());
    let dev = t.srv.dev.as_ref().unwrap();
    let e = t
        .call_as(
            &dev.reviewer_token,
            "RepoService",
            "CreateSocialPreviewUpload",
            json!({ "repo": repo(), "contentType": "image/png", "sizeBytes": 10 }),
        )
        .await
        .expect_err("not a maintainer");
    assert!(
        matches!(reason(&e), "PERMISSION_DENIED" | "POLICY_DENIED"),
        "{e:?}"
    );
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

/// F416-032: GetRepoHealth reports counts, bytes, large files and warnings at a ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_health_reports_size_and_large_files() {
    let t = start_default().await;
    t.land_policy("[push]\nmax_file_size = 2400\n", 9700).await;
    let (tip, _) = t.tip("main").await;
    let big = vec![b'x'; 2000];
    let head = id_text(
        &t.commit(
            &tip,
            &[
                ("assets/big.bin", &big[..]),
                ("src/a.rs", b"fn main() {}\n"),
            ],
            9701,
            "files",
        )
        .await["id"],
    );
    t.land(&head).await.expect("land");
    let h = t
        .call(
            "AdminService",
            "GetRepoHealth",
            json!({ "repo": repo(), "largeFileBytes": "1000" }),
        )
        .await
        .expect("health");
    assert_eq!(h["ref"], "main");
    assert_eq!(id_text(&h["tip"]), t.tip("main").await.0);
    let n = |k: &str| h[k].as_str().unwrap_or("0").parse::<u64>().unwrap();
    assert!(n("files") >= 3, "{h}");
    assert!(n("tipBytes") >= 2000 + 13);
    assert_eq!(n("largestFileBytes"), 2000);
    assert!(n("historyDepth") >= 2);
    assert!(n("refs") >= 1);
    assert!(n("directories") >= 2);
    assert_eq!(h["largeFiles"][0]["path"], b64("assets/big.bin"));
    assert_eq!(h["largeFiles"][0]["sizeBytes"], "2000");
    let codes: Vec<&str> = h["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(
        codes.contains(&"large_file") && codes.contains(&"near_push_limit"),
        "{codes:?}"
    );
    // The default threshold (50 MiB) lists nothing here; unknown refs are NOT_FOUND.
    let h = t
        .call("AdminService", "GetRepoHealth", json!({ "repo": repo() }))
        .await
        .unwrap();
    assert!(
        h.get("largeFiles")
            .is_none_or(|v| v.as_array().is_none_or(|a| a.is_empty()))
    );
    let e = t
        .call(
            "AdminService",
            "GetRepoHealth",
            json!({ "repo": repo(), "ref": "nope" }),
        )
        .await
        .expect_err("unknown ref");
    assert_eq!(reason(&e), "NOT_FOUND");
}

async fn resolve(t: &TestServer, name: &str) -> Result<Value, bee_proto::v1::ErrorDetail> {
    t.call(
        "ObjectService",
        "ResolveRef",
        json!({ "repo": repo(), "name": name }),
    )
    .await
}

/// RenameRef: the new name continues the old version line with the same ref log, and the old
/// name resolves to the new ref until a ref of that name exists again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_ref_carries_versions_and_log_and_redirects() {
    let t = start_default().await;
    let (tip, _) = t.tip("main").await;
    let c1 = id_text(&t.commit(&tip, &[("r1.txt", b"1\n")], 9801, "r1").await["id"]);
    let c2 = id_text(&t.commit(&c1, &[("r2.txt", b"2\n")], 9802, "r2").await["id"]);
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "feature/a", "target": id_json(&c1) }),
    )
    .await
    .expect("create");
    t.call(
        "RefService",
        "UpdateRef",
        json!({ "repo": repo(), "name": "feature/a", "target": id_json(&c2), "expectedVersion": "1" }),
    )
    .await
    .expect("update");
    let renamed = t
        .call(
            "RefService",
            "RenameRef",
            json!({ "repo": repo(), "name": "feature/a", "newName": "feature/b", "expectedVersion": "2" }),
        )
        .await
        .expect("rename");
    assert_eq!(renamed["name"], "feature/b");
    assert_eq!(
        renamed["version"], "2",
        "the version line moves with the ref"
    );
    assert_eq!(id_text(&renamed["target"]), c2);
    let log = t
        .call(
            "RefService",
            "GetRefLog",
            json!({ "repo": repo(), "name": "feature/b" }),
        )
        .await
        .unwrap();
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{log}");
    assert_eq!(id_text(&entries[0]["new"]), c1);
    assert_eq!(id_text(&entries[1]["new"]), c2);
    // The old name redirects.
    let r = resolve(&t, "feature/a").await.expect("redirect");
    assert_eq!(r["ref"]["name"], "feature/b");
    assert_eq!(r["renamedTo"], "feature/b");
    assert_eq!(id_text(&r["ref"]["target"]), c2);
    // A new ref of the old name wins over the redirect.
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "feature/a", "target": id_json(&c1) }),
    )
    .await
    .expect("recreate");
    let r = resolve(&t, "feature/a").await.unwrap();
    assert_eq!(r["ref"]["name"], "feature/a");
    assert!(r.get("renamedTo").is_none_or(|v| v == ""), "{r}");
}

/// F416-031: a pull mirror fetches an external git remote and imports it, manually
/// (SyncRepoMirror) and on its schedule, recording `last_sync_at` and `last_error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_mirrors_sync_external_remotes_with_status() {
    let t = start_with(|c| {
        c.forge.mirror_allow_insecure = true;
        c.forge.mirror_poll_ms = 100;
    })
    .await;
    // The "external" remote: another repo served over git smart HTTP.
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "copy", "autoInit": false }),
    )
    .await
    .expect("mirror repo");
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(&tip, &[("mirrored.txt", b"from upstream\n")], 9901, "up")
            .await["id"],
    );
    t.land(&head).await.unwrap();
    let remote = format!("http://{}/dev/playground.git", t.srv.addr);
    let m = t
        .call(
            "RepoService",
            "CreateRepoMirror",
            json!({ "repo": { "fullName": "dev/copy" }, "remoteUrl": remote,
                    "credential": t.token, "direction": "MIRROR_DIRECTION_PULL",
                    "intervalSeconds": 300 }),
        )
        .await
        .expect("mirror");
    let id = m["id"].as_str().unwrap().to_string();
    let synced = t
        .call(
            "RepoService",
            "SyncRepoMirror",
            json!({ "repo": { "fullName": "dev/copy" }, "id": id }),
        )
        .await
        .expect("sync");
    assert_eq!(synced["lastError"].as_str().unwrap_or(""), "", "{synced}");
    assert!(synced["lastSyncAt"].is_string());
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": { "fullName": "dev/copy" }, "name": "main" }),
        )
        .await
        .expect("mirrored main");
    let mirrored = t
        .call(
            "QueryService",
            "ReadPath",
            json!({ "repo": { "fullName": "dev/copy" }, "at": { "expr": id_text(&r["ref"]["target"]) },
                    "path": base64::engine::general_purpose::STANDARD.encode("mirrored.txt") }),
        )
        .await
        .expect("read mirrored file");
    let data = mirrored["items"][0]["data"].as_str().unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap(),
        b"from upstream\n"
    );

    // A mirror that never synced runs on the schedule; a broken remote records its error.
    t.call(
        "RepoService",
        "CreateRepoMirror",
        json!({ "repo": { "fullName": "dev/copy" }, "remoteUrl": "http://127.0.0.1:1/none.git",
                "direction": "MIRROR_DIRECTION_PULL", "intervalSeconds": 300 }),
    )
    .await
    .expect("broken mirror");
    let broken = eventually(60, "the scheduled sync", || async {
        let l = t
            .call(
                "RepoService",
                "ListRepoMirrors",
                json!({ "repo": { "fullName": "dev/copy" } }),
            )
            .await
            .ok()?;
        l["mirrors"]
            .as_array()?
            .iter()
            .find(|m| {
                m["remoteUrl"] == "http://127.0.0.1:1/none.git" && m["lastSyncAt"].is_string()
            })
            .cloned()
    })
    .await;
    assert!(
        !broken["lastError"].as_str().unwrap_or("").is_empty(),
        "{broken}"
    );
    // Unknown mirrors and non-admins.
    let e = t
        .call(
            "RepoService",
            "SyncRepoMirror",
            json!({ "repo": { "fullName": "dev/copy" }, "id": "nope" }),
        )
        .await
        .expect_err("unknown");
    assert_eq!(reason(&e), "NOT_FOUND");
}
