//! Serving a site: [`serve`] is a pure function of a manifest and a request; [`Pages::serve`]
//! finds the manifest for a host; [`Pages::respond`] turns an `http::Request` into an
//! `http::Response`, ready for the server to mount on its static-site listener.
//!
//! # Rules, in order
//!
//! 1. **HTTPS.** A site that enforces HTTPS answers plain HTTP with `301` to the `https` URL.
//! 2. **Paths.** The path is percent-decoded, repeated slashes collapse, `..` and NUL are `404`.
//!    Any segment starting with `.` other than `.well-known` is `404`.
//! 3. **Project base.** On the default host `https://<org>.<pages domain>/<repo>/`, the repo
//!    segment is the base: `/<repo>` redirects to `/<repo>/`, other repos are other sites.
//! 4. **Redirects.** Rules of the `_redirects` file in order: exact paths, or a trailing `/*`
//!    with `:splat` in the target; status 301, 302, 303, 307, 308 redirect, 200 rewrites. A rule
//!    applies only when no file exists at the path, unless it is forced with `!`.
//! 5. **Files.** `/` and any path ending in `/` serve `index.html` of that folder; a path
//!    without a slash serves the file, else `<path>.html`; a folder requested without its slash
//!    redirects (`301`) to the slash form, query preserved.
//! 6. **Not found.** `404.html` at the root, status `404`; otherwise a short built-in page.
//!
//! Responses carry `Content-Type` by extension, `Cache-Control: max-age=600`, a strong `ETag`
//! from the content id (`304` on a matching `If-None-Match`) and
//! `X-Content-Type-Options: nosniff`.

use std::collections::BTreeSet;

use bee_policy::Action;
use bee_proto::v1::{PagesAccess, PagesDeploymentState};
use bytes::Bytes;

use super::model::{DeploymentRec, EntryRec, HostRec, ManifestRec, RedirectRec};
use super::{DEPLOYMENTS, HOSTS, MANIFESTS, Pages, SURFACE};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::packages::http::{pct_decode, resp as resp_with};

/// A request to a site.
#[derive(Clone, Debug, Default)]
pub struct ServeRequest<'a> {
    /// Host name, with or without a port.
    pub host: &'a str,
    /// Raw request path (percent-encoded).
    pub path: &'a str,
    /// Raw query string without `?`.
    pub query: &'a str,
    /// The request arrived over TLS.
    pub https: bool,
    /// Value of `If-None-Match`.
    pub if_none_match: &'a str,
}

/// The state of a site that a request is served against.
#[derive(Clone, Debug)]
pub struct SiteView<'a> {
    /// The deployment's files and rules.
    pub manifest: &'a ManifestRec,
    /// Redirect plain HTTP to HTTPS.
    pub enforce_https: bool,
    /// Path prefix of a project site on the default host (`/repo`), empty otherwise.
    pub base_path: &'a str,
}

/// The answer to a [`ServeRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Served {
    /// Serve the content `blob` (SHA-256 hex) of `size` bytes with `status` and `headers`.
    File {
        /// HTTP status (200, or 404 for `404.html`).
        status: u16,
        /// Content id, readable with [`Pages::read_site_blob`].
        blob: String,
        /// Size in bytes.
        size: u64,
        /// Response headers.
        headers: Vec<(String, String)>,
    },
    /// `304`: the client's copy is current.
    NotModified {
        /// Response headers (the `ETag`).
        headers: Vec<(String, String)>,
    },
    /// A redirect.
    Redirect {
        /// 301, 302, 303, 307 or 308.
        status: u16,
        /// Target URL or path.
        location: String,
    },
    /// A generated body (the built-in 404).
    Body {
        /// HTTP status.
        status: u16,
        /// Response headers.
        headers: Vec<(String, String)>,
        /// Body.
        body: Vec<u8>,
    },
    /// The site is private and the request carries no credential.
    AuthRequired,
}

fn not_found_page() -> Served {
    Served::Body {
        status: 404,
        headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
        body: b"<!doctype html><title>404</title><h1>404</h1><p>File not found.</p>".to_vec(),
    }
}

fn file_headers(e: &EntryRec) -> Vec<(String, String)> {
    vec![
        ("content-type".into(), e.content_type.clone()),
        ("cache-control".into(), "max-age=600".into()),
        (
            "etag".into(),
            format!("\"{}\"", &e.blob[..e.blob.len().min(32)]),
        ),
        ("x-content-type-options".into(), "nosniff".into()),
    ]
}

fn serve_entry(e: &EntryRec, status: u16, inm: &str) -> Served {
    let headers = file_headers(e);
    let etag = format!("\"{}\"", &e.blob[..e.blob.len().min(32)]);
    if status == 200
        && !inm.is_empty()
        && inm
            .split(',')
            .any(|t| t.trim().trim_start_matches("W/") == etag)
    {
        return Served::NotModified {
            headers: vec![("etag".into(), etag)],
        };
    }
    Served::File {
        status,
        blob: e.blob.clone(),
        size: e.size,
        headers,
    }
}

/// Collapses `//`, decodes `%xx`; `None` for NUL or `..`.
fn clean_path(raw: &str) -> Option<String> {
    let p = pct_decode(raw.split('?').next().unwrap_or(raw));
    if p.contains('\0') {
        return None;
    }
    let trailing = p.ends_with('/');
    let mut parts = Vec::new();
    for c in p.split('/').filter(|c| !c.is_empty() && *c != ".") {
        if c == ".." {
            return None;
        }
        parts.push(c);
    }
    let mut out = format!("/{}", parts.join("/"));
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    Some(out)
}

fn hidden(path: &str) -> bool {
    path.split('/')
        .any(|s| s.starts_with('.') && s != ".well-known" && !s.is_empty())
}

struct Files<'a> {
    by_path: std::collections::BTreeMap<&'a str, &'a EntryRec>,
    dirs: BTreeSet<String>,
}

impl<'a> Files<'a> {
    fn new(m: &'a ManifestRec) -> Files<'a> {
        let mut dirs = BTreeSet::new();
        let mut by_path = std::collections::BTreeMap::new();
        for e in &m.entries {
            by_path.insert(e.path.as_str(), e);
            let mut p = e.path.as_str();
            while let Some((d, _)) = p.rsplit_once('/') {
                dirs.insert(d.to_string());
                p = d;
            }
        }
        Files { by_path, dirs }
    }

    /// The entry served for `path` (leading slash), or a redirect to the slash form.
    fn resolve(&self, path: &str) -> Resolved<'a> {
        let rel = path.trim_start_matches('/');
        if rel.is_empty() {
            return self
                .by_path
                .get("index.html")
                .map_or(Resolved::Missing, |e| Resolved::Entry(e));
        }
        if let Some(dir) = rel.strip_suffix('/') {
            return self
                .by_path
                .get(format!("{dir}/index.html").as_str())
                .map_or(Resolved::Missing, |e| Resolved::Entry(e));
        }
        if let Some(e) = self.by_path.get(rel) {
            return Resolved::Entry(e);
        }
        if let Some(e) = self.by_path.get(format!("{rel}.html").as_str()) {
            return Resolved::Entry(e);
        }
        if self.dirs.contains(rel)
            && self
                .by_path
                .contains_key(format!("{rel}/index.html").as_str())
        {
            return Resolved::SlashRedirect;
        }
        Resolved::Missing
    }
}

enum Resolved<'a> {
    Entry(&'a EntryRec),
    SlashRedirect,
    Missing,
}

fn rule_match(rule: &RedirectRec, path: &str) -> Option<String> {
    if let Some(prefix) = rule.from.strip_suffix("/*") {
        let rest = path.strip_prefix(prefix)?;
        let splat = rest
            .strip_prefix('/')
            .or(if rest.is_empty() { Some("") } else { None })?;
        return Some(rule.to.replace(":splat", splat));
    }
    let a = rule.from.trim_end_matches('/');
    let b = path.trim_end_matches('/');
    (a == b).then(|| rule.to.clone())
}

/// Serves `req` from `view`. Pure: the same manifest and request always give the same answer.
pub fn serve(view: &SiteView<'_>, req: &ServeRequest<'_>) -> Served {
    let host = req.host.split(':').next().unwrap_or("");
    if view.enforce_https && !req.https {
        let q = if req.query.is_empty() {
            String::new()
        } else {
            format!("?{}", req.query)
        };
        return Served::Redirect {
            status: 301,
            location: format!("https://{host}{}{q}", req.path),
        };
    }
    let Some(full) = clean_path(req.path) else {
        return not_found_page();
    };
    let q = if req.query.is_empty() {
        String::new()
    } else {
        format!("?{}", req.query)
    };
    let path = if view.base_path.is_empty() {
        full
    } else if full == view.base_path {
        return Served::Redirect {
            status: 301,
            location: format!("{full}/{q}"),
        };
    } else if let Some(rest) = full.strip_prefix(&format!("{}/", view.base_path)) {
        format!("/{rest}")
    } else {
        return not_found_page();
    };
    if hidden(&path) {
        return not_found_page();
    }
    let files = Files::new(view.manifest);
    let resolved = files.resolve(&path);
    let exists = !matches!(resolved, Resolved::Missing);
    for rule in &view.manifest.redirects {
        if exists && !rule.force {
            continue;
        }
        let Some(target) = rule_match(rule, &path) else {
            continue;
        };
        if rule.status == 200 {
            if let Resolved::Entry(e) = files.resolve(&target) {
                return serve_entry(e, 200, req.if_none_match);
            }
            continue;
        }
        let location = if target.starts_with('/') {
            format!("{}{target}", view.base_path)
        } else {
            target
        };
        return Served::Redirect {
            status: rule.status as u16,
            location,
        };
    }
    match resolved {
        Resolved::Entry(e) => serve_entry(e, 200, req.if_none_match),
        Resolved::SlashRedirect => Served::Redirect {
            status: 301,
            location: format!("{}{}/{q}", view.base_path, path),
        },
        Resolved::Missing => match files.by_path.get("404.html") {
            Some(e) => {
                let mut headers = file_headers(e);
                headers.retain(|(k, _)| k != "etag" && k != "cache-control");
                Served::File {
                    status: 404,
                    blob: e.blob.clone(),
                    size: e.size,
                    headers,
                }
            }
            None => not_found_page(),
        },
    }
}

impl Pages {
    /// The bytes of a content id of a repo's site.
    pub async fn read_site_blob(&self, repo_id: &str, blob: &str) -> Result<Bytes> {
        self.blobs
            .get(&Self::blob_key(repo_id, blob))
            .await?
            .ok_or_else(|| {
                ForgeError::new(
                    bee_proto::v1::ErrorReason::Integrity,
                    "site content is missing from the store",
                )
                .with_subcode("blob_missing")
            })
    }

    /// Finds what `req.host` serves and answers the request. Unknown hosts, unpublished sites,
    /// expired previews and private sites the caller cannot read are `404`; a private site
    /// without a credential is [`Served::AuthRequired`].
    pub async fn serve(
        &self,
        caller: Option<&Caller>,
        req: &ServeRequest<'_>,
    ) -> Result<(Served, Option<String>)> {
        let cx = Cx::system();
        let host = req.host.split(':').next().unwrap_or("").to_lowercase();
        let now = self.db.now_ms();
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let (repo_id, base_path, preview): (String, String, Option<String>) =
            match tx.get(&HOSTS, "", &host).await? {
                Some(r) => {
                    let h: HostRec = r.decode()?;
                    let dep = (h.kind == "preview").then_some(h.deployment_id);
                    (h.repo_id, String::new(), dep)
                }
                None => {
                    let suffix = format!(".{}", self.config.pages_domain);
                    let Some(org_name) = host.strip_suffix(&suffix).filter(|o| !o.contains('.'))
                    else {
                        return Ok((not_found_page(), None));
                    };
                    let clean = clean_path(req.path).unwrap_or_default();
                    let Some(repo_name) = clean
                        .trim_start_matches('/')
                        .split('/')
                        .next()
                        .filter(|s| !s.is_empty())
                    else {
                        return Ok((not_found_page(), None));
                    };
                    drop(tx);
                    let repo = match self
                        .orgs
                        .resolve_repo(
                            &cx,
                            &bee_proto::v1::RepoRef {
                                repo: Some(bee_proto::v1::repo_ref::Repo::FullName(format!(
                                    "{org_name}/{repo_name}"
                                ))),
                            },
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => return Ok((not_found_page(), None)),
                    };
                    tx = self.db.begin(SURFACE, &cx).await?;
                    (repo.id, format!("/{repo_name}"), None)
                }
            };
        let Ok(repo) = self.orgs.repo_any(&cx, &repo_id).await else {
            return Ok((not_found_page(), None));
        };
        let Some((site, _)) = Self::load_site(&mut tx, &repo.org_id, &repo.id).await? else {
            return Ok((not_found_page(), None));
        };
        let dep_id = preview
            .clone()
            .unwrap_or_else(|| site.current_deployment_id.clone());
        if dep_id.is_empty() {
            return Ok((not_found_page(), None));
        }
        let Some(dep) = tx
            .get_msg::<DeploymentRec>(&DEPLOYMENTS, &repo.id, &dep_id)
            .await?
            .map(|(d, _)| d)
        else {
            return Ok((not_found_page(), None));
        };
        if dep.state != PagesDeploymentState::Succeeded as i32
            || (dep.preview && dep.expires_ms != 0 && dep.expires_ms <= now)
        {
            return Ok((not_found_page(), None));
        }
        let Some(manifest) = tx
            .get_msg::<ManifestRec>(&MANIFESTS, &repo.id, &dep.manifest_id)
            .await?
            .map(|(m, _)| m)
        else {
            return Ok((not_found_page(), None));
        };
        drop(tx);
        if site.access == PagesAccess::Private as i32 {
            let Some(c) = caller else {
                return Ok((Served::AuthRequired, None));
            };
            if self
                .authz
                .repo(c, &repo, Action::Read, &[], None, None)
                .await
                .is_err()
            {
                return Ok((not_found_page(), None));
            }
        }
        let enforce = site.enforce_https && preview.is_none();
        let view = SiteView {
            manifest: &manifest,
            enforce_https: enforce,
            base_path: &base_path,
        };
        Ok((
            serve(
                &view,
                &ServeRequest {
                    host: &host,
                    ..req.clone()
                },
            ),
            Some(repo.id),
        ))
    }

    /// Answers an HTTP request for a site. `caller` is the authenticated reader (private
    /// sites), `https` whether the connection is TLS. `HEAD` gets headers only.
    pub async fn respond(
        &self,
        caller: Option<&Caller>,
        req: &http::Request<Bytes>,
        https: bool,
    ) -> http::Response<Bytes> {
        let host = req
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().host())
            .unwrap_or("");
        let inm = req
            .headers()
            .get("if-none-match")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let sr = ServeRequest {
            host,
            path: req.uri().path(),
            query: req.uri().query().unwrap_or(""),
            https,
            if_none_match: inm,
        };
        let head = req.method() == http::Method::HEAD;
        if req.method() != http::Method::GET && !head {
            return resp_with(405, &[("allow", "GET, HEAD")], Bytes::new());
        }
        match self.serve(caller, &sr).await {
            Ok((
                Served::File {
                    status,
                    blob,
                    size,
                    headers,
                },
                Some(repo_id),
            )) => {
                let hs: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                if head {
                    let len = size.to_string();
                    let mut all = hs.clone();
                    all.push(("content-length", len.as_str()));
                    return resp_with(status, &all, Bytes::new());
                }
                match self.read_site_blob(&repo_id, &blob).await {
                    Ok(b) => resp_with(status, &hs, b),
                    Err(_) => resp_with(
                        502,
                        &[("content-type", "text/plain")],
                        Bytes::from_static(b"site content unavailable"),
                    ),
                }
            }
            Ok((Served::File { .. }, None)) => resp_with(500, &[], Bytes::new()),
            Ok((Served::NotModified { headers }, _)) => {
                let hs: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                resp_with(304, &hs, Bytes::new())
            }
            Ok((Served::Redirect { status, location }, _)) => {
                resp_with(status, &[("location", location.as_str())], Bytes::new())
            }
            Ok((
                Served::Body {
                    status,
                    headers,
                    body,
                },
                _,
            )) => {
                let hs: Vec<(&str, &str)> = headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                resp_with(
                    status,
                    &hs,
                    if head {
                        Bytes::new()
                    } else {
                        Bytes::from(body)
                    },
                )
            }
            Ok((Served::AuthRequired, _)) => resp_with(
                401,
                &[
                    ("www-authenticate", "Bearer realm=\"Beehive Pages\""),
                    ("content-type", "text/plain"),
                ],
                Bytes::from_static(b"this site is private; sign in"),
            ),
            Err(e) => resp_with(
                if e.reason == bee_proto::v1::ErrorReason::Unavailable {
                    503
                } else {
                    500
                },
                &[("content-type", "text/plain")],
                Bytes::from(e.message.into_bytes()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, body: &str) -> EntryRec {
        EntryRec {
            path: path.into(),
            blob: crate::packages::hash::sha256_hex(body.as_bytes()),
            size: body.len() as u64,
            content_type: super::super::deploy::content_type(path).into(),
        }
    }

    fn manifest(files: &[&str], redirects: &str) -> ManifestRec {
        ManifestRec {
            id: "m".into(),
            entries: files.iter().map(|f| entry(f, f)).collect(),
            redirects: super::super::deploy::parse_redirects(redirects).unwrap(),
            total_bytes: 0,
        }
    }

    fn get(m: &ManifestRec, path: &str) -> Served {
        serve(
            &SiteView {
                manifest: m,
                enforce_https: false,
                base_path: "",
            },
            &ServeRequest {
                host: "x.test",
                path,
                https: true,
                ..Default::default()
            },
        )
    }

    fn status(s: &Served) -> u16 {
        match s {
            Served::File { status, .. }
            | Served::Body { status, .. }
            | Served::Redirect { status, .. } => *status,
            Served::NotModified { .. } => 304,
            Served::AuthRequired => 401,
        }
    }

    #[test]
    fn index_and_html_rules() {
        let m = manifest(
            &[
                "index.html",
                "about.html",
                "docs/index.html",
                "docs/guide.html",
                "a.css",
            ],
            "",
        );
        assert_eq!(status(&get(&m, "/")), 200);
        assert_eq!(status(&get(&m, "/about")), 200);
        assert_eq!(status(&get(&m, "/about.html")), 200);
        assert_eq!(status(&get(&m, "/docs/")), 200);
        assert_eq!(status(&get(&m, "/docs/guide")), 200);
        assert_eq!(
            get(&m, "/docs"),
            Served::Redirect {
                status: 301,
                location: "/docs/".into()
            }
        );
        assert_eq!(status(&get(&m, "/missing")), 404);
        assert_eq!(status(&get(&m, "/../etc/passwd")), 404);
        assert_eq!(status(&get(&m, "/%2e%2e/x")), 404);
    }

    #[test]
    fn custom_404_and_dotfiles() {
        let m = manifest(&["index.html", "404.html", ".env", ".well-known/a.txt"], "");
        match get(&m, "/nope") {
            Served::File { status, blob, .. } => {
                assert_eq!(status, 404);
                assert_eq!(blob, crate::packages::hash::sha256_hex(b"404.html"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(status(&get(&m, "/.env")), 404);
        assert_eq!(status(&get(&m, "/.well-known/a.txt")), 200);
    }

    #[test]
    fn redirects_and_rewrites() {
        let m = manifest(
            &["index.html", "new.html", "kept.html"],
            "/old /new.html\n/blog/* /posts/:splat 302\n/kept /new.html 301!\n/app/* /index.html 200\n",
        );
        assert_eq!(
            get(&m, "/old"),
            Served::Redirect {
                status: 301,
                location: "/new.html".into()
            }
        );
        assert_eq!(
            get(&m, "/blog/a/b"),
            Served::Redirect {
                status: 302,
                location: "/posts/a/b".into()
            }
        );
        // A forced rule beats an existing file.
        assert_eq!(
            get(&m, "/kept"),
            Served::Redirect {
                status: 301,
                location: "/new.html".into()
            }
        );
        // A rewrite serves the target without changing the URL.
        assert_eq!(status(&get(&m, "/app/anything")), 200);
    }

    #[test]
    fn etags_and_https() {
        let m = manifest(&["index.html"], "");
        let Served::File { headers, .. } = get(&m, "/") else {
            panic!()
        };
        let etag = headers.iter().find(|(k, _)| k == "etag").unwrap().1.clone();
        let view = SiteView {
            manifest: &m,
            enforce_https: true,
            base_path: "",
        };
        let r = ServeRequest {
            host: "x.test",
            path: "/",
            https: true,
            if_none_match: &etag,
            ..Default::default()
        };
        assert!(matches!(serve(&view, &r), Served::NotModified { .. }));
        let r = ServeRequest {
            host: "x.test",
            path: "/a",
            query: "q=1",
            https: false,
            ..Default::default()
        };
        assert_eq!(
            serve(&view, &r),
            Served::Redirect {
                status: 301,
                location: "https://x.test/a?q=1".into()
            }
        );
    }

    #[test]
    fn project_base_path() {
        let m = manifest(&["index.html", "x/index.html"], "");
        let view = SiteView {
            manifest: &m,
            enforce_https: false,
            base_path: "/repo",
        };
        let at = |p: &str| {
            serve(
                &view,
                &ServeRequest {
                    host: "o.pages.test",
                    path: p,
                    https: true,
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            at("/repo"),
            Served::Redirect {
                status: 301,
                location: "/repo/".into()
            }
        );
        assert_eq!(status(&at("/repo/")), 200);
        assert_eq!(
            at("/repo/x"),
            Served::Redirect {
                status: 301,
                location: "/repo/x/".into()
            }
        );
        assert_eq!(status(&at("/other/")), 404);
    }
}
