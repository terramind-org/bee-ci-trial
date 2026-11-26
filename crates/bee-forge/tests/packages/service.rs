//! `PackageService`: the permission matrix, grants, deletion and restore, native publish,
//! policy, retention, usage, events, and the AS-PKG scenario.

use bee_forge::Cx;
use bee_forge::packages::model::RetentionRec;
use bee_proto::v1::package_service_server::PackageService;
use bytes::Bytes;
use serde_json::json;

use super::support::*;

const NPM: i32 = PackageEcosystem::Npm as i32;
const CONTAINER: i32 = PackageEcosystem::Container as i32;

/// Publishes one version natively (BeginPackageUpload, upload to the signed targets,
/// FinalizePackageVersion). Returns the version.
pub async fn native_publish(
    w: &World,
    tok: &str,
    name: &str,
    version: &str,
    link: Option<&str>,
    vis: PackageVisibility,
) -> Result<PackageVersion, tonic::Status> {
    let data = format!("{name}@{version} tarball").into_bytes();
    let file = format!("{name}-{version}.tgz");
    let begin =
        w.s.begin_package_upload(req(
            tok,
            BeginPackageUploadRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: name.into(),
                version: version.into(),
                files: vec![PackageUploadFile {
                    name: file.clone(),
                    size_bytes: data.len() as u64,
                    digest: sha256_digest(&data),
                    media_type: "application/octet-stream".into(),
                }],
                metadata_json: json!({"name": name, "version": version}).to_string(),
                link_repo: link.and_then(rref),
                visibility: vis as i32,
            },
        ))
        .await?
        .into_inner();
    assert_eq!(begin.targets.len(), 1);
    // The signed URL names the blob key; the server's blob store would take the PUT.
    let url = &begin.targets[0].upload_url;
    let key = url
        .strip_prefix("https://blobs.beehive.invalid/")
        .unwrap()
        .split('?')
        .next()
        .unwrap();
    use bee_forge::packages::BlobPort;
    w.blobs.put(key, Bytes::from(data)).await.unwrap();
    w.s.finalize_package_version(req(
        tok,
        FinalizePackageVersionRequest {
            owner: "acme".into(),
            upload_id: begin.upload_id,
            tags: vec!["latest".into()],
        },
    ))
    .await
    .map(|r| r.into_inner())
}

fn get_req(name: &str) -> GetPackageRequest {
    GetPackageRequest {
        owner: "acme".into(),
        ecosystem: NPM,
        name: name.into(),
    }
}

async fn can_get(w: &World, who: &str, name: &str) -> String {
    match w.s.get_package(req(&w.tok(who), get_req(name))).await {
        Ok(_) => "OK".into(),
        Err(e) => bee_proto::errors::detail_from_status(&e)
            .map(|d| d.reason)
            .unwrap_or_default(),
    }
}

#[tokio::test]
async fn read_permission_matrix() {
    let w = world().await;
    let o = w.tok("owner");
    native_publish(
        &w,
        &o,
        "unlinked",
        "1.0.0",
        None,
        PackageVisibility::Private,
    )
    .await
    .unwrap();
    native_publish(
        &w,
        &o,
        "linked",
        "1.0.0",
        Some("acme/app"),
        PackageVisibility::Unspecified,
    )
    .await
    .unwrap();
    native_publish(
        &w,
        &o,
        "open-pkg",
        "1.0.0",
        Some("acme/open"),
        PackageVisibility::Unspecified,
    )
    .await
    .unwrap();
    native_publish(
        &w,
        &o,
        "internal",
        "1.0.0",
        None,
        PackageVisibility::Private,
    )
    .await
    .unwrap();
    w.s.set_package_visibility(req(
        &o,
        SetPackageVisibilityRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "internal".into(),
            visibility: PackageVisibility::Internal as i32,
        },
    ))
    .await
    .unwrap();
    // open-pkg is linked to a public repo and inherits its visibility.
    let p =
        w.s.get_package(req(&o, get_req("open-pkg")))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(p.visibility, PackageVisibility::Public as i32);
    assert_eq!(p.registry_url, "https://beehive.test/npm/acme/");
    let table: [(&str, [&str; 5]); 4] = [
        // owner, admin, writer, reader, outsider
        (
            "unlinked",
            ["OK", "OK", "NOT_FOUND", "NOT_FOUND", "NOT_FOUND"],
        ),
        ("linked", ["OK", "OK", "OK", "OK", "NOT_FOUND"]),
        ("internal", ["OK", "OK", "OK", "OK", "NOT_FOUND"]),
        ("open-pkg", ["OK", "OK", "OK", "OK", "OK"]),
    ];
    for (name, want) in table {
        for (who, w_) in ["owner", "admin", "writer", "reader", "outsider"]
            .into_iter()
            .zip(want)
        {
            assert_eq!(can_get(&w, who, name).await, w_, "{who} reading {name}");
        }
    }
    // A token without `read` is policy denied.
    let t = w.env.token("owner", actions(&[bee_policy::Action::Commit]));
    assert_eq!(
        reason(w.s.get_package(req(&t, get_req("linked"))).await),
        "POLICY_DENIED"
    );
    // Listing shows only what the caller may read.
    let list = |who: &str| {
        let t = w.tok(who);
        let s = w.s.clone();
        async move {
            s.list_packages(req(
                &t,
                ListPackagesRequest {
                    owner: "acme".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .packages
            .into_iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
        }
    };
    assert_eq!(list("owner").await.len(), 4);
    let mut reader_list = list("reader").await;
    reader_list.sort();
    assert_eq!(reader_list, vec!["internal", "linked", "open-pkg"]);
    assert_eq!(list("outsider").await, vec!["open-pkg"]);
    let by_repo =
        w.s.list_packages(req(
            &o,
            ListPackagesRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_repo.packages.len(), 1);
    assert_eq!(by_repo.packages[0].name, "linked");
}

#[tokio::test]
async fn write_and_admin_permissions() {
    let w = world().await;
    let o = w.tok("owner");
    native_publish(
        &w,
        &o,
        "linked",
        "1.0.0",
        Some("acme/app"),
        PackageVisibility::Unspecified,
    )
    .await
    .unwrap();
    let tags = |who: &str| {
        let t = w.tok(who);
        let s = w.s.clone();
        async move {
            reason(
                s.set_package_version_tags(req(
                    &t,
                    SetPackageVersionTagsRequest {
                        owner: "acme".into(),
                        ecosystem: NPM,
                        name: "linked".into(),
                        version: "1.0.0".into(),
                        tags: vec!["beta".into()],
                    },
                ))
                .await,
            )
        }
    };
    // Write role on the repo allows tags; read does not; outsiders are hidden.
    let ok =
        w.s.set_package_version_tags(req(
            &w.tok("writer"),
            SetPackageVersionTagsRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "linked".into(),
                version: "1.0.0".into(),
                tags: vec!["beta".into(), "latest".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.tags, vec!["beta".to_string(), "latest".to_string()]);
    assert!(tags("reader").await.contains("PERMISSION_DENIED"));
    assert!(tags("outsider").await.contains("NOT_FOUND"));
    // Admin operations: delete, visibility, grants. A repo writer is not enough.
    let del = |who: &str| {
        let t = w.tok(who);
        let s = w.s.clone();
        async move {
            reason(
                s.delete_package(req(
                    &t,
                    DeletePackageRequest {
                        owner: "acme".into(),
                        ecosystem: NPM,
                        name: "linked".into(),
                    },
                ))
                .await,
            )
        }
    };
    assert!(del("writer").await.contains("PERMISSION_DENIED"));
    assert!(del("reader").await.contains("PERMISSION_DENIED"));
    let vis =
        w.s.set_package_visibility(req(
            &w.tok("writer"),
            SetPackageVisibilityRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "linked".into(),
                visibility: PackageVisibility::Public as i32,
            },
        ))
        .await;
    assert_eq!(reason(vis), "PERMISSION_DENIED");
    let g =
        w.s.grant_package_access(req(
            &w.tok("writer"),
            GrantPackageAccessRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "linked".into(),
                kind: PackageGranteeKind::Principal as i32,
                principal_id: "outsider".into(),
                role: PackageRole::Read as i32,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(g), "PERMISSION_DENIED");
    // Denied admin calls are audited as `package.*` actions.
    w.flush().await;
    let org = w
        .env
        .forge
        .orgs
        .get_org(&Cx::system(), "acme")
        .await
        .unwrap();
    let (entries, _) = w
        .env
        .forge
        .audit
        .query(
            &Cx::system(),
            &format!("org:{}", org.id),
            &Default::default(),
            false,
            "",
            200,
        )
        .await
        .unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.action == "package.delete" && e.result.contains("PERMISSION_DENIED")),
        "{:?}",
        entries
            .iter()
            .map(|e| (&e.action, &e.result))
            .collect::<Vec<_>>()
    );
    // Org admin may delete.
    assert!(
        w.s.delete_package(req(
            &w.tok("admin"),
            DeletePackageRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "linked".into()
            }
        ))
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn grants_for_principals_and_workflow_repos() {
    let w = world().await;
    let o = w.tok("owner");
    native_publish(&w, &o, "shared", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    assert_eq!(can_get(&w, "outsider", "shared").await, "NOT_FOUND");
    let grant = |role: PackageRole| {
        w.s.grant_package_access(req(
            &o,
            GrantPackageAccessRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "shared".into(),
                kind: PackageGranteeKind::Principal as i32,
                principal_id: "outsider".into(),
                role: role as i32,
                ..Default::default()
            },
        ))
    };
    grant(PackageRole::Read).await.unwrap();
    assert_eq!(can_get(&w, "outsider", "shared").await, "OK");
    let set_tags = |tok: &str| {
        let t = tok.to_string();
        let s = w.s.clone();
        async move {
            s.set_package_version_tags(req(
                &t,
                SetPackageVersionTagsRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "shared".into(),
                    version: "1.0.0".into(),
                    tags: vec!["x".into()],
                },
            ))
            .await
        }
    };
    assert_eq!(
        reason(set_tags(&w.tok("outsider")).await),
        "PERMISSION_DENIED"
    );
    grant(PackageRole::Write).await.unwrap();
    assert!(set_tags(&w.tok("outsider")).await.is_ok());
    let grants =
        w.s.list_package_access_grants(req(
            &o,
            ListPackageAccessGrantsRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "shared".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(grants.grants.len(), 1);
    assert_eq!(grants.grants[0].role, PackageRole::Write as i32);
    w.s.revoke_package_access(req(
        &o,
        RevokePackageAccessRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "shared".into(),
            kind: PackageGranteeKind::Principal as i32,
            principal_id: "outsider".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(can_get(&w, "outsider", "shared").await, "NOT_FOUND");
    assert_eq!(
        reason(
            w.s.revoke_package_access(req(
                &o,
                RevokePackageAccessRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "shared".into(),
                    kind: PackageGranteeKind::Principal as i32,
                    principal_id: "outsider".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // A repo grant lets workflows of that repo (job tokens scoped to it) use the package.
    let job_open = w.job_token(&w.open);
    let job_app = w.job_token(&w.app);
    assert_eq!(reason(set_tags(&job_open).await), "NOT_FOUND");
    w.s.grant_package_access(req(
        &o,
        GrantPackageAccessRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "shared".into(),
            kind: PackageGranteeKind::Repo as i32,
            grantee_repo: rref("acme/open"),
            role: PackageRole::Write as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert!(set_tags(&job_open).await.is_ok());
    // Other repos' job tokens and unrestricted-but-unprivileged tokens get nothing from it.
    assert_eq!(reason(set_tags(&job_app).await), "NOT_FOUND");
    assert_eq!(reason(set_tags(&w.tok("reader")).await), "NOT_FOUND");
}

#[tokio::test]
async fn deletion_restore_and_purge() {
    let w = world().await;
    let o = w.tok("owner");
    native_publish(&w, &o, "gone", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    native_publish(&w, &o, "gone", "1.1.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    let before = w.blobs.len();
    assert!(before >= 2);
    // Version delete and restore.
    let v =
        w.s.get_package_version(req(
            &o,
            GetPackageVersionRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "gone".into(),
                version: "1.0.0".into(),
                include_download_urls: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        v.files[0]
            .download_url
            .starts_with("https://blobs.beehive.invalid/")
    );
    w.s.delete_package_version(req(
        &o,
        DeletePackageVersionRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "gone".into(),
            version: "1.0.0".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.get_package_version(req(
                &o,
                GetPackageVersionRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "gone".into(),
                    version: "1.0.0".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let list =
        w.s.list_package_versions(req(
            &o,
            ListPackageVersionsRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "gone".into(),
                include_deleted: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.versions.len(), 2);
    assert!(list.versions.iter().any(|v| v.deleted_at.is_some()));
    // The deleted version string cannot be republished while it is restorable.
    assert_eq!(
        reason(
            native_publish(&w, &o, "gone", "1.0.0", None, PackageVisibility::Private)
                .await
                .map(|_| tonic::Response::new(()))
        ),
        "ALREADY_EXISTS"
    );
    let r =
        w.s.restore_package_version(req(
            &o,
            RestorePackageVersionRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "gone".into(),
                version_id: v.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(r.deleted_at.is_none());
    assert_eq!(
        reason(
            w.s.restore_package_version(req(
                &o,
                RestorePackageVersionRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "gone".into(),
                    version_id: v.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Package delete and restore; the name is taken again => ALREADY_EXISTS.
    w.s.delete_package(req(
        &o,
        DeletePackageRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "gone".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(can_get(&w, "owner", "gone").await, "NOT_FOUND");
    native_publish(&w, &o, "gone", "9.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    let restore = |s: bee_forge::service::ForgeServices, o: String| async move {
        s.restore_package(req(
            &o,
            RestorePackageRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "gone".into(),
            },
        ))
        .await
    };
    assert_eq!(
        reason(restore(w.s.clone(), o.clone()).await),
        "ALREADY_EXISTS"
    );
    w.s.delete_package(req(
        &o,
        DeletePackageRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "gone".into(),
        },
    ))
    .await
    .unwrap();
    let back = restore(w.s.clone(), o.clone()).await.unwrap().into_inner();
    assert!(back.deleted_at.is_none());
    assert!(back.version_count >= 1);
    // Past the 30-day window the package is not restorable and maintenance purges it.
    w.s.delete_package(req(
        &o,
        DeletePackageRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "gone".into(),
        },
    ))
    .await
    .unwrap();
    w.clock.advance(31 * 86_400_000);
    let o = w.tok("owner");
    assert_eq!(reason(restore(w.s.clone(), o.clone()).await), "NOT_FOUND");
    w.env.forge.maintain().await.unwrap();
    assert_eq!(w.blobs.len(), 0, "{:?}", w.blobs.keys());
}

#[tokio::test]
async fn deleting_popular_public_packages_is_refused() {
    let w = world().await;
    let o = w.tok("owner");
    native_publish(
        &w,
        &o,
        "pop",
        "1.0.0",
        Some("acme/open"),
        PackageVisibility::Unspecified,
    )
    .await
    .unwrap();
    native_publish(
        &w,
        &o,
        "pop",
        "1.1.0",
        Some("acme/open"),
        PackageVisibility::Unspecified,
    )
    .await
    .unwrap();
    for _ in 0..4 {
        let r = w
            .http(rq(
                "GET",
                "/npm/acme/pop/-/pop-1.0.0.tgz",
                None,
                &[],
                Bytes::new(),
            ))
            .await;
        assert_eq!(r.status(), 200);
    }
    // More than 3 downloads (the test limit): the package cannot be deleted.
    assert_eq!(
        reason(
            w.s.delete_package(req(
                &o,
                DeletePackageRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "pop".into()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // One version can go while another remains; the last popular one cannot.
    w.s.delete_package_version(req(
        &o,
        DeletePackageVersionRequest {
            owner: "acme".into(),
            ecosystem: NPM,
            name: "pop".into(),
            version: "1.1.0".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.delete_package_version(req(
                &o,
                DeletePackageVersionRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "pop".into(),
                    version: "1.0.0".into()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Usage figures.
    let u =
        w.s.get_package_usage(req(
            &o,
            GetPackageUsageRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "pop".into(),
                since: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u.download_count, 4);
    assert!(u.storage_bytes > 0);
    let since = bee_forge::ids::ts(T0 + 86_400_000 * 2);
    let u2 =
        w.s.get_package_usage(req(
            &o,
            GetPackageUsageRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "pop".into(),
                since: Some(since),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u2.download_count, 0);
}

#[tokio::test]
async fn native_publish_validates_and_verifies() {
    let w = world().await;
    let o = w.tok("owner");
    // Bad version string for the ecosystem: subcode bad_version.
    let e = native_publish(&w, &o, "n", "1.0", None, PackageVisibility::Private)
        .await
        .unwrap_err();
    let d = bee_proto::errors::detail_from_status(&e).unwrap();
    assert_eq!(
        (d.reason.as_str(), d.subcode.as_str()),
        ("INVALID_ARGUMENT", "bad_version")
    );
    native_publish(&w, &o, "n", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    // Versions are immutable.
    let e = native_publish(&w, &o, "n", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap_err();
    assert_eq!(
        bee_proto::errors::detail_from_status(&e).unwrap().reason,
        "ALREADY_EXISTS"
    );
    // A file that was not uploaded is MISSING_OBJECT; a corrupted one is INTEGRITY.
    let data = b"abc".to_vec();
    let begin = |ver: &str| {
        w.s.begin_package_upload(req(
            &o,
            BeginPackageUploadRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "n".into(),
                version: ver.into(),
                files: vec![PackageUploadFile {
                    name: "n.tgz".into(),
                    size_bytes: 3,
                    digest: sha256_digest(&data),
                    media_type: String::new(),
                }],
                ..Default::default()
            },
        ))
    };
    let b = begin("2.0.0").await.unwrap().into_inner();
    let fin = |id: &str| {
        w.s.finalize_package_version(req(
            &o,
            FinalizePackageVersionRequest {
                owner: "acme".into(),
                upload_id: id.into(),
                tags: vec![],
            },
        ))
    };
    assert_eq!(reason(fin(&b.upload_id).await), "MISSING_OBJECT");
    use bee_forge::packages::BlobPort;
    let key = b.targets[0]
        .upload_url
        .strip_prefix("https://blobs.beehive.invalid/")
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_string();
    w.blobs.put(&key, Bytes::from_static(b"abd")).await.unwrap();
    assert_eq!(reason(fin(&b.upload_id).await), "INTEGRITY");
    w.blobs
        .put(&key, Bytes::from_static(b"abcd"))
        .await
        .unwrap();
    assert_eq!(reason(fin(&b.upload_id).await), "INTEGRITY");
    w.blobs.put(&key, Bytes::from_static(b"abc")).await.unwrap();
    let v = fin(&b.upload_id).await.unwrap().into_inner();
    assert_eq!(v.version, "2.0.0");
    // A finished upload cannot be finalized twice.
    assert_eq!(reason(fin(&b.upload_id).await), "NOT_FOUND");
    // Quota: an owner over its storage quota gets QUOTA_EXCEEDED.
    let org = w
        .env
        .forge
        .packages
        .owner(&Cx::system(), "acme")
        .await
        .unwrap();
    w.env
        .forge
        .packages
        .set_owner_settings(&Cx::system(), &org, None, Some(5))
        .await
        .unwrap();
    assert_eq!(reason(begin("3.0.0").await), "QUOTA_EXCEEDED");
    // Provenance: none recorded for native publishes; a registry publish with run headers
    // records an unverified claim.
    assert_eq!(
        reason(
            w.s.get_package_version_provenance(req(
                &o,
                GetPackageVersionProvenanceRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "n".into(),
                    version: "1.0.0".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}

#[tokio::test]
async fn provenance_is_recorded_from_a_workflow_publish() {
    let w = world().await;
    let job = w.job_token(&w.app);
    let tgz = gzip(&tar(&[("package/package.json", b"{}")]));
    let doc = json!({
        "name": "prov", "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "prov", "version": "1.0.0", "repository": "https://beehive.test/acme/app"}},
        "_attachments": {"prov-1.0.0.tgz": {"data": bee_forge::packages::hash::b64_encode(&tgz), "length": tgz.len()}},
    });
    let r = w
        .http(rq(
            "PUT",
            "/npm/acme/prov",
            Some(&job),
            &[
                ("x-bee-run-id", "run-7"),
                ("x-bee-workflow", ".bee/workflows/release.yml"),
                ("x-bee-job", "publish"),
                ("x-bee-repo-id", &w.app.id),
                ("x-bee-ref", "main"),
            ],
            doc.to_string().into_bytes(),
        ))
        .await;
    assert_eq!(r.status(), 200, "{}", body_str(&r));
    let o = w.tok("owner");
    let pr =
        w.s.get_package_version_provenance(req(
            &o,
            GetPackageVersionProvenanceRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "prov".into(),
                version: "1.0.0".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(pr.workflow_run_id, "run-7");
    assert_eq!(pr.workflow_path, ".bee/workflows/release.yml");
    assert_eq!(pr.job_id, "publish");
    assert!(!pr.verified);
    let v =
        w.s.get_package_version(req(
            &o,
            GetPackageVersionRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "prov".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(v.has_provenance);
    assert_eq!(v.workflow_run_id, "run-7");
    assert_eq!(v.publisher.unwrap().id, "ci");
}

#[tokio::test]
async fn owner_policy_links_and_retention() {
    let w = world().await;
    let o = w.tok("owner");
    let forge = &w.env.forge;
    let org = forge.packages.owner(&Cx::system(), "acme").await.unwrap();
    // The org forbids public packages.
    forge
        .packages
        .set_owner_settings(&Cx::system(), &org, Some(true), None)
        .await
        .unwrap();
    native_publish(&w, &o, "p", "1.0.0", None, PackageVisibility::Private)
        .await
        .unwrap();
    assert_eq!(
        reason(
            w.s.set_package_visibility(req(
                &o,
                SetPackageVisibilityRequest {
                    owner: "acme".into(),
                    ecosystem: NPM,
                    name: "p".into(),
                    visibility: PackageVisibility::Public as i32
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    let e = native_publish(&w, &o, "q", "1.0.0", None, PackageVisibility::Public)
        .await
        .unwrap_err();
    assert_eq!(
        bee_proto::errors::detail_from_status(&e).unwrap().reason,
        "POLICY_DENIED"
    );
    // Link and unlink.
    let p =
        w.s.link_package_to_repo(req(
            &o,
            LinkPackageToRepoRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "p".into(),
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(p.inherit_access_from_repo);
    assert_eq!(can_get(&w, "reader", "p").await, "OK");
    let p =
        w.s.unlink_package_from_repo(req(
            &o,
            UnlinkPackageFromRepoRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "p".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(p.repo.is_none());
    assert_eq!(can_get(&w, "reader", "p").await, "NOT_FOUND");
    // Retention: keep the newest two versions; the rest are deleted but restorable.
    for v in ["1.1.0", "1.2.0", "1.3.0"] {
        native_publish(&w, &o, "p", v, None, PackageVisibility::Private)
            .await
            .unwrap();
    }
    let pkg = forge
        .packages
        .get(
            &Cx::system(),
            &org,
            bee_forge::packages::model::Eco::Npm,
            "p",
        )
        .await
        .unwrap();
    forge
        .packages
        .set_retention(
            &Cx::system(),
            &org,
            Some(&pkg),
            RetentionRec {
                keep_last: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // The newest version carries the `latest` tag and is protected; so is the tagged one.
    forge.maintain().await.unwrap();
    let live =
        w.s.list_package_versions(req(
            &o,
            ListPackageVersionsRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "p".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        live.versions.len(),
        2,
        "{:?}",
        live.versions.iter().map(|v| &v.version).collect::<Vec<_>>()
    );
    assert_eq!(live.versions[0].version, "1.3.0");
    let all =
        w.s.list_package_versions(req(
            &o,
            ListPackageVersionsRequest {
                owner: "acme".into(),
                ecosystem: NPM,
                name: "p".into(),
                include_deleted: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.versions.len(), 4);
}

#[tokio::test]
async fn as_pkg_01_publish_container_and_npm_from_a_workflow_then_consume() {
    let w = world().await;
    let job = w.job_token(&w.app);
    let hint = [("x-bee-repo", "acme/app"), ("x-bee-run-id", "run-1")];
    // Container: config, layer, manifest pushed with the job token.
    let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}".to_vec();
    let layer = b"layer".repeat(40);
    let mut digests = vec![];
    for blob in [&config, &layer] {
        let d = sha256_digest(blob);
        let r = w
            .http(rq(
                "POST",
                &format!("/v2/acme/svc/blobs/uploads/?digest={d}"),
                Some(&job),
                &hint,
                blob.clone(),
            ))
            .await;
        assert_eq!(r.status(), 201, "{}", body_str(&r));
        digests.push(d);
    }
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digests[0], "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digests[1], "size": layer.len()}],
    }))
    .unwrap();
    let r = w
        .http(rq(
            "PUT",
            "/v2/acme/svc/manifests/1.0.0",
            Some(&job),
            &[
                ("content-type", "application/vnd.oci.image.manifest.v1+json"),
                ("x-bee-run-id", "run-1"),
            ],
            manifest.clone(),
        ))
        .await;
    assert_eq!(r.status(), 201, "{}", body_str(&r));
    // npm package from the same workflow.
    let tgz = gzip(&tar(&[(
        "package/package.json",
        b"{\"name\":\"@acme/svc\"}",
    )]));
    let doc = json!({
        "name": "@acme/svc", "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "@acme/svc", "version": "1.0.0", "repository": "https://beehive.test/acme/app"}},
        "_attachments": {"svc-1.0.0.tgz": {"data": bee_forge::packages::hash::b64_encode(&tgz), "length": tgz.len()}},
    });
    let r = w
        .http(rq(
            "PUT",
            "/npm/acme/@acme%2Fsvc",
            Some(&job),
            &[("x-bee-run-id", "run-1")],
            doc.to_string().into_bytes(),
        ))
        .await;
    assert_eq!(r.status(), 200, "{}", body_str(&r));
    // Consume both as another member of the org with repo read access.
    let reader = w.tok("reader");
    let m = w
        .http(rq(
            "GET",
            "/v2/acme/svc/manifests/1.0.0",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(m.status(), 200);
    assert_eq!(m.body().as_ref(), manifest.as_slice());
    let b = w
        .http(rq(
            "GET",
            &format!("/v2/acme/svc/blobs/{}", digests[1]),
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(b.body().as_ref(), layer.as_slice());
    let t = w
        .http(rq(
            "GET",
            "/npm/acme/@acme/svc/-/svc-1.0.0.tgz",
            Some(&reader),
            &[],
            Bytes::new(),
        ))
        .await;
    assert_eq!(t.body().as_ref(), tgz.as_slice());
    // Both packages are linked to the repo and carry the run's provenance.
    let by_repo =
        w.s.list_packages(req(
            &reader,
            ListPackagesRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut names: Vec<String> = by_repo.packages.iter().map(|p| p.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["@acme/svc", "svc"]);
    for (eco, name, version) in [(CONTAINER, "svc", "1.0.0"), (NPM, "@acme/svc", "1.0.0")] {
        let pr =
            w.s.get_package_version_provenance(req(
                &reader,
                GetPackageVersionProvenanceRequest {
                    owner: "acme".into(),
                    ecosystem: eco,
                    name: name.into(),
                    version: version.into(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(pr.workflow_run_id, "run-1");
    }
    // Events reached the org log.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "package.published").count(),
        2,
        "{kinds:?}"
    );
}
