//! Coverage rows of PRODUCT §4.5, §4.11–§4.14 and §4.18 proven against an in-process
//! `bee-server --dev` over HTTP/JSON and gRPC.

mod common;

use base64::Engine as _;
use bee_proto::v1;
use common::*;
use serde_json::json;

/// `GetServerInfo` over HTTP/JSON without a credential.
async fn anon(base: &str, req: serde_json::Value) -> (u16, http::HeaderMap, serde_json::Value) {
    let (status, headers, body) = http(
        "POST",
        &format!("{base}/bee.v1.ObjectService/GetServerInfo"),
        &[("content-type", "application/json")],
        serde_json::to_vec(&req).unwrap(),
    )
    .await;
    (status, headers, serde_json::from_slice(&body).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_info_reports_versions_keys_limits_client_id_and_repo_epochs() {
    let t = start_with(|c| c.login.github_client_id = "Iv1.bee-device-flow".into()).await;
    // Anonymous: everything a client needs before it has a token.
    let (status, headers, info) = anon(t.url(), json!({})).await;
    assert_eq!(status, 200, "{info}");
    assert_eq!(info["tier"], "server");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["protocolVersions"], json!(["bee.v1"]));
    assert!(
        info["algorithms"]
            .as_array()
            .unwrap()
            .contains(&json!("blake3-256"))
    );
    let keys = info["tokenPublicKeys"].as_array().expect("keys");
    assert!(!keys.is_empty());
    for k in keys {
        assert_eq!(k["algorithm"], "ed25519");
        assert!(!k["keyId"].as_str().unwrap().is_empty());
        assert!(!k["publicKey"].as_str().unwrap().is_empty());
    }
    let limits = &info["limits"];
    assert_eq!(limits["maxPathBytes"], "4096");
    assert_eq!(limits["maxPathComponents"], 256);
    assert_eq!(limits["maxNameBytes"], 255);
    assert_eq!(limits["maxBatchIds"], t.cfg.limits.max_batch_ids);
    assert!(
        limits["inlineObjectBytes"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(
        info["maxBlobBytes"],
        t.cfg.limits.max_blob_bytes.to_string()
    );
    assert_eq!(info["githubClientId"], "Iv1.bee-device-flow");
    assert_eq!(info["cell"], t.cfg.server.cell);
    assert!(
        info["features"]
            .as_array()
            .unwrap()
            .contains(&json!("land_coalescer"))
    );
    assert!(!info["connectBaseUrl"].as_str().unwrap().is_empty());
    // Epochs are per repo; without one they are zero (proto3 JSON omits them).
    assert!(info.get("redactionEpoch").is_none());
    assert_eq!(
        headers.get("trailer-bee-tier").map(|v| v.to_str().unwrap()),
        Some("server"),
        "the tier that answered"
    );
    // Naming a repo needs a credential that can read it.
    let (status, _, e) = anon(t.url(), json!({ "repo": repo() })).await;
    assert_eq!(status, 401, "{e}");

    // With a repo: both epochs, equal to the trailers of reads on that repo.
    let epochs = || async {
        let info = t
            .call("ObjectService", "GetServerInfo", json!({ "repo": repo() }))
            .await
            .expect("server info");
        let p: u64 = info["policyEpoch"].as_str().unwrap().parse().unwrap();
        let r: u64 = info["redactionEpoch"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap();
        (p, r)
    };
    let (p0, r0) = epochs().await;
    assert!(p0 > 0);
    let mut grpc = v1::object_service_client::ObjectServiceClient::new(t.grpc_channel().await);
    let trailer = |md: &tonic::metadata::MetadataMap, name: &str| -> u64 {
        md.get(name).unwrap().to_str().unwrap().parse().unwrap()
    };
    let resp = grpc
        .resolve_ref(t.bearer(
            &t.token,
            v1::ResolveRefRequest {
                repo: prepo(),
                name: "main".into(),
                ..Default::default()
            },
        ))
        .await
        .expect("resolve");
    assert_eq!(trailer(resp.metadata(), "bee-policy-epoch"), p0);
    assert_eq!(trailer(resp.metadata(), "bee-redaction-epoch"), r0);
    // A policy change moves the policy epoch.
    t.land_policy(
        "[[access]]\npath = \"/docs\"\nread = [\"@everyone\"]\nwrite = [\"@humans\"]\n",
        900,
    )
    .await;
    let (p1, _) = epochs().await;
    assert_ne!(p1, p0, "a new policy is a new policy epoch");
    // A redaction moves the redaction epoch.
    let (tip, _) = t.tip("main").await;
    let cs = t
        .commit(&tip, &[("leak.txt", b"token=abc\n")], 901, "leak")
        .await;
    t.land(&id_text(&cs["id"])).await.expect("land");
    let blob = bee_object::blob_id(b"token=abc\n").to_text();
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leaked token" }),
    )
    .await
    .expect("redact");
    let (p2, r2) = epochs().await;
    assert_eq!(p2, p1);
    assert!(
        r2 > r0,
        "redaction bumps the repo's redaction epoch: {r0} -> {r2}"
    );
    // The same values as the trailers of a streamed read, which also names the tier.
    let mut stream = grpc
        .get_objects(t.bearer(
            &t.token,
            v1::GetObjectsRequest {
                repo: prepo(),
                ids: vec![pid(&tip)],
                ..Default::default()
            },
        ))
        .await
        .expect("get objects")
        .into_inner();
    while stream.message().await.expect("chunk").is_some() {}
    let md = stream.trailers().await.expect("trailers").expect("some");
    assert_eq!(trailer(&md, "bee-policy-epoch"), p2);
    assert_eq!(trailer(&md, "bee-redaction-epoch"), r2);
    assert_eq!(md.get("bee-tier").unwrap(), "server");
}

/// Lands `files` (`(path, mode, bytes)`) on `main` in one changeset.
async fn land_modes(t: &TestServer, files: &[(&str, &str, &[u8])], cid: u64) -> String {
    let blobs: Vec<&[u8]> = files.iter().map(|(_, _, b)| *b).collect();
    let ids = t.upload(&blobs).await;
    let delta: Vec<serde_json::Value> = files
        .iter()
        .zip(&ids)
        .map(|((p, m, b), id)| {
            json!({ "path": B64.encode(p.as_bytes()),
                    "put": { "mode": m, "blob": id_json(id), "size": b.len().to_string() } })
        })
        .collect();
    let (tip, _) = t.tip("main").await;
    let cs = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({ "repo": repo(), "parents": [id_json(&tip)], "delta": delta,
                    "changeId": change_id(cid), "author": "tester",
                    "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                    "message": "archive fixture" }),
        )
        .await
        .expect("changeset");
    let head = id_text(&cs["changeset"]["id"]);
    t.land(&head).await.expect("land");
    head
}

/// Extracts tar.gz bytes with the system `tar` and zip bytes with the system `unzip` into a new
/// directory. GNU tar cannot read zip; Info-ZIP `unzip` restores the Unix mode bits and symbolic
/// links asserted by this test.
fn extract(bytes: &[u8], name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(name);
    std::fs::write(&file, bytes).unwrap();
    let out_dir = dir.path().join("x");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = if name.ends_with(".zip") {
        std::process::Command::new("unzip")
            .arg("-q")
            .arg(&file)
            .arg("-d")
            .arg(&out_dir)
            .output()
            .expect("unzip runs")
    } else {
        std::process::Command::new("tar")
            .arg("-xf")
            .arg(&file)
            .arg("-C")
            .arg(&out_dir)
            .output()
            .expect("tar runs")
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_archives_stream_trees_as_tar_gz_and_zip_cached_by_tree_id() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = start_default().await;
    let long = format!("src/{}/deep.rs", "n".repeat(110));
    land_modes(
        &t,
        &[
            ("README.md", "MODE_FILE", b"# archive me\n"),
            ("bin/run.sh", "MODE_EXECUTABLE", b"#!/bin/sh\necho hi\n"),
            ("docs/readme-link", "MODE_SYMLINK", b"../README.md"),
            (long.as_str(), "MODE_FILE", b"fn deep() {}\n"),
        ],
        700,
    )
    .await;
    for (format, ext) in [
        ("SOURCE_ARCHIVE_FORMAT_TAR_GZ", "tar.gz"),
        ("SOURCE_ARCHIVE_FORMAT_ZIP", "zip"),
    ] {
        let ask = || {
            t.call(
                "ReleaseService",
                "GetSourceArchiveUrl",
                json!({ "repo": repo(), "at": { "expr": "main" }, "format": format, "prefix": "snap" }),
            )
        };
        let r = ask().await.expect("archive url");
        let url = r["url"].as_str().unwrap().to_string();
        let path = url.split('?').next().unwrap().to_string();
        assert!(path.ends_with(&format!("/snap.{ext}")), "{url}");
        let tree = id_text(&r["tree"]);
        assert!(
            path.contains(&format!(
                "/{}/",
                bee_core::hex::encode(&B64.decode(r["tree"]["raw"].as_str().unwrap()).unwrap())
            )),
            "{path} names the tree {tree}"
        );
        // First download: streamed from the tree.
        let (status, headers, first) = http("GET", &url, &[], Vec::new()).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&first));
        assert_eq!(headers["x-bee-archive-cache"], "miss");
        assert_eq!(
            headers["content-disposition"],
            format!("attachment; filename=\"snap.{ext}\"").as_str()
        );
        let x = extract(&first, &format!("a.{ext}"));
        let root = x.path().join("x/snap");
        assert_eq!(
            std::fs::read(root.join("README.md")).unwrap(),
            b"# archive me\n"
        );
        assert_eq!(std::fs::read(root.join(&long)).unwrap(), b"fn deep() {}\n");
        let mode = std::fs::metadata(root.join("bin/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "{ext}: executables keep their bit");
        assert_eq!(
            std::fs::read_link(root.join("docs/readme-link")).unwrap(),
            std::path::Path::new("../README.md")
        );
        // The same tree again: the same key, and once built it comes from the cache.
        let again = ask().await.expect("archive url again");
        assert_eq!(
            again["url"].as_str().unwrap().split('?').next().unwrap(),
            path
        );
        let cached = eventually(20, "the archive to be kept", || {
            let url = url.clone();
            async move {
                let (status, headers, body) = http("GET", &url, &[], Vec::new()).await;
                (status == 200 && headers["x-bee-archive-cache"] == "hit").then_some(body)
            }
        })
        .await;
        assert_eq!(
            cached, first,
            "{ext}: archives are a pure function of the tree"
        );
        // The signature is the authorization.
        let forged = url.replace("signature=", "signature=00");
        assert_eq!(http("GET", &forged, &[], Vec::new()).await.0, 403);
    }
    // GitHub's tarball and zipball routes redirect to the signed archive.
    let auth = format!("token {}", t.token);
    for (route, magic) in [("tarball", &b"\x1f\x8b"[..]), ("zipball", &b"PK"[..])] {
        let (status, headers, _) = http(
            "GET",
            &format!("{}/repos/dev/playground/{route}/main", t.url()),
            &[("authorization", auth.as_str())],
            Vec::new(),
        )
        .await;
        assert_eq!(status, 302, "{route}");
        let location = headers["location"].to_str().unwrap().to_string();
        let (status, _, body) = http("GET", &location, &[], Vec::new()).await;
        assert_eq!(status, 200);
        assert!(body.starts_with(magic), "{route}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_archives_need_read_access_to_every_path() {
    let t = start_default().await;
    let dev = t.dev_user();
    t.land_policy(
        &format!("[[access]]\npath = \"/secret\"\nread = [\"{dev}\"]\nwrite = [\"{dev}\"]\n"),
        710,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let cs = t
        .commit(
            &tip,
            &[("secret/key.txt", b"hidden\n"), ("open.txt", b"open\n")],
            711,
            "files",
        )
        .await;
    t.land(&id_text(&cs["id"])).await.expect("land");
    // A reader of the repo who is kept out of /secret.
    let (reader_id, reader) = t.principal("human", "reader", "").await;
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": repo(), "principalId": reader_id, "role": "REPO_ROLE_READ" }),
        )
        .await
        .expect("add collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        t.call_as(
            &reader,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept");
    }
    let req =
        json!({ "repo": repo(), "at": { "expr": "main" }, "format": "SOURCE_ARCHIVE_FORMAT_ZIP" });
    let read = json!({ "repo": repo(), "at": { "expr": "main" }, "path": B64.encode("open.txt") });
    assert!(
        t.call_as(&reader, "QueryService", "ReadPath", read)
            .await
            .is_ok(),
        "the reader reads the repo"
    );
    let e = t
        .call_as(
            &reader,
            "ReleaseService",
            "GetSourceArchiveUrl",
            req.clone(),
        )
        .await
        .expect_err("the reader cannot read /secret");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    t.call("ReleaseService", "GetSourceArchiveUrl", req)
        .await
        .expect("the dev user reads every path");
}

/// Adds `id` as a collaborator with `role` on the dev repo and accepts as them.
async fn collaborator(t: &TestServer, id: &str, token: &str, role: &str) {
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": repo(), "principalId": id, "role": role }),
        )
        .await
        .expect("add collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        t.call_as(
            token,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_mode_keeps_meta_verbatim_for_an_importer_with_ref_admin_and_root_write() {
    let t = start_default().await;
    let dev = t.dev_user();
    let (importer_id, importer) = t.principal("service", "importer", "").await;
    let (writer_id, writer) = t.principal("service", "ci-writer", "").await;
    // Policy: the importer administers the repo; the other service may only write.
    let (tip, _) = t.tip("main").await;
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{dev}\", \"{importer_id}\"]\nwrite = [\"@humans\", \"{writer_id}\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n"
    );
    let p = t
        .commit(
            &tip,
            &[(".beehive/policy.toml", policy.as_bytes())],
            720,
            "policy",
        )
        .await;
    t.land(&id_text(&p["id"])).await.expect("policy lands");
    collaborator(&t, &importer_id, &importer, "REPO_ROLE_ADMIN").await;
    collaborator(&t, &writer_id, &writer, "REPO_ROLE_WRITE").await;
    let example = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id().starts_with("github") && r.confidence().blocks())
        .and_then(|r| r.example().map(str::to_string))
        .expect("a github rule example");
    let ids = t
        .upload_as(
            &importer,
            &[b"imported history\n".as_slice(), example.as_bytes()],
        )
        .await;
    let git_ext = b"author Imp Orter <imp@example.com> 1600000000 +0200\n\
                    committer Ann Other <ann@example.com> 1600000100 -0500\n\n";
    // The importer's ChangeId is derived from the original commit's id (SPEC §3.3), which is
    // what lets export reproduce that commit byte for byte.
    let hash = |kind, body: &[u8]| bee_git::ObjectFormat::Sha1.hash_object(kind, body).unwrap();
    let readme_oid = hash(bee_git::ObjectType::Blob, b"imported history\n");
    let config_oid = hash(bee_git::ObjectType::Blob, example.as_bytes());
    let mut old_tree = b"100644 config.yml\0".to_vec();
    old_tree.extend_from_slice(config_oid.as_bytes());
    let old_oid = hash(bee_git::ObjectType::Tree, &old_tree);
    let mut root_tree = b"100644 README\0".to_vec();
    root_tree.extend_from_slice(readme_oid.as_bytes());
    root_tree.extend_from_slice(b"40000 old\0");
    root_tree.extend_from_slice(old_oid.as_bytes());
    let root_oid = hash(bee_git::ObjectType::Tree, &root_tree);
    let original = bee_git::export::imported_commit(root_oid, git_ext, "imported commit\n");
    let original_oid = hash(bee_git::ObjectType::Commit, &original.serialize());
    let import_cid = bee_object::import_change_id(original_oid.as_bytes());
    let request = |import_mode: bool, ext: bool| {
        let mut r = json!({
            "repo": repo(),
            "parents": [],
            "delta": [
                { "path": B64.encode("README"), "put": { "mode": "MODE_FILE", "blob": id_json(&ids[0]), "size": "17" } },
                { "path": B64.encode("old/config.yml"), "put": { "mode": "MODE_FILE", "blob": id_json(&ids[1]), "size": example.len().to_string() } },
            ],
            "changeId": { "raw": B64.encode(import_cid.as_bytes()) },
            "author": "Imp Orter <imp@example.com>",
            "authorTime": { "name": "+0200", "time": "2020-09-13T12:26:40Z" },
            "committer": "Ann Other <ann@example.com>",
            "committerTime": { "name": "-0500", "time": "2020-09-13T12:28:20Z" },
            "message": "imported commit\n",
            "importMode": import_mode,
        });
        if ext {
            r["extensions"] = json!([{ "key": 64, "value": B64.encode(git_ext) }]);
        }
        r
    };
    // A human may not use import mode, and nobody may send the git extension without it.
    let e = t
        .call("WriteService", "CreateChangeset", request(true, true))
        .await
        .expect_err("humans are not importers");
    assert_eq!(e.reason, "PERMISSION_DENIED", "{e:?}");
    let e = t
        .call_as(&importer, "WriteService", "CreateChangeset", {
            let mut r = request(false, true);
            r["parents"] = json!([id_json(&t.tip("main").await.0)]);
            r["delta"] = json!([r["delta"][0].clone()]);
            r
        })
        .await
        .expect_err("extensions need import mode");
    assert_eq!(
        (e.reason.as_str(), e.subcode.as_str()),
        ("INVALID_ARGUMENT", "invalid_field")
    );
    // A service without ref administration is refused.
    let e = t
        .call_as(
            &writer,
            "WriteService",
            "CreateChangeset",
            request(true, true),
        )
        .await
        .expect_err("needs ref-admin");
    assert!(
        matches!(e.reason.as_str(), "POLICY_DENIED" | "PERMISSION_DENIED"),
        "{e:?}"
    );
    // Outside import mode the old secret is refused by push protection.
    let e = t
        .call_as(&importer, "WriteService", "CreateChangeset", {
            let mut r = request(false, false);
            r["parents"] = json!([id_json(&t.tip("main").await.0)]);
            r
        })
        .await
        .expect_err("push protection");
    assert_eq!(e.reason, "SECRET_DETECTED");
    // The importer: a root changeset (no parents), history kept as it was.
    let cs = t
        .call_as(
            &importer,
            "WriteService",
            "CreateChangeset",
            request(true, true),
        )
        .await
        .expect("imported");
    let id = id_text(&cs["changeset"]["id"]);
    let log = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": id }, "includeMeta": true }),
        )
        .await
        .expect("log");
    let entry = &log["changesets"][0];
    assert!(
        entry["parents"].as_array().is_none_or(|p| p.is_empty()),
        "{entry}"
    );
    let meta = &entry["meta"];
    assert_eq!(meta["author"], "Imp Orter <imp@example.com>", "{entry}");
    assert_eq!(meta["committer"], "Ann Other <ann@example.com>", "{entry}");
    assert_eq!(meta["message"], "imported commit\n");
    assert_eq!(
        meta["creator"],
        importer_id.as_str(),
        "the importer is recorded"
    );
    // Its git id is the commit the extension describes (exact headers, offsets included).
    let r = t
        .call(
            "GitService",
            "GitResolve",
            json!({ "repo": repo(), "queries": [{ "beeId": id_json(&id) }] }),
        )
        .await
        .expect("resolve");
    let oid = r["results"][0]["gitOid"]
        .as_str()
        .expect("git id")
        .to_string();
    let mut c = v1::git_service_client::GitServiceClient::new(t.grpc_channel().await);
    let mut s = c
        .get_git_objects(t.bearer(
            &t.token,
            v1::GetGitObjectsRequest {
                repo: prepo(),
                oids: vec![oid.clone()],
                ..Default::default()
            },
        ))
        .await
        .expect("objects")
        .into_inner();
    let mut data = Vec::new();
    while let Some(ch) = s.message().await.expect("chunk") {
        assert!(ch.error_reason.is_empty(), "{ch:?}");
        data.extend(ch.data);
    }
    let h = bee_git::ObjectFormat::Sha1
        .hash_object(bee_git::ObjectType::Commit, &data)
        .unwrap();
    assert_eq!(h.to_hex(), oid, "the bytes are the commit");
    assert_eq!(
        oid,
        original_oid.to_hex(),
        "the original commit id, exactly"
    );
    assert_eq!(r["results"][0]["imported"], true);
    let text = String::from_utf8(data).unwrap();
    assert!(
        text.contains("author Imp Orter <imp@example.com> 1600000000 +0200\n")
            && text.contains("committer Ann Other <ann@example.com> 1600000100 -0500\n"),
        "{text}"
    );
}

/// A local git repository with two commits, for `StartImport` (`IMPORT_SOURCE_KIND_GIT`).
fn git_source(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .env("GIT_AUTHOR_NAME", "Original Author")
            .env("GIT_AUTHOR_EMAIL", "orig@example.com")
            .env("GIT_COMMITTER_NAME", "Original Author")
            .env("GIT_COMMITTER_EMAIL", "orig@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(dir).unwrap();
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "first"]);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "second"]);
}

async fn import_git(t: &TestServer, name: &str, src: &std::path::Path) -> serde_json::Value {
    let started = t
        .call(
            "ImportService",
            "StartImport",
            json!({ "source": format!("dev/{name}"), "sourceKind": "IMPORT_SOURCE_KIND_GIT",
                    "sourceUrl": src.display().to_string() }),
        )
        .await
        .expect("start import");
    let id = started["id"].as_str().unwrap().to_string();
    eventually(60, "the import to finish", || {
        let id = id.clone();
        async move {
            let g = t
                .call("ImportService", "GetImport", json!({ "id": id }))
                .await
                .ok()?;
            match g["state"].as_str().unwrap_or("") {
                "IMPORT_STATE_LIVE" => Some(g),
                "IMPORT_STATE_FAILED" => panic!("import failed: {g}"),
                _ => None,
            }
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imports_run_as_the_orgs_importer_service_principal() {
    let t = start_default().await;
    let dev = t.dev_user();
    let src = t.dir.path().join("source");
    git_source(&src);
    import_git(&t, "imported", &src).await;
    let log = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": { "fullName": "dev/imported" }, "from": { "expr": "main" },
                    "includeMeta": true }),
        )
        .await
        .expect("log");
    let changesets = log["changesets"].as_array().unwrap();
    assert_eq!(changesets.len(), 2, "{log}");
    let importer = changesets[0]["meta"]["creator"]
        .as_str()
        .expect("creator")
        .to_string();
    assert_ne!(importer, dev, "history is not attributed to the starter");
    for c in changesets {
        assert_eq!(c["meta"]["creator"], importer.as_str());
        assert_eq!(c["meta"]["author"], "Original Author <orig@example.com>");
    }
    // The importer is a service principal of the org, owned by the admin who started it.
    let raw = t
        .srv
        .state
        .kv
        .get(bee_server::auth::NS_PRINCIPAL, importer.as_bytes())
        .await
        .unwrap()
        .expect("a principal record");
    let rec: bee_server::auth::PrincipalRecord = serde_json::from_slice(&raw).unwrap();
    assert_eq!(rec.kind, "service");
    assert_eq!(rec.owner, dev);
    assert_eq!(rec.org, t.srv.dev.as_ref().unwrap().org.0.to_text());
    // The ref log names it too.
    let reflog = t
        .call(
            "RefService",
            "GetRefLog",
            json!({ "repo": { "fullName": "dev/imported" }, "name": "main" }),
        )
        .await
        .expect("ref log");
    assert!(reflog.to_string().contains(&importer), "{reflog}");
    // The next import into the org reuses the same principal.
    import_git(&t, "imported-again", &src).await;
    let log2 = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": { "fullName": "dev/imported-again" }, "from": { "expr": "main" },
                    "includeMeta": true }),
        )
        .await
        .expect("log");
    assert_eq!(log2["changesets"][0]["meta"]["creator"], importer.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diff_options_detect_renames_ignore_whitespace_and_cut_large_diffs() {
    let t = start_default().await;
    let long: String = (0..30)
        .map(|i| format!("line {i} of the moved file\n"))
        .collect();
    let (root, _) = t.tip("main").await;
    let base = t
        .commit(
            &root,
            &[("a.txt", long.as_bytes()), ("b.txt", b"x  y\n")],
            730,
            "base",
        )
        .await;
    let base_id = id_text(&base["id"]);
    t.land(&base_id).await.expect("land base");
    // Move a.txt with one line changed, re-space b.txt, add ten files.
    let moved = long.replace("line 7 of", "line seven of");
    let mut blobs: Vec<Vec<u8>> = vec![moved.clone().into_bytes(), b"x y\n".to_vec()];
    blobs.extend((0..10).map(|i| format!("new {i}\n").into_bytes()));
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let ids = t.upload(&refs).await;
    let put = |path: &str, i: usize| {
        json!({ "path": B64.encode(path), "put": { "mode": "MODE_FILE", "blob": id_json(&ids[i]),
                "size": blobs[i].len().to_string() } })
    };
    let mut delta = vec![
        json!({ "path": B64.encode("a.txt"), "delete": true }),
        put("moved/a.txt", 0),
        put("b.txt", 1),
    ];
    delta.extend((0..10).map(|i| put(&format!("n/{i}.txt"), i + 2)));
    let cs = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({ "repo": repo(), "parents": [id_json(&base_id)], "delta": delta,
                    "changeId": change_id(731), "author": "tester",
                    "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                    "message": "move and more" }),
        )
        .await
        .expect("changeset");
    let head = id_text(&cs["changeset"]["id"]);
    let diff = |opts: serde_json::Value| {
        let mut req = json!({ "repo": repo(), "from": { "expr": base_id }, "to": { "expr": head },
                              "includeHunks": true });
        for (k, v) in opts.as_object().unwrap() {
            req[k] = v.clone();
        }
        t.call("QueryService", "Diff", req)
    };
    let path_of = |item: &serde_json::Value, key: &str| {
        String::from_utf8(B64.decode(item[key].as_str().unwrap_or("")).unwrap()).unwrap()
    };
    // Rename detection: one RENAMED item with its old path and similarity.
    let renamed = diff(json!({ "detectRenames": true })).await.expect("diff");
    let items = renamed["items"].as_array().unwrap();
    let mv = items
        .iter()
        .find(|i| path_of(i, "path") == "moved/a.txt")
        .expect("the moved file");
    assert_eq!(mv["status"], "DIFF_STATUS_RENAMED", "{mv}");
    assert_eq!(path_of(mv, "oldPath"), "a.txt");
    let similarity = mv["similarity"].as_u64().unwrap();
    assert!((90..100).contains(&similarity), "{similarity}");
    assert_eq!(
        (mv["additions"].as_u64(), mv["deletions"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(!items.iter().any(|i| path_of(i, "path") == "a.txt"));
    // Without it: a delete and an add.
    let plain = diff(json!({})).await.expect("diff");
    let plain = plain["items"].as_array().unwrap();
    let status = |path: &str| {
        plain
            .iter()
            .find(|i| path_of(i, "path") == path)
            .map(|i| i["status"].as_str().unwrap().to_string())
    };
    assert_eq!(status("a.txt").as_deref(), Some("DIFF_STATUS_DELETED"));
    assert_eq!(status("moved/a.txt").as_deref(), Some("DIFF_STATUS_ADDED"));
    assert_eq!(status("b.txt").as_deref(), Some("DIFF_STATUS_MODIFIED"));
    // Ignoring whitespace drops the re-spaced file's changes.
    let ws = diff(json!({ "ignoreWhitespace": true }))
        .await
        .expect("diff");
    let b = ws["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| path_of(i, "path") == "b.txt")
        .cloned();
    assert!(
        b.as_ref()
            .is_none_or(|b| b["hunks"].as_array().is_none_or(|h| h.is_empty())),
        "{b:?}"
    );
    // Large diffs are cut at max_files and say so on the last item.
    let cut = diff(json!({ "maxFiles": 3 })).await.expect("diff");
    let cut = cut["items"].as_array().unwrap();
    assert_eq!(cut.len(), 3);
    assert_eq!(cut[2]["truncated"], true);
    assert!(cut[..2].iter().all(|i| i["truncated"] != true));
    assert!(plain.len() > 3 && plain.iter().all(|i| i["truncated"] != true));
}

/// `git <args>` in `dir` with an isolated home; output and success.
fn git_in(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leaked_secret_is_remediated_by_redaction_on_every_read_path() {
    // A server where push protection is off: the leak gets in, as on any forge.
    let t = start_with(|c| c.push_protection.enabled = false).await;
    let secret = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id().starts_with("github") && r.confidence().blocks())
        .and_then(|r| r.example().map(str::to_string))
        .expect("a github rule example");
    // The leak: push protection is off and a credential lands on main.
    t.call(
        "SecurityService",
        "UpdateSecurityConfiguration",
        json!({ "repo": repo(), "secretScanningPushProtection": false }),
    )
    .await
    .expect("push protection off");
    let (root, _) = t.tip("main").await;
    let cs = t
        .commit(
            &root,
            &[("config.yml", secret.as_bytes()), ("README.md", b"fine\n")],
            750,
            "add config",
        )
        .await;
    let head = id_text(&cs["id"]);
    t.land(&head).await.expect("land");
    let blob = bee_object::blob_id(secret.as_bytes()).to_text();
    assert_eq!(
        t.read("config.yml", "main").await.unwrap(),
        secret.as_bytes()
    );
    // Readable everywhere before remediation: git and a source archive.
    let work = tempfile::tempdir().unwrap();
    let enc: String = t
        .token
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let url = format!("http://git:{enc}@{}/dev/playground.git", t.srv.addr);
    let (ok, out) = git_in(work.path(), &["clone", "-q", &url, "before"]);
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::read(work.path().join("before/config.yml")).unwrap(),
        secret.as_bytes()
    );
    let archive = t
        .call(
            "ReleaseService",
            "GetSourceArchiveUrl",
            json!({ "repo": repo(), "at": { "expr": "main" }, "format": "SOURCE_ARCHIVE_FORMAT_ZIP" }),
        )
        .await
        .expect("archive url");
    let archive_url = archive["url"].as_str().unwrap().to_string();
    let (status, _, zip) = http("GET", &archive_url, &[], Vec::new()).await;
    assert_eq!(status, 200);
    assert!(
        zip.windows(secret.len()).any(|w| w == secret.as_bytes())
            || bee_forge::packages::archive::zip_entries(&zip, 1 << 20)
                .unwrap()
                .iter()
                .any(|e| e.data == secret.as_bytes()),
        "the archive carries the leak"
    );
    // Remediation: redact the blob. History and ids stay; the bytes are gone.
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leaked github token" }),
    )
    .await
    .expect("redact");
    let e = t.read("config.yml", "main").await.expect_err("redacted");
    assert_eq!(e.reason, "REDACTED");
    assert_eq!(t.read("README.md", "main").await.unwrap(), b"fine\n");
    assert_eq!(
        t.tip("main").await.0,
        head,
        "no history rewrite: ids are unchanged"
    );
    let (ok, out) = git_in(work.path(), &["clone", "-q", &url, "after"]);
    assert!(!ok, "git cannot serve the quarantined blob");
    // In quarantine the bytes are set aside (the store reports them unavailable).
    assert!(
        out.contains("REDACTED") || out.contains("MISSING_OBJECT"),
        "{out}"
    );
    // The archive built before the redaction is not served again.
    let (status, _, body) = http("GET", &archive_url, &[], Vec::new()).await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("REDACTED"));
    // Purge: the bytes are deleted for good and cannot be uploaded again.
    t.call(
        "AdminService",
        "PurgeNow",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "purge the leak" }),
    )
    .await
    .expect("purge");
    let mut again =
        json!({ "id": id_json(&blob), "offset": "0", "data": B64.encode(&secret), "last": true });
    again["repo"] = repo();
    let e = t
        .call(
            "WriteService",
            "UploadObjects",
            json!({ "messages": [again] }),
        )
        .await
        .expect_err("a purged object cannot come back");
    assert_eq!(e.reason, "REDACTED", "{e:?}");
    let (ok, out) = git_in(work.path(), &["clone", "-q", &url, "purged"]);
    assert!(!ok && out.contains("REDACTED"), "{out}");
    assert_eq!(t.tip("main").await.0, head);
}
