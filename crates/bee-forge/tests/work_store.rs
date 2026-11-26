//! The work store on every backend (memory, SQLite, Postgres when `BEE_TEST_PG` is set):
//! one number space shared by issues and change requests under concurrency, and the issue and
//! project paths that touch several tables in one transaction.

use crate::common;
use std::collections::BTreeSet;
use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::work::{Actor, IssueTarget, ItemContent, NewIssue, NumberKind, OwnerInfo, RepoInfo};
use bee_proto::v1::{
    IssueState, IssueStateReason, Principal, PrincipalKind, ProjectFieldKind, ProjectFieldValue,
    ProjectOwnerKind, project_field_value::Value,
};

fn actor() -> Actor {
    Actor::staff(Principal {
        id: "tester".into(),
        kind: PrincipalKind::Human as i32,
        display: "tester".into(),
        ..Default::default()
    })
}

async fn scenario(store: Arc<dyn ForgeStore>, name: &str) {
    let env = common::env_with(store, Arc::new(SystemClock)).await;
    let work = env.forge.work.clone();
    let cx = Cx::system();
    let repo = RepoInfo {
        id: format!("01REPO{name}"),
        org_id: format!("01ORG{name}"),
        full_name: "acme/app".into(),
    };

    // Issues and change requests allocate from one number space, concurrently.
    let mut tasks = Vec::new();
    for i in 0..24 {
        let (work, repo) = (work.clone(), repo.clone());
        tasks.push(tokio::spawn(async move {
            let cx = Cx::system();
            if i % 2 == 0 {
                work.create_issue(
                    &cx,
                    &repo,
                    &actor(),
                    NewIssue {
                        title: format!("issue {i}"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .number
            } else {
                work.allocate_number(&cx, &repo.id, NumberKind::ChangeRequest, &format!("CR{i}"))
                    .await
                    .unwrap()
            }
        }));
    }
    let mut seen = BTreeSet::new();
    for t in tasks {
        assert!(seen.insert(t.await.unwrap()), "number handed out twice");
    }
    assert_eq!(
        seen,
        (1..=24).collect::<BTreeSet<u64>>(),
        "{name}: gaps in the shared space"
    );
    // Reserving a used number fails; a free one moves the counter past it.
    assert!(
        work.reserve_number(&cx, &repo.id, 5, NumberKind::Other, "x")
            .await
            .is_err()
    );
    work.reserve_number(&cx, &repo.id, 100, NumberKind::Other, "x")
        .await
        .unwrap();
    assert_eq!(
        work.allocate_number(&cx, &repo.id, NumberKind::ChangeRequest, "y")
            .await
            .unwrap(),
        101
    );
    let entry = work.number_entry(&cx, &repo.id, 3).await.unwrap().unwrap();
    assert!(matches!(
        entry.kind(),
        NumberKind::Issue | NumberKind::ChangeRequest
    ));

    // Issue lifecycle, comments with history, a project whose automation follows the close.
    let issue = work
        .create_issue(
            &cx,
            &repo,
            &actor(),
            NewIssue {
                title: "tracked".into(),
                body: "- [ ] a\n- [x] b".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(issue.number, 102);
    assert_eq!(issue.task_list.unwrap().total, 2);
    let c = work
        .create_comment(
            &cx,
            &repo,
            &actor(),
            102,
            "first",
            "",
            false,
            false,
            &Default::default(),
        )
        .await
        .unwrap();
    work.update_comment(
        &cx,
        &repo,
        &actor(),
        &c.id,
        "second",
        0,
        &Default::default(),
    )
    .await
    .unwrap();
    let (edits, _) = work.list_comment_edits(&cx, &c.id, 10, "").await.unwrap();
    assert_eq!(edits[0].previous_body, "first");
    let owner = OwnerInfo {
        id: repo.org_id.clone(),
        login: "acme".into(),
        kind: ProjectOwnerKind::Org,
    };
    let p = work
        .create_project(&cx, &owner, &actor(), "Board", "", "", false, &[])
        .await
        .unwrap();
    let item = work
        .add_item(
            &cx,
            &actor(),
            &p.id,
            ItemContent::Issue(IssueTarget::Number {
                repo_id: repo.id.clone(),
                number: 102,
            }),
        )
        .await
        .unwrap();
    let status = |item: &bee_proto::v1::ProjectItem| {
        item.field_values
            .iter()
            .find(|v| v.field_name == "Status")
            .map(|v| v.display_value.clone())
            .unwrap_or_default()
    };
    assert_eq!(status(&item), "Todo");
    let closed = work
        .close_issue(
            &cx,
            &repo,
            &actor(),
            102,
            IssueStateReason::Completed,
            None,
            "",
            0,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(closed.state, IssueState::Closed as i32);
    let after = work.get_item(&cx, &p.id, &item.id).await.unwrap();
    assert_eq!(status(&after), "Done");
    // A custom field round trip.
    let f = work
        .create_project_field(&cx, &p.id, "Effort", ProjectFieldKind::Number, &[], None)
        .await
        .unwrap();
    let set = work
        .set_item_value(
            &cx,
            &actor(),
            &p.id,
            &item.id,
            &ProjectFieldValue {
                field_id: f.id.clone(),
                value: Some(Value::Number(2.5)),
                ..Default::default()
            },
            0,
        )
        .await
        .unwrap();
    assert!(
        set.field_values
            .iter()
            .any(|v| v.field_name == "Effort" && v.display_value == "2.5")
    );
    // Deleting the issue removes its item and keeps its number retired.
    work.delete_issue(&cx, &repo, &actor(), 102).await.unwrap();
    assert!(work.get_item(&cx, &p.id, &item.id).await.is_err());
    assert_eq!(
        work.number_entry(&cx, &repo.id, 102)
            .await
            .unwrap()
            .unwrap()
            .kind(),
        NumberKind::Retired
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_on_memory() {
    scenario(Arc::new(MemoryStore::new()), "mem").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_on_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    scenario(Arc::new(store), "sqlite").await;
    assert!(dir.path().join("forge-work.db").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_on_postgres_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_work_test_{}", std::process::id());
    }
    cfg.default_pool_size = 4;
    let store = SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap();
    scenario(Arc::new(store), "pg").await;
}
