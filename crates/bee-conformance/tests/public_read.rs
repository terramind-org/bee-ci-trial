//! Anonymous read of public repos (`forge.allow_anonymous_public_read`, SPEC §9.6,
//! `docs/spec/server.md` §2): a caller with no credentials reads a public repo's tree, files
//! and history, finds it in trending, search and leaderboards, and reads public profiles and
//! stargazers, over gRPC and git smart HTTP alike. It also reads every tab of a public repo
//! (changes and review, checks, leases, runs, issues, projects, releases, packages, Pages,
//! languages and the live event stream). A private repo answers `NOT_FOUND` (git asks for
//! credentials), and every write answers `UNAUTHENTICATED`. With the switch off, every call
//! needs a token again.

use crate::scale_support;

use bee_conformance::load::client::{Client, RpcError, change_id, pid};
use bee_proto::v1;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::check_service_client::CheckServiceClient;
use bee_proto::v1::event_service_client::EventServiceClient;
use bee_proto::v1::insights_service_client::InsightsServiceClient;
use bee_proto::v1::issue_service_client::IssueServiceClient;
use bee_proto::v1::lease_service_client::LeaseServiceClient;
use bee_proto::v1::object_service_client::ObjectServiceClient;
use bee_proto::v1::package_service_client::PackageServiceClient;
use bee_proto::v1::pages_service_client::PagesServiceClient;
use bee_proto::v1::project_service_client::ProjectServiceClient;
use bee_proto::v1::query_service_client::QueryServiceClient;
use bee_proto::v1::release_service_client::ReleaseServiceClient;
use bee_proto::v1::repo_service_client::RepoServiceClient;
use bee_proto::v1::search_service_client::SearchServiceClient;
use bee_proto::v1::user_service_client::UserServiceClient;
use bee_proto::v1::workflow_service_client::WorkflowServiceClient;
use bee_proto::v1::write_service_client::WriteServiceClient;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use scale_support::*;

const OPEN: &str = "dev/open";
const CLOSED: &str = "dev/closed";

fn rref(name: &str) -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName(name.into())),
    })
}

fn reason(e: tonic::Status) -> String {
    RpcError::from(e).reason()
}

/// A request without any `authorization` header, as a signed-out browser sends it.
fn bare<T>(m: T) -> tonic::Request<T> {
    tonic::Request::new(m)
}

/// `dev/open` (public) and `dev/closed` (private), each with a README and one landed change
/// by the dev user, and a star by the dev user on both.
async fn world(f: impl FnOnce(&mut bee_server::config::Config)) -> (Srv, tempfile::TempDir) {
    let (s, dir) = start_fresh(f, None).await;
    let repos = RepoServiceClient::new(s.client.channel().clone());
    for (name, vis) in [
        ("open", v1::Visibility::Public),
        ("closed", v1::Visibility::Private),
    ] {
        repos
            .clone()
            .create_repo(s.client.req(v1::CreateRepoRequest {
                org: "dev".into(),
                name: name.into(),
                visibility: vis as i32,
                description: format!("the {name} repo"),
                topics: vec!["anon-read".into()],
                auto_init: true,
                ..Default::default()
            }))
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
        let c = Client::new(
            s.client.channel().clone(),
            s.client.token(),
            format!("dev/{name}"),
        );
        let (tip, _) = main_tip(&c).await;
        let head = commit_now(
            &s,
            &c,
            &tip,
            format!("// {name}\n").into_bytes(),
            if name == "open" { 1 } else { 2 },
        )
        .await;
        c.land("main", &head).await.expect("land");
        UserServiceClient::new(s.client.channel().clone())
            .star_repo(s.client.req(v1::StarRepoRequest {
                repo: rref(&format!("dev/{name}")),
            }))
            .await
            .expect("star");
    }
    (s, dir)
}

/// Puts `src/lib.rs` on `parent` in one changeset authored now, so trending and leaderboard
/// windows count it (the load client stamps a fixed, older author time).
async fn commit_now(
    s: &Srv,
    c: &Client,
    parent: &bee_core::ObjectId,
    data: Vec<u8>,
    n: u64,
) -> bee_core::ObjectId {
    c.upload(std::slice::from_ref(&data)).await.expect("upload");
    let blob = bee_object::blob_id(&data);
    let now = s.srv.state.now_ms() / 1000;
    let cs = WriteServiceClient::new(c.channel().clone())
        .create_changeset(c.req(v1::CreateChangesetRequest {
            repo: c.repo(),
            parents: vec![pid(parent)],
            delta: vec![v1::DeltaEntry {
                path: b"src/lib.rs".to_vec(),
                op: Some(v1::delta_entry::Op::Put(v1::Put {
                    mode: 1,
                    blob: Some(pid(&blob)),
                    size: data.len() as u64,
                })),
            }],
            change_id: Some(v1::ChangeId {
                raw: change_id(n).as_bytes().to_vec(),
            }),
            message: "add lib".into(),
            author_time: Some(v1::Signature {
                name: "dev".into(),
                time: Some(pbjson_types::Timestamp {
                    seconds: now as i64,
                    nanos: 0,
                }),
                tz_offset_minutes: 0,
            }),
            ..Default::default()
        }))
        .await
        .expect("create changeset")
        .into_inner()
        .changeset
        .and_then(|c| c.id)
        .expect("changeset id");
    bee_core::ObjectId::from_bytes(&cs.raw).expect("id")
}

async fn log(
    s: &Srv,
    repo: &str,
    req: impl Fn(v1::LogRequest) -> tonic::Request<v1::LogRequest>,
) -> Result<usize, String> {
    QueryServiceClient::new(s.client.channel().clone())
        .log(req(v1::LogRequest {
            repo: rref(repo),
            from: Some(v1::Revision {
                expr: "main".into(),
            }),
            ..Default::default()
        }))
        .await
        .map(|r| r.into_inner().changesets.len())
        .map_err(reason)
}

async fn list_tree(s: &Srv, repo: &str) -> Result<Vec<String>, String> {
    QueryServiceClient::new(s.client.channel().clone())
        .list_tree(bare(v1::ListTreeRequest {
            repo: rref(repo),
            at: Some(v1::Revision {
                expr: "main".into(),
            }),
            recursive: true,
            ..Default::default()
        }))
        .await
        .map(|r| {
            r.into_inner()
                .entries
                .iter()
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect()
        })
        .map_err(reason)
}

async fn read_path(s: &Srv, repo: &str, path: &str) -> Result<Vec<u8>, String> {
    let mut stream = QueryServiceClient::new(s.client.channel().clone())
        .read_path(bare(v1::ReadPathRequest {
            repo: rref(repo),
            at: Some(v1::Revision {
                expr: "main".into(),
            }),
            path: path.as_bytes().to_vec(),
        }))
        .await
        .map_err(reason)?
        .into_inner();
    let mut out = Vec::new();
    while let Some(chunk) = stream.message().await.map_err(reason)? {
        out.extend_from_slice(&chunk.data);
    }
    Ok(out)
}

async fn resolve(s: &Srv, repo: &str) -> Result<(), String> {
    ObjectServiceClient::new(s.client.channel().clone())
        .resolve_ref(bare(v1::ResolveRefRequest {
            repo: rref(repo),
            name: "main".into(),
            ..Default::default()
        }))
        .await
        .map(|_| ())
        .map_err(reason)
}

async fn trending(s: &Srv) -> Result<Vec<String>, String> {
    SearchServiceClient::new(s.client.channel().clone())
        .list_trending_repos(bare(v1::ListTrendingReposRequest {
            window: v1::TrendingWindow::Weekly as i32,
            ..Default::default()
        }))
        .await
        .map(|r| {
            r.into_inner()
                .repos
                .into_iter()
                .filter_map(|t| t.repo.map(|r| r.full_name))
                .collect()
        })
        .map_err(reason)
}

async fn leaderboard(
    s: &Srv,
    subject: v1::LeaderboardSubject,
    metric: v1::LeaderboardMetric,
) -> Result<Vec<v1::LeaderboardEntry>, String> {
    SearchServiceClient::new(s.client.channel().clone())
        .list_leaderboard(bare(v1::ListLeaderboardRequest {
            window: v1::TrendingWindow::Weekly as i32,
            subject: subject as i32,
            metric: metric as i32,
            ..Default::default()
        }))
        .await
        .map(|r| r.into_inner().entries)
        .map_err(reason)
}

/// The status of `GET /<repo>.git/info/refs?service=git-upload-pack` without credentials.
async fn git_refs(s: &Srv, repo: &str) -> u16 {
    let http: HttpClient<_, Empty<Bytes>> = HttpClient::builder(TokioExecutor::new()).build_http();
    let uri = format!("{}/{repo}.git/info/refs?service=git-upload-pack", s.srv.url);
    let resp = http
        .request(
            http::Request::get(uri)
                .header("user-agent", "git/2.45.0")
                .body(Empty::new())
                .expect("request"),
        )
        .await
        .expect("info/refs");
    let status = resp.status().as_u16();
    let _ = resp.into_body().collect().await;
    status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anonymous_callers_read_public_repos_and_nothing_else() {
    let (s, _dir) = world(|_| {}).await;

    // Tree, file and history of the public repo, without any header.
    let tree = list_tree(&s, OPEN).await.expect("anonymous ListTree");
    assert!(tree.iter().any(|p| p == "src/lib.rs"), "{tree:?}");
    assert_eq!(
        read_path(&s, OPEN, "src/lib.rs").await.unwrap(),
        b"// open\n"
    );
    assert!(log(&s, OPEN, bare).await.expect("anonymous Log") >= 2);
    resolve(&s, OPEN).await.expect("anonymous ResolveRef");
    // A blank bearer (`Authorization: Bearer `) is no credential either.
    let blank = s.client.with_token("");
    assert!(
        log(&s, OPEN, |m| blank.req(m))
            .await
            .expect("blank bearer Log")
            >= 2
    );
    // The forge's repo record of a public repo.
    let repo = RepoServiceClient::new(s.client.channel().clone())
        .get_repo(bare(v1::GetRepoRequest { repo: rref(OPEN) }))
        .await
        .expect("anonymous GetRepo")
        .into_inner();
    assert_eq!(repo.full_name, OPEN);

    // The private repo does not exist for an anonymous caller.
    assert_eq!(list_tree(&s, CLOSED).await.unwrap_err(), "NOT_FOUND");
    assert_eq!(
        read_path(&s, CLOSED, "src/lib.rs").await.unwrap_err(),
        "NOT_FOUND"
    );
    assert_eq!(log(&s, CLOSED, bare).await.unwrap_err(), "NOT_FOUND");
    assert_eq!(resolve(&s, CLOSED).await.unwrap_err(), "NOT_FOUND");
    let e = RepoServiceClient::new(s.client.channel().clone())
        .get_repo(bare(v1::GetRepoRequest { repo: rref(CLOSED) }))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "NOT_FOUND");
    // A repo that does not exist answers the same.
    assert_eq!(
        log(&s, "dev/nothing-here", bare).await.unwrap_err(),
        "NOT_FOUND"
    );

    // Discovery: trending, search and topics see the public repo only.
    let names = trending(&s).await.expect("anonymous ListTrendingRepos");
    assert!(names.iter().any(|n| n == OPEN), "{names:?}");
    assert!(!names.iter().any(|n| n == CLOSED), "{names:?}");
    let hits = SearchServiceClient::new(s.client.channel().clone())
        .search_repos(bare(v1::SearchReposRequest {
            query: "topic:anon-read".into(),
            ..Default::default()
        }))
        .await
        .expect("anonymous SearchRepos")
        .into_inner()
        .hits;
    // The hit carries the repo's real star count (the world starred it once), so sorting by
    // stars orders by stars rather than by name.
    let hits: Vec<(String, u64)> = hits
        .into_iter()
        .map(|h| (h.full_name, h.star_count))
        .collect();
    assert_eq!(hits, vec![(OPEN.to_string(), 1)]);
    let topic = SearchServiceClient::new(s.client.channel().clone())
        .get_topic(bare(v1::GetTopicRequest {
            name: "anon-read".into(),
            top_repo_count: 10,
        }))
        .await
        .expect("anonymous GetTopic")
        .into_inner();
    assert_eq!(topic.repo_count, 1, "{topic:?}");

    // Leaderboards: repos by lands and stars, the dev user by lands.
    let by_lands = leaderboard(
        &s,
        v1::LeaderboardSubject::Repos,
        v1::LeaderboardMetric::Lands,
    )
    .await
    .expect("anonymous ListLeaderboard");
    let names: Vec<String> = by_lands
        .iter()
        .filter_map(|e| e.repo.as_ref().map(|r| r.full_name.clone()))
        .collect();
    assert!(names.contains(&OPEN.to_string()), "{names:?}");
    assert!(!names.contains(&CLOSED.to_string()), "{names:?}");
    assert_eq!(by_lands[0].rank, 1);
    let by_stars = leaderboard(
        &s,
        v1::LeaderboardSubject::Repos,
        v1::LeaderboardMetric::Stars,
    )
    .await
    .expect("stars leaderboard");
    let starred: Vec<(String, u64)> = by_stars
        .iter()
        .filter_map(|e| e.repo.as_ref().map(|r| (r.full_name.clone(), e.score)))
        .collect();
    assert_eq!(starred, vec![(OPEN.to_string(), 1)]);
    let people = leaderboard(
        &s,
        v1::LeaderboardSubject::Humans,
        v1::LeaderboardMetric::Lands,
    )
    .await
    .expect("humans leaderboard");
    let dev = people
        .iter()
        .find(|e| {
            e.user
                .as_ref()
                .and_then(|u| u.principal.as_ref())
                .is_some_and(|p| p.id == s.user())
        })
        .unwrap_or_else(|| panic!("the dev user {} ranks: {people:?}", s.user()));
    // Only the public repo's change counts for an anonymous caller (the private one does not,
    // and the server's auto-init README ranks nobody).
    assert_eq!(dev.score, 1, "{people:?}");
    assert!(
        people.iter().all(|e| e
            .user
            .as_ref()
            .and_then(|u| u.principal.as_ref())
            .is_some_and(|p| p.id != "server")),
        "{people:?}"
    );
    let e = leaderboard(
        &s,
        v1::LeaderboardSubject::Repos,
        v1::LeaderboardMetric::Reviews,
    )
    .await
    .unwrap_err();
    assert_eq!(e, "INVALID_ARGUMENT");

    // Profiles and stargazers are public reads.
    let users = UserServiceClient::new(s.client.channel().clone());
    let me = users
        .clone()
        .get_user(bare(v1::GetUserRequest {
            user: Some(v1::get_user_request::User::Id(s.user())),
        }))
        .await
        .expect("anonymous GetUser")
        .into_inner();
    assert!(!me.login.is_empty());
    users
        .clone()
        .list_user_followers(bare(v1::ListUserFollowersRequest {
            login: me.login.clone(),
            ..Default::default()
        }))
        .await
        .expect("anonymous ListUserFollowers");
    let starred = users
        .clone()
        .list_starred_repos(bare(v1::ListStarredReposRequest {
            login: me.login.clone(),
            ..Default::default()
        }))
        .await
        .expect("anonymous ListStarredRepos")
        .into_inner();
    let starred: Vec<String> = starred.repos.into_iter().map(|r| r.full_name).collect();
    assert_eq!(starred, vec![OPEN.to_string()], "private stars stay hidden");
    let gazers = users
        .clone()
        .list_repo_stargazers(bare(v1::ListRepoStargazersRequest {
            repo: rref(OPEN),
            ..Default::default()
        }))
        .await
        .expect("anonymous ListRepoStargazers")
        .into_inner();
    assert_eq!(gazers.total_count, 1);
    let e = users
        .clone()
        .list_repo_stargazers(bare(v1::ListRepoStargazersRequest {
            repo: rref(CLOSED),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "NOT_FOUND");
    // "Me" needs an account.
    let e = users
        .clone()
        .list_user_followers(bare(v1::ListUserFollowersRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "UNAUTHENTICATED");
    let e = users
        .clone()
        .get_viewer(bare(v1::GetViewerRequest {}))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "UNAUTHENTICATED");

    // Every write without credentials is refused before it reaches the repo.
    let anon = Client::new(s.client.channel().clone(), "", OPEN);
    let (tip, _) = main_tip(&s.client.with_token(s.client.token())).await;
    let e = anon.land("main", &tip).await.unwrap_err();
    assert_eq!(e.reason(), "UNAUTHENTICATED");
    let e = anon
        .upload(&[b"anonymous bytes".to_vec()])
        .await
        .unwrap_err();
    assert_eq!(e.reason(), "UNAUTHENTICATED");
    let e = users
        .clone()
        .star_repo(bare(v1::StarRepoRequest { repo: rref(OPEN) }))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "UNAUTHENTICATED");
    let e = users
        .clone()
        .follow_user(bare(v1::FollowUserRequest {
            login: me.login.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "UNAUTHENTICATED");
    let e = SearchServiceClient::new(s.client.channel().clone())
        .create_saved_search(bare(v1::CreateSavedSearchRequest {
            name: "mine".into(),
            query: "x".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(reason(e), "UNAUTHENTICATED");

    // git clone and fetch: the public repo serves, the private one asks for credentials.
    assert_eq!(git_refs(&s, OPEN).await, 200);
    assert_eq!(git_refs(&s, CLOSED).await, 401);
    assert_eq!(git_refs(&s, "dev/nothing-here").await, 401);

    // The anonymous tier answered.
    let metrics = s.srv.state.metrics.render();
    assert!(
        metrics.contains("tier=\"anonymous\""),
        "anonymous requests are admitted on their own tier"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anonymous_read_off_needs_a_token_everywhere() {
    let (s, _dir) = world(|c| c.forge.allow_anonymous_public_read = false).await;
    assert_eq!(list_tree(&s, OPEN).await.unwrap_err(), "UNAUTHENTICATED");
    assert_eq!(log(&s, OPEN, bare).await.unwrap_err(), "UNAUTHENTICATED");
    assert_eq!(trending(&s).await.unwrap_err(), "UNAUTHENTICATED");
    assert_eq!(
        leaderboard(
            &s,
            v1::LeaderboardSubject::Repos,
            v1::LeaderboardMetric::Lands
        )
        .await
        .unwrap_err(),
        "UNAUTHENTICATED"
    );
    assert_eq!(git_refs(&s, OPEN).await, 401);
    // Signed-in callers are unaffected.
    assert!(
        log(&s, OPEN, |m| s.client.req(m))
            .await
            .expect("signed-in Log")
            >= 2
    );
    let names = SearchServiceClient::new(s.client.channel().clone())
        .list_trending_repos(s.client.req(v1::ListTrendingReposRequest {
            window: v1::TrendingWindow::Weekly as i32,
            ..Default::default()
        }))
        .await
        .expect("signed-in trending")
        .into_inner()
        .repos;
    assert!(names.len() >= 2, "a member sees both repos: {names:?}");
}

/// The reason of a call that must fail.
fn refused<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>, what: &str) -> String {
    match r {
        Ok(v) => panic!("{what}: expected a refusal, got {v:?}"),
        Err(e) => reason(e),
    }
}

/// The value of a call that must succeed.
fn served<T>(r: Result<tonic::Response<T>, tonic::Status>, what: &str) -> T {
    r.unwrap_or_else(|e| panic!("{what}: {e:?}")).into_inner()
}

/// The first message of a server stream, or the reason it was refused (at the call or on the
/// first message).
async fn first<T>(
    r: Result<tonic::Response<tonic::Streaming<T>>, tonic::Status>,
) -> Result<Option<T>, String> {
    let mut s = r.map_err(reason)?.into_inner();
    match tokio::time::timeout(std::time::Duration::from_secs(10), s.message()).await {
        Ok(m) => m.map_err(reason),
        Err(_) => Ok(None),
    }
}

fn ch(id: &bee_core::ObjectId) -> Option<v1::ObjectId> {
    Some(pid(id))
}

/// One repo's objects behind its tabs: an open change and two issues (`#1` is the parent).
struct Tabs {
    change: String,
    head: bee_core::ObjectId,
    issue: u64,
    issue_id: String,
}

/// Opens a change and two issues in `dev/<name>` as the dev user, takes a lease and turns on
/// Pages.
async fn tabs(s: &Srv, name: &str, n: u64) -> Tabs {
    let repo = format!("dev/{name}");
    let c = Client::new(s.client.channel().clone(), s.client.token(), repo.clone());
    let (tip, _) = main_tip(&c).await;
    let head = commit_now(s, &c, &tip, format!("// {name} change\n").into_bytes(), n).await;
    let change = ChangeServiceClient::new(s.client.channel().clone())
        .open_change(s.client.req(v1::OpenChangeRequest {
            repo: rref(&repo),
            target_ref: "main".into(),
            head: ch(&head),
            title: format!("{name} change"),
            ..Default::default()
        }))
        .await
        .unwrap_or_else(|e| panic!("open change in {repo}: {e:?}"))
        .into_inner()
        .id;
    let issues = IssueServiceClient::new(s.client.channel().clone());
    let mut made = Vec::new();
    for title in [format!("{name} parent"), format!("{name}-only secret plan")] {
        made.push(
            issues
                .clone()
                .create_issue(s.client.req(v1::CreateIssueRequest {
                    repo: rref(&repo),
                    title,
                    body: "notes".into(),
                    ..Default::default()
                }))
                .await
                .unwrap_or_else(|e| panic!("create issue in {repo}: {e:?}"))
                .into_inner(),
        );
    }
    LeaseServiceClient::new(s.client.channel().clone())
        .acquire_lease(s.client.req(v1::AcquireLeaseRequest {
            repo: rref(&repo),
            r#ref: "main".into(),
            paths: vec![b"src".to_vec()],
            mode: v1::LeaseMode::Advisory as i32,
            note: format!("{name} work"),
            ..Default::default()
        }))
        .await
        .unwrap_or_else(|e| panic!("lease in {repo}: {e:?}"));
    PagesServiceClient::new(s.client.channel().clone())
        .create_pages_site(s.client.req(v1::CreatePagesSiteRequest {
            repo: rref(&repo),
            build_type: v1::PagesBuildType::Ref as i32,
            source: Some(v1::PagesSource {
                r#ref: "main".into(),
                path: "/".into(),
            }),
            ..Default::default()
        }))
        .await
        .unwrap_or_else(|e| panic!("pages site of {repo}: {e:?}"));
    Tabs {
        change,
        head,
        issue: made[0].number,
        issue_id: made[1].id.clone(),
    }
}

/// Every read a public repo's tabs make, as a caller with no credentials: `dev/open` answers,
/// `dev/closed` is `NOT_FOUND` for all of them, and the writes beside them stay refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anonymous_callers_read_every_tab_of_a_public_repo() {
    let (s, _dir) = world(|_| {}).await;
    let open = tabs(&s, "open", 3).await;
    let closed = tabs(&s, "closed", 4).await;
    let chan = s.client.channel().clone();

    // A private issue made a sub-issue of a public one, and an org project holding both repos'
    // issues: what a visitor sees of them must say nothing about the private repo.
    IssueServiceClient::new(chan.clone())
        .add_issue_sub_issue(s.client.req(v1::AddIssueSubIssueRequest {
            repo: rref(OPEN),
            number: open.issue,
            sub_issue: Some(v1::IssueRef {
                repo: rref(CLOSED),
                id: closed.issue_id.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .await
        .expect("add a private sub-issue");
    let projects = ProjectServiceClient::new(chan.clone());
    let project = projects
        .clone()
        .create_project(s.client.req(v1::CreateProjectRequest {
            owner: "dev".into(),
            title: "Public roadmap".into(),
            public: true,
            link_repos: vec![rref(OPEN).unwrap_or_default()],
            ..Default::default()
        }))
        .await
        .expect("create project")
        .into_inner();
    for (repo, id) in [
        (OPEN, open_issue_id(&s, OPEN, open.issue).await),
        (CLOSED, closed.issue_id.clone()),
    ] {
        projects
            .clone()
            .add_project_item(s.client.req(v1::AddProjectItemRequest {
                project_id: project.id.clone(),
                content: Some(v1::SubjectRef {
                    kind: v1::SubjectKind::Issue as i32,
                    repo: rref(repo),
                    id,
                }),
            }))
            .await
            .unwrap_or_else(|e| panic!("add {repo} item: {e:?}"));
    }

    let (changes, checks, leases, runs, issues) = (
        ChangeServiceClient::new(chan.clone()),
        CheckServiceClient::new(chan.clone()),
        LeaseServiceClient::new(chan.clone()),
        WorkflowServiceClient::new(chan.clone()),
        IssueServiceClient::new(chan.clone()),
    );
    let (repos, releases, packages, pages, insights, events) = (
        RepoServiceClient::new(chan.clone()),
        ReleaseServiceClient::new(chan.clone()),
        PackageServiceClient::new(chan.clone()),
        PagesServiceClient::new(chan.clone()),
        InsightsServiceClient::new(chan.clone()),
        EventServiceClient::new(chan.clone()),
    );

    for (name, t, public) in [(OPEN, &open, true), (CLOSED, &closed, false)] {
        // Each call is made the same way on both repos; the public one answers, the private one
        // is not found. `check!` takes the call and, for the public repo, what to assert.
        macro_rules! check {
            ($what:expr, $call:expr) => {{
                let r = $call;
                if public {
                    served(r, $what);
                } else {
                    assert_eq!(refused(r, $what), "NOT_FOUND", "{} on {name}", $what);
                }
            }};
            ($what:expr, $call:expr, |$v:ident| $ok:expr) => {{
                let r = $call;
                if public {
                    let $v = served(r, $what);
                    $ok;
                } else {
                    assert_eq!(refused(r, $what), "NOT_FOUND", "{} on {name}", $what);
                }
            }};
        }
        let repo = rref(name);
        check!(
            "GetContext",
            repos
                .clone()
                .get_context(bare(v1::GetContextRequest {
                    repo: repo.clone(),
                    at: Some(v1::Revision {
                        expr: "main".into()
                    }),
                    paths: vec![b"src/lib.rs".to_vec()],
                }))
                .await,
            |ctx| assert_eq!(ctx.active_leases.len(), 1, "{ctx:?}")
        );
        check!(
            "ListChanges",
            changes
                .clone()
                .list_changes(bare(v1::ListChangesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await,
            |l| assert!(l.changes.iter().any(|c| c.id == t.change))
        );
        check!(
            "GetChange",
            changes
                .clone()
                .get_change(bare(v1::GetChangeRequest {
                    repo: repo.clone(),
                    id: t.change.clone(),
                }))
                .await,
            |c| assert_eq!(c.id, t.change)
        );
        check!(
            "GetMergeQueue",
            changes
                .clone()
                .get_merge_queue(bare(v1::GetMergeQueueRequest {
                    repo: repo.clone(),
                    r#ref: "main".into(),
                }))
                .await
        );
        check!(
            "EvaluateLanding",
            changes
                .clone()
                .evaluate_landing(bare(v1::EvaluateLandingRequest {
                    repo: repo.clone(),
                    change_request_id: t.change.clone(),
                }))
                .await
        );
        check!(
            "ListReviews",
            changes
                .clone()
                .list_reviews(bare(v1::ListReviewsRequest {
                    repo: repo.clone(),
                    change_request_id: t.change.clone(),
                }))
                .await
        );
        check!(
            "ListReviewThreads",
            changes
                .clone()
                .list_review_threads(bare(v1::ListReviewThreadsRequest {
                    repo: repo.clone(),
                    change_request_id: t.change.clone(),
                    ..Default::default()
                }))
                .await
        );
        let diff = first(
            changes
                .clone()
                .diff_change_revisions(bare(v1::DiffChangeRevisionsRequest {
                    repo: repo.clone(),
                    change_request_id: t.change.clone(),
                    from_revision: 1,
                    to_revision: 1,
                    ..Default::default()
                }))
                .await,
        )
        .await;
        if public {
            diff.expect("DiffChangeRevisions");
        } else {
            assert_eq!(
                diff.unwrap_err(),
                "NOT_FOUND",
                "DiffChangeRevisions on {name}"
            );
        }
        check!(
            "ListChecks",
            checks
                .clone()
                .list_checks(bare(v1::ListChecksRequest {
                    repo: repo.clone(),
                    changeset: ch(&t.head),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "ListLeases",
            leases
                .clone()
                .list_leases(bare(v1::ListLeasesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await,
            |l| assert_eq!(l.leases.len(), 1)
        );
        check!(
            "ListWorkflowRuns",
            runs.clone()
                .list_workflow_runs(bare(v1::ListWorkflowRunsRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "ListIssues",
            issues
                .clone()
                .list_issues(bare(v1::ListIssuesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await,
            |l| assert_eq!(l.issues.len(), 2)
        );
        check!(
            "GetIssueByNumber",
            issues
                .clone()
                .get_issue_by_number(bare(v1::GetIssueByNumberRequest {
                    repo: repo.clone(),
                    number: t.issue,
                    ..Default::default()
                }))
                .await,
            |i| assert_eq!(i.number, t.issue)
        );
        check!(
            "ListIssueComments",
            issues
                .clone()
                .list_issue_comments(bare(v1::ListIssueCommentsRequest {
                    repo: repo.clone(),
                    number: t.issue,
                    ..Default::default()
                }))
                .await
        );
        check!(
            "ListIssueSubIssues",
            issues
                .clone()
                .list_issue_sub_issues(bare(v1::ListIssueSubIssuesRequest {
                    repo: repo.clone(),
                    number: t.issue,
                    ..Default::default()
                }))
                .await,
            // The private sub-issue is left out.
            |l| assert!(l.sub_issues.is_empty(), "{:?}", l.sub_issues)
        );
        check!(
            "ListProjects",
            projects
                .clone()
                .list_projects(bare(v1::ListProjectsRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "ListReleases",
            releases
                .clone()
                .list_releases(bare(v1::ListReleasesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "ListPackages",
            packages
                .clone()
                .list_packages(bare(v1::ListPackagesRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "GetPagesSite",
            pages
                .clone()
                .get_pages_site(bare(v1::GetPagesSiteRequest { repo: repo.clone() }))
                .await
        );
        check!(
            "ListPagesDeployments",
            pages
                .clone()
                .list_pages_deployments(bare(v1::ListPagesDeploymentsRequest {
                    repo: repo.clone(),
                    ..Default::default()
                }))
                .await
        );
        check!(
            "GetLanguageStats",
            insights
                .clone()
                .get_language_stats(bare(v1::GetLanguageStatsRequest {
                    repo: repo.clone(),
                    at: Some(v1::Revision {
                        expr: "main".into()
                    }),
                    ..Default::default()
                }))
                .await
        );
        // Run, job and check-run reads by id: the gate lets the caller through to the repo's
        // decision. Without such an object the public repo answers that it does not exist,
        // which is not a refusal of the caller; the private repo is not found.
        let missing = "01JANONYMOUSREADMISSINGID0";
        for (what, r) in [
            (
                "GetWorkflowRun",
                runs.clone()
                    .get_workflow_run(bare(v1::GetWorkflowRunRequest {
                        repo: repo.clone(),
                        run_id: missing.into(),
                    }))
                    .await
                    .map(|_| ()),
            ),
            (
                "ListWorkflowJobs",
                runs.clone()
                    .list_workflow_jobs(bare(v1::ListWorkflowJobsRequest {
                        repo: repo.clone(),
                        run_id: missing.into(),
                        ..Default::default()
                    }))
                    .await
                    .map(|_| ()),
            ),
            (
                "ListCheckAnnotations",
                checks
                    .clone()
                    .list_check_annotations(bare(v1::ListCheckAnnotationsRequest {
                        repo: repo.clone(),
                        check_run_id: missing.into(),
                        ..Default::default()
                    }))
                    .await
                    .map(|_| ()),
            ),
        ] {
            if let Err(e) = r {
                let got = reason(e);
                assert!(
                    !["UNAUTHENTICATED", "PERMISSION_DENIED"].contains(&got.as_str()),
                    "{what} on {name}: {got}"
                );
                if !public {
                    assert_eq!(got, "NOT_FOUND", "{what} on {name}");
                }
            } else {
                assert!(public, "{what} answered on the private repo");
            }
        }
        let tail = first(
            runs.clone()
                .tail_workflow_job_logs(bare(v1::TailWorkflowJobLogsRequest {
                    repo: repo.clone(),
                    job_id: missing.into(),
                    ..Default::default()
                }))
                .await,
        )
        .await;
        if let Err(got) = &tail {
            assert!(
                !["UNAUTHENTICATED", "PERMISSION_DENIED"].contains(&got.as_str()),
                "TailWorkflowJobLogs on {name}: {got}"
            );
        }
        if !public {
            assert_eq!(
                tail.unwrap_err(),
                "NOT_FOUND",
                "TailWorkflowJobLogs on {name}"
            );
        }
        // The event stream replays the repo's log for the public repo only.
        let stream = first(
            events
                .clone()
                .subscribe(bare(v1::SubscribeRequest {
                    repo: repo.clone(),
                    from_seq: 0,
                    ..Default::default()
                }))
                .await,
        )
        .await;
        if public {
            assert!(
                stream.expect("Subscribe").is_some(),
                "the public log replays"
            );
        } else {
            assert_eq!(stream.unwrap_err(), "NOT_FOUND", "Subscribe on {name}");
        }
    }

    // An org's event stream is for its members.
    let e = first(
        events
            .clone()
            .subscribe(bare(v1::SubscribeRequest {
                org: "dev".into(),
                ..Default::default()
            }))
            .await,
    )
    .await
    .unwrap_err();
    assert!(
        ["PERMISSION_DENIED", "NOT_FOUND"].contains(&e.as_str()),
        "{e}"
    );

    // Package versions are read by name: a missing package is not found, not refused.
    let e = refused(
        packages
            .clone()
            .list_package_versions(bare(v1::ListPackageVersionsRequest {
                owner: "dev".into(),
                ecosystem: v1::PackageEcosystem::Npm as i32,
                name: "nothing-here".into(),
                ..Default::default()
            }))
            .await,
        "ListPackageVersions",
    );
    assert_eq!(e, "NOT_FOUND");

    // The public project: its fields read, its private-repo item is a placeholder in a plain
    // listing, and a filter on the private issue's title matches nothing.
    let fields = served(
        projects
            .clone()
            .list_project_fields(bare(v1::ListProjectFieldsRequest {
                project_id: project.id.clone(),
                ..Default::default()
            }))
            .await,
        "ListProjectFields",
    );
    assert!(!fields.fields.is_empty());
    let items = served(
        projects
            .clone()
            .list_project_items(bare(v1::ListProjectItemsRequest {
                project_id: project.id.clone(),
                ..Default::default()
            }))
            .await,
        "ListProjectItems",
    );
    let kinds: Vec<i32> = items.items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds.len(), 2, "{items:?}");
    assert!(
        kinds.contains(&(v1::ProjectItemKind::Redacted as i32)),
        "{items:?}"
    );
    for query in ["closed-only secret", "title:closed-only"] {
        let found = served(
            projects
                .clone()
                .list_project_items(bare(v1::ListProjectItemsRequest {
                    project_id: project.id.clone(),
                    query: query.into(),
                    ..Default::default()
                }))
                .await,
            "ListProjectItems with a query",
        );
        assert_eq!(found.total_count, 0, "{query}: {found:?}");
        assert!(found.items.is_empty(), "{query}: {found:?}");
    }
    // A member still finds it.
    let found = served(
        projects
            .clone()
            .list_project_items(s.client.req(v1::ListProjectItemsRequest {
                project_id: project.id.clone(),
                query: "closed-only secret".into(),
                ..Default::default()
            }))
            .await,
        "member ListProjectItems",
    );
    assert_eq!(found.total_count, 1, "{found:?}");
    // And the member sees the private sub-issue under the public parent.
    let subs = served(
        issues
            .clone()
            .list_issue_sub_issues(s.client.req(v1::ListIssueSubIssuesRequest {
                repo: rref(OPEN),
                number: open.issue,
                ..Default::default()
            }))
            .await,
        "member ListIssueSubIssues",
    );
    assert_eq!(subs.sub_issues.len(), 1, "{subs:?}");

    // The writes beside these reads stay closed to a caller without credentials.
    let e = refused(
        changes
            .clone()
            .open_change(bare(v1::OpenChangeRequest {
                repo: rref(OPEN),
                target_ref: "main".into(),
                head: ch(&open.head),
                title: "anonymous".into(),
                ..Default::default()
            }))
            .await,
        "OpenChange",
    );
    assert_eq!(e, "UNAUTHENTICATED");
    let e = refused(
        issues
            .clone()
            .create_issue(bare(v1::CreateIssueRequest {
                repo: rref(OPEN),
                title: "anonymous".into(),
                ..Default::default()
            }))
            .await,
        "CreateIssue",
    );
    assert_eq!(e, "UNAUTHENTICATED");
    let e = refused(
        leases
            .clone()
            .acquire_lease(bare(v1::AcquireLeaseRequest {
                repo: rref(OPEN),
                r#ref: "main".into(),
                paths: vec![b"src".to_vec()],
                ..Default::default()
            }))
            .await,
        "AcquireLease",
    );
    assert_eq!(e, "UNAUTHENTICATED");
    // Still closed reads: org members, agents and principals (which carry external ids).
    let e = refused(
        bee_proto::v1::auth_service_client::AuthServiceClient::new(chan.clone())
            .get_principal(bare(v1::GetPrincipalRequest {
                principal_id: s.user(),
            }))
            .await,
        "GetPrincipal",
    );
    assert_eq!(e, "UNAUTHENTICATED");
}

/// The id of issue `number` in `repo`, read as the dev user.
async fn open_issue_id(s: &Srv, repo: &str, number: u64) -> String {
    IssueServiceClient::new(s.client.channel().clone())
        .get_issue_by_number(s.client.req(v1::GetIssueByNumberRequest {
            repo: rref(repo),
            number,
            ..Default::default()
        }))
        .await
        .expect("issue")
        .into_inner()
        .id
}
