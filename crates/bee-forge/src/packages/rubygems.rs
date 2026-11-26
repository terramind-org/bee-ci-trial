//! RubyGems (`/rubygems/<owner>/...`), written from the public RubyGems API and compact index
//! documentation.
//!
//! - `POST api/v1/gems`: push a `.gem` (a tar holding `metadata.gz` and `data.tar.gz`); the
//!   gemspec YAML is read for name, version, platform, summary, authors, runtime dependencies
//!   and the Ruby and RubyGems requirements. Repushing a version is `409`.
//! - `GET gems/<name>-<version>[-<platform>].gem`: download (counts one).
//! - `DELETE api/v1/gems/yank` and `PUT api/v1/gems/unyank` (`gem_name`, `version`, optional
//!   `platform`, as a form body or query): yanked versions stay listed as `-version` in the
//!   compact index, vanish from `info` and cannot be downloaded.
//! - compact index: `GET versions` and `GET info/<gem>`, with the `checksum:` (SHA-256 of the
//!   gem), `ruby:` and `rubygems:` requirement fields, `ETag` and plain-text bodies.
//! - JSON: `GET api/v1/gems/<name>.json`, `GET api/v1/versions/<name>.json`.
//! - legacy indexes: `specs.4.8.gz`, `latest_specs.4.8.gz`, `prerelease_specs.4.8.gz` as Ruby
//!   Marshal 4.8 arrays of `[name, Gem::Version, platform]`.
//!
//! The `Authorization` header carries a Beehive token (the gem API key). The gemspec reader is
//! a purpose-built line scanner for the YAML `gem build` writes, not a general YAML parser.

use bytes::Bytes;
use serde_json::{Value, json};

use super::access::Need;
use super::archive::{gunzip, gzip, tar_entries};
use super::hash::{md5_hex, sha1_hex, sha256_digest, sha256_hex, sha512_b64};
use super::http::{
    Req, Resp, header, iso8601, json as json_resp, pct_decode, query, resp, status_of,
};
use super::manage::{ListFilter, NewFile, NewVersion, OnConflict};
use super::model::{Eco, PackageRec, VersionRec};
use super::names::validate_name;
use super::registry::{CreateOpts, PackageRegistry, owner_and_rest};
use crate::error::{ForgeError, Result};

fn err(e: &ForgeError) -> Resp {
    let mut h = vec![("content-type", "text/plain")];
    if status_of(e) == 401 {
        h.push(("www-authenticate", "Basic realm=\"Beehive RubyGems\""));
    }
    resp(status_of(e), &h, e.message.clone().into_bytes())
}

/// Serves one RubyGems request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    match route(reg, req).await {
        Ok(r) => r,
        Err(e) => err(&e),
    }
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let path = req.uri().path().to_string();
    let Some((owner, segs)) = owner_and_rest(&path, "/rubygems/") else {
        return Err(ForgeError::not_found("gem source"));
    };
    let method = req.method().as_str().to_string();
    match (segs.as_slice(), method.as_str()) {
        (["api", "v1", "gems"], "POST") => push(reg, &req, &owner).await,
        (["api", "v1", "gems", "yank"], "DELETE") => yank(reg, &req, &owner, true).await,
        (["api", "v1", "gems", "unyank"], "PUT") => yank(reg, &req, &owner, false).await,
        (["api", "v1", "gems", f], "GET") if f.ends_with(".json") => {
            gem_json(reg, &req, &owner, &pct_decode(f.trim_end_matches(".json"))).await
        }
        (["api", "v1", "versions", f], "GET") if f.ends_with(".json") => {
            versions_json(reg, &req, &owner, &pct_decode(f.trim_end_matches(".json"))).await
        }
        (["gems", f], "GET" | "HEAD") => {
            download(reg, &req, &owner, &pct_decode(f), method == "HEAD").await
        }
        (["versions"], "GET" | "HEAD") => compact_versions(reg, &req, &owner).await,
        (["info", name], "GET" | "HEAD") => {
            compact_info(reg, &req, &owner, &pct_decode(name)).await
        }
        ([f @ ("specs.4.8.gz" | "latest_specs.4.8.gz" | "prerelease_specs.4.8.gz")], "GET") => {
            specs(reg, &req, &owner, f).await
        }
        _ => Err(ForgeError::not_found("gem source resource")),
    }
}

/// What the gemspec says.
#[derive(Debug, Default)]
struct Spec {
    name: String,
    version: String,
    platform: String,
    summary: String,
    description: String,
    authors: Vec<String>,
    licenses: Vec<String>,
    homepage: String,
    source_code_uri: String,
    ruby: String,
    rubygems: String,
    /// `(name, requirement, runtime)`
    deps: Vec<(String, String, bool)>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
        .unwrap_or(s);
    s.to_string()
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// The requirement in the block that starts after line `i` (`requirements:` pairs).
fn requirement(lines: &[&str], i: usize) -> String {
    let base = indent(lines[i]);
    let mut parts = Vec::new();
    let mut op = String::new();
    for l in &lines[i + 1..] {
        if l.trim().is_empty() {
            continue;
        }
        if indent(l) <= base {
            break;
        }
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("- - ") {
            op = unquote(rest);
        } else if let Some(v) = t.strip_prefix("version:") {
            parts.push(format!("{op} {}", unquote(v)).trim().to_string());
        }
    }
    if parts.is_empty() {
        ">= 0".to_string()
    } else {
        parts.join(", ")
    }
}

fn parse_spec(yaml: &str) -> Result<Spec> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut s = Spec {
        platform: "ruby".into(),
        ..Spec::default()
    };
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if indent(l) == 0 {
            let t = l.trim();
            let val = |k: &str| t.strip_prefix(k).map(unquote);
            if let Some(v) = val("name:") {
                s.name = v;
            } else if let Some(v) = val("platform:") {
                s.platform = v;
            } else if let Some(v) = val("summary:") {
                s.summary = v;
            } else if let Some(v) = val("description:") {
                s.description = v;
            } else if let Some(v) = val("homepage:") {
                s.homepage = v;
            } else if t.starts_with("version:") && t.contains("Gem::Version") {
                if let Some(n) = lines
                    .get(i + 1)
                    .and_then(|n| n.trim().strip_prefix("version:"))
                {
                    s.version = unquote(n);
                }
            } else if t == "authors:" || t == "licenses:" {
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim_start().starts_with("- ") {
                    let v = unquote(lines[j].trim_start().trim_start_matches("- "));
                    if t == "authors:" {
                        s.authors.push(v)
                    } else {
                        s.licenses.push(v)
                    }
                    j += 1;
                }
            } else if t.starts_with("required_ruby_version:") {
                s.ruby = requirement(&lines, i);
            } else if t.starts_with("required_rubygems_version:") {
                s.rubygems = requirement(&lines, i);
            } else if t == "dependencies:" {
                let mut j = i + 1;
                let mut cur: Option<(String, String, bool)> = None;
                while j < lines.len() && (indent(lines[j]) > 0 || lines[j].starts_with("- ")) {
                    let lt = lines[j].trim();
                    if lt.starts_with("- !ruby/object:Gem::Dependency") {
                        if let Some(d) = cur.take() {
                            s.deps.push(d);
                        }
                        cur = Some((String::new(), ">= 0".into(), true));
                    } else if let Some(c) = cur.as_mut()
                        && indent(lines[j]) <= 2
                    {
                        if let Some(v) = lt.strip_prefix("name:") {
                            c.0 = unquote(v);
                        } else if lt.starts_with("requirement:") {
                            c.1 = requirement(&lines, j);
                        } else if let Some(v) = lt.strip_prefix("type:") {
                            c.2 = unquote(v).trim_start_matches(':') == "runtime";
                        }
                    }
                    j += 1;
                }
                if let Some(d) = cur.take() {
                    s.deps.push(d);
                }
                i = j;
                continue;
            } else if t == "metadata:" {
                let mut j = i + 1;
                while j < lines.len() && indent(lines[j]) > 0 {
                    if let Some(v) = lines[j].trim().strip_prefix("source_code_uri:") {
                        s.source_code_uri = unquote(v);
                    }
                    j += 1;
                }
            }
        }
        i += 1;
    }
    if s.name.is_empty() || s.version.is_empty() {
        return Err(ForgeError::invalid(
            "bad_gem",
            "the gemspec has no name or version",
        ));
    }
    Ok(s)
}

fn gem_version_string(s: &Spec) -> String {
    if s.platform == "ruby" || s.platform.is_empty() {
        s.version.clone()
    } else {
        format!("{}-{}", s.version, s.platform)
    }
}

fn read_gem(data: &[u8]) -> Result<Spec> {
    let entries = tar_entries(data, 1 << 30)
        .map_err(|_| ForgeError::invalid("bad_gem", "not a .gem file (a gem is a tar archive)"))?;
    let meta = entries
        .iter()
        .find(|e| e.path == "metadata.gz")
        .ok_or_else(|| ForgeError::invalid("bad_gem", "the gem has no metadata.gz"))?;
    if !entries.iter().any(|e| e.path == "data.tar.gz") {
        return Err(ForgeError::invalid("bad_gem", "the gem has no data.tar.gz"));
    }
    let yaml = gunzip(&meta.data, 8 << 20)?;
    parse_spec(&String::from_utf8_lossy(&yaml))
}

async fn push(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let data = req.body().clone();
    let spec = read_gem(&data)?;
    validate_name(Eco::Rubygems, &spec.name)?;
    let ver = gem_version_string(&spec);
    super::names::validate_version(Eco::Rubygems, &spec.version)?;
    let opts = CreateOpts {
        visibility: None,
        link: PackageRegistry::repo_hint(req).or_else(|| {
            [&spec.source_code_uri, &spec.homepage]
                .into_iter()
                .find_map(|u| reg.repo_from_url(u))
        }),
        description: spec.summary.clone(),
    };
    let (o, p, cx, _) = reg
        .publish_target(&caller, owner, Eco::Rubygems, &spec.name, &opts)
        .await?;
    let pk = reg.pk();
    let digest = sha256_digest(&data);
    let key = super::Packages::content_key(&o.id, &digest);
    pk.ports.blobs.put(&key, data.clone()).await?;
    let fname = format!("{}-{ver}.gem", spec.name);
    let mut nv = NewVersion::new(&ver);
    nv.digest = digest.clone();
    nv.metadata_json = json!({
        "name": spec.name, "version": spec.version, "platform": if spec.platform.is_empty() { "ruby" } else { spec.platform.as_str() },
        "summary": spec.summary, "description": spec.description, "authors": spec.authors,
        "licenses": spec.licenses, "homepage": spec.homepage, "ruby_version": spec.ruby,
        "rubygems_version": spec.rubygems, "sha256": digest.trim_start_matches("sha256:"),
        "dependencies": spec.deps.iter().map(|(n, r, rt)| json!({"name": n, "requirement": r, "type": if *rt {"runtime"} else {"development"}})).collect::<Vec<_>>(),
    })
    .to_string();
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
    nv.on_conflict = OnConflict::Reject;
    nv.provenance = PackageRegistry::claim(req);
    pk.publish(&cx, &o, &p, nv).await.map_err(|e| {
        if e.subcode == "version_exists" || e.subcode == "version_deleted" {
            ForgeError::already_exists("gem version").with_subcode("version_exists")
        } else {
            e
        }
    })?;
    Ok(resp(
        200,
        &[("content-type", "text/plain")],
        format!("Successfully registered gem: {} ({})", spec.name, ver).into_bytes(),
    ))
}

fn form(req: &Req) -> Vec<(String, String)> {
    let mut v = query(req);
    for p in String::from_utf8_lossy(req.body())
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, x) = p.split_once('=').unwrap_or((p, ""));
        v.push((
            pct_decode(&k.replace('+', " ")),
            pct_decode(&x.replace('+', " ")),
        ));
    }
    v
}

async fn yank(reg: &PackageRegistry, req: &Req, owner: &str, yank: bool) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let f = form(req);
    let get = |k: &str| {
        f.iter()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let (name, version, platform) = (get("gem_name"), get("version"), get("platform"));
    if name.is_empty() || version.is_empty() {
        return Err(ForgeError::missing("gem_name and version"));
    }
    let ver = if platform.is_empty() || platform == "ruby" {
        version
    } else {
        format!("{version}-{platform}")
    };
    let (o, p, cx) = reg
        .pkg_for(Some(&caller), owner, Eco::Rubygems, &name, Need::Write)
        .await?;
    reg.pk()
        .update_version_flags(&cx, &o, &p, &ver, None, Some(yank))
        .await?;
    Ok(resp(
        200,
        &[("content-type", "text/plain")],
        format!(
            "Successfully {} gem: {name} ({ver})",
            if yank { "yanked" } else { "unyanked" }
        )
        .into_bytes(),
    ))
}

async fn download(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    file: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let stem = file
        .strip_suffix(".gem")
        .ok_or_else(|| ForgeError::not_found(format!("gem {file}")))?;
    // `<name>-<version>[-<platform>]`: the name ends at the last `-` before a digit.
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
    let mut found: Option<(PackageRec, String)> = None;
    for (i, _) in stem.match_indices('-') {
        let (name, ver) = (&stem[..i], &stem[i + 1..]);
        if !ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Some(p) = reg.pk().find(&lookup, &o, Eco::Rubygems, name).await? {
            found = Some((p, ver.to_string()));
            break;
        }
    }
    let Some((p, ver)) = found else {
        return Err(if caller.is_none() {
            ForgeError::new(
                bee_proto::v1::ErrorReason::Unauthenticated,
                "needs a credential",
            )
        } else {
            ForgeError::not_found(format!("gem {file}"))
        });
    };
    let cx = reg
        .pk()
        .authorize(caller.as_ref(), &p, Need::Read)
        .await
        .map_err(|e| {
            if caller.is_none() {
                ForgeError::new(
                    bee_proto::v1::ErrorReason::Unauthenticated,
                    "needs a credential",
                )
            } else {
                e
            }
        })?;
    let v = reg.pk().version(&cx, &p, &ver).await?;
    if v.yanked {
        return Err(ForgeError::not_found(format!("gem {file} was yanked")));
    }
    let f = reg
        .pk()
        .file(&cx, &v, file)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("gem {file}")))?;
    let len = f.size.to_string();
    let h = [
        ("content-type", "application/octet-stream"),
        ("content-length", len.as_str()),
    ];
    if head {
        return Ok(resp(200, &h, Bytes::new()));
    }
    let data = reg.pk().read_file(&f).await?;
    reg.count_download(&p, &v.id).await;
    Ok(resp(200, &h, data))
}

fn info_line(v: &VersionRec) -> String {
    let m: Value = serde_json::from_str(&v.metadata_json).unwrap_or(Value::Null);
    let deps: Vec<String> = m
        .get("dependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|d| d.get("type").and_then(Value::as_str) == Some("runtime"))
        .map(|d| {
            let req = d
                .get("requirement")
                .and_then(Value::as_str)
                .unwrap_or(">= 0");
            let req = req.split(", ").collect::<Vec<_>>().join("&");
            format!(
                "{}:{req}",
                d.get("name").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    let mut reqs = vec![format!(
        "checksum:{}",
        m.get("sha256")
            .and_then(Value::as_str)
            .unwrap_or(v.digest.trim_start_matches("sha256:"))
    )];
    for (k, f) in [("ruby", "ruby_version"), ("rubygems", "rubygems_version")] {
        if let Some(r) = m
            .get(f)
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty() && *r != ">= 0")
        {
            reqs.push(format!(
                "{k}:{}",
                r.split(", ").collect::<Vec<_>>().join("&")
            ));
        }
    }
    format!("{} {}|{}", v.version, deps.join(","), reqs.join(","))
}

async fn info_text(reg: &PackageRegistry, cx: &crate::cx::Cx, p: &PackageRec) -> Result<String> {
    let mut vs = reg.pk().all_versions(cx, p).await?;
    vs.reverse();
    let mut out = String::from("---\n");
    for v in vs.iter().filter(|v| !v.yanked) {
        out.push_str(&info_line(v));
        out.push('\n');
    }
    Ok(out)
}

fn text_response(body: String, head: bool) -> Resp {
    let etag = format!("\"{}\"", md5_hex(body.as_bytes()));
    resp(
        200,
        &[
            ("content-type", "text/plain; charset=utf-8"),
            ("etag", etag.as_str()),
            ("accept-ranges", "none"),
        ],
        if head {
            Bytes::new()
        } else {
            Bytes::from(body.into_bytes())
        },
    )
}

async fn readable(
    reg: &PackageRegistry,
    caller: &Option<crate::auth::Caller>,
    owner: &str,
) -> Result<Vec<(PackageRec, crate::cx::Cx)>> {
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
    let (pkgs, _) = reg
        .pk()
        .list(
            &lookup,
            &ListFilter {
                owner_id: o.id,
                eco: Some(Eco::Rubygems),
                ..Default::default()
            },
            "",
            10_000,
        )
        .await?;
    let mut out = Vec::new();
    for p in pkgs {
        if let Ok(cx) = reg.pk().authorize(caller.as_ref(), &p, Need::Read).await {
            out.push((p, cx));
        }
    }
    Ok(out)
}

async fn compact_versions(reg: &PackageRegistry, req: &Req, owner: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let pkgs = readable(reg, &caller, owner).await?;
    let mut out = format!("created_at: {}\n---\n", iso8601(0).replace(".000Z", "Z"));
    let mut names: Vec<(String, String)> = Vec::new();
    for (p, cx) in &pkgs {
        let mut vs = reg.pk().all_versions(cx, p).await?;
        vs.reverse();
        if vs.is_empty() {
            continue;
        }
        let list: Vec<String> = vs
            .iter()
            .map(|v| {
                if v.yanked {
                    format!("-{}", v.version)
                } else {
                    v.version.clone()
                }
            })
            .collect();
        let info = info_text(reg, cx, p).await?;
        names.push((
            p.name.clone(),
            format!(
                "{} {} {}\n",
                p.name,
                list.join(","),
                md5_hex(info.as_bytes())
            ),
        ));
    }
    names.sort();
    for (_, l) in names {
        out.push_str(&l);
    }
    Ok(text_response(out, header(req, "x-head") == "1"))
}

async fn compact_info(reg: &PackageRegistry, req: &Req, owner: &str, name: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Rubygems, name)
        .await?;
    Ok(text_response(info_text(reg, &cx, &p).await?, false))
}

async fn gem_json(reg: &PackageRegistry, req: &Req, owner: &str, name: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (o, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Rubygems, name)
        .await?;
    let vs = reg.pk().all_versions(&cx, &p).await?;
    let latest = vs
        .iter()
        .find(|v| !v.yanked)
        .ok_or_else(|| ForgeError::not_found(format!("gem {name}")))?;
    let m: Value = serde_json::from_str(&latest.metadata_json).unwrap_or(Value::Null);
    let usage = reg.pk().usage(&cx, &p, None).await?;
    let base = reg.pk().config().base_url.trim_end_matches('/').to_string();
    Ok(json_resp(
        200,
        &json!({
            "name": p.name, "version": m.get("version").cloned().unwrap_or(json!(latest.version)),
            "platform": m.get("platform").cloned().unwrap_or(json!("ruby")),
            "authors": m.get("authors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
            "info": p.description, "licenses": m.get("licenses").cloned().unwrap_or(json!([])),
            "downloads": usage.download_count,
            "gem_uri": format!("{base}/rubygems/{}/gems/{}-{}.gem", o.name, p.name, latest.version),
            "sha": m.get("sha256").cloned().unwrap_or(json!("")),
            "homepage_uri": m.get("homepage").cloned().unwrap_or(json!("")),
        }),
    ))
}

async fn versions_json(reg: &PackageRegistry, req: &Req, owner: &str, name: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Rubygems, name)
        .await?;
    let usage = reg.pk().usage(&cx, &p, None).await?;
    let vs = reg.pk().all_versions(&cx, &p).await?;
    let out: Vec<Value> = vs
        .iter()
        .map(|v| {
            let m: Value = serde_json::from_str(&v.metadata_json).unwrap_or(Value::Null);
            let number = m.get("version").and_then(Value::as_str).unwrap_or(&v.version).to_string();
            json!({
                "number": number, "platform": m.get("platform").cloned().unwrap_or(json!("ruby")),
                "created_at": iso8601(v.created_ms),
                "downloads_count": usage.versions.iter().find(|u| u.version_id == v.id).map(|u| u.download_count).unwrap_or(0),
                "prerelease": number.chars().any(|c| c.is_ascii_alphabetic()),
                "yanked": v.yanked, "sha": m.get("sha256").cloned().unwrap_or(json!("")),
            })
        })
        .collect();
    Ok(json_resp(200, &Value::Array(out)))
}

// ---- Ruby Marshal 4.8 for the legacy specs indexes ----

fn m_fix(out: &mut Vec<u8>, n: usize) {
    match n {
        0 => out.push(0),
        1..=122 => out.push(n as u8 + 5),
        123..=255 => out.extend_from_slice(&[1, n as u8]),
        256..=65_535 => out.extend_from_slice(&[2, (n & 0xff) as u8, (n >> 8) as u8]),
        _ => out.extend_from_slice(&[
            4,
            (n & 0xff) as u8,
            (n >> 8 & 0xff) as u8,
            (n >> 16 & 0xff) as u8,
            (n >> 24 & 0xff) as u8,
        ]),
    }
}

fn m_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(b"I\"");
    m_fix(out, s.len());
    out.extend_from_slice(s.as_bytes());
    out.extend_from_slice(&[6, b':', 6, b'E', b'T']);
}

/// Marshal 4.8 of an array of `[name, Gem::Version, platform]`.
pub(crate) fn marshal_specs(entries: &[(String, String, String)]) -> Vec<u8> {
    let mut out = vec![4, 8, b'['];
    m_fix(&mut out, entries.len());
    for (name, version, platform) in entries {
        out.push(b'[');
        m_fix(&mut out, 3);
        m_str(&mut out, name);
        out.extend_from_slice(b"U:\x11Gem::Version[\x06");
        m_str(&mut out, version);
        m_str(&mut out, platform);
    }
    out
}

async fn specs(reg: &PackageRegistry, req: &Req, owner: &str, which: &str) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let mut entries: Vec<(String, String, String)> = Vec::new();
    for (p, cx) in readable(reg, &caller, owner).await? {
        let vs: Vec<VersionRec> = reg
            .pk()
            .all_versions(&cx, &p)
            .await?
            .into_iter()
            .filter(|v| !v.yanked)
            .collect();
        let mut rows: Vec<(String, String, String)> = vs
            .iter()
            .map(|v| {
                let m: Value = serde_json::from_str(&v.metadata_json).unwrap_or(Value::Null);
                (
                    p.name.clone(),
                    m.get("version")
                        .and_then(Value::as_str)
                        .unwrap_or(&v.version)
                        .to_string(),
                    m.get("platform")
                        .and_then(Value::as_str)
                        .unwrap_or("ruby")
                        .to_string(),
                )
            })
            .collect();
        let pre = |v: &str| v.chars().any(|c| c.is_ascii_alphabetic());
        match which {
            "prerelease_specs.4.8.gz" => rows.retain(|r| pre(&r.1)),
            "latest_specs.4.8.gz" => {
                rows.retain(|r| !pre(&r.1));
                rows.truncate(1); // newest first
            }
            _ => rows.retain(|r| !pre(&r.1)),
        }
        rows.reverse();
        entries.extend(rows);
    }
    entries.sort();
    let _ = sha256_hex(b"");
    Ok(resp(
        200,
        &[("content-type", "application/octet-stream")],
        gzip(&marshal_specs(&entries)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "--- !ruby/object:Gem::Specification\nname: acme-widget\nversion: !ruby/object:Gem::Version\n  version: 1.2.0\nplatform: ruby\nauthors:\n- Ada\n- Bob\nsummary: A widget\ndescription: A longer text\nhomepage: https://beehive.invalid/acme/widget\nlicenses:\n- MIT\ndependencies:\n- !ruby/object:Gem::Dependency\n  name: rake\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \">=\"\n      - !ruby/object:Gem::Version\n        version: '12.0'\n    - - \"<\"\n      - !ruby/object:Gem::Version\n        version: '14'\n  type: :runtime\n  prerelease: false\n- !ruby/object:Gem::Dependency\n  name: rspec\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \"~>\"\n      - !ruby/object:Gem::Version\n        version: '3.0'\n  type: :development\n  prerelease: false\nrequired_ruby_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: 2.5.0\nrequired_rubygems_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: '0'\nmetadata:\n  source_code_uri: https://beehive.invalid/acme/widget\n";

    #[test]
    fn gemspec_is_read() {
        let s = parse_spec(SPEC).unwrap();
        assert_eq!(
            (s.name.as_str(), s.version.as_str(), s.platform.as_str()),
            ("acme-widget", "1.2.0", "ruby")
        );
        assert_eq!(s.authors, ["Ada", "Bob"]);
        assert_eq!(s.deps.len(), 2);
        assert_eq!(s.deps[0], ("rake".into(), ">= 12.0, < 14".into(), true));
        assert!(!s.deps[1].2);
        assert_eq!(s.ruby, ">= 2.5.0");
        assert_eq!(s.source_code_uri, "https://beehive.invalid/acme/widget");
    }

    #[test]
    fn marshal_is_well_formed() {
        let b = marshal_specs(&[("a".into(), "1.0.0".into(), "ruby".into())]);
        assert_eq!(&b[..3], &[4, 8, b'[']);
        assert!(b.windows(12).any(|w| w == b"Gem::Version"));
    }
}
