//! OCI Distribution v1.1 (`/v2/...`), written from the public distribution specification.
//!
//! Repository `<owner>/<name>` is the package `<name>` of container ecosystem under `<owner>`;
//! `<name>` may contain further `/` components. Supported:
//!
//! - blobs: `HEAD`/`GET`/`DELETE`; uploads by `POST` (start, monolithic with `?digest=`,
//!   cross-repo `?mount=&from=`), chunked `PATCH` (ordered, `Content-Range` checked), closing
//!   `PUT ?digest=` (digest verified), status `GET`, `DELETE`;
//! - manifests: `PUT` (digest checked, referenced blobs and child manifests must exist; image
//!   manifests, image indexes, Docker v2 manifests and lists, artifact manifests with `subject`),
//!   `GET`/`HEAD` by tag or digest, `DELETE` by digest (version, restorable) or tag;
//! - `GET tags/list` with `n` and `last` pagination, `GET referrers/<digest>` with the
//!   `artifactType` filter.
//!
//! Every manifest is a package version keyed by its digest; tags are tags. Layers and the config
//! are the version's files, shared by reference counting across versions of an owner.
//! Deletion is soft (30-day restore); blobs are garbage-collected by retention.

use bytes::Bytes;
use serde_json::{Value, json};

use super::access::Need;
use super::hash::sha256_digest;
use super::http::{Req, Resp, empty, header, json, query_get, resp, segments, status_of};
use super::manage::{NewFile, NewVersion, OnConflict};
use super::model::{Eco, PackageRec, PlatformRec};
use super::names::{is_digest, validate_name};
use super::registry::{CreateOpts, PackageRegistry};
use crate::error::{ForgeError, Result};

const MAX_MANIFEST: usize = 4 << 20;
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

enum Route {
    Root,
    Catalog,
    UploadStart(String),
    Upload(String, String),
    Blob(String, String),
    Manifest(String, String),
    Tags(String),
    Referrers(String, String),
}

fn parse(path: &str) -> Option<Route> {
    if path == "/v2" || path == "/v2/" {
        return Some(Route::Root);
    }
    let segs = segments(path, "/v2/");
    if segs == ["_catalog"] {
        return Some(Route::Catalog);
    }
    let n = segs.len();
    let name = |k: usize| segs[..k].join("/");
    match segs.as_slice() {
        [.., "blobs", "uploads"] if n >= 3 => Some(Route::UploadStart(name(n - 2))),
        [.., "blobs", "uploads", id] if n >= 4 => {
            Some(Route::Upload(name(n - 3), (*id).to_string()))
        }
        [.., "blobs", d] if n >= 3 => Some(Route::Blob(name(n - 2), (*d).to_string())),
        [.., "manifests", r] if n >= 3 => Some(Route::Manifest(name(n - 2), (*r).to_string())),
        [.., "tags", "list"] if n >= 3 => Some(Route::Tags(name(n - 2))),
        [.., "referrers", d] if n >= 3 => Some(Route::Referrers(name(n - 2), (*d).to_string())),
        _ => None,
    }
}

fn oci_err(e: &ForgeError) -> Resp {
    let code = match e.subcode.as_str() {
        "blob_unknown" => "BLOB_UNKNOWN",
        "manifest_unknown" => "MANIFEST_UNKNOWN",
        "manifest_invalid" => "MANIFEST_INVALID",
        "manifest_blob_unknown" => "MANIFEST_BLOB_UNKNOWN",
        "digest_invalid" => "DIGEST_INVALID",
        "name_invalid" | "bad_name" => "NAME_INVALID",
        "tag_invalid" | "bad_tag" => "TAG_INVALID",
        "blob_upload_unknown" => "BLOB_UPLOAD_UNKNOWN",
        "range_not_satisfiable" => "BLOB_UPLOAD_INVALID",
        "unsupported" => "UNSUPPORTED",
        _ => match status_of(e) {
            401 => "UNAUTHORIZED",
            403 => "DENIED",
            404 => "NAME_UNKNOWN",
            413 => "SIZE_INVALID",
            429 => "TOOMANYREQUESTS",
            400 => "MANIFEST_INVALID",
            _ => "UNKNOWN",
        },
    };
    let status = if e.subcode == "range_not_satisfiable" {
        416
    } else if e.subcode == "unsupported" {
        405
    } else if e.subcode == "manifest_blob_unknown" || e.subcode == "digest_invalid" {
        400
    } else {
        status_of(e)
    };
    let body = json!({"errors": [{"code": code, "message": e.message, "detail": null}]});
    let mut headers = vec![
        ("content-type", "application/json"),
        ("docker-distribution-api-version", "registry/2.0"),
    ];
    if status == 401 {
        headers.push((
            "www-authenticate",
            "Basic realm=\"Beehive packages\",charset=\"UTF-8\"",
        ));
    }
    resp(status, &headers, body.to_string().into_bytes())
}

fn named(code: &'static str, what: &str) -> ForgeError {
    ForgeError::not_found(what).with_subcode(code)
}

fn split_name(name: &str) -> Result<(&str, &str)> {
    let (owner, pkg) = name
        .split_once('/')
        .ok_or_else(|| ForgeError::invalid("name_invalid", "a repository is `<owner>/<name>`"))?;
    validate_name(Eco::Container, pkg)?;
    Ok((owner, pkg))
}

fn with_api(mut r: Resp) -> Resp {
    r.headers_mut().insert(
        "docker-distribution-api-version",
        http::HeaderValue::from_static("registry/2.0"),
    );
    r
}

/// Serves one OCI request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    match route(reg, req).await {
        Ok(r) => with_api(r),
        Err(e) => oci_err(&e),
    }
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let Some(r) = parse(req.uri().path()) else {
        return Err(named("name_unknown", "no such registry endpoint"));
    };
    let method = req.method().as_str().to_string();
    match (r, method.as_str()) {
        (Route::Root, "GET" | "HEAD") => {
            // 401 without a credential (so clients learn how to authenticate), 200 with a
            // valid one.
            reg.require_caller(&req).await?;
            Ok(json(200, &json!({})))
        }
        (Route::Catalog, _) => Err(ForgeError::invalid(
            "unsupported",
            "catalog listing is not supported; list packages through the API",
        )),
        (Route::UploadStart(n), "POST") => upload_start(reg, &req, &n).await,
        (Route::Upload(n, id), "PATCH") => upload_patch(reg, &req, &n, &id).await,
        (Route::Upload(n, id), "PUT") => upload_put(reg, &req, &n, &id).await,
        (Route::Upload(n, id), "GET") => upload_status(reg, &req, &n, &id).await,
        (Route::Upload(n, id), "DELETE") => upload_delete(reg, &req, &n, &id).await,
        (Route::Blob(n, d), "GET" | "HEAD") => blob_get(reg, &req, &n, &d, method == "HEAD").await,
        (Route::Blob(n, d), "DELETE") => blob_delete(reg, &req, &n, &d).await,
        (Route::Manifest(n, r), "PUT") => manifest_put(reg, req, &n, &r).await,
        (Route::Manifest(n, r), "GET" | "HEAD") => {
            manifest_get(reg, &req, &n, &r, method == "HEAD").await
        }
        (Route::Manifest(n, r), "DELETE") => manifest_delete(reg, &req, &n, &r).await,
        (Route::Tags(n), "GET") => tags_list(reg, &req, &n).await,
        (Route::Referrers(n, d), "GET") => referrers(reg, &req, &n, &d).await,
        _ => Err(ForgeError::invalid(
            "unsupported",
            "method not allowed on this endpoint",
        )),
    }
}

fn blob_location(name: &str, digest: &str) -> String {
    format!("/v2/{name}/blobs/{digest}")
}

fn upload_headers(name: &str, id: &str, size: u64) -> Vec<(String, String)> {
    vec![
        ("location".into(), format!("/v2/{name}/blobs/uploads/{id}")),
        ("docker-upload-uuid".into(), id.to_string()),
        ("range".into(), format!("0-{}", size.saturating_sub(1))),
    ]
}

fn with_headers(status: u16, h: &[(String, String)]) -> Resp {
    let hs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    resp(status, &hs, Bytes::new())
}

fn check_digest(d: &str) -> Result<()> {
    if is_digest(d) {
        Ok(())
    } else {
        Err(ForgeError::invalid(
            "digest_invalid",
            format!("`{d}` is not a sha256 digest"),
        ))
    }
}

/// Resolves a repository for a write; creates the package on first push.
async fn write_repo(
    reg: &PackageRegistry,
    req: &Req,
    name: &str,
) -> Result<(crate::packages::Owner, PackageRec, crate::cx::Cx)> {
    let caller = reg.require_caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (o, p, cx, _) = reg
        .publish_target(
            &caller,
            owner,
            Eco::Container,
            pkg,
            &CreateOpts {
                link: PackageRegistry::repo_hint(req),
                ..CreateOpts::default()
            },
        )
        .await?;
    Ok((o, p, cx))
}

async fn upload_start(reg: &PackageRegistry, req: &Req, name: &str) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, req, name).await?;
    let pk = reg.pk();
    if let (Some(d), Some(from)) = (query_get(req, "mount"), query_get(req, "from")) {
        check_digest(&d)?;
        let caller = reg.require_caller(req).await?;
        if let Ok((o2, src, _)) = {
            let (fo, fp) = split_name(&from)?;
            reg.pkg_for(Some(&caller), fo, Eco::Container, fp, Need::Read)
                .await
        } && o2.id == owner.id
            && pk.mount_blob(&cx, &owner, &src, &p, &d).await?
        {
            return Ok(with_headers(
                201,
                &[
                    ("location".into(), blob_location(name, &d)),
                    ("docker-content-digest".into(), d),
                ],
            ));
        }
    }
    if let Some(d) = query_get(req, "digest") {
        check_digest(&d)?;
        let body = req.body().clone();
        if sha256_digest(&body) != d {
            return Err(ForgeError::invalid(
                "digest_invalid",
                "the uploaded content does not match the digest",
            ));
        }
        pk.put_blob(&cx, &owner, &p, &d, body).await?;
        return Ok(with_headers(
            201,
            &[
                ("location".into(), blob_location(name, &d)),
                ("docker-content-digest".into(), d),
            ],
        ));
    }
    let u = pk.upload_start(&cx, &owner, &p).await?;
    let mut h = upload_headers(name, &u.id, 0);
    h[2].1 = "0-0".into();
    Ok(with_headers(202, &h))
}

fn parse_content_range(v: &str) -> Option<u64> {
    let v = v
        .trim()
        .trim_start_matches("bytes ")
        .trim_start_matches("bytes=");
    v.split('-').next()?.trim().parse().ok()
}

async fn upload_patch(reg: &PackageRegistry, req: &Req, name: &str, id: &str) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, req, name).await?;
    let start = match header(req, "content-range") {
        "" => None,
        v => Some(parse_content_range(v).ok_or_else(|| {
            ForgeError::invalid("range_not_satisfiable", "unreadable Content-Range")
        })?),
    };
    let u = reg
        .pk()
        .upload_append(&cx, &owner, &p, id, start, req.body().clone())
        .await
        .map_err(upload_unknown)?;
    Ok(with_headers(202, &upload_headers(name, id, u.size)))
}

fn upload_unknown(e: ForgeError) -> ForgeError {
    if e.reason == bee_proto::v1::ErrorReason::NotFound {
        e.with_subcode("blob_upload_unknown")
    } else {
        e
    }
}

async fn upload_put(reg: &PackageRegistry, req: &Req, name: &str, id: &str) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, req, name).await?;
    let d = query_get(req, "digest").ok_or_else(|| {
        ForgeError::invalid("digest_invalid", "closing an upload needs `?digest=`")
    })?;
    check_digest(&d)?;
    reg.pk()
        .upload_finish(&cx, &owner, &p, id, Some(req.body().clone()), &d)
        .await
        .map_err(upload_unknown)?;
    Ok(with_headers(
        201,
        &[
            ("location".into(), blob_location(name, &d)),
            ("docker-content-digest".into(), d),
        ],
    ))
}

async fn upload_status(reg: &PackageRegistry, req: &Req, name: &str, id: &str) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, req, name).await?;
    let u = reg
        .pk()
        .upload(&cx, &owner, &p, id)
        .await
        .map_err(upload_unknown)?;
    Ok(with_headers(204, &upload_headers(name, id, u.size)))
}

async fn upload_delete(reg: &PackageRegistry, req: &Req, name: &str, id: &str) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, req, name).await?;
    reg.pk()
        .upload_cancel(&cx, &owner, &p, id)
        .await
        .map_err(upload_unknown)?;
    Ok(empty(204))
}

async fn blob_get(
    reg: &PackageRegistry,
    req: &Req,
    name: &str,
    d: &str,
    head: bool,
) -> Result<Resp> {
    check_digest(d)?;
    let caller = reg.caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Container, pkg)
        .await?;
    let b = reg
        .pk()
        .blob(&cx, &p, d)
        .await?
        .ok_or_else(|| named("blob_unknown", "blob"))?;
    let len = b.size.to_string();
    let headers = [
        ("content-type", "application/octet-stream"),
        ("docker-content-digest", d),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &headers, Bytes::new()));
    }
    let data = reg
        .pk()
        .ports
        .blobs
        .get(&b.blob_key)
        .await?
        .ok_or_else(|| {
            ForgeError::new(
                bee_proto::v1::ErrorReason::Integrity,
                "blob content is missing",
            )
        })?;
    Ok(resp(200, &headers, data))
}

async fn blob_delete(reg: &PackageRegistry, req: &Req, name: &str, d: &str) -> Result<Resp> {
    check_digest(d)?;
    let caller = reg.require_caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Container, pkg, Need::Write)
        .await?;
    if reg.pk().delete_blob(&cx, &o, &p, d).await? {
        Ok(empty(202))
    } else {
        Err(named("blob_unknown", "blob"))
    }
}

struct Parsed {
    media_type: String,
    config: Option<(String, u64)>,
    layers: Vec<(String, u64, String)>,
    children: Vec<(String, PlatformRec)>,
    subject: String,
    artifact_type: String,
    is_index: bool,
}

fn manifest_invalid(msg: impl Into<String>) -> ForgeError {
    ForgeError::invalid("manifest_invalid", msg)
}

fn desc(v: &Value, what: &str) -> Result<(String, u64, String)> {
    let d = v
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| manifest_invalid(format!("{what} has no digest")))?;
    check_digest(d).map_err(|_| manifest_invalid(format!("{what} digest `{d}` is not sha256")))?;
    let size = v.get("size").and_then(Value::as_u64).unwrap_or(0);
    let mt = v.get("mediaType").and_then(Value::as_str).unwrap_or("");
    Ok((d.to_string(), size, mt.to_string()))
}

fn parse_manifest(body: &[u8], content_type: &str) -> Result<Parsed> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| manifest_invalid(format!("manifest is not JSON: {e}")))?;
    if v.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
        return Err(manifest_invalid("schemaVersion must be 2"));
    }
    let mt = v
        .get("mediaType")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(content_type.split(';').next().unwrap_or("").trim())
        .to_string();
    let subject = match v.get("subject") {
        Some(s) => desc(s, "subject")?.0,
        None => String::new(),
    };
    match mt.as_str() {
        OCI_INDEX | DOCKER_LIST => {
            let list = v
                .get("manifests")
                .and_then(Value::as_array)
                .ok_or_else(|| manifest_invalid("an index needs `manifests`"))?;
            let mut children = Vec::new();
            for m in list {
                let (d, _, _) = desc(m, "manifest entry")?;
                let pl = m.get("platform");
                let s = |k: &str| {
                    pl.and_then(|p| p.get(k))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                };
                children.push((
                    d.clone(),
                    PlatformRec {
                        os: s("os"),
                        arch: s("architecture"),
                        variant: s("variant"),
                        digest: d,
                    },
                ));
            }
            Ok(Parsed {
                artifact_type: v
                    .get("artifactType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                media_type: mt,
                config: None,
                layers: Vec::new(),
                children,
                subject,
                is_index: true,
            })
        }
        OCI_MANIFEST | DOCKER_MANIFEST => {
            let cfg = v
                .get("config")
                .ok_or_else(|| manifest_invalid("a manifest needs `config`"))?;
            let (cd, cs, cmt) = desc(cfg, "config")?;
            let mut layers = Vec::new();
            for l in v
                .get("layers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                layers.push(desc(l, "layer")?);
            }
            let at = v
                .get("artifactType")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(cmt);
            Ok(Parsed {
                media_type: mt,
                config: Some((cd, cs)),
                layers,
                children: Vec::new(),
                subject,
                artifact_type: at,
                is_index: false,
            })
        }
        other => Err(manifest_invalid(format!(
            "unsupported manifest media type `{other}`"
        ))),
    }
}

async fn manifest_put(
    reg: &PackageRegistry,
    req: Req,
    name: &str,
    reference: &str,
) -> Result<Resp> {
    let (owner, p, cx) = write_repo(reg, &req, name).await?;
    let pk = reg.pk();
    let body = req.body().clone();
    if body.len() > MAX_MANIFEST {
        return Err(ForgeError::too_large(
            "manifest size",
            MAX_MANIFEST as u64,
            body.len() as u64,
        ));
    }
    let digest = sha256_digest(&body);
    let tag = if is_digest(reference) {
        if reference != digest {
            return Err(ForgeError::invalid(
                "digest_invalid",
                format!("the manifest has digest {digest}, not {reference}"),
            ));
        }
        None
    } else {
        super::names::validate_version(Eco::Container, reference).map_err(|_| {
            ForgeError::invalid("tag_invalid", format!("`{reference}` is not a valid tag"))
        })?;
        Some(reference.to_string())
    };
    let m = parse_manifest(&body, header(&req, "content-type"))?;
    let mut files = Vec::new();
    let mut total = body.len() as u64;
    let mut config_key = String::new();
    if let Some((cd, cs)) = &m.config {
        let b = pk.blob(&cx, &p, cd).await?.ok_or_else(|| {
            ForgeError::invalid(
                "manifest_blob_unknown",
                format!("config blob {cd} is unknown"),
            )
        })?;
        if b.size != *cs && *cs != 0 {
            return Err(manifest_invalid(format!(
                "config blob {cd} is {} bytes, not {cs}",
                b.size
            )));
        }
        config_key = b.blob_key.clone();
        total += b.size;
        files.push(NewFile {
            name: "config".into(),
            size: b.size,
            digest: cd.clone(),
            media_type: "application/vnd.oci.image.config.v1+json".into(),
            blob_key: b.blob_key,
            ..Default::default()
        });
    }
    for (i, (ld, ls, lmt)) in m.layers.iter().enumerate() {
        let b = pk.blob(&cx, &p, ld).await?.ok_or_else(|| {
            ForgeError::invalid(
                "manifest_blob_unknown",
                format!("layer blob {ld} is unknown"),
            )
        })?;
        if b.size != *ls && *ls != 0 {
            return Err(manifest_invalid(format!(
                "layer {ld} is {} bytes, not {ls}",
                b.size
            )));
        }
        total += b.size;
        files.push(NewFile {
            name: format!("layer-{i:04}"),
            size: b.size,
            digest: ld.clone(),
            media_type: lmt.clone(),
            blob_key: b.blob_key,
            ..Default::default()
        });
    }
    let mut platforms = Vec::new();
    for (cd, pl) in &m.children {
        let mut tx_ok = false;
        if let Ok(v) = pk.version(&cx, &p, cd).await {
            tx_ok = v.digest == *cd;
        }
        if !tx_ok {
            return Err(ForgeError::invalid(
                "manifest_blob_unknown",
                format!("child manifest {cd} is unknown"),
            ));
        }
        platforms.push(pl.clone());
    }
    let mkey = super::Packages::content_key(&owner.id, &digest);
    pk.ports.blobs.put(&mkey, body.clone()).await?;
    files.insert(
        0,
        NewFile {
            name: "manifest".into(),
            size: body.len() as u64,
            digest: digest.clone(),
            media_type: m.media_type.clone(),
            blob_key: mkey,
            ..Default::default()
        },
    );
    let mut nv = NewVersion::new(&digest);
    nv.digest = digest.clone();
    nv.files = files;
    nv.tags = tag.into_iter().collect();
    nv.media_type = m.media_type.clone();
    nv.subject = m.subject.clone();
    nv.artifact_type = m.artifact_type.clone();
    nv.platforms = platforms;
    nv.on_conflict = OnConflict::Idempotent;
    nv.provenance = PackageRegistry::claim(&req);
    nv.metadata_json = json!({
        "mediaType": m.media_type, "isIndex": m.is_index,
        "layers": m.layers.len(), "totalBytes": total,
    })
    .to_string();
    let out = pk.publish(&cx, &owner, &p, nv).await?;
    if out.created && p.repo_id.is_empty() && !config_key.is_empty() {
        link_from_config(reg, &req, &owner, &p, &config_key).await;
    }
    let mut h = vec![
        (
            "location".to_string(),
            format!("/v2/{name}/manifests/{digest}"),
        ),
        ("docker-content-digest".to_string(), digest),
    ];
    if !m.subject.is_empty() {
        h.push(("oci-subject".into(), m.subject));
    }
    Ok(with_headers(201, &h))
}

/// Links a new image to the repo named by its `org.opencontainers.image.source` label, when
/// the pusher may write to that repo. Best effort: a failure leaves the package unlinked.
async fn link_from_config(
    reg: &PackageRegistry,
    req: &Req,
    owner: &crate::packages::Owner,
    p: &PackageRec,
    config_key: &str,
) {
    let Ok(Some(cfg)) = reg.pk().ports.blobs.get(config_key).await else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&cfg) else {
        return;
    };
    let src = v
        .pointer("/config/Labels/org.opencontainers.image.source")
        .or_else(|| v.pointer("/container_config/Labels/org.opencontainers.image.source"))
        .and_then(Value::as_str);
    let Some(full) = src.and_then(|u| reg.repo_from_url(u)) else {
        return;
    };
    let (Some(repo), Ok(caller)) = (reg.resolve_repo(&full).await, reg.require_caller(req).await)
    else {
        return;
    };
    if repo.org_id != owner.id {
        return;
    }
    if reg
        .pk()
        .authorize_create(&caller, owner, Some(&repo))
        .await
        .is_ok()
    {
        let _ = reg
            .pk()
            .link_repo(&reg.lookup_cx(), owner, p, &repo.id)
            .await;
    }
}

async fn manifest_get(
    reg: &PackageRegistry,
    req: &Req,
    name: &str,
    reference: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Container, pkg)
        .await?;
    let pk = reg.pk();
    let v = pk
        .version(&cx, &p, reference)
        .await
        .map_err(|_| named("manifest_unknown", "manifest"))?;
    let f = pk
        .file(&cx, &v, "manifest")
        .await?
        .ok_or_else(|| named("manifest_unknown", "manifest"))?;
    let len = f.size.to_string();
    let headers = [
        ("content-type", v.media_type.as_str()),
        ("docker-content-digest", v.digest.as_str()),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &headers, Bytes::new()));
    }
    let data = pk.read_file(&f).await?;
    reg.count_download(&p, &v.id).await;
    Ok(resp(200, &headers, data))
}

async fn manifest_delete(
    reg: &PackageRegistry,
    req: &Req,
    name: &str,
    reference: &str,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Container, pkg, Need::Write)
        .await?;
    if is_digest(reference) {
        reg.pk()
            .delete_version(&cx, &o, &p, reference)
            .await
            .map_err(|e| {
                if e.reason == bee_proto::v1::ErrorReason::NotFound {
                    named("manifest_unknown", "manifest")
                } else {
                    e
                }
            })?;
    } else {
        reg.pk()
            .delete_tag(&cx, &o, &p, reference)
            .await
            .map_err(|_| named("manifest_unknown", "tag"))?;
    }
    Ok(empty(202))
}

async fn tags_list(reg: &PackageRegistry, req: &Req, name: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Container, pkg)
        .await?;
    let mut tags: Vec<String> = reg
        .pk()
        .tags(&cx, &p)
        .await?
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    tags.sort();
    if let Some(last) = query_get(req, "last") {
        tags.retain(|t| t.as_str() > last.as_str());
    }
    let n = query_get(req, "n").and_then(|n| n.parse::<usize>().ok());
    let mut link = String::new();
    if let Some(n) = n
        && tags.len() > n
    {
        tags.truncate(n);
        if let Some(l) = tags.last() {
            link = format!("</v2/{name}/tags/list?last={l}&n={n}>; rel=\"next\"");
        }
    }
    let body = json!({"name": name, "tags": tags});
    let mut r = json(200, &body);
    if !link.is_empty()
        && let Ok(v) = http::HeaderValue::from_str(&link)
    {
        r.headers_mut().insert("link", v);
    }
    Ok(r)
}

async fn referrers(reg: &PackageRegistry, req: &Req, name: &str, d: &str) -> Result<Resp> {
    check_digest(d)?;
    let caller = reg.caller(req).await?;
    let (owner, pkg) = split_name(name)?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Container, pkg)
        .await?;
    let filter = query_get(req, "artifactType");
    let mut manifests = Vec::new();
    for v in reg.pk().referrers(&cx, &p, d).await? {
        if filter.as_ref().is_some_and(|f| *f != v.artifact_type) {
            continue;
        }
        let size = reg
            .pk()
            .file(&cx, &v, "manifest")
            .await?
            .map(|f| f.size)
            .unwrap_or(0);
        let mut m = json!({"mediaType": v.media_type, "digest": v.digest, "size": size});
        if !v.artifact_type.is_empty() {
            m["artifactType"] = json!(v.artifact_type);
        }
        manifests.push(m);
    }
    let body = json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests});
    let mut r = resp(
        200,
        &[("content-type", OCI_INDEX)],
        body.to_string().into_bytes(),
    );
    if filter.is_some() {
        r.headers_mut().insert(
            "oci-filters-applied",
            http::HeaderValue::from_static("artifactType"),
        );
    }
    Ok(r)
}
