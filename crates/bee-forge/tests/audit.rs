//! The hash-chained audit log: append, verify, tamper detection, query filters, stream.

use std::sync::Arc;
use std::time::Duration;

use bee_forge::Cx;
use bee_forge::audit::{AuditDraft, AuditLog, ENTRIES, EntryKey, entry_hash, org_scope};
use bee_forge::ids::{IdGen, ManualClock};
use bee_forge::module::Registry;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::{Db, ForgeStore, Row, Surface};
use bee_proto::v1::{AuditEntry, AuditLogFilter, PrincipalKind};

const ORG: &str = "01J00000000000000000000RG1";

async fn setup() -> (Db, Arc<AuditLog>, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(1_767_225_600_000)); // 2026-01-01
    let store = Arc::new(MemoryStore::new());
    store
        .migrate(&Registry::core().unwrap().migrations())
        .await
        .unwrap();
    let db = Db::new(store, Arc::new(IdGen::new(clock.clone())));
    (db.clone(), Arc::new(AuditLog::new(db)), clock)
}

fn cx(who: &str) -> Cx {
    let mut c =
        Cx::new(who, PrincipalKind::Agent).with_chain(vec![who.into(), "human-root".into()]);
    c.request_id = format!("req-{who}");
    c.ip = "10.0.0.1".into();
    c.decision = "role:admin".into();
    c
}

async fn append(log: &AuditLog, clock: &ManualClock, who: &str, action: &str) -> AuditEntry {
    let e = AuditDraft::new(action, ORG).after(r#"{"x":1}"#).into_entry(
        &cx(who),
        clock.advance(1000),
        String::new(),
    );
    log.append(&cx(who), e).await.unwrap()
}

async fn edit(db: &Db, seq: u64, f: impl FnOnce(&mut AuditEntry)) {
    let scope = org_scope(ORG);
    let id = format!("{seq:020}");
    let mut tx = db.begin(Surface::AUDIT, &Cx::system()).await.unwrap();
    let row = tx.get(&ENTRIES, &scope, &id).await.unwrap().unwrap();
    let mut e: AuditEntry = row.decode().unwrap();
    f(&mut e);
    tx.put(
        &ENTRIES,
        &Row {
            body: prost::Message::encode_to_vec(&e),
            ..row
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn entries_chain_and_record_who_what_where_why() {
    let (_db, log, clock) = setup().await;
    let a = append(&log, &clock, "agent-1", "repo.create").await;
    let b = append(&log, &clock, "agent-2", "lease.acquire").await;
    assert_eq!((a.seq, b.seq), (1, 2));
    assert_eq!(a.prev_hash, vec![0u8; 32]);
    assert_eq!(b.prev_hash, a.hash);
    assert_eq!(
        b.delegation_chain,
        vec!["agent-2".to_string(), "human-root".to_string()]
    );
    assert_eq!(b.on_behalf_of, "human-root");
    assert_eq!(b.request_id, "req-agent-2");
    assert_eq!(b.ip, "10.0.0.1");
    assert_eq!(b.result, "ok");
    let after: serde_json::Value = serde_json::from_str(&b.after).unwrap();
    assert_eq!(after["policy_decision"], "role:admin");
    assert_eq!(after["state"]["x"], 1);
    // Appending the same entry id twice is a no-op.
    let again = log.append(&cx("agent-2"), b.clone()).await.unwrap();
    assert_eq!(again.seq, 2);
    let v = log
        .verify(&Cx::system(), &org_scope(ORG), 0, 0, &b.hash)
        .await
        .unwrap();
    assert!(v.ok, "{v:?}");
    assert_eq!((v.verified_from_seq, v.verified_to_seq), (1, 2));
    assert_eq!(v.head_hash, b.hash);
    let by_id = log
        .get(&Cx::system(), &org_scope(ORG), &EntryKey::Id(a.id.clone()))
        .await
        .unwrap();
    assert_eq!(by_id.seq, 1);
}

#[tokio::test]
async fn tampering_is_detected() {
    let (db, log, clock) = setup().await;
    for i in 0..5 {
        append(&log, &clock, "agent", &format!("repo.update{i}")).await;
    }
    let scope = org_scope(ORG);
    let sys = Cx::system();
    assert!(log.verify(&sys, &scope, 0, 0, &[]).await.unwrap().ok);

    // An edited entry no longer matches its hash.
    edit(&db, 3, |e| e.action = "repo.delete".into()).await;
    let v = log.verify(&sys, &scope, 0, 0, &[]).await.unwrap();
    assert!(!v.ok);
    assert_eq!(
        (v.first_broken_seq, v.failure.as_str()),
        (3, "hash_mismatch")
    );
    assert_eq!(v.verified_to_seq, 2);

    // Re-hashing the edit breaks the link to the next entry instead.
    edit(&db, 3, |e| e.hash = entry_hash(e)).await;
    let v = log.verify(&sys, &scope, 0, 0, &[]).await.unwrap();
    assert_eq!(
        (v.first_broken_seq, v.failure.as_str()),
        (4, "prev_hash_mismatch")
    );

    // A removed entry is a gap.
    let (db2, log2, clock2) = setup().await;
    for i in 0..4 {
        append(&log2, &clock2, "agent", &format!("x{i}")).await;
    }
    let mut tx = db2.begin(Surface::AUDIT, &sys).await.unwrap();
    tx.delete(&ENTRIES, &scope, &format!("{:020}", 2))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let v = log2.verify(&sys, &scope, 0, 0, &[]).await.unwrap();
    assert_eq!((v.first_broken_seq, v.failure.as_str()), (2, "gap"));

    // A truncated tail no longer reaches the recorded head; a stale anchor is caught.
    let (db3, log3, clock3) = setup().await;
    let mut last = Vec::new();
    for i in 0..3 {
        last = append(&log3, &clock3, "agent", &format!("y{i}")).await.hash;
    }
    let v = log3.verify(&sys, &scope, 0, 0, &[1, 2, 3]).await.unwrap();
    assert_eq!(v.failure, "head_mismatch");
    let mut tx = db3.begin(Surface::AUDIT, &sys).await.unwrap();
    tx.delete(&ENTRIES, &scope, &format!("{:020}", 3))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let v = log3.verify(&sys, &scope, 0, 0, &last).await.unwrap();
    assert!(!v.ok);
    assert_eq!(v.first_broken_seq, 3);
}

#[tokio::test]
async fn query_filters_and_paging() {
    let (_db, log, clock) = setup().await;
    append(&log, &clock, "a", "repo.create").await;
    append(&log, &clock, "b", "lease.acquire").await;
    append(&log, &clock, "a", "lease.release").await;
    let sys = Cx::system();
    let scope = org_scope(ORG);
    let q = |f: AuditLogFilter, oldest: bool, token: &str, n: usize| {
        let log = log.clone();
        let sys = sys.clone();
        let scope = scope.clone();
        let token = token.to_string();
        async move {
            log.query(&sys, &scope, &f, oldest, &token, n)
                .await
                .unwrap()
        }
    };
    let (all, _) = q(AuditLogFilter::default(), false, "", 10).await;
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 2, 1]);
    let (leases, _) = q(
        AuditLogFilter {
            action_prefixes: vec!["lease.".into()],
            ..Default::default()
        },
        true,
        "",
        10,
    )
    .await;
    assert_eq!(leases.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
    let (by_a, _) = q(
        AuditLogFilter {
            phrase: "actor:a action:lease.".into(),
            ..Default::default()
        },
        true,
        "",
        10,
    )
    .await;
    assert_eq!(by_a.len(), 1);
    let (dated, _) = q(
        AuditLogFilter {
            phrase: "created:2026-01-01".into(),
            ..Default::default()
        },
        true,
        "",
        10,
    )
    .await;
    assert_eq!(dated.len(), 3);
    let (none, _) = q(
        AuditLogFilter {
            phrase: "created:<2025-12-31".into(),
            ..Default::default()
        },
        true,
        "",
        10,
    )
    .await;
    assert!(none.is_empty());
    let (p1, next) = q(AuditLogFilter::default(), true, "", 2).await;
    let (p2, end) = q(AuditLogFilter::default(), true, &next, 2).await;
    assert_eq!(p1.len() + p2.len(), 3);
    assert_eq!(p2[0].seq, 3);
    assert!(end.is_empty());
    let (root, _) = q(
        AuditLogFilter {
            on_behalf_of: "human-root".into(),
            ..Default::default()
        },
        true,
        "",
        10,
    )
    .await;
    assert_eq!(root.len(), 3);
}

#[tokio::test]
async fn stream_follows_new_entries() {
    let (_db, log, clock) = setup().await;
    append(&log, &clock, "a", "repo.create").await;
    let mut rx = log
        .stream(
            &Cx::system(),
            &org_scope(ORG),
            0,
            &AuditLogFilter {
                action_prefixes: vec!["lease.".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    append(&log, &clock, "a", "repo.update").await;
    append(&log, &clock, "a", "lease.acquire").await;
    let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!((e.seq, e.action.as_str()), (3, "lease.acquire"));
}
