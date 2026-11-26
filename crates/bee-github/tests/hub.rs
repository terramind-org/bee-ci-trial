//! The Beehive forge as the import target ([`HubSink`]): PR and issue numbers survive in
//! the shared number space, revisions follow the force-push timeline, reviews and anchored
//! comments land in the changes domain, issues in the work domain, and placing everything
//! a second time (a fresh journal against the same forge) duplicates nothing.

use crate::common;
use std::sync::Arc;

use bee_forge::Forge;
use bee_forge::changes::{ChangeHooks, Changes, HistoryVcs, MemoryTeams};
use bee_forge::cx::Cx;
use bee_forge::forge::{ForgeConfig, Hooks};
use bee_forge::ids::{Clock as ForgeClock, SystemClock};
use bee_forge::orgs::repos::NewRepo;
use bee_forge::store::memory::MemoryStore;
use bee_forge::work::RepoInfo;
use bee_github::Importer;
use bee_github::hub::HubSink;
use bee_github::report::category;
use bee_github::state::StateDir;
use bee_proto::v1::{ChangeState, Principal, PrincipalKind, Repo};
use bee_store::{OrgId, RepoId};
use common::*;

struct Hub {
    forge: Arc<Forge>,
    changes: Arc<Changes>,
    repo: Repo,
}

async fn hub(stores_out: &mut Option<Stores>) -> Hub {
    let clock: Arc<dyn ForgeClock> = Arc::new(SystemClock);
    let ring =
        bee_auth::KeyRing::new(bee_auth::SigningKey::from_secret_bytes(1, &[9u8; 32]).unwrap());
    let (forge, _) = Forge::open(
        Arc::new(MemoryStore::new()),
        clock.clone(),
        ring.verifying_keys(),
        Arc::new(bee_auth::InMemoryRevocations::new()),
        Hooks::default(),
        ForgeConfig::default(),
    )
    .await
    .unwrap();
    let cx = Cx::new("owner", PrincipalKind::Human);
    let owner = Principal {
        id: "owner".into(),
        kind: PrincipalKind::Human as i32,
        display: "owner".into(),
        ..Default::default()
    };
    let org = forge.orgs.create_org(&cx, OWNER, "", owner).await.unwrap();
    let repo = forge
        .orgs
        .create_repo(
            &cx,
            &org.id,
            NewRepo {
                id: forge.db.ids().ulid().unwrap(),
                name: NAME.into(),
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();
    let st = stores_with(
        OrgId(bee_core::Ulid::parse(&org.id).unwrap()),
        RepoId(bee_core::Ulid::parse(&repo.id).unwrap()),
    )
    .await;
    let vcs = Arc::new(HistoryVcs::new(st.writer.clone(), clock));
    let changes = Changes::new(
        &forge,
        ChangeHooks {
            vcs: vcs.clone(),
            lander: vcs,
            teams: Arc::new(MemoryTeams::new()),
        },
    );
    *stores_out = Some(st);
    Hub {
        forge,
        changes,
        repo,
    }
}

fn importer(w: &World, h: &Hub, state: &std::path::Path) -> Importer {
    let sink = HubSink::new(
        h.forge.clone(),
        h.changes.clone(),
        h.repo.clone(),
        w.stores.writer.clone(),
    )
    .unwrap()
    .with_assets(None);
    let client = w.client_with(w.config(), bee_github::auth::Credential::Token("t".into()));
    Importer::new(
        client,
        StateDir::open(state).unwrap(),
        w.stores.target(),
        Arc::new(sink),
        w.options(),
    )
}

async fn comment_counts(h: &Hub) -> Vec<(u64, usize, usize, usize)> {
    let cx = Cx::system();
    let mut out = Vec::new();
    for n in [2u64, 3, 5] {
        let cr = h.changes.get_by_number(&cx, &h.repo, n).await.unwrap();
        let (cs, _) = h
            .changes
            .comments(&cx, &h.repo, &cr.id, "", 100)
            .await
            .unwrap();
        let (ts, _) = h
            .changes
            .threads(&cx, &h.repo, &cr.id, None, "", 100)
            .await
            .unwrap();
        let rv = h.changes.reviews(&cx, &h.repo, &cr.id).await.unwrap();
        out.push((
            n,
            cs.len(),
            ts.iter().map(|t| t.comments.len()).sum(),
            rv.len(),
        ));
    }
    out
}

#[tokio::test]
async fn as_mig_01_hub_keeps_numbers_reviews_and_anchors() {
    let mut w = World::new(false).await;
    let mut st = None;
    let h = hub(&mut st).await;
    w.stores = st.unwrap();
    let r = importer(&w, &h, w.state.path()).run().await.unwrap();
    let cx = Cx::system();
    // Change requests with GitHub's numbers.
    let pr2 = h.changes.get_by_number(&cx, &h.repo, 2).await.unwrap();
    assert_eq!(pr2.number, 2);
    assert_eq!(pr2.state, ChangeState::Open as i32);
    assert_eq!(pr2.target_ref, "main");
    assert_eq!(pr2.revisions.len(), 2, "first push and the force-push");
    assert_eq!(
        pr2.author.as_ref().unwrap().id,
        bee_github::principals::principal_id(ALICE)
    );
    assert!(pr2.body.contains("https://hub.test/acme/api/issues/1"));
    let pr3 = h.changes.get_by_number(&cx, &h.repo, 3).await.unwrap();
    assert_eq!(pr3.state, ChangeState::Closed as i32);
    assert!(
        pr3.close_reason.contains("merged on GitHub"),
        "{}",
        pr3.close_reason
    );
    assert_eq!(
        pr3.target_ref, "pull/3/base",
        "anchored on its base: its commits are already on main"
    );
    let pr5 = h.changes.get_by_number(&cx, &h.repo, 5).await.unwrap();
    assert_eq!(pr5.state, ChangeState::Closed as i32);
    // Issues with their numbers, in the work domain.
    let info = RepoInfo::from(&h.repo);
    let i1 = h
        .forge
        .work
        .get_issue_by_number(&cx, &info, 1)
        .await
        .unwrap();
    assert_eq!(i1.title, "Crash on start");
    let i4 = h
        .forge
        .work
        .get_issue_by_number(&cx, &info, 4)
        .await
        .unwrap();
    assert_eq!(i4.state, bee_proto::v1::IssueState::Closed as i32);
    // Reviews, threads and conversation comments on #2: approved + commented reviews, the
    // anchored comment with its reply, the comment whose line vanished as a file comment.
    let counts = comment_counts(&h).await;
    assert_eq!(counts[0], (2, 1, 3, 2), "{counts:?}");
    let rc = r.report.category(category::REVIEW_COMMENTS).unwrap();
    assert_eq!(
        (rc.imported, rc.archived, rc.dropped),
        (2, 1, 0),
        "{}",
        r.report.human()
    );
    let prs = r.report.category(category::PULL_REQUESTS).unwrap();
    assert_eq!(prs.dropped, 0, "{}", r.report.human());
    assert_eq!(r.report.github.unexplained_drops, 0);
    validate_report(&r.report.to_value());

    // A fresh journal against the same forge places nothing twice.
    let again = tempfile::tempdir().unwrap();
    let r2 = importer(&w, &h, again.path()).run().await.unwrap();
    assert_eq!(comment_counts(&h).await, counts);
    assert_eq!(
        r2.report.category(category::PULL_REQUESTS).unwrap().dropped,
        0,
        "{}",
        r2.report.human()
    );
    assert_eq!(
        r2.report.category(category::ISSUES).unwrap().dropped,
        0,
        "{}",
        r2.report.human()
    );
    let (cs, _) = h
        .forge
        .work
        .list_comments(&cx, &info, 1, None, 100, "")
        .await
        .unwrap();
    assert_eq!(cs.len(), 1);
}
