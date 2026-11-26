//! Briefs and evidence over HTTP (`docs/spec/forge-briefs.md`, `docs/spec/server.md` §3): the
//! `BriefService` RPCs and the signed `/_evidence/...` transfers, which decide repo access
//! again each time they are redeemed (SPEC §9.6).

mod common;

use std::time::Duration;

use base64::Engine as _;
use bee_mcp::{ClientConfig, RpcClient};
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// A 16 by 16 PNG header; enough for the server to sniff and measure it.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAAAAAA";

fn png() -> Vec<u8> {
    B64.decode(PNG).unwrap()
}

/// The path and query of a signed URL, so it can be redeemed against the test server.
fn local(url: &str) -> String {
    let at = url.find("/_evidence/").expect("an evidence URL");
    url[at..].to_string()
}

async fn open_change(t: &TestServer, cid: u64, file: &str, bytes: &[u8]) -> String {
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[(file, bytes)], cid, file).await["id"]);
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": file }),
        )
        .await
        .expect("open change");
    cr["id"].as_str().unwrap().to_string()
}

async fn add_screenshot(t: &TestServer, cr: &str) -> Value {
    t.call(
        "BriefService",
        "AddEvidence",
        json!({
            "repo": repo(),
            "changeId": cr,
            "spec": { "kind": "EVIDENCE_KIND_SCREENSHOT", "title": "The new page" },
            "assets": [{
                "role": "EVIDENCE_ASSET_ROLE_MAIN",
                "contentType": "image/png",
                "data": PNG,
                "alt": "A 16 by 16 test image",
            }],
        }),
    )
    .await
    .expect("add evidence")
}

async fn asset_url(t: &TestServer, token: Option<&str>, id: &str) -> Result<String, String> {
    let mut c = ClientConfig::new(t.url().to_string());
    c.token = token.map(str::to_string);
    c.request_timeout = Duration::from_secs(60);
    let got = RpcClient::new(&c)
        .call_rpc_json(
            "BriefService",
            "GetEvidence",
            &json!({ "repo": repo(), "id": id }),
        )
        .await
        .map_err(|e| e.reason)?;
    Ok(got["assets"][0]["url"].as_str().unwrap().to_string())
}

async fn get(t: &TestServer, url: &str, range: Option<&str>) -> (u16, http::HeaderMap, Vec<u8>) {
    let target = format!("{}{}", t.url(), local(url));
    let headers: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
    http("GET", &target, &headers, Vec::new()).await
}

async fn checklist(t: &TestServer, cr: &str) -> Value {
    t.call(
        "BriefService",
        "GetBriefChecklist",
        json!({ "repo": repo(), "changeId": cr }),
    )
    .await
    .expect("checklist")
}

/// A download URL names the principal it was issued to, and redeeming it decides repo read
/// access for that principal again: making the repo private ends a stranger's and an
/// anonymous reader's access at once, while the owner's URL keeps working. Tampered URLs and
/// upload URLs are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evidence_urls_redecide_repo_access() {
    let t = start_default().await;
    let cr = open_change(&t, 9101, "shot.txt", b"x\n").await;
    let evidence = add_screenshot(&t, &cr).await;
    let id = evidence["id"].as_str().unwrap();
    let (_, stranger) = t.principal("human", "stranger", "").await;
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": repo(), "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public");
    let owner = asset_url(&t, Some(&t.token), id).await.unwrap();
    let theirs = asset_url(&t, Some(&stranger), id).await.unwrap();
    let anon = asset_url(&t, None, id).await.unwrap();
    for url in [&owner, &theirs, &anon] {
        let (status, h, body) = get(&t, url, None).await;
        assert_eq!(status, 200, "{url}");
        assert_eq!(body, png());
        assert_eq!(h["content-type"], "image/png");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert!(
            h["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("sandbox")
        );
        assert!(h.get("content-disposition").is_none(), "images show inline");
    }
    let (status, h, body) = get(&t, &owner, Some("bytes=0-7")).await;
    assert_eq!((status, body.len()), (206, 8));
    assert_eq!(h["content-range"], "bytes 0-7/33");
    assert_eq!(get(&t, &owner, Some("bytes=99-")).await.0, 416);

    // A tampered signature, and an upload URL's shape, are refused.
    let tampered = owner.replace("signature=", "signature=00");
    assert_eq!(get(&t, &tampered, None).await.0, 403);
    let other_chain = owner.replace("/as/", "/as/x");
    assert_eq!(get(&t, &other_chain, None).await.0, 403);

    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": repo(), "visibility": "VISIBILITY_PRIVATE" }),
    )
    .await
    .expect("private");
    assert_eq!(
        get(&t, &theirs, None).await.0,
        404,
        "stranger, made private"
    );
    assert_eq!(get(&t, &anon, None).await.0, 404, "anonymous, made private");
    assert_eq!(get(&t, &owner, None).await.0, 200, "the owner still reads");
    assert_eq!(
        asset_url(&t, Some(&stranger), id).await.unwrap_err(),
        "NOT_FOUND"
    );
}

/// Begin, `PUT` the bytes, finalize: a Markdown document is served as an attachment (never
/// shown in the page), with `HEAD` and `Range` working, and the upload URL only takes bytes
/// of a pending item that are the declared type.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploaded_documents_are_served_as_attachments() {
    let t = start_default().await;
    let cr = open_change(&t, 9102, "doc.txt", b"x\n").await;
    let notes = b"# Notes\n\nThe cache key includes every input.\n";
    let begin = t
        .call(
            "BriefService",
            "BeginEvidenceUpload",
            json!({
                "repo": repo(),
                "changeId": cr,
                "spec": { "kind": "EVIDENCE_KIND_DOCUMENT", "title": "Design notes" },
                "assets": [{
                    "role": "EVIDENCE_ASSET_ROLE_MAIN",
                    "contentType": "text/markdown",
                    "sizeBytes": notes.len().to_string(),
                }],
            }),
        )
        .await
        .expect("begin");
    let id = begin["evidence"]["id"].as_str().unwrap();
    let up = format!(
        "{}{}",
        t.url(),
        local(begin["uploads"][0]["uploadUrl"].as_str().unwrap())
    );
    // Not yet downloadable, and HTML is not Markdown.
    let (status, ..) = http(
        "PUT",
        &up,
        &[],
        b"<html><script>alert(1)</script></html>".to_vec(),
    )
    .await;
    assert_eq!(status, 415);
    let (status, ..) = http("PUT", &up, &[], notes.to_vec()).await;
    assert_eq!(status, 200);
    let digest: String = Sha256::digest(notes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    t.call(
        "BriefService",
        "FinalizeEvidence",
        json!({
            "repo": repo(),
            "evidenceId": id,
            "digests": [{ "role": "EVIDENCE_ASSET_ROLE_MAIN", "sha256": digest }],
        }),
    )
    .await
    .expect("finalize");
    // The pending item took its bytes once; the URL does nothing for a ready item.
    let (status, ..) = http("PUT", &up, &[], notes.to_vec()).await;
    assert_eq!(status, 409);

    let url = asset_url(&t, Some(&t.token), id).await.unwrap();
    let (status, h, body) = get(&t, &url, None).await;
    assert_eq!((status, body.as_slice()), (200, &notes[..]));
    assert_eq!(h["content-disposition"], "attachment");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["etag"].to_str().unwrap(), format!("\"sha256:{digest}\""));
    let head = http(
        "HEAD",
        &format!("{}{}", t.url(), local(&url)),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!((head.0, head.2.len()), (200, 0));
    assert_eq!(head.1["content-length"], notes.len().to_string());
    // The download URL is not an upload URL.
    let put = http(
        "PUT",
        &format!("{}{}", t.url(), local(&url)),
        &[],
        notes.to_vec(),
    )
    .await;
    assert_eq!(put.0, 403);
}

/// The brief and checklist are served: a UI change owes a screenshot until one is attached for
/// this revision, and the brief round-trips through the change request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checklist_follows_the_brief_and_evidence() {
    let t = start_default().await;
    let cr = open_change(&t, 9103, "ui/page.tsx", b"export const A = 1;\n").await;
    let before = checklist(&t, &cr).await;
    let ids: Vec<&str> = before["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["evidence:ui"], "{before}");
    assert_ne!(before["complete"], true);

    let set = t
        .call(
            "BriefService",
            "SetChangeBrief",
            json!({
                "repo": repo(),
                "changeId": cr,
                "brief": {
                    "outcome": "A new page",
                    "highlights": [{ "text": "Adds the page", "paths": ["ui/page.tsx"] }],
                },
            }),
        )
        .await
        .expect("set brief");
    assert_eq!(set["brief"]["outcome"], "A new page");
    let got = t
        .call(
            "ChangeService",
            "GetChange",
            json!({ "repo": repo(), "id": cr }),
        )
        .await
        .expect("get change");
    assert_eq!(got["brief"]["outcome"], "A new page", "{got}");

    add_screenshot(&t, &cr).await;
    let after = checklist(&t, &cr).await;
    assert_eq!(after["complete"], true, "{after}");
    let listed = t
        .call(
            "BriefService",
            "ListEvidence",
            json!({ "repo": repo(), "changeIds": [cr], "heroOnly": true }),
        )
        .await
        .expect("list");
    assert_eq!(listed["evidence"].as_array().unwrap().len(), 1, "{listed}");
}
