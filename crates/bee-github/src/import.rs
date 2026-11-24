//! The importer: one command from a GitHub repository to Beehive (SPEC §17.1).
//!
//! [`Importer::run`] does, resuming wherever an earlier run stopped:
//!
//! 1. **Git first.** Fetch every branch, tag and pull ref into the local mirror and import
//!    it through the [`GitTarget`] with exact git ids. The repository is usable now.
//! 2. **Metadata.** The org migration archive when given (no quota), then the API for the
//!    rest ([`crate::fetch`]), checkpointed page by page.
//! 3. **Revision commits.** Fetch every force-push's `before` head while GitHub still
//!    serves it, pin PR bases and old heads under `refs/pull/<n>/{base,rev/<k>}`, and import
//!    the new history.
//! 4. **Principals.** One placeholder per GitHub account ([`crate::principals`]).
//! 5. **Records.** Labels, milestones, PRs (same numbers), issues (same numbers), releases
//!    with assets, LFS objects, land rules from branch protection and CODEOWNERS, webhooks,
//!    workflows, secrets checklist, the wiki — each through the [`ForgeSink`], journaled
//!    with its outcome so a resumed run never places a record twice and a changed record
//!    is placed again.
//! 6. **Report.** The fidelity report, the redirect map and the generated policy are
//!    written to the state directory.
//!
//! Live sync ([`crate::live`]) and cutover ([`Importer::cutover`]) reuse the same steps for
//! the records that changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use bee_import::ImportReport;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::archive;
use crate::client::Client;
use crate::convert::{self, Links, PrGit};
use crate::error::{Result, invalid_state, not_found};
use crate::fetch::{ARCHIVE_STEPS, Fetcher, replay_into};
use crate::git::{MIRROR_REFSPECS, Mirror, wiki_url};
use crate::links::RedirectMap;
use crate::model::{Metadata, Repository};
use crate::policy::{CODEOWNERS_PATHS, Translation, translate};
use crate::principals::{Claim, ClaimProof, Directory, unhex};
use crate::report::{
    FidelityReport, GithubSection, Outcome, PlaceholderRecord, Status, category, reason,
};
use crate::sink::{
    AssetImport, ForgeSink, LabelImport, LfsImport, MilestoneImport, PolicyImport, ReleaseImport,
    WebhookImport,
};
use crate::state::{Checkpoint, Entry, Phase, Replay, StateDir};
use crate::target::{GitOptions, GitRepo, GitTarget, merge_reports};

/// How to import.
#[derive(Clone, Debug)]
pub struct Options {
    /// `owner/name` on GitHub.
    pub repo: String,
    /// Beehive base URL, for rewritten links and redirects (`https://hub.example`).
    pub hub_url: String,
    /// Beehive org (defaults to the GitHub owner).
    pub hub_org: Option<String>,
    /// Stop after the git phase.
    pub mirror_only: bool,
    /// An org migration archive (`.tar.gz`) to read metadata (and git data) from.
    pub archive: Option<PathBuf>,
    /// Import the wiki.
    pub wiki: bool,
    /// Fetch LFS objects.
    pub lfs: bool,
    /// Round-trip verification of the git import.
    pub verify: bool,
    /// Largest release asset to download, in bytes.
    pub max_asset_bytes: u64,
    /// Time stamped on imported git rows (epoch ms, from the caller's clock).
    pub at_ms: u64,
}

impl Options {
    /// Defaults for `owner/name`.
    pub fn new(repo: impl Into<String>) -> Options {
        Options {
            repo: repo.into(),
            hub_url: "https://beehive.local".into(),
            hub_org: None,
            mirror_only: false,
            archive: None,
            wiki: true,
            lfs: true,
            verify: true,
            max_asset_bytes: 2 << 30,
            at_ms: 0,
        }
    }

    fn owner(&self) -> &str {
        self.repo.split('/').next().unwrap_or("")
    }

    fn name(&self) -> &str {
        self.repo.split('/').nth(1).unwrap_or("")
    }

    /// The Beehive repository URL.
    pub fn hub_repo(&self) -> String {
        format!(
            "{}/{}/{}",
            self.hub_url.trim_end_matches('/'),
            self.hub_org.as_deref().unwrap_or(self.owner()),
            self.name()
        )
    }
}

/// What a run produced.
#[derive(Clone, Debug)]
pub struct RunResult {
    /// The fidelity report (also in `<state>/report.json`).
    pub report: FidelityReport,
    /// The redirect map (also in `<state>/redirects.json`).
    pub redirects: RedirectMap,
    /// The land-rule translation (also in `<state>/policy.toml`).
    pub policy: Translation,
    /// The phase reached.
    pub phase: Phase,
    /// Records placed by this run (keys).
    pub placed: Vec<String>,
}

/// A dry-run estimate (SPEC §17.1 step 2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    /// `owner/name`.
    pub repository: String,
    /// Pull requests on GitHub.
    pub pull_requests: u64,
    /// Issues on GitHub.
    pub issues: u64,
    /// Estimated API requests (REST pages, GraphQL batches, reaction lists).
    pub requests: u64,
    /// The credential's hourly limit.
    pub rate_limit: u64,
    /// Requests left in the current window.
    pub rate_remaining: u64,
    /// Estimated seconds to fetch the metadata at the live rate limit.
    pub eta_secs: u64,
    /// What will be archived or dropped by design (secrets, pending reviews, ...).
    pub notes: Vec<String>,
}

/// The importer.
pub struct Importer {
    client: Client,
    state: StateDir,
    git: Arc<dyn GitTarget>,
    sink: Arc<dyn ForgeSink>,
    opts: Options,
    lock: tokio::sync::Mutex<()>,
}

/// Everything loaded for one operation.
pub(crate) struct Session<'a> {
    pub(crate) cp: Checkpoint,
    pub(crate) fetcher: Fetcher<'a>,
    pub(crate) applied: BTreeMap<String, (String, Vec<Outcome>)>,
    pub(crate) dir: Directory,
    pub(crate) placed: Vec<String>,
}

impl Importer {
    /// An importer for `opts`, fetching with `client`, keeping state in `state`, writing
    /// history to `git` and metadata to `sink`.
    pub fn new(
        client: Client,
        state: StateDir,
        git: Arc<dyn GitTarget>,
        sink: Arc<dyn ForgeSink>,
        opts: Options,
    ) -> Importer {
        Importer {
            client,
            state,
            git,
            sink,
            opts,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The state directory.
    pub fn state(&self) -> &StateDir {
        &self.state
    }

    /// The client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The options.
    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// The sink.
    pub fn sink(&self) -> &Arc<dyn ForgeSink> {
        &self.sink
    }

    pub(crate) async fn guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    pub(crate) fn session(&self) -> Result<Session<'_>> {
        let mut cp = self.state.load()?;
        if !cp.source.is_empty() && cp.source != self.opts.repo {
            return Err(crate::error::invalid(
                "state_mismatch",
                format!(
                    "{} holds the import of {}, not {}",
                    self.state.root().display(),
                    cp.source,
                    self.opts.repo
                ),
            ));
        }
        cp.source = self.opts.repo.clone();
        if cp.claim_secret.is_empty() {
            cp.claim_secret = Directory::new_secret()?;
        }
        self.state.save(&cp)?;
        self.client.load_etags(cp.etags.clone());
        let replay: Replay = self.state.replay()?;
        let mut meta = Metadata::default();
        replay_into(&mut meta, &replay);
        let dir = Directory::new(
            unhex(&cp.claim_secret).unwrap_or_default(),
            cp.claims.clone(),
        );
        let fetcher = Fetcher::new(&self.client, &self.state, &self.opts.repo, &replay, meta);
        Ok(Session {
            cp,
            fetcher,
            applied: replay.applied,
            dir,
            placed: Vec::new(),
        })
    }

    pub(crate) fn save(&self, s: &mut Session<'_>) -> Result<()> {
        s.cp.etags = self.client.etags();
        self.state.save(&s.cp)
    }

    fn links(&self, repo: &Repository) -> Links {
        let web = if repo.html_url.is_empty() {
            format!(
                "{}/{}",
                self.client.config().web_url.trim_end_matches('/'),
                self.opts.repo
            )
        } else {
            repo.html_url.clone()
        };
        Links {
            web_repo: web,
            hub_repo: self.opts.hub_repo(),
        }
    }

    pub(crate) async fn mirror(&self) -> Result<Mirror> {
        Mirror::open(&self.state.mirror(), self.client.git_auth_header().await?)
    }

    fn clone_url(&self, repo: &Repository) -> String {
        if repo.clone_url.is_empty() {
            format!(
                "{}/{}.git",
                self.client.config().web_url.trim_end_matches('/'),
                self.opts.repo
            )
        } else {
            repo.clone_url.clone()
        }
    }

    fn git_opts(&self) -> GitOptions {
        GitOptions {
            source: format!("github:{}", self.opts.repo),
            verify: self.opts.verify,
            at_ms: self.opts.at_ms,
        }
    }

    fn load_git_report(&self) -> Option<ImportReport> {
        std::fs::read_to_string(self.state.path("git-report.json"))
            .ok()
            .and_then(|t| ImportReport::from_json(&t).ok())
    }

    /// Imports the mirror and folds the result into the stored git report.
    pub(crate) async fn import_git(&self) -> Result<ImportReport> {
        let r = self
            .git
            .import(&self.state.mirror(), GitRepo::Main, &self.git_opts())
            .await?
            .ok_or_else(|| invalid_state("the git target has no repository to import into"))?;
        let merged = merge_reports(self.load_git_report(), r);
        self.state
            .write_atomic("git-report.json", merged.to_json().as_bytes())?;
        Ok(merged)
    }

    /// Phase 1: fetch and import git history (incremental on re-runs).
    pub(crate) async fn git_phase(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
    ) -> Result<ImportReport> {
        let mirror = self.mirror().await?;
        let source = match self.archive_git_dir() {
            Some(dir) => dir.display().to_string(),
            None => self.clone_url(repo),
        };
        mirror.fetch(&source, &MIRROR_REFSPECS)?;
        if !repo.default_branch.is_empty() {
            mirror.set_head(&repo.default_branch)?;
        }
        let r = self.import_git().await?;
        if s.cp.phase < Phase::Git {
            s.cp.phase = Phase::Git;
        }
        self.save(s)?;
        Ok(r)
    }

    fn archive_git_dir(&self) -> Option<PathBuf> {
        let d = self.state.path("archive/archive-repo.git");
        (self.opts.archive.is_some() && d.join("HEAD").exists()).then_some(d)
    }

    /// Reads the migration archive once into the journal.
    async fn archive_phase(&self, s: &mut Session<'_>) -> Result<bool> {
        let Some(path) = &self.opts.archive else {
            return Ok(false);
        };
        if s.fetcher.done.contains("archive") {
            return Ok(true);
        }
        let extract = self.state.path("archive");
        let mut data = archive::open(path, &self.opts.repo, &extract)?;
        // Archives name accounts by login; ask the API for their numeric ids.
        let mut ids: BTreeMap<String, (u64, String)> = BTreeMap::new();
        for login in &data.logins {
            match self
                .client
                .get_json(&format!("/users/{}", crate::fetch::encode_path(login)))
                .await
            {
                Ok(v) => {
                    if let Some(id) = v["id"].as_u64() {
                        ids.insert(
                            login.clone(),
                            (id, v["type"].as_str().unwrap_or("User").to_string()),
                        );
                    }
                }
                Err(e) if crate::fetch::is_access_error(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let synthetic = archive::assign_user_ids(&mut data.meta, &ids);
        let m = &data.meta;
        let ser = |v: Vec<Value>| v;
        s.fetcher
            .absorb("labels", ser(m.labels.values().map(|x| json!(x)).collect()))?;
        s.fetcher.absorb(
            "milestones",
            m.milestones.values().map(|x| json!(x)).collect(),
        )?;
        s.fetcher
            .absorb("issues", m.issues.values().map(|x| json!(x)).collect())?;
        s.fetcher
            .absorb("pulls", m.pulls.values().map(|x| json!(x)).collect())?;
        s.fetcher.absorb(
            "review_comments",
            m.review_comments.values().map(|x| json!(x)).collect(),
        )?;
        s.fetcher.absorb(
            "issue_comments",
            m.issue_comments.values().map(|x| json!(x)).collect(),
        )?;
        s.fetcher
            .absorb("releases", m.releases.values().map(|x| json!(x)).collect())?;
        for (n, rs) in &m.reviews {
            s.fetcher.absorb(
                &format!("reviews:{n}"),
                rs.values().map(|x| json!(x)).collect(),
            )?;
        }
        for (b, p) in &m.protections {
            s.fetcher
                .absorb(&format!("protection:{b}"), vec![json!(p)])?;
        }
        for login in synthetic {
            s.fetcher.absorb(
                "unreadable:user-id",
                vec![json!({ "error": format!("no numeric GitHub id for {login}; its placeholder is keyed by login") })],
            )?;
        }
        for st in ARCHIVE_STEPS {
            s.fetcher.mark_done(st)?;
        }
        s.fetcher.mark_done("archive")?;
        Ok(true)
    }

    /// Phase 2: metadata.
    pub(crate) async fn metadata_phase(&self, s: &mut Session<'_>) -> Result<()> {
        let from_archive = self.archive_phase(s).await?;
        let skip: &[&str] = if from_archive { &ARCHIVE_STEPS } else { &[] };
        s.fetcher.lists(skip).await?;
        s.fetcher.timelines(None, false).await?;
        s.fetcher.reactions().await?;
        s.fetcher.meta.collect_users();
        if s.cp.phase < Phase::Metadata {
            s.cp.phase = Phase::Metadata;
        }
        self.save(s)
    }

    /// Phase 3: pins PR bases and pre-force-push heads in the mirror (fetching those GitHub
    /// still serves) and imports what is new. Returns the commits present.
    pub(crate) async fn revision_commits(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
        only: Option<&BTreeSet<u64>>,
    ) -> Result<BTreeSet<String>> {
        let mirror = self.mirror().await?;
        let url = match self.archive_git_dir() {
            Some(d) => d.display().to_string(),
            None => self.clone_url(repo),
        };
        let meta = &s.fetcher.meta;
        let mut wanted: Vec<String> = Vec::new();
        for (n, pr) in &meta.pulls {
            wanted.push(pr.head.sha.clone());
            wanted.push(pr.base.sha.clone());
            for fp in meta.force_pushes.get(n).into_iter().flatten() {
                wanted.extend(fp.before.clone());
            }
        }
        let mut present = mirror.existing_commits(&wanted)?;
        let mut pins: Vec<(String, String)> = Vec::new();
        let have: BTreeMap<String, String> = mirror.refs()?.into_iter().collect();
        let mut fetched_any = false;
        for (n, pr) in &meta.pulls {
            // Only PRs that changed are worth a network fetch; presence covers them all.
            let may_fetch = only.is_none_or(|o| o.contains(n));
            if !pr.base.sha.is_empty() {
                let name = format!("refs/pull/{n}/base");
                if may_fetch
                    && !present.contains(&pr.base.sha)
                    && mirror.fetch_commit(&url, &pr.base.sha, &name)?
                {
                    present.insert(pr.base.sha.clone());
                    fetched_any = true;
                }
                if present.contains(&pr.base.sha) && have.get(&name) != Some(&pr.base.sha) {
                    pins.push((name, pr.base.sha.clone()));
                }
            }
            for (k, fp) in meta.force_pushes.get(n).into_iter().flatten().enumerate() {
                let Some(sha) = &fp.before else { continue };
                let name = format!("refs/pull/{n}/rev/{}", k + 1);
                if may_fetch && !present.contains(sha) && mirror.fetch_commit(&url, sha, &name)? {
                    present.insert(sha.clone());
                    fetched_any = true;
                }
                if present.contains(sha) && have.get(&name) != Some(sha) {
                    pins.push((name, sha.clone()));
                }
            }
        }
        let changed = !pins.is_empty() || fetched_any;
        mirror.update_refs(&pins)?;
        if changed {
            self.import_git().await?;
        }
        Ok(present)
    }

    /// Places one record unless it was already placed with the same fingerprint.
    pub(crate) async fn place<F, Fut>(
        &self,
        s: &mut Session<'_>,
        key: &str,
        fingerprint: String,
        f: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Outcome>>>,
    {
        if s.applied.get(key).is_some_and(|(fp, _)| *fp == fingerprint) {
            return Ok(());
        }
        let outcomes = f().await?;
        self.state.append(&[Entry::Applied {
            key: key.to_string(),
            fingerprint: fingerprint.clone(),
            outcomes: outcomes.clone(),
        }])?;
        s.applied.insert(key.to_string(), (fingerprint, outcomes));
        s.placed.push(key.to_string());
        Ok(())
    }

    /// Phases 4–5: principals and records. `only` limits PRs and issues to those numbers
    /// (live sync); other sections are re-placed only when they changed.
    pub(crate) async fn place_records(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
        present: &BTreeSet<String>,
        git: &ImportReport,
    ) -> Result<Translation> {
        let links = self.links(repo);
        let meta = s.fetcher.meta.clone();
        s.dir.add_all(meta.users.values());
        // Principals.
        let all: Vec<_> = s.dir.all().cloned().collect();
        let sink = self.sink.clone();
        self.place(s, "principals", convert::fingerprint(&all), || async move {
            sink.principals(&all).await
        })
        .await?;
        // Labels and milestones.
        for l in meta.labels.values() {
            let rec = LabelImport {
                name: l.name.clone(),
                color: l.color.clone(),
                description: l.description.clone().unwrap_or_default(),
            };
            let sink = self.sink.clone();
            self.place(
                s,
                &format!("label:{}", l.name),
                convert::fingerprint(&rec),
                || async move { sink.label(&rec).await },
            )
            .await?;
        }
        for m in meta.milestones.values() {
            let rec = MilestoneImport {
                number: m.number,
                title: m.title.clone(),
                description: m.description.clone().unwrap_or_default(),
                state: m.state.clone(),
                due_on: m.due_on.clone(),
            };
            let sink = self.sink.clone();
            self.place(
                s,
                &format!("milestone:{}", m.number),
                convert::fingerprint(&rec),
                || async move { sink.milestone(&rec).await },
            )
            .await?;
        }
        // Change requests, in number order (stacked parents first).
        let mirror_refs: BTreeSet<String> = self
            .mirror()
            .await?
            .refs()?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for pr in meta.pulls.values() {
            let g = PrGit {
                present: present.clone(),
                base_ref: mirror_refs.contains(&format!("refs/pull/{}/base", pr.number)),
            };
            let (rec, pre) = convert::change(&meta, pr, &g, &mut s.dir, &links);
            let key = format!("pr:{}", pr.number);
            let fp = convert::fingerprint(&(&rec, &pre));
            let sink = self.sink.clone();
            self.place(s, &key, fp, || async move {
                let mut out = pre;
                if let Some(c) = rec {
                    out.extend(sink.change(&c).await?);
                }
                Ok(out)
            })
            .await?;
        }
        // Issues.
        for i in meta.issues.values() {
            let rec = convert::issue(&meta, i, &mut s.dir, &links);
            let sink = self.sink.clone();
            self.place(
                s,
                &format!("issue:{}", i.number),
                convert::fingerprint(&rec),
                || async move { sink.issue(&rec).await },
            )
            .await?;
        }
        // Releases with assets.
        for r in meta.releases.values() {
            let key = format!("release:{}", r.id);
            let fp = convert::fingerprint(r);
            if s.applied.get(&key).is_some_and(|(f, _)| *f == fp) {
                continue;
            }
            let mut pre = Vec::new();
            let mut assets = Vec::new();
            for a in &r.assets {
                let item = format!("release:{}/{}", r.tag_name, a.name);
                if a.size > self.opts.max_asset_bytes {
                    pre.push(Outcome::dropped(
                        category::RELEASE_ASSETS,
                        item,
                        bee_import::report::reason::TOO_LARGE,
                        format!(
                            "{} bytes, over the {}-byte asset limit",
                            a.size, self.opts.max_asset_bytes
                        ),
                    ));
                    continue;
                }
                let url = if a.url.is_empty() {
                    &a.browser_download_url
                } else {
                    &a.url
                };
                match self
                    .client
                    .download(url, Some("application/octet-stream"))
                    .await
                {
                    Ok(data) => assets.push(AssetImport {
                        id: a.id,
                        name: a.name.clone(),
                        label: a.label.clone().unwrap_or_default(),
                        content_type: a.content_type.clone(),
                        data,
                    }),
                    Err(e) if e.retryable && e.reason != bee_core::Reason::Unavailable => {
                        return Err(e);
                    }
                    Err(e) => pre.push(Outcome::dropped(
                        category::RELEASE_ASSETS,
                        item,
                        reason::ASSET_FETCH_FAILED,
                        e.message,
                    )),
                }
            }
            let rec = ReleaseImport {
                id: r.id,
                tag: r.tag_name.clone(),
                name: r.name.clone().unwrap_or_default(),
                body: links.text(r.body.as_deref().unwrap_or("")),
                draft: r.draft,
                prerelease: r.prerelease,
                target: r.target_commitish.clone(),
                author: convert::author(&mut s.dir, &r.author),
                created_at: r.created_at.clone(),
                published_at: r.published_at.clone(),
                assets,
            };
            let sink = self.sink.clone();
            self.place(s, &key, fp, || async move {
                let mut out = pre;
                out.extend(sink.release(&rec).await?);
                Ok(out)
            })
            .await?;
        }
        // LFS objects.
        if self.opts.lfs && !git.lfs_objects.is_empty() {
            self.lfs(s, git).await?;
        }
        // Land rules and owners.
        let policy = self.policy(s, repo, &meta).await?;
        // Webhooks.
        let hooks: Vec<WebhookImport> = meta
            .hooks
            .values()
            .map(|h| WebhookImport {
                id: h.id,
                url: h.config.url.clone().unwrap_or_default(),
                events: h.events.clone(),
                content_type: h
                    .config
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "json".into()),
                active_on_github: h.active,
            })
            .collect();
        let hooks_unreadable = meta.unreadable.get("hooks").cloned();
        let sink = self.sink.clone();
        self.place(s, "webhooks", convert::fingerprint(&(&hooks, &hooks_unreadable)), || async move {
            let mut out = sink.webhooks(&hooks).await?;
            if let Some(why) = hooks_unreadable {
                out.push(Outcome::dropped(
                    category::WEBHOOKS,
                    "webhooks",
                    reason::UNREADABLE,
                    format!("the credential cannot list webhooks ({why}); copy them by hand or use an admin token or the GitHub App"),
                ));
            }
            Ok(out)
        })
        .await?;
        // Workflows (kept as-is in git; bee-workflow runs them).
        let mirror = self.mirror().await?;
        let mut wf: Vec<String> = mirror
            .list_dir("HEAD", ".github/workflows")?
            .into_iter()
            .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
            .collect();
        wf.sort();
        self.place(s, "workflows", convert::fingerprint(&wf), || async move {
            Ok(wf
                .iter()
                .map(|f| Outcome::imported(category::WORKFLOWS, format!(".github/workflows/{f}")))
                .collect())
        })
        .await?;
        // Secrets and deploy keys: names only.
        let named: Vec<_> = meta.named.iter().cloned().collect();
        let unreadable: Vec<(String, String)> = ["secrets", "keys"]
            .iter()
            .filter_map(|k| meta.unreadable.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect();
        self.place(s, "named", convert::fingerprint(&(&named, &unreadable)), || async move {
            let mut out = Vec::new();
            for n in &named {
                let cat = if n.kind == "deploy_key" { category::DEPLOY_KEYS } else { category::SECRETS };
                out.push(Outcome::archived(
                    cat,
                    format!("{}:{}", n.kind, n.name),
                    "name listed in the manual checklist; GitHub never exports the value",
                ));
            }
            for (k, why) in &unreadable {
                let cat = if k == "keys" { category::DEPLOY_KEYS } else { category::SECRETS };
                out.push(Outcome::dropped(
                    cat,
                    k.clone(),
                    reason::UNREADABLE,
                    format!("the credential cannot list {k} ({why}); check the GitHub settings by hand"),
                ));
            }
            Ok(out)
        })
        .await?;
        // Wiki.
        if self.opts.wiki && repo.has_wiki {
            self.wiki(s, repo).await?;
        }
        Ok(policy)
    }

    async fn lfs(&self, s: &mut Session<'_>, git: &ImportReport) -> Result<()> {
        let pending: Vec<_> = git
            .lfs_objects
            .iter()
            .filter(|o| !s.applied.contains_key(&format!("lfs:{}", o.oid)))
            .cloned()
            .collect();
        for chunk in pending.chunks(100) {
            let url = format!(
                "{}/{}.git/info/lfs/objects/batch",
                self.client.config().web_url.trim_end_matches('/'),
                self.opts.repo
            );
            let objects: Vec<Value> = chunk
                .iter()
                .map(|o| json!({ "oid": o.oid.trim_start_matches("sha256:"), "size": o.size }))
                .collect();
            let resp = self
                .client
                .post_raw(
                    &url,
                    &json!({ "operation": "download", "transfers": ["basic"], "objects": objects }),
                    &[
                        ("accept", "application/vnd.git-lfs+json".to_string()),
                        ("content-type", "application/vnd.git-lfs+json".to_string()),
                    ],
                    true,
                )
                .await;
            let batch = match resp {
                Ok(r) if r.status == 200 => r.json().unwrap_or(Value::Null),
                Ok(r) => json!({ "error": format!("HTTP {}", r.status) }),
                Err(e) => json!({ "error": e.message }),
            };
            for o in chunk {
                let hex = o.oid.trim_start_matches("sha256:").to_string();
                let entry = batch["objects"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["oid"].as_str() == Some(&hex)))
                    .cloned()
                    .unwrap_or(Value::Null);
                let href = entry["actions"]["download"]["href"]
                    .as_str()
                    .map(str::to_string);
                let headers: Vec<(String, String)> = entry["actions"]["download"]["header"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| {
                                Some((k.to_ascii_lowercase(), v.as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let outcome = match href {
                    None => vec![Outcome::dropped(
                        category::LFS_OBJECTS,
                        o.oid.clone(),
                        reason::LFS_FETCH_FAILED,
                        format!(
                            "the LFS server did not offer it ({})",
                            entry["error"]["message"]
                                .as_str()
                                .or(batch["error"].as_str())
                                .unwrap_or("not listed in the batch response")
                        ),
                    )],
                    Some(href) => {
                        let extra: Vec<(&str, String)> = headers
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.clone()))
                            .collect();
                        match self.client.get_raw(&href, &extra).await {
                            Ok(r) if r.status == 200 => {
                                let got = crate::principals::hex(&sha2::Sha256::digest(&r.body));
                                if got == hex {
                                    self.sink
                                        .lfs_object(&LfsImport {
                                            oid: o.oid.clone(),
                                            data: r.body.to_vec(),
                                        })
                                        .await?
                                } else {
                                    vec![Outcome::dropped(
                                        category::LFS_OBJECTS,
                                        o.oid.clone(),
                                        reason::INTEGRITY,
                                        format!("downloaded bytes hash to sha256:{got}"),
                                    )]
                                }
                            }
                            Ok(r) => vec![Outcome::dropped(
                                category::LFS_OBJECTS,
                                o.oid.clone(),
                                reason::LFS_FETCH_FAILED,
                                format!("download returned HTTP {}", r.status),
                            )],
                            Err(e) => vec![Outcome::dropped(
                                category::LFS_OBJECTS,
                                o.oid.clone(),
                                reason::LFS_FETCH_FAILED,
                                e.message,
                            )],
                        }
                    }
                };
                let key = format!("lfs:{}", o.oid);
                self.place(s, &key, o.oid.clone(), || async move { Ok(outcome) })
                    .await?;
            }
        }
        Ok(())
    }

    async fn policy(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
        meta: &Metadata,
    ) -> Result<Translation> {
        let mirror = self.mirror().await?;
        let mut owners: Option<(String, String)> = None;
        for p in CODEOWNERS_PATHS {
            if let Some(b) = mirror.read_file("HEAD", p)? {
                owners = Some((p.to_string(), String::from_utf8_lossy(&b).into_owned()));
                break;
            }
        }
        let t = translate(
            &repo.full_name,
            &meta.protections,
            owners.as_ref().map(|(p, t)| (p.as_str(), t.as_str())),
            &mut s.dir,
        );
        self.state.write_atomic("policy.toml", t.toml.as_bytes())?;
        let sink = self.sink.clone();
        let tr = t.clone();
        let unreadable_protection: Vec<(String, String)> = meta
            .unreadable
            .iter()
            .filter(|(k, _)| k.starts_with("protection:") || k.as_str() == "branches")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let owners_text = owners.clone();
        self.place(
            s,
            "policy",
            convert::fingerprint(&(&t.toml, &t.untranslatable, &unreadable_protection)),
            || async move {
                let stored = sink
                    .policy(&PolicyImport {
                        toml: tr.toml.clone(),
                        codeowners: tr.codeowners.clone(),
                    })
                    .await?;
                let mut out = Vec::new();
                let note = if stored {
                    "translated into the generated land rules, waiting for review"
                } else {
                    "translated into policy.toml in the state directory, waiting for review"
                };
                for p in &tr.translated {
                    let caveats: Vec<&str> = tr
                        .untranslatable
                        .iter()
                        .filter(|u| u.item == *p)
                        .map(|u| u.setting.as_str())
                        .collect();
                    if caveats.is_empty() {
                        out.push(Outcome::imported(category::BRANCH_PROTECTION, p.clone()));
                    } else {
                        out.push(Outcome::archived(
                            category::BRANCH_PROTECTION,
                            p.clone(),
                            format!("{note}; not translated: {}", caveats.join(", ")),
                        ));
                    }
                }
                for p in &tr.skipped {
                    out.push(Outcome::dropped(
                        category::BRANCH_PROTECTION,
                        p.clone(),
                        reason::UNTRANSLATABLE,
                        "the branch pattern has no bee equivalent; write a land rule per branch",
                    ));
                }
                for (k, why) in &unreadable_protection {
                    out.push(Outcome::dropped(
                        category::BRANCH_PROTECTION,
                        k.clone(),
                        reason::UNREADABLE,
                        format!("the credential cannot read branch protection ({why})"),
                    ));
                }
                if let Some((path, text)) = &owners_text {
                    for (i, line) in text.lines().enumerate() {
                        let l = line.trim();
                        if l.is_empty() || l.starts_with('#') {
                            continue;
                        }
                        let item = format!("{path}:{}", i + 1);
                        match tr.untranslatable.iter().find(|u| u.item == item) {
                            Some(u) => out.push(Outcome::dropped(
                                category::CODEOWNERS,
                                item,
                                reason::UNTRANSLATABLE,
                                u.detail.clone(),
                            )),
                            None => out.push(Outcome::imported(category::CODEOWNERS, item)),
                        }
                    }
                }
                Ok(out)
            },
        )
        .await?;
        Ok(t)
    }

    async fn wiki(&self, s: &mut Session<'_>, repo: &Repository) -> Result<()> {
        let mirror = Mirror::open(&self.state.wiki(), self.client.git_auth_header().await?)?;
        let src = match self.state.path("archive/archive-wiki.git") {
            p if self.opts.archive.is_some() && p.join("HEAD").exists() => p.display().to_string(),
            _ => wiki_url(&self.clone_url(repo)),
        };
        let outcomes = match mirror.fetch(&src, &MIRROR_REFSPECS[..2]) {
            Err(e) if e.reason == bee_core::Reason::NotFound => vec![Outcome::excluded(
                category::WIKI,
                "wiki",
                "the wiki is enabled but has no pages",
            )],
            Err(e) => return Err(e),
            Ok(()) => {
                let opts = self.git_opts();
                match self
                    .git
                    .import(&self.state.wiki(), GitRepo::Wiki, &opts)
                    .await?
                {
                    None => vec![Outcome::dropped(
                        category::WIKI,
                        "wiki",
                        reason::NOT_SUPPORTED_BY_TARGET,
                        format!(
                            "the target has no wiki repository; the wiki is mirrored at {}",
                            self.state.wiki().display()
                        ),
                    )],
                    Some(r) => {
                        let mut out: Vec<Outcome> = r
                            .refs
                            .iter()
                            .map(|x| Outcome::imported(category::WIKI, format!("wiki:{}", x.bee)))
                            .collect();
                        for c in &r.categories {
                            for d in &c.drops {
                                out.push(Outcome::dropped(
                                    category::WIKI,
                                    format!("wiki:{}", d.item),
                                    &d.reason,
                                    d.detail.clone(),
                                ));
                            }
                        }
                        if out.is_empty() {
                            out.push(Outcome::imported(category::WIKI, "wiki"));
                        }
                        out
                    }
                }
            }
        };
        let refs = mirror.refs()?;
        self.place(s, "wiki", convert::fingerprint(&refs), || async move {
            Ok(outcomes)
        })
        .await
    }

    /// Builds, stores and returns the report and redirect map.
    pub(crate) fn finish(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
        git: ImportReport,
        policy: Translation,
        phase: Phase,
        label: &str,
    ) -> Result<RunResult> {
        let meta = &s.fetcher.meta;
        let links = self.links(repo);
        let redirects = RedirectMap::build(meta, &links.web_repo, &links.hub_repo);
        self.state.write_atomic(
            "redirects.json",
            serde_json::to_string_pretty(&redirects)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        let outcomes: Vec<Outcome> = s
            .applied
            .values()
            .flat_map(|(_, o)| o.iter().cloned())
            .collect();
        let contributions = convert::contributions(meta);
        let placeholders = s
            .dir
            .all()
            .map(|p| PlaceholderRecord {
                github_id: p.github_id,
                login: p.login.clone(),
                principal: p.principal.clone(),
                kind: p.kind().into(),
                claimed: s.dir.claimed(p.github_id),
                contributions: contributions.get(&p.github_id).copied().unwrap_or(0),
            })
            .collect();
        let mut manual = Vec::new();
        for n in &meta.named {
            manual.push(match n.kind.as_str() {
                "deploy_key" => format!("re-add deploy key `{}` in Beehive", n.name),
                _ => format!(
                    "re-enter Actions secret `{}` in Beehive (GitHub never exports secret values)",
                    n.name
                ),
            });
        }
        for h in meta.hooks.values() {
            manual.push(format!(
                "re-enter the secret of webhook {} and enable it",
                h.config.url.clone().unwrap_or_default()
            ));
        }
        for (k, why) in &meta.unreadable {
            manual.push(format!(
                "GitHub did not let this credential read {k} ({why}); check it by hand or rerun with an admin token or the GitHub App"
            ));
        }
        if !policy.translated.is_empty() || policy.codeowners.is_some() {
            manual.push(format!(
                "review the generated land rules in {} and land them as .beehive/policy.toml",
                self.state.path("policy.toml").display()
            ));
        }
        if outcomes.iter().any(|o| matches!(&o.status, Status::Dropped { reason: r, .. } if r == reason::NOT_SUPPORTED_BY_TARGET)) {
            manual.push(format!(
                "records this Beehive server cannot store yet are kept in {}; rerun with --resume once it can",
                self.state.path("journal.jsonl").display()
            ));
        }
        let mut sources = vec!["git".to_string(), "api".to_string()];
        if self.opts.archive.is_some() {
            sources.push("migration_archive".into());
        }
        let section = GithubSection {
            repository: self.opts.repo.clone(),
            phase: label.to_string(),
            sources,
            placeholders,
            manual_steps: manual,
            untranslatable: policy.untranslatable.clone(),
            redirects: redirects.len() as u64,
            unexplained_drops: 0,
        };
        let report = FidelityReport::build(git, &outcomes, section);
        self.state
            .write_atomic("report.json", report.to_json().as_bytes())?;
        if s.cp.phase < phase {
            s.cp.phase = phase;
        }
        self.save(s)?;
        Ok(RunResult {
            report,
            redirects,
            policy,
            phase: s.cp.phase,
            placed: std::mem::take(&mut s.placed),
        })
    }

    /// Runs (or resumes) the import up to the mirror phase.
    pub async fn run(&self) -> Result<RunResult> {
        let _g = self.guard().await;
        let mut s = self.session()?;
        if s.cp.phase >= Phase::Frozen {
            return Err(invalid_state(
                "this import is frozen for cutover; it no longer follows GitHub",
            ));
        }
        let repo = s.fetcher.repo().await?;
        if self.opts.archive.is_some() {
            // Extract the archive first so its git data is the fetch source.
            self.archive_phase(&mut s).await?;
        }
        let git = self.git_phase(&mut s, &repo).await?;
        if self.opts.mirror_only {
            let label = "mirror_only";
            return self.finish(
                &mut s,
                &repo,
                git,
                Translation::default(),
                Phase::Git,
                label,
            );
        }
        self.metadata_phase(&mut s).await?;
        let present = self.revision_commits(&mut s, &repo, None).await?;
        let git = self.load_git_report().unwrap_or(git);
        let policy = self.place_records(&mut s, &repo, &present, &git).await?;
        let phase = if s.cp.phase > Phase::Mirror {
            s.cp.phase
        } else {
            Phase::Mirror
        };
        self.finish(&mut s, &repo, git, policy, phase, phase.as_str())
    }

    /// The last report written, if any.
    pub fn last_report(&self) -> Option<FidelityReport> {
        std::fs::read_to_string(self.state.path("report.json"))
            .ok()
            .and_then(|t| FidelityReport::from_json(&t).ok())
    }

    /// The claim token for a GitHub account (hand it to the person).
    pub fn claim_token(&self, github_id: u64) -> Result<String> {
        let s = self.session()?;
        Ok(s.dir.claim_token(github_id))
    }

    /// Claims a placeholder for `principal` and moves its history in the target. One claim
    /// covers every repository imported, because placeholder ids depend only on the GitHub
    /// id.
    pub async fn claim(
        &self,
        github_id: u64,
        proof: &ClaimProof,
        principal: &str,
    ) -> Result<(Claim, u64)> {
        let _g = self.guard().await;
        let mut s = self.session()?;
        s.fetcher.meta.collect_users();
        s.dir.add_all(s.fetcher.meta.users.values());
        if s.dir.get(github_id).is_none() {
            return Err(not_found(format!(
                "GitHub user {github_id} has nothing in this import"
            )));
        }
        let claim = s.dir.claim(github_id, proof, principal)?;
        s.cp.claims = s.dir.claims().clone();
        self.save(&mut s)?;
        let moved = self.sink.reassign(&claim).await?;
        Ok((claim, moved))
    }

    /// Estimates an import without fetching it.
    pub async fn dry_run(&self) -> Result<Estimate> {
        let rl = self.client.fetch_rate_limit().await?;
        let count = |q: String| async move {
            self.client
                .get_json(&format!("/search/issues?q={q}&per_page=1"))
                .await
                .map(|v| v["total_count"].as_u64().unwrap_or(0))
        };
        let prs = count(format!("repo:{}+is:pr", self.opts.repo)).await?;
        let issues = count(format!("repo:{}+is:issue", self.opts.repo)).await?;
        let per_page = u64::from(self.client.config().per_page.max(1));
        // Lists: issues (PRs included), PRs, two comment lists (~3 comments per item);
        // GraphQL timelines in batches; reactions on ~5% of items.
        let items = prs + issues;
        let requests = items.div_ceil(per_page)
            + prs.div_ceil(per_page)
            + (3 * items).div_ceil(per_page) * 2
            + prs.div_ceil(crate::fetch::TIMELINE_BATCH as u64)
            + items / 20
            + 12;
        let limit = rl.limit.unwrap_or(5000).max(1);
        let remaining = rl.remaining.unwrap_or(limit);
        let eta = if requests <= remaining {
            requests / 3
        } else {
            (requests - remaining) * 3600 / limit + remaining / 3
        };
        Ok(Estimate {
            repository: self.opts.repo.clone(),
            pull_requests: prs,
            issues,
            requests,
            rate_limit: limit,
            rate_remaining: remaining,
            eta_secs: eta,
            notes: vec![
                "secret values, deploy keys and webhook secrets are not exportable: they become a checklist".into(),
                "pending (unsubmitted) reviews are excluded".into(),
                "commits replaced by force-pushes are carried only while GitHub still serves them".into(),
                "branch protection is translated into land rules for review; settings without an equivalent are listed".into(),
            ],
        })
    }
}
