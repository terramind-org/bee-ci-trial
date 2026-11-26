//! Metadata backfill: checkpointed, resumable fetches from the REST and GraphQL APIs.
//!
//! The fetch is a list of **steps**. A list step pages through one endpoint; each page is
//! journaled ([`crate::state::Entry::Items`]) together with the next page's URL, so a killed
//! run resumes at the first page it had not stored. Per-PR data comes from GraphQL in
//! batches ([`TIMELINE_BATCH`] PRs per query: force-push events and reviews together), which
//! is what keeps a Kubernetes-sized import within an App's hourly quota. Optional sections a
//! credential may not read (webhooks, secrets, deploy keys, branch protection) are recorded
//! as unreadable instead of failing the import.
//!
//! Endpoints used (all repo-wide where GitHub offers it):
//!
//! | step | endpoint |
//! |---|---|
//! | `repo` | `GET /repos/{o}/{r}` |
//! | `labels`, `milestones`, `releases` | `GET /repos/{o}/{r}/{labels,milestones?state=all,releases}` |
//! | `issues` | `GET /repos/{o}/{r}/issues?state=all` (PR entries give PR reaction totals) |
//! | `pulls` | `GET /repos/{o}/{r}/pulls?state=all` |
//! | `review_comments` | `GET /repos/{o}/{r}/pulls/comments` |
//! | `issue_comments` | `GET /repos/{o}/{r}/issues/comments` |
//! | `branches` + `protection:<b>` | `GET /repos/{o}/{r}/branches?protected=true`, `GET …/branches/{b}/protection` |
//! | `hooks`, `secrets`, `keys` | `GET …/hooks`, `GET …/actions/secrets`, `GET …/keys` |
//! | `timeline:<n>` | GraphQL: `HeadRefForcePushedEvent`s and reviews of up to 25 PRs from #n |
//! | `reactions:<subject>` | `GET …/reactions` of each subject whose summary is non-zero |

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::client::{Client, decode};
use crate::error::Result;
use crate::model::{
    Branch, ForcePush, Hook, Issue, IssueComment, Label, Metadata, Milestone, NamedItem,
    Protection, PullRequest, Reaction, Release, Repository, Review, ReviewComment, User,
};
use crate::state::{Entry, Replay, StateDir};

/// PRs per GraphQL timeline query.
pub const TIMELINE_BATCH: usize = 25;

/// Merges one journaled page into the metadata. Unknown steps are ignored (a newer
/// journal read by an older binary keeps working).
pub fn apply(meta: &mut Metadata, step: &str, items: &[Value]) {
    let (kind, arg) = step.split_once(':').unwrap_or((step, ""));
    let each = |items: &[Value]| -> Vec<Value> { items.to_vec() };
    match kind {
        "repo" => {
            if let Some(v) = items.first()
                && let Ok(r) = serde_json::from_value::<Repository>(v.clone())
            {
                meta.repo = Some(r);
            }
        }
        "labels" => {
            for v in each(items) {
                if let Ok(l) = serde_json::from_value::<Label>(v) {
                    meta.labels.insert(l.name.clone(), l);
                }
            }
        }
        "milestones" => {
            for v in each(items) {
                if let Ok(m) = serde_json::from_value::<Milestone>(v) {
                    meta.milestones.insert(m.number, m);
                }
            }
        }
        "issues" | "issues_since" => {
            for v in each(items) {
                if let Ok(i) = serde_json::from_value::<Issue>(v) {
                    meta.put_issue_item(i);
                }
            }
        }
        "pulls" | "pull" => {
            for v in each(items) {
                if let Ok(p) = serde_json::from_value::<PullRequest>(v) {
                    meta.pulls.insert(p.number, p);
                }
            }
        }
        "review_comments" | "review_comments_since" => {
            for v in each(items) {
                if let Ok(c) = serde_json::from_value::<ReviewComment>(v) {
                    meta.review_comments.insert(c.id, c);
                }
            }
        }
        "issue_comments" | "issue_comments_since" => {
            for v in each(items) {
                if let Ok(c) = serde_json::from_value::<IssueComment>(v) {
                    meta.issue_comments.insert(c.id, c);
                }
            }
        }
        "releases" => {
            for v in each(items) {
                if let Ok(r) = serde_json::from_value::<Release>(v) {
                    meta.releases.insert(r.id, r);
                }
            }
        }
        "branches" => {
            for v in each(items) {
                if let Ok(b) = serde_json::from_value::<Branch>(v) {
                    meta.protected_branches.insert(b.name);
                }
            }
        }
        "protection" => {
            if let Some(v) = items.first()
                && let Ok(p) = serde_json::from_value::<Protection>(v.clone())
            {
                meta.protections.insert(arg.to_string(), p);
            }
        }
        "hooks" => {
            for v in each(items) {
                if let Ok(h) = serde_json::from_value::<Hook>(v) {
                    meta.hooks.insert(h.id, h);
                }
            }
        }
        "secrets" => {
            for v in items {
                if let Some(n) = v["name"].as_str() {
                    meta.named.insert(NamedItem {
                        kind: "actions_secret".into(),
                        name: n.to_string(),
                    });
                }
            }
        }
        "keys" => {
            for v in items {
                if let Some(n) = v["title"].as_str() {
                    meta.named.insert(NamedItem {
                        kind: "deploy_key".into(),
                        name: n.to_string(),
                    });
                }
            }
        }
        "unreadable" => {
            if let Some(v) = items.first() {
                meta.unreadable.insert(
                    arg.to_string(),
                    v["error"].as_str().unwrap_or("unreadable").to_string(),
                );
            }
        }
        "timeline" => {
            for v in items {
                let Some(n) = v["pr"].as_u64() else { continue };
                if let Ok(f) = serde_json::from_value::<Vec<ForcePush>>(v["force_pushes"].clone()) {
                    meta.force_pushes.insert(n, f);
                }
                if let Ok(rs) = serde_json::from_value::<Vec<Review>>(v["reviews"].clone()) {
                    let e = meta.reviews.entry(n).or_default();
                    for r in rs {
                        e.insert(r.id, r);
                    }
                }
            }
        }
        "reviews" => {
            if let Ok(n) = arg.parse::<u64>() {
                for v in each(items) {
                    if let Ok(r) = serde_json::from_value::<Review>(v) {
                        meta.reviews.entry(n).or_default().insert(r.id, r);
                    }
                }
            }
        }
        "reactions" => {
            for v in each(items) {
                if let Ok(r) = serde_json::from_value::<Reaction>(v) {
                    meta.reactions
                        .entry(arg.to_string())
                        .or_default()
                        .insert(r.id, r);
                }
            }
        }
        _ => {}
    }
}

/// Replays a journal into metadata.
pub fn replay_into(meta: &mut Metadata, replay: &Replay) {
    for (step, items) in &replay.items {
        apply(meta, step, items);
    }
}

/// Runs the backfill steps against GitHub, journaling as it goes.
pub struct Fetcher<'a> {
    client: &'a Client,
    state: &'a StateDir,
    owner: String,
    repo: String,
    /// Steps already finished (from the journal).
    pub done: BTreeSet<String>,
    /// Saved cursors (from the journal).
    pub cursors: BTreeMap<String, Option<String>>,
    /// The metadata being built.
    pub meta: Metadata,
}

/// Steps whose data the org migration archive already provides.
pub const ARCHIVE_STEPS: [&str; 7] = [
    "labels",
    "milestones",
    "issues",
    "pulls",
    "review_comments",
    "issue_comments",
    "releases",
];

impl<'a> Fetcher<'a> {
    /// A fetcher resuming from `replay`.
    pub fn new(
        client: &'a Client,
        state: &'a StateDir,
        full_name: &str,
        replay: &Replay,
        meta: Metadata,
    ) -> Fetcher<'a> {
        let (owner, repo) = full_name.split_once('/').unwrap_or((full_name, ""));
        Fetcher {
            client,
            state,
            owner: owner.to_string(),
            repo: repo.to_string(),
            done: replay.done.clone(),
            cursors: replay.cursors.clone(),
            meta,
        }
    }

    fn base(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.repo)
    }

    fn record(&mut self, entries: Vec<Entry>) -> Result<()> {
        self.state.append(&entries)?;
        for e in entries {
            match e {
                Entry::Items { step, items } => apply(&mut self.meta, &step, &items),
                Entry::Cursor { step, next } => {
                    self.cursors.insert(step, next);
                }
                Entry::Done { step } => {
                    self.done.insert(step);
                }
                Entry::Applied { .. } => {}
            }
        }
        Ok(())
    }

    /// Marks a step done without fetching (data came from elsewhere).
    pub fn mark_done(&mut self, step: &str) -> Result<()> {
        if self.done.contains(step) {
            return Ok(());
        }
        self.record(vec![Entry::Done { step: step.into() }])
    }

    /// Fetches the repository object.
    pub async fn repo(&mut self) -> Result<Repository> {
        if !self.done.contains("repo") || self.meta.repo.is_none() {
            let v = self.client.get_json(&self.base()).await?;
            self.record(vec![
                Entry::Items {
                    step: "repo".into(),
                    items: vec![v],
                },
                Entry::Done {
                    step: "repo".into(),
                },
            ])?;
        }
        self.meta
            .repo
            .clone()
            .ok_or_else(|| crate::error::bad_json("repository", "missing"))
    }

    /// Pages through `path` as `step`, resuming at the saved cursor.
    pub async fn list(&mut self, step: &str, path: &str) -> Result<()> {
        if self.done.contains(step) {
            return Ok(());
        }
        let mut next = self
            .cursors
            .get(step)
            .cloned()
            .flatten()
            .unwrap_or_else(|| self.client.list_url(path));
        loop {
            let page = self.client.get_page(&next).await?;
            let more = page.next.clone();
            self.record(vec![
                Entry::Items {
                    step: step.into(),
                    items: page.items,
                },
                Entry::Cursor {
                    step: step.into(),
                    next: more.clone(),
                },
            ])?;
            match more {
                Some(n) => next = n,
                None => break,
            }
        }
        self.record(vec![Entry::Done { step: step.into() }])
    }

    /// Like [`Fetcher::list`], but a permission or not-found error marks the section
    /// unreadable instead of failing.
    pub async fn optional_list(&mut self, step: &str, path: &str) -> Result<()> {
        if self.done.contains(step) {
            return Ok(());
        }
        match self.list(step, path).await {
            Ok(()) => Ok(()),
            Err(e) if is_access_error(&e) => self.unreadable(step, &e.message),
            Err(e) => Err(e),
        }
    }

    fn unreadable(&mut self, step: &str, why: &str) -> Result<()> {
        self.record(vec![
            Entry::Items {
                step: format!("unreadable:{step}"),
                items: vec![json!({ "error": why })],
            },
            Entry::Done { step: step.into() },
        ])
    }

    /// The list steps (everything except per-PR and per-subject data).
    pub async fn lists(&mut self, skip: &[&str]) -> Result<()> {
        let b = self.base();
        let steps: [(&str, String); 7] = [
            ("labels", format!("{b}/labels")),
            ("milestones", format!("{b}/milestones?state=all")),
            (
                "issues",
                format!("{b}/issues?state=all&sort=created&direction=asc"),
            ),
            (
                "pulls",
                format!("{b}/pulls?state=all&sort=created&direction=asc"),
            ),
            (
                "review_comments",
                format!("{b}/pulls/comments?sort=created&direction=asc"),
            ),
            (
                "issue_comments",
                format!("{b}/issues/comments?sort=created&direction=asc"),
            ),
            ("releases", format!("{b}/releases")),
        ];
        for (step, path) in steps {
            if skip.contains(&step) {
                self.mark_done(step)?;
            } else {
                self.list(step, &path).await?;
            }
        }
        self.optional_list("branches", &format!("{b}/branches?protected=true"))
            .await?;
        let protected: Vec<String> = self.meta.protected_branches.iter().cloned().collect();
        for br in protected {
            let step = format!("protection:{br}");
            if self.done.contains(&step) {
                continue;
            }
            match self
                .client
                .get_json(&format!("{b}/branches/{}/protection", encode_path(&br)))
                .await
            {
                Ok(v) => self.record(vec![
                    Entry::Items {
                        step: step.clone(),
                        items: vec![v],
                    },
                    Entry::Done { step },
                ])?,
                Err(e) if is_access_error(&e) => self.unreadable(&step, &e.message)?,
                Err(e) => return Err(e),
            }
        }
        self.optional_list("hooks", &format!("{b}/hooks")).await?;
        self.secrets().await?;
        self.optional_list("keys", &format!("{b}/keys")).await?;
        Ok(())
    }

    async fn secrets(&mut self) -> Result<()> {
        if self.done.contains("secrets") {
            return Ok(());
        }
        let path = self
            .client
            .list_url(&format!("{}/actions/secrets", self.base()));
        match self.client.get_json(&path).await {
            Ok(v) => {
                let items = v["secrets"].as_array().cloned().unwrap_or_default();
                self.record(vec![
                    Entry::Items {
                        step: "secrets".into(),
                        items,
                    },
                    Entry::Done {
                        step: "secrets".into(),
                    },
                ])
            }
            Err(e) if is_access_error(&e) => self.unreadable("secrets", &e.message),
            Err(e) => Err(e),
        }
    }

    /// Force-push events and reviews of `numbers` (or of every PR), in GraphQL batches.
    pub async fn timelines(&mut self, numbers: Option<&[u64]>, force: bool) -> Result<()> {
        let all: Vec<u64> = match numbers {
            Some(n) => n.to_vec(),
            None => self.meta.pulls.keys().copied().collect(),
        };
        for chunk in all.chunks(TIMELINE_BATCH) {
            let step = format!("timeline:{}", chunk[0]);
            if !force && self.done.contains(&step) {
                continue;
            }
            let items = self.timeline_batch(chunk).await?;
            let mut entries = vec![Entry::Items {
                step: "timeline".into(),
                items,
            }];
            if !force {
                entries.push(Entry::Done { step });
            }
            self.record(entries)?;
        }
        Ok(())
    }

    async fn timeline_batch(&self, numbers: &[u64]) -> Result<Vec<Value>> {
        let mut q = String::from(
            "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) {",
        );
        for n in numbers {
            q.push_str(&format!(
                " p{n}: pullRequest(number: {n}) {{ number \
                 timelineItems(first: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT]) {{ \
                 pageInfo {{ hasNextPage }} \
                 nodes {{ ... on HeadRefForcePushedEvent {{ createdAt actor {{ login }} \
                 beforeCommit {{ oid }} afterCommit {{ oid }} }} }} }} \
                 reviews(first: 100) {{ pageInfo {{ hasNextPage }} \
                 nodes {{ databaseId body state submittedAt commit {{ oid }} \
                 author {{ __typename login ... on User {{ databaseId }} ... on Bot {{ databaseId }} }} }} }} }}"
            ));
        }
        q.push_str(" } }");
        let data = self
            .client
            .graphql(&q, json!({ "owner": self.owner, "name": self.repo }))
            .await?;
        let mut out = Vec::new();
        for n in numbers {
            let pr = &data["repository"][format!("p{n}")];
            if pr.is_null() {
                continue;
            }
            let mut pushes = Vec::new();
            for node in pr["timelineItems"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if node.get("createdAt").is_none() {
                    continue;
                }
                pushes.push(ForcePush {
                    at: node["createdAt"].as_str().unwrap_or("").to_string(),
                    before: node["beforeCommit"]["oid"].as_str().map(str::to_string),
                    after: node["afterCommit"]["oid"].as_str().map(str::to_string),
                    actor: node["actor"]["login"].as_str().map(str::to_string),
                });
            }
            pushes.sort();
            let mut reviews = Vec::new();
            for node in pr["reviews"]["nodes"].as_array().into_iter().flatten() {
                let Some(id) = node["databaseId"].as_u64() else {
                    continue;
                };
                let a = &node["author"];
                let user = a["databaseId"].as_u64().map(|uid| User {
                    id: uid,
                    login: a["login"].as_str().unwrap_or("").to_string(),
                    kind: a["__typename"].as_str().unwrap_or("User").to_string(),
                    name: None,
                });
                reviews.push(Review {
                    id,
                    user,
                    body: node["body"].as_str().map(str::to_string),
                    state: node["state"].as_str().unwrap_or("").to_string(),
                    commit_id: node["commit"]["oid"].as_str().map(str::to_string),
                    submitted_at: node["submittedAt"].as_str().map(str::to_string),
                });
            }
            let more_reviews = pr["reviews"]["pageInfo"]["hasNextPage"].as_bool() == Some(true);
            if more_reviews {
                // Over 100 reviews: the REST list has them all.
                let rest = self
                    .client
                    .get_all(&format!("{}/pulls/{n}/reviews", self.base()))
                    .await?;
                reviews = rest
                    .into_iter()
                    .filter_map(|v| decode::<Review>("review", &v).ok())
                    .collect();
            }
            out.push(json!({ "pr": n, "force_pushes": pushes, "reviews": reviews }));
        }
        Ok(out)
    }

    /// Per-user reactions of every subject whose summary says it has some.
    pub async fn reactions(&mut self) -> Result<()> {
        let b = self.base();
        let mut subjects: Vec<(String, String)> = Vec::new();
        for (n, i) in &self.meta.issues {
            if i.reactions.as_ref().is_some_and(|r| r.total_count > 0) {
                subjects.push((format!("issue:{n}"), format!("{b}/issues/{n}/reactions")));
            }
        }
        for (n, total) in &self.meta.pr_reactions {
            if *total > 0 {
                subjects.push((format!("issue:{n}"), format!("{b}/issues/{n}/reactions")));
            }
        }
        for (id, c) in &self.meta.issue_comments {
            if c.reactions.as_ref().is_some_and(|r| r.total_count > 0) {
                subjects.push((
                    format!("issue_comment:{id}"),
                    format!("{b}/issues/comments/{id}/reactions"),
                ));
            }
        }
        for (id, c) in &self.meta.review_comments {
            if c.reactions.as_ref().is_some_and(|r| r.total_count > 0) {
                subjects.push((
                    format!("review_comment:{id}"),
                    format!("{b}/pulls/comments/{id}/reactions"),
                ));
            }
        }
        subjects.sort();
        subjects.dedup();
        for (subject, path) in subjects {
            self.list(&format!("reactions:{subject}"), &path).await?;
        }
        Ok(())
    }

    /// Refetches one PR (live sync).
    pub async fn pull(&mut self, number: u64) -> Result<()> {
        let v = self
            .client
            .get_json(&format!("{}/pulls/{number}", self.base()))
            .await?;
        self.record(vec![Entry::Items {
            step: "pull".into(),
            items: vec![v],
        }])
    }

    /// Records items fetched elsewhere (polling, webhooks) under `step`.
    pub fn absorb(&mut self, step: &str, items: Vec<Value>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.record(vec![Entry::Items {
            step: step.into(),
            items,
        }])
    }

    /// The client.
    pub fn client(&self) -> &Client {
        self.client
    }
}

/// True for errors that mean "this credential cannot read that".
pub fn is_access_error(e: &bee_core::BeeError) -> bool {
    matches!(
        e.reason,
        bee_core::Reason::NotFound | bee_core::Reason::PermissionDenied
    )
}

/// Percent-encodes a branch name for a URL path segment (`/` kept encoded).
pub fn encode_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_is_idempotent() {
        let mut m = Metadata::default();
        let page = vec![
            json!({"number": 1, "title": "bug", "state": "open"}),
            json!({"number": 2, "title": "pr", "state": "open", "pull_request": {}, "reactions": {"total_count": 3}}),
        ];
        apply(&mut m, "issues", &page);
        apply(&mut m, "issues", &page);
        assert_eq!(m.issues.len(), 1);
        assert_eq!(m.pr_reactions[&2], 3);
        apply(
            &mut m,
            "timeline",
            &[
                json!({"pr": 2, "force_pushes": [{"at": "t", "before": "a", "after": "b", "actor": null}], "reviews": [{"id": 9, "state": "APPROVED"}]}),
            ],
        );
        assert_eq!(m.force_pushes[&2].len(), 1);
        assert_eq!(m.reviews[&2][&9].state, "APPROVED");
        apply(&mut m, "unreadable:hooks", &[json!({"error": "denied"})]);
        assert_eq!(m.unreadable["hooks"], "denied");
    }

    #[test]
    fn branch_names_are_encoded() {
        assert_eq!(encode_path("release/1.0"), "release%2F1.0");
    }
}
