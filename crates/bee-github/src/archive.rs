//! The org migration archive (`POST /orgs/{org}/migrations`) as a metadata source: bulk JSON
//! that costs no REST quota (SPEC §17.1 step 4), plus the repository's bare git directory.
//!
//! An archive is a gzipped tar of JSON arrays split into numbered files
//! (`pull_requests_000001.json`, …) and the repositories under `repositories/<owner>/`.
//! Records refer to each other by URL rather than id (`"user": "https://github.com/alice"`,
//! `"url": "https://github.com/o/r/issues/3#issuecomment-55"`); this reader resolves those
//! URLs to numbers, ids and logins. Accounts carry no numeric id in an archive, so
//! [`ArchiveData::logins`] lists them for the caller to resolve through the API
//! (`GET /users/{login}`); offline, [`synthetic_user_id`] keys them by login instead and the
//! report says so.
//!
//! The tar reader understands ustar, GNU long names and pax `path` records, and extracts
//! only below the destination (no absolute paths, no `..`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::client::Client;
use crate::error::{Result, invalid, io, unavailable};
use crate::model::{
    Issue, IssueComment, Label, Metadata, Milestone, Protection, PullRequest, Release, Review,
    ReviewComment, User,
};

/// One file of a tar archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarEntry {
    /// Path inside the archive.
    pub path: String,
    /// `true` for directories.
    pub dir: bool,
    /// File bytes.
    pub data: Vec<u8>,
}

fn octal(field: &[u8]) -> Option<u64> {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok()
}

fn cstr(field: &[u8]) -> String {
    String::from_utf8_lossy(&field[..field.iter().position(|b| *b == 0).unwrap_or(field.len())])
        .into_owned()
}

/// Reads every entry of an uncompressed tar stream.
pub fn read_tar(bytes: &[u8]) -> Result<Vec<TarEntry>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    while pos + 512 <= bytes.len() {
        let h = &bytes[pos..pos + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let size = octal(&h[124..136])
            .ok_or_else(|| invalid("invalid_archive", format!("bad tar size at offset {pos}")))?
            as usize;
        let kind = h[156];
        let mut name = cstr(&h[0..100]);
        if &h[257..262] == b"ustar" {
            let prefix = cstr(&h[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let start = pos + 512;
        let end = start
            .checked_add(size)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| invalid("invalid_archive", "tar entry runs past the end"))?;
        let data = &bytes[start..end];
        pos = start + size.div_ceil(512) * 512;
        match kind {
            b'L' => {
                long_name = Some(cstr(data));
                continue;
            }
            b'x' => {
                pax_path = pax_value(data, "path");
                continue;
            }
            b'g' => continue,
            _ => {}
        }
        if let Some(n) = pax_path.take().or(long_name.take()) {
            name = n;
        }
        match kind {
            b'0' | 0 | b'7' => out.push(TarEntry {
                path: name,
                dir: false,
                data: data.to_vec(),
            }),
            b'5' => out.push(TarEntry {
                path: name,
                dir: true,
                data: Vec::new(),
            }),
            // Links, devices and fifos are never part of an archive we read.
            _ => {}
        }
    }
    Ok(out)
}

fn pax_value(data: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    for rec in text.lines() {
        let (_, kv) = rec.split_once(' ')?;
        if let Some(v) = kv.strip_prefix(&format!("{key}=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// Builds a ustar archive (used by tests and by `bee-github` to package fixtures).
pub fn write_tar(entries: &[TarEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        let mut h = [0u8; 512];
        let name = e.path.as_bytes();
        let n = name.len().min(100);
        h[..n].copy_from_slice(&name[..n]);
        let mode: &[u8] = if e.dir { b"0000755\0" } else { b"0000644\0" };
        h[100..108].copy_from_slice(mode);
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", if e.dir { 0 } else { e.data.len() });
        h[124..136].copy_from_slice(size.as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = if e.dir { b'5' } else { b'0' };
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(chk.as_bytes());
        out.extend_from_slice(&h);
        if !e.dir {
            out.extend_from_slice(&e.data);
            let pad = e.data.len().div_ceil(512) * 512 - e.data.len();
            out.extend(std::iter::repeat_n(0u8, pad));
        }
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

/// A safe relative path inside `dest`, or `None`.
fn safe_join(dest: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let mut out = dest.to_path_buf();
    for c in p.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// What an archive holds.
#[derive(Clone, Debug, Default)]
pub struct ArchiveData {
    /// Metadata of the repository.
    pub meta: Metadata,
    /// Logins seen (to resolve to numeric ids).
    pub logins: BTreeSet<String>,
    /// The extracted bare repository, when the archive has git data.
    pub git_dir: Option<PathBuf>,
    /// The extracted wiki, when present.
    pub wiki_dir: Option<PathBuf>,
    /// Records the reader could not use: (file, reason).
    pub skipped: Vec<(String, String)>,
}

/// A deterministic stand-in id for an account known only by login (offline archive
/// imports). The top bit is set so it can never collide with a real GitHub id.
pub fn synthetic_user_id(login: &str) -> u64 {
    let h = blake3::hash(format!("github-login\0{}", login.to_ascii_lowercase()).as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&h.as_bytes()[..8]);
    u64::from_be_bytes(b) | (1 << 63)
}

fn login_of(url: &Value) -> Option<String> {
    let s = url.as_str()?;
    let l = s.trim_end_matches('/').rsplit('/').next()?;
    (!l.is_empty()).then(|| l.to_string())
}

fn number_in(url: &str, marker: &str) -> Option<u64> {
    let i = url.find(marker)? + marker.len();
    let digits: String = url[i..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn fragment_id(url: &str, marker: &str) -> Option<u64> {
    number_in(url, marker)
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn opt(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Opens a `.tar.gz` (or plain `.tar`) archive, reads the metadata of `full_name`, and
/// extracts its git directories under `extract_to`.
pub fn open(archive: &Path, full_name: &str, extract_to: &Path) -> Result<ArchiveData> {
    let raw = std::fs::read(archive).map_err(|e| io("reading the migration archive", e))?;
    let bytes = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut v = Vec::new();
        d.read_to_end(&mut v)
            .map_err(|e| invalid("invalid_archive", format!("bad gzip data: {e}")))?;
        v
    } else {
        raw
    };
    read_entries(&read_tar(&bytes)?, full_name, extract_to)
}

/// Reads already-unpacked tar entries (see [`open`]).
pub fn read_entries(
    entries: &[TarEntry],
    full_name: &str,
    extract_to: &Path,
) -> Result<ArchiveData> {
    let mut data = ArchiveData::default();
    let mut users: BTreeMap<String, User> = BTreeMap::new();
    let mut files: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let repo_url_suffix = format!("/{full_name}");
    let git_prefix = format!("repositories/{full_name}.git/");
    let wiki_prefix = format!("repositories/{full_name}.wiki.git/");
    for e in entries {
        let path = e.path.trim_start_matches("./");
        for (prefix, slot, sub) in [
            (&git_prefix, 0, "archive-repo.git"),
            (&wiki_prefix, 1, "archive-wiki.git"),
        ] {
            if let Some(rel) = path.strip_prefix(prefix.as_str()) {
                let root = extract_to.join(sub);
                let target = safe_join(&root, rel).ok_or_else(|| {
                    invalid("invalid_archive", format!("unsafe path in archive: {path}"))
                })?;
                if e.dir {
                    std::fs::create_dir_all(&target).map_err(|er| io("extracting", er))?;
                } else {
                    if let Some(p) = target.parent() {
                        std::fs::create_dir_all(p).map_err(|er| io("extracting", er))?;
                    }
                    std::fs::write(&target, &e.data).map_err(|er| io("extracting", er))?;
                }
                if slot == 0 {
                    data.git_dir = Some(root);
                } else {
                    data.wiki_dir = Some(root);
                }
            }
        }
        if e.dir || !path.ends_with(".json") || path.contains('/') {
            continue;
        }
        let kind = path
            .trim_end_matches(".json")
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('_')
            .to_string();
        match serde_json::from_slice::<Value>(&e.data) {
            Ok(Value::Array(items)) => files.entry(kind).or_default().extend(items),
            Ok(_) => data
                .skipped
                .push((path.to_string(), "not a JSON array".into())),
            Err(er) => data
                .skipped
                .push((path.to_string(), format!("bad JSON: {er}"))),
        }
    }
    let in_repo = |v: &Value| {
        let r = v["repository"].as_str().or(v["url"].as_str()).unwrap_or("");
        r.contains(&repo_url_suffix)
    };
    for u in files.get("users").into_iter().flatten() {
        if let Some(l) = u["login"].as_str() {
            users.insert(
                l.to_string(),
                User {
                    id: 0,
                    login: l.to_string(),
                    kind: if u["type"].as_str() == Some("bot") {
                        "Bot".into()
                    } else {
                        "User".into()
                    },
                    name: opt(&u["name"]),
                },
            );
        }
    }
    let mut user = |v: &Value| -> Option<User> {
        let login = login_of(v)?;
        data.logins.insert(login.clone());
        Some(users.get(&login).cloned().unwrap_or(User {
            id: 0,
            login: login.clone(),
            kind: if login.ends_with("[bot]") {
                "Bot".into()
            } else {
                "User".into()
            },
            name: None,
        }))
    };
    let meta = &mut data.meta;
    for r in files.get("repositories").into_iter().flatten() {
        if !r["url"].as_str().unwrap_or("").ends_with(&repo_url_suffix) {
            continue;
        }
        for l in r["labels"].as_array().into_iter().flatten() {
            if let Some(name) = l["name"].as_str() {
                meta.labels.insert(
                    name.to_string(),
                    Label {
                        name: name.to_string(),
                        color: s(&l["color"]),
                        description: opt(&l["description"]),
                    },
                );
            }
        }
    }
    let label_names = |v: &Value| -> Vec<Label> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let name = l
                    .as_str()
                    .map(|u| u.rsplit('/').next().unwrap_or("").replace("%20", " "))
                    .or_else(|| l["name"].as_str().map(str::to_string))?;
                Some(Label {
                    name,
                    ..Label::default()
                })
            })
            .collect()
    };
    let mut milestone_by_url: BTreeMap<String, Milestone> = BTreeMap::new();
    for m in files
        .get("milestones")
        .into_iter()
        .flatten()
        .filter(|v| in_repo(v))
    {
        let url = s(&m["url"]);
        let Some(number) = number_in(&url, "/milestone/") else {
            continue;
        };
        let ms = Milestone {
            number,
            title: s(&m["title"]),
            description: opt(&m["description"]),
            state: s(&m["state"]),
            due_on: opt(&m["due_on"]),
            closed_at: opt(&m["closed_at"]),
        };
        milestone_by_url.insert(url, ms.clone());
        meta.milestones.insert(number, ms);
    }
    let milestone_of =
        |v: &Value| -> Option<Milestone> { milestone_by_url.get(v.as_str()?).cloned() };
    let assignees = |v: &Value, user: &mut dyn FnMut(&Value) -> Option<User>| -> Vec<User> {
        let mut out = Vec::new();
        if let Some(a) = v["assignees"].as_array() {
            out.extend(a.iter().filter_map(&mut *user));
        } else if let Some(u) = user(&v["assignee"]) {
            out.push(u);
        }
        out
    };
    for i in files
        .get("issues")
        .into_iter()
        .flatten()
        .filter(|v| in_repo(v))
    {
        let url = s(&i["url"]);
        let Some(number) = number_in(&url, "/issues/") else {
            continue;
        };
        let issue = Issue {
            number,
            title: s(&i["title"]),
            body: opt(&i["body"]),
            state: if i["closed_at"].is_string() {
                "closed".into()
            } else {
                "open".into()
            },
            state_reason: opt(&i["state_reason"]),
            user: user(&i["user"]),
            labels: label_names(&i["labels"]),
            assignees: assignees(i, &mut user),
            milestone: milestone_of(&i["milestone"]),
            locked: i["locked"].as_bool().unwrap_or(false),
            created_at: s(&i["created_at"]),
            updated_at: s(&i["updated_at"]),
            closed_at: opt(&i["closed_at"]),
            pull_request: None,
            reactions: None,
        };
        meta.issues.insert(number, issue);
    }
    for p in files
        .get("pull_requests")
        .into_iter()
        .flatten()
        .filter(|v| in_repo(v))
    {
        let url = s(&p["url"]);
        let Some(number) = number_in(&url, "/pull/") else {
            continue;
        };
        let end = |v: &Value| crate::model::PrEnd {
            branch: s(&v["ref"]),
            sha: s(&v["sha"]),
            repo: v["repo"].as_str().map(|r| crate::model::RepoName {
                full_name: r
                    .trim_end_matches('/')
                    .rsplitn(3, '/')
                    .take(2)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/"),
            }),
        };
        let pr = PullRequest {
            number,
            title: s(&p["title"]),
            body: opt(&p["body"]),
            state: if p["closed_at"].is_string() || p["merged_at"].is_string() {
                "closed".into()
            } else {
                "open".into()
            },
            draft: p["draft"].as_bool().unwrap_or(false),
            locked: p["locked"].as_bool().unwrap_or(false),
            merged_at: opt(&p["merged_at"]),
            merge_commit_sha: opt(&p["merge_commit_sha"]),
            user: user(&p["user"]),
            labels: label_names(&p["labels"]),
            assignees: assignees(p, &mut user),
            requested_reviewers: Vec::new(),
            milestone: milestone_of(&p["milestone"]),
            head: end(&p["head"]),
            base: end(&p["base"]),
            created_at: s(&p["created_at"]),
            updated_at: s(&p["updated_at"]),
            closed_at: opt(&p["closed_at"]),
        };
        meta.pulls.insert(number, pr);
    }
    let review_state = |v: &Value| -> String {
        match v {
            Value::String(s) => s.to_ascii_uppercase(),
            Value::Number(n) => match n.as_u64() {
                Some(1) => "PENDING",
                Some(30) => "CHANGES_REQUESTED",
                Some(40) => "APPROVED",
                Some(50) => "DISMISSED",
                _ => "COMMENTED",
            }
            .into(),
            _ => "COMMENTED".into(),
        }
    };
    for r in files
        .get("pull_request_reviews")
        .into_iter()
        .flatten()
        .filter(|v| {
            v["pull_request"]
                .as_str()
                .unwrap_or("")
                .contains(&repo_url_suffix)
        })
    {
        let pr_url = s(&r["pull_request"]);
        let (Some(n), Some(id)) = (
            number_in(&pr_url, "/pull/"),
            fragment_id(&s(&r["url"]), "pullrequestreview-"),
        ) else {
            continue;
        };
        meta.reviews.entry(n).or_default().insert(
            id,
            Review {
                id,
                user: user(&r["user"]),
                body: opt(&r["body"]),
                state: review_state(&r["state"]),
                commit_id: opt(&r["head_sha"]).or_else(|| opt(&r["commit_id"])),
                submitted_at: opt(&r["submitted_at"]).or_else(|| opt(&r["created_at"])),
            },
        );
    }
    for c in files
        .get("pull_request_review_comments")
        .into_iter()
        .flatten()
        .filter(|v| {
            v["pull_request"]
                .as_str()
                .unwrap_or("")
                .contains(&repo_url_suffix)
        })
    {
        let pr_url = s(&c["pull_request"]);
        let Some(id) = fragment_id(&s(&c["url"]), "discussion_r") else {
            continue;
        };
        let Some(n) = number_in(&pr_url, "/pull/") else {
            continue;
        };
        let rc = ReviewComment {
            id,
            pull_request_review_id: c["pull_request_review"]
                .as_str()
                .and_then(|u| fragment_id(u, "pullrequestreview-")),
            user: user(&c["user"]),
            body: s(&c["body"]),
            path: s(&c["path"]),
            commit_id: s(&c["commit_id"]),
            original_commit_id: opt(&c["original_commit_id"]).unwrap_or_else(|| s(&c["commit_id"])),
            line: c["line"].as_u64().map(|v| v as u32),
            original_line: c["original_line"]
                .as_u64()
                .or(c["line"].as_u64())
                .map(|v| v as u32),
            start_line: c["start_line"].as_u64().map(|v| v as u32),
            original_start_line: c["original_start_line"]
                .as_u64()
                .or(c["start_line"].as_u64())
                .map(|v| v as u32),
            side: opt(&c["side"]),
            subject_type: opt(&c["subject_type"]),
            diff_hunk: s(&c["diff_hunk"]),
            in_reply_to_id: c["in_reply_to"]
                .as_str()
                .and_then(|u| fragment_id(u, "discussion_r")),
            created_at: s(&c["created_at"]),
            updated_at: s(&c["updated_at"]),
            pull_request_url: format!("pulls/{n}"),
            reactions: None,
        };
        meta.review_comments.insert(id, rc);
    }
    for c in files.get("issue_comments").into_iter().flatten() {
        let parent = c["issue"]
            .as_str()
            .or(c["pull_request"].as_str())
            .unwrap_or("");
        if !parent.contains(&repo_url_suffix) {
            continue;
        }
        let Some(id) = fragment_id(&s(&c["url"]), "issuecomment-") else {
            continue;
        };
        let n = number_in(parent, "/issues/").or_else(|| number_in(parent, "/pull/"));
        let Some(n) = n else { continue };
        meta.issue_comments.insert(
            id,
            IssueComment {
                id,
                user: user(&c["user"]),
                body: s(&c["body"]),
                created_at: s(&c["created_at"]),
                updated_at: s(&c["updated_at"]),
                issue_url: format!("issues/{n}"),
                reactions: None,
            },
        );
    }
    for r in files
        .get("releases")
        .into_iter()
        .flatten()
        .filter(|v| in_repo(v))
    {
        let id = number_in(&s(&r["url"]), "/releases/").unwrap_or_else(|| {
            let h = blake3::hash(s(&r["tag_name"]).as_bytes());
            u64::from_be_bytes(h.as_bytes()[..8].try_into().unwrap_or([0; 8])) >> 1
        });
        let rel = Release {
            id,
            tag_name: s(&r["tag_name"]),
            name: opt(&r["name"]),
            body: opt(&r["body"]),
            draft: r["state"].as_str() == Some("draft"),
            prerelease: r["prerelease"].as_bool().unwrap_or(false),
            target_commitish: s(&r["target_commitish"]),
            author: user(&r["user"]),
            created_at: s(&r["created_at"]),
            published_at: opt(&r["published_at"]),
            assets: Vec::new(),
        };
        meta.releases.insert(id, rel);
    }
    for b in files
        .get("protected_branches")
        .into_iter()
        .flatten()
        .filter(|v| in_repo(v))
    {
        if let (Some(name), Ok(p)) = (
            b["name"].as_str(),
            serde_json::from_value::<Protection>(b.clone()),
        ) {
            meta.protected_branches.insert(name.to_string());
            meta.protections.insert(name.to_string(), p);
        }
    }
    Ok(data)
}

/// Gives every account in the metadata its numeric id: `ids` maps login → id (from the
/// API); logins without one get [`synthetic_user_id`]. Returns the logins that stayed
/// synthetic.
pub fn assign_user_ids(meta: &mut Metadata, ids: &BTreeMap<String, (u64, String)>) -> Vec<String> {
    let mut synthetic = BTreeSet::new();
    let mut fix = |u: &mut User| {
        if u.id != 0 {
            return;
        }
        match ids.get(&u.login) {
            Some((id, kind)) => {
                u.id = *id;
                if !kind.is_empty() {
                    u.kind = kind.clone();
                }
            }
            None => {
                u.id = synthetic_user_id(&u.login);
                synthetic.insert(u.login.clone());
            }
        }
    };
    let fix_opt = |u: &mut Option<User>, fix: &mut dyn FnMut(&mut User)| {
        if let Some(u) = u {
            fix(u);
        }
    };
    for i in meta.issues.values_mut() {
        fix_opt(&mut i.user, &mut fix);
        i.assignees.iter_mut().for_each(&mut fix);
    }
    for p in meta.pulls.values_mut() {
        fix_opt(&mut p.user, &mut fix);
        p.assignees.iter_mut().for_each(&mut fix);
        p.requested_reviewers.iter_mut().for_each(&mut fix);
    }
    for rs in meta.reviews.values_mut() {
        for r in rs.values_mut() {
            fix_opt(&mut r.user, &mut fix);
        }
    }
    for c in meta.review_comments.values_mut() {
        fix_opt(&mut c.user, &mut fix);
    }
    for c in meta.issue_comments.values_mut() {
        fix_opt(&mut c.user, &mut fix);
    }
    for r in meta.releases.values_mut() {
        fix_opt(&mut r.author, &mut fix);
    }
    meta.users.clear();
    meta.collect_users();
    synthetic.into_iter().collect()
}

/// Starts an org migration export of `repos` (without locking them) and returns its id.
pub async fn start_org_migration(client: &Client, org: &str, repos: &[String]) -> Result<u64> {
    let v = client
        .post_json(
            &format!("/orgs/{org}/migrations"),
            &serde_json::json!({ "repositories": repos, "lock_repositories": false }),
        )
        .await?;
    v["id"]
        .as_u64()
        .ok_or_else(|| crate::error::bad_json("migration", "no id"))
}

/// Polls a migration until it is exported (`Ok`) or failed (`UNAVAILABLE`), sleeping
/// `poll` between checks, at most `max_polls` times.
pub async fn wait_org_migration(
    client: &Client,
    org: &str,
    id: u64,
    poll: Duration,
    max_polls: u32,
) -> Result<()> {
    for _ in 0..max_polls {
        let v = client
            .get_json(&format!("/orgs/{org}/migrations/{id}"))
            .await?;
        match v["state"].as_str().unwrap_or("") {
            "exported" => return Ok(()),
            "failed" => return Err(unavailable(format!("GitHub migration {id} failed"))),
            _ => client.clock().sleep(poll).await,
        }
    }
    Err(unavailable(format!(
        "GitHub migration {id} did not finish exporting in time; run again with --resume"
    )))
}

/// Downloads a finished migration archive to `dest`.
pub async fn download_org_migration(
    client: &Client,
    org: &str,
    id: u64,
    dest: &Path,
) -> Result<()> {
    let bytes = client
        .download(&format!("/orgs/{org}/migrations/{id}/archive"), None)
        .await?;
    std::fs::write(dest, bytes).map_err(|e| io("writing the migration archive", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_round_trip_and_safety() {
        let entries = vec![
            TarEntry {
                path: "a/".into(),
                dir: true,
                data: vec![],
            },
            TarEntry {
                path: "a/b.json".into(),
                dir: false,
                data: b"[1]".to_vec(),
            },
        ];
        let t = write_tar(&entries);
        let back = read_tar(&t).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].data, b"[1]");
        assert!(safe_join(Path::new("/x"), "../etc").is_none());
        assert!(safe_join(Path::new("/x"), "/etc").is_none());
        assert_eq!(
            safe_join(Path::new("/x"), "a/b").unwrap(),
            Path::new("/x/a/b")
        );
    }

    #[test]
    fn synthetic_ids_never_look_real() {
        assert!(synthetic_user_id("alice") >= 1 << 63);
        assert_eq!(synthetic_user_id("Alice"), synthetic_user_id("alice"));
    }

    #[test]
    fn urls_to_numbers() {
        assert_eq!(
            number_in("https://github.com/o/r/pull/12", "/pull/"),
            Some(12)
        );
        assert_eq!(
            fragment_id(
                "https://github.com/o/r/issues/3#issuecomment-55",
                "issuecomment-"
            ),
            Some(55)
        );
    }
}
