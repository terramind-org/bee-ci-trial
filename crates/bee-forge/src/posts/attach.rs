//! Attachments: what a post is about, beside its media. Any number of change requests, and at
//! most one release, one issue and one agent run, all of the post's own repo
//! (`docs/spec/forge-community.md` "Posts and feeds"). `POST_ATTACHMENT_KIND_SWARM` is reserved
//! and refused until swarms exist.
//!
//! - **Writing.** [`Posts::resolve_attachments`] checks the list ([`check`], pure) and that
//!   every attachment exists in the post's repo. The author holds `commit` on that repo (the
//!   service checked), so what exists there is readable by them; a draft release is refused
//!   because the post's readers could not read it. Anything missing, in another repo or a
//!   draft is `INVALID_ARGUMENT`/`attachment_not_found`, so the error never tells a missing
//!   attachment from one elsewhere.
//! - **Stored.** Only `kind` and `id`, in the order given. The change request that carries a
//!   post is not stored in the list: [`refs_of`] puts it first on every read, which is also
//!   how a row written before attachments existed reads back with one attachment.
//! - **Reading.** [`Posts::attachments`] fills each card (number, title, state, tag) from the
//!   attachment as it is now: one point read per attachment, in one transaction per surface
//!   involved, so a client renders the cards without further calls. An attachment that is
//!   gone (or a release back in draft) keeps `kind` and `id` alone. Every attachment is in the
//!   post's repo, which the reader reads.

use std::collections::BTreeSet;

use bee_proto::v1::{
    AgentRun, AgentRunState, ChangeState, ErrorReason, IssueState, Post, PostAttachment,
    PostAttachmentKind,
};

use super::MAX_ATTACHMENTS;
use super::store::Posts;
use crate::agents::model::RunRec;
use crate::changes::Changes;
use crate::community::releases::{RELEASES, ReleaseRec};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::store::{Surface, Tx};

/// The subcode of an attachment that is not in the post's repo (or is not readable there).
pub const NOT_FOUND: &str = "attachment_not_found";

/// The subcode of an attachment list the rules refuse.
pub const BAD: &str = "bad_attachment";

/// Most characters of an agent run's prompt shown as its card title.
const RUN_TITLE_CHARS: usize = 120;

/// The kinds a post may attach, in the order their surfaces are read.
const KINDS: [PostAttachmentKind; 4] = [
    PostAttachmentKind::ChangeRequest,
    PostAttachmentKind::Release,
    PostAttachmentKind::Issue,
    PostAttachmentKind::AgentRun,
];

fn kind_of(a: &PostAttachment) -> PostAttachmentKind {
    PostAttachmentKind::try_from(a.kind).unwrap_or(PostAttachmentKind::Unspecified)
}

/// The surface the records of a kind live on.
fn surface(kind: PostAttachmentKind) -> Surface {
    match kind {
        PostAttachmentKind::Release => crate::community::SURFACE,
        PostAttachmentKind::Issue => crate::work::SURFACE,
        PostAttachmentKind::AgentRun => crate::agents::SURFACE,
        _ => crate::changes::SURFACE,
    }
}

fn bad(message: impl Into<String>) -> ForgeError {
    ForgeError::invalid(BAD, message)
}

/// What a kind is called in messages.
fn noun(kind: PostAttachmentKind) -> &'static str {
    match kind {
        PostAttachmentKind::ChangeRequest => "change request",
        PostAttachmentKind::Release => "release",
        PostAttachmentKind::Issue => "issue",
        PostAttachmentKind::AgentRun => "agent run",
        _ => "attachment",
    }
}

fn not_found(a: &PostAttachment) -> ForgeError {
    let what = noun(kind_of(a));
    let which = if a.id.trim().is_empty() {
        format!("#{}", a.number)
    } else {
        a.id.trim().to_string()
    };
    ForgeError::invalid(
        NOT_FOUND,
        format!("{what} {which} is not a {what} of this repository that you can read"),
    )
}

/// An enum value name as a card state: without its prefix, in lower case.
fn short(name: &str, prefix: &str) -> String {
    name.trim_start_matches(prefix).to_lowercase()
}

/// The card title of an agent run: the first line of its prompt, else its agent's name.
fn run_title(run: &AgentRun) -> String {
    let prompt = run.task.as_ref().map(|t| t.prompt.as_str()).unwrap_or("");
    match prompt.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => line.chars().take(RUN_TITLE_CHARS).collect(),
        None => run
            .agent
            .as_ref()
            .map(|a| a.display.clone())
            .unwrap_or_default(),
    }
}

fn check_count(count: usize) -> Result<()> {
    if count > MAX_ATTACHMENTS {
        return Err(bad(format!(
            "a post carries at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    Ok(())
}

/// Checks an attachment list as a writer sent it: at most [`MAX_ATTACHMENTS`], each of a kind
/// that can be attached, named by id (a change request or an issue also by number), and at
/// most one release, one issue and one agent run. `bad_attachment` otherwise.
pub fn check(input: &[PostAttachment]) -> Result<()> {
    check_count(input.len())?;
    let mut single = BTreeSet::new();
    for a in input {
        let kind = kind_of(a);
        match kind {
            PostAttachmentKind::Unspecified => {
                return Err(bad(
                    "an attachment needs a kind: a change request, a release, an issue or an \
                     agent run",
                ));
            }
            PostAttachmentKind::Swarm => {
                return Err(bad("swarms cannot be attached to a post yet"));
            }
            _ => {}
        }
        let numbered = matches!(
            kind,
            PostAttachmentKind::ChangeRequest | PostAttachmentKind::Issue
        );
        let named = !a.id.trim().is_empty() || (numbered && a.number != 0);
        if !named {
            return Err(bad(format!(
                "an attached {} needs its id{}",
                noun(kind),
                if numbered { " or number" } else { "" }
            )));
        }
        if kind != PostAttachmentKind::ChangeRequest && !single.insert(a.kind) {
            return Err(bad(
                "a post carries at most one release, one issue and one agent run",
            ));
        }
    }
    Ok(())
}

/// The attachments of a post as references (`kind` and `id`), in display order: the change
/// request that carries it first, then the stored list.
pub fn refs_of(post: &Post) -> Vec<PostAttachment> {
    let mut out = Vec::with_capacity(post.attachments.len() + 1);
    if !post.change_id.is_empty() {
        out.push(PostAttachment {
            kind: PostAttachmentKind::ChangeRequest as i32,
            id: post.change_id.clone(),
            ..Default::default()
        });
    }
    for a in &post.attachments {
        let own = a.kind == PostAttachmentKind::ChangeRequest as i32 && a.id == post.change_id;
        if !own {
            out.push(PostAttachment {
                kind: a.kind,
                id: a.id.clone(),
                ..Default::default()
            });
        }
    }
    out
}

/// The card of the attachment `a` names, as it is now in `repo_id`; `None` when the repo has
/// no such attachment (or it is a draft release). `tx` is on the surface of `a`'s kind.
async fn card(tx: &mut Tx, repo_id: &str, a: &PostAttachment) -> Result<Option<PostAttachment>> {
    let id = a.id.trim();
    Ok(match kind_of(a) {
        PostAttachmentKind::ChangeRequest => {
            let found = if id.is_empty() {
                Changes::load_number(tx, repo_id, a.number).await
            } else {
                Changes::load(tx, repo_id, id).await
            };
            match found {
                Ok((rec, _)) => {
                    let c = rec.cr();
                    let state = ChangeState::try_from(c.state).unwrap_or(ChangeState::Unspecified);
                    Some(PostAttachment {
                        kind: a.kind,
                        id: c.id.clone(),
                        number: c.number,
                        title: c.title.clone(),
                        state: short(state.as_str_name(), "CHANGE_STATE_"),
                        tag_name: String::new(),
                    })
                }
                Err(e) if e.reason == ErrorReason::NotFound => None,
                Err(e) => return Err(e),
            }
        }
        PostAttachmentKind::Issue => {
            let found = if id.is_empty() {
                crate::work::load_issue_by_number(tx, repo_id, a.number).await?
            } else {
                crate::work::load_issue_by_id(tx, id).await?
            };
            // An issue moved to another repo is no longer this repo's.
            found
                .filter(|(issue, _)| issue.repo_id == repo_id)
                .map(|(issue, _)| {
                    let state =
                        IssueState::try_from(issue.state).unwrap_or(IssueState::Unspecified);
                    PostAttachment {
                        kind: a.kind,
                        id: issue.id,
                        number: issue.number,
                        title: issue.title,
                        state: short(state.as_str_name(), "ISSUE_STATE_"),
                        tag_name: String::new(),
                    }
                })
        }
        PostAttachmentKind::Release => tx
            .get_msg::<ReleaseRec>(&RELEASES, repo_id, id)
            .await?
            .and_then(|(rec, row)| {
                let r = rec.release.unwrap_or_default();
                if r.draft {
                    return None;
                }
                let title = if r.name.is_empty() {
                    r.tag_name.clone()
                } else {
                    r.name
                };
                let state = if r.prerelease {
                    "prerelease"
                } else {
                    "published"
                };
                Some(PostAttachment {
                    kind: a.kind,
                    id: row.id,
                    number: 0,
                    title,
                    state: state.to_string(),
                    tag_name: r.tag_name,
                })
            }),
        PostAttachmentKind::AgentRun => tx
            .get_msg::<RunRec>(&crate::agents::RUNS, repo_id, id)
            .await?
            .map(|(rec, _)| {
                let run = rec.run();
                let state =
                    AgentRunState::try_from(run.state).unwrap_or(AgentRunState::Unspecified);
                PostAttachment {
                    kind: a.kind,
                    title: run_title(&run),
                    state: short(state.as_str_name(), "AGENT_RUN_STATE_"),
                    id: run.id,
                    number: 0,
                    tag_name: String::new(),
                }
            }),
        _ => None,
    })
}

impl Posts {
    /// The cards of `refs` in `repo_id`, position by position (`None`: not there). One
    /// transaction per surface involved, one after the other (a transaction holds its surface).
    async fn cards(
        &self,
        cx: &Cx,
        repo_id: &str,
        refs: &[PostAttachment],
    ) -> Result<Vec<Option<PostAttachment>>> {
        let mut out: Vec<Option<PostAttachment>> = vec![None; refs.len()];
        for kind in KINDS {
            if !refs.iter().any(|a| a.kind == kind as i32) {
                continue;
            }
            let mut tx = self.db.begin(surface(kind), cx).await?;
            for (i, a) in refs.iter().enumerate() {
                if a.kind == kind as i32 {
                    out[i] = card(&mut tx, repo_id, a).await?;
                }
            }
        }
        Ok(out)
    }

    /// Checks the attachments a writer sent for a post in `repo_id` and returns them as stored:
    /// `kind` and `id`, in the order given, without `own_change` (the change request that
    /// carries the post, which is always attached). `bad_attachment` for a list the rules
    /// refuse or that names one attachment twice, `attachment_not_found` for one that is not
    /// in the repo (see the module docs).
    pub(crate) async fn resolve_attachments(
        &self,
        cx: &Cx,
        repo_id: &str,
        input: &[PostAttachment],
        own_change: &str,
    ) -> Result<Vec<PostAttachment>> {
        check(input)?;
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let found = self.cards(cx, repo_id, input).await?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(input.len());
        for (a, card) in input.iter().zip(found) {
            let Some(card) = card else {
                return Err(not_found(a));
            };
            if !seen.insert((card.kind, card.id.clone())) {
                return Err(bad(format!(
                    "{} {} is attached twice",
                    noun(kind_of(a)),
                    card.id
                )));
            }
            if card.kind == PostAttachmentKind::ChangeRequest as i32 && card.id == own_change {
                continue;
            }
            out.push(PostAttachment {
                kind: card.kind,
                id: card.id,
                ..Default::default()
            });
        }
        check_count(out.len() + usize::from(!own_change.is_empty()))?;
        Ok(out)
    }

    /// The attachments of a post as a reader of its repo sees them: every reference of
    /// [`refs_of`] with its card as it is now, or the bare reference when it is gone.
    pub async fn attachments(&self, cx: &Cx, post: &Post) -> Result<Vec<PostAttachment>> {
        let refs = refs_of(post);
        if refs.is_empty() {
            return Ok(refs);
        }
        let repo_id = crate::community::common::ref_repo_id(&post.repo);
        let found = self.cards(cx, &repo_id, &refs).await?;
        Ok(refs
            .into_iter()
            .zip(found)
            .map(|(r, card)| card.unwrap_or(r))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(kind: PostAttachmentKind, id: &str) -> PostAttachment {
        PostAttachment {
            kind: kind as i32,
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_list_rules() {
        use PostAttachmentKind::*;
        assert!(check(&[]).is_ok());
        // Any number of change requests, one of each other kind.
        assert!(
            check(&[
                att(ChangeRequest, "c1"),
                att(ChangeRequest, "c2"),
                att(Release, "r1"),
                att(Issue, "i1"),
                att(AgentRun, "a1"),
            ])
            .is_ok()
        );
        for twice in [Release, Issue, AgentRun] {
            let e = check(&[att(twice, "x"), att(twice, "y")]).unwrap_err();
            assert_eq!(e.subcode, BAD, "{twice:?}");
        }
        // A change request and an issue may be named by number; the others need an id.
        let numbered = |kind: PostAttachmentKind| PostAttachment {
            kind: kind as i32,
            number: 7,
            ..Default::default()
        };
        assert!(check(&[numbered(ChangeRequest), numbered(Issue)]).is_ok());
        assert_eq!(check(&[numbered(Release)]).unwrap_err().subcode, BAD);
        assert_eq!(check(&[numbered(AgentRun)]).unwrap_err().subcode, BAD);
        assert_eq!(check(&[att(Issue, " ")]).unwrap_err().subcode, BAD);
        // No kind, an unknown kind, and the reserved swarm kind.
        assert_eq!(check(&[att(Unspecified, "x")]).unwrap_err().subcode, BAD);
        let unknown = PostAttachment {
            kind: 99,
            id: "x".into(),
            ..Default::default()
        };
        assert_eq!(check(&[unknown]).unwrap_err().subcode, BAD);
        let swarm = check(&[att(Swarm, "s1")]).unwrap_err();
        assert_eq!(swarm.reason, ErrorReason::InvalidArgument);
        assert_eq!(swarm.subcode, BAD);
        // The size limit.
        let many: Vec<PostAttachment> = (0..=MAX_ATTACHMENTS)
            .map(|i| att(ChangeRequest, &format!("c{i}")))
            .collect();
        assert_eq!(check(&many).unwrap_err().subcode, BAD);
        assert!(check(&many[..MAX_ATTACHMENTS]).is_ok());
    }

    #[test]
    fn a_post_on_a_change_request_lists_it_first() {
        use PostAttachmentKind::*;
        // A row written before attachments existed: its change request alone.
        let old = Post {
            change_id: "cr1".into(),
            ..Default::default()
        };
        assert_eq!(refs_of(&old), vec![att(ChangeRequest, "cr1")]);
        // A post of its own: the stored list, nothing added.
        let own = Post {
            attachments: vec![att(Issue, "i1"), att(ChangeRequest, "cr2")],
            ..Default::default()
        };
        assert_eq!(
            refs_of(&own),
            vec![att(Issue, "i1"), att(ChangeRequest, "cr2")]
        );
        assert!(refs_of(&Post::default()).is_empty());
        // The carrying change request is never listed twice, and cards are not stored.
        let both = Post {
            change_id: "cr1".into(),
            attachments: vec![
                att(ChangeRequest, "cr1"),
                PostAttachment {
                    title: "stale".into(),
                    ..att(Release, "r1")
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            refs_of(&both),
            vec![att(ChangeRequest, "cr1"), att(Release, "r1")]
        );
    }

    #[test]
    fn card_states_and_run_titles() {
        assert_eq!(
            short(ChangeState::Landed.as_str_name(), "CHANGE_STATE_"),
            "landed"
        );
        assert_eq!(
            short(
                AgentRunState::WaitingForReview.as_str_name(),
                "AGENT_RUN_STATE_"
            ),
            "waiting_for_review"
        );
        let mut run = AgentRun {
            agent: Some(bee_proto::v1::Principal {
                display: "builder".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(run_title(&run), "builder");
        run.task = Some(bee_proto::v1::AgentTask {
            prompt: format!("\n  Fix the flaky test  \n{}", "x".repeat(500)),
            ..Default::default()
        });
        assert_eq!(run_title(&run), "Fix the flaky test");
        run.task = Some(bee_proto::v1::AgentTask {
            prompt: "y".repeat(500),
            ..Default::default()
        });
        assert_eq!(run_title(&run).chars().count(), RUN_TITLE_CHARS);
    }
}
