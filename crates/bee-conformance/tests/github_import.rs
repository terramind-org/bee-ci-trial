//! AS-MIG-01 end to end: a GitHub repository (a local git repository plus the fake GitHub
//! API of `bee-github`'s tests) imported into an in-process `bee-server --dev` on SQLite.
//! The server imports the history through its `ImportService` (exact commit ids); the
//! metadata goes into the Beehive forge over the server's own stores, with PR and issue
//! numbers kept.

#[allow(unused_imports)]
#[path = "../../bee-github/tests/common/mod.rs"]
mod common;
use std::sync::Arc;

use bee_forge::Forge;
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams};
use bee_forge::cx::Cx;
use bee_forge::forge::{ForgeConfig, Hooks};
use bee_forge::ids::{Clock as ForgeClock, SystemClock};
use bee_forge::store::memory::MemoryStore;
use bee_forge::work::RepoInfo;
use bee_git::GitOid;
use bee_github::Importer;
use bee_github::hub::HubSink;
use bee_github::server::{ServerTarget, UnwiredSink, rpc};
use bee_github::state::StateDir;
use bee_proto::v1::{ChangeState, Repo};
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_mig_01_github_import_into_bee_server_keeps_ids_and_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = ServerOptions::new(Config::dev(
        dir.path().to_path_buf(),
        Some("127.0.0.1:0".into()),
    ));
    opts.dev = true;
    let srv = start(opts).await.expect("server starts");
    let token = srv.dev.as_ref().unwrap().token.clone();
    let w = World::new(false).await;
    let clock: Arc<dyn bee_github::clock::Clock> = Arc::new(bee_github::clock::SystemClock);
    let git = Arc::new(ServerTarget::new(
        rpc(&srv.url, Some(token.clone())),
        "dev",
        NAME,
        clock,
    ));
    let mut o = w.options();
    o.hub_org = Some("dev".into());
    // Git first: the server imports the mirror; the repository exists and is usable.
    let mut mirror = o.clone();
    mirror.mirror_only = true;
    let client = || w.client_with(w.config(), bee_github::auth::Credential::Token("t".into()));
    let r = Importer::new(
        client(),
        StateDir::open(w.state.path()).unwrap(),
        git.clone(),
        Arc::new(UnwiredSink),
        mirror,
    )
    .run()
    .await
    .unwrap();
    assert!(
        r.report
            .git
            .categories
            .iter()
            .any(|c| c.category == "commits" && c.imported > 0),
        "{}",
        r.report.human()
    );
    let s = &srv.state;
    let org = s.meta.org_by_name("dev").await.unwrap().unwrap();
    let repo_row = s.meta.repo_by_name(org.id, NAME).await.unwrap().unwrap();
    // Every commit GitHub serves keeps its id in the server's stores.
    for sha in w.src.all_commits() {
        let oid = GitOid::parse(&sha).unwrap();
        let cs = bee_import::lookup_bee_id(s.meta.as_ref(), repo_row.id, &oid)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{sha} not imported"));
        let back = bee_import::lookup_git_id(s.meta.as_ref(), repo_row.id, &cs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.to_hex(), sha);
    }
    // Metadata into the forge over the same stores.
    let fclock: Arc<dyn ForgeClock> = Arc::new(SystemClock);
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, _) = Forge::open(
        Arc::new(MemoryStore::new()),
        fclock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Hooks::default(),
        ForgeConfig::default(),
    )
    .await
    .unwrap();
    let vcs = Arc::new(HistoryVcs::new(s.writer.clone(), fclock));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs,
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    let repo = Repo {
        id: repo_row.id.0.to_text(),
        org_id: org.id.0.to_text(),
        name: NAME.into(),
        full_name: format!("dev/{NAME}"),
        default_ref: "main".into(),
        ..Default::default()
    };
    let sink = HubSink::new(
        forge.clone(),
        changes.clone(),
        repo.clone(),
        s.writer.clone(),
    )
    .unwrap()
    .with_assets(None);
    let r = Importer::new(
        client(),
        StateDir::open(w.state.path()).unwrap(),
        git,
        Arc::new(sink),
        o,
    )
    .run()
    .await
    .unwrap();
    let cx = Cx::system();
    for n in [2u64, 3, 5] {
        let cr = changes.get_by_number(&cx, &repo, n).await.unwrap();
        assert_eq!(cr.number, n);
        assert_eq!(cr.state == ChangeState::Open as i32, n == 2);
    }
    let info = RepoInfo::from(&repo);
    for n in [1u64, 4] {
        assert_eq!(
            forge
                .work
                .get_issue_by_number(&cx, &info, n)
                .await
                .unwrap()
                .number,
            n
        );
    }
    assert_eq!(r.report.github.unexplained_drops, 0);
    let prs = r.report.category("pull_requests").unwrap();
    assert_eq!(
        (prs.imported + prs.archived, prs.dropped),
        (3, 0),
        "{}",
        r.report.human()
    );
    validate_report(&r.report.to_value());
    srv.shutdown().await;
}
