//! Agents adopting followed upgrades, over real history (`HistoryVcs` on in-memory stores)
//! with the deterministic `FakeRunner` (`docs/spec/agents.md` §5 "Feed triggers",
//! `docs/spec/forge-community.md` "Posts and feeds"):
//!
//! - provenance: `inspired_by_post_id` on issues and change requests needs a published post
//!   the caller can read (`post_not_readable` otherwise), `inspired_by` is shown only to
//!   readers of the post's repo, adoptions are counted and listed per caller, follow their
//!   issue across a transfer and go with it when it is deleted;
//! - the feed trigger: who may configure it, its sources followed as the agent, one debounced
//!   ADOPT_EVALUATE run per post (folded within the window, idempotent on redelivery), the
//!   prompt's context, the run cap and the budget, and the run's outcome.

use crate::common;
use std::sync::Arc;

use bee_auth::{AgentInfo, InMemoryRevocations, KeyRing, MintRequest, PrincipalRef, Scope};
use bee_forge::Cx;
use bee_forge::Forge;
use bee_forge::agents::fake::{FakeRunner, FakeScript};
use bee_forge::agents::local::LocalProvider;
use bee_forge::agents::service::AgentApi;
use bee_forge::agents::{
    AgentDefinition, AgentPorts, Agents, AgentsConfig, KeyRingIssuer, MemoryBlobs,
};
use bee_forge::changes::service::ChangeServices;
use bee_forge::changes::vcs::NewCommit;
use bee_forge::changes::{ChangeHooks, ChangeVcs, Changes, HistoryVcs, MemoryTeams, NewChange};
use bee_forge::forge::Hooks;
use bee_forge::hooks::{MemoryDirectory, MetaProvisioner};
use bee_forge::ids::{Clock, ManualClock};
use bee_forge::media::MediaSigner;
use bee_forge::orgs::repos::NewRepo;
use bee_forge::posts::service::PostServices;
use bee_forge::posts::{PostPorts, Posts};
use bee_forge::service::ForgeServices;
use bee_forge::store::memory::MemoryStore;
use bee_proto::v1::agent_service_server::AgentService;
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::post_service_server::PostService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use bee_store::{MemoryBlobStore, MemoryMetaStore, Writer};

use common::req;

const T0: u64 = 1_790_000_000_000;
const WINDOW_MS: u64 = 60_000;

/// `(id, kind, owner)`. `maint` maintains `app`; `scout` is maint's agent; `reader` reads
/// `app` only; `poster` writes `upstream` and `vault`; `outsider` is in no repo.
const PEOPLE: &[(&str, PrincipalKind, &str)] = &[
    ("owner", PrincipalKind::Human, ""),
    ("maint", PrincipalKind::Human, ""),
    ("reader", PrincipalKind::Human, ""),
    ("poster", PrincipalKind::Human, ""),
    ("outsider", PrincipalKind::Human, ""),
    ("scout", PrincipalKind::Agent, "maint"),
    ("idle", PrincipalKind::Agent, "maint"),
];

fn ring() -> KeyRing {
    KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap())
}

fn principal(id: &str) -> Principal {
    let (_, k, o) = PEOPLE
        .iter()
        .find(|(p, _, _)| *p == id)
        .copied()
        .unwrap_or((id, PrincipalKind::Human, ""));
    Principal {
        id: id.into(),
        kind: k as i32,
        display: id.into(),
        owner_id: o.into(),
        ..Default::default()
    }
}

fn cx(id: &str) -> Cx {
    let p = principal(id);
    let c = Cx::new(id, PrincipalKind::try_from(p.kind).unwrap());
    if p.owner_id.is_empty() {
        c
    } else {
        c.with_chain(vec![id.into(), p.owner_id])
    }
}

fn rref(r: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(r.full_name.clone())),
    })
}

fn named(r: &Repo) -> RepoRef {
    rref(r).unwrap()
}

fn subcode<T: std::fmt::Debug>(
    r: std::result::Result<tonic::Response<T>, tonic::Status>,
) -> String {
    let st = r.expect_err("expected failure");
    bee_proto::errors::detail_from_status(&st)
        .map(|d| format!("{}/{}", d.reason, d.subcode))
        .unwrap_or_else(|| format!("no detail: {st:?}"))
}

fn ok<T>(r: std::result::Result<tonic::Response<T>, tonic::Status>) -> T {
    match r {
        Ok(r) => r.into_inner(),
        Err(e) => panic!("call failed: {e:?}"),
    }
}

struct W {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    posts: Arc<Posts>,
    vcs: Arc<HistoryVcs>,
    agents: Arc<Agents>,
    runner: Arc<FakeRunner>,
    clock: Arc<ManualClock>,
    api: AgentApi,
    svc: ForgeServices,
    ps: PostServices,
    cs: ChangeServices,
    /// Private, the adopting repo: maint maintains it, scout writes, reader reads.
    app: Repo,
    /// Public, the followed repo: poster writes.
    upstream: Repo,
    /// Private: poster writes, nobody else reads.
    vault: Repo,
    _dir: tempfile::TempDir,
}

async fn world() -> W {
    let clock = Arc::new(ManualClock::new(T0));
    let dynclock: Arc<dyn Clock> = clock.clone();
    let meta = Arc::new(MemoryMetaStore::new());
    let writer = Writer::new(
        meta.clone(),
        Arc::new(MemoryBlobStore::new(dynclock.clone())),
    );
    let directory = Arc::new(MemoryDirectory::new());
    for (id, _, _) in PEOPLE {
        directory.register(principal(id), vec![format!("{id}-login")], vec![]);
    }
    let revocations = Arc::new(InMemoryRevocations::new());
    let (forge, _) = Forge::open(
        Arc::new(MemoryStore::new()),
        dynclock.clone(),
        ring().verifying_keys(),
        revocations.clone(),
        Hooks {
            directory: directory.clone(),
            provisioner: Arc::new(MetaProvisioner::new(meta, dynclock.clone())),
            ..Hooks::default()
        },
        common::test_config(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(writer, dynclock.clone()));
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
    let posts = Posts::new(
        &forge,
        PostPorts {
            signer: MediaSigner::new("https://hub.example", &[3u8; 32]),
            ..PostPorts::default()
        },
    );
    posts.attach(&changes);
    let runner = Arc::new(FakeRunner::with_forge(
        changes.clone(),
        vcs.clone(),
        forge.leases.clone(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let agents = Agents::new(
        forge.clone(),
        changes.clone(),
        AgentPorts {
            runner: runner.clone(),
            issuer: Arc::new(KeyRingIssuer::new(
                ring(),
                Some(revocations.clone()),
                dynclock.clone(),
            )),
            blobs: Arc::new(MemoryBlobs::new()),
            repo: vcs.clone(),
            workspaces: Arc::new(LocalProvider::new(dir.path().join("ws"), vcs.clone())),
        },
        AgentsConfig {
            watch_debounce: std::time::Duration::from_millis(WINDOW_MS),
            ..AgentsConfig::default()
        },
    );
    agents.attach();
    let c = Cx::new("owner", PrincipalKind::Human);
    let org = forge
        .orgs
        .create_org(&c, "acme", "", principal("owner"))
        .await
        .unwrap();
    forge.hooks.provisioner.create_org(&org).await.unwrap();
    let mut repos = Vec::new();
    for (name, visibility) in [
        ("app", Visibility::Private),
        ("upstream", Visibility::Public),
        ("vault", Visibility::Private),
    ] {
        let repo = forge
            .orgs
            .create_repo(
                &c,
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
                    edits: vec![("README.md".into(), Some(format!("# {name}\n").into_bytes()))],
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
    let (app, upstream, vault) = (repos[0].clone(), repos[1].clone(), repos[2].clone());
    // Members see private repos only through a repo role (base role none).
    forge
        .orgs
        .update_settings(&c, &org.id, 0, |s| s.base_repo_role = RepoRole::None as i32)
        .await
        .unwrap();
    for who in ["maint", "reader", "poster", "scout", "idle"] {
        forge
            .orgs
            .set_member(&c, &org.id, principal(who), Role::Member)
            .await
            .unwrap();
    }
    for (who, repo, role) in [
        ("maint", &app, RepoRole::Maintain),
        ("scout", &app, RepoRole::Write),
        ("idle", &app, RepoRole::Write),
        ("reader", &app, RepoRole::Read),
        ("poster", &upstream, RepoRole::Write),
        ("poster", &vault, RepoRole::Write),
    ] {
        forge
            .orgs
            .add_collaborator(&c, &repo.id, principal(who), role, "")
            .await
            .unwrap();
    }
    for (agent, enabled) in [("scout", true), ("idle", false)] {
        agents
            .define_agent(
                &cx("maint"),
                AgentDefinition {
                    org_id: org.id.clone(),
                    agent: principal(agent),
                    name: agent.into(),
                    runtime: "claude-code".into(),
                    toolsets: vec!["read".into(), "commit".into()],
                    default_budget: AgentRunBudget {
                        max_usd_micros: 5_000_000,
                        ..Default::default()
                    },
                    owners: vec!["maint".into()],
                    enabled,
                    ..AgentDefinition::default()
                },
            )
            .await
            .unwrap();
    }
    W {
        api: AgentApi::new(agents.clone()),
        svc: ForgeServices::new(forge.clone()),
        ps: PostServices::new(forge.clone(), posts.clone()),
        cs: ChangeServices::new(forge.clone(), changes.clone()),
        forge,
        changes,
        posts,
        vcs,
        agents,
        runner,
        clock,
        app,
        upstream,
        vault,
        _dir: dir,
    }
}

impl W {
    fn token(&self, id: &str) -> String {
        let p = principal(id);
        let (principal, obo) = if p.kind == PrincipalKind::Agent as i32 {
            (
                bee_auth::Principal::agent(
                    id,
                    id,
                    AgentInfo {
                        owner: bee_policy::PrincipalId::new(p.owner_id.clone()),
                        runtime: "claude-code".into(),
                        purpose: "t".into(),
                    },
                ),
                vec![PrincipalRef::new(
                    p.owner_id.clone(),
                    bee_policy::PrincipalKind::Human,
                )],
            )
        } else {
            (bee_auth::Principal::human(id, id), vec![])
        };
        ring()
            .mint(
                &MintRequest {
                    principal,
                    on_behalf_of: obo,
                    scope: Scope::new(),
                },
                self.clock.now_ms() / 1000,
            )
            .unwrap()
            .text
    }

    async fn settle(&self) {
        for _ in 0..4 {
            self.forge.flush().await.unwrap();
            self.agents.dispatch_once().await.unwrap();
        }
        self.forge.flush().await.unwrap();
    }

    /// A change request on `repo` by `who` editing `path`, with a post draft.
    async fn draft(
        &self,
        who: &str,
        repo: &Repo,
        path: &str,
        title: &str,
        text: &str,
    ) -> ChangeRequest {
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
        let cr = self
            .changes
            .open(
                &cx(who),
                repo,
                NewChange {
                    head: Some(head),
                    title: title.into(),
                    ..NewChange::default()
                },
            )
            .await
            .unwrap();
        self.posts
            .set(&cx(who), repo, &cr.id, text, &[], 0)
            .await
            .unwrap();
        cr
    }

    /// A published post: `who`'s change with a post, landed.
    async fn publish(&self, who: &str, repo: &Repo, path: &str, title: &str, text: &str) -> Post {
        let cr = self.draft(who, repo, path, title, text).await;
        self.clock.advance(1_000);
        self.changes
            .land(&cx("owner"), repo, &cr.id, LandMethod::Rebase, "", 0)
            .await
            .unwrap();
        let (rec, _) = self
            .posts
            .for_change(&Cx::system(), &repo.id, &cr.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.state(), PostState::Published);
        rec.post().clone()
    }

    async fn set_trigger(
        &self,
        who: &str,
        agent: &str,
        sources: &[&Repo],
        budget: f64,
        cap: u32,
    ) -> std::result::Result<tonic::Response<AgentFeedTrigger>, tonic::Status> {
        self.api
            .set_agent_feed_trigger(req(
                &self.token(who),
                SetAgentFeedTriggerRequest {
                    repo: rref(&self.app),
                    agent: agent.into(),
                    enabled: true,
                    daily_usd_budget: budget,
                    max_runs_per_day: cap,
                    sources: sources.iter().map(|r| named(r)).collect(),
                },
            ))
            .await
    }

    async fn follows(&self, who: &str, repo: &Repo) -> bool {
        ok(self
            .svc
            .is_following_repo(req(
                &self.token(who),
                IsFollowingRepoRequest { repo: rref(repo) },
            ))
            .await)
        .following
    }

    async fn adopt_runs(&self) -> Vec<AgentRun> {
        let mut runs = self
            .agents
            .list_runs(
                &Cx::system(),
                std::slice::from_ref(&self.app.id),
                "scout",
                &[],
                0,
                "",
                "",
                1000,
            )
            .await
            .unwrap()
            .0;
        runs.retain(|r| {
            r.task.as_ref().map(|t| t.kind) == Some(AgentTaskKind::AdoptEvaluate as i32)
        });
        runs
    }

    async fn create_issue(
        &self,
        who: &str,
        repo: &Repo,
        title: &str,
        post: &str,
    ) -> std::result::Result<tonic::Response<Issue>, tonic::Status> {
        self.svc
            .create_issue(req(
                &self.token(who),
                CreateIssueRequest {
                    repo: rref(repo),
                    title: title.into(),
                    inspired_by_post_id: post.into(),
                    ..Default::default()
                },
            ))
            .await
    }

    async fn get_issue(&self, who: &str, id: &str) -> Issue {
        ok(self
            .svc
            .get_issue(req(
                &self.token(who),
                GetIssueRequest {
                    id: id.into(),
                    ..Default::default()
                },
            ))
            .await)
    }

    async fn post_as(&self, who: &str, id: &str) -> Post {
        ok(self
            .ps
            .get_post(req(&self.token(who), GetPostRequest { post_id: id.into() }))
            .await)
    }

    async fn adoptions(&self, who: &str, id: &str) -> Vec<PostAdoption> {
        ok(self
            .ps
            .list_post_adoptions(req(
                &self.token(who),
                ListPostAdoptionsRequest {
                    post_id: id.into(),
                    page: None,
                },
            ))
            .await)
        .adoptions
    }

    async fn event_kinds(&self, repo: &Repo) -> Vec<String> {
        self.forge.flush().await.unwrap();
        let (events, _) = self
            .forge
            .events
            .read(&Cx::system(), &repo.id, 0, &[], 10_000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    }
}

// ----- provenance -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspired_by_needs_a_published_post_the_caller_reads() {
    let w = world().await;
    let public = w
        .publish(
            "poster",
            &w.upstream,
            "src/cache.rs",
            "Cache the build layers",
            "3x faster",
        )
        .await;
    let private = w
        .publish("poster", &w.vault, "src/x.rs", "Secret speedup", "hush")
        .await;
    let draft = w
        .draft("poster", &w.upstream, "src/y.rs", "Not landed", "soon")
        .await;
    let draft_id = w
        .posts
        .for_change(&Cx::system(), &w.upstream.id, &draft.id)
        .await
        .unwrap()
        .unwrap()
        .0
        .post()
        .id
        .clone();
    // Missing, unpublished, and unreadable posts are refused alike, and open nothing.
    for bad in [
        "01NOSUCHPOST0000000000000A",
        draft_id.as_str(),
        private.id.as_str(),
    ] {
        assert_eq!(
            subcode(w.create_issue("maint", &w.app, "Port it", bad).await),
            "INVALID_ARGUMENT/post_not_readable",
            "{bad}"
        );
    }
    let open = ok(w
        .svc
        .list_issues(req(
            &w.token("maint"),
            ListIssuesRequest {
                repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await);
    assert!(open.issues.is_empty(), "a refused link opens nothing");
    // A published post the caller reads: the link is stored and shown.
    let issue = ok(w
        .create_issue("maint", &w.app, "Cache our build layers too", &public.id)
        .await);
    let by = issue
        .inspired_by
        .clone()
        .expect("inspired_by on the created issue");
    assert_eq!(by.post_id, public.id);
    assert_eq!(by.repo, rref(&w.upstream));
    assert_eq!(by.change_title, "Cache the build layers");
    assert_eq!(by.change_number, public.change_number);
    assert_eq!(
        w.get_issue("reader", &issue.id).await.inspired_by,
        Some(by.clone())
    );
    // The poster of a private post may link it in a repo they write; readers of that repo who
    // cannot read the source see no link.
    let poster_issue = ok(w
        .create_issue("poster", &w.upstream, "Bring the speedup over", &private.id)
        .await);
    assert_eq!(
        poster_issue.inspired_by.as_ref().map(|p| p.post_id.clone()),
        Some(private.id.clone())
    );
    let seen_by_outsider = ok(w
        .svc
        .get_issue(req(
            &w.token("outsider"),
            GetIssueRequest {
                id: poster_issue.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(
        seen_by_outsider.inspired_by, None,
        "the private source stays hidden"
    );
    // Lists carry it too.
    let listed = ok(w
        .svc
        .list_issues(req(
            &w.token("reader"),
            ListIssuesRequest {
                repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(listed.issues[0].inspired_by, Some(by));
    // The event is recorded in the adopting repo, naming the post by id only.
    assert!(
        w.event_kinds(&w.app)
            .await
            .contains(&"post.adopted".to_string())
    );
    let (events, _) = w
        .forge
        .events
        .read(&Cx::system(), &w.upstream.id, 0, &[], 10_000)
        .await
        .unwrap();
    let adopted: Vec<&Event> = events.iter().filter(|e| e.kind == "post.adopted").collect();
    assert_eq!(adopted.len(), 1, "the poster's own adoption, in upstream");
    assert!(adopted[0].payload_json.contains(&private.id));
    assert!(!adopted[0].payload_json.contains("Secret speedup"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_requests_carry_inspired_by_and_adoptions_are_filtered_per_caller() {
    let w = world().await;
    let post = w
        .publish(
            "poster",
            &w.upstream,
            "src/retry.rs",
            "Back off between retries",
            "capped",
        )
        .await;
    // A change request in the private repo, inspired by the public post.
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
                edits: vec![("src/retry.rs".into(), Some(b"backoff\n".to_vec()))],
                message: "Back off too".into(),
                author: "maint".into(),
                creator: "maint".into(),
                chain: vec!["maint".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    let bad =
        w.cs.open_change(req(
            &w.token("maint"),
            OpenChangeRequest {
                repo: rref(&w.app),
                head: Some(head.clone()),
                title: "Back off too".into(),
                inspired_by_post_id: "01NOSUCHPOST0000000000000A".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(subcode(bad), "INVALID_ARGUMENT/post_not_readable");
    let cr = ok(w
        .cs
        .open_change(req(
            &w.token("maint"),
            OpenChangeRequest {
                repo: rref(&w.app),
                head: Some(head),
                title: "Back off too".into(),
                inspired_by_post_id: post.id.clone(),
                ..Default::default()
            },
        ))
        .await);
    assert_eq!(
        cr.inspired_by.as_ref().map(|p| p.post_id.clone()),
        Some(post.id.clone())
    );
    let got = ok(w
        .cs
        .get_change(req(
            &w.token("reader"),
            GetChangeRequest {
                repo: rref(&w.app),
                id: cr.id.clone(),
            },
        ))
        .await);
    assert_eq!(got.inspired_by, cr.inspired_by);
    // And a public issue in upstream itself.
    let issue = ok(w
        .create_issue("poster", &w.upstream, "Document the backoff", &post.id)
        .await);
    // Readers of app see both adoptions; others see only the public one.
    for (who, n) in [("maint", 2), ("reader", 2), ("outsider", 1), ("poster", 1)] {
        assert_eq!(
            w.post_as(who, &post.id).await.counts.unwrap().adoptions,
            n,
            "{who}"
        );
        let list = w.adoptions(who, &post.id).await;
        assert_eq!(list.len(), n as usize, "{who}");
        assert!(
            list.iter()
                .any(|a| a.subject.as_ref().unwrap().id == issue.id)
        );
    }
    let mine = w.adoptions("maint", &post.id).await;
    // Newest first: the issue was opened after the change request.
    assert_eq!(
        mine[0].subject.as_ref().unwrap().kind,
        SubjectKind::Issue as i32
    );
    assert_eq!(
        mine[1].subject.as_ref().unwrap().kind,
        SubjectKind::ChangeRequest as i32
    );
    assert_eq!(mine[1].repo, rref(&w.app));
    assert_eq!(mine[1].title, "Back off too");
    assert_eq!(mine[1].author.as_ref().unwrap().id, "maint");
    // Pages.
    let first = ok(w
        .ps
        .list_post_adoptions(req(
            &w.token("maint"),
            ListPostAdoptionsRequest {
                post_id: post.id.clone(),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token: String::new(),
                }),
            },
        ))
        .await);
    let token = first.page.unwrap().next_page_token;
    assert!(!token.is_empty());
    let second = ok(w
        .ps
        .list_post_adoptions(req(
            &w.token("maint"),
            ListPostAdoptionsRequest {
                post_id: post.id.clone(),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token: token,
                }),
            },
        ))
        .await);
    assert_eq!(second.adoptions[0].subject, mine[1].subject);
    let garbage =
        w.ps.list_post_adoptions(req(
            &w.token("maint"),
            ListPostAdoptionsRequest {
                post_id: post.id.clone(),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token: "nope".into(),
                }),
            },
        ))
        .await;
    assert_eq!(subcode(garbage), "INVALID_ARGUMENT/bad_page_token");
    // A deleted issue's adoption goes with it.
    ok(w.svc
        .delete_issue(req(
            &w.token("owner"),
            DeleteIssueRequest {
                repo: rref(&w.upstream),
                number: issue.number,
            },
        ))
        .await);
    assert_eq!(
        w.post_as("outsider", &post.id)
            .await
            .counts
            .unwrap()
            .adoptions,
        0
    );
    assert_eq!(w.adoptions("maint", &post.id).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_adoption_follows_its_issue_across_a_transfer() {
    let w = world().await;
    let post = w
        .publish(
            "poster",
            &w.upstream,
            "src/a.rs",
            "Stream the logs",
            "live logs",
        )
        .await;
    let issue = ok(w
        .create_issue("owner", &w.upstream, "Stream ours", &post.id)
        .await);
    assert_eq!(
        w.post_as("outsider", &post.id)
            .await
            .counts
            .unwrap()
            .adoptions,
        1
    );
    ok(w.svc
        .transfer_issue(req(
            &w.token("owner"),
            TransferIssueRequest {
                repo: rref(&w.upstream),
                number: issue.number,
                target_repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await);
    // Now in the private repo: the outsider no longer sees it; readers of app do.
    assert_eq!(
        w.post_as("outsider", &post.id)
            .await
            .counts
            .unwrap()
            .adoptions,
        0
    );
    let list = w.adoptions("reader", &post.id).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].repo, rref(&w.app));
    assert_eq!(
        w.get_issue("reader", &issue.id)
            .await
            .inspired_by
            .unwrap()
            .post_id,
        post.id
    );
}

// ----- the feed trigger -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feed_triggers_are_configured_by_maintainers_for_their_agents() {
    let w = world().await;
    // Repo maintain is needed; the agent must be enabled and the caller's.
    assert_eq!(
        subcode(
            w.set_trigger("reader", "scout", &[&w.upstream], 0.0, 0)
                .await
        ),
        "PERMISSION_DENIED/"
    );
    assert_eq!(
        subcode(w.set_trigger("maint", "idle", &[&w.upstream], 0.0, 0).await),
        "INVALID_ARGUMENT/no_agent"
    );
    assert_eq!(
        subcode(
            w.set_trigger("maint", "ghost", &[&w.upstream], 0.0, 0)
                .await
        ),
        "NOT_FOUND/"
    );
    // Sources must be readable by the agent, and never the trigger's own repo.
    assert_eq!(
        subcode(w.set_trigger("maint", "scout", &[&w.vault], 0.0, 0).await),
        "INVALID_ARGUMENT/source_not_readable"
    );
    assert_eq!(
        subcode(w.set_trigger("maint", "scout", &[&w.app], 0.0, 0).await),
        "INVALID_ARGUMENT/source_is_target"
    );
    assert_eq!(
        subcode(
            w.set_trigger("maint", "scout", &[&w.upstream], -1.0, 0)
                .await
        ),
        "INVALID_ARGUMENT/invalid_budget"
    );
    assert!(!w.follows("scout", &w.upstream).await);
    let t = ok(w
        .set_trigger("maint", "scout-login", &[&w.upstream], 2.5, 3)
        .await);
    assert_eq!(t.agent.as_ref().unwrap().id, "scout");
    assert_eq!(t.repo, rref(&w.app));
    assert_eq!(t.sources, vec![named(&w.upstream)]);
    assert_eq!((t.daily_usd_budget, t.max_runs_per_day), (2.5, 3));
    assert_eq!(t.check_interval_seconds, WINDOW_MS / 1000);
    assert!(t.enabled && t.created_at.is_some());
    // The agent follows its sources.
    assert!(w.follows("scout", &w.upstream).await);
    // Readers of the repo see it; others do not.
    let got = ok(w
        .api
        .get_agent_feed_trigger(req(
            &w.token("reader"),
            GetAgentFeedTriggerRequest {
                repo: rref(&w.app),
                agent: "scout".into(),
            },
        ))
        .await);
    assert_eq!(got, t);
    let listed = ok(w
        .api
        .list_agent_feed_triggers(req(
            &w.token("reader"),
            ListAgentFeedTriggersRequest {
                repo: rref(&w.app),
                page: None,
            },
        ))
        .await);
    assert_eq!(listed.triggers, vec![t.clone()]);
    assert_eq!(
        subcode(
            w.api
                .list_agent_feed_triggers(req(
                    &w.token("outsider"),
                    ListAgentFeedTriggersRequest {
                        repo: rref(&w.app),
                        page: None,
                    },
                ))
                .await
        ),
        "NOT_FOUND/"
    );
    assert_eq!(
        subcode(
            w.api
                .get_agent_feed_trigger(req(
                    &w.token("reader"),
                    GetAgentFeedTriggerRequest {
                        repo: rref(&w.app),
                        agent: "idle".into(),
                    },
                ))
                .await
        ),
        "NOT_FOUND/"
    );
    // Replacing the sources unfollows the dropped one.
    let t = ok(w.set_trigger("maint", "scout", &[], 0.0, 0).await);
    assert!(t.sources.is_empty());
    assert!(!w.follows("scout", &w.upstream).await);
    assert!(
        w.event_kinds(&w.app)
            .await
            .contains(&"agent_feed_trigger.updated".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followed_post_starts_one_debounced_adopt_run_that_opens_an_inspired_issue() {
    let w = world().await;
    ok(w.set_trigger("maint", "scout", &[&w.upstream], 0.0, 0)
        .await);
    let post = w
        .publish(
            "poster",
            &w.upstream,
            "src/cache.rs",
            "Cache the build layers",
            "Builds are 3x faster",
        )
        .await;
    w.forge.flush().await.unwrap();
    // Inside the window: nothing yet.
    w.clock.advance(WINDOW_MS / 2);
    w.settle().await;
    assert!(w.adopt_runs().await.is_empty());
    // A second post inside the window folds into the same run.
    let second = w
        .publish(
            "poster",
            &w.upstream,
            "src/logs.rs",
            "Stream the logs",
            "Live logs",
        )
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    let runs = w.adopt_runs().await;
    assert_eq!(runs.len(), 1, "one run for both posts");
    let run = &runs[0];
    assert_eq!(run.task.as_ref().unwrap().post_id, post.id);
    assert_eq!(run.trigger, "feed");
    assert_eq!(run.state(), AgentRunState::Succeeded);
    assert!(
        run.summary.starts_with("opened issue #")
            && run.summary.ends_with(": Adopt: Cache the build layers"),
        "{}",
        run.summary
    );
    // The prompt carried both posts, their changes and the target.
    let job = w
        .runner
        .jobs()
        .into_iter()
        .find(|j| j.run.id == run.id)
        .unwrap();
    for want in [
        "Builds are 3x faster",
        "Change #",
        "Cache the build layers",
        "- src/cache.rs",
        "Stream the logs",
        "- src/logs.rs",
        &post.id,
        &second.id,
        &w.app.full_name,
        "inspired_by_post_id",
    ] {
        assert!(
            job.prompt.contains(want),
            "missing {want:?} in\n{}",
            job.prompt
        );
    }
    // The issue it opened names the post; the post counts the adoption.
    let issues = ok(w
        .svc
        .list_issues(req(
            &w.token("reader"),
            ListIssuesRequest {
                repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await)
    .issues;
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].author.as_ref().unwrap().id, "scout");
    assert_eq!(issues[0].inspired_by.as_ref().unwrap().post_id, post.id);
    assert_eq!(
        w.post_as("maint", &post.id).await.counts.unwrap().adoptions,
        1
    );
    // Delivering the event again starts nothing new.
    let (events, _) = w
        .forge
        .events
        .read(&Cx::system(), &w.upstream.id, 0, &[], 10_000)
        .await
        .unwrap();
    for e in events.iter().filter(|e| e.kind == "post.published") {
        w.agents.on_event(e).await.unwrap();
    }
    w.clock.advance(10 * WINDOW_MS);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 1);
    // A disabled trigger does not fire.
    ok(w.api
        .set_agent_feed_trigger(req(
            &w.token("maint"),
            SetAgentFeedTriggerRequest {
                repo: rref(&w.app),
                agent: "scout".into(),
                enabled: false,
                sources: vec![named(&w.upstream)],
                ..Default::default()
            },
        ))
        .await);
    w.publish("poster", &w.upstream, "src/z.rs", "Another", "more")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(2 * WINDOW_MS);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_relevant_runs_open_nothing_and_spent_days_skip_runs() {
    let w = world().await;
    ok(w.set_trigger("maint", "scout", &[&w.upstream], 0.0, 1)
        .await);
    w.runner.script(
        "scout",
        FakeScript {
            not_relevant: Some("we never rebase".into()),
            ..FakeScript::default()
        },
    );
    w.publish(
        "poster",
        &w.upstream,
        "src/a.rs",
        "Order the conflicts",
        "sorted",
    )
    .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    let runs = w.adopt_runs().await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].summary, "not relevant: we never rebase");
    let issues = ok(w
        .svc
        .list_issues(req(
            &w.token("maint"),
            ListIssuesRequest {
                repo: rref(&w.app),
                ..Default::default()
            },
        ))
        .await)
    .issues;
    assert!(issues.is_empty());
    let t = ok(w
        .api
        .get_agent_feed_trigger(req(
            &w.token("maint"),
            GetAgentFeedTriggerRequest {
                repo: rref(&w.app),
                agent: "scout".into(),
            },
        ))
        .await);
    assert_eq!(t.runs_today, 1);
    assert!(
        t.spent_usd_today > 0.0,
        "the run's cost is charged to the day"
    );
    // The cap of one run a day is spent: the next post fires as skipped.
    w.publish("poster", &w.upstream, "src/b.rs", "Second", "again")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 1);
    let (events, _) = w
        .forge
        .events
        .read(&Cx::system(), &w.app.id, 0, &[], 10_000)
        .await
        .unwrap();
    let fired: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == "agent_feed_trigger.fired")
        .collect();
    assert_eq!(fired.len(), 2);
    assert!(fired[1].payload_json.contains("daily run cap reached"));
    // A new UTC day resets the counters.
    w.clock.advance(86_400_000);
    w.publish("poster", &w.upstream, "src/c.rs", "Third", "next day")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 2);
    // A budget below one run's cost: the first run spends it, the next is skipped.
    ok(
        w.set_trigger("maint", "scout", &[&w.upstream], 0.000_001, 0)
            .await,
    );
    w.clock.advance(86_400_000);
    w.publish("poster", &w.upstream, "src/d.rs", "Fourth", "x")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 3);
    w.publish("poster", &w.upstream, "src/e.rs", "Fifth", "y")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    assert_eq!(w.adopt_runs().await.len(), 3);
    let (events, _) = w
        .forge
        .events
        .read(&Cx::system(), &w.app.id, 0, &[], 10_000)
        .await
        .unwrap();
    assert!(
        events.iter().any(|e| e.kind == "agent_feed_trigger.fired"
            && e.payload_json.contains("daily budget spent"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posts_the_agent_cannot_read_start_nothing() {
    let w = world().await;
    ok(w.set_trigger("maint", "scout", &[&w.upstream], 0.0, 0)
        .await);
    // The agent follows vault by itself (it cannot read it), and vault publishes.
    w.forge
        .community
        .users
        .follow_repo_as(&Cx::system(), "scout", &w.vault)
        .await
        .unwrap();
    w.publish("poster", &w.vault, "src/s.rs", "Secret", "hush")
        .await;
    w.forge.flush().await.unwrap();
    w.clock.advance(WINDOW_MS + 1);
    w.settle().await;
    assert!(w.adopt_runs().await.is_empty());
}
