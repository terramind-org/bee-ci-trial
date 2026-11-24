//! Port implementations over the other forge domains.
//!
//! - [`WorkIssues`] backs [`IssueBridge`] with the work domain: conversions read and open real
//!   issues, and converting an issue to a discussion comments on it and closes it.
//!   [`crate::forge::Forge::open`] installs it unless the hooks bring their own bridge.
//! - [`ChangesSource`] backs [`ChangeSource`] with the changes domain: generated release notes
//!   list the change requests the changes store knows as landed, decided by ancestry in the
//!   changes VCS. The server builds it from its [`Changes`] and puts it in
//!   `Hooks::community`.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ChangeState, ObjectId, Principal, Repo};

use crate::changes::Changes;
use crate::changes::query::ListFilter;
use crate::cx::Cx;
use crate::error::Result;
use crate::ids::ts_ms;
use crate::work::{Actor, NewIssue, RepoInfo, Work};

use super::ports::{ChangeSource, IssueBridge, IssueSnapshot, LandedChange};

/// Most landed change requests one notes generation reads.
pub const MAX_LANDED: usize = 5000;

/// [`IssueBridge`] over the work domain.
pub struct WorkIssues(pub Arc<Work>);

fn actor_of(p: &Principal) -> Actor {
    Actor {
        principal: p.clone(),
        triage: true,
        admin: false,
        association: "MEMBER".into(),
    }
}

#[async_trait]
impl IssueBridge for WorkIssues {
    async fn get_issue(&self, repo: &Repo, number: u64) -> Result<IssueSnapshot> {
        let i = self
            .0
            .get_issue_by_number(&Cx::system(), &RepoInfo::from(repo), number)
            .await?;
        Ok(IssueSnapshot {
            number: i.number,
            title: i.title,
            body: i.body,
            author: i.author.unwrap_or_default(),
            labels: i.labels.into_iter().map(|l| l.name).collect(),
        })
    }

    async fn create_issue(
        &self,
        repo: &Repo,
        title: &str,
        body: &str,
        author: &Principal,
        labels: &[String],
    ) -> Result<u64> {
        let issue = self
            .0
            .create_issue(
                &Cx::system(),
                &RepoInfo::from(repo),
                &actor_of(author),
                NewIssue {
                    title: title.into(),
                    body: body.into(),
                    labels: labels.to_vec(),
                    ..Default::default()
                },
            )
            .await?;
        Ok(issue.number)
    }

    async fn converted_to_discussion(
        &self,
        repo: &Repo,
        number: u64,
        discussion_url: &str,
        by: &Principal,
    ) -> Result<()> {
        self.0
            .create_comment(
                &Cx::system(),
                &RepoInfo::from(repo),
                &actor_of(by),
                number,
                &format!("Converted to a discussion: {discussion_url}"),
                "",
                true,
                false,
                &Default::default(),
            )
            .await
            .map(|_| ())
    }
}

/// [`ChangeSource`] over the changes domain.
pub struct ChangesSource(pub Arc<Changes>);

#[async_trait]
impl ChangeSource for ChangesSource {
    async fn landed_between(
        &self,
        repo: &Repo,
        since: Option<&ObjectId>,
        until: &ObjectId,
    ) -> Result<Vec<LandedChange>> {
        let cx = Cx::system();
        let filter = ListFilter {
            state: ChangeState::Landed,
            ..Default::default()
        };
        let (mut token, mut all) = (String::new(), Vec::new());
        loop {
            let (page, next) = self.0.list(&cx, repo, &filter, &token, 500).await?;
            all.extend(page);
            if next.is_empty() || all.len() >= MAX_LANDED {
                break;
            }
            token = next;
        }
        let vcs = self.0.vcs();
        let mut out = Vec::new();
        for cr in all {
            let Some(at) = cr.landed_ref.as_ref().and_then(|r| r.target.clone()) else {
                continue;
            };
            if !vcs.is_ancestor(repo, &at, until).await? {
                continue;
            }
            if let Some(s) = since
                && vcs.is_ancestor(repo, &at, s).await?
            {
                continue;
            }
            out.push(LandedChange {
                id: cr.id,
                number: cr.number,
                title: cr.title,
                author: cr.author.unwrap_or_default(),
                labels: cr.labels.into_iter().map(|l| l.name).collect(),
                landed_at_ms: cr.landed_at.as_ref().map(ts_ms).unwrap_or(0),
            });
        }
        out.sort_by_key(|c| (c.landed_at_ms, c.number));
        Ok(out)
    }
}
