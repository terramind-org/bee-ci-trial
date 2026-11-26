//! Working-copy behaviour against the in-memory server: status against a full rehash under
//! randomized edits, crash recovery at every fault point, conflict round trips, stacks,
//! undo, sparse profiles and ignore rules.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bee_core::Mode;

use crate::error::ExitClass;
use crate::journal::Faults;
use crate::manifest::{self, Manifest};
use crate::ops::{CommitOptions, CreateOptions, Pick, create};
use crate::remote::Signature;
use crate::report::file_changes;
use crate::scan::{self, Stat, StatCache};
use crate::sparse::Sparse;
use crate::testing::Fake;
use crate::wc::{OpenOptions, WorkingCopy};

fn sig() -> Signature {
    Signature {
        name: "tester".into(),
        time: 1_792_281_600,
        tz_offset_minutes: 0,
    }
}

fn opts(sparse: Sparse) -> CreateOptions {
    CreateOptions {
        endpoint: None,
        repo: "dev/playground".into(),
        target_ref: "main".into(),
        sparse,
        seed: Some([7; 32]),
        adopt: false,
    }
}

fn clone_into(fake: &Fake, dir: &Path) -> WorkingCopy {
    create(dir, &opts(Sparse::full()), fake, &OpenOptions::default())
        .expect("clone")
        .0
}

fn reopen(fake: &Fake, dir: &Path) -> WorkingCopy {
    WorkingCopy::open(dir, fake, &OpenOptions::default()).expect("open")
}

fn commit(wc: &mut WorkingCopy, fake: &Fake, message: &str) -> crate::report::OpReport {
    wc.commit(
        fake,
        &CommitOptions {
            message: message.into(),
            author: sig(),
            allow_empty: false,
            paths: Vec::new(),
            provenance: None,
        },
    )
    .expect("commit")
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    fs::write(p, text).expect("write");
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).expect("read")
}

/// The working tree as a manifest, hashing every file (no cache).
fn full_rehash(wc: &WorkingCopy) -> Manifest {
    scan::scan(
        wc.root(),
        i64::MIN,
        &wc.state().sparse,
        wc.manifest(),
        &wc.state().hidden_set(),
        &StatCache::default(),
    )
    .expect("scan")
    .actual
}

fn seeded_fake() -> Fake {
    let fake = Fake::new();
    fake.push(
        &[
            ("a.txt", Some("one\ntwo\nthree\n")),
            ("src/lib.rs", Some("fn a() {}\n")),
            ("src/util/mod.rs", Some("pub mod x;\n")),
            ("docs/readme.md", Some("# docs\n")),
        ],
        "seed",
    );
    fake
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn status_agrees_with_a_full_rehash_under_randomized_edits() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    assert!(wc.status().expect("status").clean);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let names = [
        "a.txt",
        "src/lib.rs",
        "src/util/mod.rs",
        "docs/readme.md",
        "new/x.txt",
        "b.txt",
    ];
    for round in 0..80 {
        for _ in 0..=rng.below(3) {
            let name = names[rng.below(names.len() as u64) as usize];
            let path = root.join(name);
            match rng.below(5) {
                // Same-size rewrite with the old mtime restored: only ctime (or a re-hash of
                // a racily clean entry) can tell.
                0 if path.is_file() => {
                    let meta = fs::metadata(&path).expect("meta");
                    let old = fs::read(&path).expect("read");
                    let mut new = old.clone();
                    if let Some(b) = new.first_mut() {
                        *b = if *b == b'Z' { b'Y' } else { b'Z' };
                    }
                    fs::write(&path, &new).expect("write");
                    let f = fs::File::options().write(true).open(&path).expect("open");
                    f.set_modified(meta.modified().expect("mtime"))
                        .expect("set mtime");
                }
                1 => write(&root, name, &format!("round {round} {}\n", rng.next())),
                2 if path.is_file() => fs::remove_file(&path).expect("rm"),
                #[cfg(unix)]
                3 if path.is_file() => {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = fs::metadata(&path).expect("meta").permissions().mode();
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode ^ 0o100))
                        .expect("chmod");
                }
                _ => {
                    let to = names[rng.below(names.len() as u64) as usize];
                    if path.is_file() && to != name {
                        let target = root.join(to);
                        fs::create_dir_all(target.parent().expect("p")).expect("mkdir");
                        fs::rename(&path, target).expect("mv");
                    }
                }
            }
        }
        let status = wc.status().expect("status");
        let truth = file_changes(&manifest::diff(wc.manifest(), &full_rehash(&wc)));
        assert_eq!(status.changes, truth, "round {round}");
        // Sometimes commit, so the base moves too.
        if round % 17 == 16 && !status.changes.is_empty() {
            commit(&mut wc, &fake, &format!("round {round}"));
            assert!(wc.status().expect("status").changes.is_empty());
        }
    }
}

#[test]
fn racily_clean_entries_are_rehashed() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let wc = clone_into(&fake, &root);
    let path = root.join("a.txt");
    let meta = fs::metadata(&path).expect("meta");
    let st = Stat::of(&meta);
    // A cache that claims the current stat hashed to something else, taken at a clock no
    // later than the file's mtime: the entry must not be trusted.
    let mut cache = StatCache {
        clock_ns: st.mtime_ns,
        entries: BTreeMap::new(),
    };
    let wrong = crate::manifest::Entry::for_bytes(Mode::File, b"stale");
    cache.entries.insert(b"a.txt".to_vec(), (st, wrong));
    let scanned = scan::scan(
        &root,
        st.mtime_ns + 1,
        &Sparse::full(),
        wc.manifest(),
        &Default::default(),
        &cache,
    )
    .expect("scan");
    assert_eq!(
        scanned.actual[&b"a.txt".to_vec()],
        wc.manifest()[&b"a.txt".to_vec()]
    );
    // With a later clock the (wrong) entry is trusted: that is what the clock rule protects.
    cache.clock_ns = st.mtime_ns + 1;
    let trusted = scan::scan(
        &root,
        st.mtime_ns + 2,
        &Sparse::full(),
        wc.manifest(),
        &Default::default(),
        &cache,
    )
    .expect("scan");
    assert_eq!(trusted.actual[&b"a.txt".to_vec()], wrong);
}

#[test]
fn ignore_rules_hide_untracked_files_only() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, ".gitignore", "*.log\nbuild/\n!keep.log\n");
    write(&root, "x.log", "noise");
    write(&root, "keep.log", "kept");
    write(&root, "build/out.bin", "bin");
    write(&root, "src/.gitignore", "*.tmp\n");
    write(&root, "src/a.tmp", "tmp");
    write(&root, "a.tmp", "not ignored at the root");
    let paths: Vec<String> = wc
        .status()
        .expect("status")
        .changes
        .into_iter()
        .map(|c| c.path)
        .collect();
    assert_eq!(
        paths,
        vec![".gitignore", "a.tmp", "keep.log", "src/.gitignore"]
    );
    // A tracked file is never ignored.
    write(&root, ".gitignore", "*.log\nbuild/\n!keep.log\na.txt\n");
    write(&root, "a.txt", "changed\n");
    let status = wc.status().expect("status");
    assert!(
        status
            .changes
            .iter()
            .any(|c| c.path == "a.txt" && c.status == "modified")
    );
}

/// Runs a sync that crashes at `point`, then reopens and checks the recovery.
fn crash_during_sync(point: &str) {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let wc = clone_into(&fake, &root);
    drop(wc);
    fake.push(
        &[
            ("a.txt", Some("ONE\ntwo\nthree\n")),
            ("src/lib.rs", None),
            ("z/new.txt", Some("new\n")),
            ("docs/readme.md", Some("# docs v2\n")),
        ],
        "upstream",
    );
    let mut crashing = WorkingCopy::open(
        &root,
        &fake,
        &OpenOptions {
            faults: Faults::at(point),
            ..OpenOptions::default()
        },
    )
    .expect("open");
    let e = crashing.sync(&fake).expect_err("crash");
    assert_eq!(e.subcode, "fault_injected", "{point}: {e}");
    drop(crashing);
    let mut wc = reopen(&fake, &root);
    assert_eq!(wc.recovered(), Some("sync"), "{point}");
    let status = wc.status().expect("status");
    assert!(status.clean, "{point}: {:?}", status.changes);
    let (tip, _) = fake.tip();
    assert_eq!(wc.state().head, tip);
    assert_eq!(read(&root, "a.txt"), "ONE\ntwo\nthree\n");
    assert!(!root.join("src/lib.rs").exists());
    assert_eq!(read(&root, "z/new.txt"), "new\n");
    assert_eq!(full_rehash(&wc), fake.tree(&tip));
    let syncs = wc
        .oplog()
        .expect("log")
        .iter()
        .filter(|r| r.op == "sync")
        .count();
    assert_eq!(syncs, 1, "{point}");
}

#[test]
fn sync_recovers_from_a_crash_at_every_fault_point() {
    for point in ["journal", "files:1", "files:2", "manifest", "state"] {
        crash_during_sync(point);
    }
}

#[test]
fn clone_recovers_from_a_crash_and_a_torn_intent_is_ignored() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let e = create(
        &root,
        &opts(Sparse::full()),
        &fake,
        &OpenOptions {
            faults: Faults::at("files:2"),
            ..OpenOptions::default()
        },
    )
    .err()
    .expect("crash");
    assert_eq!(e.subcode, "fault_injected");
    let mut wc = reopen(&fake, &root);
    assert_eq!(wc.recovered(), Some("clone"));
    assert!(wc.status().expect("status").clean);
    assert_eq!(read(&root, "src/util/mod.rs"), "pub mod x;\n");
    drop(wc);
    // A torn intent (never committed) and a stray manifest.next are dropped.
    let journal = root.join(".bee/journal");
    fs::write(journal.join("pending.json"), b"{\"record\":").expect("torn");
    fs::write(journal.join("manifest.next"), b"junk").expect("junk");
    let wc = reopen(&fake, &root);
    assert_eq!(wc.recovered(), None);
    assert!(!journal.join("manifest.next").exists());
    // A crash before the first intent leaves no working copy, and cloning again works.
    drop(wc);
    let again = tmp.path().join("again");
    let e = create(
        &again,
        &opts(Sparse::full()),
        &fake,
        &OpenOptions {
            faults: Faults::at("journal"),
            ..OpenOptions::default()
        },
    )
    .err()
    .expect("crash");
    assert_eq!(e.subcode, "fault_injected");
    let mut wc = reopen(&fake, &again);
    assert_eq!(wc.recovered(), Some("clone"));
    assert!(wc.status().expect("status").clean);
}

#[test]
fn commit_amend_and_undo() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    let empty = wc
        .commit(
            &fake,
            &CommitOptions {
                message: "nothing".into(),
                author: sig(),
                allow_empty: false,
                paths: Vec::new(),
                provenance: None,
            },
        )
        .expect_err("empty");
    assert_eq!(empty.reason, "EMPTY_CHANGE");
    write(&root, "a.txt", "one\nTWO\nthree\n");
    write(&root, "new.txt", "hello\n");
    fs::remove_file(root.join("docs/readme.md")).expect("rm");
    let r = commit(&mut wc, &fake, "first change");
    assert_eq!(r.stack.len(), 1);
    let first = r.changeset.clone().expect("cs");
    let change_id = r.change_id.clone().expect("cid");
    assert!(wc.status().expect("status").clean);
    // Amend keeps the ChangeId; reverting a file to its parent drops it from the delta.
    write(&root, "a.txt", "one\ntwo\nthree\n");
    let amended = wc
        .amend(&fake, Some("first change, amended"), &[], &sig())
        .expect("amend");
    assert_eq!(amended.change_id.as_deref(), Some(change_id.as_str()));
    assert_ne!(amended.changeset.as_deref(), Some(first.as_str()));
    let head = wc.state().head;
    let stored = fake.data.borrow().changesets[&head].clone();
    let paths: Vec<String> = stored
        .delta
        .iter()
        .map(|(p, _)| String::from_utf8_lossy(p).into_owned())
        .collect();
    assert_eq!(paths, vec!["docs/readme.md", "new.txt"]);
    assert_eq!(stored.message, "first change, amended");
    // Undo the amend (soft): the base goes back, the files stay, so the change shows again.
    let u = wc.undo(&fake).expect("undo");
    assert_eq!(u.undid.as_deref(), Some("amend"));
    assert_eq!(wc.state().head.to_text(), first);
    let status = wc.status().expect("status");
    assert_eq!(status.changes.len(), 1, "{:?}", status.changes);
    // Undo the commit too: everything is uncommitted again (a.txt was reverted by hand
    // before the amend) and the stack is empty.
    wc.undo(&fake).expect("undo commit");
    assert!(wc.state().stack.is_empty());
    assert_eq!(wc.status().expect("status").changes.len(), 2);
    // The clone cannot be undone.
    let e = wc.undo(&fake).expect_err("clone");
    assert_eq!(e.subcode, "not_undoable");
}

#[test]
fn dirty_working_copies_refuse_sync_and_land() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "a.txt", "dirty\n");
    let e = wc.sync(&fake).expect_err("dirty");
    assert_eq!(e.exit_class(), ExitClass::Dirty);
    assert_eq!(e.paths, vec!["a.txt"]);
    let e = wc.land(&fake).expect_err("nothing to land");
    assert_eq!(e.subcode, "nothing_to_land");
    commit(&mut wc, &fake, "c");
    write(&root, "b.txt", "more\n");
    let e = wc.land(&fake).expect_err("dirty");
    assert_eq!(e.exit_class(), ExitClass::Dirty);
}

#[test]
fn conflicts_roundtrip_through_markers_and_resolve() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "a.txt", "one\nmine\nthree\n");
    write(&root, "mine.txt", "only mine\n");
    commit(&mut wc, &fake, "my edit");
    fake.push(&[("a.txt", Some("one\ntheirs\nthree\n"))], "their edit");
    let e = wc.sync(&fake).expect_err("conflict");
    assert_eq!(e.exit_class(), ExitClass::Conflicts);
    assert_eq!(e.paths, vec!["a.txt"]);
    assert!(e.report.is_some());
    assert_eq!(
        read(&root, "a.txt"),
        "one\n<<<<<<< ours\ntheirs\n||||||| base\ntwo\n=======\nmine\n>>>>>>> theirs\nthree\n"
    );
    assert_eq!(
        read(&root, "mine.txt"),
        "only mine\n",
        "clean paths are applied"
    );
    let status = wc.status().expect("status");
    assert_eq!(status.conflicts.len(), 1);
    assert!(!status.clean);
    // Other commands refuse while the rebase is open.
    assert_eq!(
        wc.land(&fake).expect_err("rebase").subcode,
        "rebase_in_progress"
    );
    // Markers still there: resolve refuses.
    let e = wc.resolve(&fake, &[], None, &sig()).expect_err("markers");
    assert_eq!(e.exit_class(), ExitClass::Conflicts);
    write(&root, "a.txt", "one\ntheirs and mine\nthree\n");
    let r = wc.resolve(&fake, &[], None, &sig()).expect("resolve");
    assert!(r.rebase.is_none());
    assert_eq!(r.stack.len(), 1);
    assert!(wc.status().expect("status").clean);
    let land = wc.land(&fake).expect("land");
    assert_eq!(land.land.as_ref().expect("land").landed.len(), 1);
    let (tip, _) = fake.tip();
    assert_eq!(
        String::from_utf8(
            fake.data.borrow().blobs[&fake.tree(&tip)[&b"a.txt".to_vec()].id].clone()
        )
        .expect("utf8"),
        "one\ntheirs and mine\nthree\n"
    );
    assert!(wc.state().stack.is_empty());
}

#[test]
fn abort_and_pick_a_side() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "a.txt", "one\nmine\nthree\n");
    commit(&mut wc, &fake, "my edit");
    let before = wc.state().clone();
    fake.push(&[("a.txt", Some("one\ntheirs\nthree\n"))], "their edit");
    wc.sync(&fake).expect_err("conflict");
    write(&root, "a.txt", "half-edited\n");
    wc.abort(&fake).expect("abort");
    assert_eq!(wc.state().head, before.head);
    assert_eq!(wc.state().stack, before.stack);
    assert_eq!(read(&root, "a.txt"), "one\nmine\nthree\n");
    // Again, and take theirs (the moved change's side).
    wc.sync(&fake).expect_err("conflict");
    let r = wc
        .resolve(&fake, &["a.txt".into()], Some(Pick::Theirs), &sig())
        .expect("pick");
    assert!(r.rebase.is_none());
    assert_eq!(read(&root, "a.txt"), "one\nmine\nthree\n");
    // Taking ours empties the change, which is dropped.
    fake.push(&[("a.txt", Some("one\nthird\nthree\n"))], "third edit");
    wc.sync(&fake).expect_err("conflict");
    let r = wc
        .resolve(&fake, &[], Some(Pick::Ours), &sig())
        .expect("ours");
    assert_eq!(r.dropped.len(), 1);
    assert!(wc.state().stack.is_empty());
    assert_eq!(read(&root, "a.txt"), "one\nthird\nthree\n");
}

#[test]
fn stacks_new_edit_squash_split_and_describe() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "one.txt", "1\n");
    commit(&mut wc, &fake, "one");
    write(&root, "two.txt", "2\n");
    write(&root, "two-b.txt", "2b\n");
    commit(&mut wc, &fake, "two");
    write(&root, "three.txt", "3\n");
    commit(&mut wc, &fake, "three");
    assert_eq!(wc.state().stack.len(), 3);
    let ids: Vec<_> = wc.state().stack.iter().map(|e| e.change_id).collect();
    // Edit the middle change and amend it: the top is rebased, ChangeIds survive.
    wc.stack_edit(&fake, "2").expect("edit");
    assert!(!root.join("three.txt").exists());
    write(&root, "two.txt", "2 amended\n");
    let r = wc.amend(&fake, None, &[], &sig()).expect("amend");
    assert_eq!(r.stack.len(), 3);
    assert_eq!(
        wc.state()
            .stack
            .iter()
            .map(|e| e.change_id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(wc.state().head, wc.state().stack[1].changeset);
    let top = wc.state().stack[2].clone();
    assert_eq!(top.parent, wc.state().stack[1].changeset);
    assert_eq!(fake.tree(&top.changeset)[&b"two.txt".to_vec()].size, 10);
    // Describe the top without moving.
    wc.describe(&fake, Some("3"), "three, described", &sig())
        .expect("describe");
    assert_eq!(wc.state().stack[2].message, "three, described");
    assert_eq!(wc.state().head, wc.state().stack[1].changeset);
    // Split two-b out of the middle change.
    let r = wc
        .split(&fake, None, &["two-b.txt".into()], Some("two b"), &sig())
        .expect("split");
    assert_eq!(r.stack.len(), 4);
    assert_eq!(wc.state().stack[1].message, "two b");
    assert_eq!(wc.state().stack[2].change_id, ids[1]);
    assert_eq!(wc.state().head, wc.state().stack[2].changeset);
    // Squash it back.
    wc.squash(&fake, None, None, &sig()).expect("squash");
    assert_eq!(wc.state().stack.len(), 3);
    assert_eq!(wc.state().stack[1].message, "two b");
    // A new empty change on top of head, then fill it with amend.
    wc.goto(&fake, "3", false).expect("goto top");
    assert!(root.join("three.txt").exists());
    wc.stack_new(&fake, "four", &sig()).expect("new");
    write(&root, "four.txt", "4\n");
    wc.amend(&fake, None, &[], &sig()).expect("fill");
    assert_eq!(wc.state().stack.len(), 4);
    // Land everything.
    let land = wc.land(&fake).expect("land");
    assert_eq!(land.land.expect("land").landed.len(), 4);
    let (tip, _) = fake.tip();
    let tree = fake.tree(&tip);
    for p in ["one.txt", "two.txt", "two-b.txt", "three.txt", "four.txt"] {
        assert!(tree.contains_key(p.as_bytes()), "{p}");
    }
}

#[test]
fn goto_keeps_unrelated_edits_and_refuses_overlapping_ones() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "x.txt", "x\n");
    commit(&mut wc, &fake, "x");
    write(&root, "y.txt", "y\n");
    commit(&mut wc, &fake, "y");
    write(&root, "a.txt", "unrelated edit\n");
    wc.goto(&fake, "1", false).expect("goto");
    assert!(!root.join("y.txt").exists());
    assert_eq!(read(&root, "a.txt"), "unrelated edit\n");
    write(&root, "x.txt", "edited x\n");
    let e = wc.goto(&fake, "@base", false).expect_err("overlap");
    assert_eq!(e.exit_class(), ExitClass::Dirty);
    assert_eq!(e.paths, vec!["x.txt"]);
    let e = wc.goto(&fake, "main", false).expect_err("stack");
    assert!(
        e.subcode == "stack_not_empty" || e.exit_class() == ExitClass::Dirty,
        "{e}"
    );
}

#[test]
fn sparse_profiles_add_and_remove_paths() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let (mut wc, _) = create(
        &root,
        &opts(Sparse::from_prefixes(["src"]).expect("sparse")),
        &fake,
        &OpenOptions::default(),
    )
    .expect("clone");
    assert!(root.join("src/lib.rs").exists());
    assert!(!root.join("a.txt").exists());
    assert!(wc.status().expect("status").clean);
    // Files outside the profile are invisible to status.
    write(&root, "outside.txt", "x");
    assert!(wc.status().expect("status").clean);
    let add = wc
        .state()
        .sparse
        .with_added(&Sparse::from_prefixes(["docs"]).expect("s"));
    wc.set_sparse(&fake, add).expect("add");
    assert_eq!(read(&root, "docs/readme.md"), "# docs\n");
    write(&root, "docs/readme.md", "edited\n");
    let narrower = Sparse::from_prefixes(["src"]).expect("s");
    let e = wc.set_sparse(&fake, narrower.clone()).expect_err("dirty");
    assert_eq!(e.exit_class(), ExitClass::Dirty);
    write(&root, "docs/readme.md", "# docs\n");
    wc.set_sparse(&fake, narrower).expect("remove");
    assert!(!root.join("docs").exists());
    // Commit and land inside the profile; paths outside are untouched on the server.
    write(&root, "src/lib.rs", "fn a() { 1 }\n");
    commit(&mut wc, &fake, "sparse edit");
    wc.land(&fake).expect("land");
    let (tip, _) = fake.tip();
    let tree = fake.tree(&tip);
    assert!(tree.contains_key(b"a.txt".as_slice()));
    assert!(tree.contains_key(b"docs/readme.md".as_slice()));
}

#[test]
fn change_ids_are_reproducible_from_the_seed() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let mut ids = Vec::new();
    for name in ["one", "two"] {
        let root: PathBuf = tmp.path().join(name);
        let mut wc = clone_into(&fake, &root);
        write(&root, "f.txt", name);
        ids.push(commit(&mut wc, &fake, name).change_id.expect("cid"));
    }
    assert_eq!(ids[0], ids[1], "same seed and counter, same ChangeId");
}

fn commit_paths(
    wc: &mut WorkingCopy,
    fake: &Fake,
    message: &str,
    paths: &[&str],
) -> crate::report::OpReport {
    wc.commit(
        fake,
        &CommitOptions {
            message: message.into(),
            author: sig(),
            allow_empty: false,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            provenance: None,
        },
    )
    .expect("commit")
}

/// F410-011: redacted files are written as the fixed placeholder, which status treats as
/// clean; a file redacted after checkout is replaced on the next mutating command when it is
/// clean, and reported (left alone) when it holds local changes; an unredaction brings the
/// content back. Status and a full rehash agree throughout.
#[test]
fn redacted_files_are_placeholders_refreshed_by_the_next_mutating_command() {
    use crate::checkout::REDACTED_PLACEHOLDER;
    let fake = seeded_fake();
    fake.set_redacted("# docs\n", true);
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    // Redacted before the clone: the placeholder, clean, listed as hidden.
    assert_eq!(
        fs::read(root.join("docs/readme.md")).expect("placeholder"),
        REDACTED_PLACEHOLDER
    );
    let status = wc.status().expect("status");
    assert!(status.clean, "{status:?}");
    assert!(
        status
            .hidden
            .iter()
            .any(|h| h.path == "docs/readme.md" && h.reason == "redacted"),
        "{status:?}"
    );
    assert_eq!(&full_rehash(&wc), wc.manifest());
    // Redacted after the checkout: one clean file, one with local edits.
    write(&root, "src/lib.rs", "fn b() {}\n");
    fake.set_redacted("one\ntwo\nthree\n", true);
    fake.set_redacted("fn a() {}\n", true);
    // Status never touches the network: nothing changes yet.
    assert_eq!(read(&root, "a.txt"), "one\ntwo\nthree\n");
    write(&root, "new.txt", "n\n");
    let report = commit_paths(&mut wc, &fake, "add new", &["new.txt"]);
    assert_eq!(report.redacted, vec!["a.txt".to_owned()]);
    assert_eq!(report.redacted_dirty, vec!["src/lib.rs".to_owned()]);
    assert_eq!(
        fs::read(root.join("a.txt")).expect("a"),
        REDACTED_PLACEHOLDER
    );
    assert_eq!(read(&root, "src/lib.rs"), "fn b() {}\n", "local edits kept");
    let status = wc.status().expect("status");
    let changed: Vec<String> = status.changes.iter().map(|c| c.path.clone()).collect();
    assert_eq!(changed, vec!["src/lib.rs".to_owned()], "{status:?}");
    // A full rehash agrees that the placeholder is unchanged.
    let rehash = full_rehash(&wc);
    assert_eq!(
        rehash.get(b"a.txt".as_slice()),
        wc.manifest().get(b"a.txt".as_slice())
    );
    // Reopening keeps all of it (the epoch is in the state).
    drop(wc);
    let mut wc = reopen(&fake, &root);
    assert!(wc.state().redaction_epoch >= 3);
    // Unredacted: the next mutating command restores the content.
    fake.set_redacted("one\ntwo\nthree\n", false);
    write(&root, "new2.txt", "m\n");
    let report = commit_paths(&mut wc, &fake, "add new2", &["new2.txt"]);
    assert!(report.redacted.is_empty(), "{report:?}");
    assert_eq!(read(&root, "a.txt"), "one\ntwo\nthree\n");
    assert!(
        !wc.state()
            .hidden
            .iter()
            .any(|h| h.path.0 == b"a.txt".to_vec()),
        "{:?}",
        wc.state().hidden
    );
    let status = wc.status().expect("status");
    let changed: Vec<String> = status.changes.iter().map(|c| c.path.clone()).collect();
    assert_eq!(changed, vec!["src/lib.rs".to_owned()], "{status:?}");
}

/// F46-004: the state is canonical TOML (`state.toml`), and the binary index files are
/// versioned and sealed: an unknown version or a torn file is refused, never misread. A
/// `state.json` an older client left is read and replaced by `state.toml` on open.
#[test]
fn state_is_canonical_toml_and_the_binary_index_is_versioned() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    write(&root, "a.txt", "changed\n");
    commit(&mut wc, &fake, "change");
    let dot = root.join(".bee");
    let text = fs::read_to_string(dot.join("state.toml")).expect("state.toml");
    assert!(text.starts_with("version = 1\n"), "{text}");
    assert!(!dot.join("state.json").exists());
    let state = wc.state().clone();
    assert_eq!(
        crate::state::parse_toml(text.as_bytes()).expect("parse"),
        state
    );
    assert_eq!(
        crate::state::to_toml(&state).expect("toml"),
        text.as_bytes(),
        "canonical"
    );
    // The binary manifest is sealed with a version: a future version and a torn file fail.
    let manifest = fs::read(dot.join("manifest")).expect("manifest");
    assert_eq!(&manifest[..4], b"SWMF");
    let mut future = manifest[..manifest.len() - 32].to_vec();
    future[4..8].copy_from_slice(&2u32.to_le_bytes());
    let sum = bee_object::digest(&future);
    future.extend_from_slice(&sum);
    let e = manifest::decode(&future).expect_err("version 2");
    assert!(e.message.contains("version 2"), "{e:?}");
    let e = manifest::decode(&manifest[..manifest.len() - 1]).expect_err("torn");
    assert!(e.message.contains("checksum"), "{e:?}");
    // An older client's state.json is read and replaced.
    drop(wc);
    fs::write(
        dot.join("state.json"),
        crate::state::to_json(&state).unwrap(),
    )
    .unwrap();
    fs::remove_file(dot.join("state.toml")).unwrap();
    let wc = reopen(&fake, &root);
    assert_eq!(wc.state(), &state);
    assert!(dot.join("state.toml").exists());
    assert!(!dot.join("state.json").exists());
}

/// F46-003: the `.bee/` layout: `config.json`, `state.toml`, the `manifest` and `statcache`
/// index files, `journal/`, `oplog.jsonl`, `objects/`, the advisory `lock` and `conflicts/`,
/// which holds one file per conflicted path while a rebase is stopped and is emptied when it
/// is resolved.
#[test]
fn bee_dir_layout_and_the_conflicts_directory() {
    let fake = seeded_fake();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    let dot = root.join(".bee");
    write(&root, "a.txt", "one\nmine\nthree\n");
    commit(&mut wc, &fake, "my edit");
    for name in [
        "config.json",
        "state.toml",
        "manifest",
        "statcache",
        "journal",
        "oplog.jsonl",
        "lock",
        "conflicts",
    ] {
        assert!(dot.join(name).exists(), "{name}");
    }
    let listing = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read")
            .flatten()
            .map(|e| e.path())
            .collect();
        v.sort();
        v
    };
    assert!(listing(&dot.join("conflicts")).is_empty());
    // The lock is held while the working copy is open: a second opener waits and gives up.
    let e = WorkingCopy::open(
        &root,
        &fake,
        &OpenOptions {
            lock_wait: std::time::Duration::from_millis(50),
            ..OpenOptions::default()
        },
    )
    .err()
    .expect("locked");
    assert!(
        e.message.contains("lock") || e.subcode.contains("lock"),
        "{e:?}"
    );
    // A stopped rebase: one file per conflicted path.
    fake.push(
        &[("a.txt", Some("one\ntheirs\nthree\n")), ("b.txt", None)],
        "their edit",
    );
    wc.sync(&fake).expect_err("conflict");
    let files = listing(&dot.join("conflicts"));
    assert_eq!(files.len(), 1, "{files:?}");
    let text = fs::read_to_string(&files[0]).expect("conflict file");
    assert!(text.contains("path = \"a.txt\""), "{text}");
    assert!(text.contains("kind = \"content\""), "{text}");
    assert!(text.contains("markers = true"), "{text}");
    // Resolved: the directory is empty again.
    write(&root, "a.txt", "one\ntheirs and mine\nthree\n");
    wc.resolve(&fake, &[], None, &sig()).expect("resolve");
    assert!(listing(&dot.join("conflicts")).is_empty());
}

/// F41-014: line endings follow `.gitattributes`. A text path with `eol=crlf` is checked out
/// with CRLF and stored with LF; a `text` path's CRLF edits are committed as LF; a `binary`
/// path is stored byte for byte; status and a full rehash agree that freshly converted files
/// are clean.
#[test]
fn line_endings_follow_gitattributes() {
    let fake = Fake::new();
    fake.push(
        &[
            (
                ".gitattributes",
                Some("*.bat eol=crlf\n*.txt text\n*.bin binary\n"),
            ),
            ("run.bat", Some("echo a\necho b\n")),
            ("notes.txt", Some("one\ntwo\n")),
            ("data.bin", Some("raw\r\nbytes\n")),
        ],
        "seed",
    );
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("wc");
    let mut wc = clone_into(&fake, &root);
    assert_eq!(
        read(&root, "run.bat"),
        "echo a\r\necho b\r\n",
        "CRLF on checkout"
    );
    assert_eq!(read(&root, "notes.txt"), "one\ntwo\n");
    assert_eq!(
        read(&root, "data.bin"),
        "raw\r\nbytes\n",
        "binary as stored"
    );
    let status = wc.status().expect("status");
    assert!(status.clean, "{status:?}");
    assert_eq!(&full_rehash(&wc), wc.manifest(), "a rehash agrees");
    // CRLF edits to text paths are committed as LF; the binary file keeps its bytes.
    write(&root, "notes.txt", "one\r\ntwo\r\nthree\r\n");
    write(&root, "run.bat", "echo a\r\necho c\r\n");
    write(&root, "data.bin", "raw\r\nmore\r\n");
    commit(&mut wc, &fake, "edit");
    let tree = fake.tree(&wc.state().head);
    let stored = |p: &str| {
        String::from_utf8(fake.data.borrow().blobs[&tree[&p.as_bytes().to_vec()].id].clone())
            .expect("utf8")
    };
    assert_eq!(stored("notes.txt"), "one\ntwo\nthree\n");
    assert_eq!(stored("run.bat"), "echo a\necho c\n");
    assert_eq!(stored("data.bin"), "raw\r\nmore\r\n");
    assert!(wc.status().expect("status").clean);
    // Landed, another clone of the new tip writes CRLF again.
    wc.land(&fake).expect("land");
    let other = tmp.path().join("other");
    let wc2 = clone_into(&fake, &other);
    assert_eq!(read(&other, "run.bat"), "echo a\r\necho c\r\n");
    assert_eq!(read(&other, "notes.txt"), "one\ntwo\nthree\n");
    drop(wc2);
}

#[test]
fn a_clone_larger_than_one_fetch_group_writes_every_file() {
    // More files than one fetch-and-write group, in many directories, so groups split
    // directories and the next group downloads while the previous one is written.
    let fake = Fake::new();
    let n = crate::checkout::PIPELINE_FILES * 2 + 37;
    let files: Vec<(String, String)> = (0..n)
        .map(|i| (format!("d{:02}/f{i:05}.txt", i % 50), format!("file {i}\n")))
        .collect();
    let refs: Vec<(&str, Option<&str>)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), Some(c.as_str())))
        .collect();
    fake.push(&refs, "many files");
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("wc");
    let wc = clone_into(&fake, &dir);
    for (p, c) in &files {
        assert_eq!(read(&dir, p), *c, "{p}");
    }
    assert_eq!(full_rehash(&wc), fake.tree(&fake.tip().0));
}
