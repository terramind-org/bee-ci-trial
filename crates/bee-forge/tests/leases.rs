//! Leases (SPEC §11.5): the advisory/exclusive overlap matrix, expiry and renewal, release,
//! auto-release on land/close, the land check, and lease events.

use crate::common;
use std::sync::Arc;

use bee_forge::Cx;
use bee_forge::ids::{IdGen, ManualClock, SystemClock};
use bee_forge::leases::{AcquireRequest, Leases};
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_forge::store::unavailable::UnavailableStore;
use bee_forge::store::{Db, ForgeStore};
use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::{ChangeId, ChangeRequest, ErrorReason, Event, LeaseMode, PrincipalKind, event};

const REPO: &str = "01J0000000000000000000REP0";
const ORG: &str = "01J0000000000000000000ORG0";

fn cx(who: &str) -> Cx {
    Cx::new(who, PrincipalKind::Agent)
}

fn acq(paths: &[&str], mode: LeaseMode, r: &str) -> AcquireRequest {
    AcquireRequest {
        repo_id: REPO.into(),
        org_id: ORG.into(),
        ref_name: r.into(),
        paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
        mode,
        ..AcquireRequest::default()
    }
}

/// Separate Postgres store handles model two forge replicas. An exclusive acquire must lock
/// its repository before looking for conflicts, including while the lock row is first created.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn postgres_exclusive_lease_race_has_one_winner_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_lease_race_{}", std::process::id());
    }
    let first: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    first
        .migrate(&bee_forge::module::Registry::core().unwrap().migrations())
        .await
        .unwrap();
    let left = Arc::new(Leases::new(Db::new(
        first,
        Arc::new(IdGen::new(Arc::new(SystemClock))),
    )));
    let right = Arc::new(Leases::new(Db::new(
        second,
        Arc::new(IdGen::new(Arc::new(SystemClock))),
    )));

    let different_left = cx("different-left");
    let different_right = cx("different-right");
    let different_repos = tokio::join!(
        left.acquire(
            &different_left,
            acq(&["shared/config"], LeaseMode::Exclusive, "main")
        ),
        right.acquire(
            &different_right,
            AcquireRequest {
                repo_id: "01J0000000000000000000REP1".into(),
                ..acq(&["shared/config"], LeaseMode::Exclusive, "main")
            },
        )
    );
    assert!(different_repos.0.is_ok());
    assert!(different_repos.1.is_ok());
    let (different_lease, _) = different_repos.0.unwrap();
    left.release(&different_left, ORG, REPO, &different_lease.id, false)
        .await
        .unwrap();

    for round in 0..24 {
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..8 {
            let leases = if i % 2 == 0 {
                left.clone()
            } else {
                right.clone()
            };
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                (
                    i,
                    leases
                        .acquire(
                            &cx(&format!("agent-{round}-{i}")),
                            acq(&["shared/config"], LeaseMode::Exclusive, "main"),
                        )
                        .await,
                )
            });
        }
        let mut winner = None;
        while let Some(result) = tasks.join_next().await {
            let (i, result) = result.unwrap();
            match result {
                Ok((lease, _)) => {
                    assert!(
                        winner.replace((i, lease)).is_none(),
                        "more than one winner in round {round}"
                    );
                }
                Err(error) => assert_eq!(error.reason, ErrorReason::Leased, "round {round}"),
            }
        }
        let (i, lease) = winner.expect("one winner");
        let leases = if i % 2 == 0 { &left } else { &right };
        leases
            .release(
                &cx(&format!("agent-{round}-{i}")),
                ORG,
                REPO,
                &lease.id,
                false,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn overlap_matrix() {
    use LeaseMode::{Advisory as A, Exclusive as X};
    // (existing holder, mode, paths, ref) then (new holder, mode, paths, ref) -> outcome
    // Some(n) = ok with n overlapping leases reported; None = LEASED.
    type Side<'a> = (&'a str, LeaseMode, &'a [&'a str], &'a str);
    type Case<'a> = (Side<'a>, Side<'a>, Option<usize>);
    let rows: &[Case] = &[
        (
            ("a", A, &["src"], "main"),
            ("b", A, &["src/x.rs"], "main"),
            Some(1),
        ),
        (
            ("a", A, &["src"], "main"),
            ("b", X, &["src"], "main"),
            Some(1),
        ),
        (
            ("a", X, &["src"], "main"),
            ("b", A, &["src/x.rs"], "main"),
            None,
        ),
        (
            ("a", X, &["src/x.rs"], "main"),
            ("b", X, &["src"], "main"),
            None,
        ),
        (
            ("a", X, &["src"], "main"),
            ("b", X, &["src2"], "main"),
            Some(0),
        ),
        (
            ("a", X, &["src"], "main"),
            ("b", X, &["src"], "release"),
            Some(0),
        ),
        (
            ("a", X, &["src"], "main"),
            ("a", X, &["src"], "main"),
            Some(0),
        ),
        (("a", X, &["/"], "main"), ("b", A, &["docs"], "main"), None),
        (
            ("a", A, &["docs"], "main"),
            ("b", A, &["src"], "main"),
            Some(0),
        ),
    ];
    for (i, ((h1, m1, p1, r1), (h2, m2, p2, r2), want)) in rows.iter().enumerate() {
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let db = Db::new(Arc::new(MemoryStore::new()), Arc::new(IdGen::new(clock)));
        db.store()
            .migrate(&bee_forge::module::Registry::core().unwrap().migrations())
            .await
            .unwrap();
        let leases = Leases::new(db);
        leases.acquire(&cx(h1), acq(p1, *m1, r1)).await.unwrap();
        let got = leases.acquire(&cx(h2), acq(p2, *m2, r2)).await;
        match (want, got) {
            (Some(n), Ok((_, overlapping))) => assert_eq!(overlapping.len(), *n, "case {i}"),
            (None, Err(e)) => {
                assert_eq!(e.reason, ErrorReason::Leased, "case {i}");
                assert!(
                    matches!(e.payload.as_deref(), Some(Payload::LeaseConflict(c)) if c.lease_ids.len() == 1),
                    "case {i}"
                );
            }
            (w, g) => panic!("case {i}: wanted {w:?}, got {g:?}"),
        }
    }
}

async fn setup() -> (Arc<ManualClock>, common::Env) {
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let env = common::env_with(Arc::new(MemoryStore::new()), clock.clone()).await;
    (clock, env)
}

#[tokio::test]
async fn leases_expire_renew_and_release() {
    let (clock, env) = setup().await;
    let leases = &env.forge.leases;
    let mut r = acq(&["src"], LeaseMode::Exclusive, "main");
    r.ttl_seconds = 60;
    let (lease, _) = leases.acquire(&cx("a"), r).await.unwrap();

    // Renewal: holder only; bounded TTL.
    assert_eq!(
        leases
            .renew(&cx("b"), ORG, REPO, &lease.id, 60)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        leases
            .renew(&cx("a"), ORG, REPO, &lease.id, 10 * 86_400)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidArgument
    );
    clock.advance(50_000);
    leases
        .renew(&cx("a"), ORG, REPO, &lease.id, 60)
        .await
        .unwrap();
    clock.advance(50_000);
    leases
        .get(&cx("a"), REPO, &lease.id)
        .await
        .expect("renewed lease alive");

    // Expiry: invisible at once, then swept with an event.
    clock.advance(11_000);
    assert_eq!(
        leases
            .get(&cx("a"), REPO, &lease.id)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    assert!(leases.list(&cx("a"), REPO, &[]).await.unwrap().is_empty());
    leases
        .acquire(&cx("b"), acq(&["src"], LeaseMode::Exclusive, "main"))
        .await
        .expect("free after expiry");
    assert_eq!(leases.sweep_expired(&Cx::system()).await.unwrap(), 1);

    // Release: holder, or forced by a maintainer.
    let (held, _) = leases
        .acquire(&cx("c"), acq(&["docs"], LeaseMode::Advisory, "main"))
        .await
        .unwrap();
    assert_eq!(
        leases
            .release(&cx("d"), ORG, REPO, &held.id, false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    leases
        .release(&cx("d"), ORG, REPO, &held.id, true)
        .await
        .unwrap();

    env.forge.flush().await.unwrap();
    let (events, _) = env
        .forge
        .events
        .read(&Cx::system(), REPO, 0, &["lease.*".into()], 100)
        .await
        .unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "lease.acquired",
            "lease.renewed",
            "lease.acquired",
            "lease.expired",
            "lease.acquired",
            "lease.released"
        ]
    );
    assert!(events.iter().all(|e| e.org_id == ORG));
    // Every change is audited too.
    let (entries, _) = env
        .forge
        .audit
        .query(
            &Cx::system(),
            &format!("org:{ORG}"),
            &Default::default(),
            true,
            "",
            100,
        )
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"lease.break"), "{actions:?}");
    assert!(actions.contains(&"lease.expire"), "{actions:?}");
}

#[tokio::test]
async fn landing_or_closing_a_change_releases_its_leases() {
    let (_clock, env) = setup().await;
    let change = ChangeId { raw: vec![7; 16] };
    let mut r = acq(&["src"], LeaseMode::Exclusive, "main");
    r.change_id = Some(change.clone());
    env.forge.leases.acquire(&cx("a"), r).await.unwrap();
    env.forge
        .leases
        .acquire(&cx("a"), acq(&["docs"], LeaseMode::Advisory, "main"))
        .await
        .unwrap();
    let landed = Event {
        kind: "cr.landed".into(),
        repo_id: REPO.into(),
        org_id: ORG.into(),
        body: Some(event::Body::Change(ChangeRequest {
            change_ids: vec![change],
            ..Default::default()
        })),
        ..Default::default()
    };
    env.forge
        .events
        .append(&Cx::system(), landed, "landed-1")
        .await
        .unwrap();
    let left = env.forge.leases.list(&cx("a"), REPO, &[]).await.unwrap();
    assert_eq!(
        left.len(),
        1,
        "only the lease without the change id remains"
    );
    assert_eq!(left[0].paths, vec![b"docs".to_vec()]);
}

#[tokio::test]
async fn land_check_and_policy_input() {
    let (_clock, env) = setup().await;
    let leases = &env.forge.leases;
    leases
        .acquire(&cx("a"), acq(&["src/auth"], LeaseMode::Exclusive, "main"))
        .await
        .unwrap();
    let paths = vec!["src/auth/login.rs".to_string()];
    let err = leases
        .land_check(&cx("b"), REPO, "main", &paths, "b")
        .await
        .unwrap_err();
    assert_eq!(err.reason, ErrorReason::Leased);
    leases
        .land_check(&cx("a"), REPO, "main", &paths, "a")
        .await
        .expect("holder lands");
    leases
        .land_check(&cx("b"), REPO, "release", &paths, "b")
        .await
        .expect("other ref");
    let info = leases.policy_leases(&cx("b"), REPO, "main").await.unwrap();
    assert_eq!(info.len(), 1);
    assert!(info[0].exclusive);

    // The land path never blocks on a forge outage.
    let broken = Leases::new(Db::new(
        Arc::new(UnavailableStore),
        Arc::new(IdGen::new(Arc::new(ManualClock::new(1)))),
    ));
    broken
        .land_check_fail_open(&cx("b"), REPO, "main", &paths, "b")
        .await
        .expect("fail open");
    assert_eq!(
        broken
            .land_check(&cx("b"), REPO, "main", &paths, "b")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::Unavailable
    );
}

#[tokio::test]
async fn invalid_requests_are_rejected() {
    let (_clock, env) = setup().await;
    let leases = &env.forge.leases;
    for bad in [
        acq(&[], LeaseMode::Advisory, "main"),
        acq(&["a/../b"], LeaseMode::Advisory, "main"),
    ] {
        assert_eq!(
            leases.acquire(&cx("a"), bad).await.unwrap_err().reason,
            ErrorReason::InvalidArgument
        );
    }
    let mut long = acq(&["x"], LeaseMode::Advisory, "main");
    long.ttl_seconds = 25 * 3600;
    assert_eq!(
        leases.acquire(&cx("a"), long).await.unwrap_err().subcode,
        "ttl_too_long"
    );
}
