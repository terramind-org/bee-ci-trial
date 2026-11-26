//! Working-copy scenarios: the real `bee` binary against an in-process `bee-server` in dev
//! mode (SQLite and disk in a temp directory). Every `--json` document is validated against
//! its published schema in `schemas/v1/`.

use crate::support;

use std::path::{Path, PathBuf};

use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use serde_json::Value;
use support::{Bee, Run, validate, validate_error};

struct Env {
    srv: Option<RunningServer>,
    runtime: tokio::runtime::Runtime,
    token: String,
    user: String,
    url: String,
    tmp: tempfile::TempDir,
}

impl Env {
    fn start() -> Env {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        let tmp = tempfile::tempdir().expect("tmp");
        let data = tmp.path().join("server");
        let mut cfg = Config::dev(data, Some("127.0.0.1:0".into()));
        cfg.redaction.purge_interval_secs = 3600;
        let mut opts = ServerOptions::new(cfg);
        opts.dev = true;
        let srv = runtime.block_on(start(opts)).expect("server starts");
        let dev = srv.dev.as_ref().expect("dev bootstrap");
        Env {
            token: dev.token.clone(),
            user: dev.user.clone(),
            url: srv.url.clone(),
            srv: Some(srv),
            runtime,
            tmp,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    /// `bee <args> --json` in `dir`, with the dev endpoint and token.
    fn run_env(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut all: Vec<&str> = args.to_vec();
        all.push("--json");
        let mut cmd = Bee::new(&all)
            .current_dir(dir)
            .env("BEE_ENDPOINT", &self.url)
            .env("BEE_TOKEN", &self.token)
            .env("BEE_AUTHOR", "tester")
            .env("BEE_DATE", "1792281600");
        for (k, v) in env {
            cmd = cmd.env(k, v);
        }
        cmd.run().0
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Run {
        self.run_env(dir, args, &[])
    }

    /// Runs, expects exit 0 and validates the output against the command's schema.
    fn ok(&self, dir: &Path, args: &[&str]) -> Value {
        let run = self.run(dir, args);
        assert_eq!(run.code, 0, "bee {args:?}: {run:?}");
        let doc = run.json();
        validate(&schema_file(args), &doc);
        doc["data"].clone()
    }

    /// Runs, expects `code` and validates the error document; returns its `error`.
    fn fails(&self, dir: &Path, args: &[&str], code: i32) -> Value {
        let run = self.run(dir, args);
        assert_eq!(run.code, code, "bee {args:?}: {run:?}");
        validate_error(&run.json())
    }

    fn clone_wc(&self, name: &str, extra: &[&str]) -> PathBuf {
        let mut args = vec!["clone", "dev/playground", name];
        args.extend(extra);
        let data = self.ok(self.tmp.path(), &args);
        assert_eq!(data["op"], "clone");
        self.path(name)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(srv) = self.srv.take() {
            self.runtime.block_on(srv.shutdown());
        }
    }
}

/// `schemas/v1/cli/<command words>.schema.json` for an argument list.
fn schema_file(args: &[&str]) -> String {
    let groups = ["stack", "sparse", "cr", "auth", "admin", "change"];
    let words: Vec<&str> = if groups.contains(&args[0]) {
        args[..2].to_vec()
    } else {
        args[..1].to_vec()
    };
    format!("schemas/v1/cli/{}.schema.json", words.join("."))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, text).expect("write");
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).expect("read")
}

fn messages(log: &Value) -> Vec<String> {
    log["changesets"]
        .as_array()
        .expect("changesets")
        .iter()
        .map(|c| c["meta"]["message"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[test]
fn as_core_01_clone_edit_commit_land_and_log_shows_it() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    let status = env.ok(&wc, &["status"]);
    assert_eq!(status["clean"], true);
    write(&wc, "src/lib.rs", "fn a() {}\n");
    write(&wc, "README.md", "hello\n");
    let status = env.ok(&wc, &["status"]);
    assert_eq!(status["changes"].as_array().expect("changes").len(), 2);
    let diff = env.ok(&wc, &["diff"]);
    assert_eq!(diff["source"], "working-copy");
    assert!(
        diff["files"][1]["unified"]
            .as_str()
            .expect("u")
            .contains("+fn a() {}")
    );
    let commit = env.ok(
        &wc,
        &["commit", "-m", "add lib", "--runtime", "claude-code/3.2"],
    );
    assert_eq!(commit["stack"].as_array().expect("stack").len(), 1);
    assert_eq!(env.ok(&wc, &["status"])["clean"], true);
    let land = env.ok(&wc, &["land"]);
    assert_eq!(land["land"]["version"], 2);
    assert_eq!(land["land"]["landed"][0]["skipped"], false);
    assert!(land["stack"].as_array().expect("stack").is_empty());
    let log = env.ok(&wc, &["log"]);
    assert_eq!(messages(&log), vec!["add lib", ""]);
    // History over the API: show, blame, cat, server diff.
    let show = env.ok(&wc, &["show"]);
    assert_eq!(
        show["changesets"]["changesets"][0]["meta"]["message"],
        "add lib"
    );
    assert_eq!(
        show["changesets"]["changesets"][0]["meta"]["provenance"]["runtime"],
        "claude-code/3.2"
    );
    assert_eq!(show["files"].as_array().expect("files").len(), 2);
    let blame = env.ok(&wc, &["blame", "src/lib.rs"]);
    assert_eq!(blame["ranges"][0]["lineCount"], 1);
    let cat = env.ok(&wc, &["cat", "src/lib.rs", "--rev", "main"]);
    assert_eq!(cat["content"], "fn a() {}\n");
    let diff = env.ok(&wc, &["diff", "--from", "main~1", "--to", "main"]);
    assert_eq!(diff["source"], "server");
    assert_eq!(diff["files"].as_array().expect("files").len(), 2);
    // Outside a working copy, --repo names the repo.
    let log = env.ok(
        env.tmp.path(),
        &["log", "--repo", "dev/playground", "--rev", "main"],
    );
    assert_eq!(messages(&log).len(), 2);
    let ops = env.ok(&wc, &["oplog"]);
    let names: Vec<&str> = ops["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .map(|o| o["op"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["clone", "commit", "land"]);
    let undo = env.fails(&wc, &["undo"], 1);
    assert_eq!(undo["payload"]["subcode"], "not_undoable");
}

#[test]
fn as_core_03_three_change_stack_amend_the_middle_and_land() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    for (file, msg) in [
        ("one.txt", "one"),
        ("two.txt", "two"),
        ("three.txt", "three"),
    ] {
        write(&wc, file, &format!("{msg}\n"));
        env.ok(&wc, &["commit", "-m", msg]);
    }
    let stack = env.ok(&wc, &["stack", "list"]);
    let ids: Vec<String> = stack["stack"]
        .as_array()
        .expect("stack")
        .iter()
        .map(|c| c["changeId"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(ids.len(), 3);
    env.ok(&wc, &["stack", "edit", "2"]);
    assert!(!wc.join("three.txt").exists());
    write(&wc, "two.txt", "two, amended\n");
    let amend = env.ok(&wc, &["amend", "-m", "two (amended)"]);
    let after: Vec<String> = amend["stack"]
        .as_array()
        .expect("stack")
        .iter()
        .map(|c| c["changeId"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(after, ids, "ChangeIds survive amend and the restack");
    assert_eq!(amend["stack"][1]["head"], true);
    env.ok(&wc, &["goto", "3"]);
    assert_eq!(read(&wc, "two.txt"), "two, amended\n");
    let land = env.ok(&wc, &["land"]);
    assert_eq!(land["land"]["landed"].as_array().expect("landed").len(), 3);
    let log = env.ok(&wc, &["log", "--rev", "main"]);
    assert_eq!(messages(&log), vec!["three", "two (amended)", "one", ""]);
    let cat = env.ok(&wc, &["cat", "two.txt", "--rev", "main"]);
    assert_eq!(cat["content"], "two, amended\n");
}

#[test]
fn as_wc_01_sparse_checkout_change_commit_land() {
    let env = Env::start();
    let full = env.clone_wc("full", &[]);
    write(&full, "services/api/main.rs", "fn main() {}\n");
    write(&full, "services/web/index.ts", "export {};\n");
    write(&full, "docs/guide.md", "# guide\n");
    env.ok(&full, &["commit", "-m", "layout"]);
    env.ok(&full, &["land"]);
    let api = env.clone_wc("api", &["--sparse", "services/api"]);
    assert!(api.join("services/api/main.rs").exists());
    assert!(!api.join("services/web").exists());
    assert!(!api.join("docs").exists());
    let sparse = env.ok(&api, &["sparse", "list"]);
    assert_eq!(sparse["sparse"], serde_json::json!(["services/api"]));
    write(&api, "services/api/main.rs", "fn main() { serve() }\n");
    env.ok(&api, &["commit", "-m", "serve"]);
    env.ok(&api, &["land"]);
    let web = env.ok(&api, &["cat", "services/web/index.ts", "--rev", "main"]);
    assert_eq!(web["content"], "export {};\n");
    let main = env.ok(&api, &["cat", "services/api/main.rs", "--rev", "main"]);
    assert_eq!(main["content"], "fn main() { serve() }\n");
    // Widen and narrow the profile.
    env.ok(&api, &["sparse", "add", "docs"]);
    assert_eq!(read(&api, "docs/guide.md"), "# guide\n");
    env.ok(&api, &["sparse", "remove", "docs"]);
    assert!(!api.join("docs").exists());
}

#[test]
fn as_wc_02_interrupt_a_rebase_midway_recover_and_finish() {
    let env = Env::start();
    let a = env.clone_wc("a", &[]);
    let b = env.clone_wc("b", &[]);
    write(&b, "mine-1.txt", "1\n");
    env.ok(&b, &["commit", "-m", "mine 1"]);
    write(&b, "mine-2.txt", "2\n");
    env.ok(&b, &["commit", "-m", "mine 2"]);
    write(&a, "theirs/x.txt", "x\n");
    write(&a, "theirs/y.txt", "y\n");
    env.ok(&a, &["commit", "-m", "theirs"]);
    env.ok(&a, &["land"]);
    // The sync is killed after writing one file.
    let crashed = env.run_env(&b, &["sync"], &[("BEE_WC_FAULT", "files:1")]);
    assert_ne!(crashed.code, 0, "{crashed:?}");
    assert_eq!(
        validate_error(&crashed.json())["payload"]["subcode"],
        "fault_injected"
    );
    // The next command finishes it.
    let status = env.ok(&b, &["status"]);
    assert_eq!(status["recovered"], "sync");
    assert_eq!(status["clean"], true);
    assert_eq!(read(&b, "theirs/y.txt"), "y\n");
    assert_eq!(status["stack"].as_array().expect("stack").len(), 2);
    let land = env.ok(&b, &["land"]);
    assert_eq!(land["land"]["landed"].as_array().expect("landed").len(), 2);
    let log = env.ok(&b, &["log", "--rev", "main"]);
    assert_eq!(messages(&log), vec!["mine 2", "mine 1", "theirs", ""]);
}

#[test]
fn two_working_copies_racing_to_land_both_land_through_the_coalescer() {
    let env = Env::start();
    let a = env.clone_wc("a", &[]);
    let b = env.clone_wc("b", &[]);
    write(&a, "a.txt", "a\n");
    env.ok(&a, &["commit", "-m", "from a"]);
    write(&b, "b.txt", "b\n");
    env.ok(&b, &["commit", "-m", "from b"]);
    let (ra, rb) = std::thread::scope(|s| {
        let ha = s.spawn(|| env.run(&a, &["land"]));
        let hb = s.spawn(|| env.run(&b, &["land"]));
        (ha.join().expect("a"), hb.join().expect("b"))
    });
    for run in [&ra, &rb] {
        assert_eq!(run.code, 0, "{run:?}");
        validate("schemas/v1/cli/land.schema.json", &run.json());
    }
    let versions: Vec<u64> = [&ra, &rb]
        .iter()
        .map(|r| r.json()["data"]["land"]["version"].as_u64().expect("v"))
        .collect();
    assert!(
        versions.contains(&2) && versions.contains(&3),
        "{versions:?}"
    );
    let log = env.ok(&a, &["log", "--rev", "main"]);
    let mut msgs = messages(&log);
    msgs.sort();
    assert_eq!(msgs, vec!["", "from a", "from b"]);
    // Each working copy syncs to see the other's file.
    env.ok(&a, &["sync"]);
    assert_eq!(read(&a, "b.txt"), "b\n");
}

#[test]
fn conflicting_edit_exits_3_with_markers_then_resolve_and_land() {
    let env = Env::start();
    let a = env.clone_wc("a", &[]);
    write(&a, "f.txt", "one\ntwo\nthree\n");
    env.ok(&a, &["commit", "-m", "base"]);
    env.ok(&a, &["land"]);
    let b = env.clone_wc("b", &[]);
    write(&a, "f.txt", "one\nA\nthree\n");
    env.ok(&a, &["commit", "-m", "edit from a"]);
    env.ok(&a, &["land"]);
    write(&b, "f.txt", "one\nB\nthree\n");
    env.ok(&b, &["commit", "-m", "edit from b"]);
    let error = env.fails(&b, &["land"], 3);
    assert_eq!(error["reason"], "MERGE_CONFLICT");
    assert_eq!(error["payload"]["paths"], serde_json::json!(["f.txt"]));
    assert_eq!(
        error["payload"]["report"]["conflicts"][0]["kind"],
        "content"
    );
    assert_eq!(
        read(&b, "f.txt"),
        "one\n<<<<<<< ours\nA\n||||||| base\ntwo\n=======\nB\n>>>>>>> theirs\nthree\n"
    );
    let status = env.ok(&b, &["status"]);
    assert_eq!(status["conflicts"][0]["resolved"], false);
    assert_eq!(status["rebase"]["kind"], "sync");
    // Markers left: resolve refuses with exit 3.
    env.fails(&b, &["resolve"], 3);
    write(&b, "f.txt", "one\nA and B\nthree\n");
    let resolved = env.ok(&b, &["resolve"]);
    assert!(resolved["rebase"].is_null());
    env.ok(&b, &["land"]);
    let cat = env.ok(&b, &["cat", "f.txt", "--rev", "main"]);
    assert_eq!(cat["content"], "one\nA and B\nthree\n");
    let log = env.ok(&b, &["log", "--rev", "main"]);
    assert_eq!(messages(&log)[0], "edit from b");
}

#[test]
fn dirty_working_copy_refuses_with_exit_4() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    write(&wc, "x.txt", "x\n");
    let error = env.fails(&wc, &["sync"], 4);
    assert_eq!(error["reason"], "INVALID_STATE");
    assert_eq!(error["payload"]["subcode"], "dirty_working_copy");
    assert_eq!(error["payload"]["paths"], serde_json::json!(["x.txt"]));
    env.ok(&wc, &["commit", "-m", "x"]);
    write(&wc, "y.txt", "y\n");
    env.fails(&wc, &["land"], 4);
    // Commit with nothing to commit is EMPTY_CHANGE, exit 1.
    env.ok(&wc, &["commit", "-m", "y"]);
    let empty = env.fails(&wc, &["commit", "-m", "nothing"], 1);
    assert_eq!(empty["reason"], "EMPTY_CHANGE");
    // Undo the last commit: y.txt is uncommitted again.
    let undo = env.ok(&wc, &["undo"]);
    assert_eq!(undo["undid"], "commit");
    let status = env.ok(&wc, &["status"]);
    assert_eq!(status["changes"][0]["path"], "y.txt");
    // Not a working copy: exit 1 with a stable subcode.
    let outside = env.fails(env.tmp.path(), &["status"], 1);
    assert_eq!(outside["payload"]["subcode"], "not_a_working_copy");
}

#[test]
fn policy_denial_exits_5() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{}\"]\nwrite = [\"@humans\", \"@agents\", \"@services\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\
         [[land]]\nref = \"main\"\nrequired_checks = [\"test\"]\n",
        env.user
    );
    write(&wc, ".beehive/policy.toml", &policy);
    env.ok(&wc, &["commit", "-m", "require checks"]);
    env.ok(&wc, &["land"]);
    write(&wc, "x.txt", "x\n");
    env.ok(&wc, &["commit", "-m", "x"]);
    let error = env.fails(&wc, &["land"], 5);
    assert_eq!(error["reason"], "POLICY_DENIED");
    assert_eq!(
        error["payload"]["policyDenial"]["missing"],
        serde_json::json!(["check:test"])
    );
    // The change stays in the local stack.
    let stack = env.ok(&wc, &["stack", "list"]);
    assert_eq!(stack["stack"].as_array().expect("stack").len(), 1);
}

/// `bee redact` (F410-012, F47-020): every version of a path, an object by id and a
/// changeset's metadata, quarantined or purged at once, as the server's redaction list shows.
#[test]
fn redact_paths_objects_and_metadata_from_the_cli() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    write(&wc, "config/prod.env", "KEY=one\n");
    env.ok(&wc, &["commit", "-m", "add config"]);
    env.ok(&wc, &["land"]);
    write(&wc, "config/prod.env", "KEY=two\n");
    write(&wc, "notes.txt", "customer list\n");
    env.ok(&wc, &["commit", "-m", "rotate key, by jane@example.com"]);
    env.ok(&wc, &["land"]);
    // Every version of the path, relative to the current directory, quarantined for an hour.
    let sub = wc.join("config");
    let data = env.ok(
        &sub,
        &[
            "redact",
            "--path",
            "prod.env",
            "--all-versions",
            "--purge-after",
            "1h",
            "--reason",
            "leaked key",
        ],
    );
    let ids: Vec<String> = data["redacted"]
        .as_array()
        .expect("redacted")
        .iter()
        .map(id_of)
        .collect();
    let one = bee_object::blob_id(b"KEY=one\n").to_text();
    let two = bee_object::blob_id(b"KEY=two\n").to_text();
    let mut want = vec![one.clone(), two.clone()];
    want.sort();
    assert_eq!(ids, want);
    assert!(data["effectiveBy"].is_string(), "{data}");
    // The content is gone from reads.
    let cat = env.run(&wc, &["cat", "config/prod.env", "--rev", "main"]);
    assert_ne!(cat.code, 0, "{cat:?}");
    // An object by id, purged now; and the metadata of `main` (a changeset revision).
    let notes = bee_object::blob_id(b"customer list\n").to_text();
    env.ok(
        &wc,
        &[
            "redact",
            "--object",
            &notes,
            "--now",
            "--reason",
            "customer data",
        ],
    );
    let data = env.ok(
        &wc,
        &["redact", "--meta", "main", "--reason", "personal data"],
    );
    let meta = id_of(&data["redacted"][0]);
    assert!(meta.starts_with("m"), "a meta id: {meta}");
    // The redaction list records each with its state.
    let list = env.ok(
        &wc,
        &["admin", "list-redactions", "--repo", "dev/playground"],
    );
    let state = |id: &str| {
        list["redactions"]
            .as_array()
            .expect("redactions")
            .iter()
            .find(|r| id_of(&r["id"]) == id)
            .map(|r| r["state"].as_str().unwrap_or_default().to_string())
            .unwrap_or_else(|| panic!("{id} listed: {list}"))
    };
    assert_eq!(state(&one), "REDACTION_STATE_QUARANTINED");
    assert_eq!(state(&two), "REDACTION_STATE_QUARANTINED");
    assert_eq!(state(&notes), "REDACTION_STATE_PURGED");
    assert_eq!(state(&meta), "REDACTION_STATE_QUARANTINED");
    // Naming nothing is a usage error; --now and --purge-after exclude each other.
    env.fails(&wc, &["redact", "--reason", "nothing"], 2);
    env.fails(
        &wc,
        &[
            "redact",
            "--object",
            &notes,
            "--now",
            "--purge-after",
            "1h",
            "--reason",
            "x",
        ],
        2,
    );
}

fn id_of(v: &Value) -> String {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(v["raw"].as_str().unwrap_or_default())
        .expect("base64 id");
    bee_core::ObjectId::from_bytes(&raw).expect("id").to_text()
}

/// F410-011 against the real server: a clone writes redacted files as the placeholder and
/// status calls them clean; a working copy that already holds a newly redacted file replaces
/// it on its next mutating command when clean, and reports it when it has local edits.
#[test]
fn redacted_files_become_placeholders_in_working_copies() {
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    write(&wc, "secret.txt", "password=1\n");
    write(&wc, "draft.txt", "v1\n");
    env.ok(&wc, &["commit", "-m", "files"]);
    env.ok(&wc, &["land"]);
    let other = env.clone_wc("other", &[]);
    env.ok(
        &wc,
        &[
            "redact",
            "--path",
            "secret.txt",
            "--reason",
            "leaked password",
        ],
    );
    env.ok(&wc, &["redact", "--path", "draft.txt", "--reason", "pii"]);
    // A fresh clone: placeholders, clean.
    let fresh = env.clone_wc("fresh", &[]);
    let placeholder =
        String::from_utf8(bee_wc::checkout::REDACTED_PLACEHOLDER.to_vec()).expect("utf-8");
    assert_eq!(read(&fresh, "secret.txt"), placeholder);
    let status = env.ok(&fresh, &["status"]);
    assert_eq!(status["clean"], true, "{status}");
    assert!(
        status["hidden"]
            .as_array()
            .expect("hidden")
            .iter()
            .any(|h| h["path"] == "secret.txt" && h["reason"] == "redacted"),
        "{status}"
    );
    // An older working copy: one clean file, one edited.
    write(&other, "draft.txt", "v2 with my edits\n");
    write(&other, "mine.txt", "mine\n");
    let commit = env.ok(&other, &["commit", "-m", "mine", "mine.txt"]);
    assert_eq!(
        commit["redacted"],
        serde_json::json!(["secret.txt"]),
        "{commit}"
    );
    assert_eq!(commit["redactedDirty"], serde_json::json!(["draft.txt"]));
    assert_eq!(read(&other, "secret.txt"), placeholder);
    assert_eq!(read(&other, "draft.txt"), "v2 with my edits\n");
    let status = env.ok(&other, &["status"]);
    let changed: Vec<&str> = status["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|c| c["path"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(changed, vec!["draft.txt"], "{status}");
    // Committing the edit replaces the redacted content with new content; landing carries
    // the other redacted entry forward unchanged.
    env.ok(&other, &["commit", "-m", "rewrite the draft"]);
    env.ok(&other, &["land"]);
    let cat = env.ok(&other, &["cat", "draft.txt", "--rev", "main"]);
    assert_eq!(cat["content"], "v2 with my edits\n");
}

/// The "Show your work on a change request" prompt, command by command, against a real server:
/// the brief, the checklist, inline and uploaded evidence (a file over 8 MiB goes through
/// Begin, PUT and Finalize), staleness after a revision, and removal.
#[test]
fn show_your_work_brief_checklist_and_evidence_from_the_cli() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
        .expect("png");
    let env = Env::start();
    let wc = env.clone_wc("wc", &[]);
    let files = env.path("media");
    std::fs::create_dir_all(&files).expect("media dir");
    for name in ["before.png", "after.png", "demo.png"] {
        std::fs::write(files.join(name), &png).expect("write");
    }
    std::fs::write(
        files.join("demo.vtt"),
        "WEBVTT\n\n00:00.000 --> 00:01.000\nPay with a saved card\n",
    )
    .expect("vtt");
    // A WebM over the inline limit: the EBML header with the `webm` doc type, then padding.
    let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x82, 0x84];
    webm.extend_from_slice(b"webm");
    webm.resize(9 << 20, 0);
    std::fs::write(files.join("demo.webm"), &webm).expect("webm");
    let path = |name: &str| files.join(name).to_str().expect("utf8").to_owned();

    write(
        &wc,
        "src/checkout/Payment.tsx",
        "export const Payment = () => null;\n",
    );
    env.ok(
        &wc,
        &[
            "commit",
            "-m",
            "Show the total",
            "--runtime",
            "claude-code/3.2",
            "--model",
            "test-model",
        ],
    );
    let cr = env.ok(
        &wc,
        &[
            "cr",
            "open",
            "--title",
            "Show the total",
            "--outcome",
            "Checkout shows the final price",
            "--highlight",
            "Total with tax on the payment step::src/checkout/Payment.tsx",
        ],
    );
    let id = cr["id"].as_str().expect("id").to_owned();
    assert_eq!(cr["brief"]["outcome"], "Checkout shows the final price");
    let owed = |checklist: &Value| -> Vec<String> {
        checklist["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|i| i["satisfied"] != true)
            .map(|i| i["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    // Opening with a brief printed what a UI change still owes: a picture.
    assert_eq!(owed(&cr["checklist"]), vec!["evidence:ui"], "{cr}");

    let brief = env.ok(
        &wc,
        &[
            "cr",
            "brief",
            &id,
            "--outcome",
            "Checkout shows the final price before paying",
            "--highlight",
            "Show the total with tax::src/checkout/Payment.tsx",
            "--risk",
            "Rounding may differ::one shared rounding function",
            "--rollback",
            "Revert this change",
            "--ask",
            "Show tax?::yes|no",
            "--try-it",
            "https://preview.example.com/checkout",
        ],
    );
    assert_eq!(brief["brief"]["asks"][0]["options"][1], "no");
    let number = cr["number"].as_str().expect("number").to_owned();
    assert_eq!(
        env.ok(&wc, &["cr", "checklist", &format!("#{number}")])["revision"],
        1
    );

    // Inline: a before and after, the hero.
    let ba = env.ok(
        &wc,
        &[
            "cr",
            "evidence",
            "add",
            &number,
            "--before",
            &path("before.png"),
            "--after",
            &path("after.png"),
            "--alt",
            "The payment step",
            "--after-alt",
            "The payment step with the total and tax shown",
            "--title",
            "Payment step",
            "--hero",
        ],
    );
    assert_eq!(ba["spec"]["kind"], "EVIDENCE_KIND_BEFORE_AFTER");
    assert_eq!(ba["state"], "EVIDENCE_STATE_READY");
    assert_eq!(ba["assets"][0]["width"], 1);
    assert!(owed(&env.ok(&wc, &["cr", "checklist", &id])).is_empty());

    // Numbers and a test report need no files.
    let bench = env.ok(
        &wc,
        &[
            "cr",
            "evidence",
            "add",
            &id,
            "--bench",
            "p95 latency:ms:120:85:lower",
            "--bench",
            "bundle size:kB:312:290:lower",
        ],
    );
    assert_eq!(bench["spec"]["benchmark"][1]["unit"], "kB");
    let tests = env.ok(
        &wc,
        &[
            "cr",
            "evidence",
            "add",
            &id,
            "--tests",
            "214:0:3:61000",
            "--summary",
            "All 214 billing tests pass",
        ],
    );
    assert_eq!(tests["spec"]["testReport"]["passed"], 214);

    // Over 8 MiB: Begin, one PUT, Finalize with the SHA-256 of the file.
    let video = env.ok(
        &wc,
        &[
            "cr",
            "evidence",
            "add",
            &id,
            &path("demo.webm"),
            "--captions",
            &path("demo.vtt"),
            "--poster",
            &path("demo.png"),
            "--alt",
            "Poster of the demo",
            "--width",
            "1440",
            "--height",
            "900",
            "--duration-ms",
            "24000",
            "--title",
            "Pay with a saved card",
        ],
    );
    assert_eq!(video["state"], "EVIDENCE_STATE_READY", "{video}");
    assert_eq!(video["spec"]["kind"], "EVIDENCE_KIND_VIDEO");
    assert_eq!(video["assets"][0]["contentType"], "video/webm");
    assert_eq!(video["assets"][0]["sizeBytes"], webm.len().to_string());
    assert_eq!(
        video["assets"][0]["digest"],
        format!("sha256:{:x}", Sha256::digest(&webm))
    );
    assert_eq!(video["assets"].as_array().expect("assets").len(), 3);

    let listed = env.ok(&wc, &["cr", "evidence", "list", &id]);
    assert_eq!(listed["evidence"].as_array().expect("evidence").len(), 4);
    assert_eq!(listed["evidence"][0]["spec"]["hero"], true);
    let hero = env.ok(&wc, &["cr", "evidence", "list", &id, "--hero-only"]);
    assert_eq!(hero["evidence"].as_array().expect("evidence").len(), 1);

    // A second revision makes the first one's evidence stale.
    write(
        &wc,
        "src/checkout/Payment.tsx",
        "export const Payment = () => 'total';\n",
    );
    env.ok(&wc, &["amend", "-m", "Show the total"]);
    let head = env.ok(&wc, &["stack", "list"])["stack"][0]["changeset"]
        .as_str()
        .expect("changeset")
        .to_owned();
    env.ok(
        &wc,
        &[
            "change",
            "revise-change",
            "--repo",
            "dev/playground",
            "--id",
            &id,
            "--head",
            &head,
        ],
    );
    let after = env.ok(&wc, &["cr", "evidence", "list", &id]);
    assert!(
        after["evidence"]
            .as_array()
            .expect("evidence")
            .iter()
            .any(|e| e["stale"] == true),
        "{after}"
    );
    assert_eq!(
        owed(&env.ok(&wc, &["cr", "checklist", &id])),
        vec!["evidence:ui"]
    );

    // Remove one item; a bad request is a §12.2 error with the server's subcode.
    let gone = ba["id"].as_str().expect("id");
    env.ok(&wc, &["cr", "evidence", "rm", gone]);
    let after = env.ok(&wc, &["cr", "evidence", "list", &id]);
    assert_eq!(after["evidence"].as_array().expect("evidence").len(), 3);
    let err = env.fails(
        &wc,
        &[
            "cr",
            "evidence",
            "add",
            &id,
            &path("before.png"),
            "--title",
            "No alt text",
        ],
        1,
    );
    assert_eq!(err["reason"], "INVALID_ARGUMENT");
    assert_eq!(err["payload"]["subcode"], "missing_alt");
}
