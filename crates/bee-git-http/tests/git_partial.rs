//! Partial clone filters and `object-info` with the real `git` (and `curl` for the raw
//! protocol v2 command): `blob:limit=<n>` sends only small blobs, `tree:<depth>` stops at a
//! depth, and the missing objects arrive lazily (`docs/spec/git-gateway.md` §5).

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use common::*;

/// A source like `make_source` plus a 6 KiB file, imported as `dev/r`.
async fn source_with_a_big_file(env: &Env) -> std::path::PathBuf {
    let src = make_source(env).await;
    let big: Vec<u8> = (0..6144u32).map(|i| b'a' + (i % 26) as u8).collect();
    write(&src.join("assets/big.bin"), &big);
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "a big file"]).await;
    env.import(&src, "r").await;
    src
}

/// Object ids `git rev-list --missing=print` reports missing in `dir`.
async fn missing(env: &Env, dir: &Path) -> BTreeSet<String> {
    env.git(dir, &["rev-list", "--objects", "--all", "--missing=print"])
        .await
        .lines()
        .filter_map(|l| l.strip_prefix('?'))
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_limit_clones_send_small_blobs_and_fetch_big_ones_lazily() {
    let env = start_env().await;
    let src = source_with_a_big_file(&env).await;
    let big = env.git(&src, &["rev-parse", "HEAD:assets/big.bin"]).await;
    let small = env.git(&src, &["rev-parse", "HEAD:a.txt"]).await;
    let dest = env.path("limited");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:limit=1k",
            "--no-checkout",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    let gone = missing(&env, &dest).await;
    assert!(gone.contains(&big), "the 6 KiB blob is left out: {gone:?}");
    assert!(!gone.contains(&small), "blobs under the limit are sent");
    assert!(
        gone.iter().all(|o| o == &big),
        "only blobs at or over 1 KiB are missing: {gone:?}"
    );
    env.git(&dest, &["checkout", "-q", "main"]).await;
    assert_eq!(
        std::fs::read(dest.join("assets/big.bin")).unwrap(),
        std::fs::read(src.join("assets/big.bin")).unwrap(),
        "fetched on demand"
    );
    assert!(missing(&env, &dest).await.is_empty());
    // A limit above every blob sends them all.
    let all = env.path("all");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:limit=1m",
            "--no-checkout",
            &env.url("r"),
            all.to_str().unwrap(),
        ],
    )
    .await;
    assert!(missing(&env, &all).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tree_depth_clones_stop_at_the_depth_and_fill_in_lazily() {
    let env = start_env().await;
    let src = source_with_a_big_file(&env).await;
    let root = env.git(&src, &["rev-parse", "HEAD^{tree}"]).await;
    let dir = env.git(&src, &["rev-parse", "HEAD:dir"]).await;
    let sub = env.git(&src, &["rev-parse", "HEAD:dir/sub"]).await;
    let top_blob = env.git(&src, &["rev-parse", "HEAD:a.txt"]).await;
    let dest = env.path("depth1");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=tree:1",
            "--no-checkout",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    let gone = missing(&env, &dest).await;
    assert!(!gone.contains(&root), "root trees (depth 0) are sent");
    // Entries of the root tree (depth 1) are left out; what is below them cannot even be
    // listed until they arrive.
    for o in [&dir, &top_blob] {
        assert!(gone.contains(o), "{o} is at depth 1: {gone:?}");
    }
    assert!(!gone.contains(&sub));
    // Depth 2 keeps `dir` (depth 1) but not what is inside it.
    let d2 = env.path("depth2");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--filter=tree:2",
            "--no-checkout",
            &env.url("r"),
            d2.to_str().unwrap(),
        ],
    )
    .await;
    let gone2 = missing(&env, &d2).await;
    assert!(!gone2.contains(&dir) && !gone2.contains(&top_blob));
    assert!(gone2.contains(&sub), "{gone2:?}");
    env.git(&dest, &["checkout", "-q", "main"]).await;
    assert_eq!(
        std::fs::read(dest.join("dir/sub/c.txt")).unwrap(),
        b"charlie\n"
    );
}

/// A pkt-line.
fn pkt(s: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", s.len() + 4).into_bytes();
    v.extend_from_slice(s.as_bytes());
    v
}

/// The data lines of a pkt-line stream, as text.
fn lines(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let n = usize::from_str_radix(std::str::from_utf8(&body[i..i + 4]).unwrap(), 16).unwrap();
        if n < 4 {
            i += 4;
            continue;
        }
        out.push(
            String::from_utf8_lossy(&body[i + 4..i + n])
                .trim_end_matches('\n')
                .to_string(),
        );
        i += n;
    }
    out
}

/// `curl` to the gateway with protocol v2: the response body.
async fn curl_v2(env: &Env, path: &str, body: Option<Vec<u8>>) -> Vec<u8> {
    let url = format!("http://{}/dev/r.git/{path}", env.addr());
    let user = format!("git:{}", env.token);
    let req = env.path("v2-request");
    let mut args = vec![
        "-sf".to_string(),
        "-u".into(),
        user,
        "-H".into(),
        "Git-Protocol: version=2".into(),
    ];
    if let Some(b) = body {
        std::fs::write(&req, b).unwrap();
        args.extend([
            "-H".into(),
            "Content-Type: application/x-git-upload-pack-request".into(),
            "--data-binary".into(),
            format!("@{}", req.display()),
        ]);
    }
    args.push(url);
    let out = tokio::task::spawn_blocking(move || {
        Command::new("curl")
            .args(&args)
            .output()
            .expect("curl runs")
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn object_info_reports_object_sizes_over_protocol_v2() {
    let env = start_env().await;
    let src = source_with_a_big_file(&env).await;
    let caps = lines(&curl_v2(&env, "info/refs?service=git-upload-pack", None).await);
    assert!(caps.iter().any(|l| l == "object-info"), "{caps:?}");
    let objects: Vec<(String, String)> =
        ["HEAD", "HEAD^{tree}", "HEAD:assets/big.bin", "HEAD:a.txt"]
            .iter()
            .map(|r| {
                let oid = std::process::Command::new("git")
                    .args(["rev-parse", r])
                    .current_dir(&src)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&oid.stdout).trim().to_string()
            })
            .map(|oid| {
                let size = std::process::Command::new("git")
                    .args(["cat-file", "-s", &oid])
                    .current_dir(&src)
                    .output()
                    .unwrap();
                (
                    oid,
                    String::from_utf8_lossy(&size.stdout).trim().to_string(),
                )
            })
            .collect();
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let mut body = pkt("command=object-info\n");
    body.extend(pkt("agent=git/test\n"));
    body.extend(b"0001");
    body.extend(pkt("size\n"));
    for (oid, _) in &objects {
        body.extend(pkt(&format!("oid {oid}\n")));
    }
    body.extend(pkt(&format!("oid {absent}\n")));
    body.extend(b"0000");
    let resp = lines(&curl_v2(&env, "git-upload-pack", Some(body)).await);
    let mut want = vec!["size".to_string()];
    want.extend(objects.iter().map(|(o, n)| format!("{o} {n}")));
    want.push(format!("{absent} "));
    assert_eq!(resp, want);
    assert_eq!(objects[2].1, "6144");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn negotiation_is_ready_at_the_first_common_commit_and_sends_only_new_objects() {
    let env = start_env().await;
    let src = make_source(&env).await;
    for i in 0..40 {
        write(
            &src.join(format!("hist/f{i}.txt")),
            format!("{i}\n").as_bytes(),
        );
        env.git(&src, &["add", "-A"]).await;
        env.git(&src, &["commit", "-q", "-m", &format!("history {i}")])
            .await;
    }
    env.import(&src, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    let have = env.git(&dest, &["rev-parse", "origin/main"]).await;
    let history: usize = env
        .git(&dest, &["rev-list", "--count", "--all"])
        .await
        .parse()
        .unwrap();
    for n in 1..=3u8 {
        env.api_commit_and_land(
            "r",
            &[(&format!("new{n}.txt"), format!("new {n}\n").as_bytes())],
            &format!("new {n}"),
            n,
        )
        .await;
    }
    let loose = |out: &str| -> u64 {
        out.lines()
            .find_map(|l| l.strip_prefix("count: "))
            .unwrap()
            .parse()
            .unwrap()
    };
    let before = loose(&env.git(&dest, &["count-objects", "-v"]).await);
    let trace = env.path("packet-trace");
    let mut c = env.git_cmd(
        &dest,
        &["-c", "protocol.version=2", "fetch", "-q", "origin"],
    );
    c.env("GIT_TRACE_PACKET", &trace);
    let out = tokio::task::spawn_blocking(move || c.output().expect("git runs"))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = loose(&env.git(&dest, &["count-objects", "-v"]).await);
    // Three commits, three root trees and three blobs: nothing the client already had.
    assert_eq!(after - before, 9, "only new objects are sent");
    let t = std::fs::read_to_string(&trace).unwrap();
    let haves = t.matches("> have ").count() + t.matches("git> have ").count();
    assert!(
        t.contains(&format!("ACK {have}")),
        "the client's tip is common"
    );
    assert!(
        t.contains("< ready") || t.contains("git< ready"),
        "ready after one round"
    );
    assert!(
        haves > 0 && haves < history,
        "negotiation stops early: {haves} haves of {history} commits"
    );
    assert_eq!(
        env.git(
            &dest,
            &["rev-list", "--count", &format!("{have}..origin/main")]
        )
        .await,
        "3"
    );
}
