//! Posts on change requests, repo follows and the Following feed, through `PostService`,
//! `UserService` and `ChangeService` over real history (`HistoryVcs` on in-memory stores):
//! text and media rules, who may write, publishing in the land's transaction, agent posts
//! waiting for a person's approval, discard and restore, withdrawal, visibility (private,
//! deleted and signed-out), likes and reposts, the feed's sources, order, paging and polling,
//! agents following and reading the feed, auto-follow on repo creation, `ChangeRequest.post`,
//! media URLs bound to the principal chain, and the upkeep of unattached media; and posts of
//! their own (`CreatePost`): no change request, a Markdown body and an article, attachments
//! checked against the repo and read back as cards, agent posts held for approval, their own
//! media, and the same likes, reposts, adoptions and feed.

use crate::common;
use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams, NewChange};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner, PrincipalDirectory};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::media::MediaSigner;
use bee_forge::orgs::repos::NewRepo;
use bee_forge::posts::service::PostServices;
use bee_forge::posts::{PostPorts, Posts};
use bee_forge::service::ForgeServices;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::post_service_server::PostService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};
use sha2::{Digest, Sha256};

const T0: u64 = 1_800_000_000_000;

const PEOPLE: &[(&str, PrincipalKind)] = &[
    ("owner", PrincipalKind::Human),
    ("alice", PrincipalKind::Human),
    ("writer", PrincipalKind::Human),
    ("reader", PrincipalKind::Human),
    ("outsider", PrincipalKind::Human),
    ("ci", PrincipalKind::Service),
    ("agent", PrincipalKind::Agent),
];

struct World {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    posts: Arc<Posts>,
    vcs: Arc<HistoryVcs>,
    clock: Arc<ManualClock>,
    ring: bee_auth::KeyRing,
    signer: MediaSigner,
    org: Org,
    /// Private; writer, alice, agent and ci write, reader reads.
    app: Repo,
    /// Public; owner only.
    open: Repo,
    s: PostServices,
    u: ForgeServices,
    c: ChangeServices,
}

async fn world() -> World {
    let clock = Arc::new(ManualClock::new(T0));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(dyn_clock.clone()));
    let writer = Writer::new(meta.clone(), blobs);
    let directory = Arc::new(MemoryDirectory::new());
    for (id, kind) in PEOPLE {
        directory.register(
            Principal {
                id: id.to_string(),
                kind: *kind as i32,
                display: id.to_string(),
                ..Default::default()
            },
            vec![format!("{id}-login")],
            vec![format!("{id}@example.com")],
        );
    }
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, _) = Forge::open(
        Arc::new(MemoryStore::new()),
        dyn_clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Hooks {
            directory: directory.clone(),
            provisioner: Arc::new(MetaProvisioner::new(meta, dyn_clock.clone())),
            ..Hooks::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, dyn_clock.clone()));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs.clone(),
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    changes.attach(&forge);
    forge.set_changes(changes.clone());
    let signer = MediaSigner::new("https://hub.example", &[3u8; 32]);
    let posts = Posts::new(
        &forge,
        PostPorts {
            signer: signer.clone(),
            ..PostPorts::default()
        },
    );
    posts.attach(&changes);
    let owner_cx = Cx::new("owner", PrincipalKind::Human);
    let owner = directory.get("owner").await.unwrap();
    let org = forge
        .orgs
        .create_org(&owner_cx, "acme", "", owner)
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let mut repos = Vec::new();
    for (name, visibility) in [("app", Visibility::Private), ("open", Visibility::Public)] {
        let repo = forge
            .orgs
            .create_repo(
                &owner_cx,
                &org.id,
                NewRepo {
                    id: forge.db.ids().ulid().unwrap(),
                    name: name.into(),
                    visibility,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        forge
            .hooks
            .provisioner
            .create_repo(&repo, &bee_forge::hooks::RepoInit::default())
            .await
            .unwrap();
        let root = vcs
            .create_commit(
                &repo,
                NewCommit {
                    parent: None,
                    edits: vec![("README.md".into(), Some(b"# hello\n".to_vec()))],
                    message: "root".into(),
                    author: "owner".into(),
                    creator: "owner".into(),
                    chain: vec!["owner".into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        vcs.set_ref(&repo, "main", &root).await.unwrap();
        repos.push(repo);
    }
    let (app, open) = (repos[0].clone(), repos[1].clone());
    for who in ["reader", "writer", "alice", "agent", "ci"] {
        forge
            .orgs
            .set_member(
                &owner_cx,
                &org.id,
                directory.get(who).await.unwrap(),
                Role::Member,
            )
            .await
            .unwrap();
    }
    for who in ["writer", "alice", "agent", "ci"] {
        forge
            .orgs
            .add_collaborator(
                &owner_cx,
                &app.id,
                directory.get(who).await.unwrap(),
                RepoRole::Write,
                "",
            )
            .await
            .unwrap();
    }
    forge
        .orgs
        .add_collaborator(
            &owner_cx,
            &app.id,
            directory.get("reader").await.unwrap(),
            RepoRole::Read,
            "",
        )
        .await
        .unwrap();
    World {
        s: PostServices::new(forge.clone(), posts.clone()),
        u: ForgeServices::new(forge.clone()),
        c: ChangeServices::new(forge.clone(), changes.clone()),
        forge,
        changes,
        posts,
        vcs,
        clock,
        ring,
        signer,
        org,
        app,
        open,
    }
}

fn rref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(repo.full_name.clone())),
    })
}

fn reason<T: std::fmt::Debug>(r: std::result::Result<tonic::Response<T>, tonic::Status>) -> String {
    common::reason(r)
}

fn subcode<T: std::fmt::Debug>(
    r: std::result::Result<tonic::Response<T>, tonic::Status>,
) -> String {
    let st = r.expect_err("expected failure");
    bee_proto::errors::detail_from_status(&st)
        .map(|d| format!("{}/{}", d.reason, d.subcode))
        .unwrap_or_default()
}

/// A minimal PNG of `w` × `h` (signature and header: what sniffing and sizing read).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

impl World {
    fn kind_of(id: &str) -> PrincipalKind {
        PEOPLE
            .iter()
            .find(|(p, _)| *p == id)
            .map(|(_, k)| *k)
            .unwrap_or(PrincipalKind::Human)
    }

    fn cx(&self, id: &str) -> Cx {
        Cx::new(id, Self::kind_of(id))
    }

    fn mint(&self, id: &str, scope: bee_auth::Scope) -> String {
        let (principal, on_behalf_of) = match Self::kind_of(id) {
            PrincipalKind::Service => (bee_auth::Principal::service(id, id), vec![]),
            PrincipalKind::Agent => (
                bee_auth::Principal::agent(
                    id,
                    id,
                    bee_auth::AgentInfo {
                        owner: bee_policy::PrincipalId::new("alice"),
                        runtime: "claude-code".into(),
                        purpose: "tests".into(),
                    },
                ),
                vec![bee_auth::PrincipalRef::new(
                    "alice",
                    bee_policy::PrincipalKind::Human,
                )],
            ),
            _ => (bee_auth::Principal::human(id, id), vec![]),
        };
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal,
                    on_behalf_of,
                    scope,
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    fn tok(&self, id: &str) -> String {
        self.mint(id, bee_auth::Scope::new())
    }

    /// The server's anonymous read token (a signed-out caller).
    fn anon(&self) -> String {
        let scope = bee_auth::Scope::new().actions([bee_policy::Action::Read]);
        self.ring
            .mint(
                &bee_auth::MintRequest {
                    principal: bee_auth::Principal::human(
                        bee_forge::auth::ANONYMOUS_PRINCIPAL,
                        "anonymous",
                    ),
                    on_behalf_of: vec![],
                    scope,
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    fn tick(&self) {
        self.clock.advance(1_000);
    }

    /// Opens a change request on `repo` as `who`, editing `path`.
    async fn open_cr(&self, who: &str, repo: &Repo, path: &str, title: &str) -> ChangeRequest {
        let tip = self
            .vcs
            .ref_tip(repo, "main")
            .await
            .unwrap()
            .unwrap()
            .target
            .unwrap();
        let head = self
            .vcs
            .create_commit(
                repo,
                NewCommit {
                    parent: Some(tip),
                    edits: vec![(path.into(), Some(format!("{title}\n").into_bytes()))],
                    message: title.into(),
                    author: who.into(),
                    creator: who.into(),
                    chain: vec![who.into()],
                    change_id: None,
                },
            )
            .await
            .unwrap();
        self.changes
            .open(
                &self.cx(who),
                repo,
                NewChange {
                    head: Some(head),
                    title: title.into(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap()
    }

    async fn land(&self, repo: &Repo, cr: &ChangeRequest) -> ChangeRequest {
        self.tick();
        self.changes
            .land(&self.cx("owner"), repo, &cr.id, LandMethod::Rebase, "", 0)
            .await
            .unwrap()
            .0
    }

    async fn set(
        &self,
        who: &str,
        repo: &Repo,
        cr: &ChangeRequest,
        text: &str,
        media: &[String],
    ) -> std::result::Result<tonic::Response<Post>, tonic::Status> {
        self.s
            .set_change_post(common::req(
                &self.tok(who),
                SetChangePostRequest {
                    repo: rref(repo),
                    change_id: cr.id.clone(),
                    text: text.into(),
                    media_ids: media.to_vec(),
                    expected_version: 0,
                    ..Default::default()
                },
            ))
            .await
    }

    /// Uploads `bytes` as `who` for `cr` and finalizes it; returns the media.
    async fn upload(
        &self,
        who: &str,
        repo: &Repo,
        cr: &ChangeRequest,
        content_type: &str,
        bytes: &[u8],
        alt: &str,
    ) -> PostMedia {
        let kind = if content_type.starts_with("video/") {
            PostMediaKind::Video
        } else {
            PostMediaKind::Image
        };
        let b = self
            .s
            .begin_post_media_upload(common::req(
                &self.tok(who),
                BeginPostMediaUploadRequest {
                    repo: rref(repo),
                    change_id: cr.id.clone(),
                    kind: kind as i32,
                    content_type: content_type.into(),
                    size_bytes: bytes.len() as u64,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        // What the signed PUT URL's handler does.
        self.posts
            .receive_media(
                &Cx::system(),
                &repo.id,
                &b.media_id,
                bytes::Bytes::from(bytes.to_vec()),
            )
            .await
            .unwrap();
        self.s
            .finalize_post_media(common::req(
                &self.tok(who),
                FinalizePostMediaRequest {
                    repo: rref(repo),
                    media_id: b.media_id,
                    sha256: sha(bytes),
                    alt_text: alt.into(),
                    width: 640,
                    height: 360,
                    duration_ms: 1500,
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn get_post(&self, who: &str, id: &str) -> Post {
        self.s
            .get_post(common::req(
                &self.tok(who),
                GetPostRequest { post_id: id.into() },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn kinds(&self, repo: &Repo) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&self.cx("owner"), &repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }

    async fn feed(
        &self,
        who: &str,
        page_size: u32,
        token: &str,
        after: &str,
    ) -> ListFollowingFeedResponse {
        self.s
            .list_following_feed(common::req(
                &self.tok(who),
                ListFollowingFeedRequest {
                    page: Some(PageRequest {
                        page_size,
                        page_token: token.into(),
                    }),
                    after: after.into(),
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn follow_repo(&self, token: &str, repo: &Repo) {
        self.u
            .follow_repo(common::req(token, FollowRepoRequest { repo: rref(repo) }))
            .await
            .unwrap();
    }

    /// How many published posts `repo` lists.
    async fn listed(&self, repo: &Repo) -> usize {
        self.s
            .list_repo_posts(common::req(
                &self.tok("owner"),
                ListRepoPostsRequest {
                    repo: rref(repo),
                    page: None,
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .posts
            .len()
    }

    /// `CreatePost` as `who` in `repo`: `r` without its repo.
    async fn create(
        &self,
        who: &str,
        repo: &Repo,
        r: CreatePostRequest,
    ) -> std::result::Result<tonic::Response<Post>, tonic::Status> {
        self.s
            .create_post(common::req(
                &self.tok(who),
                CreatePostRequest {
                    repo: rref(repo),
                    ..r
                },
            ))
            .await
    }

    /// Uploads `bytes` as `who` for a post of its own (no change request) and finalizes it.
    async fn upload_own(&self, who: &str, repo: &Repo, bytes: &[u8], alt: &str) -> PostMedia {
        let b = self
            .s
            .begin_post_media_upload(common::req(
                &self.tok(who),
                BeginPostMediaUploadRequest {
                    repo: rref(repo),
                    change_id: String::new(),
                    kind: PostMediaKind::Image as i32,
                    content_type: "image/png".into(),
                    size_bytes: bytes.len() as u64,
                },
            ))
            .await
            .unwrap()
            .into_inner();
        self.posts
            .receive_media(
                &Cx::system(),
                &repo.id,
                &b.media_id,
                bytes::Bytes::from(bytes.to_vec()),
            )
            .await
            .unwrap();
        self.s
            .finalize_post_media(common::req(
                &self.tok(who),
                FinalizePostMediaRequest {
                    repo: rref(repo),
                    media_id: b.media_id,
                    sha256: sha(bytes),
                    alt_text: alt.into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn issue(&self, who: &str, repo: &Repo, title: &str, inspired_by: &str) -> Issue {
        self.u
            .create_issue(common::req(
                &self.tok(who),
                CreateIssueRequest {
                    repo: rref(repo),
                    title: title.into(),
                    inspired_by_post_id: inspired_by.into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    /// Stores a release row as `ReleaseService` keeps it (this world has no tag store to cut
    /// a release from).
    async fn release(&self, repo: &Repo, id: &str, tag: &str, name: &str, draft: bool) {
        use bee_forge::community::releases::{RELEASES, ReleaseRec};
        let rec = ReleaseRec {
            release: Some(Release {
                tag_name: tag.into(),
                name: name.into(),
                draft,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut tx = self
            .forge
            .db
            .begin(bee_forge::community::SURFACE, &self.cx("owner"))
            .await
            .unwrap();
        tx.insert(
            &RELEASES,
            &bee_forge::store::Row::new(repo.id.clone(), id)
                .k1(tag)
                .n1(T0 as i64)
                .msg(&rec),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// Stores an agent run row as the agents module keeps it (this world runs no agents).
    async fn run(&self, repo: &Repo, id: &str, prompt: &str, state: AgentRunState) {
        use bee_forge::agents::model::RunRec;
        let rec = RunRec {
            run: Some(AgentRun {
                id: id.into(),
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(repo.id.clone())),
                }),
                task: Some(AgentTask {
                    prompt: prompt.into(),
                    ..Default::default()
                }),
                state: state as i32,
                ..Default::default()
            }),
            org_id: repo.org_id.clone(),
            ..Default::default()
        };
        let mut tx = self
            .forge
            .db
            .begin(bee_forge::agents::SURFACE, &self.cx("owner"))
            .await
            .unwrap();
        tx.insert(
            &bee_forge::agents::RUNS,
            &bee_forge::store::Row::new(repo.id.clone(), id)
                .n1(T0 as i64)
                .msg(&rec),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    /// A published post by `who` on `repo` (opened, written, landed).
    async fn published(&self, who: &str, repo: &Repo, path: &str, text: &str) -> Post {
        let cr = self.open_cr(who, repo, path, text).await;
        let p = self
            .set(who, repo, &cr, text, &[])
            .await
            .unwrap()
            .into_inner();
        self.land(repo, &cr).await;
        let p = self.get_post("owner", &p.id).await;
        assert_eq!(p.state(), PostState::Published);
        p
    }
}

#[tokio::test]
async fn text_rules_and_who_may_write() {
    let w = world().await;
    let cr = w
        .open_cr("writer", &w.app, "docs/a.md", "Faster builds")
        .await;
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, &"x".repeat(501), &[]).await),
        "INVALID_ARGUMENT/text_too_long"
    );
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, "bell\u{7}", &[]).await),
        "INVALID_ARGUMENT/bad_text"
    );
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, "", &[]).await),
        "INVALID_ARGUMENT/empty_post"
    );
    // Readers may not write it; outsiders do not see the repo at all.
    assert_eq!(
        reason(w.set("reader", &w.app, &cr, "hi", &[]).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(w.set("outsider", &w.app, &cr, "hi", &[]).await),
        "NOT_FOUND"
    );
    // A token scoped to read is refused.
    let read_only = w.mint(
        "writer",
        bee_auth::Scope::new().actions([bee_policy::Action::Read]),
    );
    let r =
        w.s.set_change_post(common::req(
            &read_only,
            SetChangePostRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                text: "hi".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "POLICY_DENIED");
    // The change's author, then another writer: the author is whoever set the text last,
    // stored as NFC.
    let p = w
        .set(
            "writer",
            &w.app,
            &cr,
            "Builds are 3\u{d7} faster cafe\u{301}",
            &[],
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.text, "Builds are 3\u{d7} faster caf\u{e9}");
    assert_eq!(p.state(), PostState::Draft);
    assert_eq!(p.author.as_ref().unwrap().id, "writer");
    assert_eq!(
        (p.change_number, p.change_title.as_str()),
        (cr.number, "Faster builds")
    );
    assert!(p.viewer.as_ref().unwrap().can_edit);
    let p2 = w
        .set("alice", &w.app, &cr, "Builds are faster", &[])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (p2.id.as_str(), p2.author.as_ref().unwrap().id.as_str()),
        (p.id.as_str(), "alice")
    );
    // A stale expected version is refused.
    let r =
        w.s.set_change_post(common::req(
            &w.tok("alice"),
            SetChangePostRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                text: "again".into(),
                expected_version: p.version,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "VERSION_MISMATCH");
    // Clearing removes it.
    w.s.clear_change_post(common::req(
        &w.tok("writer"),
        ClearChangePostRequest {
            repo: rref(&w.app),
            change_id: cr.id.clone(),
        },
    ))
    .await
    .unwrap();
    let r =
        w.s.get_change_post(common::req(
            &w.tok("writer"),
            GetChangePostRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
}

#[tokio::test]
async fn a_persons_post_publishes_in_the_land_transaction() {
    let w = world().await;
    let cr = w
        .open_cr("writer", &w.app, "docs/a.md", "Cache layers")
        .await;
    let draft = w
        .set("writer", &w.app, &cr, "Layers are cached now #ci", &[])
        .await
        .unwrap()
        .into_inner();
    // The change request carries its post on reads.
    let got =
        w.c.get_change(common::req(
            &w.tok("reader"),
            GetChangeRequest {
                repo: rref(&w.app),
                id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.post.as_ref().unwrap().id, draft.id);
    let landed = w.land(&w.app, &cr).await;
    let p = w.get_post("reader", &draft.id).await;
    assert_eq!(p.state(), PostState::Published);
    assert_eq!(p.published_at, landed.landed_at);
    let kinds = w.kinds(&w.app).await;
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("post.drafted") < at("cr.landed"));
    assert!(at("cr.landed") < at("post.published"));
    // Once published it can no longer change or be cleared.
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, "edit", &[]).await),
        "INVALID_STATE/post_not_editable"
    );
    // The repo lists it.
    let list =
        w.s.list_repo_posts(common::req(
            &w.tok("reader"),
            ListRepoPostsRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.posts.len(), 1);
    // A change that landed without a post gets none afterwards.
    let bare = w.open_cr("writer", &w.app, "docs/b.md", "b").await;
    w.land(&w.app, &bare).await;
    assert_eq!(
        subcode(w.set("writer", &w.app, &bare, "late", &[]).await),
        "INVALID_STATE/post_not_editable"
    );
}

#[tokio::test]
async fn agent_posts_wait_for_a_persons_approval() {
    let w = world().await;
    // Opened by an agent with a post: `OpenChange.post_text`.
    let tip = w
        .vcs
        .ref_tip(&w.app, "main")
        .await
        .unwrap()
        .unwrap()
        .target
        .unwrap();
    let head = w
        .vcs
        .create_commit(
            &w.app,
            NewCommit {
                parent: Some(tip),
                edits: vec![("src/x.rs".into(), Some(b"fn x() {}\n".to_vec()))],
                message: "Port the cache".into(),
                author: "agent".into(),
                creator: "agent".into(),
                chain: vec!["agent".into(), "alice".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    let open = |text: &str| {
        common::req(
            &w.tok("agent"),
            OpenChangeRequest {
                repo: rref(&w.app),
                head: Some(head.clone()),
                title: "Port the cache".into(),
                post_text: text.into(),
                ..Default::default()
            },
        )
    };
    // A refused text opens nothing.
    assert_eq!(
        reason(w.c.open_change(open(&"y".repeat(600))).await),
        "INVALID_ARGUMENT"
    );
    let cr =
        w.c.open_change(open("Ported the layer cache"))
            .await
            .unwrap()
            .into_inner();
    let draft = cr.post.as_ref().expect("post filled").clone();
    assert_eq!(draft.author.as_ref().unwrap().kind(), PrincipalKind::Agent);
    assert_eq!(draft.state(), PostState::Draft);
    let approve = |who: &str| {
        common::req(
            &w.tok(who),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: draft.id.clone(),
            },
        )
    };
    let landed = w.land(&w.app, &cr).await;
    let p = w.get_post("alice", &draft.id).await;
    assert_eq!(p.state(), PostState::PendingApproval);
    assert!(p.published_at.is_none());
    assert!(p.viewer.as_ref().unwrap().can_approve);
    assert!(
        w.kinds(&w.app)
            .await
            .contains(&"post.approval_requested".to_string())
    );
    // Agents, services and readers cannot approve.
    assert_eq!(
        subcode(w.s.approve_post(approve("agent")).await),
        "PERMISSION_DENIED/human_required"
    );
    assert_eq!(
        subcode(w.s.approve_post(approve("ci")).await),
        "PERMISSION_DENIED/human_required"
    );
    assert_eq!(
        reason(w.s.approve_post(approve("reader")).await),
        "PERMISSION_DENIED"
    );
    w.clock.advance(60_000);
    let p =
        w.s.approve_post(approve("alice"))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(p.state(), PostState::Published);
    assert_eq!(p.approved_by.as_ref().unwrap().id, "alice");
    // Published at the approval, after the land.
    assert!(p.published_at.as_ref().unwrap().seconds > landed.landed_at.as_ref().unwrap().seconds);
    assert_eq!(
        reason(w.s.approve_post(approve("writer")).await),
        "INVALID_STATE"
    );
}

#[tokio::test]
async fn approval_before_landing_publishes_on_land_and_edits_clear_it() {
    let w = world().await;
    let cr = w
        .open_cr("agent", &w.app, "src/y.rs", "Tune the pool")
        .await;
    let p = w
        .set("agent", &w.app, &cr, "Pool tuned", &[])
        .await
        .unwrap()
        .into_inner();
    let approve = || {
        common::req(
            &w.tok("writer"),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: p.id.clone(),
            },
        )
    };
    let a = w.s.approve_post(approve()).await.unwrap().into_inner();
    assert_eq!(
        (a.state(), a.approved_by.as_ref().unwrap().id.as_str()),
        (PostState::Draft, "writer")
    );
    // The agent rewrites it: the approval was for other words.
    let edited = w
        .set("agent", &w.app, &cr, "Pool tuned, 2x", &[])
        .await
        .unwrap()
        .into_inner();
    assert!(edited.approved_by.is_none());
    w.s.approve_post(approve()).await.unwrap();
    w.land(&w.app, &cr).await;
    assert_eq!(
        w.get_post("writer", &p.id).await.state(),
        PostState::Published
    );
    // A person's post needs no approval, and nobody approves their own.
    let mine = w.open_cr("writer", &w.app, "docs/z.md", "Mine").await;
    let mp = w
        .set("writer", &w.app, &mine, "mine", &[])
        .await
        .unwrap()
        .into_inner();
    let r =
        w.s.approve_post(common::req(
            &w.tok("writer"),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: mp.id.clone(),
            },
        ))
        .await;
    assert_eq!(subcode(r), "PERMISSION_DENIED/own_post");
    let r =
        w.s.approve_post(common::req(
            &w.tok("alice"),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: mp.id.clone(),
            },
        ))
        .await;
    assert_eq!(subcode(r), "INVALID_STATE/approval_not_needed");
}

#[tokio::test]
async fn closing_discards_and_reopening_restores() {
    let w = world().await;
    let cr = w.open_cr("writer", &w.app, "docs/c.md", "Maybe").await;
    let p = w
        .set("writer", &w.app, &cr, "maybe", &[])
        .await
        .unwrap()
        .into_inner();
    w.changes
        .close(&w.cx("writer"), &w.app, &cr.id, "not now")
        .await
        .unwrap();
    assert_eq!(
        w.get_post("writer", &p.id).await.state(),
        PostState::Discarded
    );
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, "edit", &[]).await),
        "INVALID_STATE/post_not_editable"
    );
    w.changes
        .reopen(&w.cx("writer"), &w.app, &cr.id)
        .await
        .unwrap();
    assert_eq!(w.get_post("writer", &p.id).await.state(), PostState::Draft);
    w.land(&w.app, &cr).await;
    assert_eq!(
        w.get_post("writer", &p.id).await.state(),
        PostState::Published
    );
}

#[tokio::test]
async fn media_rules_and_urls_bound_to_the_chain() {
    let w = world().await;
    let cr = w
        .open_cr("writer", &w.app, "docs/m.md", "Screenshots")
        .await;
    let img = png(800, 600);
    // Begin: the type and size are checked.
    let begin = |ct: &str, size: u64, kind: PostMediaKind| {
        common::req(
            &w.tok("writer"),
            BeginPostMediaUploadRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                kind: kind as i32,
                content_type: ct.into(),
                size_bytes: size,
            },
        )
    };
    assert_eq!(
        subcode(
            w.s.begin_post_media_upload(begin("image/svg+xml", 10, PostMediaKind::Image))
                .await
        ),
        "INVALID_ARGUMENT/bad_media"
    );
    assert_eq!(
        reason(
            w.s.begin_post_media_upload(begin("image/png", (8 << 20) + 1, PostMediaKind::Image))
                .await
        ),
        "TOO_LARGE"
    );
    assert!(
        w.s.begin_post_media_upload(begin("video/mp4", 64 << 20, PostMediaKind::Video))
            .await
            .is_ok()
    );
    assert_eq!(
        reason(
            w.s.begin_post_media_upload(begin("video/mp4", (64 << 20) + 1, PostMediaKind::Video))
                .await
        ),
        "TOO_LARGE"
    );
    let b =
        w.s.begin_post_media_upload(begin("image/png", img.len() as u64, PostMediaKind::Image))
            .await
            .unwrap()
            .into_inner();
    assert!(b.upload_url.starts_with(&format!(
        "https://hub.example/posts/{}/{}?",
        w.app.id, b.media_id
    )));
    assert_eq!(
        b.upload_headers.get("content-type").map(String::as_str),
        Some("image/png")
    );
    // HTML declared as an image is refused by its bytes.
    let e = w
        .posts
        .receive_media(
            &Cx::system(),
            &w.app.id,
            &b.media_id,
            bytes::Bytes::from_static(b"<html>"),
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "bad_media");
    w.posts
        .receive_media(
            &Cx::system(),
            &w.app.id,
            &b.media_id,
            bytes::Bytes::from(img.clone()),
        )
        .await
        .unwrap();
    let fin = |sha256: String, alt: &str| {
        common::req(
            &w.tok("writer"),
            FinalizePostMediaRequest {
                repo: rref(&w.app),
                media_id: b.media_id.clone(),
                sha256,
                alt_text: alt.into(),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        reason(
            w.s.finalize_post_media(fin("ab".repeat(32), "a chart"))
                .await
        ),
        "INTEGRITY"
    );
    assert_eq!(
        subcode(w.s.finalize_post_media(fin(sha(&img), "")).await),
        "INVALID_ARGUMENT/alt_text_required"
    );
    let m =
        w.s.finalize_post_media(fin(sha(&img), "A chart of build times"))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        (m.width, m.height, m.content_type.as_str()),
        (800, 600, "image/png")
    );
    // Not attached yet: a draft cannot use media that is not finalized.
    let pending =
        w.s.begin_post_media_upload(begin("image/png", img.len() as u64, PostMediaKind::Image))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        subcode(
            w.set(
                "writer",
                &w.app,
                &cr,
                "shots",
                std::slice::from_ref(&pending.media_id)
            )
            .await
        ),
        "INVALID_STATE/media_not_final"
    );
    // At most four images; a video stands alone.
    let mut five = Vec::new();
    for i in 0..5 {
        five.push(
            w.upload("writer", &w.app, &cr, "image/png", &png(10 + i, 10), "shot")
                .await
                .id,
        );
    }
    assert_eq!(
        subcode(w.set("writer", &w.app, &cr, "shots", &five).await),
        "INVALID_ARGUMENT/bad_media"
    );
    let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x84];
    webm.extend_from_slice(b"webm");
    webm.extend_from_slice(&[0u8; 32]);
    let video = w
        .upload("writer", &w.app, &cr, "video/webm", &webm, "")
        .await;
    assert_eq!(
        (video.kind(), video.duration_ms),
        (PostMediaKind::Video, 1500)
    );
    assert_eq!(
        subcode(
            w.set(
                "writer",
                &w.app,
                &cr,
                "both",
                &[video.id.clone(), m.id.clone()]
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_media"
    );
    // Media of another change request cannot be attached.
    let other = w.open_cr("writer", &w.app, "docs/o.md", "Other").await;
    assert_eq!(
        subcode(
            w.set(
                "writer",
                &w.app,
                &other,
                "steal",
                std::slice::from_ref(&m.id)
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_media"
    );
    // Four images, empty text: fine.
    let p = w
        .set("writer", &w.app, &cr, "", &five[..4])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.media.len(), 4);
    // Every read signs each URL for the reader's principal chain.
    let seen = w.get_post("reader", &p.id).await;
    let url = &seen.media[0].url;
    let (path, query) = url["https://hub.example".len()..].split_once('?').unwrap();
    let (key, chain) = w
        .signer
        .verify_download("posts/", path, query, w.clock.now_ms())
        .unwrap();
    assert_eq!(key, format!("posts/{}/{}", w.app.id, five[0]));
    assert_eq!(chain, vec![("reader".to_string(), "human".to_string())]);
    let agent = w.get_post("agent", &p.id).await;
    let url = &agent.media[0].url;
    let (path, query) = url["https://hub.example".len()..].split_once('?').unwrap();
    let (_, chain) = w
        .signer
        .verify_download("posts/", path, query, w.clock.now_ms())
        .unwrap();
    assert_eq!(
        chain,
        vec![
            ("agent".to_string(), "agent".to_string()),
            ("alice".to_string(), "human".to_string())
        ]
    );
    // Served once finalized and attached; the media of the fifth upload stays unattached.
    let rec = w
        .posts
        .media_for_download(&Cx::system(), &w.app.id, &five[0])
        .await
        .unwrap();
    assert_eq!(
        w.posts.media_bytes(&rec).await.unwrap().len(),
        png(10, 10).len()
    );
    assert!(
        w.posts
            .media_for_download(&Cx::system(), &w.app.id, &pending.media_id)
            .await
            .is_err()
    );
    // A day later the upkeep deletes every unattached upload (the fifth image, the video, the
    // first image, the pending and the big video reservations), never attached ones.
    assert_eq!(w.posts.maintain(&Cx::system()).await.unwrap(), 0);
    w.clock.advance(25 * 3600 * 1000);
    assert_eq!(w.posts.maintain(&Cx::system()).await.unwrap(), 5);
    assert!(
        w.posts
            .media_for_download(&Cx::system(), &w.app.id, &five[4])
            .await
            .is_err()
    );
    assert!(
        w.posts
            .media_for_download(&Cx::system(), &w.app.id, &five[0])
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn withdrawal_and_visibility() {
    let w = world().await;
    let p = w
        .published("writer", &w.app, "docs/w.md", "Shipped search")
        .await;
    // Private: readers of the repo see it, outsiders and signed-out callers do not.
    let r =
        w.s.get_post(common::req(
            &w.tok("outsider"),
            GetPostRequest {
                post_id: p.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    let r =
        w.s.get_post(common::req(
            &w.anon(),
            GetPostRequest {
                post_id: p.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    // Naming the wrong repo is NOT_FOUND too.
    let r =
        w.s.withdraw_post(common::req(
            &w.tok("writer"),
            WithdrawPostRequest {
                repo: rref(&w.open),
                post_id: p.id.clone(),
                reason: String::new(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    // Public: signed-out callers read it.
    let pubp = w
        .published("owner", &w.open, "docs/p.md", "Public news")
        .await;
    let seen =
        w.s.get_post(common::req(
            &w.anon(),
            GetPostRequest {
                post_id: pubp.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(seen.text, "Public news");
    let v = seen.viewer.unwrap();
    assert!(!v.can_edit && !v.can_withdraw && !v.can_approve);
    let list =
        w.s.list_repo_posts(common::req(
            &w.anon(),
            ListRepoPostsRequest {
                repo: rref(&w.open),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.posts.len(), 1);
    // Withdrawing: not another writer, but its author or an admin.
    let withdraw = |who: &str| {
        common::req(
            &w.tok(who),
            WithdrawPostRequest {
                repo: rref(&w.app),
                post_id: p.id.clone(),
                reason: "wrong numbers".into(),
            },
        )
    };
    assert_eq!(
        reason(w.s.withdraw_post(withdraw("alice")).await),
        "PERMISSION_DENIED"
    );
    let gone =
        w.s.withdraw_post(withdraw("writer"))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(gone.state(), PostState::Withdrawn);
    assert_eq!(
        (gone.text.as_str(), gone.withdrawn_reason.as_str()),
        ("", "wrong numbers")
    );
    assert!(
        w.kinds(&w.app)
            .await
            .contains(&"post.withdrawn".to_string())
    );
    let list =
        w.s.list_repo_posts(common::req(
            &w.tok("reader"),
            ListRepoPostsRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(list.posts.is_empty());
    // An admin withdraws someone else's post.
    let q = w.published("alice", &w.app, "docs/q.md", "Another").await;
    let r =
        w.s.withdraw_post(common::req(
            &w.tok("owner"),
            WithdrawPostRequest {
                repo: rref(&w.app),
                post_id: q.id.clone(),
                reason: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.state(), PostState::Withdrawn);
    // Posts of a deleted repo are never shown.
    w.forge
        .orgs
        .delete_repo(&w.cx("owner"), &w.open.id, &w.open.full_name)
        .await
        .unwrap();
    let r =
        w.s.get_post(common::req(
            &w.tok("owner"),
            GetPostRequest {
                post_id: pubp.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
}

#[tokio::test]
async fn likes_and_reposts_count_once() {
    let w = world().await;
    let cr = w.open_cr("writer", &w.app, "docs/l.md", "Likeable").await;
    let draft = w
        .set("writer", &w.app, &cr, "like me", &[])
        .await
        .unwrap()
        .into_inner();
    let like = |who: &str| {
        common::req(
            &w.tok(who),
            LikePostRequest {
                post_id: draft.id.clone(),
            },
        )
    };
    assert_eq!(
        subcode(w.s.like_post(like("reader")).await),
        "INVALID_STATE/post_not_published"
    );
    w.land(&w.app, &cr).await;
    w.s.like_post(like("reader")).await.unwrap();
    let p = w.s.like_post(like("reader")).await.unwrap().into_inner();
    assert_eq!(p.counts.as_ref().unwrap().likes, 1);
    assert!(p.viewer.as_ref().unwrap().liked);
    let p = w.s.like_post(like("agent")).await.unwrap().into_inner();
    assert_eq!(p.counts.as_ref().unwrap().likes, 2);
    let p =
        w.s.unlike_post(common::req(
            &w.tok("reader"),
            UnlikePostRequest {
                post_id: draft.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            p.counts.as_ref().unwrap().likes,
            p.viewer.as_ref().unwrap().liked
        ),
        (1, false)
    );
    let p =
        w.s.repost_post(common::req(
            &w.tok("alice"),
            RepostPostRequest {
                post_id: draft.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            p.counts.as_ref().unwrap().reposts,
            p.viewer.as_ref().unwrap().reposted
        ),
        (1, true)
    );
    w.s.undo_repost(common::req(
        &w.tok("alice"),
        UndoRepostRequest {
            post_id: draft.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.get_post("alice", &draft.id).await.counts.unwrap().reposts,
        0
    );
    // Outsiders cannot even see it; signed-out callers cannot like.
    assert_eq!(reason(w.s.like_post(like("outsider")).await), "NOT_FOUND");
    let r =
        w.s.like_post(common::req(
            &w.anon(),
            LikePostRequest {
                post_id: draft.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "UNAUTHENTICATED");
    let kinds = w.kinds(&w.app).await;
    assert_eq!(kinds.iter().filter(|k| *k == "post.liked").count(), 2);
    assert_eq!(kinds.iter().filter(|k| *k == "post.reposted").count(), 1);
}

#[tokio::test]
async fn repo_follows_for_people_and_agents() {
    let w = world().await;
    let agent = w.tok("agent");
    w.follow_repo(&agent, &w.app).await;
    w.follow_repo(&agent, &w.app).await;
    let is =
        w.u.is_following_repo(common::req(
            &agent,
            IsFollowingRepoRequest { repo: rref(&w.app) },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(is.following);
    let followers =
        w.u.list_repo_followers(common::req(
            &w.tok("reader"),
            ListRepoFollowersRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<&str> = followers.followers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["agent"]);
    // Agents are named by id; only readable repos are listed.
    let listed =
        w.u.list_followed_repos(common::req(
            &w.tok("reader"),
            ListFollowedReposRequest {
                user: "agent".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.repos.len(), 1);
    let hidden =
        w.u.list_followed_repos(common::req(
            &w.tok("outsider"),
            ListFollowedReposRequest {
                user: "agent".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(hidden.repos.is_empty());
    // Outsiders cannot follow a private repo; signed-out callers cannot follow at all.
    let r =
        w.u.follow_repo(common::req(
            &w.tok("outsider"),
            FollowRepoRequest { repo: rref(&w.app) },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    let r =
        w.u.follow_repo(common::req(
            &w.anon(),
            FollowRepoRequest {
                repo: rref(&w.open),
            },
        ))
        .await;
    assert_eq!(reason(r), "UNAUTHENTICATED");
    w.u.unfollow_repo(common::req(
        &agent,
        UnfollowRepoRequest { repo: rref(&w.app) },
    ))
    .await
    .unwrap();
    let is =
        w.u.is_following_repo(common::req(
            &agent,
            IsFollowingRepoRequest { repo: rref(&w.app) },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!is.following);
    assert!(
        w.kinds(&w.app)
            .await
            .contains(&"follow.repo_removed".to_string())
    );
}

#[tokio::test]
async fn a_repos_creator_follows_it() {
    let w = world().await;
    w.forge.flush().await.unwrap();
    for repo in [&w.app, &w.open] {
        assert!(
            w.forge
                .community
                .users
                .is_following_repo(&Cx::system(), "owner", &repo.id)
                .await
                .unwrap()
        );
    }
    // An agent creating a repo follows it too.
    let repo = w
        .forge
        .orgs
        .create_repo(
            &w.cx("agent"),
            &w.org.id,
            NewRepo {
                id: w.forge.db.ids().ulid().unwrap(),
                name: "by-agent".into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    w.forge.flush().await.unwrap();
    assert!(
        w.forge
            .community
            .users
            .is_following_repo(&Cx::system(), "agent", &repo.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_following_feed() {
    let w = world().await;
    let reader = w.tok("reader");
    let empty = w.feed("reader", 10, "", "").await;
    assert!(empty.items.is_empty());
    // An empty feed still returns a position to poll from.
    assert_eq!(empty.newest_cursor, format!("{}|0", w.clock.now_ms()));
    // reader follows the app repo and the person alice.
    w.follow_repo(&reader, &w.app).await;
    w.u.follow_user(common::req(
        &reader,
        FollowUserRequest {
            login: "alice-login".into(),
        },
    ))
    .await
    .unwrap();
    let a1 = w.published("writer", &w.app, "docs/1.md", "one").await;
    let a2 = w.published("writer", &w.app, "docs/2.md", "two").await;
    // alice posts on the public repo: in reader's feed through alice.
    let o1 = w.published("alice", &w.open, "docs/3.md", "three").await;
    // owner posts on the public repo: nothing reader follows leads there.
    let o2 = w.published("owner", &w.open, "docs/4.md", "four").await;
    // A draft never appears.
    let d = w.open_cr("writer", &w.app, "docs/5.md", "five").await;
    w.set("writer", &w.app, &d, "five", &[]).await.unwrap();
    let ids = |r: &ListFollowingFeedResponse| -> Vec<String> {
        r.items
            .iter()
            .map(|i| i.post.as_ref().unwrap().id.clone())
            .collect()
    };
    let f = w.feed("reader", 10, "", "").await;
    assert_eq!(ids(&f), vec![o1.id.clone(), a2.id.clone(), a1.id.clone()]);
    assert!(f.items.iter().all(|i| i.reposted_by.is_none()));
    assert_eq!(f.items[0].at, o1.published_at);
    // alice reposts o2: it appears once, under the repost, newest.
    w.tick();
    w.s.repost_post(common::req(
        &w.tok("alice"),
        RepostPostRequest {
            post_id: o2.id.clone(),
        },
    ))
    .await
    .unwrap();
    // alice also reposts a1, which reader already gets through the repo: it moves up, once.
    w.tick();
    w.s.repost_post(common::req(
        &w.tok("alice"),
        RepostPostRequest {
            post_id: a1.id.clone(),
        },
    ))
    .await
    .unwrap();
    let f = w.feed("reader", 10, "", "").await;
    assert_eq!(
        ids(&f),
        vec![a1.id.clone(), o2.id.clone(), o1.id.clone(), a2.id.clone()]
    );
    assert_eq!(f.items[0].reposted_by.as_ref().unwrap().id, "alice");
    assert_eq!(f.items[1].reposted_by.as_ref().unwrap().id, "alice");
    // Paging: two by two, nothing repeated or lost.
    let p1 = w.feed("reader", 2, "", "").await;
    let token = p1.page.as_ref().unwrap().next_page_token.clone();
    assert!(!token.is_empty());
    let p2 = w.feed("reader", 2, &token, "").await;
    assert_eq!(
        [ids(&p1), ids(&p2)].concat(),
        vec![a1.id.clone(), o2.id.clone(), o1.id.clone(), a2.id.clone()]
    );
    assert!(p2.page.as_ref().unwrap().next_page_token.is_empty());
    // A malformed cursor.
    let r =
        w.s.list_following_feed(common::req(
            &reader,
            ListFollowingFeedRequest {
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: "nope".into(),
                }),
                after: String::new(),
            },
        ))
        .await;
    assert_eq!(subcode(r), "INVALID_ARGUMENT/bad_page_token");
    // A withdrawn post leaves the feed; losing access hides the repo's posts.
    w.s.withdraw_post(common::req(
        &w.tok("writer"),
        WithdrawPostRequest {
            repo: rref(&w.app),
            post_id: a2.id.clone(),
            reason: String::new(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        ids(&w.feed("reader", 10, "", "").await),
        vec![a1.id.clone(), o2.id.clone(), o1.id.clone()]
    );
    w.forge
        .orgs
        .remove_collaborator(&w.cx("owner"), &w.app.id, "reader")
        .await
        .unwrap();
    // Role unspecified removes the membership (and the org's base role with it).
    w.forge
        .orgs
        .set_member(
            &w.cx("owner"),
            &w.org.id,
            Principal {
                id: "reader".into(),
                ..Default::default()
            },
            Role::Unspecified,
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&w.feed("reader", 10, "", "").await),
        vec![o2.id.clone(), o1.id.clone()]
    );
}

#[tokio::test]
async fn agents_poll_the_feed_with_a_cursor() {
    let w = world().await;
    let agent = w.tok("agent");
    w.follow_repo(&agent, &w.open).await;
    let first = w.published("owner", &w.open, "docs/a.md", "a").await;
    let f = w.feed("agent", 10, "", "").await;
    assert_eq!(f.items.len(), 1);
    let cursor = f.newest_cursor.clone();
    assert_eq!(
        cursor,
        format!(
            "{}|{}",
            first
                .published_at
                .as_ref()
                .map(|t| t.seconds as u64 * 1000 + t.nanos as u64 / 1_000_000)
                .unwrap(),
            first.id
        )
    );
    // Nothing new: the cursor comes back unchanged.
    let none = w.feed("agent", 10, "", &cursor).await;
    assert!(none.items.is_empty());
    assert_eq!(none.newest_cursor, cursor);
    let b = w.published("owner", &w.open, "docs/b.md", "b").await;
    let c = w.published("owner", &w.open, "docs/c.md", "c").await;
    // Newer items come oldest first, capped by the page size.
    let one = w.feed("agent", 1, "", &cursor).await;
    assert_eq!(one.items[0].post.as_ref().unwrap().id, b.id);
    let rest = w.feed("agent", 10, "", &one.newest_cursor).await;
    assert_eq!(rest.items.len(), 1);
    assert_eq!(rest.items[0].post.as_ref().unwrap().id, c.id);
    let r =
        w.s.list_following_feed(common::req(
            &agent,
            ListFollowingFeedRequest {
                page: None,
                after: "garbage".into(),
            },
        ))
        .await;
    assert_eq!(subcode(r), "INVALID_ARGUMENT/bad_page_token");
    // Signed-out callers have no feed.
    let r =
        w.s.list_following_feed(common::req(&w.anon(), ListFollowingFeedRequest::default()))
            .await;
    assert_eq!(reason(r), "UNAUTHENTICATED");
}

#[tokio::test]
async fn an_empty_feed_hands_out_a_cursor_to_poll_from() {
    let w = world().await;
    let agent = w.tok("agent");
    w.follow_repo(&agent, &w.open).await;
    let empty = w.feed("agent", 10, "", "").await;
    assert!(empty.items.is_empty());
    let cursor = empty.newest_cursor.clone();
    assert_eq!(cursor, format!("{}|0", w.clock.now_ms()));
    let first = w.published("owner", &w.open, "docs/a.md", "a").await;
    let polled = w.feed("agent", 10, "", &cursor).await;
    assert_eq!(polled.items.len(), 1);
    assert_eq!(polled.items[0].post.as_ref().unwrap().id, first.id);
    // The `<ms>|` form (no id) is accepted too.
    let bare = cursor.replace("|0", "|");
    let polled = w.feed("agent", 10, "", &bare).await;
    assert_eq!(polled.items.len(), 1);
    // Feed items name their repo, for agents as much as for the web.
    let repo = polled.items[0].post.as_ref().unwrap().repo.clone().unwrap();
    assert_eq!(
        repo.repo,
        Some(repo_ref::Repo::FullName(w.open.full_name.clone()))
    );
    // So do GetPost and ListRepoPosts.
    let got = w.get_post("agent", &first.id).await;
    assert_eq!(
        got.repo.unwrap().repo,
        Some(repo_ref::Repo::FullName(w.open.full_name.clone()))
    );
    for bad in ["garbage", "x|id", "|id"] {
        let r =
            w.s.list_following_feed(common::req(
                &agent,
                ListFollowingFeedRequest {
                    page: None,
                    after: bad.into(),
                },
            ))
            .await;
        assert_eq!(subcode(r), "INVALID_ARGUMENT/bad_page_token", "{bad}");
    }
}

#[tokio::test]
async fn list_changes_carries_each_post() {
    let w = world().await;
    let a = w.open_cr("writer", &w.app, "docs/a.md", "a").await;
    let b = w.open_cr("writer", &w.app, "docs/b.md", "b").await;
    let pa = w
        .set("writer", &w.app, &a, "post a", &[])
        .await
        .unwrap()
        .into_inner();
    let list =
        w.c.list_changes(common::req(
            &w.tok("reader"),
            ListChangesRequest {
                repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let by_id = |id: &str| list.changes.iter().find(|c| c.id == id).unwrap();
    assert_eq!(by_id(&a.id).post.as_ref().unwrap().id, pa.id);
    assert!(by_id(&b.id).post.is_none());
}

fn attach(kind: PostAttachmentKind, id: &str) -> PostAttachment {
    PostAttachment {
        kind: kind as i32,
        id: id.into(),
        ..Default::default()
    }
}

fn article(title: &str, body: &str) -> Option<PostArticle> {
    Some(PostArticle {
        title: title.into(),
        body_markdown: body.into(),
    })
}

#[tokio::test]
async fn a_post_of_its_own_needs_no_change_request() {
    let w = world().await;
    let p = w
        .create(
            "writer",
            &w.app,
            CreatePostRequest {
                text: "Builds are 3\u{d7} faster".into(),
                body_markdown: "## What changed\r\n\r\n- layers are cached, cafe\u{301}\n".into(),
                article: article(
                    "  How we cut build time  ",
                    "# Before\r\n\r\n\tmake all\r\n",
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    // Published at once, by its author, with no change request and nothing attached.
    assert_eq!(p.state(), PostState::Published);
    assert!(p.published_at.is_some());
    assert_eq!(p.author.as_ref().unwrap().id, "writer");
    assert_eq!(
        (
            p.change_id.as_str(),
            p.change_number,
            p.change_title.as_str()
        ),
        ("", 0, "")
    );
    assert!(p.attachments.is_empty());
    // The body and the article come back as stored: NFC, `\n` line ends, the title trimmed.
    assert_eq!(p.text, "Builds are 3\u{d7} faster");
    assert_eq!(
        p.body_markdown,
        "## What changed\n\n- layers are cached, caf\u{e9}\n"
    );
    assert_eq!(
        p.article,
        article("How we cut build time", "# Before\n\n\tmake all\n")
    );
    let v = p.viewer.as_ref().unwrap();
    assert!(!v.can_edit && !v.can_approve && v.can_withdraw);
    assert!(
        w.kinds(&w.app)
            .await
            .contains(&"post.published".to_string())
    );
    // Readers of the repo read it by id and in the repo's list; nobody else does.
    let got = w.get_post("reader", &p.id).await;
    assert_eq!(
        (&got.body_markdown, &got.article),
        (&p.body_markdown, &p.article)
    );
    assert!(!got.viewer.as_ref().unwrap().can_withdraw);
    let list =
        w.s.list_repo_posts(common::req(
            &w.tok("reader"),
            ListRepoPostsRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.posts.len(), 1);
    assert_eq!(list.posts[0].id, p.id);
    let r =
        w.s.get_post(common::req(
            &w.tok("outsider"),
            GetPostRequest {
                post_id: p.id.clone(),
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    // A body alone is a post; nothing at all is not.
    let body_only = w
        .create(
            "writer",
            &w.app,
            CreatePostRequest {
                body_markdown: "Only a body.".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (body_only.text.as_str(), body_only.body_markdown.as_str()),
        ("", "Only a body.")
    );
    assert!(body_only.article.is_none());
    assert_eq!(
        subcode(
            w.create("writer", &w.app, CreatePostRequest::default())
                .await
        ),
        "INVALID_ARGUMENT/empty_post"
    );
    // The limits: summary, body, article and its title.
    let with = |f: fn(&mut CreatePostRequest)| {
        let mut r = CreatePostRequest {
            text: "a summary".into(),
            ..Default::default()
        };
        f(&mut r);
        r
    };
    for (r, want) in [
        (
            with(|r| r.text = "x".repeat(501)),
            "INVALID_ARGUMENT/text_too_long",
        ),
        (
            with(|r| r.body_markdown = "x".repeat(20_001)),
            "INVALID_ARGUMENT/body_too_long",
        ),
        (
            with(|r| r.body_markdown = "bell\u{7}".into()),
            "INVALID_ARGUMENT/bad_text",
        ),
        (
            with(|r| r.article = article("Title", &"x".repeat(100_001))),
            "INVALID_ARGUMENT/article_too_long",
        ),
        (
            with(|r| r.article = article("", "A body without a title.")),
            "INVALID_ARGUMENT/bad_article",
        ),
        (
            with(|r| r.article = article("A title without a body", "")),
            "INVALID_ARGUMENT/bad_article",
        ),
    ] {
        assert_eq!(subcode(w.create("writer", &w.app, r).await), want);
    }
    // A body at the limit is accepted.
    assert!(
        w.create(
            "writer",
            &w.app,
            with(|r| r.body_markdown = "x".repeat(20_000))
        )
        .await
        .is_ok()
    );
    // Who may write: repo Write with a token that allows it.
    let hi = || CreatePostRequest {
        text: "hi".into(),
        ..Default::default()
    };
    assert_eq!(
        reason(w.create("reader", &w.app, hi()).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(w.create("outsider", &w.app, hi()).await),
        "NOT_FOUND"
    );
    let read_only = w.mint(
        "writer",
        bee_auth::Scope::new().actions([bee_policy::Action::Read]),
    );
    let r =
        w.s.create_post(common::req(
            &read_only,
            CreatePostRequest {
                repo: rref(&w.app),
                ..hi()
            },
        ))
        .await;
    assert_eq!(reason(r), "POLICY_DENIED");
    // Its author withdraws it: the words are gone, the permalink stays.
    let gone =
        w.s.withdraw_post(common::req(
            &w.tok("writer"),
            WithdrawPostRequest {
                repo: rref(&w.app),
                post_id: p.id.clone(),
                reason: "wrong numbers".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(gone.state(), PostState::Withdrawn);
    assert!(gone.text.is_empty() && gone.body_markdown.is_empty() && gone.article.is_none());
}

#[tokio::test]
async fn attachments_are_checked_and_read_back_as_cards() {
    use PostAttachmentKind as K;
    let w = world().await;
    let cr1 = w
        .open_cr("writer", &w.app, "docs/a.md", "Cache layers")
        .await;
    let cr2 = w
        .open_cr("writer", &w.app, "docs/b.md", "Trim images")
        .await;
    let issue = w.issue("writer", &w.app, "Builds are slow", "").await;
    w.release(&w.app, "01RELEASE", "v1.2.0", "Bee 1.2", false)
        .await;
    w.run(
        &w.app,
        "01RUN",
        "Speed up the build\n\nStart with the cache.",
        AgentRunState::Succeeded,
    )
    .await;
    let post = |attachments: Vec<PostAttachment>| CreatePostRequest {
        text: "The build work, in one place".into(),
        attachments,
        ..Default::default()
    };
    // Change requests and issues by id or by number; the rest by id.
    let p = w
        .create(
            "writer",
            &w.app,
            post(vec![
                attach(K::ChangeRequest, &cr1.id),
                PostAttachment {
                    kind: K::ChangeRequest as i32,
                    number: cr2.number,
                    ..Default::default()
                },
                attach(K::Release, "01RELEASE"),
                PostAttachment {
                    kind: K::Issue as i32,
                    number: issue.number,
                    ..Default::default()
                },
                attach(K::AgentRun, "01RUN"),
            ]),
        )
        .await
        .unwrap()
        .into_inner();
    let card =
        |kind: K, id: &str, number: u64, title: &str, state: &str, tag: &str| PostAttachment {
            kind: kind as i32,
            id: id.into(),
            number,
            title: title.into(),
            state: state.into(),
            tag_name: tag.into(),
        };
    let want = vec![
        card(
            K::ChangeRequest,
            &cr1.id,
            cr1.number,
            "Cache layers",
            "open",
            "",
        ),
        card(
            K::ChangeRequest,
            &cr2.id,
            cr2.number,
            "Trim images",
            "open",
            "",
        ),
        card(K::Release, "01RELEASE", 0, "Bee 1.2", "published", "v1.2.0"),
        card(
            K::Issue,
            &issue.id,
            issue.number,
            "Builds are slow",
            "open",
            "",
        ),
        card(
            K::AgentRun,
            "01RUN",
            0,
            "Speed up the build",
            "succeeded",
            "",
        ),
    ];
    assert_eq!(p.attachments, want);
    assert!(p.change_id.is_empty());
    // Stored as references; the cards are read from the attachments as they are now.
    let (rec, _) = w.posts.get(&w.cx("owner"), &w.app.id, &p.id).await.unwrap();
    assert_eq!(
        rec.post().attachments,
        vec![
            attach(K::ChangeRequest, &cr1.id),
            attach(K::ChangeRequest, &cr2.id),
            attach(K::Release, "01RELEASE"),
            attach(K::Issue, &issue.id),
            attach(K::AgentRun, "01RUN"),
        ]
    );
    w.land(&w.app, &cr1).await;
    let got = w.get_post("reader", &p.id).await;
    assert_eq!(got.attachments[0].state, "landed");
    assert_eq!(got.attachments[1..], want[1..]);
    // Something from another repository is refused, whatever its kind, and so is anything
    // that does not exist or that the post's readers could not read (a draft release).
    let elsewhere = w.open_cr("owner", &w.open, "docs/c.md", "Elsewhere").await;
    let their_issue = w.issue("owner", &w.open, "Theirs", "").await;
    w.release(&w.open, "01THEIRS", "v9.0.0", "Theirs", false)
        .await;
    w.run(&w.open, "01THEIRRUN", "Theirs", AgentRunState::Running)
        .await;
    w.release(&w.app, "01DRAFT", "v2.0.0", "Not out yet", true)
        .await;
    for bad in [
        attach(K::ChangeRequest, &elsewhere.id),
        attach(K::Issue, &their_issue.id),
        attach(K::Release, "01THEIRS"),
        attach(K::AgentRun, "01THEIRRUN"),
        attach(K::ChangeRequest, "01NOSUCHCHANGE"),
        PostAttachment {
            kind: K::Issue as i32,
            number: 9_999,
            ..Default::default()
        },
        attach(K::Release, "01DRAFT"),
    ] {
        assert_eq!(
            subcode(w.create("writer", &w.app, post(vec![bad.clone()])).await),
            "INVALID_ARGUMENT/attachment_not_found",
            "{bad:?}"
        );
    }
    // Lists the rules refuse: the reserved swarm kind, no kind, the same attachment twice
    // (by id and by number), two issues.
    let other_issue = w.issue("writer", &w.app, "Another", "").await;
    for bad in [
        vec![attach(K::Swarm, "01SWARM")],
        vec![attach(K::Unspecified, &cr2.id)],
        vec![
            attach(K::ChangeRequest, &cr2.id),
            PostAttachment {
                kind: K::ChangeRequest as i32,
                number: cr2.number,
                ..Default::default()
            },
        ],
        vec![
            attach(K::Issue, &issue.id),
            attach(K::Issue, &other_issue.id),
        ],
    ] {
        assert_eq!(
            subcode(w.create("writer", &w.app, post(bad.clone())).await),
            "INVALID_ARGUMENT/bad_attachment",
            "{bad:?}"
        );
    }
    // A refused post is not written.
    let list =
        w.s.list_repo_posts(common::req(
            &w.tok("reader"),
            ListRepoPostsRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.posts.len(), 1);
}

#[tokio::test]
async fn a_change_requests_post_reads_back_with_its_change_attached() {
    use PostAttachmentKind as K;
    let w = world().await;
    // The shape every post had before attachments existed: text on a change request.
    let cr = w
        .open_cr("writer", &w.app, "docs/a.md", "Cache layers")
        .await;
    let draft = w
        .set("writer", &w.app, &cr, "Layers are cached now", &[])
        .await
        .unwrap()
        .into_inner();
    let (rec, _) = w
        .posts
        .get(&w.cx("owner"), &w.app.id, &draft.id)
        .await
        .unwrap();
    assert!(rec.post().attachments.is_empty(), "nothing new is stored");
    assert!(rec.post().body_markdown.is_empty() && rec.post().article.is_none());
    let own = |state: &str| PostAttachment {
        kind: K::ChangeRequest as i32,
        id: cr.id.clone(),
        number: cr.number,
        title: "Cache layers".into(),
        state: state.into(),
        tag_name: String::new(),
    };
    assert_eq!(draft.attachments, vec![own("open")]);
    assert_eq!(draft.change_id, cr.id);
    // The newer fields on the same RPC: a body, an article and attachments beside the change
    // request itself, which stays first and is never listed twice.
    let issue = w.issue("writer", &w.app, "Builds are slow", "").await;
    let full =
        w.s.set_change_post(common::req(
            &w.tok("writer"),
            SetChangePostRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                text: "Layers are cached now".into(),
                body_markdown: "Each layer is keyed by its inputs.".into(),
                article: article("Layer caching", "## Keys\n"),
                attachments: vec![
                    attach(K::Issue, &issue.id),
                    attach(K::ChangeRequest, &cr.id),
                ],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(full.id, draft.id);
    assert_eq!(full.body_markdown, "Each layer is keyed by its inputs.");
    assert_eq!(full.article, article("Layer caching", "## Keys\n"));
    assert_eq!(full.attachments.len(), 2);
    assert_eq!(full.attachments[0], own("open"));
    assert_eq!(
        (
            full.attachments[1].kind(),
            full.attachments[1].id.as_str(),
            full.attachments[1].number,
            full.attachments[1].title.as_str(),
            full.attachments[1].state.as_str()
        ),
        (
            K::Issue,
            issue.id.as_str(),
            issue.number,
            "Builds are slow",
            "open"
        )
    );
    // It is the change request's post, as before.
    let on_change =
        w.s.get_change_post(common::req(
            &w.tok("reader"),
            GetChangePostRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(on_change.attachments, full.attachments);
    // A client that knows only text and media replaces the rest with nothing.
    let plain = w
        .set("writer", &w.app, &cr, "Layers are cached", &[])
        .await
        .unwrap()
        .into_inner();
    assert!(plain.body_markdown.is_empty() && plain.article.is_none());
    assert_eq!(plain.attachments, vec![own("open")]);
    // Landing publishes it; the card follows the change request.
    w.land(&w.app, &cr).await;
    let p = w.get_post("reader", &draft.id).await;
    assert_eq!(p.state(), PostState::Published);
    assert_eq!(p.attachments, vec![own("landed")]);
    assert_eq!(
        (p.change_number, p.change_title.as_str()),
        (cr.number, "Cache layers")
    );
}

#[tokio::test]
async fn an_agents_own_post_waits_for_a_persons_approval() {
    let w = world().await;
    let p = w
        .create(
            "agent",
            &w.app,
            CreatePostRequest {
                text: "I ported the layer cache".into(),
                body_markdown: "Three change requests, all landed.".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.author.as_ref().unwrap().kind(), PrincipalKind::Agent);
    assert_eq!(p.state(), PostState::PendingApproval);
    assert!(p.published_at.is_none());
    assert!(!p.viewer.as_ref().unwrap().can_approve);
    let kinds = w.kinds(&w.app).await;
    assert!(kinds.contains(&"post.approval_requested".to_string()));
    assert!(!kinds.contains(&"post.published".to_string()));
    // Not published: not in the repo's list, not likeable.
    assert_eq!(w.listed(&w.app).await, 0);
    let like =
        w.s.like_post(common::req(
            &w.tok("reader"),
            LikePostRequest {
                post_id: p.id.clone(),
            },
        ))
        .await;
    assert_eq!(subcode(like), "INVALID_STATE/post_not_published");
    let approve = |who: &str| {
        common::req(
            &w.tok(who),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: p.id.clone(),
            },
        )
    };
    // Agents, services and readers cannot approve; a person with Write can.
    assert_eq!(
        subcode(w.s.approve_post(approve("agent")).await),
        "PERMISSION_DENIED/human_required"
    );
    assert_eq!(
        subcode(w.s.approve_post(approve("ci")).await),
        "PERMISSION_DENIED/human_required"
    );
    assert_eq!(
        reason(w.s.approve_post(approve("reader")).await),
        "PERMISSION_DENIED"
    );
    assert!(w.get_post("alice", &p.id).await.viewer.unwrap().can_approve);
    w.clock.advance(60_000);
    let approved =
        w.s.approve_post(approve("alice"))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(approved.state(), PostState::Published);
    assert_eq!(approved.approved_by.as_ref().unwrap().id, "alice");
    assert!(approved.published_at.is_some());
    assert_eq!(w.listed(&w.app).await, 1);
    assert!(
        w.kinds(&w.app)
            .await
            .contains(&"post.published".to_string())
    );
    // A person's own post needs no approval.
    let mine = w
        .create(
            "writer",
            &w.app,
            CreatePostRequest {
                text: "mine".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    let r =
        w.s.approve_post(common::req(
            &w.tok("alice"),
            ApprovePostRequest {
                repo: rref(&w.app),
                post_id: mine.id.clone(),
            },
        ))
        .await;
    assert_eq!(subcode(r), "INVALID_STATE/approval_not_needed");
}

#[tokio::test]
async fn own_posts_carry_media_uploaded_by_their_author() {
    let w = world().await;
    let img = png(800, 600);
    let m = w.upload_own("writer", &w.app, &img, "A chart").await;
    // Uploading for a post of one's own needs Write, like the post.
    let begin = |who: &str| {
        common::req(
            &w.tok(who),
            BeginPostMediaUploadRequest {
                repo: rref(&w.app),
                change_id: String::new(),
                kind: PostMediaKind::Image as i32,
                content_type: "image/png".into(),
                size_bytes: img.len() as u64,
            },
        )
    };
    assert_eq!(
        reason(w.s.begin_post_media_upload(begin("reader")).await),
        "PERMISSION_DENIED"
    );
    // Only the uploader finalizes and attaches it.
    let pending =
        w.s.begin_post_media_upload(begin("writer"))
            .await
            .unwrap()
            .into_inner();
    let r =
        w.s.finalize_post_media(common::req(
            &w.tok("alice"),
            FinalizePostMediaRequest {
                repo: rref(&w.app),
                media_id: pending.media_id.clone(),
                sha256: sha(&img),
                alt_text: "not mine".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "NOT_FOUND");
    let with = |ids: Vec<String>| CreatePostRequest {
        text: "with a chart".into(),
        media_ids: ids,
        ..Default::default()
    };
    assert_eq!(
        subcode(w.create("alice", &w.app, with(vec![m.id.clone()])).await),
        "INVALID_ARGUMENT/bad_media"
    );
    assert_eq!(
        subcode(
            w.create("writer", &w.app, with(vec![pending.media_id.clone()]))
                .await
        ),
        "INVALID_STATE/media_not_final"
    );
    // Media uploaded for a change request stays with that change request.
    let cr = w.open_cr("writer", &w.app, "docs/m.md", "Charts").await;
    let theirs = w
        .upload("writer", &w.app, &cr, "image/png", &img, "On the change")
        .await;
    assert_eq!(
        subcode(
            w.create("writer", &w.app, with(vec![theirs.id.clone()]))
                .await
        ),
        "INVALID_ARGUMENT/bad_media"
    );
    let p = w
        .create("writer", &w.app, with(vec![m.id.clone()]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.media.len(), 1);
    assert_eq!(
        (p.media[0].id.as_str(), p.media[0].alt_text.as_str()),
        (m.id.as_str(), "A chart")
    );
    assert!(!p.media[0].url.is_empty());
    // One post per upload.
    assert_eq!(
        subcode(w.create("writer", &w.app, with(vec![m.id.clone()])).await),
        "INVALID_ARGUMENT/bad_media"
    );
    // Media alone is a post.
    let m2 = w.upload_own("writer", &w.app, &img, "Another chart").await;
    let bare = w
        .create(
            "writer",
            &w.app,
            CreatePostRequest {
                media_ids: vec![m2.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    assert!(bare.text.is_empty());
    assert_eq!(bare.media.len(), 1);
}

#[tokio::test]
async fn own_posts_are_liked_reposted_adopted_and_in_the_feed() {
    let w = world().await;
    let reader = w.tok("reader");
    w.follow_repo(&reader, &w.open).await;
    // A post of its own on the public repo, then an older-style post on a landed change.
    let own = w
        .create(
            "owner",
            &w.open,
            CreatePostRequest {
                text: "How we cut build time".into(),
                article: article("Caching, step by step", "## One\n"),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_inner();
    w.tick();
    let landed = w.published("owner", &w.open, "docs/1.md", "one").await;
    let f = w.feed("reader", 10, "", "").await;
    let ids: Vec<String> = f
        .items
        .iter()
        .map(|i| i.post.as_ref().unwrap().id.clone())
        .collect();
    assert_eq!(ids, vec![landed.id.clone(), own.id.clone()]);
    let in_feed = f.items[1].post.as_ref().unwrap();
    assert_eq!(in_feed.article, own.article);
    assert!(in_feed.attachments.is_empty());
    assert_eq!(f.items[1].at, own.published_at);
    assert_eq!(f.items[0].post.as_ref().unwrap().attachments.len(), 1);
    // Likes and reposts count as on any published post.
    let p =
        w.s.like_post(common::req(
            &reader,
            LikePostRequest {
                post_id: own.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.counts.as_ref().unwrap().likes, 1);
    assert_eq!(p.counts.as_ref().unwrap().comments, 0);
    let p =
        w.s.repost_post(common::req(
            &w.tok("alice"),
            RepostPostRequest {
                post_id: own.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p.counts.as_ref().unwrap().reposts, 1);
    assert!(p.viewer.as_ref().unwrap().reposted);
    // Adopting: an issue names the post as its inspiration. The link needs nothing of a
    // change request, so the reference carries the post and its repo alone.
    let issue = w
        .issue("writer", &w.app, "Cache our layers too", &own.id)
        .await;
    let by = issue.inspired_by.expect("inspired_by on the created issue");
    assert_eq!(by.post_id, own.id);
    assert_eq!(by.repo, rref(&w.open));
    assert_eq!((by.change_number, by.change_title.as_str()), (0, ""));
    assert_eq!(
        w.get_post("writer", &own.id)
            .await
            .counts
            .as_ref()
            .unwrap()
            .adoptions,
        1
    );
    let adoptions =
        w.s.list_post_adoptions(common::req(
            &w.tok("writer"),
            ListPostAdoptionsRequest {
                post_id: own.id.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(adoptions.adoptions.len(), 1);
    assert_eq!(adoptions.adoptions[0].title, "Cache our layers too");
}
