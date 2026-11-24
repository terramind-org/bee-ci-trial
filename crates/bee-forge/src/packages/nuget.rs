//! NuGet v3 (`/nuget/<owner>/v3/index.json`), written from the public NuGet protocol
//! documentation.
//!
//! - service index with `PackagePublish/2.0.0`, `SearchQueryService`, `RegistrationsBaseUrl`
//!   and `PackageBaseAddress/3.0.0`;
//! - push: `PUT api/v2/package` with a multipart body (as `nuget push` and `dotnet nuget push`
//!   send it) or a raw `.nupkg`; the `.nuspec` is read from the package, the id and version
//!   validated, the version normalized (`1.0` is `1.0.0`), and the same normalized version can
//!   never be pushed twice (`409`);
//! - delete: `DELETE api/v2/package/<id>/<version>` removes the version (restorable);
//! - flat container: `<id>/index.json`, `<id>/<version>/<id>.<version>.nupkg` and `.nuspec`;
//! - registration: `<id>/index.json` with one inlined page of catalog entries (dependency
//!   groups included) and a leaf per version;
//! - search: `query?q=&skip=&take=&prerelease=` over the owner's readable packages.
//!
//! The API key (`X-NuGet-ApiKey`) or HTTP Basic password is a Beehive token.

use std::cmp::Ordering;

use bytes::Bytes;
use roxmltree::Document;
use serde_json::{Value, json};

use super::access::Need;
use super::archive::zip_entries;
use super::hash::{md5_hex, sha1_hex, sha256_digest, sha512_b64};
use super::http::{
    Req, Resp, header, iso8601, json as json_resp, pct_decode, query_get, resp, status_of,
};
use super::manage::{ListFilter, NewFile, NewVersion, OnConflict};
use super::model::{Eco, PackageRec, VersionRec};
use super::names::{SemVer, nuget_normalize, validate_name};
use super::registry::{CreateOpts, PackageRegistry, owner_and_rest};
use crate::error::{ForgeError, Result};

fn err(e: &ForgeError) -> Resp {
    let mut h = vec![("content-type", "application/json")];
    if status_of(e) == 401 {
        h.push(("www-authenticate", "Basic realm=\"Beehive NuGet\""));
    }
    resp(
        status_of(e),
        &h,
        json!({"message": e.message}).to_string().into_bytes(),
    )
}

/// Serves one NuGet request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    match route(reg, req).await {
        Ok(r) => r,
        Err(e) => err(&e),
    }
}

fn base(reg: &PackageRegistry, owner: &str) -> String {
    format!(
        "{}/nuget/{owner}",
        reg.pk().config().base_url.trim_end_matches('/')
    )
}

fn cmp_versions(a: &str, b: &str) -> Ordering {
    let (ma, pa) = a
        .split_once('-')
        .map(|(m, p)| (m, Some(p)))
        .unwrap_or((a, None));
    let (mb, pb) = b
        .split_once('-')
        .map(|(m, p)| (m, Some(p)))
        .unwrap_or((b, None));
    let nums = |s: &str| {
        s.split('.')
            .map(|x| x.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (mut x, mut y) = (nums(ma), nums(mb));
    while x.len() < 4 {
        x.push(0);
    }
    while y.len() < 4 {
        y.push(0);
    }
    x.cmp(&y).then_with(|| match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(p), Some(q)) => {
            let sa = SemVer::parse(&format!("0.0.0-{p}"));
            let sb = SemVer::parse(&format!("0.0.0-{q}"));
            match (sa, sb) {
                (Some(sa), Some(sb)) => sa.cmp(&sb),
                _ => p.cmp(q),
            }
        }
    })
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let path = req.uri().path().to_string();
    let Some((owner, segs)) = owner_and_rest(&path, "/nuget/") else {
        return Err(ForgeError::not_found("feed"));
    };
    let method = req.method().as_str().to_string();
    let head = method == "HEAD";
    match (segs.as_slice(), method.as_str()) {
        (["v3", "index.json"], "GET" | "HEAD") => Ok(index(reg, &owner)),
        (["api", "v2", "package"], "PUT" | "POST") => push(reg, &req, &owner).await,
        (["api", "v2", "package", id, version], "DELETE") => {
            delete(reg, &req, &owner, &pct_decode(id), &pct_decode(version)).await
        }
        (["v3", "flatcontainer", id, "index.json"], "GET" | "HEAD") => {
            flat_index(reg, &req, &owner, &pct_decode(id)).await
        }
        (["v3", "flatcontainer", id, version, file], "GET" | "HEAD") => {
            flat_file(
                reg,
                &req,
                &owner,
                &pct_decode(id),
                &pct_decode(version),
                &pct_decode(file),
                head,
            )
            .await
        }
        (["v3", "registration", id, "index.json"], "GET" | "HEAD") => {
            registration(reg, &req, &owner, &pct_decode(id)).await
        }
        (["v3", "registration", id, leaf], "GET" | "HEAD") if leaf.ends_with(".json") => {
            let v = leaf.trim_end_matches(".json");
            registration_leaf(reg, &req, &owner, &pct_decode(id), &pct_decode(v)).await
        }
        (["v3", "query"], "GET" | "HEAD") => search(reg, &req, &owner).await,
        _ => Err(ForgeError::not_found("feed resource")),
    }
}

fn index(reg: &PackageRegistry, owner: &str) -> Resp {
    let b = base(reg, owner);
    json_resp(
        200,
        &json!({"version": "3.0.0", "resources": [
            {"@id": format!("{b}/api/v2/package"), "@type": "PackagePublish/2.0.0"},
            {"@id": format!("{b}/v3/query"), "@type": "SearchQueryService"},
            {"@id": format!("{b}/v3/query"), "@type": "SearchQueryService/3.0.0-beta"},
            {"@id": format!("{b}/v3/query"), "@type": "SearchQueryService/3.5.0"},
            {"@id": format!("{b}/v3/registration/"), "@type": "RegistrationsBaseUrl"},
            {"@id": format!("{b}/v3/registration/"), "@type": "RegistrationsBaseUrl/3.6.0"},
            {"@id": format!("{b}/v3/flatcontainer/"), "@type": "PackageBaseAddress/3.0.0"},
        ]}),
    )
}

/// The first part of a `multipart/form-data` body, or the whole body when it is not multipart.
fn upload_body(req: &Req) -> Result<Bytes> {
    let ct = header(req, "content-type");
    let Some(b) = ct
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
    else {
        return Ok(req.body().clone());
    };
    let boundary = format!("--{}", b.trim_matches('"'));
    let body = req.body();
    let find = |hay: &[u8], needle: &[u8], from: usize| -> Option<usize> {
        hay.get(from..)?
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|i| i + from)
    };
    let start = find(body, boundary.as_bytes(), 0)
        .ok_or_else(|| ForgeError::invalid("bad_multipart", "no multipart boundary in the body"))?;
    let headers_end = find(body, b"\r\n\r\n", start)
        .ok_or_else(|| ForgeError::invalid("bad_multipart", "multipart part has no headers"))?
        + 4;
    let end = find(body, format!("\r\n{boundary}").as_bytes(), headers_end)
        .ok_or_else(|| ForgeError::invalid("bad_multipart", "multipart part is not closed"))?;
    Ok(body.slice(headers_end..end))
}

struct Nuspec {
    id: String,
    version: String,
    meta: Value,
    repository: Option<String>,
    description: String,
}

fn parse_nuspec(xml: &str) -> Result<Nuspec> {
    let doc = Document::parse(xml).map_err(|e| {
        ForgeError::invalid("bad_nuspec", format!("the .nuspec is not valid XML: {e}"))
    })?;
    let meta = doc
        .descendants()
        .find(|n| n.has_tag_name("metadata"))
        .ok_or_else(|| ForgeError::invalid("bad_nuspec", "the .nuspec has no <metadata>"))?;
    let text = |name: &str| {
        meta.children()
            .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name))
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let id = text("id");
    let version = text("version");
    if id.is_empty() || version.is_empty() {
        return Err(ForgeError::invalid(
            "bad_nuspec",
            "the .nuspec needs <id> and <version>",
        ));
    }
    let mut groups = Vec::new();
    if let Some(deps) = meta.children().find(|n| n.has_tag_name("dependencies")) {
        let dep_of = |g: roxmltree::Node| {
            g.children()
                .filter(|d| d.has_tag_name("dependency"))
                .map(|d| json!({"id": d.attribute("id").unwrap_or(""), "range": d.attribute("version").unwrap_or("")}))
                .collect::<Vec<_>>()
        };
        let grouped: Vec<_> = deps
            .children()
            .filter(|g| g.has_tag_name("group"))
            .collect();
        if grouped.is_empty() {
            groups.push(json!({"targetFramework": "", "dependencies": dep_of(deps)}));
        }
        for g in grouped {
            groups.push(json!({"targetFramework": g.attribute("targetFramework").unwrap_or(""), "dependencies": dep_of(g)}));
        }
    }
    let repository = meta
        .children()
        .find(|n| n.has_tag_name("repository"))
        .and_then(|n| n.attribute("url").map(str::to_string))
        .or_else(|| Some(text("projectUrl")).filter(|s| !s.is_empty()));
    let description = text("description");
    Ok(Nuspec {
        meta: json!({
            "id": id, "version": version, "authors": text("authors"), "description": description,
            "summary": text("summary"), "title": text("title"), "tags": text("tags"),
            "projectUrl": text("projectUrl"), "licenseUrl": text("licenseUrl"),
            "iconUrl": text("iconUrl"), "dependencyGroups": groups,
            "requireLicenseAcceptance": text("requireLicenseAcceptance") == "true",
        }),
        id,
        version,
        repository,
        description,
    })
}

async fn push(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let nupkg = upload_body(req)?;
    if nupkg.is_empty() {
        return Err(ForgeError::invalid("bad_package", "the package is empty"));
    }
    let entries = zip_entries(&nupkg, 512 << 20)?;
    let spec_entry = entries
        .iter()
        .find(|e| !e.path.contains('/') && e.path.ends_with(".nuspec"))
        .ok_or_else(|| {
            ForgeError::invalid("bad_package", "the package has no .nuspec at its root")
        })?;
    let spec = parse_nuspec(&String::from_utf8_lossy(&spec_entry.data))?;
    validate_name(Eco::Nuget, &spec.id)?;
    let norm = nuget_normalize(&spec.version).ok_or_else(|| {
        ForgeError::invalid(
            "bad_version",
            format!("`{}` is not a valid NuGet version", spec.version),
        )
    })?;
    let lower = spec.id.to_lowercase();
    let opts = CreateOpts {
        visibility: None,
        link: PackageRegistry::repo_hint(req).or_else(|| {
            spec.repository
                .as_deref()
                .and_then(|u| reg.repo_from_url(u))
        }),
        description: spec.description.clone(),
    };
    let (o, p, cx, _) = reg
        .publish_target(&caller, owner, Eco::Nuget, &spec.id, &opts)
        .await?;
    let pk = reg.pk();
    let digest = sha256_digest(&nupkg);
    let key = super::Packages::content_key(&o.id, &digest);
    pk.ports.blobs.put(&key, nupkg.clone()).await?;
    let nuspec_bytes = Bytes::from(spec_entry.data.clone());
    let nd = sha256_digest(&nuspec_bytes);
    let nkey = super::Packages::content_key(&o.id, &nd);
    pk.ports.blobs.put(&nkey, nuspec_bytes.clone()).await?;
    let mut meta = spec.meta.clone();
    meta["version"] = json!(norm);
    let mut nv = NewVersion::new(&norm);
    nv.digest = digest.clone();
    nv.metadata_json = meta.to_string();
    nv.files = vec![
        NewFile {
            name: format!("{lower}.{norm}.nupkg"),
            size: nupkg.len() as u64,
            digest,
            media_type: "application/octet-stream".into(),
            blob_key: key,
            sha1: sha1_hex(&nupkg),
            md5: md5_hex(&nupkg),
            sha512_b64: sha512_b64(&nupkg),
        },
        NewFile {
            name: format!("{lower}.nuspec"),
            size: nuspec_bytes.len() as u64,
            digest: nd,
            media_type: "application/xml".into(),
            blob_key: nkey,
            ..Default::default()
        },
    ];
    nv.on_conflict = OnConflict::Reject;
    nv.provenance = PackageRegistry::claim(req);
    pk.publish(&cx, &o, &p, nv).await?;
    Ok(resp(201, &[], Bytes::new()))
}

async fn delete(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    id: &str,
    version: &str,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Nuget, id, Need::Write)
        .await?;
    let v = nuget_normalize(version).unwrap_or_else(|| version.to_string());
    reg.pk().delete_version(&cx, &o, &p, &v).await?;
    Ok(resp(204, &[], Bytes::new()))
}

async fn sorted_versions(
    reg: &PackageRegistry,
    cx: &crate::cx::Cx,
    p: &PackageRec,
) -> Result<Vec<VersionRec>> {
    let mut vs = reg.pk().all_versions(cx, p).await?;
    vs.sort_by(|a, b| cmp_versions(&a.version, &b.version));
    Ok(vs)
}

async fn flat_index(reg: &PackageRegistry, req: &Req, owner: &str, id: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Nuget, id).await?;
    let versions: Vec<String> = sorted_versions(reg, &cx, &p)
        .await?
        .into_iter()
        .map(|v| v.version.to_lowercase())
        .collect();
    Ok(json_resp(200, &json!({"versions": versions})))
}

async fn flat_file(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    id: &str,
    version: &str,
    file: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Nuget, id).await?;
    let norm = nuget_normalize(version).unwrap_or_else(|| version.to_lowercase());
    let v = reg.pk().version(&cx, &p, &norm).await?;
    let lower = id.to_lowercase();
    let want = file.to_lowercase();
    if want != format!("{lower}.{norm}.nupkg") && want != format!("{lower}.nuspec") {
        return Err(ForgeError::not_found(format!("file {file}")));
    }
    let f = reg
        .pk()
        .file(&cx, &v, &want)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("file {file}")))?;
    let len = f.size.to_string();
    let h = [
        ("content-type", f.media_type.as_str()),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &h, Bytes::new()));
    }
    let data = reg.pk().read_file(&f).await?;
    if want.ends_with(".nupkg") {
        reg.count_download(&p, &v.id).await;
    }
    Ok(resp(200, &h, data))
}

fn catalog_entry(reg: &PackageRegistry, owner: &str, p: &PackageRec, v: &VersionRec) -> Value {
    let b = base(reg, owner);
    let lower = p.name.to_lowercase();
    let meta: Value = serde_json::from_str(&v.metadata_json).unwrap_or(Value::Null);
    let content = format!(
        "{b}/v3/flatcontainer/{lower}/{0}/{lower}.{0}.nupkg",
        v.version
    );
    let leaf = format!("{b}/v3/registration/{lower}/{}.json", v.version);
    json!({
        "@id": leaf, "@type": "Package", "commitTimeStamp": iso8601(v.updated_ms),
        "packageContent": content, "registration": format!("{b}/v3/registration/{lower}/index.json"),
        "catalogEntry": {
            "@id": leaf, "@type": "PackageDetails",
            "authors": meta.get("authors").cloned().unwrap_or(json!("")),
            "dependencyGroups": meta.get("dependencyGroups").cloned().unwrap_or(json!([])),
            "description": meta.get("description").cloned().unwrap_or(json!("")),
            "id": p.name, "version": v.version, "listed": true, "packageContent": content,
            "published": iso8601(v.created_ms),
            "requireLicenseAcceptance": meta.get("requireLicenseAcceptance").cloned().unwrap_or(json!(false)),
            "summary": meta.get("summary").cloned().unwrap_or(json!("")),
            "tags": meta.get("tags").cloned().unwrap_or(json!("")),
        },
    })
}

async fn registration(reg: &PackageRegistry, req: &Req, owner: &str, id: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Nuget, id).await?;
    let versions = sorted_versions(reg, &cx, &p).await?;
    let b = base(reg, owner);
    let lower = p.name.to_lowercase();
    let items: Vec<Value> = versions
        .iter()
        .map(|v| catalog_entry(reg, owner, &p, v))
        .collect();
    let (lo, hi) = (
        versions
            .first()
            .map(|v| v.version.clone())
            .unwrap_or_default(),
        versions
            .last()
            .map(|v| v.version.clone())
            .unwrap_or_default(),
    );
    let page = format!("{b}/v3/registration/{lower}/index.json#page/{lo}/{hi}");
    Ok(json_resp(
        200,
        &json!({
            "@id": format!("{b}/v3/registration/{lower}/index.json"),
            "count": 1,
            "items": [{"@id": page, "@type": "catalog:CatalogPage", "count": items.len(),
                       "lower": lo, "upper": hi, "items": items}],
        }),
    ))
}

async fn registration_leaf(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    id: &str,
    version: &str,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg.read_pkg(caller.as_ref(), owner, Eco::Nuget, id).await?;
    let norm = nuget_normalize(version).unwrap_or_else(|| version.to_lowercase());
    let v = reg.pk().version(&cx, &p, &norm).await?;
    let e = catalog_entry(reg, owner, &p, &v);
    Ok(json_resp(
        200,
        &json!({
            "@id": e["@id"], "@type": "Package", "listed": true, "published": iso8601(v.created_ms),
            "packageContent": e["packageContent"], "registration": e["registration"],
            "catalogEntry": e["@id"],
        }),
    ))
}

async fn search(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let lookup = reg.lookup_cx();
    let o = match reg.pk().owner(&lookup, owner).await {
        Ok(o) => o,
        Err(e) => {
            return if caller.is_none() {
                Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Unauthenticated,
                    "this feed needs a credential",
                ))
            } else {
                Err(e)
            };
        }
    };
    let q = query_get(req, "q").unwrap_or_default().to_lowercase();
    let skip = query_get(req, "skip")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let take = query_get(req, "take")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20)
        .min(100);
    let pre = query_get(req, "prerelease").is_some_and(|v| v == "true");
    let (pkgs, _) = reg
        .pk()
        .list(
            &lookup,
            &ListFilter {
                owner_id: o.id.clone(),
                eco: Some(Eco::Nuget),
                ..Default::default()
            },
            "",
            1000,
        )
        .await?;
    let b = base(reg, owner);
    let mut data = Vec::new();
    for p in pkgs {
        if !q.is_empty()
            && !p.name.to_lowercase().contains(&q)
            && !p.description.to_lowercase().contains(&q)
        {
            continue;
        }
        let Ok(cx) = reg.pk().authorize(caller.as_ref(), &p, Need::Read).await else {
            continue;
        };
        let versions: Vec<VersionRec> = sorted_versions(reg, &cx, &p)
            .await?
            .into_iter()
            .filter(|v| pre || !v.version.contains('-'))
            .collect();
        let Some(last) = versions.last() else {
            continue;
        };
        let usage = reg.pk().usage(&cx, &p, None).await?;
        let lower = p.name.to_lowercase();
        data.push(json!({
            "@id": format!("{b}/v3/registration/{lower}/index.json"), "@type": "Package",
            "registration": format!("{b}/v3/registration/{lower}/index.json"),
            "id": p.name, "version": last.version, "description": p.description,
            "totalDownloads": usage.download_count,
            "versions": versions.iter().map(|v| json!({
                "version": v.version, "downloads": usage.versions.iter().find(|u| u.version_id == v.id).map(|u| u.download_count).unwrap_or(0),
                "@id": format!("{b}/v3/registration/{lower}/{}.json", v.version),
            })).collect::<Vec<_>>(),
        }));
    }
    let total = data.len();
    let page: Vec<Value> = data.into_iter().skip(skip).take(take).collect();
    Ok(json_resp(200, &json!({"totalHits": total, "data": page})))
}
