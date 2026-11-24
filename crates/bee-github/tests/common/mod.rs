//! Test harness: a local git repository shaped like a small GitHub project, and an
//! in-process fake of the GitHub REST/GraphQL/LFS APIs serving metadata written here (no
//! recorded GitHub data). Faults (rate limits, secondary limits, crashes) are switchable.

#![allow(dead_code, missing_docs, clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bee_core::Ulid;
use bee_github::clock::{Clock, ManualClock};
use bee_github::state::StateDir;
use bee_github::target::StoreTarget;
use bee_github::{Client, ClientConfig, Importer, MemorySink, Options};
use bee_import::ImportTarget;
use bee_store::{
    DEFAULT_DRAFT_RETENTION_DAYS, ManualClock as StoreClock, MemoryBlobStore, MemoryMetaStore,
    MetaStore, OrgId, OrgRow, RepoId, RepoRow, Writer,
};
use serde_json::{Value, json};

pub const OWNER: &str = "acme";
pub const NAME: &str = "api";
pub const FULL: &str = "acme/api";
pub const LFS_DATA: &[u8] = b"large binary content stored in git lfs\n";
pub const ASSET_DATA: &[u8] = b"release notes asset\n";
pub const UNREACHABLE: &str = "0123456789abcdef0123456789abcdef01234567";

// ----- the git side ----------------------------------------------------------------------

pub struct Source {
    pub dir: tempfile::TempDir,
    pub shas: BTreeMap<&'static str, String>,
}

impl Source {
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("src")
    }
    pub fn sha(&self, k: &str) -> String {
        self.shas[k].clone()
    }
    /// Every commit reachable from the refs GitHub serves (branches, tags, pull refs).
    pub fn all_commits(&self) -> Vec<String> {
        let out = git(
            &self.path(),
            &["rev-list", "--branches", "--tags", "--glob=refs/pull/*"],
        );
        out.lines().map(str::to_string).collect()
    }
}

static TICK: AtomicU64 = AtomicU64::new(0);

pub fn git(dir: &Path, args: &[&str]) -> String {
    let t = 1_700_000_000 + TICK.fetch_add(60, Ordering::SeqCst);
    let date = format!("{t} +0000");
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Alice")
        .env("GIT_AUTHOR_EMAIL", "alice@example.com")
        .env("GIT_COMMITTER_NAME", "Alice")
        .env("GIT_COMMITTER_EMAIL", "alice@example.com")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub fn lfs_oid() -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(LFS_DATA);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The repository: `main` with a merge of `fix` (PR #3), `feat` force-pushed (PR #2, its
/// old head kept under a hidden ref), PR #5 whose branch was deleted, and tag `v1.0`.
pub fn source(messy: bool) -> Source {
    let dir = tempfile::tempdir().unwrap();
    let s = dir.path().join("src");
    std::fs::create_dir_all(&s).unwrap();
    git(&s, &["init", "-q", "-b", "main"]);
    git(&s, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    git(&s, &["config", "commit.gpgsign", "false"]);
    git(&s, &["config", "tag.gpgsign", "false"]);
    let mut shas = BTreeMap::new();
    write(&s, "README.md", "# api\n");
    write(&s, "src/lib.rs", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
    let extra = if messy { "[ab]* @bob\n" } else { "" };
    write(
        &s,
        ".github/CODEOWNERS",
        &format!("* @acme/core\n/docs/ @alice\n{extra}"),
    );
    write(&s, ".github/workflows/ci.yml", "on: push\njobs: {}\n");
    write(
        &s,
        "big.bin",
        &format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            lfs_oid(),
            LFS_DATA.len()
        ),
    );
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-q", "-m", "init"]);
    shas.insert("c1", git(&s, &["rev-parse", "HEAD"]));
    // PR #2: first push, then a force-push.
    git(&s, &["checkout", "-q", "-b", "feat"]);
    write(&s, "src/lib.rs", "a\nB1\nc\nd\ne\nf\ng\nh\ni\nj\n");
    git(&s, &["commit", "-q", "-am", "feat v1"]);
    shas.insert("c2a", git(&s, &["rev-parse", "HEAD"]));
    git(&s, &["update-ref", "refs/keep/old", "HEAD"]);
    git(&s, &["reset", "-q", "--hard", &shas["c1"]]);
    write(&s, "src/lib.rs", "a\nB2\nc\nd\ne\nf\ng\nh\ni\nj\n");
    git(&s, &["commit", "-q", "-am", "feat v2"]);
    shas.insert("c2", git(&s, &["rev-parse", "HEAD"]));
    git(&s, &["update-ref", "refs/pull/2/head", "HEAD"]);
    // PR #3: merged with a merge commit.
    git(&s, &["checkout", "-q", "-b", "fix", &shas["c1"]]);
    write(&s, "README.md", "# api\n\nfixed\n");
    git(&s, &["commit", "-q", "-am", "fix readme"]);
    shas.insert("c3", git(&s, &["rev-parse", "HEAD"]));
    git(&s, &["update-ref", "refs/pull/3/head", "HEAD"]);
    git(&s, &["checkout", "-q", "main"]);
    git(
        &s,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #3",
            "fix",
        ],
    );
    shas.insert("m1", git(&s, &["rev-parse", "HEAD"]));
    // PR #5: closed, branch deleted.
    git(&s, &["checkout", "-q", "-b", "tmp", &shas["c1"]]);
    write(&s, "docs/x.md", "x\n");
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-q", "-m", "docs"]);
    shas.insert("c5", git(&s, &["rev-parse", "HEAD"]));
    git(&s, &["update-ref", "refs/pull/5/head", "HEAD"]);
    git(&s, &["checkout", "-q", "main"]);
    git(&s, &["branch", "-q", "-D", "tmp"]);
    git(&s, &["tag", "-a", "v1.0", "-m", "release 1.0"]);
    Source { dir, shas }
}

// ----- the fake API --------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct Data {
    pub repo: Value,
    pub labels: Vec<Value>,
    pub milestones: Vec<Value>,
    pub issues: Vec<Value>,
    pub pulls: Vec<Value>,
    pub review_comments: Vec<Value>,
    pub issue_comments: Vec<Value>,
    pub releases: Vec<Value>,
    pub protected: Vec<String>,
    pub protections: BTreeMap<String, Value>,
    pub hooks: Option<Vec<Value>>,
    pub secrets: Vec<Value>,
    pub keys: Vec<Value>,
    pub force_pushes: BTreeMap<u64, Vec<Value>>,
    pub reviews: BTreeMap<u64, Vec<Value>>,
    pub reactions: BTreeMap<String, Vec<Value>>,
    pub assets: BTreeMap<u64, Vec<u8>>,
    pub users: BTreeMap<String, Value>,
    pub migration_archive: Vec<u8>,
}

#[derive(Default)]
pub struct Faults {
    /// Requests after this many fail with 500.
    pub fail_after: Option<u64>,
    /// The first request answers 403 with an exhausted primary limit resetting in N s.
    pub primary_once: Option<u64>,
    /// The first request to a path containing this answers 429 with `Retry-After: 7`.
    pub secondary_once: Option<String>,
}

pub struct Fake {
    pub data: Mutex<Data>,
    pub faults: Mutex<Faults>,
    pub hits: Mutex<BTreeMap<String, u64>>,
    pub total: AtomicU64,
    pub archived: Mutex<bool>,
    pub now: u64,
    /// Public key of the GitHub App; when set, API calls need the installation token.
    pub app_public_key: Mutex<Option<Vec<u8>>>,
    pub base: Mutex<String>,
}

pub const APP_TOKEN: &str = "ghs_fake_installation_token";

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, x| {
        (h ^ u64::from(*x)).wrapping_mul(0x100000001b3)
    })
}

fn query(uri: &Uri) -> BTreeMap<String, String> {
    uri.query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl Fake {
    fn hit(&self, key: &str) {
        *self
            .hits
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default() += 1;
    }

    pub fn hits(&self, key: &str) -> u64 {
        self.hits.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    fn page(&self, uri: &Uri, headers: &HeaderMap, items: Vec<Value>) -> Response {
        let q = query(uri);
        let per: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(30);
        let page: usize = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
        let items: Vec<Value> = match q.get("since") {
            Some(since) => items
                .into_iter()
                .filter(|i| i["updated_at"].as_str().unwrap_or("") >= since.as_str())
                .collect(),
            None => items,
        };
        let start = (page - 1) * per;
        let chunk: Vec<Value> = items.iter().skip(start).take(per).cloned().collect();
        let body = serde_json::to_vec(&chunk).unwrap();
        let etag = format!("\"{:x}\"", fnv(&body));
        let mut r =
            if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
                StatusCode::NOT_MODIFIED.into_response()
            } else {
                (StatusCode::OK, body).into_response()
            };
        let h = r.headers_mut();
        h.insert("etag", etag.parse().unwrap());
        h.insert("x-ratelimit-limit", "5000".parse().unwrap());
        h.insert("x-ratelimit-remaining", "4000".parse().unwrap());
        h.insert(
            "x-ratelimit-reset",
            (self.now + 3600).to_string().parse().unwrap(),
        );
        if start + per < items.len() {
            let mut q2 = q.clone();
            q2.insert("page".into(), (page + 1).to_string());
            let qs: Vec<String> = q2.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let next = format!(
                "{}{}?{}",
                self.base.lock().unwrap(),
                uri.path(),
                qs.join("&")
            );
            h.insert("link", format!("<{next}>; rel=\"next\"").parse().unwrap());
        }
        r
    }

    fn json(&self, v: Value) -> Response {
        (StatusCode::OK, v.to_string()).into_response()
    }

    fn graphql(&self, body: &[u8]) -> Response {
        let v: Value = serde_json::from_slice(body).unwrap_or_default();
        let q = v["query"].as_str().unwrap_or("").to_string();
        let d = self.data.lock().unwrap();
        let mut repo = serde_json::Map::new();
        for chunk in q.split("pullRequest(number: ").skip(1) {
            let n: u64 = chunk
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap();
            repo.insert(
                format!("p{n}"),
                json!({
                    "number": n,
                    "timelineItems": { "pageInfo": { "hasNextPage": false }, "nodes": d.force_pushes.get(&n).cloned().unwrap_or_default() },
                    "reviews": { "pageInfo": { "hasNextPage": false }, "nodes": d.reviews.get(&n).cloned().unwrap_or_default() },
                }),
            );
        }
        self.json(json!({ "data": { "repository": repo } }))
    }

    fn handle(&self, method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Response {
        let path = uri.path().to_string();
        self.hit(&path);
        let n = self.total.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut f = self.faults.lock().unwrap();
            if f.fail_after.is_some_and(|max| n > max) {
                return (StatusCode::INTERNAL_SERVER_ERROR, "crash").into_response();
            }
            if let Some(reset) = f.primary_once.take() {
                let mut r = (
                    StatusCode::FORBIDDEN,
                    r#"{"message":"API rate limit exceeded"}"#,
                )
                    .into_response();
                let h = r.headers_mut();
                h.insert("x-ratelimit-remaining", "0".parse().unwrap());
                h.insert(
                    "x-ratelimit-reset",
                    (self.now + reset).to_string().parse().unwrap(),
                );
                return r;
            }
            if f.secondary_once
                .as_ref()
                .is_some_and(|s| path.contains(s.as_str()))
            {
                f.secondary_once = None;
                let mut r = (
                    StatusCode::TOO_MANY_REQUESTS,
                    r#"{"message":"You have exceeded a secondary rate limit"}"#,
                )
                    .into_response();
                r.headers_mut().insert("retry-after", "7".parse().unwrap());
                return r;
            }
        }
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if let Some(pk) = self.app_public_key.lock().unwrap().clone() {
            if let Some(rest) = path.strip_prefix("/app/installations/") {
                let jwt = auth.strip_prefix("Bearer ").unwrap_or("");
                let ok = bee_github::auth::verify_jwt(jwt, &pk)
                    .is_some_and(|c| c["iss"] == json!("4242"));
                if !ok || !rest.ends_with("/access_tokens") || method != Method::POST {
                    return (StatusCode::UNAUTHORIZED, r#"{"message":"bad JWT"}"#).into_response();
                }
                return (
                    StatusCode::CREATED,
                    json!({ "token": APP_TOKEN, "expires_at": "2099-01-01T00:00:00Z" }).to_string(),
                )
                    .into_response();
            }
            let lfs = path.contains("/info/lfs/") || path.starts_with("/lfs/");
            if !lfs && auth != format!("token {APP_TOKEN}") {
                return (StatusCode::UNAUTHORIZED, r#"{"message":"Bad credentials"}"#)
                    .into_response();
            }
        }
        let d = self.data.lock().unwrap().clone();
        let base = format!("/repos/{FULL}");
        let rel = path.strip_prefix(&base);
        match (method.as_str(), rel) {
            ("GET", Some("")) => {
                let mut r = d.repo.clone();
                r["archived"] = json!(*self.archived.lock().unwrap());
                self.json(r)
            }
            ("PATCH", Some("")) => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                if v["archived"] == json!(true) {
                    *self.archived.lock().unwrap() = true;
                }
                self.json(d.repo.clone())
            }
            ("GET", Some("/labels")) => self.page(uri, headers, d.labels),
            ("GET", Some("/milestones")) => self.page(uri, headers, d.milestones),
            ("GET", Some("/issues")) => self.page(uri, headers, d.issues),
            ("GET", Some("/pulls")) => self.page(uri, headers, d.pulls),
            ("GET", Some("/pulls/comments")) => self.page(uri, headers, d.review_comments),
            ("GET", Some("/issues/comments")) => self.page(uri, headers, d.issue_comments),
            ("GET", Some("/releases")) => self.page(uri, headers, d.releases),
            ("GET", Some("/branches")) => self.page(
                uri,
                headers,
                d.protected.iter().map(|b| json!({ "name": b, "protected": true })).collect(),
            ),
            ("GET", Some("/hooks")) => match d.hooks {
                Some(h) => self.page(uri, headers, h),
                None => (StatusCode::FORBIDDEN, r#"{"message":"Must have admin rights to Repository."}"#).into_response(),
            },
            ("GET", Some("/keys")) => self.page(uri, headers, d.keys),
            ("GET", Some("/actions/secrets")) => self.json(json!({ "total_count": d.secrets.len(), "secrets": d.secrets })),
            ("GET", Some(r)) if r.starts_with("/branches/") && r.ends_with("/protection") => {
                let b = r.trim_start_matches("/branches/").trim_end_matches("/protection").replace("%2F", "/").replace("%2A", "*").replace("%5B", "[").replace("%5D", "]");
                match d.protections.get(&b) {
                    Some(p) => self.json(p.clone()),
                    None => (StatusCode::NOT_FOUND, r#"{"message":"Branch not protected"}"#).into_response(),
                }
            }
            ("GET", Some(r)) if r.starts_with("/pulls/") && r[7..].chars().all(|c| c.is_ascii_digit()) => {
                let n: u64 = r[7..].parse().unwrap();
                match d.pulls.iter().find(|p| p["number"] == json!(n)) {
                    Some(p) => self.json(p.clone()),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
            ("GET", Some(r)) if r.ends_with("/reactions") => {
                let key = if let Some(x) = r.strip_prefix("/issues/comments/") {
                    format!("issue_comment:{}", x.trim_end_matches("/reactions"))
                } else if let Some(x) = r.strip_prefix("/pulls/comments/") {
                    format!("review_comment:{}", x.trim_end_matches("/reactions"))
                } else {
                    format!("issue:{}", r.trim_start_matches("/issues/").trim_end_matches("/reactions"))
                };
                self.page(uri, headers, d.reactions.get(&key).cloned().unwrap_or_default())
            }
            ("GET", Some(r)) if r.starts_with("/releases/assets/") => {
                let id: u64 = r.trim_start_matches("/releases/assets/").parse().unwrap_or(0);
                match d.assets.get(&id) {
                    Some(b) => (StatusCode::OK, b.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }
            _ => match (method.as_str(), path.as_str()) {
                ("POST", "/graphql") => self.graphql(body),
                ("GET", "/rate_limit") => self.json(json!({ "resources": { "core": { "limit": 5000, "remaining": 4000, "reset": self.now + 3600 } } })),
                ("GET", "/search/issues") => {
                    let q = query(uri);
                    let is_pr = q.get("q").is_some_and(|v| v.contains("is:pr"));
                    let total = if is_pr { d.pulls.len() } else { d.issues.iter().filter(|i| i.get("pull_request").is_none()).count() };
                    self.json(json!({ "total_count": total, "items": [] }))
                }
                ("POST", p) if p == format!("/{FULL}.git/info/lfs/objects/batch") => {
                    let v: Value = serde_json::from_slice(body).unwrap_or_default();
                    let objs: Vec<Value> = v["objects"].as_array().cloned().unwrap_or_default().into_iter().map(|o| {
                        let oid = o["oid"].as_str().unwrap_or("").to_string();
                        if oid == lfs_oid() {
                            json!({ "oid": oid, "size": o["size"], "actions": { "download": { "href": format!("{}/lfs/{oid}", self.base.lock().unwrap()), "header": { "X-Lfs-Token": "t" } } } })
                        } else {
                            json!({ "oid": oid, "size": o["size"], "error": { "code": 404, "message": "Object does not exist" } })
                        }
                    }).collect();
                    self.json(json!({ "transfer": "basic", "objects": objs }))
                }
                ("GET", p) if p.starts_with("/lfs/") => {
                    if headers.get("x-lfs-token").is_none() {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    (StatusCode::OK, LFS_DATA.to_vec()).into_response()
                }
                ("GET", p) if p.starts_with("/users/") => {
                    match d.users.get(p.trim_start_matches("/users/")) {
                        Some(u) => self.json(u.clone()),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
                ("POST", p) if p == format!("/orgs/{OWNER}/migrations") => {
                    (StatusCode::CREATED, json!({ "id": 77, "state": "pending" }).to_string()).into_response()
                }
                ("GET", p) if p == format!("/orgs/{OWNER}/migrations/77") => {
                    let state = if self.hits(p) >= 2 { "exported" } else { "exporting" };
                    self.json(json!({ "id": 77, "state": state }))
                }
                ("GET", p) if p == format!("/orgs/{OWNER}/migrations/77/archive") => {
                    let mut r = StatusCode::FOUND.into_response();
                    r.headers_mut().insert("location", "/storage/archive.tar.gz".parse().unwrap());
                    r
                }
                ("GET", "/storage/archive.tar.gz") => (StatusCode::OK, d.migration_archive.clone()).into_response(),
                _ => (StatusCode::NOT_FOUND, format!(r#"{{"message":"no route {method} {path}"}}"#)).into_response(),
            },
        }
    }
}

pub fn user(id: u64, login: &str) -> Value {
    json!({ "id": id, "login": login, "type": if login.ends_with("[bot]") { "Bot" } else { "User" } })
}

pub const ALICE: u64 = 1001;
pub const BOB: u64 = 1002;
pub const CAROL: u64 = 1003;
pub const DAVE: u64 = 1004;
pub const BOT: u64 = 1005;

fn gql_user(id: u64, login: &str) -> Value {
    json!({ "__typename": if login.ends_with("[bot]") { "Bot" } else { "User" }, "login": login, "databaseId": id })
}

/// The metadata of the fixture project. `messy` adds things that must be reported as
/// drops: an unreachable force-push commit, an untranslatable branch pattern, unreadable
/// webhooks.
pub fn data(src: &Source, base: &str, messy: bool) -> Data {
    let alice = user(ALICE, "alice");
    let bob = user(BOB, "bob");
    let carol = user(CAROL, "carol");
    let dave = user(DAVE, "dave");
    let bot = user(BOT, "dependabot[bot]");
    let html = format!("{base}/{FULL}");
    let label_bug =
        json!({ "name": "bug", "color": "d73a4a", "description": "Something is broken" });
    let label_enh = json!({ "name": "enhancement", "color": "a2eeef", "description": null });
    let ms1 = json!({ "number": 1, "title": "v1", "description": "first", "state": "open", "due_on": "2026-12-01T00:00:00Z" });
    let pr = |n: u64,
              title: &str,
              state: &str,
              head_ref: &str,
              head: &str,
              base_sha: &str,
              merged: bool,
              author: &Value,
              upd: &str| {
        json!({
            "number": n, "title": title, "body": format!("PR {n}, see {html}/issues/1"), "state": state,
            "draft": false, "locked": false,
            "merged_at": if merged { json!("2026-01-03T00:00:00Z") } else { Value::Null },
            "merge_commit_sha": if merged { json!(src.sha("m1")) } else { Value::Null },
            "user": author, "labels": if n == 2 { vec![label_enh.clone()] } else { vec![] },
            "assignees": if n == 2 { vec![bob.clone()] } else { vec![] },
            "requested_reviewers": if n == 2 { vec![carol.clone()] } else { vec![] },
            "milestone": if n == 2 { ms1.clone() } else { Value::Null },
            "head": { "ref": head_ref, "sha": head, "repo": { "full_name": FULL } },
            "base": { "ref": "main", "sha": base_sha, "repo": { "full_name": FULL } },
            "created_at": format!("2026-01-0{n}T00:00:00Z"), "updated_at": upd,
            "closed_at": if state == "closed" { json!("2026-01-06T00:00:00Z") } else { Value::Null },
        })
    };
    let pulls = vec![
        pr(
            2,
            "Add feature",
            "open",
            "feat",
            &src.sha("c2"),
            &src.sha("c1"),
            false,
            &alice,
            "2026-01-05T00:00:00Z",
        ),
        pr(
            3,
            "Fix readme",
            "closed",
            "fix",
            &src.sha("c3"),
            &src.sha("c1"),
            true,
            &bot,
            "2026-01-03T00:00:00Z",
        ),
        pr(
            5,
            "Docs",
            "closed",
            "tmp",
            &src.sha("c5"),
            &src.sha("c1"),
            false,
            &dave,
            "2026-01-06T00:00:00Z",
        ),
    ];
    let issue = |n: u64, title: &str, state: &str, author: &Value, reactions: u64| {
        json!({
            "number": n, "title": title, "body": format!("issue {n} body"), "state": state,
            "state_reason": if state == "closed" { json!("not_planned") } else { Value::Null },
            "user": author, "labels": [label_bug.clone()], "assignees": [], "milestone": if n == 1 { ms1.clone() } else { Value::Null },
            "locked": false, "created_at": format!("2026-01-0{n}T00:00:00Z"), "updated_at": format!("2026-01-0{n}T01:00:00Z"),
            "closed_at": if state == "closed" { json!("2026-01-05T00:00:00Z") } else { Value::Null },
            "reactions": { "total_count": reactions },
        })
    };
    let mut issues = vec![
        issue(1, "Crash on start", "open", &carol, 1),
        issue(4, "Wontfix", "closed", &bob, 0),
    ];
    for p in &pulls {
        let n = p["number"].as_u64().unwrap();
        issues.push(json!({
            "number": n, "title": p["title"], "state": p["state"], "user": p["user"], "labels": [], "assignees": [],
            "created_at": p["created_at"], "updated_at": p["updated_at"], "pull_request": { "url": format!("{base}/repos/{FULL}/pulls/{n}") },
            "reactions": { "total_count": if n == 2 { 1 } else { 0 } },
        }));
    }
    issues.sort_by_key(|i| i["number"].as_u64());
    let rc = |id: u64, line: u32, reply: Option<u64>, author: &Value| {
        json!({
            "id": id, "pull_request_review_id": 900, "user": author, "body": format!("review comment {id}"), "path": "src/lib.rs",
            "commit_id": src.sha("c2"), "original_commit_id": src.sha("c2"), "line": line, "original_line": line,
            "side": "RIGHT", "subject_type": "line", "diff_hunk": "@@ -1,3 +1,3 @@\n a\n-b\n+B2", "in_reply_to_id": reply,
            "created_at": format!("2026-01-04T00:00:{:02}Z", id - 100), "updated_at": "2026-01-04T00:00:00Z",
            "pull_request_url": format!("{base}/repos/{FULL}/pulls/2"), "reactions": { "total_count": 0 },
        })
    };
    let review_comments = vec![
        rc(100, 2, None, &bob),
        rc(101, 2, Some(100), &alice),
        rc(102, 999, None, &carol),
    ];
    let issue_comments = vec![
        json!({ "id": 200, "user": bob, "body": format!("same as {html}/pull/2"), "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z", "issue_url": format!("{base}/repos/{FULL}/issues/1"), "reactions": { "total_count": 1 } }),
        json!({ "id": 201, "user": carol, "body": "looks good", "created_at": "2026-01-04T00:01:00Z", "updated_at": "2026-01-04T00:01:00Z", "issue_url": format!("{base}/repos/{FULL}/issues/2"), "reactions": { "total_count": 0 } }),
    ];
    let mut force_pushes = BTreeMap::new();
    force_pushes.insert(2, vec![json!({ "createdAt": "2026-01-03T12:00:00Z", "actor": { "login": "alice" }, "beforeCommit": { "oid": src.sha("c2a") }, "afterCommit": { "oid": src.sha("c2") } })]);
    if messy {
        force_pushes.insert(5, vec![json!({ "createdAt": "2026-01-05T12:00:00Z", "actor": { "login": "dave" }, "beforeCommit": { "oid": UNREACHABLE }, "afterCommit": { "oid": src.sha("c5") } })]);
    }
    let review = |id: u64, state: &str, who: (u64, &str), commit: &str, at: &str| json!({ "databaseId": id, "body": format!("review {id}"), "state": state, "submittedAt": at, "commit": { "oid": commit }, "author": gql_user(who.0, who.1) });
    let mut reviews = BTreeMap::new();
    reviews.insert(
        2,
        vec![
            review(
                900,
                "APPROVED",
                (BOB, "bob"),
                &src.sha("c2"),
                "2026-01-04T00:00:30Z",
            ),
            review(
                901,
                "COMMENTED",
                (CAROL, "carol"),
                &src.sha("c2"),
                "2026-01-04T00:02:00Z",
            ),
            review(
                902,
                "PENDING",
                (DAVE, "dave"),
                &src.sha("c2"),
                "2026-01-04T00:03:00Z",
            ),
        ],
    );
    reviews.insert(
        3,
        vec![review(
            910,
            "CHANGES_REQUESTED",
            (CAROL, "carol"),
            &src.sha("c3"),
            "2026-01-03T00:00:00Z",
        )],
    );
    let mut reactions = BTreeMap::new();
    reactions.insert(
        "issue:1".to_string(),
        vec![json!({ "id": 3001, "user": bob, "content": "heart" })],
    );
    reactions.insert(
        "issue:2".to_string(),
        vec![json!({ "id": 3002, "user": carol, "content": "rocket" })],
    );
    reactions.insert(
        "issue_comment:200".to_string(),
        vec![json!({ "id": 3003, "user": carol, "content": "+1" })],
    );
    let releases = vec![json!({
        "id": 50, "tag_name": "v1.0", "name": "1.0", "body": "first release", "draft": false, "prerelease": false,
        "target_commitish": "main", "author": alice, "created_at": "2026-01-07T00:00:00Z", "published_at": "2026-01-07T00:00:00Z",
        "assets": [{ "id": 60, "name": "notes.txt", "label": null, "content_type": "text/plain", "size": ASSET_DATA.len(),
                     "browser_download_url": format!("{base}/download/notes.txt"), "url": format!("{base}/repos/{FULL}/releases/assets/60") }],
    })];
    let mut protections = BTreeMap::new();
    protections.insert("main".to_string(), json!({
        "required_status_checks": { "strict": true, "contexts": ["ci/test"], "checks": [] },
        "required_pull_request_reviews": { "required_approving_review_count": 1, "require_code_owner_reviews": true, "dismiss_stale_reviews": true, "require_last_push_approval": false },
        "enforce_admins": { "enabled": true },
        "required_linear_history": { "enabled": false },
        "allow_force_pushes": { "enabled": false },
        "allow_deletions": { "enabled": false },
    }));
    let mut protected = vec!["main".to_string()];
    if messy {
        protections.insert(
            "feat-[0-9]*".to_string(),
            json!({ "enforce_admins": { "enabled": true } }),
        );
        protected.push("feat-[0-9]*".into());
    }
    let mut assets = BTreeMap::new();
    assets.insert(60, ASSET_DATA.to_vec());
    let mut users = BTreeMap::new();
    for (id, l) in [
        (ALICE, "alice"),
        (BOB, "bob"),
        (CAROL, "carol"),
        (DAVE, "dave"),
        (BOT, "dependabot[bot]"),
    ] {
        users.insert(l.to_string(), user(id, l));
    }
    Data {
        repo: json!({
            "id": 777, "node_id": "R_kgDO", "name": NAME, "full_name": FULL, "default_branch": "main", "private": false,
            "has_wiki": false, "html_url": html, "clone_url": src.path().display().to_string(), "description": "fixture",
        }),
        labels: vec![label_bug, label_enh],
        milestones: vec![ms1],
        issues,
        pulls,
        review_comments,
        issue_comments,
        releases,
        protected,
        protections,
        hooks: if messy {
            None
        } else {
            Some(vec![
                json!({ "id": 70, "active": true, "events": ["push", "pull_request"], "config": { "url": "https://ci.example/hook", "content_type": "json" } }),
            ])
        },
        secrets: vec![json!({ "name": "NPM_TOKEN" })],
        keys: vec![json!({ "title": "deploy" })],
        force_pushes,
        reviews,
        reactions,
        assets,
        users,
        migration_archive: Vec::new(),
    }
}

/// Starts the fake on a local port.
pub async fn fake(data: Data, now: u64) -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake {
        data: Mutex::new(data),
        faults: Mutex::new(Faults::default()),
        hits: Mutex::new(BTreeMap::new()),
        total: AtomicU64::new(0),
        archived: Mutex::new(false),
        now,
        app_public_key: Mutex::new(None),
        base: Mutex::new(String::new()),
    });
    async fn h(
        State(f): State<Arc<Fake>>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        f.handle(&method, &uri, &headers, &body)
    }
    let app = axum::Router::new().fallback(h).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");
    *fake.base.lock().unwrap() = base.clone();
    (fake, base)
}

// ----- the bee side -----------------------------------------------------------------------

pub const NOW: u64 = 1_800_000_000;

pub struct Stores {
    pub meta: Arc<MemoryMetaStore>,
    pub blobs: Arc<MemoryBlobStore>,
    pub writer: Writer,
    pub org: OrgId,
    pub repo: RepoId,
}

pub async fn stores_with(org: OrgId, repo: RepoId) -> Stores {
    let clock = Arc::new(StoreClock::new(1_000));
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(clock));
    meta.create_org(OrgRow {
        id: org,
        name: OWNER.into(),
        created_at_ms: 1,
    })
    .await
    .unwrap();
    meta.create_repo(RepoRow {
        id: repo,
        org,
        name: NAME.into(),
        created_at_ms: 1,
        draft_retention_days: DEFAULT_DRAFT_RETENTION_DAYS,
    })
    .await
    .unwrap();
    let writer = Writer::new(meta.clone(), blobs.clone());
    Stores {
        meta,
        blobs,
        writer,
        org,
        repo,
    }
}

pub async fn stores() -> Stores {
    stores_with(OrgId(Ulid::from_u128(1)), RepoId(Ulid::from_u128(2))).await
}

impl Stores {
    pub fn target(&self) -> Arc<StoreTarget> {
        Arc::new(StoreTarget {
            main: ImportTarget {
                writer: self.writer.clone(),
                org: self.org,
                repo: self.repo,
            },
            wiki: None,
            creator: "svc:github-import".into(),
            derived: None,
        })
    }

    pub fn meta_dyn(&self) -> Arc<dyn MetaStore> {
        self.meta.clone()
    }
}

pub struct World {
    pub src: Source,
    pub fake: Arc<Fake>,
    pub base: String,
    pub stores: Stores,
    pub sink: Arc<MemorySink>,
    pub state: tempfile::TempDir,
    pub clock: Arc<ManualClock>,
}

impl World {
    pub async fn new(messy: bool) -> World {
        let src = source(messy);
        let (fake, base) = fake(Data::default(), NOW).await;
        *fake.data.lock().unwrap() = data(&src, &base, messy);
        World {
            src,
            fake,
            base,
            stores: stores().await,
            sink: Arc::new(MemorySink::new()),
            state: tempfile::tempdir().unwrap(),
            clock: Arc::new(ManualClock::new(NOW)),
        }
    }

    pub fn client_with(&self, cfg: ClientConfig, cred: bee_github::auth::Credential) -> Client {
        let clock: Arc<dyn Clock> = self.clock.clone();
        Client::new(cfg, cred, clock)
    }

    pub fn config(&self) -> ClientConfig {
        let mut c = ClientConfig::at(&self.base);
        c.per_page = 100;
        c
    }

    pub fn options(&self) -> Options {
        let mut o = Options::new(FULL);
        o.hub_url = "https://hub.test".into();
        o.at_ms = 5_000;
        o
    }

    pub fn importer_with(&self, cfg: ClientConfig, opts: Options) -> Importer {
        let client = self.client_with(cfg, bee_github::auth::Credential::Token("ghp_test".into()));
        Importer::new(
            client,
            StateDir::open(self.state.path()).unwrap(),
            self.stores.target(),
            self.sink.clone(),
            opts,
        )
    }

    pub fn importer(&self) -> Importer {
        self.importer_with(self.config(), self.options())
    }
}

/// Validates a report against `schemas/v1/import-report.schema.json`.
pub fn validate_report(v: &Value) {
    let schema_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1/import-report.schema.json");
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(v)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "report does not match the schema: {errors:#?}"
    );
}
