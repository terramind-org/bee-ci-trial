//! Imports the corpus repositories (`corpus/build.sh`) into the in-memory stores and checks
//! what SPEC §17.4 and §17.6 promise: every commit exports to its original id with its
//! original parents, every reachable git object is reproduced byte for byte, the stores pass
//! fsck, re-imports are incremental, ref renames are recorded and the fidelity report matches
//! its schema.

mod schema;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use bee_core::{BeeError, ObjectId, ObjectKind, Reason, RefName, Result, Ulid};
use bee_git::{
    BeeObjectSource, Exporter, GitObjectSource, GitOid, ObjectFormat, ObjectType, RawObject,
};
use bee_import::report::{category, reason};
use bee_import::{
    ImportOptions, ImportOutcome, ImportReport, ImportTarget, RepoSource, import_pack,
    import_repository, lookup_bee_id, lookup_git_id, lookup_git_tag, parse_ref_list,
};
use bee_object::{Changeset, Meta, Tree};
use bee_store::{
    BlobStore, DEFAULT_DRAFT_RETENTION_DAYS, FsckOptions, ManualClock, MemoryBlobStore,
    MemoryMetaStore, MetaStore, OrgId, OrgRow, RepoId, RepoRow, Writer, fsck,
};

// ----- fixtures ----------------------------------------------------------------------------

fn tmp_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// Builds the corpus once per test process.
fn corpus() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = tmp_root();
        // Leftovers from earlier runs.
        if let Ok(dir) = std::fs::read_dir(&root) {
            for e in dir.flatten() {
                if e.file_name().to_string_lossy().starts_with("bee-import-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let out = root.join(format!("bee-import-corpus-{}", std::process::id()));
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/build.sh");
        let status = Command::new("bash")
            .arg(&script)
            .arg(&out)
            .stdout(Stdio::null())
            .status()
            .expect("corpus/build.sh needs bash and git");
        assert!(status.success(), "corpus/build.sh failed");
        out
    })
}

fn scratch_dir(name: &str) -> PathBuf {
    let d = tmp_root().join(format!("bee-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "@1800000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1800000000 +0000")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_text(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git(dir, args)).trim().to_string()
}

struct Env {
    meta: Arc<MemoryMetaStore>,
    blobs: Arc<MemoryBlobStore>,
    target: ImportTarget,
}

async fn env() -> Env {
    let clock = Arc::new(ManualClock::new(1_000));
    let meta = Arc::new(MemoryMetaStore::new());
    let blobs = Arc::new(MemoryBlobStore::new(clock));
    let org = OrgId(Ulid::from_u128(1));
    let repo = RepoId(Ulid::from_u128(2));
    meta.create_org(OrgRow {
        id: org,
        name: "acme".into(),
        created_at_ms: 1,
    })
    .await
    .unwrap();
    meta.create_repo(RepoRow {
        id: repo,
        org,
        name: "api".into(),
        created_at_ms: 1,
        draft_retention_days: DEFAULT_DRAFT_RETENTION_DAYS,
    })
    .await
    .unwrap();
    let writer = Writer::new(meta.clone(), blobs.clone());
    Env {
        meta,
        blobs,
        target: ImportTarget { writer, org, repo },
    }
}

fn opts() -> ImportOptions {
    let mut o = ImportOptions::new("svc:importer");
    o.verify = true;
    o.at_ms = 5_000;
    // Small batches so every corpus import crosses several flushes.
    o.batch_commits = 3;
    o.batch_bytes = 4096;
    o
}

fn report_schema() -> serde_json::Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1/import-report.schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn assert_schema_valid(report: &ImportReport) {
    let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    let errors = schema::validate(&report_schema(), &value);
    assert!(errors.is_empty(), "report violates its schema: {errors:#?}");
    assert_eq!(&ImportReport::from_json(&report.to_json()).unwrap(), report);
}

async fn assert_fsck_clean(env: &Env) {
    let report = fsck(
        env.meta.as_ref(),
        env.blobs.as_ref(),
        env.target.org,
        &FsckOptions::default(),
    )
    .await
    .unwrap();
    assert!(report.is_clean(), "fsck: {:#?}", report.violations);
}

// ----- an independent round trip over everything stored ----------------------------------

/// Everything the stores hold for the repo, decoded, so a fresh exporter can run on it.
#[derive(Default)]
struct Dump {
    blobs: BTreeMap<ObjectId, Vec<u8>>,
    trees: BTreeMap<ObjectId, Tree>,
    metas: BTreeMap<ObjectId, Meta>,
    changesets: BTreeMap<ObjectId, Changeset>,
}

fn miss(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::MissingObject, id.to_text())
}

impl BeeObjectSource for Dump {
    fn blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        self.blobs.get(id).cloned().ok_or_else(|| miss(id))
    }
    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        self.trees.get(id).cloned().ok_or_else(|| miss(id))
    }
    fn meta(&self, id: &ObjectId) -> Result<Meta> {
        self.metas.get(id).cloned().ok_or_else(|| miss(id))
    }
    fn changeset(&self, id: &ObjectId) -> Result<Changeset> {
        self.changesets.get(id).cloned().ok_or_else(|| miss(id))
    }
}

async fn dump(env: &Env) -> Dump {
    let mut d = Dump::default();
    let mut after = None;
    loop {
        let rows = env
            .meta
            .list_changesets(env.target.repo, after, 500)
            .await
            .unwrap();
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for r in rows {
            d.changesets.insert(r.id, r.changeset);
        }
    }
    let mut after = None;
    loop {
        let rows = env
            .meta
            .list_objects(env.target.org, after, 500)
            .await
            .unwrap();
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        for r in rows {
            let bytes = match r.inline {
                Some(b) => b,
                None => env.blobs.get(env.target.org, &r.id).await.unwrap().unwrap(),
            };
            match r.id.kind() {
                ObjectKind::Blob => {
                    d.blobs.insert(r.id, bytes);
                }
                ObjectKind::Tree => {
                    d.trees.insert(r.id, Tree::decode(&bytes).unwrap());
                }
                ObjectKind::Meta => {
                    d.metas.insert(r.id, Meta::decode(&bytes).unwrap());
                }
                _ => panic!("unexpected object row {}", r.id),
            }
        }
    }
    d
}

/// `git rev-list --all --parents` restricted to the refs the import carried.
fn git_graph(repo: &Path, refs: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut args = vec!["rev-list", "--parents"];
    args.extend(refs.iter().map(String::as_str));
    git_text(repo, &args)
        .lines()
        .map(|l| {
            let mut it = l.split(' ').map(str::to_string);
            let c = it.next().unwrap();
            (c, it.collect())
        })
        .collect()
}

/// Exports everything stored with a fresh exporter and checks every commit id, every parent
/// list, and every reachable git object byte for byte against the source repository.
async fn assert_exact_round_trip(env: &Env, repo: &Path, out: &ImportOutcome) {
    let src = RepoSource::open(repo).unwrap();
    let format = src.format();
    let d = dump(env).await;
    let refs = env.meta.list_refs(env.target.repo, "").await.unwrap();
    let tips: Vec<ObjectId> = refs.iter().filter_map(|r| r.target).collect();
    let mut exp = Exporter::new(format);
    let mut emitted: BTreeMap<GitOid, RawObject> = BTreeMap::new();
    exp.export_history(&d, &tips, &mut |oid, raw| {
        emitted.insert(oid, raw);
        Ok(())
    })
    .unwrap();

    let commits: Vec<String> = out.report.refs.iter().map(|r| r.commit.clone()).collect();
    let graph = git_graph(repo, &commits);
    assert_eq!(
        exp.mapping().commits().count(),
        graph.len(),
        "every reachable commit, and nothing else, is a changeset"
    );
    for (commit, parents) in &graph {
        let oid = format.parse_hex(commit.as_bytes()).unwrap();
        let cs = lookup_bee_id(env.meta.as_ref(), env.target.repo, &oid)
            .await
            .unwrap()
            .expect("a git_mapping row for every commit");
        assert_eq!(
            exp.mapping().git(&cs),
            Some(oid),
            "commit {commit} keeps its id"
        );
        assert_eq!(
            lookup_git_id(env.meta.as_ref(), env.target.repo, &cs)
                .await
                .unwrap(),
            Some(oid)
        );
        let mut want: Vec<GitOid> = Vec::new();
        for p in parents {
            let p = format.parse_hex(p.as_bytes()).unwrap();
            if !want.contains(&p) {
                want.push(p);
            }
        }
        let got: Vec<GitOid> = d.changesets[&cs]
            .parents()
            .iter()
            .map(|p| exp.mapping().git(p).unwrap())
            .collect();
        assert_eq!(got, want, "parents of {commit}");
    }
    // Every reachable commit, tree and blob comes back byte for byte.
    let mut args = vec!["rev-list", "--objects"];
    args.extend(commits.iter().map(String::as_str));
    let listed = git_text(repo, &args);
    let mut checked = 0;
    for line in listed.lines() {
        let oid = format
            .parse_hex(line.split(' ').next().unwrap().as_bytes())
            .unwrap();
        let original = src.git_object(&oid).unwrap();
        let got = emitted
            .get(&oid)
            .unwrap_or_else(|| panic!("{oid} was not exported"));
        assert_eq!(got, &original, "{oid} differs");
        checked += 1;
    }
    assert!(checked > 0);
    // Annotated tags are kept byte for byte.
    for r in &out.report.refs {
        for t in &r.tags {
            let oid = format.parse_hex(t.as_bytes()).unwrap();
            let raw = src.git_object(&oid).unwrap();
            assert_eq!(raw.kind, ObjectType::Tag);
            let rec = lookup_git_tag(env.meta.as_ref(), env.target.repo, &oid)
                .await
                .unwrap();
            assert_eq!(rec, Some(raw.data));
        }
    }
}

fn changeset_count(report: &ImportReport) -> u64 {
    report.category(category::COMMITS).unwrap().imported
}

// ----- tests -------------------------------------------------------------------------------

#[tokio::test]
async fn main_corpus_round_trips_exactly() {
    let repo = corpus().join("main");
    let env = env().await;
    let o = opts();
    let fut = import_repository(&repo, &env.target, &o);
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&fut);
    let out = fut.await.unwrap();
    let r = &out.report;
    assert!(r.complete, "{}", r.to_json());
    assert_eq!(r.summary, "imported 100%");
    assert!(r.verification.performed && r.verification.passed());
    let total = git_text(&repo, &["rev-list", "--all", "--count"]);
    assert_eq!(changeset_count(r).to_string(), total);
    assert_eq!(r.verification.commits_checked, changeset_count(r));
    assert_eq!(r.stats.changesets_written, changeset_count(r));
    assert_eq!(r.default_branch.as_deref(), Some("main"));

    // Renamed refs are recorded and exist under their new names.
    let renamed: BTreeMap<&str, &str> = r
        .renamed_refs
        .iter()
        .map(|x| (x.git_hex.as_str(), x.bee.as_str()))
        .collect();
    let hex = |s: &[u8]| bee_core::hex::encode(s);
    assert_eq!(
        renamed.get(hex(b"refs/heads/feature@x").as_str()),
        Some(&"feature%40x")
    );
    assert_eq!(
        renamed.get(hex(b"refs/heads/c1_abcdef0123").as_str()),
        Some(&"c1%5Fabcdef0123")
    );
    assert_eq!(
        renamed.get(hex(b"refs/heads/caf\xe9").as_str()),
        Some(&"caf%E9")
    );
    assert_eq!(r.renamed_refs.len(), 3);
    for name in [
        "main",
        "feature",
        "feature%40x",
        "caf%E9",
        "odd-tree",
        "orphan",
        "tags/v1.0",
        "tags/v1.0-nested",
        "tags/light-v1",
        "git/notes/commits",
    ] {
        let row = env
            .meta
            .get_ref(env.target.repo, &RefName::new(name).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("ref {name} missing"));
        assert_eq!(row.version, 1);
    }
    let nested = r.refs.iter().find(|x| x.bee == "tags/v1.0-nested").unwrap();
    assert_eq!(nested.tags.len(), 2, "tag of a tag peels through both");
    assert_eq!(r.category(category::TAGS).unwrap().imported, 2);
    assert_eq!(r.category(category::SUBMODULES).unwrap().imported, 1);
    assert_eq!(r.category(category::LFS).unwrap().imported, 1);
    assert_eq!(r.lfs_objects.len(), 1);
    assert_eq!(r.lfs_objects[0].size, 4096);
    assert_eq!(r.category(category::REFS).unwrap().imported, 14);

    assert_exact_round_trip(&env, &repo, &out).await;
    assert_fsck_clean(&env).await;
    assert_schema_valid(r);
}

#[tokio::test]
async fn sha256_corpus_round_trips_exactly() {
    let repo = corpus().join("sha256");
    let env = env().await;
    let out = import_repository(&repo, &env.target, &opts())
        .await
        .unwrap();
    assert_eq!(out.report.object_format, "sha256");
    assert!(out.report.complete, "{}", out.report.to_json());
    assert_eq!(changeset_count(&out.report), 4);
    assert_exact_round_trip(&env, &repo, &out).await;
    assert_fsck_clean(&env).await;
    assert_schema_valid(&out.report);
}

#[tokio::test]
async fn pack_and_ref_list_import_like_the_repository() {
    let dir = corpus();
    let pack = std::fs::read(dir.join("main.pack")).unwrap();
    let refs = parse_ref_list(
        &std::fs::read(dir.join("main.refs")).unwrap(),
        ObjectFormat::Sha1,
    )
    .unwrap();
    let from_pack = env().await;
    let a = import_pack(
        pack,
        ObjectFormat::Sha1,
        &refs,
        None,
        &from_pack.target,
        &opts(),
    )
    .await
    .unwrap();
    assert!(a.report.complete, "{}", a.report.to_json());
    let from_repo = env().await;
    let b = import_repository(&dir.join("main"), &from_repo.target, &opts())
        .await
        .unwrap();
    // Same changesets for the same refs; same report apart from the source label and HEAD.
    assert_eq!(a.report.refs, b.report.refs);
    assert_eq!(a.report.categories, b.report.categories);
    assert_eq!(a.report.stats, b.report.stats);
    assert_eq!(a.mapping, b.mapping);
    assert_exact_round_trip(&from_pack, &dir.join("main"), &a).await;
    assert_fsck_clean(&from_pack).await;
}

#[tokio::test]
async fn reimport_writes_nothing_then_only_new_commits() {
    let mirror = scratch_dir("incremental");
    git(
        corpus(),
        &["clone", "-q", "--mirror", "main", mirror.to_str().unwrap()],
    );
    let env = env().await;
    let first = import_repository(&mirror, &env.target, &opts())
        .await
        .unwrap();
    assert!(first.report.complete);
    let objects_before = env.blobs.len();

    // Unchanged history: nothing is written at all.
    let seq = env.meta.commit_seq();
    let again = import_repository(&mirror, &env.target, &opts())
        .await
        .unwrap();
    assert_eq!(
        env.meta.commit_seq(),
        seq,
        "a no-op re-import writes nothing"
    );
    assert_eq!(env.blobs.len(), objects_before);
    let s = &again.report.stats;
    assert_eq!(
        (
            s.commits_walked,
            s.changesets_written,
            s.objects_written,
            s.refs_written
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(s.records_written, 0);
    assert!(s.commits_already_imported > 0);
    assert!(again.report.complete);
    assert!(again.report.refs.iter().all(|r| r.action == "unchanged"));

    // One new commit on main and a new branch: only that commit is imported.
    let main = git_text(&mirror, &["rev-parse", "main"]);
    let tree = git_text(&mirror, &["rev-parse", "main^{tree}"]);
    let blob = {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"new file\n")
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let listing = git_text(&mirror, &["ls-tree", &tree]);
    let new_tree = {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .arg("mktree")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let input = format!("{listing}\n100644 blob {blob}\tnew.txt\n");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let new = git_text(
        &mirror,
        &["commit-tree", &new_tree, "-p", &main, "-m", "Incremental"],
    );
    git(&mirror, &["update-ref", "refs/heads/main", &new]);
    git(&mirror, &["update-ref", "refs/heads/next", &new]);
    let third = import_repository(&mirror, &env.target, &opts())
        .await
        .unwrap();
    let r = &third.report;
    assert!(r.complete, "{}", r.to_json());
    assert_eq!(r.stats.commits_walked, 1);
    assert_eq!(r.stats.changesets_written, 1);
    assert_eq!(r.verification.commits_checked, 1);
    // A blob, the root tree and a meta.
    assert_eq!(r.stats.objects_written, 3);
    assert_eq!(r.stats.refs_written, 2);
    let main_ref = r.refs.iter().find(|x| x.bee == "main").unwrap();
    assert_eq!((main_ref.action.as_str(), main_ref.version), ("updated", 2));
    let next_ref = r.refs.iter().find(|x| x.bee == "next").unwrap();
    assert_eq!((next_ref.action.as_str(), next_ref.version), ("created", 1));
    assert_eq!(main_ref.commit, new);
    assert_exact_round_trip(&env, &mirror, &third).await;
    assert_fsck_clean(&env).await;
    let _ = std::fs::remove_dir_all(&mirror);
}

#[tokio::test]
async fn edge_corpus_reports_every_drop() {
    let repo = corpus().join("edge");
    let env = env().await;
    let out = import_repository(&repo, &env.target, &opts())
        .await
        .unwrap();
    let r = &out.report;
    assert!(!r.complete);
    assert!(r.summary.starts_with("incomplete:"), "{}", r.summary);
    let commit_reasons: BTreeSet<String> = r
        .category(category::COMMITS)
        .unwrap()
        .drops
        .iter()
        .map(|d| d.reason.clone())
        .collect();
    assert_eq!(
        commit_reasons,
        [
            reason::TOO_MANY_PARENTS,
            reason::PARENT_DROPPED,
            reason::UNSUPPORTED_NAME
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    let commits = r.category(category::COMMITS).unwrap();
    assert_eq!(commits.dropped, 3);
    assert_eq!(commits.dropped as usize, commits.drops.len());
    // Base, the 17 side commits, and the commit reusing the dropped merge's tree (whose
    // objects were first converted for the dropped merge and must still be stored).
    assert_eq!(commits.imported, 19);
    let refs = r.category(category::REFS).unwrap();
    let by_item: BTreeMap<&str, &str> = refs
        .drops
        .iter()
        .map(|d| (d.item.as_str(), d.reason.as_str()))
        .collect();
    assert_eq!(
        by_item.get("refs/heads/too-many-parents"),
        Some(&reason::TARGET_DROPPED)
    );
    assert_eq!(
        by_item.get("refs/heads/reserved-name"),
        Some(&reason::TARGET_DROPPED)
    );
    assert_eq!(
        by_item.get("refs/tags/tree-tag"),
        Some(&reason::TARGET_NOT_COMMIT)
    );
    assert_eq!(refs.imported, 2);
    // The reused tree was stored with its git mapping although it was converted first for
    // the dropped merge.
    let reuse = r.refs.iter().find(|x| x.bee == "reuses-tree").unwrap();
    let cs = ObjectId::parse(&reuse.changeset).unwrap();
    let row = env
        .meta
        .get_changeset(env.target.repo, &cs, bee_store::ReadFrom::Primary)
        .await
        .unwrap()
        .unwrap();
    let root_git = lookup_git_id(env.meta.as_ref(), env.target.repo, row.changeset.root())
        .await
        .unwrap();
    assert_eq!(
        root_git.map(|g| g.to_hex()),
        Some(git_text(&repo, &["rev-parse", "reuses-tree^{tree}"]))
    );
    // The merged/ directory tree and its file are counted although they arrived by repair.
    assert!(r.category(category::TREES).unwrap().imported >= 3);
    // What was carried still round-trips, and the dropped commits left nothing dangling.
    assert!(r.verification.passed(), "{:?}", r.verification);
    assert_fsck_clean(&env).await;
    assert_schema_valid(r);
}

#[tokio::test]
async fn shallow_clones_report_their_boundary() {
    let shallow = scratch_dir("shallow");
    let url = format!("file://{}", corpus().join("main").display());
    git(
        corpus(),
        &[
            "clone",
            "-q",
            "--bare",
            "--depth",
            "2",
            "--branch",
            "feature",
            &url,
            shallow.to_str().unwrap(),
        ],
    );
    let env = env().await;
    let out = import_repository(&shallow, &env.target, &opts())
        .await
        .unwrap();
    let r = &out.report;
    assert!(!r.complete);
    let commits = r.category(category::COMMITS).unwrap();
    assert!(
        commits
            .drops
            .iter()
            .any(|d| d.reason == reason::SHALLOW_BOUNDARY),
        "{}",
        r.to_json()
    );
    assert!(
        commits
            .drops
            .iter()
            .all(|d| d.reason == reason::SHALLOW_BOUNDARY || d.reason == reason::PARENT_DROPPED)
    );
    assert_fsck_clean(&env).await;
    assert_schema_valid(r);
    let _ = std::fs::remove_dir_all(&shallow);
}

#[test]
fn schema_validator_rejects_broken_reports() {
    let schema = report_schema();
    let report = ImportReport::new("x", "sha1", Default::default());
    let mut v: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert!(schema::validate(&schema, &v).is_empty());
    v["object_format"] = "md5".into();
    v["categories"][0]["dropped"] = (-1).into();
    v["extra"] = true.into();
    v.as_object_mut().unwrap().remove("stats");
    let errors = schema::validate(&schema, &v);
    assert_eq!(errors.len(), 4, "{errors:#?}");
}

/// Imports a real public repository and prints timing. Run with
/// `cargo test -p bee-import --test corpus -- --ignored --nocapture`.
/// `BEE_IMPORT_REAL_REPO` names an existing local repository; otherwise
/// `BEE_IMPORT_REAL_URL` (default `https://github.com/sharkdp/hyperfine`) is cloned
/// (full history, bare) into the target directory and deleted afterwards.
#[tokio::test]
#[ignore]
async fn real_public_repository_imports_exactly() {
    let (repo, cloned) = match std::env::var("BEE_IMPORT_REAL_REPO") {
        Ok(p) => (PathBuf::from(p), false),
        Err(_) => {
            let url = std::env::var("BEE_IMPORT_REAL_URL")
                .unwrap_or_else(|_| "https://github.com/sharkdp/hyperfine".to_string());
            let dir = scratch_dir("real");
            let t = Instant::now();
            git(
                &tmp_root(),
                &["clone", "-q", "--bare", &url, dir.to_str().unwrap()],
            );
            eprintln!("cloned {url} in {:.1}s", t.elapsed().as_secs_f64());
            (dir, true)
        }
    };
    let env = env().await;
    let mut o = opts();
    o.batch_commits = 512;
    o.batch_bytes = 8 << 20;
    let t = Instant::now();
    let out = import_repository(&repo, &env.target, &o).await.unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    let r = &out.report;
    eprintln!(
        "imported {} commits, {} trees, {} blobs, {} refs in {elapsed:.1}s (verify included): {}",
        changeset_count(r),
        r.category(category::TREES).unwrap().imported,
        r.category(category::BLOBS).unwrap().imported,
        r.category(category::REFS).unwrap().imported,
        r.summary
    );
    eprintln!(
        "verification: {} commits, {} refs checked, {} id mismatches, {} graph mismatches",
        r.verification.commits_checked,
        r.verification.refs_checked,
        r.verification.commit_id_mismatches.len(),
        r.verification.graph_mismatches.len()
    );
    for d in r.drops().take(20) {
        eprintln!("drop: {} {} {}", d.item, d.reason, d.detail);
    }
    let t = Instant::now();
    let seq = env.meta.commit_seq();
    let again = import_repository(&repo, &env.target, &o).await.unwrap();
    eprintln!(
        "re-import of unchanged history: {:.2}s, {} writes",
        t.elapsed().as_secs_f64(),
        env.meta.commit_seq() - seq
    );
    assert_eq!(env.meta.commit_seq(), seq);
    assert!(again.report.complete);
    assert!(r.verification.passed());
    assert_fsck_clean(&env).await;
    assert_schema_valid(r);
    if cloned {
        let _ = std::fs::remove_dir_all(&repo);
    }
    assert!(r.complete, "{}", r.to_json());
}
