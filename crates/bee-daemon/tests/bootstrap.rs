//! Daemon bootstrap from server snapshots (`ObjectService/GetDaemonSnapshot`).

use crate::common;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bee_daemon::{TierConfig, TierKind, Upstream};
use bee_wc::Sparse;
use common::*;
use serde_json::json;

fn policy(user: &str) -> String {
    format!(
        "version = 1\n[members]\nadmin = [\"{user}\"]\nwrite = [\"@humans\", \"@agents\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\
         [[access]]\npath = \"/secret\"\nread = [\"@humans\"]\nwrite = [\"@humans\"]\n"
    )
}

#[test]
fn daemon_bootstraps_from_a_verified_snapshot_and_fetches_no_object_after() {
    let srv = start_server();
    let mut files: Vec<(String, Vec<u8>)> = (0..300)
        .map(|i| {
            (
                format!("scope/m{:02}/f{i:04}.txt", i / 50),
                format!("scope file {i}\n").into_bytes(),
            )
        })
        .collect();
    files.push((
        ".beehive/policy.toml".into(),
        policy(&srv.user).into_bytes(),
    ));
    files.push(("secret/key.txt".into(), b"hidden\n".to_vec()));
    srv.land_files(&files, "seed");
    let up: Arc<dyn Upstream> = Arc::new(srv.http_upstream());
    let mut d = start_daemon(&srv.rt, up, TierConfig::new(TierKind::Daemon));
    let agent = srv.agent_token("bootstrapper");
    // The snapshot is built for its caller: the agent's full snapshot lacks the secret.
    let human_full = d
        .client(Some(&srv.token))
        .local("Bootstrap", &json!({ "repo": REPO }))
        .expect("human bootstrap");
    let agent_full = d
        .client(Some(&agent))
        .local("Bootstrap", &json!({ "repo": REPO }))
        .expect("agent bootstrap");
    assert_eq!(
        human_full["objects"].as_u64().expect("n"),
        agent_full["objects"].as_u64().expect("n") + 1
    );
    assert_eq!(agent_full["loaded"], 0, "everything was already cached");
    // A fresh daemon bootstraps a scope, then serves a clone without fetching any object.
    let counting = Arc::new(Counting::new(srv.http_upstream()));
    let up2: Arc<dyn Upstream> = counting.clone();
    let mut d2 = start_daemon(&srv.rt, up2, TierConfig::new(TierKind::Daemon));
    let t0 = Instant::now();
    let r = d2
        .client(Some(&agent))
        .local("Bootstrap", &json!({ "repo": REPO, "sparse": ["scope"] }))
        .expect("bootstrap");
    eprintln!(
        "bootstrap of a 300-object scope: {:.0} ms ({} bytes)",
        t0.elapsed().as_secs_f64() * 1000.0,
        r["bytes"]
    );
    assert_eq!(r["objects"], 300);
    assert_eq!(r["loaded"], 300);
    assert_eq!(r["ref"], "main");
    let dir = tempfile::tempdir().expect("dir");
    let wc = clone_wc(
        &dir.path().join("wc"),
        &d2.transport(&agent),
        Sparse::from_prefixes(["scope"]).expect("sparse"),
    );
    assert_eq!(wc.manifest().len(), 300);
    assert_eq!(
        counting.objects_fetched(),
        0,
        "no object bytes after bootstrap"
    );
    assert!(
        counting.has_calls.load(Ordering::SeqCst) > 0,
        "decisions still come from the tier above"
    );
    drop(wc);
    d2.stop(&srv.rt);
    d.stop(&srv.rt);
}
