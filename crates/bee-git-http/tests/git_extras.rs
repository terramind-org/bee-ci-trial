//! Tests with the real `git` and `git-lfs` CLIs for the gateway's finishing items: annotated tags
//! from imports, LFS objects surviving garbage collection, pushes from shallow clones, thin
//! packs and the pack cache.

#![allow(missing_docs, clippy::result_large_err)]

use crate::common;
use std::process::Command;
use std::sync::Arc;

use common::*;
use serde_json::json;

fn lfs_installed() -> bool {
    Command::new("git")
        .args(["lfs", "version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `(id, ref)` pairs of `refs/tags/*` as `git ls-remote` prints them (peeled lines included).
fn tag_lines(text: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_annotated_tags_are_advertised_with_their_exact_ids() {
    let env = start_env().await;
    let src = make_source(&env).await;
    env.git(&src, &["tag", "-a", "v2", "-m", "release two\n\nnotes"])
        .await;
    env.git(
        &src,
        &["tag", "-a", "-m", "an older release", "old", "HEAD~1"],
    )
    .await;
    // A tag of a tag stays servable as a lightweight tag of its commit.
    env.git(&src, &["tag", "-a", "-m", "outer", "outer", "v2"])
        .await;
    env.import(&src, "r").await;
    let want = env.git(&src, &["ls-remote", ".", "refs/tags/*"]).await;
    let want = tag_lines(&want);
    assert!(
        want.iter().any(|(_, n)| n == "refs/tags/v2^{}"),
        "the source has annotated tags: {want:?}"
    );
    for proto in ["2", "0", "1"] {
        let got = env
            .git(
                env.dir.path(),
                &[
                    "-c",
                    &format!("protocol.version={proto}"),
                    "ls-remote",
                    &env.url("r"),
                    "refs/tags/*",
                ],
            )
            .await;
        let got = tag_lines(&got);
        for name in ["v1", "v2", "old"] {
            let r = format!("refs/tags/{name}");
            let w: Vec<_> = want
                .iter()
                .filter(|(_, n)| n.starts_with(&r) && (n == &r || n == &format!("{r}^{{}}")))
                .collect();
            let g: Vec<_> = got
                .iter()
                .filter(|(_, n)| n.starts_with(&r) && (n == &r || n == &format!("{r}^{{}}")))
                .collect();
            assert_eq!(
                g, w,
                "protocol v{proto}: {name} is advertised like the source"
            );
        }
    }
    // The tag of a tag is a ref to the same commit.
    let outer = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/tags/outer"],
        )
        .await;
    assert!(outer.contains("refs/tags/outer"), "{outer}");
    // A clone receives the tag objects byte for byte and `fsck` is happy.
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    for name in ["v2", "old"] {
        assert_eq!(env.git(&dest, &["cat-file", "-t", name]).await, "tag");
        assert_eq!(
            env.git(&dest, &["rev-parse", name]).await,
            env.git(&src, &["rev-parse", name]).await,
            "{name}: the exact tag id"
        );
        assert_eq!(
            env.git(&dest, &["cat-file", "tag", name]).await,
            env.git(&src, &["cat-file", "tag", name]).await,
            "{name}: the exact tag bytes"
        );
    }
    assert_eq!(env.git(&dest, &["cat-file", "-t", "v1"]).await, "commit");
    env.git(&dest, &["fsck", "--strict"]).await;
    // Fetching one tag by name over v2 works too.
    let one = env.path("one");
    env.git(env.dir.path(), &["init", "-q", one.to_str().unwrap()])
        .await;
    env.git(
        &one,
        &["fetch", "-q", &env.url("r"), "refs/tags/v2:refs/tags/v2"],
    )
    .await;
    assert_eq!(env.git(&one, &["cat-file", "-t", "v2"]).await, "tag");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_objects_survive_garbage_collection() {
    if !lfs_installed() {
        eprintln!("skipping lfs_objects_survive_garbage_collection: git-lfs is not installed");
        return;
    }
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = Arc::new(bee_store::ManualClock::new(start));
    let env = start_env_with(Some(clock.clone())).await;
    imported(&env, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    env.git(&dest, &["lfs", "install", "--local"]).await;
    env.git(&dest, &["lfs", "track", "*.bin"]).await;
    let big: Vec<u8> = (0..100_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
        .collect();
    write(&dest.join("big.bin"), &big);
    env.git(&dest, &["add", ".gitattributes", "big.bin"]).await;
    env.git(&dest, &["commit", "-q", "-m", "an LFS object"])
        .await;
    env.git(&dest, &["push", "-q", "origin", "HEAD:main"]).await;
    // Age everything past every window and collect: the LFS object is named by no tree, only
    // by its `lfs_object` row, and must survive.
    clock.advance(20 * 24 * 3600 * 1000);
    let out = env
        .rpc("AdminService", "RunGc", json!({ "org": "dev" }))
        .await
        .expect("gc");
    assert!(!out["dryRun"].as_bool().unwrap_or(false), "{out}");
    let fresh = env.path("fresh");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), fresh.to_str().unwrap()],
    )
    .await;
    env.git(&fresh, &["lfs", "install", "--local"]).await;
    env.git(&fresh, &["lfs", "pull"]).await;
    assert_eq!(std::fs::read(fresh.join("big.bin")).unwrap(), big);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_from_a_shallow_clone_resolve_against_stored_history() {
    let env = start_env().await;
    imported(&env, "r").await;
    let dest = env.path("shallow");
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    assert!(dest.join(".git/shallow").exists());
    let boundary = env.git(&dest, &["rev-parse", "HEAD"]).await;
    write(&dest.join("new/file.txt"), b"from a shallow clone\n");
    env.git(&dest, &["add", "-A"]).await;
    env.git(&dest, &["commit", "-q", "-m", "shallow work"])
        .await;
    let tip = env.git(&dest, &["rev-parse", "HEAD"]).await;
    // A workspace branch, then main (Land).
    env.git(&dest, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
        .await;
    env.git(&dest, &["push", "-q", "origin", "HEAD:main"]).await;
    let log = env.log_messages("r", "main").await;
    assert!(
        log.first().is_some_and(|m| m.starts_with("shallow work")),
        "{log:?}"
    );
    // A full clone sees the stored history under the pushed commit, with the exact ids.
    let full = env.path("full");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), full.to_str().unwrap()],
    )
    .await;
    assert_eq!(env.git(&full, &["rev-parse", "HEAD"]).await, tip);
    assert_eq!(env.git(&full, &["rev-parse", "HEAD^"]).await, boundary);
    assert!(
        env.git(&full, &["rev-list", "--count", "HEAD"])
            .await
            .parse::<u32>()
            .unwrap()
            > 2,
        "the history below the shallow boundary is the stored one"
    );
    env.git(&full, &["fsck", "--strict"]).await;
    // A clone shallow at a commit the repo lacks is refused with the reason.
    let other = env.path("other");
    env.git(env.dir.path(), &["init", "-q", other.to_str().unwrap()])
        .await;
    write(&other.join("x"), b"x");
    env.git(&other, &["add", "-A"]).await;
    env.git(&other, &["commit", "-q", "-m", "x"]).await;
    let alien = env.git(&other, &["rev-parse", "HEAD"]).await;
    std::fs::write(other.join(".git/shallow"), format!("{alien}\n")).unwrap();
    write(&other.join("y"), b"y");
    env.git(&other, &["add", "-A"]).await;
    env.git(&other, &["commit", "-q", "-m", "y"]).await;
    let err = env
        .git_fails(&other, &["push", &env.url("r"), "HEAD:refs/heads/alien"])
        .await;
    assert!(err.contains("shallow at"), "{err}");
}

fn pkt(line: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", line.len() + 4).into_bytes();
    v.extend_from_slice(line.as_bytes());
    v
}

/// POSTs a protocol v2 `fetch` and returns the `X-Bee-Pack-Cache` header and the body.
fn raw_fetch(env: &Env, dir: &std::path::Path, args: &[String]) -> (String, Vec<u8>) {
    let mut body = pkt("command=fetch\n");
    body.extend(pkt("object-format=sha1\n"));
    body.extend_from_slice(b"0001");
    for a in args {
        body.extend(pkt(&format!("{a}\n")));
    }
    body.extend_from_slice(b"0000");
    let body_path = dir.join("fetch.body");
    std::fs::write(&body_path, &body).unwrap();
    let headers = dir.join("fetch.headers");
    let out = Command::new("curl")
        .args([
            "-sS",
            "-u",
            &format!("git:{}", env.token),
            "-H",
            "Git-Protocol: version=2",
            "-H",
            "Content-Type: application/x-git-upload-pack-request",
            "-D",
            headers.to_str().unwrap(),
            "--data-binary",
            &format!("@{}", body_path.display()),
            &format!("http://{}/dev/r.git/git-upload-pack", env.addr()),
        ])
        .output()
        .expect("curl runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let h = std::fs::read_to_string(&headers)
        .unwrap()
        .to_ascii_lowercase();
    let cache = h
        .lines()
        .find_map(|l| l.strip_prefix("x-bee-pack-cache:"))
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    (cache, out.stdout)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thin_packs_delta_against_client_blobs_and_repeat_clones_hit_the_pack_cache() {
    let env = start_env().await;
    let src = env.path("src");
    std::fs::create_dir_all(&src).unwrap();
    env.git(&src, &["init", "-q"]).await;
    // Incompressible content, so a whole copy is as big as the file and a delta is tiny.
    let mut data: Vec<u8> = Vec::with_capacity(200_000);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while data.len() < 200_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        data.push((x >> 24) as u8);
    }
    write(&src.join("data.bin"), &data);
    write(&src.join("note.txt"), b"hello\n");
    env.git(&src, &["add", "-A"]).await;
    env.git(&src, &["commit", "-q", "-m", "first"]).await;
    env.import(&src, "r").await;
    let dest = env.path("clone");
    env.git(
        env.dir.path(),
        &["clone", "-q", &env.url("r"), dest.to_str().unwrap()],
    )
    .await;
    let old = env.git(&dest, &["rev-parse", "HEAD"]).await;
    // A new commit through the API touches a few bytes in the middle of the big file.
    let mut changed = data.clone();
    for b in &mut changed[100_000..100_040] {
        *b = b.wrapping_add(1);
    }
    env.api_commit_and_land("r", &[("data.bin", &changed)], "tweak the data", 1)
        .await;
    let new = env
        .git(
            env.dir.path(),
            &["ls-remote", &env.url("r"), "refs/heads/main"],
        )
        .await
        .split('\t')
        .next()
        .unwrap()
        .to_string();
    assert_ne!(new, old);
    // The same fetch with and without `thin-pack`.
    let base = vec![
        "ofs-delta".to_string(),
        format!("want {new}"),
        format!("have {old}"),
        "done".to_string(),
    ];
    let mut thin = base.clone();
    thin.insert(0, "thin-pack".to_string());
    let (_, plain_out) = raw_fetch(&env, env.dir.path(), &base);
    let (_, thin_out) = raw_fetch(&env, env.dir.path(), &thin);
    assert!(
        plain_out.len() > 150_000,
        "without thin-pack the changed blob travels whole: {} bytes",
        plain_out.len()
    );
    assert!(
        thin_out.len() < 20_000,
        "with thin-pack it is a delta against the client's copy: {} bytes",
        thin_out.len()
    );
    // The real client completes the thin pack and ends up with exactly the right objects.
    // (`index-pack --fix-thin` appends the missing bases to the stored pack, so the wire is
    // where thinness shows; the trace proves this client asked for a thin pack.)
    let mut c = env.git_cmd(
        &dest,
        &[
            "-c",
            "fetch.unpackLimit=1",
            "-c",
            "transfer.unpackLimit=1",
            "fetch",
            "origin",
        ],
    );
    c.env("GIT_TRACE_PACKET", "1");
    let out = tokio::task::spawn_blocking(move || c.output().expect("git runs"))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        trace.contains("thin-pack"),
        "git asks for thin packs:\n{trace}"
    );
    assert_eq!(env.git(&dest, &["rev-parse", "origin/main"]).await, new);
    env.git(&dest, &["fsck", "--strict"]).await;
    env.git(&dest, &["merge", "-q", "--ff-only", "origin/main"])
        .await;
    assert_eq!(std::fs::read(dest.join("data.bin")).unwrap(), changed);
    // Repeating a request is served from the pack cache.
    let (first, a) = raw_fetch(&env, env.dir.path(), &thin);
    let (second, b) = raw_fetch(&env, env.dir.path(), &thin);
    assert_eq!(
        first, "hit",
        "the earlier identical request filled the cache"
    );
    assert_eq!(second, "hit");
    assert_eq!(a, b, "cached and fresh packs are the same bytes");
    // A clone of an unchanged tip that is not the stock one (no thin-pack): miss, then hit.
    let clone_args = vec![
        "ofs-delta".to_string(),
        format!("want {new}"),
        "done".to_string(),
    ];
    let (c1, p1) = raw_fetch(&env, env.dir.path(), &clone_args);
    let (c2, p2) = raw_fetch(&env, env.dir.path(), &clone_args);
    assert_eq!((c1.as_str(), c2.as_str()), ("miss", "hit"));
    assert_eq!(p1, p2);
    // The stock clone's request was built right after the land (`land.warm_pack`).
    env.warmed().await;
    let stock = vec![
        "thin-pack".to_string(),
        "ofs-delta".to_string(),
        format!("want {new}"),
        "done".to_string(),
    ];
    let (c3, _) = raw_fetch(&env, env.dir.path(), &stock);
    assert_eq!(c3, "hit");
    // Real clones are unaffected by the cache.
    for n in ["c1", "c2"] {
        let d = env.path(n);
        env.git(
            env.dir.path(),
            &["clone", "-q", &env.url("r"), d.to_str().unwrap()],
        )
        .await;
        assert_eq!(env.git(&d, &["rev-parse", "HEAD"]).await, new);
        env.git(&d, &["fsck", "--strict"]).await;
    }
}

async fn lfs_clone(env: &Env, token: &str, name: &str) -> std::path::PathBuf {
    let dest = env.path(name);
    env.git(
        env.dir.path(),
        &[
            "clone",
            "-q",
            &env.url_as(token, "r"),
            dest.to_str().unwrap(),
        ],
    )
    .await;
    env.git(&dest, &["lfs", "install", "--local"]).await;
    env.git(&dest, &["config", "--local", "lfs.locksverify", "true"])
        .await;
    dest
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_locks_work_with_the_real_git_lfs_and_locksverify() {
    if !lfs_installed() {
        eprintln!("skipping lfs_locks_work_with_the_real_git_lfs_and_locksverify: no git-lfs");
        return;
    }
    let env = start_env().await;
    imported(&env, "r").await;
    let other_token = env.agent_token("other").await;
    // A tracks binaries as lockable, commits one and pushes.
    let a = lfs_clone(&env, &env.token, "a").await;
    env.git(&a, &["lfs", "track", "--lockable", "*.bin"]).await;
    write(&a.join("art/logo.bin"), &[7u8; 5000]);
    write(&a.join("art/other.bin"), &[9u8; 5000]);
    env.git(&a, &["add", "-A"]).await;
    env.git(&a, &["commit", "-q", "-m", "art"]).await;
    env.git(&a, &["push", "-q", "origin", "HEAD:main"]).await;
    // A takes a lock; it is listed, and verify calls it ours.
    let out = env.git(&a, &["lfs", "lock", "art/logo.bin"]).await;
    assert!(out.contains("Locked art/logo.bin"), "{out}");
    let listed = env.git(&a, &["lfs", "locks"]).await;
    assert!(listed.contains("art/logo.bin"), "{listed}");
    let verified = env.git(&a, &["lfs", "locks", "--verify"]).await;
    assert!(
        verified
            .lines()
            .any(|l| l.starts_with("O ") && l.contains("art/logo.bin")),
        "ours is marked O: {verified}"
    );
    // B (another principal) sees it as theirs, cannot lock or unlock it.
    let b = lfs_clone(&env, &other_token, "b").await;
    env.git(&b, &["lfs", "pull"]).await;
    let theirs = env.git(&b, &["lfs", "locks", "--verify"]).await;
    assert!(
        theirs.contains("art/logo.bin") && !theirs.lines().any(|l| l.starts_with("O ")),
        "{theirs}"
    );
    let err = env.git_fails(&b, &["lfs", "lock", "art/logo.bin"]).await;
    assert!(err.to_lowercase().contains("lock"), "{err}");
    let err = env.git_fails(&b, &["lfs", "unlock", "art/logo.bin"]).await;
    assert!(
        err.contains("PERMISSION_DENIED") || err.to_lowercase().contains("forbidden"),
        "{err}"
    );
    let err = env
        .git_fails(&b, &["lfs", "unlock", "--force", "art/logo.bin"])
        .await;
    assert!(
        err.contains("force_update") || err.to_lowercase().contains("forbidden"),
        "force unlock needs the permission: {err}"
    );
    // locksverify: B's push that changes the locked file is refused, an unrelated one is not.
    env.git(&b, &["lfs", "track", "*.bin"]).await;
    write(&b.join("art/logo.bin"), &[8u8; 5000]);
    env.git(&b, &["add", "-A"]).await;
    env.git(&b, &["commit", "-q", "-m", "change the locked logo"])
        .await;
    let err = env
        .git_fails(&b, &["push", "origin", "HEAD:refs/heads/ws/logo"])
        .await;
    assert!(
        err.contains("Cannot update locked files"),
        "git-lfs refuses the push over a file locked by someone else: {err}"
    );
    env.git(&b, &["reset", "-q", "--hard", "HEAD~1"]).await;
    write(&b.join("art/free.txt"), b"not locked\n");
    env.git(&b, &["add", "-A"]).await;
    env.git(&b, &["commit", "-q", "-m", "unrelated"]).await;
    env.git(&b, &["push", "-q", "origin", "HEAD:refs/heads/ws/free"])
        .await;
    // The owner unlocks; B can lock it now, and A (an admin) breaks B's lock by force.
    env.git(&a, &["lfs", "unlock", "art/logo.bin"]).await;
    let none = env.git(&a, &["lfs", "locks"]).await;
    assert!(!none.contains("art/logo.bin"), "{none}");
    env.git(&b, &["lfs", "lock", "art/other.bin"]).await;
    let out = env
        .git(&a, &["lfs", "unlock", "--force", "art/other.bin"])
        .await;
    assert!(out.contains("Unlocked"), "{out}");
    assert!(
        !env.git(&b, &["lfs", "locks"])
            .await
            .contains("art/other.bin")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_lock_api_conflicts_paging_and_validation() {
    let env = start_env().await;
    imported(&env, "r").await;
    let base = format!("http://{}/dev/r.git/info/lfs", env.addr());
    let call = |method: &str, path: &str, body: &str| {
        let out = Command::new("curl")
            .args([
                "-sS",
                "-X",
                method,
                "-u",
                &format!("git:{}", env.token),
                "-H",
                "Content-Type: application/vnd.git-lfs+json",
                "-d",
                body,
                "-w",
                "\n%{http_code}",
                &format!("{base}{path}"),
            ])
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (b, code) = text.rsplit_once('\n').unwrap();
        (
            code.trim().parse::<u16>().unwrap(),
            serde_json::from_str::<serde_json::Value>(b).unwrap_or(json!({})),
        )
    };
    let (status, first) = call("POST", "/locks", r#"{"path":"a/one.bin"}"#);
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["lock"]["path"], "a/one.bin");
    assert!(first["lock"]["locked_at"].as_str().unwrap().ends_with('Z'));
    let id = first["lock"]["id"].as_str().unwrap().to_string();
    // The same path again: 409 with the holder.
    let (status, again) = call("POST", "/locks", r#"{"path":"a/one.bin"}"#);
    assert_eq!(status, 409, "{again}");
    assert_eq!(again["lock"]["id"], id);
    // Bad paths and bodies.
    for bad in [
        r#"{"path":"../x"}"#,
        r#"{"path":"/x"}"#,
        r#"{"path":""}"#,
        "nope",
    ] {
        let (status, v) = call("POST", "/locks", bad);
        assert_eq!(status, 400, "{bad}: {v}");
    }
    // Paging: three more locks, pages of two.
    for p in ["b.bin", "c.bin", "d.bin"] {
        let (s, _) = call("POST", "/locks", &format!(r#"{{"path":"{p}"}}"#));
        assert_eq!(s, 201);
    }
    let (_, page1) = call("GET", "/locks?limit=2", "");
    assert_eq!(page1["locks"].as_array().unwrap().len(), 2);
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty());
    let (_, page2) = call("GET", &format!("/locks?limit=2&cursor={cursor}"), "");
    assert_eq!(page2["locks"].as_array().unwrap().len(), 2);
    assert_eq!(page2["next_cursor"], "");
    let paths: Vec<&str> = page1["locks"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["locks"].as_array().unwrap())
        .map(|l| l["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a/one.bin", "b.bin", "c.bin", "d.bin"]);
    let (_, by_path) = call("GET", "/locks?path=c.bin", "");
    assert_eq!(by_path["locks"].as_array().unwrap().len(), 1);
    let (_, by_id) = call("GET", &format!("/locks?id={id}"), "");
    assert_eq!(by_id["locks"][0]["path"], "a/one.bin");
    let (_, v) = call("POST", "/locks/verify", "{}");
    assert_eq!(v["ours"].as_array().unwrap().len(), 4);
    assert!(v["theirs"].as_array().unwrap().is_empty());
    // Unlock, unknown id, and a second unlock.
    let (status, u) = call("POST", &format!("/locks/{id}/unlock"), "{}");
    assert_eq!(status, 200, "{u}");
    assert_eq!(u["lock"]["path"], "a/one.bin");
    let (status, _) = call("POST", &format!("/locks/{id}/unlock"), "{}");
    assert_eq!(status, 404);
    let (status, _) = call("POST", "/locks", r#"{"path":"a/one.bin"}"#);
    assert_eq!(status, 201, "the path is free again");
    // Without credentials: 401 with the LFS challenge.
    let out = Command::new("curl")
        .args(["-sS", "-i", &format!("{base}/locks")])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(text.starts_with("http/1.1 401"), "{text}");
    assert!(text.contains("lfs-authenticate"), "{text}");
}

/// Runs a stock `git clone` of `r` into `name` over protocol `proto` and returns the
/// `X-Bee-Pack-Cache` value of each upload-pack response that planned a pack.
async fn traced_clone(env: &Env, proto: &str, name: &str) -> Vec<String> {
    let dest = env.path(name);
    let mut c = env.git_cmd(
        env.dir.path(),
        &[
            "-c",
            &format!("protocol.version={proto}"),
            "clone",
            "-q",
            &env.url("r"),
            dest.to_str().unwrap(),
        ],
    );
    c.env("GIT_TRACE_CURL", "1")
        .env("GIT_TRACE_CURL_NO_DATA", "1");
    let out = tokio::task::spawn_blocking(move || c.output().expect("git runs"))
        .await
        .unwrap();
    let trace = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{trace}");
    trace
        .lines()
        .filter_map(|l| {
            l.to_ascii_lowercase()
                .split_once("x-bee-pack-cache:")
                .map(|(_, v)| v.trim().to_string())
        })
        .filter(|v| v != "none")
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_land_prebuilds_the_stock_clone_pack_so_the_first_clone_hits_the_cache() {
    let env = start_env().await;
    let src = make_source(&env).await;
    // An annotated tag: a clone wants the tag object's own id.
    env.git(&src, &["tag", "-a", "v2", "-m", "release two"])
        .await;
    env.import(&src, "r").await;
    env.api_commit_and_land("r", &[("new.txt", b"landed\n")], "land one", 1)
        .await;
    env.warmed().await;
    let (hits, misses, bytes) = env.pack_cache();
    assert!(bytes > 0, "the land left a pack in the cache");
    for (proto, name) in [("2", "c2"), ("0", "c0")] {
        assert_eq!(
            traced_clone(&env, proto, name).await,
            vec!["hit".to_string()],
            "protocol v{proto}: the first clone after the land is served from the cache"
        );
        let d = env.path(name);
        env.git(&d, &["fsck", "--strict"]).await;
        assert_eq!(std::fs::read(d.join("new.txt")).unwrap(), b"landed\n");
        assert_eq!(env.git(&d, &["cat-file", "-t", "v2"]).await, "tag");
        assert_eq!(
            env.git(&d, &["rev-parse", "origin/feature"]).await,
            env.git(&src, &["rev-parse", "feature"]).await
        );
    }
    let (hits2, misses2, _) = env.pack_cache();
    assert_eq!(hits2, hits + 2);
    assert_eq!(misses2, misses, "no clone planned its own pack");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_warm_pack_off_a_land_prebuilds_nothing() {
    let env = start_env_cfg(None, |c| c.land.warm_pack = false).await;
    imported(&env, "r").await;
    env.api_commit_and_land("r", &[("new.txt", b"landed\n")], "land one", 1)
        .await;
    env.warmed().await;
    assert_eq!(env.pack_cache(), (0, 0, 0), "nothing was built");
    assert_eq!(traced_clone(&env, "2", "c").await, vec!["miss".to_string()]);
    assert_eq!(env.pack_cache().1, 1);
}
