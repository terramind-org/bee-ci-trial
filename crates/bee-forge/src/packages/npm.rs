//! The npm registry (`/npm/<owner>/...`), written from the public registry documentation.
//!
//! - `PUT /<pkg>`: publish. The body is the packument with the new version in `versions`, the
//!   tarball base64 in `_attachments`, and `dist-tags`; `access` (`public` or `restricted`)
//!   sets the visibility of a new package and `repository` links it to a repo of this forge.
//!   Publishing over an existing version is `403` (versions are immutable). The same `PUT` with
//!   only existing versions applies `deprecated` messages and dist-tags (`npm deprecate`).
//! - `GET /<pkg>`: the packument; with `Accept: application/vnd.npm.install-v1+json` the
//!   abbreviated install metadata. `GET /<pkg>/<version-or-tag>`: one version.
//! - `GET /<pkg>/-/<file>.tgz`: the tarball (counts a download).
//! - dist-tags: `GET|PUT|DELETE /-/package/<pkg>/dist-tags[/<tag>]`.
//! - unpublish: `PUT /<pkg>/-rev/<rev>` with versions removed, `DELETE /<pkg>/-rev/<rev>` for
//!   the whole package. A version can be unpublished within 72 hours of publishing, or while it
//!   has fewer than 300 downloads (both configurable); older popular versions are `403` with
//!   advice to deprecate instead.
//! - `GET /-/whoami`, `GET /-/ping`, and empty answers for the audit endpoints.
//!
//! Scoped names arrive as `@scope%2Fname` or `@scope/name`; both work.

use std::collections::BTreeMap;

use bee_proto::v1::PackageVisibility;
use bytes::Bytes;
use serde_json::{Map, Value, json};

use super::access::Need;
use super::hash::{b64_decode, b64_encode, md5_hex, sha1_hex, sha256_digest, sha512_b64};
use super::http::{Req, Resp, header, iso8601, json as json_resp, pct_decode, resp, status_of};
use super::manage::{NewFile, NewVersion, OnConflict, Owner};
use super::model::{Eco, PackageRec, VersionRec};
use super::names::{SemVer, validate_name};
use super::registry::{CreateOpts, PackageRegistry, owner_and_rest};
use crate::error::{ForgeError, Result};

fn npm_err(e: &ForgeError) -> Resp {
    let status = match (e.reason, e.subcode.as_str()) {
        (bee_proto::v1::ErrorReason::AlreadyExists, "version_exists" | "version_deleted") => 403,
        _ => status_of(e),
    };
    let mut headers = vec![("content-type", "application/json")];
    if status == 401 {
        headers.push(("www-authenticate", "Bearer realm=\"Beehive npm\""));
    }
    resp(
        status,
        &headers,
        json!({"error": e.message, "reason": e.reason_name()})
            .to_string()
            .into_bytes(),
    )
}

/// Serves one npm request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    match route(reg, req).await {
        Ok(r) => r,
        Err(e) => npm_err(&e),
    }
}

/// The package name and remaining segments of a path below the owner.
fn split_pkg<'a>(segs: &[&'a str]) -> Option<(String, Vec<&'a str>)> {
    let first = *segs.first()?;
    let dec = pct_decode(first);
    if dec.starts_with('@') {
        if dec.contains('/') {
            return Some((dec, segs[1..].to_vec()));
        }
        if segs.len() >= 2 && segs[1] != "-" && segs[1] != "-rev" {
            return Some((format!("{dec}/{}", pct_decode(segs[1])), segs[2..].to_vec()));
        }
        return Some((dec, segs[1..].to_vec()));
    }
    Some((dec, segs[1..].to_vec()))
}

fn unscoped(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let path = req.uri().path().to_string();
    let Some((owner, segs)) = owner_and_rest(&path, "/npm/") else {
        return Err(ForgeError::not_found("registry path"));
    };
    let method = req.method().as_str().to_string();
    match segs.as_slice() {
        ["-", "ping"] => return Ok(json_resp(200, &json!({}))),
        ["-", "whoami"] => {
            let c = reg.require_caller(&req).await?;
            return Ok(json_resp(200, &json!({"username": c.cx.actor_id})));
        }
        ["-", "npm", "v1", "security", ..] => return Ok(json_resp(200, &json!({}))),
        ["-", "v1", "search"] => {
            return Ok(json_resp(
                200,
                &json!({"objects": [], "total": 0, "time": ""}),
            ));
        }
        ["-", "user", ..] => {
            return Err(ForgeError::invalid(
                "unsupported",
                "npm login is not supported; create an access token and set `_authToken`",
            ));
        }
        ["-", "package", rest @ ..] => {
            let Some((name, tail)) = split_pkg(rest) else {
                return Err(ForgeError::not_found("package"));
            };
            if tail.first() != Some(&"dist-tags") {
                return Err(ForgeError::not_found("endpoint"));
            }
            return dist_tags(reg, &req, &owner, &name, &tail[1..], &method).await;
        }
        [] => return Err(ForgeError::not_found("package")),
        _ => {}
    }
    let Some((name, rest)) = split_pkg(&segs) else {
        return Err(ForgeError::not_found("package"));
    };
    match (rest.as_slice(), method.as_str()) {
        ([], "GET" | "HEAD") => packument(reg, &req, &owner, &name).await,
        ([], "PUT") => publish(reg, &req, &owner, &name, false).await,
        (["-rev", _rev], "PUT") => publish(reg, &req, &owner, &name, true).await,
        (["-rev", _rev], "DELETE") => unpublish_package(reg, &req, &owner, &name).await,
        (["-", file], "GET" | "HEAD") => {
            tarball(reg, &req, &owner, &name, file, method == "HEAD").await
        }
        (["-", _file, "-rev", _rev], "DELETE") => Ok(json_resp(200, &json!({"ok": true}))),
        ([v], "GET" | "HEAD") => version_doc(reg, &req, &owner, &name, &pct_decode(v)).await,
        _ => Err(ForgeError::invalid(
            "unsupported",
            "endpoint is not supported",
        )),
    }
}

fn tarball_name(name: &str, version: &str) -> String {
    format!("{}-{version}.tgz", unscoped(name))
}

fn version_object(
    reg: &PackageRegistry,
    owner: &str,
    name: &str,
    v: &VersionRec,
    file: Option<&super::model::FileRec>,
) -> Map<String, Value> {
    let mut o: Map<String, Value> = serde_json::from_str::<Value>(&v.metadata_json)
        .ok()
        .and_then(|x| x.as_object().cloned())
        .unwrap_or_default();
    o.insert("name".into(), json!(name));
    o.insert("version".into(), json!(v.version));
    o.insert("_id".into(), json!(format!("{name}@{}", v.version)));
    if let Some(f) = file {
        let base = reg.pk().config().base_url.trim_end_matches('/').to_string();
        o.insert(
            "dist".into(),
            json!({
                "shasum": f.sha1,
                "integrity": format!("sha512-{}", f.sha512_b64),
                "tarball": format!("{base}/npm/{owner}/{name}/-/{}", f.name),
            }),
        );
    }
    if !v.deprecated.is_empty() {
        o.insert("deprecated".into(), json!(v.deprecated));
    }
    o
}

async fn packument(reg: &PackageRegistry, req: &Req, owner: &str, name: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (o, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Npm, name).await?;
    let pk = reg.pk();
    let versions = pk.all_versions(&cx, &p).await?;
    let tags = pk.tags(&cx, &p).await?;
    let by_id: BTreeMap<&str, &VersionRec> = versions.iter().map(|v| (v.id.as_str(), v)).collect();
    let mut dist_tags = Map::new();
    for (t, vid) in &tags {
        if let Some(v) = by_id.get(vid.as_str()) {
            dist_tags.insert(t.clone(), json!(v.version));
        }
    }
    let abbreviated = header(req, "accept").contains("vnd.npm.install-v1+json");
    let mut vmap = Map::new();
    let mut time = Map::new();
    time.insert("created".into(), json!(iso8601(p.created_ms)));
    time.insert("modified".into(), json!(iso8601(p.updated_ms)));
    for v in versions.iter().rev() {
        let file = pk.file(&cx, v, &tarball_name(name, &v.version)).await?;
        let mut obj = version_object(reg, &o.name, name, v, file.as_ref());
        if abbreviated {
            let keep = [
                "name",
                "version",
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
                "peerDependenciesMeta",
                "bin",
                "directories",
                "engines",
                "cpu",
                "os",
                "funding",
                "dist",
                "deprecated",
                "_hasShrinkwrap",
                "hasInstallScript",
            ];
            obj.retain(|k, _| keep.contains(&k.as_str()));
        }
        time.insert(v.version.clone(), json!(iso8601(v.created_ms)));
        vmap.insert(v.version.clone(), Value::Object(obj));
    }
    let body = if abbreviated {
        json!({
            "name": name, "modified": iso8601(p.updated_ms),
            "dist-tags": dist_tags, "versions": vmap,
        })
    } else {
        json!({
            "_id": name, "_rev": rev_of(&p), "name": name, "description": p.description,
            "dist-tags": dist_tags, "versions": vmap, "time": time,
        })
    };
    let ct = if abbreviated {
        "application/vnd.npm.install-v1+json"
    } else {
        "application/json"
    };
    Ok(resp(
        200,
        &[("content-type", ct)],
        body.to_string().into_bytes(),
    ))
}

fn rev_of(p: &PackageRec) -> String {
    format!(
        "{}-{}",
        p.version_count,
        &sha1_hex(p.updated_ms.to_string().as_bytes())[..16]
    )
}

async fn version_doc(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    reference: &str,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (o, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Npm, name).await?;
    let v = reg.pk().version(&cx, &p, reference).await?;
    let file = reg
        .pk()
        .file(&cx, &v, &tarball_name(name, &v.version))
        .await?;
    Ok(json_resp(
        200,
        &Value::Object(version_object(reg, &o.name, name, &v, file.as_ref())),
    ))
}

async fn tarball(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    file: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Npm, name).await?;
    let version = file
        .strip_prefix(&format!("{}-", unscoped(name)))
        .and_then(|f| f.strip_suffix(".tgz"))
        .ok_or_else(|| ForgeError::not_found(format!("tarball {file}")))?;
    let v = reg.pk().version(&cx, &p, version).await?;
    let f = reg
        .pk()
        .file(&cx, &v, file)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("tarball {file}")))?;
    let len = f.size.to_string();
    let headers = [
        ("content-type", "application/octet-stream"),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &headers, Bytes::new()));
    }
    let data = reg.pk().read_file(&f).await?;
    reg.count_download(&p, &v.id).await;
    Ok(resp(200, &headers, data))
}

fn body_json(req: &Req) -> Result<Value> {
    serde_json::from_slice(req.body())
        .map_err(|e| ForgeError::invalid("bad_body", format!("the body is not JSON: {e}")))
}

fn repository_url(v: &Value) -> Option<String> {
    match v.get("repository")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("url").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

async fn publish(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    rev_route: bool,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    validate_name(Eco::Npm, name)?;
    let doc = body_json(req)?;
    if let Some(n) = doc.get("name").and_then(Value::as_str)
        && n != name
    {
        return Err(ForgeError::invalid(
            "bad_name",
            format!("the body names `{n}` but the URL names `{name}`"),
        ));
    }
    let versions = doc
        .get("versions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let attachments = doc
        .get("_attachments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let lookup = reg.lookup_cx();
    let o = reg.pk().owner(&lookup, owner).await?;
    let existing = reg.pk().find(&lookup, &o, Eco::Npm, name).await?;
    let first = versions.values().next().cloned().unwrap_or(Value::Null);
    let opts = CreateOpts {
        visibility: match doc.get("access").and_then(Value::as_str) {
            Some("public") => Some(PackageVisibility::Public),
            Some("restricted") => Some(PackageVisibility::Private),
            _ => None,
        },
        link: repository_url(&first).and_then(|u| reg.repo_from_url(&u)),
        description: first
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    let (o, p, cx, _created) = if existing.is_none() && versions.is_empty() {
        return Err(ForgeError::not_found(format!("package {name}")));
    } else {
        reg.publish_target(&caller, owner, Eco::Npm, name, &opts)
            .await?
    };
    let pk = reg.pk();
    let stored = pk.all_versions(&cx, &p).await?;
    let stored_keys: Vec<String> = stored.iter().map(|v| v.version.clone()).collect();
    let mut published: Vec<String> = Vec::new();
    for (ver, vobj) in &versions {
        if stored_keys.contains(ver) {
            if !rev_route && attachments.contains_key(&tarball_name(name, ver)) {
                return Err(
                    ForgeError::already_exists(format!("version {ver} of {name}"))
                        .with_subcode("version_exists"),
                );
            }
            // Metadata update: deprecation.
            if let Some(d) = vobj.get("deprecated").and_then(Value::as_str) {
                pk.update_version_flags(&cx, &o, &p, ver, Some(d.to_string()), None)
                    .await?;
            }
            continue;
        }
        if rev_route {
            continue;
        }
        if SemVer::parse(ver).is_none() {
            return Err(ForgeError::invalid(
                "bad_version",
                format!("`{ver}` is not a valid semver version"),
            ));
        }
        if let Some(n) = vobj.get("version").and_then(Value::as_str)
            && n != ver
        {
            return Err(ForgeError::invalid(
                "bad_version",
                "the version object disagrees with its key",
            ));
        }
        let fname = tarball_name(name, ver);
        let att = attachments.get(&fname).ok_or_else(|| {
            ForgeError::invalid(
                "missing_attachment",
                format!("no `_attachments` entry `{fname}` for version {ver}"),
            )
        })?;
        let data = att
            .get("data")
            .and_then(Value::as_str)
            .and_then(b64_decode)
            .ok_or_else(|| {
                ForgeError::invalid("bad_attachment", "attachment data is not base64")
            })?;
        if let Some(l) = att.get("length").and_then(Value::as_u64)
            && l != data.len() as u64
        {
            return Err(ForgeError::invalid(
                "bad_attachment",
                "attachment length does not match its data",
            ));
        }
        if data.is_empty() {
            return Err(ForgeError::invalid(
                "bad_attachment",
                "the tarball is empty",
            ));
        }
        let digest = sha256_digest(&data);
        let key = super::Packages::content_key(&o.id, &digest);
        let data = Bytes::from(data);
        pk.ports.blobs.put(&key, data.clone()).await?;
        let mut meta = vobj.as_object().cloned().unwrap_or_default();
        for k in ["dist", "_id", "_nodeVersion", "_npmVersion", "gitHead_"] {
            meta.remove(k);
        }
        let mut nv = NewVersion::new(ver);
        nv.digest = digest.clone();
        nv.files = vec![NewFile {
            name: fname,
            size: data.len() as u64,
            digest,
            media_type: "application/octet-stream".into(),
            blob_key: key,
            sha1: sha1_hex(&data),
            md5: md5_hex(&data),
            sha512_b64: sha512_b64(&data),
        }];
        nv.metadata_json = Value::Object(meta).to_string();
        nv.on_conflict = OnConflict::Reject;
        nv.provenance = PackageRegistry::claim(req);
        let dist_tags = doc.get("dist-tags").and_then(Value::as_object);
        let mut tags: Vec<String> = dist_tags
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| v.as_str() == Some(ver.as_str()))
                    .map(|(t, _)| t.clone())
                    .collect()
            })
            .unwrap_or_default();
        if tags.is_empty() && dist_tags.is_none_or(|m| m.is_empty()) {
            tags.push("latest".into());
        }
        nv.tags = tags;
        pk.publish(&cx, &o, &p, nv).await?;
        published.push(ver.clone());
    }
    // Versions removed by a `-rev` PUT are unpublished.
    if rev_route {
        for v in &stored {
            if !versions.contains_key(&v.version) {
                unpublish_version(reg, &cx, &o, &p, v).await?;
            }
        }
    }
    // Dist-tags of the body that point at stored versions.
    if let Some(tags) = doc.get("dist-tags").and_then(Value::as_object) {
        let current = pk.tags(&cx, &p).await?;
        for (t, vv) in tags {
            let Some(vv) = vv.as_str() else { continue };
            if published.iter().any(|x| x == vv) {
                continue;
            }
            let Ok(v) = pk.version(&cx, &p, vv).await else {
                continue;
            };
            if current.iter().any(|(ct, cv)| ct == t && *cv == v.id) {
                continue;
            }
            let mut ts = pk.version_tags(&cx, &p, &v.id).await?;
            ts.push(t.clone());
            pk.set_tags(&cx, &o, &p, &v.id, &ts).await?;
        }
    }
    Ok(json_resp(
        200,
        &json!({"ok": true, "id": name, "success": true, "published": published}),
    ))
}

async fn unpublish_version(
    reg: &PackageRegistry,
    cx: &crate::cx::Cx,
    o: &Owner,
    p: &PackageRec,
    v: &VersionRec,
) -> Result<()> {
    let pk = reg.pk();
    let cfg = pk.config();
    let downloads = pk
        .usage(cx, p, None)
        .await?
        .versions
        .iter()
        .find(|u| u.version_id == v.id)
        .map(|u| u.download_count)
        .unwrap_or(0);
    let age_h = pk.db().now_ms().saturating_sub(v.created_ms) / 3_600_000;
    if age_h > cfg.npm_unpublish_window_hours && downloads >= cfg.npm_unpublish_download_limit {
        return Err(ForgeError::new(
            bee_proto::v1::ErrorReason::PermissionDenied,
            format!(
                "{}@{} is older than {} hours and has {downloads} downloads; deprecate it instead",
                p.name, v.version, cfg.npm_unpublish_window_hours
            ),
        )
        .with_subcode("unpublish_not_allowed"));
    }
    pk.delete_version(cx, o, p, &v.id).await?;
    Ok(())
}

async fn unpublish_package(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Npm, name, Need::Admin)
        .await?;
    let pk = reg.pk();
    let versions = pk.all_versions(&cx, &p).await?;
    for v in &versions {
        let cfg = pk.config();
        let downloads = pk
            .usage(&cx, &p, None)
            .await?
            .versions
            .iter()
            .find(|u| u.version_id == v.id)
            .map(|u| u.download_count)
            .unwrap_or(0);
        let age_h = pk.db().now_ms().saturating_sub(v.created_ms) / 3_600_000;
        if age_h > cfg.npm_unpublish_window_hours && downloads >= cfg.npm_unpublish_download_limit {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::PermissionDenied,
                format!(
                    "{}@{} cannot be unpublished; deprecate the package instead",
                    p.name, v.version
                ),
            )
            .with_subcode("unpublish_not_allowed"));
        }
    }
    pk.delete(&cx, &o, &p).await?;
    Ok(json_resp(200, &json!({"ok": true})))
}

async fn dist_tags(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    tail: &[&str],
    method: &str,
) -> Result<Resp> {
    let pk = reg.pk();
    match (tail, method) {
        ([], "GET") => {
            let caller = reg.caller(req).await?;
            let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Npm, name).await?;
            let versions = pk.all_versions(&cx, &p).await?;
            let mut m = Map::new();
            for (t, vid) in pk.tags(&cx, &p).await? {
                if let Some(v) = versions.iter().find(|v| v.id == vid) {
                    m.insert(t, json!(v.version));
                }
            }
            Ok(json_resp(200, &Value::Object(m)))
        }
        ([tag], "PUT") => {
            let caller = reg.require_caller(req).await?;
            let (o, p, cx) = reg
                .pkg_for(Some(&caller), owner, Eco::Npm, name, Need::Write)
                .await?;
            let tag = pct_decode(tag);
            let ver: String = serde_json::from_slice(req.body()).map_err(|_| {
                ForgeError::invalid("bad_body", "the body is a JSON string with the version")
            })?;
            let v = pk.version(&cx, &p, &ver).await?;
            let mut ts = pk.version_tags(&cx, &p, &v.id).await?;
            if !ts.contains(&tag) {
                ts.push(tag);
            }
            pk.set_tags(&cx, &o, &p, &v.id, &ts).await?;
            Ok(json_resp(200, &json!({"ok": true})))
        }
        ([tag], "DELETE") => {
            let caller = reg.require_caller(req).await?;
            let (o, p, cx) = reg
                .pkg_for(Some(&caller), owner, Eco::Npm, name, Need::Write)
                .await?;
            let tag = pct_decode(tag);
            if tag == "latest" {
                return Err(ForgeError::invalid(
                    "bad_tag",
                    "the `latest` dist-tag cannot be removed",
                ));
            }
            pk.delete_tag(&cx, &o, &p, &tag).await?;
            Ok(json_resp(200, &json!({"ok": true})))
        }
        _ => Err(ForgeError::invalid(
            "unsupported",
            "endpoint is not supported",
        )),
    }
}

/// Used by tests and the server docs: the base64 of a tarball for `_attachments`.
pub fn attachment_data(tarball: &[u8]) -> String {
    b64_encode(tarball)
}
