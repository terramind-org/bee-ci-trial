//! Differential tests against the `git` CLI.
//!
//! Each test builds a real repository in a temporary directory — merges (two-parent and
//! octopus), renames, deletes, binary and executable files, symlinks, an empty commit, a
//! latin-1 message with an `encoding` header, a submodule gitlink, annotated and
//! signed-looking tags, plus hand-made objects git's porcelain would not write: a non-UTF-8
//! file name, a `100664` mode, a zero-padded `040000` mode, an unsorted tree, an empty
//! directory, a commit with `gpgsig` and `mergetag` headers, a non-UTF-8 message, and a commit
//! with no message separator. Every object is read back with `git cat-file`, converted to
//! bee and back, and must reproduce its exact bytes and id. Packs are checked against
//! `git index-pack`, `git verify-pack`, `git repack` and `git pack-objects --thin`.
//!
//! When `git` is not on PATH the tests print a skip message and pass.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_git::export::Representation;
use bee_git::{
    BeeObjectSource, DeltaBase, Exporter, GitObject, GitOid, ImportedCommit, Importer,
    ObjectFormat, ObjectType, Pack, PackEntryKind, PackObject, RawObject, TreeNote, write_pack,
};
use bee_object::{Changeset, Meta, Tree};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

macro_rules! require_git {
    () => {
        if !git_available() {
            eprintln!("skipping: `git` is not on PATH; differential git tests did not run");
            return;
        }
    };
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "bee-git-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git_in(dir: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(["-c", "core.autocrlf=false", "-c", "init.defaultBranch=main"])
        .args(["-c", "gc.auto=0", "-c", "core.fsyncMethod=writeout-only"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0530")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_DATE", "1700000100 -0800")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_in(dir, args, b""))
        .unwrap()
        .trim()
        .to_string()
}

fn literal(dir: &Path, kind: &str, body: &[u8]) -> String {
    let out = git_in(
        dir,
        &["hash-object", "-t", kind, "--literally", "-w", "--stdin"],
        body,
    );
    String::from_utf8(out).unwrap().trim().to_string()
}

fn raw_oid(format: ObjectFormat, hex: &str) -> Vec<u8> {
    format
        .parse_hex(hex.as_bytes())
        .unwrap()
        .as_bytes()
        .to_vec()
}

/// Builds the corpus repository; returns (fake submodule id, thin-pack tip, its parent).
fn build_corpus(dir: &Path, format: ObjectFormat) -> (String, String, String) {
    git(
        dir,
        &["init", "-q", &format!("--object-format={}", format.name())],
    );
    let w = |name: &str, data: &[u8]| std::fs::write(dir.join(name), data).unwrap();
    w("a.txt", b"hello\n");
    let bin: Vec<u8> = (0..40).flat_map(|_| 0u8..=255).collect();
    w("bin.dat", &bin);
    w("run.sh", b"#!/bin/sh\necho hi\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
    }
    let big: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    w("big.txt", big.as_bytes());
    std::fs::create_dir_all(dir.join("src/deep/er")).unwrap();
    w("src/deep/er/x.rs", b"fn main() {}\n");
    w("src/a.c", b"int x;\n");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);

    git(dir, &["checkout", "-q", "-b", "side"]);
    w("a.txt", b"hello from side\n");
    git(dir, &["commit", "-q", "-am", "side change"]);
    git(dir, &["checkout", "-q", "main"]);
    w("big.txt", big.replace("line 5\n", "line five\n").as_bytes());
    git(dir, &["commit", "-q", "-am", "main change"]);
    git(dir, &["merge", "-q", "--no-ff", "side", "-m", "merge side"]);

    for b in ["o1", "o2"] {
        git(dir, &["checkout", "-q", "-b", b, "main"]);
        w(&format!("{b}.txt"), b.as_bytes());
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", b]);
    }
    git(dir, &["checkout", "-q", "main"]);
    git(
        dir,
        &["merge", "-q", "--no-ff", "o1", "o2", "-m", "octopus"],
    );

    git(dir, &["mv", "a.txt", "renamed.txt"]);
    git(dir, &["commit", "-q", "-m", "rename"]);
    git(dir, &["rm", "-q", "bin.dat"]);
    git(dir, &["commit", "-q", "-m", "delete binary"]);
    git(
        dir,
        &["commit", "-q", "--allow-empty", "-m", "empty commit"],
    );
    w("latin1.msg", b"caf\xe9 au lait\n");
    git(
        dir,
        &[
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "-q",
            "--allow-empty",
            "-F",
            "latin1.msg",
        ],
    );
    std::fs::remove_file(dir.join("latin1.msg")).unwrap();
    let fake_sub = "5".repeat(format.oid_len() * 2);
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{fake_sub},vendor/lib"),
        ],
    );
    git(dir, &["commit", "-q", "-m", "add submodule"]);
    git(dir, &["tag", "-a", "v1.0", "-m", "release 1.0"]);
    git(dir, &["tag", "light"]);

    // Thin-pack pair: a small change to a large file.
    let parent = git(dir, &["rev-parse", "HEAD"]);
    w("big.txt", format!("{big}appended line\n").as_bytes());
    git(dir, &["add", "big.txt"]);
    git(dir, &["commit", "-q", "-m", "grow big.txt"]);
    let tip = git(dir, &["rev-parse", "HEAD"]);

    // Hand-made objects.
    let head = tip.clone();
    let blob = String::from_utf8(git_in(
        dir,
        &["hash-object", "-w", "--stdin"],
        b"plumbing\n",
    ))
    .unwrap()
    .trim()
    .to_string();
    let empty = literal(dir, "tree", b"");
    let mut mk = Vec::new();
    mk.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
    mk.extend_from_slice(b"\xff\xfeweird name\0");
    mk.extend_from_slice(format!("100664 blob {blob}\tgroup-writable\0").as_bytes());
    mk.extend_from_slice(format!("40000 tree {empty}\tempty-dir\0").as_bytes());
    mk.extend_from_slice(format!("160000 commit {fake_sub}\tsub\0").as_bytes());
    let mktree = String::from_utf8(git_in(dir, &["mktree", "-z", "--missing"], &mk))
        .unwrap()
        .trim()
        .to_string();
    // An empty directory holding an empty directory.
    let mut nested = b"40000 inner\0".to_vec();
    nested.extend_from_slice(&raw_oid(format, &empty));
    let hollow = literal(dir, "tree", &nested);
    // Unsorted entries and a zero-padded directory mode.
    let mut odd = Vec::new();
    for (mode, name, oid) in [
        ("100644", "zz", &blob),
        ("040000", "dir", &mktree),
        ("100644", "aa", &blob),
        ("40000", "hollow", &hollow),
        ("100755", "exec-odd", &blob),
    ] {
        odd.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        odd.extend_from_slice(&raw_oid(format, oid));
    }
    let odd_tree = literal(dir, "tree", &odd);
    let signed = format!(
        "tree {odd_tree}\nparent {head}\nparent {head}\n\
         author Sig Ner <sig@example.com> 1700000200 +0000\n\
         committer Sig Ner <sig@example.com> 1700000201 +0000\n\
         mergetag object {head}\n type commit\n tag merged\n tagger T <t@x> 1 +0000\n \n merged tag\n\
         gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n =abcd\n -----END PGP SIGNATURE-----\n\
         x-unknown-header some value\n\nsigned commit\n"
    );
    let signed = literal(dir, "commit", signed.as_bytes());
    let mut nonutf8 = format!(
        "tree {mktree}\nparent {signed}\nauthor Ümläut <u@x> 1700000300 +0100\n\
         committer bad-date <b@x> notanumber\n\n"
    )
    .into_bytes();
    nonutf8.extend_from_slice(b"bytes \xff\xfe without encoding, no final newline");
    let nonutf8 = literal(dir, "commit", &nonutf8);
    let noseparator = format!("tree {empty}\nparent {nonutf8}\nauthor X <x@x> 1 +0000");
    let noseparator = literal(dir, "commit", noseparator.as_bytes());
    git(dir, &["update-ref", "refs/heads/plumbing", &noseparator]);
    let tag = format!(
        "object {signed}\ntype commit\ntag signed\ntagger T <t@x> 1700000400 +0000\n\n\
         signed tag\n-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----\n"
    );
    let tag = literal(dir, "tag", tag.as_bytes());
    git(dir, &["update-ref", "refs/tags/signed", &tag]);
    (fake_sub, tip, parent)
}

/// Every object in the repository, via `git cat-file --batch`.
fn read_objects(dir: &Path, format: ObjectFormat) -> BTreeMap<GitOid, RawObject> {
    let list = git(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    let ids: String = list.lines().map(|l| format!("{l}\n")).collect();
    let out = git_in(dir, &["cat-file", "--batch"], ids.as_bytes());
    let mut map = BTreeMap::new();
    let mut pos = 0;
    while pos < out.len() {
        let nl = pos + out[pos..].iter().position(|&b| b == b'\n').unwrap();
        let head = std::str::from_utf8(&out[pos..nl]).unwrap();
        let mut parts = head.split(' ');
        let oid = format.parse_hex(parts.next().unwrap().as_bytes()).unwrap();
        let kind = ObjectType::from_name(parts.next().unwrap().as_bytes()).unwrap();
        let size: usize = parts.next().unwrap().parse().unwrap();
        let data = out[nl + 1..nl + 1 + size].to_vec();
        assert_eq!(out[nl + 1 + size], b'\n');
        pos = nl + 2 + size;
        map.insert(oid, RawObject::new(kind, data));
    }
    map
}

#[derive(Default)]
struct BeeStore {
    blobs: BTreeMap<ObjectId, Vec<u8>>,
    trees: BTreeMap<ObjectId, Tree>,
    metas: BTreeMap<ObjectId, Meta>,
    changesets: BTreeMap<ObjectId, Changeset>,
}

impl BeeStore {
    fn add(&mut self, c: ImportedCommit) {
        self.blobs.extend(c.blobs);
        for t in c.trees {
            self.trees.insert(t.id(), t);
        }
        self.metas.insert(c.meta.id(), c.meta);
        self.changesets.insert(c.changeset.id(), c.changeset);
    }
}

fn miss(id: &ObjectId) -> BeeError {
    BeeError::new(Reason::MissingObject, id.to_text())
}

impl BeeObjectSource for BeeStore {
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

fn corpus_round_trip(format: ObjectFormat) {
    let tmp = TempDir::new(&format!("corpus-{}", format.name()));
    let dir = tmp.0.as_path();
    let (fake_sub, _, _) = build_corpus(dir, format);
    let objects = read_objects(dir, format);
    assert!(objects.len() > 40, "corpus has {} objects", objects.len());

    // 1. Every object parses and serializes byte-exactly, and hashes to its id.
    for (oid, raw) in &objects {
        assert_eq!(raw.oid(format).unwrap(), *oid);
        let parsed = GitObject::parse(raw.kind, &raw.data, format).unwrap();
        assert_eq!(parsed.serialize(), raw.data, "{oid}");
        assert_eq!(parsed.to_raw(), *raw);
    }

    // 2. Loose objects git wrote decode to the same objects.
    for (oid, raw) in &objects {
        let hex = oid.to_hex();
        let path = dir.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
        let loose = std::fs::read(&path).unwrap();
        assert_eq!(&RawObject::decode_loose(&loose, 1 << 30).unwrap(), raw);
    }

    // 3. Import every commit, then export from a fresh exporter: identical ids and bytes.
    let commits: Vec<GitOid> = objects
        .iter()
        .filter(|(_, r)| r.kind == ObjectType::Commit)
        .map(|(o, _)| *o)
        .collect();
    let src = |o: &GitOid| {
        objects
            .get(o)
            .cloned()
            .ok_or_else(|| BeeError::new(Reason::MissingObject, o.to_hex()))
    };
    let mut importer = Importer::new(format, "importer");
    let mut store = BeeStore::default();
    let n = importer
        .import_history(&src, &commits, &mut |c| {
            store.add(c);
            Ok(())
        })
        .unwrap();
    assert_eq!(n, commits.len());
    // The corpus exercises every part of the tree note and gitlinks.
    let notes: Vec<TreeNote> = store
        .trees
        .values()
        .map(|t| TreeNote::from_tree(t).unwrap())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.modes.values().any(|m| m == b"100664"))
    );
    assert!(
        notes
            .iter()
            .any(|n| n.modes.values().any(|m| m == b"040000"))
    );
    assert!(
        notes
            .iter()
            .any(|n| n.hidden.iter().any(|h| !h.note.hidden.is_empty()))
    );
    assert!(notes.iter().any(|n| n.order.is_some()));
    assert!(store.trees.values().any(|t| {
        t.entries()
            .iter()
            .any(|e| e.mode == bee_core::Mode::Gitlink && e.name == b"lib")
    }));
    assert!(
        store
            .trees
            .values()
            .any(|t| t.get(b"\xff\xfeweird name").is_some())
    );
    assert!(store.metas.values().any(|m| m.message.contains('\u{fffd}')));
    let tips: Vec<ObjectId> = commits
        .iter()
        .map(|c| importer.mapping().bee(c).unwrap())
        .collect();
    let mut emitted: BTreeMap<GitOid, RawObject> = BTreeMap::new();
    let mut order: Vec<GitOid> = Vec::new();
    let mut exporter = Exporter::new(format);
    let exported = exporter
        .export_history(&store, &tips, &mut |oid, raw| {
            if !emitted.contains_key(&oid) {
                order.push(oid);
            }
            emitted.insert(oid, raw);
            Ok(())
        })
        .unwrap();
    assert_eq!(exported.len(), commits.len());
    for e in &exported {
        assert_eq!(e.representation, Representation::Imported);
        assert_eq!(importer.mapping().bee(&e.oid), Some(e.changeset));
    }
    for (oid, raw) in &emitted {
        assert_eq!(
            objects.get(oid),
            Some(raw),
            "exported {oid} differs from git"
        );
    }
    let fake = format.parse_hex(fake_sub.as_bytes()).unwrap();
    for (oid, raw) in &objects {
        if raw.kind != ObjectType::Tag && *oid != fake {
            assert!(
                emitted.contains_key(oid),
                "git object {oid} was not reproduced"
            );
        }
    }

    // 4. Loose objects we write are readable by git.
    let other = TempDir::new(&format!("loose-{}", format.name()));
    git(
        &other.0,
        &["init", "-q", &format!("--object-format={}", format.name())],
    );
    for (oid, raw) in emitted.iter().take(12) {
        let hex = oid.to_hex();
        let d = other.0.join(".git/objects").join(&hex[..2]);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(&hex[2..]), raw.encode_loose()).unwrap();
        let t = git(&other.0, &["cat-file", "-t", &hex]);
        assert_eq!(t, raw.kind.name());
        assert_eq!(
            git_in(&other.0, &["cat-file", raw.kind.name(), &hex], b""),
            raw.data
        );
    }

    // 5. A pack we write (with ofs and ref deltas) passes index-pack and verify-pack, and
    //    git's index of it is byte-identical to ours.
    // Each blob suggests the earlier blob closest in size (an ofs-delta); the first large blob
    // instead suggests a later one (an in-pack ref-delta), which nothing else bases on.
    let blobs: Vec<(GitOid, usize)> = order
        .iter()
        .filter(|o| emitted[*o].kind == ObjectType::Blob)
        .map(|o| (*o, emitted[o].data.len()))
        .collect();
    let first_big = blobs.iter().position(|b| b.1 > 10_000);
    let closest = |len: usize, cands: &[(GitOid, usize)]| {
        cands
            .iter()
            .filter(|c| first_big.is_none_or(|f| c.0 != blobs[f].0))
            .min_by_key(|c| c.1.abs_diff(len))
            .map(|c| c.0)
    };
    let mut pack_objects = Vec::new();
    for oid in &order {
        let raw = emitted[oid].clone();
        let delta_base = blobs.iter().position(|b| b.0 == *oid).and_then(|i| {
            if Some(i) == first_big {
                closest(blobs[i].1, &blobs[i + 1..])
            } else {
                closest(blobs[i].1, &blobs[..i])
            }
            .map(DeltaBase::InPack)
        });
        pack_objects.push(PackObject {
            object: raw,
            delta_base,
        });
    }
    let written = write_pack(format, &pack_objects).unwrap();
    let parsed = Pack::parse(&written.pack, format).unwrap();
    assert!(
        parsed
            .entries()
            .iter()
            .any(|e| matches!(e.kind, PackEntryKind::OfsDelta { .. })),
        "expected at least one ofs-delta"
    );
    assert!(
        parsed
            .entries()
            .iter()
            .any(|e| matches!(e.kind, PackEntryKind::RefDelta { .. })),
        "expected at least one ref-delta"
    );
    let none = |_: &GitOid| Ok(None);
    for r in parsed.read_all(&none).unwrap() {
        assert_eq!(emitted.get(&r.oid), Some(&r.object));
    }
    let pack_path = other.0.join("ours.pack");
    std::fs::write(&pack_path, &written.pack).unwrap();
    git(&other.0, &["index-pack", "-o", "ours.idx", "ours.pack"]);
    let git_idx = std::fs::read(other.0.join("ours.idx")).unwrap();
    assert_eq!(
        git_idx,
        written.index.write().unwrap(),
        "index differs from git's"
    );
    let verify = git(&other.0, &["verify-pack", "-v", "ours.idx"]);
    assert!(verify.contains(": ok"), "{verify}");
}

#[test]
fn sha1_corpus_round_trips_exactly() {
    require_git!();
    corpus_round_trip(ObjectFormat::Sha1);
}

#[test]
fn sha256_corpus_round_trips_exactly() {
    require_git!();
    corpus_round_trip(ObjectFormat::Sha256);
}

fn git_packs(format: ObjectFormat) {
    let tmp = TempDir::new(&format!("packs-{}", format.name()));
    let dir = tmp.0.as_path();
    let (_, tip, parent) = build_corpus(dir, format);
    let objects = read_objects(dir, format);

    // A full repack with deltas: we read every object and rebuild git's exact index.
    git(dir, &["repack", "-adfq", "--window=50", "--depth=50"]);
    let pack_dir = dir.join(".git/objects/pack");
    let pack_file = std::fs::read_dir(&pack_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let bytes = std::fs::read(&pack_file).unwrap();
    let pack = Pack::parse(&bytes, format).unwrap();
    assert!(
        pack.entries()
            .iter()
            .any(|e| !matches!(e.kind, PackEntryKind::Base(_))),
        "git's repack produced no deltas"
    );
    let none = |_: &GitOid| Ok(None);
    let all = pack.read_all(&none).unwrap();
    assert_eq!(all.len(), objects.len());
    for r in &all {
        assert_eq!(objects.get(&r.oid), Some(&r.object));
    }
    let idx = std::fs::read(pack_file.with_extension("idx")).unwrap();
    assert_eq!(pack.index(&none).unwrap().write().unwrap(), idx);
    let parsed_idx = bee_git::PackIndex::parse(&idx, format).unwrap();
    assert_eq!(parsed_idx.entries().len(), objects.len());

    // A thin pack: ref-deltas against objects the receiver already has.
    let revs = format!("{tip}\n^{parent}\n");
    let thin = git_in(
        dir,
        &["pack-objects", "--stdout", "--thin", "--revs", "-q"],
        revs.as_bytes(),
    );
    let pack = Pack::parse(&thin, format).unwrap();
    let external_refs = pack
        .entries()
        .iter()
        .filter(|e| match e.kind {
            PackEntryKind::RefDelta { base } => objects.contains_key(&base),
            _ => false,
        })
        .count();
    assert!(external_refs > 0, "git produced no thin deltas");
    let err = pack.read_all(&none).unwrap_err();
    assert_eq!(err.reason, Reason::MissingObject);
    let from_repo = |o: &GitOid| Ok(objects.get(o).cloned());
    let got = pack.read_all(&from_repo).unwrap();
    assert_eq!(got.len(), pack.entries().len());
    for r in &got {
        assert_eq!(objects.get(&r.oid), Some(&r.object));
    }
}

#[test]
fn sha1_git_packs_read_exactly() {
    require_git!();
    git_packs(ObjectFormat::Sha1);
}

#[test]
fn sha256_git_packs_read_exactly() {
    require_git!();
    git_packs(ObjectFormat::Sha256);
}

#[test]
fn new_history_commits_are_ordinary_git_commits() {
    require_git!();
    use bee_core::{ChangeId, Mode, RepoPath};
    use bee_object::{DeltaEntry, MemoryTrees, Timestamp, blob_id, build_changeset};

    for format in ObjectFormat::ALL {
        // A two-changeset bee history built natively (no git extension).
        let mut store = BeeStore::default();
        let trees = MemoryTrees::new();
        let f = b"fn main() {}\n".to_vec();
        store.blobs.insert(blob_id(&f), f.clone());
        let meta1 = Meta::new(
            "agent:claude <claude@agents.example>",
            Timestamp::new(1_700_000_000, 60),
            "p1",
            "Add main\n\nFirst change.\n",
        );
        let (c1, t1) = build_changeset(
            &trees,
            vec![],
            None,
            meta1.id(),
            ChangeId::new([1; 16]),
            vec![DeltaEntry::put(
                RepoPath::new("src/main.rs").unwrap(),
                Mode::File,
                blob_id(&f),
                f.len() as u64,
            )],
        )
        .unwrap();
        store.trees.extend(t1);
        store.metas.insert(meta1.id(), meta1);
        let c1_id = c1.id();
        let root1 = *c1.root();
        store.changesets.insert(c1_id, c1);
        let mut mt = MemoryTrees::new();
        for t in store.trees.values() {
            mt.insert(t.clone());
        }
        let meta2 = Meta::new("Weird <Name>\n <w@x>", Timestamp::new(-3, -300), "p2", "");
        let (c2, t2) = build_changeset(
            &mt,
            vec![c1_id],
            Some(&root1),
            meta2.id(),
            ChangeId::new([2; 16]),
            vec![DeltaEntry::put(
                RepoPath::new("run").unwrap(),
                Mode::Executable,
                blob_id(&f),
                f.len() as u64,
            )],
        )
        .unwrap();
        store.trees.extend(t2);
        store.metas.insert(meta2.id(), meta2);
        let c2_id = c2.id();
        store.changesets.insert(c2_id, c2);

        let mut emitted = Vec::new();
        let mut exporter = Exporter::new(format);
        let out = exporter
            .export_history(&store, &[c2_id], &mut |oid, raw| {
                emitted.push((oid, raw));
                Ok(())
            })
            .unwrap();
        assert!(
            out.iter()
                .all(|e| e.representation == Representation::NewHistory)
        );
        // Exporting again from scratch gives the same ids (deterministic).
        let again = Exporter::new(format)
            .export_history(&store, &[c2_id], &mut |_, _| Ok(()))
            .unwrap();
        assert_eq!(again, out);

        // git accepts the objects, fsck is clean, and the log shows the trailer.
        let tmp = TempDir::new(&format!("newhist-{}", format.name()));
        let dir = tmp.0.as_path();
        git(
            dir,
            &["init", "-q", &format!("--object-format={}", format.name())],
        );
        let written = write_pack(
            format,
            &emitted
                .iter()
                .map(|(_, r)| PackObject::full(r.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        git_in(dir, &["index-pack", "--stdin", "--strict"], &written.pack);
        let tip = out.last().unwrap().oid.to_hex();
        git(dir, &["update-ref", "refs/heads/main", &tip]);
        git(dir, &["fsck", "--strict", "--no-dangling"]);
        let log = git(dir, &["log", "--format=%an|%ae|%B", "main"]);
        assert!(
            log.contains("Change-Id: k_01010101010101010101010101010101"),
            "{log}"
        );
        assert!(
            log.contains("Change-Id: k_02020202020202020202020202020202"),
            "{log}"
        );
        assert!(
            log.contains("agent:claude|claude@agents.example|Add main"),
            "{log}"
        );
        assert!(log.contains("Weird Name|w@x|"), "{log}");
        let files = git(
            dir,
            &["ls-tree", "-r", "--format=%(objectmode) %(path)", "main"],
        );
        assert_eq!(files, "100755 run\n100644 src/main.rs");
        // Re-importing git's view of a new-history commit yields its ChangeId trailer.
        let body = git_in(dir, &["cat-file", "commit", &tip], b"");
        let commit = bee_git::GitCommit::parse(&body, format).unwrap();
        assert_eq!(
            bee_git::export::parse_change_id_trailer(commit.message().unwrap()),
            Some(ChangeId::new([2; 16]))
        );
    }
}
