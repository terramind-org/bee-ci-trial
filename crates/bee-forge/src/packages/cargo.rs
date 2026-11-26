//! The Cargo sparse registry (`/cargo/<owner>/...`), written from the public registry index
//! and web API documentation.
//!
//! - `GET index/config.json`: `dl`, `api` and `auth-required`;
//! - `GET index/<path>`: one JSON line per version in the standard index layout (`1/`, `2/`,
//!   `3/<c>/`, `<ab>/<cd>/`), with `cksum`, dependencies in index form, `features` (and
//!   `features2` with `v: 2` when a feature uses `dep:` or `?/`), `yanked`, `links` and
//!   `rust_version`; `ETag` with `304` on `If-None-Match`;
//! - `PUT api/v1/crates/new`: publish, a length-prefixed JSON metadata blob followed by a
//!   length-prefixed `.crate`; the same name and version can never be published twice;
//! - `DELETE api/v1/crates/<crate>/<version>/yank` and `PUT .../unyank`;
//! - `GET api/v1/crates/<crate>/<version>/download`: the `.crate` (counts one; yanked crates
//!   stay downloadable, as Cargo requires for existing lockfiles);
//! - `GET api/v1/crates?q=`: a minimal search.
//!
//! Crate names are compared case-insensitively with `-` and `_` equal, as Cargo does. The
//! `Authorization` header carries a Beehive token (what `cargo login` stores).

use bytes::Bytes;
use serde_json::{Value, json};

use super::access::Need;
use super::hash::{md5_hex, sha1_hex, sha256_digest, sha256_hex, sha512_b64};
use super::http::{Req, Resp, header, json as json_resp, pct_decode, query_get, resp, status_of};
use super::manage::{ListFilter, NewFile, NewVersion, OnConflict};
use super::model::{Eco, PackageRec, VersionRec};
use super::names::{SemVer, validate_name};
use super::registry::{CreateOpts, PackageRegistry, owner_and_rest};
use crate::error::{ForgeError, Result};

fn err(e: &ForgeError, base_url: &str) -> Resp {
    let mut h = vec![("content-type", "application/json")];
    let login_url;
    if status_of(e) == 401 {
        login_url = format!(
            "Cargo login_url=\"{}/settings/tokens\"",
            base_url.trim_end_matches('/')
        );
        h.push(("www-authenticate", &login_url));
    }
    resp(
        status_of(e),
        &h,
        json!({"errors": [{"detail": e.message}]})
            .to_string()
            .into_bytes(),
    )
}

/// Serves one Cargo registry request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    let base_url = reg.pk().config().base_url.clone();
    match route(reg, req).await {
        Ok(r) => r,
        Err(e) => err(&e, &base_url),
    }
}

/// The index path of crate `name` (lower-cased): `1/a`, `2/ab`, `3/a/abc`, `ab/cd/abcd...`.
pub fn index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        0 => String::new(),
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let path = req.uri().path().to_string();
    let Some((owner, segs)) = owner_and_rest(&path, "/cargo/") else {
        return Err(ForgeError::not_found("registry path"));
    };
    let method = req.method().as_str().to_string();
    match (segs.as_slice(), method.as_str()) {
        (["index", "config.json"], "GET" | "HEAD") => config(reg, &owner),
        (["index", rest @ ..], "GET" | "HEAD") if !rest.is_empty() => {
            index_file(reg, &req, &owner, rest).await
        }
        (["api", "v1", "crates", "new"], "PUT") => publish(reg, &req, &owner).await,
        (["api", "v1", "crates", name, version, "yank"], "DELETE") => {
            set_yank(reg, &req, &owner, &pct_decode(name), version, true).await
        }
        (["api", "v1", "crates", name, version, "unyank"], "PUT") => {
            set_yank(reg, &req, &owner, &pct_decode(name), version, false).await
        }
        (["api", "v1", "crates", name, version, "download"], "GET" | "HEAD") => {
            download(
                reg,
                &req,
                &owner,
                &pct_decode(name),
                version,
                method == "HEAD",
            )
            .await
        }
        (["api", "v1", "crates"], "GET") => search(reg, &req, &owner).await,
        _ => Err(ForgeError::not_found("registry resource")),
    }
}

fn config(reg: &PackageRegistry, owner: &str) -> Result<Resp> {
    let base = reg.pk().config().base_url.trim_end_matches('/').to_string();
    Ok(json_resp(
        200,
        &json!({
            "dl": format!("{base}/cargo/{owner}/api/v1/crates/{{crate}}/{{version}}/download"),
            "api": format!("{base}/cargo/{owner}"),
            "auth-required": reg.pk().config().cargo_auth_required,
        }),
    ))
}

fn dep_to_index(d: &Value) -> Value {
    let name = d.get("name").and_then(Value::as_str).unwrap_or("");
    let explicit = d
        .get("explicit_name_in_toml")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let mut o = json!({
        "name": explicit.unwrap_or(name),
        "req": d.get("version_req").or_else(|| d.get("req")).cloned().unwrap_or(json!("*")),
        "features": d.get("features").cloned().unwrap_or(json!([])),
        "optional": d.get("optional").cloned().unwrap_or(json!(false)),
        "default_features": d.get("default_features").cloned().unwrap_or(json!(true)),
        "target": d.get("target").cloned().unwrap_or(Value::Null),
        "kind": d.get("kind").cloned().unwrap_or(json!("normal")),
        "registry": d.get("registry").cloned().unwrap_or(Value::Null),
    });
    if explicit.is_some() {
        o["package"] = json!(name);
    }
    o
}

fn index_entry(p: &PackageRec, v: &VersionRec) -> Value {
    let m: Value = serde_json::from_str(&v.metadata_json).unwrap_or(Value::Null);
    let features = m
        .get("features")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let uses_v2 = features
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .any(|f| {
            f.as_str()
                .is_some_and(|s| s.contains("dep:") || s.contains("?/"))
        });
    let mut e = json!({
        "name": p.name, "vers": v.version,
        "deps": m.get("deps").and_then(Value::as_array).map(|a| a.iter().map(dep_to_index).collect::<Vec<_>>()).unwrap_or_default(),
        "cksum": v.digest.trim_start_matches("sha256:"),
        "features": if uses_v2 { json!({}) } else { Value::Object(features.clone()) },
        "yanked": v.yanked,
        "links": m.get("links").cloned().unwrap_or(Value::Null),
    });
    if uses_v2 {
        e["features2"] = Value::Object(features);
        e["v"] = json!(2);
    } else {
        e["v"] = json!(1);
    }
    if let Some(r) = m.get("rust_version").filter(|r| !r.is_null()) {
        e["rust_version"] = r.clone();
    }
    e
}

async fn index_file(reg: &PackageRegistry, req: &Req, owner: &str, rest: &[&str]) -> Result<Resp> {
    let name = *rest.last().unwrap_or(&"");
    if index_path(name) != rest.join("/") {
        return Err(ForgeError::not_found("index file"));
    }
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Cargo, name)
        .await?;
    let mut vs = reg.pk().all_versions(&cx, &p).await?;
    vs.reverse();
    if vs.is_empty() {
        return Err(ForgeError::not_found("index file"));
    }
    let mut body = String::new();
    for v in &vs {
        body.push_str(&index_entry(&p, v).to_string());
        body.push('\n');
    }
    let etag = format!("\"{}\"", &sha256_hex(body.as_bytes())[..32]);
    if header(req, "if-none-match") == etag {
        return Ok(resp(304, &[("etag", etag.as_str())], Bytes::new()));
    }
    Ok(resp(
        200,
        &[
            ("content-type", "text/plain"),
            ("etag", etag.as_str()),
            ("cache-control", "no-cache"),
        ],
        body.into_bytes(),
    ))
}

fn le32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

async fn publish(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let body = req.body().clone();
    let bad = |m: &str| ForgeError::invalid("bad_publish", m.to_string());
    let jlen = le32(&body, 0).ok_or_else(|| bad("the publish body is truncated"))?;
    let meta_bytes = body
        .get(4..4 + jlen)
        .ok_or_else(|| bad("the metadata length runs past the body"))?;
    let clen = le32(&body, 4 + jlen).ok_or_else(|| bad("the crate length is missing"))?;
    let krate = body
        .slice(8 + jlen..)
        .slice(..clen.min(body.len().saturating_sub(8 + jlen)));
    if krate.len() != clen || krate.is_empty() {
        return Err(bad("the crate length does not match the body"));
    }
    let meta: Value = serde_json::from_slice(meta_bytes)
        .map_err(|e| bad(&format!("metadata is not JSON: {e}")))?;
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("metadata has no name"))?;
    let vers = meta
        .get("vers")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("metadata has no vers"))?;
    validate_name(Eco::Cargo, name)?;
    if SemVer::parse(vers).is_none() {
        return Err(ForgeError::invalid(
            "bad_version",
            format!("`{vers}` is not a valid semver version"),
        ));
    }
    let opts = CreateOpts {
        visibility: None,
        link: PackageRegistry::repo_hint(req).or_else(|| {
            meta.get("repository")
                .and_then(Value::as_str)
                .and_then(|u| reg.repo_from_url(u))
        }),
        description: meta
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };
    let (o, p, cx, _) = reg
        .publish_target(&caller, owner, Eco::Cargo, name, &opts)
        .await?;
    let pk = reg.pk();
    let digest = sha256_digest(&krate);
    let key = super::Packages::content_key(&o.id, &digest);
    pk.ports.blobs.put(&key, krate.clone()).await?;
    let mut nv = NewVersion::new(vers);
    nv.digest = digest.clone();
    nv.metadata_json = meta.to_string();
    nv.files = vec![NewFile {
        name: format!("{}-{vers}.crate", p.name),
        size: krate.len() as u64,
        digest,
        media_type: "application/gzip".into(),
        blob_key: key,
        sha1: sha1_hex(&krate),
        md5: md5_hex(&krate),
        sha512_b64: sha512_b64(&krate),
    }];
    nv.on_conflict = OnConflict::Reject;
    nv.provenance = PackageRegistry::claim(req);
    pk.publish(&cx, &o, &p, nv).await?;
    Ok(json_resp(
        200,
        &json!({"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}}),
    ))
}

async fn set_yank(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    version: &str,
    yank: bool,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Cargo, name, Need::Write)
        .await?;
    reg.pk()
        .update_version_flags(&cx, &o, &p, version, None, Some(yank))
        .await?;
    Ok(json_resp(200, &json!({"ok": true})))
}

async fn download(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    name: &str,
    version: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Cargo, name)
        .await?;
    let v = reg.pk().version(&cx, &p, version).await?;
    let f = reg
        .pk()
        .file(&cx, &v, &format!("{}-{version}.crate", p.name))
        .await?
        .ok_or_else(|| ForgeError::not_found("crate file"))?;
    let len = f.size.to_string();
    let h = [
        ("content-type", "application/gzip"),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &h, Bytes::new()));
    }
    let data = reg.pk().read_file(&f).await?;
    reg.count_download(&p, &v.id).await;
    Ok(resp(200, &h, data))
}

async fn search(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let lookup = reg.lookup_cx();
    let o = reg.pk().owner(&lookup, owner).await.map_err(|e| {
        if caller.is_none() {
            ForgeError::new(
                bee_proto::v1::ErrorReason::Unauthenticated,
                "needs a credential",
            )
        } else {
            e
        }
    })?;
    let q = query_get(req, "q").unwrap_or_default().to_lowercase();
    let per = query_get(req, "per_page")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
        .min(100);
    let (pkgs, _) = reg
        .pk()
        .list(
            &lookup,
            &ListFilter {
                owner_id: o.id,
                eco: Some(Eco::Cargo),
                ..Default::default()
            },
            "",
            1000,
        )
        .await?;
    let mut out = Vec::new();
    for p in pkgs {
        if !q.is_empty() && !p.name.to_lowercase().contains(&q) {
            continue;
        }
        let Ok(cx) = reg.pk().authorize(caller.as_ref(), &p, Need::Read).await else {
            continue;
        };
        let vs = reg.pk().all_versions(&cx, &p).await?;
        let Some(latest) = vs.first() else { continue };
        out.push(
            json!({"name": p.name, "max_version": latest.version, "description": p.description}),
        );
    }
    let total = out.len();
    out.truncate(per);
    Ok(json_resp(
        200,
        &json!({"crates": out, "meta": {"total": total}}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_layout() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("Serde"), "se/rd/serde");
    }
}
