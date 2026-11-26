//! Maven repository layout (`/maven/<owner>/<group path>/<artifactId>/<version>/<file>`),
//! written from the public repository layout documentation.
//!
//! The package is `groupId:artifactId`; each version holds every file deployed under its
//! directory (jar, pom, sources, javadoc, signatures). `PUT` stores a file; checksum files
//! (`.sha1`, `.md5`, `.sha256`, `.sha512`) deployed after their artifact are verified against it
//! and acknowledged, and `GET` of a checksum is always computed from the stored artifact.
//! `maven-metadata.xml` is generated: at the artifact level from the live versions (with
//! `latest`, `release` and `lastUpdated`), at the level of a `-SNAPSHOT` version from the
//! timestamped files. Release versions are immutable (re-deploying different bytes is `409`,
//! identical bytes are accepted so a retried deploy succeeds); snapshots can be overwritten.
//! `DELETE` on a version directory deletes the version, on the artifact directory the package.

use bytes::Bytes;

use super::access::Need;
use super::hash::{md5_hex, sha1_hex, sha256_digest, sha256_hex, sha512_hex};
use super::http::{Req, Resp, header, iso8601, resp, status_of};
use super::manage::{NewFile, NewVersion, OnConflict};
use super::model::{Eco, FileRec};
use super::names::maven_cmp;
use super::registry::{CreateOpts, PackageRegistry, owner_and_rest};
use crate::community::tz::civil;
use crate::error::{ForgeError, Result};

const CHECKSUMS: [&str; 4] = [".sha1", ".md5", ".sha256", ".sha512"];

fn err(e: &ForgeError) -> Resp {
    let mut h = vec![("content-type", "text/plain")];
    if status_of(e) == 401 {
        h.push((
            "www-authenticate",
            "Basic realm=\"Beehive Maven\",charset=\"UTF-8\"",
        ));
    }
    resp(status_of(e), &h, e.message.clone().into_bytes())
}

/// Serves one Maven request.
pub(crate) async fn handle(reg: &PackageRegistry, req: Req) -> Resp {
    match route(reg, req).await {
        Ok(r) => r,
        Err(e) => err(&e),
    }
}

enum Target {
    /// `<group>/<artifact>/maven-metadata.xml[.sum]`
    ArtifactMeta { pkg: String, sum: String },
    /// `<group>/<artifact>/<version>/maven-metadata.xml[.sum]`
    VersionMeta {
        pkg: String,
        artifact: String,
        version: String,
        sum: String,
    },
    /// `<group>/<artifact>/<version>/<file>`
    File {
        pkg: String,
        artifact: String,
        version: String,
        file: String,
    },
    /// `<group>/<artifact>/<version>`
    VersionDir { pkg: String, version: String },
    /// `<group>/<artifact>`
    ArtifactDir { pkg: String },
}

fn checksum_of(file: &str) -> (&str, &str) {
    for s in CHECKSUMS {
        if let Some(base) = file.strip_suffix(s) {
            return (base, s);
        }
    }
    (file, "")
}

fn parse(segs: &[&str]) -> Option<Target> {
    let n = segs.len();
    let last = *segs.last()?;
    let (base, sum) = checksum_of(last);
    let pkg_of = |k: usize| -> Option<(String, String)> {
        // k segments are group + artifact.
        if k < 2 {
            return None;
        }
        let group = segs[..k - 1].join(".");
        let artifact = segs[k - 1].to_string();
        Some((format!("{group}:{artifact}"), artifact))
    };
    if base == "maven-metadata.xml" {
        let dir = segs.get(n.checked_sub(2)?)?;
        if dir.ends_with("-SNAPSHOT") {
            let (pkg, artifact) = pkg_of(n - 2)?;
            return Some(Target::VersionMeta {
                pkg,
                artifact,
                version: (*dir).to_string(),
                sum: sum.to_string(),
            });
        }
        let (pkg, _) = pkg_of(n - 1)?;
        return Some(Target::ArtifactMeta {
            pkg,
            sum: sum.to_string(),
        });
    }
    if n >= 4 && last.contains('.') {
        let (pkg, artifact) = pkg_of(n - 2)?;
        return Some(Target::File {
            pkg,
            artifact,
            version: segs[n - 2].to_string(),
            file: last.to_string(),
        });
    }
    if n >= 3
        && segs[n - 1]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        let (pkg, _) = pkg_of(n - 1)?;
        return Some(Target::VersionDir {
            pkg,
            version: segs[n - 1].to_string(),
        });
    }
    let (pkg, _) = pkg_of(n)?;
    Some(Target::ArtifactDir { pkg })
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next().unwrap_or("") {
        "xml" | "pom" => "application/xml",
        "jar" | "war" | "ear" | "aar" => "application/java-archive",
        "asc" => "application/pgp-signature",
        "json" | "module" => "application/json",
        _ => "application/octet-stream",
    }
}

fn checksum_value(sum: &str, data: &[u8], f: Option<&FileRec>) -> String {
    match sum {
        ".sha1" => f
            .filter(|f| !f.sha1.is_empty())
            .map(|f| f.sha1.clone())
            .unwrap_or_else(|| sha1_hex(data)),
        ".md5" => f
            .filter(|f| !f.md5.is_empty())
            .map(|f| f.md5.clone())
            .unwrap_or_else(|| md5_hex(data)),
        ".sha256" => sha256_hex(data),
        _ => sha512_hex(data),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn stamp(ms: u64) -> String {
    let (y, m, d) = civil((ms / 86_400_000) as i64);
    let rem = ms % 86_400_000;
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60
    )
}

async fn route(reg: &PackageRegistry, req: Req) -> Result<Resp> {
    let path = req.uri().path().to_string();
    let Some((owner, segs)) = owner_and_rest(&path, "/maven/") else {
        return Err(ForgeError::not_found("repository path"));
    };
    let target = parse(&segs).ok_or_else(|| ForgeError::not_found("repository path"))?;
    let method = req.method().as_str().to_string();
    match (target, method.as_str()) {
        (Target::ArtifactMeta { pkg, sum }, "GET" | "HEAD") => {
            artifact_meta(reg, &req, &owner, &pkg, &sum, method == "HEAD").await
        }
        (
            Target::VersionMeta {
                pkg,
                artifact,
                version,
                sum,
            },
            "GET" | "HEAD",
        ) => {
            version_meta(
                reg,
                &req,
                &owner,
                &pkg,
                &artifact,
                &version,
                &sum,
                method == "HEAD",
            )
            .await
        }
        (
            Target::File {
                pkg, version, file, ..
            },
            "GET" | "HEAD",
        ) => get_file(reg, &req, &owner, &pkg, &version, &file, method == "HEAD").await,
        (
            Target::File {
                pkg,
                artifact,
                version,
                file,
            },
            "PUT" | "POST",
        ) => put_file(reg, &req, &owner, &pkg, &artifact, &version, &file).await,
        (Target::ArtifactMeta { .. } | Target::VersionMeta { .. }, "PUT" | "POST") => {
            reg.require_caller(&req).await?;
            Ok(resp(201, &[], Bytes::new())) // generated, not stored
        }
        (Target::VersionDir { pkg, version }, "DELETE") => {
            let caller = reg.require_caller(&req).await?;
            let (o, p, cx) = reg
                .pkg_for(Some(&caller), &owner, Eco::Maven, &pkg, Need::Write)
                .await?;
            reg.pk().delete_version(&cx, &o, &p, &version).await?;
            Ok(resp(204, &[], Bytes::new()))
        }
        (Target::ArtifactDir { pkg }, "DELETE") => {
            let caller = reg.require_caller(&req).await?;
            let (o, p, cx) = reg
                .pkg_for(Some(&caller), &owner, Eco::Maven, &pkg, Need::Admin)
                .await?;
            reg.pk().delete(&cx, &o, &p).await?;
            Ok(resp(204, &[], Bytes::new()))
        }
        _ => Err(ForgeError::invalid(
            "unsupported",
            "method not allowed on this path",
        )),
    }
}

fn sum_response(sum: &str, body: &[u8], head: bool, ct: &str) -> Resp {
    if sum.is_empty() {
        let len = body.len().to_string();
        let h = [("content-type", ct), ("content-length", len.as_str())];
        return resp(
            200,
            &h,
            if head {
                Bytes::new()
            } else {
                Bytes::copy_from_slice(body)
            },
        );
    }
    let v = checksum_value(sum, body, None);
    resp(
        200,
        &[("content-type", "text/plain")],
        if head {
            Bytes::new()
        } else {
            Bytes::from(v.into_bytes())
        },
    )
}

async fn artifact_meta(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    pkg: &str,
    sum: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Maven, pkg)
        .await?;
    let mut versions: Vec<String> = reg
        .pk()
        .all_versions(&cx, &p)
        .await?
        .into_iter()
        .map(|v| v.version)
        .collect();
    versions.sort_by(|a, b| maven_cmp(a, b));
    let (group, artifact) = pkg.split_once(':').unwrap_or((pkg, ""));
    let release = versions
        .iter()
        .rev()
        .find(|v| !v.ends_with("-SNAPSHOT"))
        .cloned();
    let latest = versions.last().cloned();
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");
    x.push_str(&format!(
        "  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <versioning>\n",
        xml_escape(group),
        xml_escape(artifact)
    ));
    if let Some(l) = &latest {
        x.push_str(&format!("    <latest>{}</latest>\n", xml_escape(l)));
    }
    if let Some(r) = &release {
        x.push_str(&format!("    <release>{}</release>\n", xml_escape(r)));
    }
    x.push_str("    <versions>\n");
    for v in &versions {
        x.push_str(&format!("      <version>{}</version>\n", xml_escape(v)));
    }
    x.push_str(&format!(
        "    </versions>\n    <lastUpdated>{}</lastUpdated>\n  </versioning>\n</metadata>\n",
        stamp(p.updated_ms)
    ));
    Ok(sum_response(sum, x.as_bytes(), head, "application/xml"))
}

struct SnapFile {
    ts: String,
    build: u64,
    classifier: String,
    ext: String,
}

fn snapshot_files(artifact: &str, base: &str, files: &[FileRec]) -> Vec<SnapFile> {
    let prefix = format!("{artifact}-{base}-");
    files
        .iter()
        .filter_map(|f| {
            let rest = f.name.strip_prefix(&prefix)?;
            let (ts, rest) = rest.split_once('-')?;
            if ts.len() != 15 || ts.as_bytes()[8] != b'.' {
                return None;
            }
            let (build, tail) = match rest.split_once('-') {
                Some((b, t)) if b.chars().all(|c| c.is_ascii_digit()) => (b, Some(t)),
                _ => {
                    let (b, e) = rest.split_once('.')?;
                    return Some(SnapFile {
                        ts: ts.into(),
                        build: b.parse().ok()?,
                        classifier: String::new(),
                        ext: e.into(),
                    });
                }
            };
            let tail = tail?;
            let (classifier, ext) = tail.split_once('.')?;
            Some(SnapFile {
                ts: ts.into(),
                build: build.parse().ok()?,
                classifier: classifier.into(),
                ext: ext.into(),
            })
        })
        .filter(|s| !CHECKSUMS.iter().any(|c| s.ext.ends_with(&c[1..])))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn version_meta(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    pkg: &str,
    artifact: &str,
    version: &str,
    sum: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Maven, pkg)
        .await?;
    let v = reg.pk().version(&cx, &p, version).await?;
    let files = reg.pk().files(&cx, &v).await?;
    let base = version.trim_end_matches("-SNAPSHOT");
    let snaps = snapshot_files(artifact, base, &files);
    let group = pkg.split_once(':').map(|(g, _)| g).unwrap_or(pkg);
    let mut x = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata modelVersion=\"1.1.0\">\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <version>{}</version>\n",
        xml_escape(group),
        xml_escape(artifact),
        xml_escape(version)
    );
    if let Some(newest) = snaps
        .iter()
        .max_by(|a, b| (&a.ts, a.build).cmp(&(&b.ts, b.build)))
    {
        x.push_str(&format!("  <versioning>\n    <snapshot>\n      <timestamp>{}</timestamp>\n      <buildNumber>{}</buildNumber>\n    </snapshot>\n    <lastUpdated>{}</lastUpdated>\n    <snapshotVersions>\n", newest.ts, newest.build, newest.ts.replace('.', "")));
        let mut seen = std::collections::BTreeSet::new();
        let mut latest: Vec<&SnapFile> = Vec::new();
        for s in snaps
            .iter()
            .filter(|s| s.ts == newest.ts && s.build == newest.build)
        {
            if seen.insert((s.classifier.clone(), s.ext.clone())) {
                latest.push(s);
            }
        }
        for s in latest {
            x.push_str("      <snapshotVersion>\n");
            if !s.classifier.is_empty() {
                x.push_str(&format!(
                    "        <classifier>{}</classifier>\n",
                    xml_escape(&s.classifier)
                ));
            }
            x.push_str(&format!("        <extension>{}</extension>\n        <value>{base}-{}-{}</value>\n        <updated>{}</updated>\n      </snapshotVersion>\n", xml_escape(&s.ext), s.ts, s.build, s.ts.replace('.', "")));
        }
        x.push_str("    </snapshotVersions>\n  </versioning>\n");
    }
    x.push_str("</metadata>\n");
    Ok(sum_response(sum, x.as_bytes(), head, "application/xml"))
}

async fn get_file(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    pkg: &str,
    version: &str,
    file: &str,
    head: bool,
) -> Result<Resp> {
    let caller = reg.caller(req).await?;
    let (_, p, cx) = reg
        .read_pkg(caller.as_ref(), owner, Eco::Maven, pkg)
        .await?;
    let v = reg.pk().version(&cx, &p, version).await?;
    let (base, sum) = checksum_of(file);
    let stored = reg
        .pk()
        .file(&cx, &v, if sum.is_empty() { file } else { base })
        .await?;
    let f = stored.ok_or_else(|| ForgeError::not_found(format!("file {file}")))?;
    if !sum.is_empty() {
        let data = reg.pk().read_file(&f).await?;
        let val = checksum_value(sum, &data, Some(&f));
        return Ok(resp(
            200,
            &[("content-type", "text/plain")],
            if head {
                Bytes::new()
            } else {
                Bytes::from(val.into_bytes())
            },
        ));
    }
    let len = f.size.to_string();
    let lm = iso8601(f.created_ms);
    let headers = [
        ("content-type", content_type(file)),
        ("content-length", len.as_str()),
        ("last-modified", lm.as_str()),
    ];
    if head {
        return Ok(resp(200, &headers, Bytes::new()));
    }
    let data = reg.pk().read_file(&f).await?;
    if !matches!(
        file.rsplit('.').next(),
        Some("pom" | "asc" | "module" | "xml")
    ) {
        reg.count_download(&p, &v.id).await;
    }
    Ok(resp(200, &headers, data))
}

async fn put_file(
    reg: &PackageRegistry,
    req: &Req,
    owner: &str,
    pkg: &str,
    artifact: &str,
    version: &str,
    file: &str,
) -> Result<Resp> {
    let caller = reg.require_caller(req).await?;
    let pk = reg.pk();
    super::names::validate_name(Eco::Maven, pkg)?;
    super::names::validate_version(Eco::Maven, version)?;
    let (base, sum) = checksum_of(file);
    if !file.starts_with(&format!(
        "{artifact}-{}",
        version.trim_end_matches("-SNAPSHOT")
    )) && !sum.is_empty()
        && !base.starts_with(&format!("{artifact}-"))
    {
        return Err(ForgeError::invalid(
            "bad_file_name",
            format!("`{file}` must start with `{artifact}-{version}`"),
        ));
    }
    if !sum.is_empty() {
        // Verify against the artifact when it is there; a checksum alone is acknowledged.
        let (_, p, cx) = reg
            .pkg_for(Some(&caller), owner, Eco::Maven, pkg, Need::Write)
            .await?;
        if let Ok(v) = pk.version(&cx, &p, version).await
            && let Some(f) = pk.file(&cx, &v, base).await?
        {
            let data = pk.read_file(&f).await?;
            let want = checksum_value(sum, &data, Some(&f));
            let got = String::from_utf8_lossy(req.body())
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if got != want {
                return Err(ForgeError::invalid(
                    "checksum_mismatch",
                    format!("{file} does not match {base}"),
                ));
            }
        }
        return Ok(resp(201, &[], Bytes::new()));
    }
    if !file.starts_with(&format!(
        "{artifact}-{}",
        version.trim_end_matches("-SNAPSHOT")
    )) {
        return Err(ForgeError::invalid(
            "bad_file_name",
            format!("`{file}` must start with `{artifact}-{version}`"),
        ));
    }
    let hint = CreateOpts::default();
    let (o, p, cx, _) = reg
        .publish_target(
            &caller,
            owner,
            Eco::Maven,
            pkg,
            &CreateOpts {
                link: PackageRegistry::repo_hint(req),
                ..hint
            },
        )
        .await?;
    let snapshot = version.ends_with("-SNAPSHOT");
    let data = req.body().clone();
    let digest = sha256_digest(&data);
    if !snapshot
        && let Ok(v) = pk.version(&cx, &p, version).await
        && let Some(f) = pk.file(&cx, &v, file).await?
    {
        if f.digest == digest {
            return Ok(resp(201, &[], Bytes::new()));
        }
        return Err(
            ForgeError::already_exists(format!("file {file} of {pkg}:{version}"))
                .with_subcode("file_exists"),
        );
    }
    let key = super::Packages::content_key(&o.id, &digest);
    pk.ports.blobs.put(&key, data.clone()).await?;
    let (group, _) = pkg.split_once(':').unwrap_or((pkg, ""));
    let mut nv = NewVersion::new(version);
    nv.digest = digest.clone();
    nv.metadata_json =
        serde_json::json!({"groupId": group, "artifactId": artifact, "version": version})
            .to_string();
    nv.files = vec![NewFile {
        name: file.to_string(),
        size: data.len() as u64,
        digest,
        media_type: content_type(file).to_string(),
        blob_key: key,
        sha1: sha1_hex(&data),
        md5: md5_hex(&data),
        sha512_b64: super::hash::sha512_b64(&data),
    }];
    nv.on_conflict = OnConflict::Merge {
        overwrite: snapshot,
    };
    nv.provenance = PackageRegistry::claim(req);
    pk.publish(&cx, &o, &p, nv).await?;
    let _ = header(req, "content-type");
    Ok(resp(201, &[], Bytes::new()))
}
