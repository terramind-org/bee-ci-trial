//! [`HubSink`]: the Beehive forge as a [`ForgeSink`], over the domain APIs in
//! `bee-forge`:
//!
//! | record | forge API |
//! |---|---|
//! | PR → change request | `Changes::open` with the number reserved in the shared number space (`work` counters), `revise`, `submit_review`, `comment_on_lines`, `reply`, `comment`, `react`, `react_comment`, `set_locked`, `close`, `reopen` |
//! | issue | `Work::import_issue` (number, times and comments kept), `add_reaction` |
//! | label, milestone | `Work::create_label`, `create_milestone`, `update_milestone` |
//! | release | `Community::releases.create`, `begin_asset` + upload + `finalize_asset` |
//! | webhook | `Webhooks::create`, disabled, GitHub payload format |
//! | placeholder principal | the forge's principal directory (`observe`) |
//!
//! Every placed comment, review and body carries an HTML comment marker
//! (`<!-- bee-github … -->`) naming its GitHub id, author and time, so placing a record
//! again (resume, live sync) skips what is already there; when link rewriting changed the
//! text, the original is kept in the marker too.
//!
//! What the forge cannot express is reported, not hidden: a merged PR becomes a closed CR
//! whose close reason names the merge commit (landing it again would rewrite the target),
//! a review comment whose lines no longer exist becomes a file comment or a quoted
//! conversation comment, and reactions on review comments have no store yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bee_forge::Forge;
use bee_forge::changes::{Changes, NewChange};
use bee_forge::community::releases::{NewRelease, UploadTicket};
use bee_forge::cx::Cx;
use bee_forge::error::ForgeError;
use bee_forge::events::webhooks::{HookOwner, NewHook};
use bee_forge::work::{Actor, ImportedComment, ImportedIssue, RepoInfo, TextEffects};
use bee_proto::v1::{
    ChangeState, IssueStateReason, MilestoneState, Principal, PrincipalKind, ReactionContent, Repo,
    Verdict, WebhookFormat,
};
use bee_store::{RepoId, Writer};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use sha2::Digest as _;

use crate::clock::parse_rfc3339;
use crate::error::{Result, unavailable};
use crate::principals::{Claim, Placeholder};
use crate::report::{Outcome, category, reason};
use crate::sink::{
    Author, ChangeEvent, ChangeImport, ForgeSink, IssueImport, LabelImport, MilestoneImport,
    PrState, ReactionImport, ReleaseImport, ReviewState, Text, WebhookImport,
};

/// Uploads release asset bytes to a signed URL.
#[async_trait]
pub trait AssetUploader: Send + Sync {
    /// Puts `data` where `ticket` says.
    async fn put(&self, ticket: &UploadTicket, data: &[u8]) -> Result<()>;
}

/// Uploads with an HTTP `PUT` (the blob store's signed URLs).
pub struct HttpAssetUploader {
    http: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
}

impl Default for HttpAssetUploader {
    fn default() -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        HttpAssetUploader {
            http: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector),
        }
    }
}

#[async_trait]
impl AssetUploader for HttpAssetUploader {
    async fn put(&self, ticket: &UploadTicket, data: &[u8]) -> Result<()> {
        let mut b = http::Request::builder().method("PUT").uri(&ticket.url.url);
        for (k, v) in &ticket.url.headers {
            b = b.header(k, v);
        }
        let req = b
            .body(Full::new(Bytes::copy_from_slice(data)))
            .map_err(|e| unavailable(format!("bad upload URL: {e}")))?;
        let resp = self
            .http
            .request(req)
            .await
            .map_err(|e| unavailable(format!("asset upload: {e}")))?;
        let status = resp.status();
        let _ = resp.into_body().collect().await;
        if !status.is_success() {
            return Err(unavailable(format!("asset upload: HTTP {status}")));
        }
        Ok(())
    }
}

/// The forge adapter.
pub struct HubSink {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    repo: Repo,
    writer: Writer,
    repo_id: RepoId,
    assets: Option<Arc<dyn AssetUploader>>,
}

fn fe(e: ForgeError) -> String {
    format!("{}: {}", e.reason.as_str_name(), e.message)
}

/// Infrastructure failures abort the run; everything else is a content outcome.
fn infra(e: &ForgeError) -> bool {
    use bee_proto::v1::ErrorReason as R;
    matches!(e.reason, R::Unavailable | R::DeadlineExceeded | R::Internal)
}

fn escape_marker(s: &str) -> String {
    s.replace("--", "-\u{2010}")
}

/// The marker for a GitHub record.
pub fn marker(kind: &str, id: u64) -> String {
    format!("bee-github {kind} {id}")
}

fn marked(text: &Text, kind: &str, id: u64, author: &Author, at: &str) -> String {
    let mut out = text.body.clone();
    out.push_str(&format!(
        "\n\n<!-- {} by @{} at {}",
        marker(kind, id),
        escape_marker(&author.login),
        escape_marker(at)
    ));
    if !text.unchanged() {
        out.push_str(&format!(
            "; original text:\n{}\n",
            escape_marker(&text.original)
        ));
    }
    out.push_str(" -->");
    out
}

fn has_marker(body: &str, kind: &str, id: u64) -> bool {
    let m = marker(kind, id);
    body.match_indices(&m).any(|(i, _)| {
        body[i + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit())
    })
}

fn reaction(content: &str) -> ReactionContent {
    match content {
        "+1" => ReactionContent::ThumbsUp,
        "-1" => ReactionContent::ThumbsDown,
        "laugh" => ReactionContent::Laugh,
        "hooray" => ReactionContent::Hooray,
        "confused" => ReactionContent::Confused,
        "heart" => ReactionContent::Heart,
        "rocket" => ReactionContent::Rocket,
        "eyes" => ReactionContent::Eyes,
        _ => ReactionContent::Unspecified,
    }
}

fn ms(t: &str) -> u64 {
    parse_rfc3339(t).map(|s| s * 1000).unwrap_or(0)
}

impl HubSink {
    /// A sink placing records in `repo` (a forge repo whose ids are the VCS repo's), with
    /// `writer` over the same VCS stores for commit lookups.
    pub fn new(
        forge: Arc<Forge>,
        changes: Arc<Changes>,
        repo: Repo,
        writer: Writer,
    ) -> Result<HubSink> {
        let ulid = bee_core::Ulid::parse(&repo.id)?;
        Ok(HubSink {
            forge,
            changes,
            repo,
            writer,
            repo_id: RepoId(ulid),
            assets: Some(Arc::new(HttpAssetUploader::default())),
        })
    }

    /// Uses `uploader` for release assets (`None`: assets are reported as not stored).
    pub fn with_assets(mut self, uploader: Option<Arc<dyn AssetUploader>>) -> HubSink {
        self.assets = uploader;
        self
    }

    fn cx(&self, a: &Author) -> Cx {
        Cx::new(
            a.principal.clone(),
            if a.agent {
                PrincipalKind::Agent
            } else {
                PrincipalKind::Human
            },
        )
    }

    fn principal(&self, a: &Author) -> Principal {
        Principal {
            id: a.principal.clone(),
            kind: if a.agent {
                PrincipalKind::Agent
            } else {
                PrincipalKind::Human
            } as i32,
            display: a.login.clone(),
            ..Default::default()
        }
    }

    fn actor(&self, a: &Author) -> Actor {
        Actor {
            principal: self.principal(a),
            triage: true,
            admin: true,
            association: "NONE".into(),
        }
    }

    fn info(&self) -> RepoInfo {
        RepoInfo::from(&self.repo)
    }

    async fn changeset(&self, sha: &str) -> Result<Option<bee_proto::v1::ObjectId>> {
        let Ok(oid) = bee_git::GitOid::parse(sha) else {
            return Ok(None);
        };
        Ok(
            bee_import::lookup_bee_id(self.writer.meta().as_ref(), self.repo_id, &oid)
                .await?
                .map(|id| bee_proto::v1::ObjectId {
                    raw: id.as_bytes().to_vec(),
                }),
        )
    }

    /// Moves the shared number counter so the next allocation is `n`. `Err(detail)` when
    /// the number is no longer free.
    async fn make_room(&self, n: u64) -> Result<std::result::Result<(), String>> {
        let cx = Cx::system();
        if let Some(e) = self
            .forge
            .work
            .number_entry(&cx, &self.repo.id, n)
            .await
            .map_err(|e| unavailable(fe(e)))?
        {
            return Ok(Err(format!(
                "#{n} is already used in Beehive ({:?})",
                e.kind()
            )));
        }
        let mut tx = self
            .forge
            .db
            .begin(bee_forge::work::SURFACE, &cx)
            .await
            .map_err(|e| unavailable(fe(e)))?;
        let cur = tx
            .add_n1(&bee_forge::work::COUNTERS, &self.repo.id, "numbers", 0)
            .await
            .map_err(|e| unavailable(fe(e)))?;
        let want = n as i64 - 1;
        if cur > want {
            return Ok(Err(format!(
                "Beehive already handed out numbers up to #{cur}; #{n} cannot be kept"
            )));
        }
        if cur < want {
            tx.add_n1(
                &bee_forge::work::COUNTERS,
                &self.repo.id,
                "numbers",
                want - cur,
            )
            .await
            .map_err(|e| unavailable(fe(e)))?;
        }
        tx.commit().await.map_err(|e| unavailable(fe(e)))?;
        Ok(Ok(()))
    }

    async fn open_change(
        &self,
        c: &ChangeImport,
        first: bee_proto::v1::ObjectId,
        out: &mut Vec<Outcome>,
    ) -> Result<Option<String>> {
        if let Err(why) = self.make_room(c.number).await? {
            out.push(Outcome::dropped(
                category::PULL_REQUESTS,
                c.item(),
                reason::NUMBER_TAKEN,
                why,
            ));
            return Ok(None);
        }
        let cx = self.cx(&c.author);
        let mut depends_on = String::new();
        if let Some(p) = c.depends_on
            && let Ok(parent) = self.changes.get_by_number(&cx, &self.repo, p).await
            && parent.state == ChangeState::Open as i32
        {
            depends_on = parent.id;
        }
        let body = marked(&c.body, "pull", c.number, &c.author, &c.created_at);
        let new = |target: &str, depends_on: &str| NewChange {
            target_ref: target.to_string(),
            head: Some(first.clone()),
            title: c.title.clone(),
            body: body.clone(),
            depends_on: depends_on.to_string(),
            draft: c.draft,
            labels: c.labels.clone(),
            assignee_ids: c.assignees.iter().map(|a| a.principal.clone()).collect(),
            reviewer_ids: c.reviewers.iter().map(|a| a.principal.clone()).collect(),
            ..NewChange::default()
        };
        // An open PR targets its base branch so it can land later. A PR whose commits
        // already reached the target (merged, closed after a manual merge) is anchored on
        // its recorded base instead.
        let mut targets: Vec<(String, String)> = Vec::new();
        let open = c.state == PrState::Open;
        if open {
            targets.push((c.target_ref.clone(), depends_on.clone()));
        }
        if let Some(b) = &c.base_ref {
            targets.push((b.clone(), String::new()));
        }
        if !open {
            targets.push((c.target_ref.clone(), String::new()));
        }
        let mut last = String::new();
        for (t, d) in targets {
            match self.changes.open(&cx, &self.repo, new(&t, &d)).await {
                Ok(cr) => {
                    if cr.number != c.number {
                        out.push(Outcome::dropped(
                            category::PULL_REQUESTS,
                            c.item(),
                            reason::NUMBER_TAKEN,
                            format!(
                                "another record took #{} concurrently; opened as #{}",
                                c.number, cr.number
                            ),
                        ));
                    }
                    return Ok(Some(cr.id));
                }
                Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                Err(e) => {
                    last = fe(e);
                    // The number was reserved by the failed attempt's allocation: make room
                    // again for the next target.
                    if let Err(why) = self.make_room(c.number).await? {
                        out.push(Outcome::dropped(
                            category::PULL_REQUESTS,
                            c.item(),
                            reason::NUMBER_TAKEN,
                            why,
                        ));
                        return Ok(None);
                    }
                }
            }
        }
        out.push(Outcome::dropped(
            category::PULL_REQUESTS,
            c.item(),
            reason::TARGET_REJECTED,
            last,
        ));
        Ok(None)
    }

    async fn conversation(
        &self,
        cr: &str,
        a: &Author,
        body: &str,
    ) -> std::result::Result<String, String> {
        self.changes
            .comment(&self.cx(a), &self.repo, cr, body, true)
            .await
            .map(|c| c.id)
            .map_err(fe)
    }

    #[allow(clippy::too_many_arguments)]
    async fn place_event(
        &self,
        c: &ChangeImport,
        cr: &str,
        e: &ChangeEvent,
        heads: &[String],
        threads: &mut BTreeMap<u64, String>,
        out: &mut Vec<Outcome>,
    ) -> Result<()> {
        let item = e.item(c.number);
        let rev_of = |sha: &str| {
            heads
                .iter()
                .position(|h| h == sha)
                .map(|i| i as u32 + 1)
                .unwrap_or(0)
        };
        match e {
            ChangeEvent::Review {
                id,
                author,
                state,
                body,
                commit,
                at,
            } => {
                let text = marked(body, "review", *id, author, at);
                let verdict = match state {
                    ReviewState::Approved => Verdict::Approve,
                    ReviewState::ChangesRequested => Verdict::RequestChanges,
                    ReviewState::Commented | ReviewState::Dismissed => Verdict::Comment,
                };
                let rev = commit.as_deref().map(rev_of).unwrap_or(0);
                let r = self
                    .changes
                    .submit_review(
                        &self.cx(author),
                        &self.repo,
                        cr,
                        rev,
                        verdict,
                        &text,
                        vec![],
                    )
                    .await;
                match r {
                    Ok(_) if *state == ReviewState::Dismissed => out.push(Outcome::archived(
                        category::REVIEWS,
                        item,
                        "dismissed on GitHub; kept as a comment review (GitHub does not export the dismissed verdict)",
                    )),
                    Ok(_) => out.push(Outcome::imported(category::REVIEWS, item)),
                    Err(err) if infra(&err) => return Err(unavailable(fe(err))),
                    Err(err) => {
                        let why = fe(err);
                        let quoted = format!(
                            "**Review ({:?}) by @{} on GitHub**\n\n{text}",
                            state, author.login
                        );
                        match self.conversation(cr, author, &quoted).await {
                            Ok(_) => out.push(Outcome::archived(
                                category::REVIEWS,
                                item,
                                format!("kept as a conversation comment ({why})"),
                            )),
                            Err(e2) => out.push(Outcome::dropped(category::REVIEWS, item, reason::TARGET_REJECTED, e2)),
                        }
                    }
                }
            }
            ChangeEvent::ReviewComment {
                id,
                author,
                body,
                path,
                side,
                start_line,
                line,
                file_level,
                commit,
                diff_hunk,
                in_reply_to,
                at,
                reactions,
                ..
            } => {
                let text = marked(body, "review-comment", *id, author, at);
                let cx = self.cx(author);
                let parent_thread = in_reply_to.and_then(|p| threads.get(&p).cloned());
                let placed = if let Some(t) = parent_thread {
                    self.changes
                        .reply(&cx, &self.repo, &t, &text, None, true)
                        .await
                        .map(|rc| (rc.thread_id, None))
                } else {
                    let end = line.unwrap_or(0);
                    let start = start_line.unwrap_or(end);
                    let rev = rev_of(commit);
                    let first = self
                        .changes
                        .comment_on_lines(
                            &cx,
                            &self.repo,
                            cr,
                            &text,
                            path.as_bytes(),
                            side,
                            start,
                            end,
                            *file_level,
                            rev,
                            None,
                            true,
                        )
                        .await;
                    match first {
                        Ok(rc) => Ok((rc.thread_id, None)),
                        Err(err) if infra(&err) => return Err(unavailable(fe(err))),
                        Err(err) if !*file_level => {
                            let why = fe(err);
                            self.changes
                                .comment_on_lines(&cx, &self.repo, cr, &text, path.as_bytes(), side, 0, 0, true, rev, None, true)
                                .await
                                .map(|rc| (rc.thread_id, Some(format!("the anchored lines are gone ({why}); kept as a file comment on {path}"))))
                        }
                        Err(err) => Err(err),
                    }
                };
                match placed {
                    Ok((thread, note)) => {
                        threads.insert(*id, thread);
                        out.push(match note {
                            Some(n) => Outcome::archived(category::REVIEW_COMMENTS, item, n),
                            None => Outcome::imported(category::REVIEW_COMMENTS, item),
                        });
                    }
                    Err(err) if infra(&err) => return Err(unavailable(fe(err))),
                    Err(err) => {
                        let why = fe(err);
                        let quoted = format!(
                            "**Review comment by @{} on `{path}` line {}**\n\n```diff\n{diff_hunk}\n```\n\n{text}",
                            author.login,
                            line.map(|l| l.to_string()).unwrap_or_else(|| "-".into())
                        );
                        match self.conversation(cr, author, &quoted).await {
                            Ok(_) => out.push(Outcome::archived(
                                category::REVIEW_COMMENTS,
                                item,
                                format!("broken anchor ({why}); kept as a conversation comment with its diff hunk"),
                            )),
                            Err(e2) => out.push(Outcome::dropped(category::REVIEW_COMMENTS, item, reason::TARGET_REJECTED, e2)),
                        }
                    }
                }
                for r in reactions {
                    out.push(Outcome::dropped(
                        category::REACTIONS,
                        format!("reaction/{}", r.id),
                        reason::NOT_SUPPORTED_BY_TARGET,
                        "Beehive has no reactions on review comments yet",
                    ));
                }
            }
            ChangeEvent::Comment {
                id,
                author,
                body,
                at,
                reactions,
            } => {
                let text = marked(body, "comment", *id, author, at);
                match self.conversation(cr, author, &text).await {
                    Ok(cid) => {
                        out.push(Outcome::imported(category::ISSUE_COMMENTS, item));
                        for r in reactions {
                            self.react_comment(cr, &cid, r, out).await?;
                        }
                    }
                    Err(e2) => out.push(Outcome::dropped(
                        category::ISSUE_COMMENTS,
                        item,
                        reason::TARGET_REJECTED,
                        e2,
                    )),
                }
            }
        }
        Ok(())
    }

    async fn react_comment(
        &self,
        cr: &str,
        cid: &str,
        r: &ReactionImport,
        out: &mut Vec<Outcome>,
    ) -> Result<()> {
        let item = format!("reaction/{}", r.id);
        let content = reaction(&r.content);
        if content == ReactionContent::Unspecified {
            out.push(Outcome::dropped(
                category::REACTIONS,
                item,
                reason::UNTRANSLATABLE,
                format!("unknown reaction {}", r.content),
            ));
            return Ok(());
        }
        match self
            .changes
            .react_comment(&self.cx(&r.author), &self.repo, cr, cid, content, true)
            .await
        {
            Ok(_) => out.push(Outcome::imported(category::REACTIONS, item)),
            Err(e) if infra(&e) => return Err(unavailable(fe(e))),
            Err(e) => out.push(Outcome::dropped(
                category::REACTIONS,
                item,
                reason::TARGET_REJECTED,
                fe(e),
            )),
        }
        Ok(())
    }

    /// Bodies already placed on a CR (conversation comments, review comments, reviews).
    async fn existing(&self, cr: &str) -> Result<(Vec<String>, BTreeMap<u64, String>)> {
        let cx = Cx::system();
        let mut bodies = Vec::new();
        let mut threads = BTreeMap::new();
        let mut token = String::new();
        loop {
            let (cs, next) = self
                .changes
                .comments(&cx, &self.repo, cr, &token, 100)
                .await
                .map_err(|e| unavailable(fe(e)))?;
            bodies.extend(cs.into_iter().map(|c| c.body));
            if next.is_empty() {
                break;
            }
            token = next;
        }
        let mut token = String::new();
        loop {
            let (ts, next) = self
                .changes
                .threads(&cx, &self.repo, cr, None, &token, 100)
                .await
                .map_err(|e| unavailable(fe(e)))?;
            for t in ts {
                for c in &t.comments {
                    if let Some(id) = marker_id(&c.body, "review-comment") {
                        threads.insert(id, t.id.clone());
                    }
                    bodies.push(c.body.clone());
                }
            }
            if next.is_empty() {
                break;
            }
            token = next;
        }
        let reviews = self
            .changes
            .reviews(&cx, &self.repo, cr)
            .await
            .map_err(|e| unavailable(fe(e)))?;
        bodies.extend(reviews.into_iter().map(|r| r.body));
        Ok((bodies, threads))
    }
}

fn marker_id(body: &str, kind: &str) -> Option<u64> {
    let m = format!("bee-github {kind} ");
    let i = body.find(&m)? + m.len();
    body[i..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn event_kind(e: &ChangeEvent) -> &'static str {
    match e {
        ChangeEvent::Review { .. } => "review",
        ChangeEvent::ReviewComment { .. } => "review-comment",
        ChangeEvent::Comment { .. } => "comment",
    }
}

fn event_category(e: &ChangeEvent) -> &'static str {
    match e {
        ChangeEvent::Review { .. } => category::REVIEWS,
        ChangeEvent::ReviewComment { .. } => category::REVIEW_COMMENTS,
        ChangeEvent::Comment { .. } => category::ISSUE_COMMENTS,
    }
}

#[async_trait]
impl ForgeSink for HubSink {
    fn name(&self) -> &str {
        "beehive"
    }

    async fn principals(&self, all: &[Placeholder]) -> Result<Vec<Outcome>> {
        let dir = &self.forge.hooks.directory;
        Ok(all
            .iter()
            .map(|p| {
                dir.observe(&p.principal_proto());
                Outcome::imported(category::PRINCIPALS, format!("github-user/{}", p.github_id))
            })
            .collect())
    }

    async fn change(&self, c: &ChangeImport) -> Result<Vec<Outcome>> {
        let mut out = Vec::new();
        // Revision heads that exist as changesets.
        let mut heads: Vec<(String, bee_proto::v1::ObjectId)> = Vec::new();
        for (i, r) in c.revisions.iter().enumerate() {
            match self.changeset(&r.head).await? {
                Some(id) => heads.push((r.head.clone(), id)),
                None => out.push(Outcome::dropped(
                    category::PR_REVISIONS,
                    format!("pull/{}@r{}", c.number, i + 1),
                    reason::MISSING_COMMIT,
                    format!("{} was not imported as a changeset", r.head),
                )),
            }
        }
        let Some(first) = heads.first().cloned() else {
            out.push(Outcome::dropped(
                category::PULL_REQUESTS,
                c.item(),
                reason::MISSING_COMMIT,
                "none of the PR's heads were imported",
            ));
            return Ok(out);
        };
        let cx = self.cx(&c.author);
        let existing = match self.changes.get_by_number(&cx, &self.repo, c.number).await {
            Ok(cr) => Some(cr),
            Err(e) if infra(&e) => return Err(unavailable(fe(e))),
            Err(_) => None,
        };
        let (cr_id, fresh) = match existing {
            Some(cr) if has_marker(&cr.body, "pull", c.number) => (cr.id, false),
            Some(_) => {
                out.push(Outcome::dropped(
                    category::PULL_REQUESTS,
                    c.item(),
                    reason::NUMBER_TAKEN,
                    format!("#{} is a change request made in Beehive", c.number),
                ));
                return Ok(out);
            }
            None => match self.open_change(c, first.1.clone(), &mut out).await? {
                Some(id) => (id, true),
                None => return Ok(out),
            },
        };
        let cr = self
            .changes
            .get(&cx, &self.repo, &cr_id)
            .await
            .map_err(|e| unavailable(fe(e)))?;
        let open_now = cr.state == ChangeState::Open as i32;
        // Reopen to add what is missing, close again at the end.
        if !open_now && cr.state == ChangeState::Closed as i32 {
            self.changes
                .reopen(&cx, &self.repo, &cr_id)
                .await
                .map_err(|e| unavailable(fe(e)))?;
        }
        if !fresh {
            let edit = bee_forge::changes::store::ChangeEdit {
                title: (cr.title != c.title).then(|| c.title.clone()),
                ..Default::default()
            };
            if edit.title.is_some()
                && let Err(e) = self.changes.edit(&cx, &self.repo, &cr_id, edit).await
                && infra(&e)
            {
                return Err(unavailable(fe(e)));
            }
        }
        // Revisions.
        let known: Vec<Vec<u8>> = cr
            .revisions
            .iter()
            .filter_map(|r| r.head.as_ref().map(|h| h.raw.clone()))
            .collect();
        let mut placed_heads: Vec<String> = Vec::new();
        for (i, (sha, id)) in heads.iter().enumerate() {
            let item = format!("pull/{}@r{}", c.number, i + 1);
            if known.contains(&id.raw) {
                placed_heads.push(sha.clone());
                out.push(Outcome::imported(category::PR_REVISIONS, item));
                continue;
            }
            let pusher = c
                .revisions
                .iter()
                .find(|r| r.head == *sha)
                .and_then(|r| r.pusher.clone());
            let pcx = self.cx(pusher.as_ref().unwrap_or(&c.author));
            match self.changes.revise(&pcx, &self.repo, &cr_id, id).await {
                Ok(_) => {
                    placed_heads.push(sha.clone());
                    out.push(Outcome::imported(category::PR_REVISIONS, item));
                }
                Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                Err(e) => out.push(Outcome::dropped(
                    category::PR_REVISIONS,
                    item,
                    reason::TARGET_REJECTED,
                    fe(e),
                )),
            }
        }
        // Events.
        let (bodies, mut threads) = if fresh {
            (Vec::new(), BTreeMap::new())
        } else {
            self.existing(&cr_id).await?
        };
        for e in &c.events {
            if bodies.iter().any(|b| has_marker(b, event_kind(e), e.id())) {
                out.push(Outcome::imported(event_category(e), e.item(c.number)));
                continue;
            }
            self.place_event(c, &cr_id, e, &placed_heads, &mut threads, &mut out)
                .await?;
        }
        // Reactions on the CR.
        for r in &c.reactions {
            let item = format!("reaction/{}", r.id);
            let content = reaction(&r.content);
            if content == ReactionContent::Unspecified {
                out.push(Outcome::dropped(
                    category::REACTIONS,
                    item,
                    reason::UNTRANSLATABLE,
                    format!("unknown reaction {}", r.content),
                ));
                continue;
            }
            match self
                .changes
                .react(&self.cx(&r.author), &self.repo, &cr_id, content, true)
                .await
            {
                Ok(_) => out.push(Outcome::imported(category::REACTIONS, item)),
                Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                Err(e) => out.push(Outcome::dropped(
                    category::REACTIONS,
                    item,
                    reason::TARGET_REJECTED,
                    fe(e),
                )),
            }
        }
        let map_err = |e: ForgeError| unavailable(fe(e));
        if c.locked != cr.locked {
            self.changes
                .set_locked(&cx, &self.repo, &cr_id, c.locked)
                .await
                .map_err(map_err)?;
        }
        if fresh || cr.draft != c.draft {
            let now = self
                .changes
                .get(&cx, &self.repo, &cr_id)
                .await
                .map_err(map_err)?;
            if now.draft != c.draft && now.state == ChangeState::Open as i32 {
                self.changes
                    .set_draft(&cx, &self.repo, &cr_id, c.draft)
                    .await
                    .map_err(map_err)?;
            }
        }
        // State.
        match &c.state {
            PrState::Open => out.push(Outcome::imported(category::PULL_REQUESTS, c.item())),
            PrState::Closed => {
                self.changes
                    .close(&cx, &self.repo, &cr_id, "closed on GitHub")
                    .await
                    .map_err(map_err)?;
                out.push(if cr.target_ref == c.target_ref {
                    Outcome::imported(category::PULL_REQUESTS, c.item())
                } else {
                    Outcome::archived(
                        category::PULL_REQUESTS,
                        c.item(),
                        format!(
                            "closed on GitHub; anchored on {} (its base commit) because its commits already reached {}",
                            cr.target_ref, c.target_ref
                        ),
                    )
                });
            }
            PrState::Merged { merge_commit } => {
                let why = format!(
                    "merged on GitHub as {}",
                    merge_commit.as_deref().unwrap_or("an unknown commit")
                );
                self.changes
                    .close(&cx, &self.repo, &cr_id, &why)
                    .await
                    .map_err(map_err)?;
                out.push(Outcome::archived(
                    category::PULL_REQUESTS,
                    c.item(),
                    format!(
                        "{why}; carried as a closed change request on {} (its commits are already in the history)",
                        cr.target_ref
                    ),
                ));
            }
        }
        Ok(out)
    }

    async fn issue(&self, i: &IssueImport) -> Result<Vec<Outcome>> {
        let cx = self.cx(&i.author);
        let info = self.info();
        let item = format!("issue/{}", i.number);
        let work = &self.forge.work;
        let mut out = Vec::new();
        let existing = work.get_issue_by_number(&cx, &info, i.number).await;
        let issue = match existing {
            Ok(iss) if has_marker(&iss.body, "issue", i.number) => {
                // Placed before: add missing comments, follow the state.
                let (cs, _) = work
                    .list_comments(&cx, &info, i.number, None, 10_000, "")
                    .await
                    .map_err(|e| unavailable(fe(e)))?;
                for c in &i.comments {
                    let citem = format!("issue/{}#comment-{}", i.number, c.id);
                    if cs.iter().any(|x| has_marker(&x.body, "comment", c.id)) {
                        out.push(Outcome::imported(category::ISSUE_COMMENTS, citem));
                        continue;
                    }
                    let body = marked(&c.body, "comment", c.id, &c.author, &c.at);
                    match work
                        .create_comment(
                            &self.cx(&c.author),
                            &info,
                            &self.actor(&c.author),
                            i.number,
                            &body,
                            "",
                            false,
                            false,
                            &TextEffects::default(),
                        )
                        .await
                    {
                        Ok(_) => out.push(Outcome::imported(category::ISSUE_COMMENTS, citem)),
                        Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                        Err(e) => out.push(Outcome::dropped(
                            category::ISSUE_COMMENTS,
                            citem,
                            reason::TARGET_REJECTED,
                            fe(e),
                        )),
                    }
                }
                let open_now = iss.state == bee_proto::v1::IssueState::Open as i32;
                let want_open = i.state == "open";
                let actor = self.actor(&i.author);
                if open_now && !want_open {
                    let reason = if i.state_reason.as_deref() == Some("not_planned") {
                        IssueStateReason::NotPlanned
                    } else {
                        IssueStateReason::Completed
                    };
                    work.close_issue(
                        &cx,
                        &info,
                        &actor,
                        i.number,
                        reason,
                        None,
                        "",
                        0,
                        &TextEffects::default(),
                    )
                    .await
                    .map_err(|e| unavailable(fe(e)))?;
                } else if !open_now && want_open {
                    work.reopen_issue(&cx, &info, &actor, i.number, "", 0, &TextEffects::default())
                        .await
                        .map_err(|e| unavailable(fe(e)))?;
                }
                out.push(Outcome::imported(category::ISSUES, item));
                return Ok(out);
            }
            Ok(_) => {
                out.push(Outcome::dropped(
                    category::ISSUES,
                    item,
                    reason::NUMBER_TAKEN,
                    format!("#{} is an issue made in Beehive", i.number),
                ));
                return Ok(out);
            }
            Err(e) if infra(&e) => return Err(unavailable(fe(e))),
            Err(_) => {
                let imp = ImportedIssue {
                    number: i.number,
                    title: i.title.clone(),
                    body: marked(&i.body, "issue", i.number, &i.author, &i.created_at),
                    author: self.principal(&i.author),
                    created_ms: ms(&i.created_at),
                    closed_ms: (i.state == "closed")
                        .then(|| i.closed_at.as_deref().map(ms).unwrap_or(ms(&i.created_at))),
                    state_reason: match i.state_reason.as_deref() {
                        Some("not_planned") => IssueStateReason::NotPlanned,
                        Some("duplicate") => IssueStateReason::Duplicate,
                        _ => IssueStateReason::Completed,
                    },
                    assignees: i.assignees.iter().map(|a| self.principal(a)).collect(),
                    labels: i
                        .labels
                        .iter()
                        .map(|l| (l.clone(), String::new()))
                        .collect(),
                    milestone: String::new(),
                    locked: i.locked,
                    comments: i
                        .comments
                        .iter()
                        .map(|c| ImportedComment {
                            author: self.principal(&c.author),
                            body: marked(&c.body, "comment", c.id, &c.author, &c.at),
                            created_ms: ms(&c.at),
                        })
                        .collect(),
                };
                match work.import_issue(&Cx::system(), &info, imp).await {
                    Ok(iss) => iss,
                    Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                    Err(e) => {
                        let r = if e.reason == bee_proto::v1::ErrorReason::AlreadyExists {
                            reason::NUMBER_TAKEN
                        } else {
                            reason::TARGET_REJECTED
                        };
                        out.push(Outcome::dropped(category::ISSUES, item, r, fe(e)));
                        return Ok(out);
                    }
                }
            }
        };
        out.push(Outcome::imported(category::ISSUES, item));
        if let Some(m) = i.milestone {
            out.push(Outcome::archived(
                category::MILESTONES,
                format!("issue/{}#milestone-{m}", i.number),
                "the issue's milestone is linked by title when the milestone record is placed",
            ));
        }
        for c in &i.comments {
            out.push(Outcome::imported(
                category::ISSUE_COMMENTS,
                format!("issue/{}#comment-{}", i.number, c.id),
            ));
        }
        // Reactions on the issue and its comments.
        let (cs, _) = work
            .list_comments(&cx, &info, i.number, None, 10_000, "")
            .await
            .map_err(|e| unavailable(fe(e)))?;
        let mut targets: Vec<(String, &ReactionImport)> =
            i.reactions.iter().map(|r| (issue.id.clone(), r)).collect();
        for c in &i.comments {
            if let Some(x) = cs.iter().find(|x| has_marker(&x.body, "comment", c.id)) {
                targets.extend(c.reactions.iter().map(|r| (x.id.clone(), r)));
            }
        }
        for (subject, r) in targets {
            let ritem = format!("reaction/{}", r.id);
            let content = reaction(&r.content);
            if content == ReactionContent::Unspecified {
                out.push(Outcome::dropped(
                    category::REACTIONS,
                    ritem,
                    reason::UNTRANSLATABLE,
                    format!("unknown reaction {}", r.content),
                ));
                continue;
            }
            match work
                .add_reaction(
                    &self.cx(&r.author),
                    &info,
                    &self.actor(&r.author),
                    &subject,
                    content,
                )
                .await
            {
                Ok(_) => out.push(Outcome::imported(category::REACTIONS, ritem)),
                Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                Err(e) => out.push(Outcome::dropped(
                    category::REACTIONS,
                    ritem,
                    reason::TARGET_REJECTED,
                    fe(e),
                )),
            }
        }
        Ok(out)
    }

    async fn label(&self, l: &LabelImport) -> Result<Vec<Outcome>> {
        let item = format!("label:{}", l.name);
        let sys = Author {
            principal: "system".into(),
            ..Author::default()
        };
        match self
            .forge
            .work
            .create_label(
                &Cx::system(),
                &self.info(),
                &self.actor(&sys),
                &l.name,
                &l.color,
                &l.description,
            )
            .await
        {
            Ok(_) => Ok(vec![Outcome::imported(category::LABELS, item)]),
            Err(e) if e.reason == bee_proto::v1::ErrorReason::AlreadyExists => {
                Ok(vec![Outcome::imported(category::LABELS, item)])
            }
            Err(e) if infra(&e) => Err(unavailable(fe(e))),
            Err(e) => Ok(vec![Outcome::dropped(
                category::LABELS,
                item,
                reason::TARGET_REJECTED,
                fe(e),
            )]),
        }
    }

    async fn milestone(&self, m: &MilestoneImport) -> Result<Vec<Outcome>> {
        let item = format!("milestone/{}", m.number);
        let work = &self.forge.work;
        let cx = Cx::system();
        let due = m.due_on.as_deref().map(ms);
        let created = match work
            .create_milestone(&cx, &self.info(), &m.title, &m.description, due)
            .await
        {
            Ok(x) => x,
            Err(e) if e.reason == bee_proto::v1::ErrorReason::AlreadyExists => {
                return Ok(vec![Outcome::imported(category::MILESTONES, item)]);
            }
            Err(e) if infra(&e) => return Err(unavailable(fe(e))),
            Err(e) => {
                return Ok(vec![Outcome::dropped(
                    category::MILESTONES,
                    item,
                    reason::TARGET_REJECTED,
                    fe(e),
                )]);
            }
        };
        if m.state == "closed" {
            work.update_milestone(
                &cx,
                &self.info(),
                created.number,
                None,
                None,
                None,
                false,
                MilestoneState::Closed,
            )
            .await
            .map_err(|e| unavailable(fe(e)))?;
        }
        Ok(vec![if created.number == m.number {
            Outcome::imported(category::MILESTONES, item)
        } else {
            Outcome::archived(
                category::MILESTONES,
                item,
                format!(
                    "milestone numbers are assigned by Beehive: #{} became #{}",
                    m.number, created.number
                ),
            )
        }])
    }

    async fn release(&self, r: &ReleaseImport) -> Result<Vec<Outcome>> {
        let item = format!("release:{}", r.tag);
        let cx = self.cx(&r.author);
        let rel = &self.forge.community.releases;
        let new = NewRelease {
            tag_name: r.tag.clone(),
            target: r.target.clone(),
            name: r.name.clone(),
            body: marked(&r.body, "release", r.id, &r.author, &r.created_at),
            draft: r.draft || !r.assets.is_empty(),
            prerelease: r.prerelease,
            ..NewRelease::default()
        };
        let mut out = Vec::new();
        let created = match rel.create(&cx, &self.repo, new).await {
            Ok(x) => x,
            Err(e) if e.reason == bee_proto::v1::ErrorReason::AlreadyExists => {
                out.push(Outcome::imported(category::RELEASES, item));
                for a in &r.assets {
                    out.push(Outcome::imported(
                        category::RELEASE_ASSETS,
                        format!("release:{}/{}", r.tag, a.name),
                    ));
                }
                return Ok(out);
            }
            // The releases domain depends on server ports (the tag store); an unwired port is
            // reported as unavailable and kept as a drop rather than aborting the import.
            Err(e) if e.reason == bee_proto::v1::ErrorReason::DeadlineExceeded => {
                return Err(unavailable(fe(e)));
            }
            Err(e) => {
                out.push(Outcome::dropped(
                    category::RELEASES,
                    item,
                    reason::TARGET_REJECTED,
                    fe(e),
                ));
                for a in &r.assets {
                    out.push(Outcome::dropped(
                        category::RELEASE_ASSETS,
                        format!("release:{}/{}", r.tag, a.name),
                        reason::PARENT_DROPPED,
                        "its release was not placed",
                    ));
                }
                return Ok(out);
            }
        };
        out.push(Outcome::imported(category::RELEASES, item));
        for a in &r.assets {
            let aitem = format!("release:{}/{}", r.tag, a.name);
            let Some(up) = &self.assets else {
                out.push(Outcome::dropped(
                    category::RELEASE_ASSETS,
                    aitem,
                    reason::NOT_SUPPORTED_BY_TARGET,
                    "no asset store is wired",
                ));
                continue;
            };
            let res = async {
                let t = rel
                    .begin_asset(
                        &cx,
                        &self.repo,
                        &created.id,
                        &a.name,
                        &a.label,
                        &a.content_type,
                        a.data.len() as u64,
                    )
                    .await
                    .map_err(fe)?;
                up.put(&t, &a.data).await.map_err(|e| e.message)?;
                let digest = sha2::Sha256::digest(&a.data);
                rel.finalize_asset(&cx, &self.repo, &t.asset_id, &digest)
                    .await
                    .map_err(fe)?;
                Ok::<_, String>(())
            }
            .await;
            out.push(match res {
                Ok(()) => Outcome::imported(category::RELEASE_ASSETS, aitem),
                Err(why) => Outcome::dropped(
                    category::RELEASE_ASSETS,
                    aitem,
                    reason::TARGET_REJECTED,
                    why,
                ),
            });
        }
        if !r.draft && !r.assets.is_empty() {
            let patch = bee_forge::community::releases::ReleasePatch {
                draft: Some(false),
                ..Default::default()
            };
            if let Err(e) = rel.update(&cx, &self.repo, &created.id, patch).await {
                out.push(Outcome::archived(
                    category::RELEASES,
                    format!("release:{}#publish", r.tag),
                    format!("left as a draft: {}", fe(e)),
                ));
            }
        }
        Ok(out)
    }

    async fn webhooks(&self, hooks: &[WebhookImport]) -> Result<Vec<Outcome>> {
        let owner = HookOwner::repo(&self.repo.id, &self.repo.org_id);
        let mut out = Vec::new();
        for h in hooks {
            let item = format!("hook/{}", h.id);
            let mut kinds: Vec<String> = Vec::new();
            if !h.events.iter().any(|e| e == "*") {
                for m in self.forge.registry.modules() {
                    for k in m.event_kinds {
                        if let Some((ev, _)) = k.github
                            && h.events.iter().any(|e| e == ev)
                        {
                            kinds.push(k.name.to_string());
                        }
                    }
                }
                kinds.sort();
                kinds.dedup();
            }
            let new = NewHook {
                url: h.url.clone(),
                kinds,
                secret: String::new(),
                format: WebhookFormat::Github,
                active: false,
                insecure_ssl: false,
            };
            match self.forge.webhooks.create(&Cx::system(), &owner, new).await {
                Ok(_) => out.push(Outcome::archived(
                    category::WEBHOOKS,
                    item,
                    "pre-filled and disabled; re-enter its secret to enable it",
                )),
                Err(e) if infra(&e) => return Err(unavailable(fe(e))),
                Err(e) => out.push(Outcome::dropped(
                    category::WEBHOOKS,
                    item,
                    reason::TARGET_REJECTED,
                    fe(e),
                )),
            }
        }
        Ok(out)
    }

    async fn reassign(&self, claim: &Claim) -> Result<u64> {
        // Placeholder ids are stable across repos; the directory links the placeholder to
        // its claimant, which is how every imported repo follows one claim.
        self.forge.hooks.directory.observe(&Principal {
            id: claim.placeholder.clone(),
            kind: PrincipalKind::Human as i32,
            display: format!("claimed by {}", claim.principal),
            owner_id: claim.principal.clone(),
            external_ids: vec![
                format!("{}{}", crate::principals::EXTERNAL_PREFIX, claim.github_id),
                format!("claimed-by:{}", claim.principal),
            ],
            ..Default::default()
        });
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip() {
        let a = Author {
            login: "al--ice".into(),
            ..Author::default()
        };
        let t = Text {
            body: "see https://hub/x".into(),
            original: "see https://github.com/x -->".into(),
        };
        let b = marked(&t, "comment", 12, &a, "2026-01-01T00:00:00Z");
        assert!(has_marker(&b, "comment", 12));
        assert!(!has_marker(&b, "comment", 1));
        assert!(!b[..b.len() - 3].contains("-->"));
        assert_eq!(marker_id(&b, "comment"), Some(12));
    }
}
