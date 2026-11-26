//! Live sync and cutover (SPEC §17.1 step 5, §17.5).
//!
//! - **Webhooks.** [`Importer::webhook`] verifies `X-Hub-Signature-256` (HMAC-SHA256 of the
//!   body under the webhook secret), ignores deliveries it has seen (`X-GitHub-Delivery`),
//!   journals the changed object from the payload and places the records it touches. A
//!   push (or branch/tag create and delete) refetches and imports git. [`webhook_router`]
//!   serves it at `POST /webhooks/github`.
//! - **Polling.** [`Importer::sync`] repairs anything a webhook missed: conditional `since`
//!   queries over issues and both comment lists (a `304` costs no quota), the PRs they
//!   name, their timelines, and git. Re-running the import does the same.
//! - **Cutover.** [`Importer::cutover`] freezes the import (webhooks are refused from then
//!   on), optionally archives the GitHub repository so nothing can change underneath, runs
//!   a final sync, writes the redirect map and the final report.
//!
//! Deletions on GitHub after the import (a deleted comment) are not mirrored: the record
//! stays in Beehive, and the webhook result says it was ignored.

use std::collections::BTreeSet;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::error::{Result, denied, invalid, invalid_state};
use crate::import::{Importer, RunResult, Session};
use crate::model::Repository;
use crate::state::{DELIVERY_MEMORY, Phase};

/// A verified webhook delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    /// `X-GitHub-Delivery`.
    pub id: String,
    /// `X-GitHub-Event`.
    pub event: String,
    /// The payload.
    pub payload: Value,
}

/// What a sync or webhook did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncResult {
    /// Records placed again (`pr:12`, `issue:7`, ...).
    pub placed: Vec<String>,
    /// Git was refetched and imported.
    pub git_synced: bool,
    /// Nothing done, and why (`duplicate delivery`, `deletions are not mirrored`, ...).
    pub ignored: Option<String>,
}

/// Verifies a delivery's signature and parses it. `headers` are lower-case names.
pub fn verify(
    secret: &[u8],
    headers: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
) -> Result<Delivery> {
    let sig = headers("x-hub-signature-256")
        .ok_or_else(|| denied("the delivery has no X-Hub-Signature-256"))?;
    let hex = sig
        .strip_prefix("sha256=")
        .ok_or_else(|| denied("malformed X-Hub-Signature-256"))?;
    let want = crate::principals::unhex(hex).ok_or_else(|| denied("malformed signature"))?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .map_err(|_| invalid("invalid_secret", "bad webhook secret"))?;
    mac.update(body);
    mac.verify_slice(&want)
        .map_err(|_| denied("the webhook signature does not match"))?;
    let payload: Value = serde_json::from_slice(body)
        .map_err(|e| invalid("bad_json", format!("the webhook body is not JSON: {e}")))?;
    Ok(Delivery {
        id: headers("x-github-delivery").unwrap_or_default(),
        event: headers("x-github-event").unwrap_or_default(),
        payload,
    })
}

/// The signature header value GitHub would send for `body` (tests, relays).
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("hmac takes any key");
    mac.update(body);
    format!(
        "sha256={}",
        crate::principals::hex(&mac.finalize().into_bytes())
    )
}

fn upper(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(s) = v["state"].as_str() {
        v["state"] = Value::String(s.to_ascii_uppercase());
    }
    v
}

impl Importer {
    async fn resync_git(&self, s: &mut Session<'_>, repo: &Repository) -> Result<()> {
        self.git_phase(s, repo).await.map(|_| ())
    }

    async fn replace_changed(
        &self,
        s: &mut Session<'_>,
        repo: &Repository,
        prs: &BTreeSet<u64>,
    ) -> Result<RunResult> {
        let present = self.revision_commits(s, repo, Some(prs)).await?;
        let git = crate::target::merge_reports(
            None,
            std::fs::read_to_string(self.state().path("git-report.json"))
                .ok()
                .and_then(|t| bee_import::ImportReport::from_json(&t).ok())
                .unwrap_or_else(|| {
                    crate::target::empty_report(&format!("github:{}", self.options().repo))
                }),
        );
        let policy = self.place_records(s, repo, &present, &git).await?;
        let phase = s.cp.phase.max(Phase::Live);
        self.finish(s, repo, git, policy, phase, phase.as_str())
    }

    /// Handles one webhook delivery (already verified with [`verify`]).
    pub async fn webhook(&self, d: &Delivery) -> Result<SyncResult> {
        let _g = self.guard().await;
        let mut s = self.session()?;
        if s.cp.phase >= Phase::Frozen {
            return Err(invalid_state(
                "the import is frozen for cutover; GitHub changes are no longer followed",
            ));
        }
        if !d.id.is_empty() && s.cp.live.deliveries.contains(&d.id) {
            return Ok(SyncResult {
                ignored: Some("duplicate delivery".into()),
                ..SyncResult::default()
            });
        }
        let p = &d.payload;
        let action = p["action"].as_str().unwrap_or("");
        let mut prs: BTreeSet<u64> = BTreeSet::new();
        let mut git = false;
        let mut ignored = None;
        if action == "deleted" {
            ignored = Some("deletions on GitHub are not mirrored".to_string());
        } else {
            match d.event.as_str() {
                "ping" => ignored = Some("ping".into()),
                "push" | "create" | "delete" => git = true,
                "pull_request" => {
                    s.fetcher.absorb("pull", vec![p["pull_request"].clone()])?;
                    if let Some(n) = p["pull_request"]["number"].as_u64() {
                        prs.insert(n);
                    }
                    git = matches!(action, "opened" | "synchronize" | "reopened" | "closed");
                }
                "issues" => s.fetcher.absorb("issues", vec![p["issue"].clone()])?,
                "issue_comment" => {
                    let mut c = p["comment"].clone();
                    if c["issue_url"].is_null() {
                        c["issue_url"] = p["issue"]["url"].clone();
                    }
                    s.fetcher.absorb("issue_comments", vec![c])?;
                    if p["issue"]["pull_request"].is_object()
                        && let Some(n) = p["issue"]["number"].as_u64()
                    {
                        prs.insert(n);
                    }
                }
                "pull_request_review" => {
                    if let Some(n) = p["pull_request"]["number"].as_u64() {
                        s.fetcher
                            .absorb(&format!("reviews:{n}"), vec![upper(&p["review"])])?;
                        prs.insert(n);
                    }
                }
                "pull_request_review_comment" => {
                    s.fetcher
                        .absorb("review_comments", vec![p["comment"].clone()])?;
                    if let Some(n) = p["pull_request"]["number"].as_u64() {
                        prs.insert(n);
                    }
                }
                "release" => s.fetcher.absorb("releases", vec![p["release"].clone()])?,
                "label" => s.fetcher.absorb("labels", vec![p["label"].clone()])?,
                "milestone" => s
                    .fetcher
                    .absorb("milestones", vec![p["milestone"].clone()])?,
                other => ignored = Some(format!("event {other} is not mirrored")),
            }
        }
        if !d.id.is_empty() {
            s.cp.live.deliveries.push_back(d.id.clone());
            while s.cp.live.deliveries.len() > DELIVERY_MEMORY {
                s.cp.live.deliveries.pop_front();
            }
        }
        if ignored.is_some() {
            self.save(&mut s)?;
            return Ok(SyncResult {
                ignored,
                ..SyncResult::default()
            });
        }
        let repo = s.fetcher.repo().await?;
        if !prs.is_empty() {
            let list: Vec<u64> = prs.iter().copied().collect();
            s.fetcher.timelines(Some(&list), true).await?;
        }
        if git {
            self.resync_git(&mut s, &repo).await?;
        }
        s.fetcher.meta.collect_users();
        let r = self.replace_changed(&mut s, &repo, &prs).await?;
        Ok(SyncResult {
            placed: r.placed,
            git_synced: git,
            ignored: None,
        })
    }

    /// One polling pass (the webhook fallback).
    pub async fn sync(&self) -> Result<SyncResult> {
        let _g = self.guard().await;
        let mut s = self.session()?;
        if s.cp.phase < Phase::Mirror {
            return Err(invalid_state("run the import before syncing it"));
        }
        let r = self.poll(&mut s).await?;
        if s.cp.phase < Phase::Live {
            s.cp.phase = Phase::Live;
            self.save(&mut s)?;
        }
        Ok(r)
    }

    async fn poll(&self, s: &mut Session<'_>) -> Result<SyncResult> {
        let repo = s.fetcher.repo().await?;
        let since = s.cp.live.since.clone().unwrap_or_else(|| {
            let m = &s.fetcher.meta;
            m.issues
                .values()
                .map(|i| i.updated_at.clone())
                .chain(m.pulls.values().map(|p| p.updated_at.clone()))
                .max()
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
        });
        let base = format!("/repos/{}", self.options().repo);
        let mut newest = since.clone();
        let mut prs: BTreeSet<u64> = BTreeSet::new();
        for (step, path) in [
            (
                "issues_since",
                format!("{base}/issues?state=all&sort=updated&direction=asc&since={since}"),
            ),
            (
                "issue_comments_since",
                format!("{base}/issues/comments?sort=updated&direction=asc&since={since}"),
            ),
            (
                "review_comments_since",
                format!("{base}/pulls/comments?sort=updated&direction=asc&since={since}"),
            ),
        ] {
            let mut next = Some(self.client().list_url(&path));
            while let Some(url) = next {
                let page = self.client().get_conditional(&url).await?;
                next = page.next.clone();
                if page.not_modified {
                    continue;
                }
                for it in &page.items {
                    if let Some(u) = it["updated_at"].as_str()
                        && u > newest.as_str()
                    {
                        newest = u.to_string();
                    }
                    match step {
                        "issues_since" if it["pull_request"].is_object() => {
                            prs.extend(it["number"].as_u64());
                        }
                        "issue_comments_since" => {
                            if let Some(n) =
                                it["issue_url"].as_str().and_then(crate::model::last_number)
                                && s.fetcher.meta.pulls.contains_key(&n)
                            {
                                prs.insert(n);
                            }
                        }
                        "review_comments_since" => {
                            prs.extend(
                                it["pull_request_url"]
                                    .as_str()
                                    .and_then(crate::model::last_number),
                            );
                        }
                        _ => {}
                    }
                }
                s.fetcher.absorb(step, page.items)?;
            }
        }
        for n in &prs {
            s.fetcher.pull(*n).await?;
        }
        if !prs.is_empty() {
            let list: Vec<u64> = prs.iter().copied().collect();
            s.fetcher.timelines(Some(&list), true).await?;
        }
        self.resync_git(s, &repo).await?;
        s.fetcher.meta.collect_users();
        s.cp.live.since = Some(newest);
        let r = self.replace_changed(s, &repo, &prs).await?;
        Ok(SyncResult {
            placed: r.placed,
            git_synced: true,
            ignored: None,
        })
    }

    /// Cuts over: freeze, (optionally) archive the GitHub repository, final sync, redirect
    /// map and final report. GitHub stays (archived, not deleted) with its history.
    pub async fn cutover(&self, archive_github: bool) -> Result<RunResult> {
        let _g = self.guard().await;
        let mut s = self.session()?;
        if s.cp.phase < Phase::Mirror {
            return Err(invalid_state("run the import before cutting over"));
        }
        s.cp.phase = s.cp.phase.max(Phase::Frozen);
        self.save(&mut s)?;
        if archive_github {
            self.client()
                .patch_json(
                    &format!("/repos/{}", self.options().repo),
                    &serde_json::json!({ "archived": true }),
                )
                .await?;
        }
        self.poll(&mut s).await?;
        let repo = s.fetcher.repo().await?;
        let mut r = self
            .replace_changed(&mut s, &repo, &BTreeSet::new())
            .await?;
        self.sink().redirects(&r.redirects).await?;
        if !archive_github {
            r.report.github.manual_steps.push(format!(
                "archive https://github.com/{} on GitHub (Settings → Archive) and point its description at Beehive",
                self.options().repo
            ));
        }
        s.cp.phase = Phase::CutOver;
        r.report.github.phase = Phase::CutOver.as_str().into();
        let outcomes: Vec<_> = s
            .applied
            .values()
            .flat_map(|(_, o)| o.iter().cloned())
            .collect();
        r.report = crate::report::FidelityReport::build(
            r.report.git.clone(),
            &outcomes,
            r.report.github.clone(),
        );
        self.state()
            .write_atomic("report.json", r.report.to_json().as_bytes())?;
        self.save(&mut s)?;
        r.phase = Phase::CutOver;
        Ok(r)
    }
}

/// State for [`webhook_router`].
#[derive(Clone)]
pub struct WebhookApp {
    /// The importer.
    pub importer: Arc<Importer>,
    /// The webhook secret.
    pub secret: Arc<Vec<u8>>,
}

/// An axum router serving `POST /webhooks/github`: `200` with the [`SyncResult`], `401` for
/// a bad signature, `409` once frozen, `503` when GitHub or the target failed (GitHub
/// redelivers).
pub fn webhook_router(app: WebhookApp) -> axum::Router {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    async fn handle(
        State(app): State<WebhookApp>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let get = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let d = match verify(&app.secret, &get, &body) {
            Ok(d) => d,
            Err(e) => {
                return (StatusCode::UNAUTHORIZED, e.message).into_response();
            }
        };
        match app.importer.webhook(&d).await {
            Ok(r) => (
                StatusCode::OK,
                serde_json::to_string(&r).unwrap_or_default(),
            )
                .into_response(),
            Err(e) if e.reason == bee_core::Reason::InvalidState => {
                (StatusCode::CONFLICT, e.message).into_response()
            }
            Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.message).into_response(),
        }
    }
    axum::Router::new()
        .route("/webhooks/github", axum::routing::post(handle))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify() {
        let body = br#"{"action":"opened"}"#;
        let sig = sign(b"s3cret", body);
        let h = |k: &str| match k {
            "x-hub-signature-256" => Some(sig.clone()),
            "x-github-event" => Some("issues".into()),
            "x-github-delivery" => Some("d1".into()),
            _ => None,
        };
        let d = verify(b"s3cret", &h, body).unwrap();
        assert_eq!(d.event, "issues");
        assert_eq!(d.id, "d1");
        let e = verify(b"other", &h, body).unwrap_err();
        assert_eq!(e.reason, bee_core::Reason::PermissionDenied);
    }
}
