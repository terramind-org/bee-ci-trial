//! Adoptions: the issues and change requests that name a post as their inspiration
//! (`CreateIssueRequest.inspired_by_post_id`, `OpenChangeRequest.inspired_by_post_id`;
//! `docs/spec/forge-community.md` "Posts and feeds").
//!
//! - **Linking.** [`Posts::inspiration`] accepts a post only when it is `PUBLISHED`, its repo
//!   is live, and every principal of the caller's chain may read that repo (it is public, or
//!   each holds a role there). The token's repo restriction is not consulted: a link reveals
//!   nothing the principals could not read, and an agent run's token is limited to the repo
//!   it works in. Anything else is `INVALID_ARGUMENT`/`post_not_readable`, so the error never
//!   tells a missing post from an unreadable one.
//! - **Recording.** The transaction that opens the issue or change request writes the
//!   adoption row on its own surface ([`record`]) and emits `post.adopted` in the adopting
//!   repo; the event names the post by id only, so readers of the adopting repo learn nothing
//!   about the source repo.
//! - **Reading.** An adoption row follows its issue across transfers and is deleted with it
//!   ([`Posts::adoptions`]); listings refresh numbers and titles from the subjects
//!   ([`Posts::live`]). Who may see which
//!   adoption, and the `inspired_by` of a subject, is decided per caller by the service.

use std::collections::BTreeMap;

use bee_proto::v1::{
    Event, PostRef, PostState, Principal, Repo, RepoRef, RepoRole, SubjectKind, SubjectRef,
    Visibility, repo_ref,
};

use super::store::Posts;
use super::{CHANGE_ADOPTIONS, ISSUE_ADOPTIONS};
use crate::audit::AuditDraft;
use crate::changes::Changes;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::orgs::{Orgs, role_rank};
use crate::store::{Key, Row, Scan, TableDef, Tx};

/// Most adoptions of one post read per subject kind (older ones are not listed or counted).
pub const MAX_ADOPTIONS: usize = 1000;

/// The subcode of a refused inspiration.
pub const NOT_READABLE: &str = "post_not_readable";

/// One adoption row: what was adopted, by which subject. The subject's repo, number and title
/// are read live; the copies here are what it had when it was opened.
#[derive(Clone, PartialEq, prost::Message)]
pub struct AdoptionRec {
    /// The post, as a reference (its repo by id).
    #[prost(message, optional, tag = "1")]
    pub post: Option<PostRef>,
    /// Repo of the issue or change request.
    #[prost(string, tag = "2")]
    pub repo_id: String,
    /// Org of that repo.
    #[prost(string, tag = "3")]
    pub org_id: String,
    /// `SubjectKind::Issue` or `SubjectKind::ChangeRequest`.
    #[prost(int32, tag = "4")]
    pub kind: i32,
    /// The issue or change request id.
    #[prost(string, tag = "5")]
    pub subject_id: String,
    /// Its number.
    #[prost(uint64, tag = "6")]
    pub number: u64,
    /// Its title.
    #[prost(string, tag = "7")]
    pub title: String,
    /// Who opened it.
    #[prost(message, optional, tag = "8")]
    pub author: Option<Principal>,
    /// When (ms).
    #[prost(uint64, tag = "9")]
    pub created_ms: u64,
}

impl AdoptionRec {
    /// The id of the adopted post.
    pub fn post_id(&self) -> &str {
        self.post.as_ref().map(|p| p.post_id.as_str()).unwrap_or("")
    }

    /// The repo id of the adopted post.
    pub fn source_repo_id(&self) -> String {
        crate::community::common::ref_repo_id(&self.post.as_ref().and_then(|p| p.repo.clone()))
    }
}

fn not_readable(post_id: &str) -> ForgeError {
    ForgeError::invalid(
        NOT_READABLE,
        format!("post {post_id} is not a published post you can read"),
    )
}

fn table(kind: SubjectKind) -> &'static TableDef {
    if kind == SubjectKind::Issue {
        &ISSUE_ADOPTIONS
    } else {
        &CHANGE_ADOPTIONS
    }
}

/// A repo reference by id.
fn by_id(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

/// Whether every principal of `cx`'s chain (the actor alone when it has none) may read
/// `repo`: it is public, or each holds at least the read role. This is the role half of
/// `Authz::repo` without the token's scope.
pub async fn chain_reads(orgs: &Orgs, cx: &Cx, repo: &Repo) -> bool {
    if Visibility::try_from(repo.visibility).unwrap_or(Visibility::Private) == Visibility::Public {
        return true;
    }
    let chain: Vec<String> = if cx.chain.is_empty() {
        vec![cx.actor_id.clone()]
    } else {
        cx.chain.clone()
    };
    for id in chain.iter().filter(|id| !id.is_empty()) {
        match orgs.effective_role(cx, repo, id).await {
            Ok(perm)
                if role_rank(RepoRole::try_from(perm.role).unwrap_or(RepoRole::None))
                    >= role_rank(RepoRole::Read) => {}
            _ => return false,
        }
    }
    !chain.iter().all(|id| id.is_empty())
}

/// Records an adoption inside the transaction that opens its subject (that subject's
/// surface: the work surface for issues, the changes surface for change requests) and
/// emits `post.adopted` in the adopting repo.
pub(crate) async fn record(tx: &mut Tx, rec: &AdoptionRec) -> Result<()> {
    let kind = SubjectKind::try_from(rec.kind).unwrap_or(SubjectKind::Unspecified);
    tx.insert(
        table(kind),
        &Row::new("", rec.subject_id.clone())
            .k1(rec.post_id().to_string())
            .n1(rec.created_ms as i64)
            .msg(rec),
    )
    .await?;
    let payload = serde_json::json!({
        "post_id": rec.post_id(),
        "subject": kind.as_str_name(),
        "number": rec.number,
    })
    .to_string();
    tx.emit_event(Event {
        kind: "post.adopted".into(),
        repo_id: rec.repo_id.clone(),
        org_id: rec.org_id.clone(),
        subject: Some(SubjectRef {
            kind: rec.kind,
            repo: by_id(&rec.repo_id),
            id: rec.subject_id.clone(),
        }),
        payload_json: payload.clone(),
        ..Default::default()
    })
    .await?;
    tx.audit(
        AuditDraft::new("post.adopt", &rec.org_id)
            .repo_id(&rec.repo_id)
            .target(kind, &rec.subject_id)
            .after(&payload),
    )
    .await
}

/// Follows an issue moved to another repo (the transfer's transaction).
pub(crate) async fn moved_issue(
    tx: &mut Tx,
    issue_id: &str,
    repo_id: &str,
    org_id: &str,
) -> Result<()> {
    let Some((mut a, row)) = tx
        .get_msg::<AdoptionRec>(&ISSUE_ADOPTIONS, "", issue_id)
        .await?
    else {
        return Ok(());
    };
    a.repo_id = repo_id.to_string();
    a.org_id = org_id.to_string();
    tx.update(
        &ISSUE_ADOPTIONS,
        &Row::new("", issue_id)
            .k1(a.post_id().to_string())
            .n1(a.created_ms as i64)
            .version(row.version)
            .msg(&a),
    )
    .await?;
    Ok(())
}

/// Drops a deleted issue's adoption (the delete's transaction).
pub(crate) async fn forget_issue(tx: &mut Tx, issue_id: &str) -> Result<()> {
    tx.delete(&ISSUE_ADOPTIONS, "", issue_id).await?;
    Ok(())
}

/// The adoption row of a subject, if it names a post.
async fn of_subject(tx: &mut Tx, kind: SubjectKind, id: &str) -> Result<Option<AdoptionRec>> {
    Ok(tx
        .get_msg::<AdoptionRec>(table(kind), "", id)
        .await?
        .map(|(r, _)| r))
}

impl Posts {
    /// The post `post_id` as an inspiration for the caller of `cx`, or
    /// `INVALID_ARGUMENT`/`post_not_readable` (see the module docs).
    pub async fn inspiration(&self, orgs: &Orgs, cx: &Cx, post_id: &str) -> Result<PostRef> {
        let post_id = post_id.trim();
        if post_id.is_empty() {
            return Err(not_readable(post_id));
        }
        let sys = Cx::system();
        let repo_id = self
            .locate(&sys, post_id)
            .await
            .map_err(|_| not_readable(post_id))?;
        let (rec, _) = self
            .get(&sys, &repo_id, post_id)
            .await
            .map_err(|_| not_readable(post_id))?;
        if rec.state() != PostState::Published {
            return Err(not_readable(post_id));
        }
        let repo = orgs
            .resolve_repo(&sys, &by_id(&repo_id).unwrap_or_default())
            .await
            .map_err(|_| not_readable(post_id))?;
        if !chain_reads(orgs, cx, &repo).await {
            return Err(not_readable(post_id));
        }
        let p = rec.post();
        Ok(PostRef {
            post_id: p.id.clone(),
            repo: by_id(&repo_id),
            change_number: p.change_number,
            change_title: p.change_title.clone(),
        })
    }

    /// The posts that inspired these change requests, by change request id.
    pub async fn change_inspirations(
        &self,
        cx: &Cx,
        ids: &[String],
    ) -> Result<BTreeMap<String, AdoptionRec>> {
        self.inspirations(cx, SubjectKind::ChangeRequest, ids).await
    }

    /// The posts that inspired these issues, by issue id.
    pub async fn issue_inspirations(
        &self,
        cx: &Cx,
        ids: &[String],
    ) -> Result<BTreeMap<String, AdoptionRec>> {
        self.inspirations(cx, SubjectKind::Issue, ids).await
    }

    async fn inspirations(
        &self,
        cx: &Cx,
        kind: SubjectKind,
        ids: &[String],
    ) -> Result<BTreeMap<String, AdoptionRec>> {
        let mut out = BTreeMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let mut tx = self.db.begin(table(kind).surface, cx).await?;
        for id in ids {
            if let Some(r) = of_subject(&mut tx, kind, id).await? {
                out.insert(id.clone(), r);
            }
        }
        Ok(out)
    }

    /// The adoptions of a post as recorded, newest first (then subject id, descending), at
    /// most [`MAX_ADOPTIONS`] of each kind. Rows follow their issue across transfers and go
    /// with it when it is deleted, so `repo_id` is the subject's current repo.
    pub async fn adoptions(&self, cx: &Cx, post_id: &str) -> Result<Vec<AdoptionRec>> {
        let scan = || Scan::scope("", MAX_ADOPTIONS).key(Key::K1(post_id.to_string()));
        let mut out = Vec::new();
        for t in [&ISSUE_ADOPTIONS, &CHANGE_ADOPTIONS] {
            let mut tx = self.db.begin(t.surface, cx).await?;
            out.extend(
                tx.scan_msgs::<AdoptionRec>(t, &scan())
                    .await?
                    .into_iter()
                    .map(|(a, _)| a),
            );
        }
        sort(&mut out);
        Ok(out)
    }

    /// Refreshes the number and title of adoptions from their subjects; those whose subject
    /// is gone are dropped. One surface at a time (a transaction holds its surface).
    pub async fn live(&self, cx: &Cx, v: Vec<AdoptionRec>) -> Result<Vec<AdoptionRec>> {
        let mut keep = vec![false; v.len()];
        let mut v = v;
        {
            let mut tx = self.db.begin(ISSUE_ADOPTIONS.surface, cx).await?;
            for (i, a) in v.iter_mut().enumerate() {
                if a.kind != SubjectKind::Issue as i32 {
                    continue;
                }
                if let Some((issue, _)) =
                    crate::work::load_issue_by_id(&mut tx, &a.subject_id).await?
                {
                    a.repo_id = issue.repo_id;
                    a.number = issue.number;
                    a.title = issue.title;
                    keep[i] = true;
                }
            }
        }
        {
            let mut tx = self.db.begin(CHANGE_ADOPTIONS.surface, cx).await?;
            for (i, a) in v.iter_mut().enumerate() {
                if a.kind == SubjectKind::Issue as i32 {
                    continue;
                }
                if let Ok((cr, _)) = Changes::load(&mut tx, &a.repo_id, &a.subject_id).await {
                    let c = cr.cr();
                    a.number = c.number;
                    a.title.clone_from(&c.title);
                    keep[i] = true;
                }
            }
        }
        Ok(v.into_iter()
            .zip(keep)
            .filter_map(|(a, k)| k.then_some(a))
            .collect())
    }
}

/// Newest first, then subject id descending (deterministic).
pub fn sort(v: &mut [AdoptionRec]) {
    v.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| b.subject_id.cmp(&a.subject_id))
    });
}

/// The page cursor of an adoption: `<created_ms>|<subject id>`.
pub fn cursor_of(a: &AdoptionRec) -> String {
    format!("{}|{}", a.created_ms, a.subject_id)
}

/// Parses an adoption cursor (`bad_page_token` when it is not one).
pub fn parse_cursor(s: &str) -> Result<Option<(u64, String)>> {
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || ForgeError::invalid("bad_page_token", "the page token is not valid");
    let (ms, id) = s.split_once('|').ok_or_else(bad)?;
    let ms: u64 = ms.parse().map_err(|_| bad())?;
    if id.is_empty() {
        return Err(bad());
    }
    Ok(Some((ms, id.to_string())))
}

/// Whether `a` sorts after the cursor position (strictly older).
pub fn after_cursor(a: &AdoptionRec, cursor: &Option<(u64, String)>) -> bool {
    match cursor {
        None => true,
        Some((ms, id)) => (a.created_ms, a.subject_id.as_str()) < (*ms, id.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ms: u64, id: &str) -> AdoptionRec {
        AdoptionRec {
            created_ms: ms,
            subject_id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn adoptions_sort_newest_first_then_by_id() {
        let mut v = vec![rec(1, "a"), rec(3, "b"), rec(3, "c"), rec(2, "z")];
        sort(&mut v);
        let ids: Vec<&str> = v.iter().map(|a| a.subject_id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "z", "a"]);
    }

    #[test]
    fn cursors_round_trip_and_bound_the_page() {
        let a = rec(42, "01JX");
        let c = parse_cursor(&cursor_of(&a)).unwrap();
        assert_eq!(c, Some((42, "01JX".to_string())));
        assert!(!after_cursor(&a, &c));
        assert!(after_cursor(&rec(42, "01JW"), &c));
        assert!(after_cursor(&rec(41, "01JZ"), &c));
        assert!(!after_cursor(&rec(43, "01JA"), &c));
        assert!(after_cursor(&a, &None));
        assert_eq!(parse_cursor("").unwrap(), None);
        for bad in ["x", "1|", "a|b", "|id"] {
            let e = parse_cursor(bad).unwrap_err();
            assert_eq!(e.subcode, "bad_page_token", "{bad}");
        }
    }

    #[test]
    fn issue_and_change_adoptions_live_on_their_subjects_surfaces() {
        assert_eq!(table(SubjectKind::Issue).surface, crate::work::SURFACE);
        assert_eq!(
            table(SubjectKind::ChangeRequest).surface,
            crate::changes::SURFACE
        );
    }
}
