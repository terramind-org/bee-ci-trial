//! Golden vectors (SPEC §4, §14): `testdata/vectors/*.json`.
//!
//! Every vector is also built here from Rust values. The test checks, for each accepted
//! vector, that the code's encoding and id equal the frozen hex and id, that decoding the hex
//! gives the same value, and that the human-readable `value` matches; for each rejected
//! vector, that decoding fails with the recorded reason and subcode.
//!
//! The files are frozen: never edit one. To add vectors, add a new file name to `files()`
//! and run `BEE_WRITE_VECTORS=1 cargo test -p bee-object --test vectors`, which writes
//! only files that do not exist yet.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bee_core::{
    BeeError, ChangeId, Limits, Mode, ObjectId, ObjectKind, Reason, RepoPath, Subcode, hex,
};
use bee_object::{
    Changeset, Cost, DeltaEntry, Extensions, LocalCheck, Meta, Object, Provenance, Timestamp, Tree,
    TreeEntry, Value, blob_id, import_change_id,
};
use serde_json::{Value as Json, json};

enum Expect {
    Accept(Object),
    Reject {
        bytes: Vec<u8>,
        reason: Reason,
        subcode: Option<Subcode>,
    },
}

struct Case {
    name: &'static str,
    description: &'static str,
    kind: ObjectKind,
    expect: Expect,
}

fn accept(name: &'static str, description: &'static str, obj: Object) -> Case {
    Case {
        name,
        description,
        kind: obj.kind(),
        expect: Expect::Accept(obj),
    }
}

fn reject(
    name: &'static str,
    description: &'static str,
    kind: ObjectKind,
    bytes: Vec<u8>,
    subcode: Subcode,
) -> Case {
    Case {
        name,
        description,
        kind,
        expect: Expect::Reject {
            bytes,
            reason: Reason::InvalidArgument,
            subcode: Some(subcode),
        },
    }
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn file(name: &[u8], content: &[u8]) -> TreeEntry {
    TreeEntry::blob(name, Mode::File, blob_id(content), content.len() as u64)
}

fn put(path: &str, content: &[u8]) -> DeltaEntry {
    DeltaEntry::put(p(path), Mode::File, blob_id(content), content.len() as u64)
}

fn fixed(kind: ObjectKind, seed: u8) -> ObjectId {
    let mut d = [0u8; 32];
    for (i, b) in d.iter_mut().enumerate() {
        *b = seed ^ (i as u8).wrapping_mul(7);
    }
    ObjectId::new(kind, d)
}

fn map_of(bytes: &[u8]) -> BTreeMap<u64, Value> {
    match Value::decode(bytes).unwrap() {
        Value::Map(m) => m,
        _ => unreachable!(),
    }
}

fn enc(m: BTreeMap<u64, Value>) -> Vec<u8> {
    Value::Map(m).encode()
}

fn idv(id: &ObjectId) -> Value {
    Value::Bytes(id.as_bytes().to_vec())
}

fn entry_value(name: &[u8], mode: u64, id: &ObjectId, size: u64) -> Value {
    Value::Map(BTreeMap::from([
        (1, Value::Bytes(name.to_vec())),
        (2, Value::Uint(mode)),
        (3, idv(id)),
        (4, Value::Uint(size)),
    ]))
}

fn tree_bytes(entries: Vec<Value>) -> Vec<u8> {
    enc(BTreeMap::from([
        (0, Value::Uint(1)),
        (1, Value::Array(entries)),
    ]))
}

fn sample_tree() -> Tree {
    Tree::new(vec![file(b"a", b"alpha\n"), file(b"b", b"beta\n")]).unwrap()
}

fn full_meta() -> Meta {
    let mut m = Meta::new(
        "Ada Lovelace <ada@example.com>",
        Timestamp::new(1_760_000_000, 120),
        "01JA2B3C4D5E6F7G8H9J0KMNPQ",
        "Fix the parser\n\nHandles empty input.\n",
    );
    m.provenance = Some(Provenance {
        principal_chain: vec![
            "01JAGENT00000000000000000A".into(),
            "01JHUMAN00000000000000000H".into(),
        ],
        runtime: Some("claude-code/3.2".into()),
        model: Some("model-2026-01".into()),
        session: Some("sess-42".into()),
        prompt_digest: Some(bee_object::digest(b"fix the parser")),
        checks_run: vec![
            LocalCheck {
                name: "cargo test".into(),
                result: "pass".into(),
                duration_ms: 48_210,
            },
            LocalCheck {
                name: "clippy".into(),
                result: "pass".into(),
                duration_ms: 9_001,
            },
        ],
        cost: Some(Cost {
            input_tokens: Some(120_000),
            output_tokens: Some(4_200),
            usd_micros: None,
        }),
    });
    m
}

fn files() -> Vec<(&'static str, Vec<Case>)> {
    vec![
        ("blob-v1.json", blobs()),
        ("tree-v1.json", trees()),
        ("meta-v1.json", metas()),
        ("changeset-v1.json", changesets()),
        ("reject-v1.json", rejects()),
    ]
}

fn blobs() -> Vec<Case> {
    vec![
        accept("blob-empty", "The empty blob.", Object::Blob(vec![])),
        accept(
            "blob-text",
            "A short text file.",
            Object::Blob(b"hello, swarm\n".to_vec()),
        ),
        accept(
            "blob-binary",
            "Every byte value once, 0x00 to 0xff.",
            Object::Blob((0..=255).collect()),
        ),
    ]
}

fn trees() -> Vec<Case> {
    let sub = Tree::new(vec![file(b"main.rs", b"fn main() {}\n")]).unwrap();
    let mut ext = Extensions::new();
    ext.insert(64, Value::Text("ignorable".into()));
    ext.insert(1000, Value::Array(vec![Value::Uint(1), Value::int(-1)]));
    vec![
        accept(
            "tree-empty",
            "The empty tree; only valid as a root.",
            Object::Tree(Tree::empty()),
        ),
        accept(
            "tree-one-file",
            "One regular file.",
            Object::Tree(Tree::new(vec![file(b"README.md", b"# swarm\n")]).unwrap()),
        ),
        accept(
            "tree-all-modes",
            "One entry of each mode: file, executable, symlink, tree, gitlink (SHA-1 and SHA-256).",
            Object::Tree(
                Tree::from_unsorted(vec![
                    file(b"file.txt", b"text\n"),
                    TreeEntry::blob("run.sh", Mode::Executable, blob_id(b"#!/bin/sh\n"), 10),
                    TreeEntry::blob("link", Mode::Symlink, blob_id(b"file.txt"), 8),
                    TreeEntry::tree("src", sub.id()),
                    TreeEntry::gitlink("vendor-sha1", ObjectId::git_foreign(&[0xab; 20]).unwrap()),
                    TreeEntry::gitlink(
                        "vendor-sha256",
                        ObjectId::git_foreign(&[0xcd; 32]).unwrap(),
                    ),
                ])
                .unwrap(),
            ),
        ),
        accept(
            "tree-bytewise-order",
            "Names sort by raw bytes: uppercase before lowercase, `a` < `a-b` < `a.c` < `a0`, non-UTF-8 last.",
            Object::Tree(
                Tree::from_unsorted(vec![
                    file(b"a0", b""),
                    file(b"a.c", b""),
                    file(b"a-b", b""),
                    file(b"a", b""),
                    file(b"Z", b""),
                    file(&[0xff, 0x41], b""),
                    file(".swarmignore".as_bytes(), b""),
                ])
                .unwrap(),
            ),
        ),
        accept(
            "tree-wide-integers",
            "Sizes that need 1, 2, 4 and 8 byte integer heads.",
            Object::Tree(
                Tree::new(vec![
                    TreeEntry::blob("s1", Mode::File, fixed(ObjectKind::Blob, 1), 24),
                    TreeEntry::blob("s2", Mode::File, fixed(ObjectKind::Blob, 2), 256),
                    TreeEntry::blob("s4", Mode::File, fixed(ObjectKind::Blob, 3), 65_536),
                    TreeEntry::blob("s8", Mode::File, fixed(ObjectKind::Blob, 4), 4_294_967_296),
                ])
                .unwrap(),
            ),
        ),
        accept(
            "tree-ignorable-extensions",
            "Even extension keys 64 and 1000 are preserved and count toward the id.",
            Object::Tree(Tree::with_extensions(vec![file(b"a", b"")], ext).unwrap()),
        ),
    ]
}

fn metas() -> Vec<Case> {
    let mut imported = Meta::new(
        "Linus <linus@example.org>",
        Timestamp::new(1_112_911_993, -420),
        "01JIMPORTER000000000000000",
        "Initial revision\n",
    );
    imported.committer = Some("Linus <linus@example.org>".into());
    imported.committer_time = Some(Timestamp::new(1_112_911_993, -420));
    imported.git = Some(
        b"author Linus <linus@example.org> 1112911993 -0700\ncommitter Linus <linus@example.org> 1112911993 -0700\n"
            .to_vec(),
    );
    let mut negative = Meta::new("Old", Timestamp::new(-86_400, -720), "01JX", "before 1970");
    negative.provenance = Some(Provenance::default());
    vec![
        accept(
            "meta-empty-root",
            "The fixed meta of every repo's empty root changeset.",
            Object::Meta(Box::new(Meta::empty_root())),
        ),
        accept(
            "meta-agent-provenance",
            "An agent's change with full provenance.",
            Object::Meta(Box::new(full_meta())),
        ),
        accept(
            "meta-imported-git",
            "An imported git commit with committer fields and the git extension (key 64).",
            Object::Meta(Box::new(imported)),
        ),
        accept(
            "meta-negative-time",
            "Negative seconds and timezone offset (major type 1) and an empty provenance map.",
            Object::Meta(Box::new(negative)),
        ),
    ]
}

fn changesets() -> Vec<Case> {
    let mut ext = Extensions::new();
    ext.insert(66, Value::Bytes(vec![1, 2, 3]));
    vec![
        accept(
            "changeset-empty-root",
            "The empty root changeset; the same id in every repo.",
            Object::Changeset(Changeset::empty_root()),
        ),
        accept(
            "changeset-first-commit",
            "A parentless changeset adding two files.",
            Object::Changeset(
                Changeset::new(
                    vec![],
                    sample_tree().id(),
                    full_meta().id(),
                    ChangeId::new(*b"0123456789abcdef"),
                    vec![put("a", b"alpha\n"), put("b", b"beta\n")],
                )
                .unwrap(),
            ),
        ),
        accept(
            "changeset-edit",
            "One parent; a put, an executable put, a delete, a file replaced by a directory, and a gitlink.",
            Object::Changeset(
                Changeset::new(
                    vec![fixed(ObjectKind::Changeset, 9)],
                    fixed(ObjectKind::Tree, 10),
                    fixed(ObjectKind::Meta, 11),
                    ChangeId::new([0x5a; 16]),
                    vec![
                        DeltaEntry::delete(p("docs")),
                        put("docs/index.md", b"# docs\n"),
                        DeltaEntry::delete(p("old.txt")),
                        DeltaEntry::put(p("run.sh"), Mode::Executable, blob_id(b"#!/bin/sh\n"), 10),
                        put("src/lib.rs", b"pub fn f() {}\n"),
                        DeltaEntry::put(
                            p("vendor/dep"),
                            Mode::Gitlink,
                            ObjectId::git_foreign(&[0x01; 20]).unwrap(),
                            0,
                        ),
                    ],
                )
                .unwrap(),
            ),
        ),
        accept(
            "changeset-merge",
            "Two parents, an empty delta (the merge took the first parent's tree) and an ignorable extension.",
            Object::Changeset(
                Changeset::with_extensions(
                    vec![
                        fixed(ObjectKind::Changeset, 1),
                        fixed(ObjectKind::Changeset, 2),
                    ],
                    fixed(ObjectKind::Tree, 3),
                    fixed(ObjectKind::Meta, 4),
                    ChangeId::new([0xff; 16]),
                    vec![],
                    ext,
                )
                .unwrap(),
            ),
        ),
    ]
}

fn rejects() -> Vec<Case> {
    let t = sample_tree().encode();
    let m = Meta::empty_root().encode();
    let c = Changeset::empty_root().encode();
    let b = blob_id(b"");
    let tree = ObjectKind::Tree;
    let with = |bytes: &[u8], key: u64, v: Value| {
        let mut map = map_of(bytes);
        map.insert(key, v);
        enc(map)
    };
    let without = |bytes: &[u8], key: u64| {
        let mut map = map_of(bytes);
        map.remove(&key);
        enc(map)
    };
    let mut trailing = t.clone();
    trailing.push(0x00);
    // {0: 1} with the 1 written as 0x18 0x01.
    let long_int = vec![0xa2, 0x00, 0x18, 0x01, 0x01, 0x80];
    let indefinite = vec![0xa2, 0x00, 0x01, 0x01, 0x9f, 0xff];
    let tagged = vec![0xa2, 0x00, 0x01, 0x01, 0xc0, 0x80];
    let float = vec![0xa2, 0x00, 0xf9, 0x3c, 0x00, 0x01, 0x80];
    let unsorted_keys = vec![0xa2, 0x01, 0x80, 0x00, 0x01];
    let dup_keys = vec![0xa3, 0x00, 0x01, 0x01, 0x80, 0x01, 0x80];
    let text_key = vec![0xa2, 0x00, 0x01, 0x61, 0x61, 0x80];
    let mut long_bytes = tree_bytes(vec![entry_value(b"a", 1, &b, 0)]);
    // Rewrite the name's head 0x41 as 0x58 0x01.
    let pos = long_bytes
        .windows(2)
        .position(|w| w == [0x41, b'a'])
        .unwrap();
    long_bytes.splice(pos..pos + 1, [0x58, 0x01]);
    let mut bad_utf8 = m.clone();
    bad_utf8.pop();
    bad_utf8.extend_from_slice(&[0x62, 0xc3, 0x28]);
    let cs_id = fixed(ObjectKind::Changeset, 1);
    let delta_entry = |path: &[u8], op: Value| {
        Value::Map(BTreeMap::from([(1, Value::Bytes(path.to_vec())), (2, op)]))
    };
    vec![
        reject(
            "reject-trailing-byte",
            "A valid tree followed by one extra byte.",
            tree,
            trailing,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-non-shortest-int",
            "format_version 1 encoded in two bytes.",
            tree,
            long_int,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-non-shortest-length",
            "A one-byte name with a two-byte length head.",
            tree,
            long_bytes,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-indefinite-array",
            "Entries as an indefinite-length array.",
            tree,
            indefinite,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-tag",
            "A tagged value.",
            tree,
            tagged,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-float",
            "A half-precision float.",
            tree,
            float,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-unsorted-keys",
            "Map keys 1 then 0.",
            tree,
            unsorted_keys,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-duplicate-keys",
            "Map key 1 twice.",
            tree,
            dup_keys,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-text-key",
            "A text map key.",
            tree,
            text_key,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-format-version-2",
            "An unknown format_version.",
            tree,
            with(&t, 0, Value::Uint(2)),
            Subcode::UnknownFormatVersion,
        ),
        reject(
            "reject-missing-format-version",
            "No key 0.",
            tree,
            without(&t, 0),
            Subcode::InvalidField,
        ),
        reject(
            "reject-critical-extension",
            "Unknown odd extension key 65.",
            tree,
            with(&t, 65, Value::Uint(0)),
            Subcode::UnknownCriticalExtension,
        ),
        reject(
            "reject-unknown-defined-key",
            "Key 2 is not defined for trees.",
            tree,
            with(&t, 2, Value::Uint(0)),
            Subcode::InvalidField,
        ),
        reject(
            "reject-tree-unsorted-entries",
            "Entries b then a.",
            tree,
            tree_bytes(vec![
                entry_value(b"b", 1, &b, 0),
                entry_value(b"a", 1, &b, 0),
            ]),
            Subcode::InvalidTree,
        ),
        reject(
            "reject-tree-duplicate-names",
            "Two entries named a.",
            tree,
            tree_bytes(vec![
                entry_value(b"a", 1, &b, 0),
                entry_value(b"a", 2, &b, 0),
            ]),
            Subcode::InvalidTree,
        ),
        reject(
            "reject-tree-reserved-name",
            "An entry named .Swarm.",
            tree,
            tree_bytes(vec![entry_value(b".Swarm", 1, &b, 0)]),
            Subcode::InvalidName,
        ),
        reject(
            "reject-tree-slash-name",
            "An entry name containing /.",
            tree,
            tree_bytes(vec![entry_value(b"a/b", 1, &b, 0)]),
            Subcode::InvalidName,
        ),
        reject(
            "reject-tree-dotdot-name",
            "An entry named ..",
            tree,
            tree_bytes(vec![entry_value(b"..", 1, &b, 0)]),
            Subcode::InvalidName,
        ),
        reject(
            "reject-tree-long-name",
            "A 256-byte entry name.",
            tree,
            tree_bytes(vec![entry_value(&[b'n'; 256], 1, &b, 0)]),
            Subcode::InvalidName,
        ),
        reject(
            "reject-tree-unknown-mode",
            "Mode 6.",
            tree,
            tree_bytes(vec![entry_value(b"a", 6, &b, 0)]),
            Subcode::InvalidMode,
        ),
        reject(
            "reject-tree-mode-kind-mismatch",
            "Mode tree with a blob id.",
            tree,
            tree_bytes(vec![entry_value(b"a", 4, &b, 0)]),
            Subcode::WrongKind,
        ),
        reject(
            "reject-tree-gitlink-size",
            "A gitlink with a non-zero size.",
            tree,
            tree_bytes(vec![entry_value(
                b"a",
                5,
                &ObjectId::git_foreign(&[1; 20]).unwrap(),
                1,
            )]),
            Subcode::InvalidField,
        ),
        reject(
            "reject-tree-reserved-algorithm",
            "An entry id with algorithm byte 0x03.",
            tree,
            {
                let mut raw = b.as_bytes().to_vec();
                raw[0] = 0x03;
                tree_bytes(vec![Value::Map(BTreeMap::from([
                    (1, Value::Bytes(b"a".to_vec())),
                    (2, Value::Uint(1)),
                    (3, Value::Bytes(raw)),
                    (4, Value::Uint(0)),
                ]))])
            },
            Subcode::InvalidId,
        ),
        reject(
            "reject-meta-invalid-utf8",
            "A message that is not UTF-8.",
            ObjectKind::Meta,
            bad_utf8,
            Subcode::NonCanonical,
        ),
        reject(
            "reject-meta-empty-chain",
            "Provenance with an explicitly empty principal_chain (must be omitted).",
            ObjectKind::Meta,
            with(
                &m,
                7,
                Value::Map(BTreeMap::from([(1, Value::Array(vec![]))])),
            ),
            Subcode::InvalidField,
        ),
        reject(
            "reject-meta-short-digest",
            "A 31-byte prompt_digest.",
            ObjectKind::Meta,
            with(
                &m,
                7,
                Value::Map(BTreeMap::from([(5, Value::Bytes(vec![0; 31]))])),
            ),
            Subcode::InvalidField,
        ),
        reject(
            "reject-meta-git-not-bytes",
            "Key 64 (git) as text.",
            ObjectKind::Meta,
            with(&m, 64, Value::Text("x".into())),
            Subcode::InvalidField,
        ),
        reject(
            "reject-changeset-short-change-id",
            "A 15-byte ChangeId.",
            ObjectKind::Changeset,
            with(&c, 4, Value::Bytes(vec![0; 15])),
            Subcode::InvalidId,
        ),
        reject(
            "reject-changeset-duplicate-parent",
            "The same parent twice.",
            ObjectKind::Changeset,
            with(&c, 1, Value::Array(vec![idv(&cs_id), idv(&cs_id)])),
            Subcode::InvalidParents,
        ),
        reject(
            "reject-changeset-parent-kind",
            "A tree id as a parent.",
            ObjectKind::Changeset,
            with(&c, 1, Value::Array(vec![idv(&Tree::empty().id())])),
            Subcode::WrongKind,
        ),
        reject(
            "reject-changeset-unsorted-delta",
            "Delta paths b then a.",
            ObjectKind::Changeset,
            with(
                &c,
                5,
                Value::Array(vec![
                    delta_entry(b"b", Value::Uint(0)),
                    delta_entry(b"a", Value::Uint(0)),
                ]),
            ),
            Subcode::InvalidDelta,
        ),
        reject(
            "reject-changeset-path-dotdot",
            "A delta path with a .. component.",
            ObjectKind::Changeset,
            with(
                &with(&c, 1, Value::Array(vec![idv(&cs_id)])),
                5,
                Value::Array(vec![delta_entry(b"a/../b", Value::Uint(0))]),
            ),
            Subcode::InvalidPath,
        ),
        reject(
            "reject-changeset-delete-without-parent",
            "A parentless changeset whose delta deletes a path.",
            ObjectKind::Changeset,
            with(&c, 5, Value::Array(vec![delta_entry(b"a", Value::Uint(0))])),
            Subcode::InvalidDelta,
        ),
        reject(
            "reject-changeset-put-tree-mode",
            "A delta put with mode tree.",
            ObjectKind::Changeset,
            with(
                &c,
                5,
                Value::Array(vec![delta_entry(
                    b"a",
                    Value::Map(BTreeMap::from([
                        (1, Value::Uint(4)),
                        (2, idv(&Tree::empty().id())),
                        (3, Value::Uint(0)),
                    ])),
                )]),
            ),
            Subcode::InvalidMode,
        ),
        reject(
            "reject-changeset-file-and-dir",
            "Puts for both a and a/b.",
            ObjectKind::Changeset,
            with(
                &c,
                5,
                Value::Array(vec![
                    delta_entry(
                        b"a",
                        Value::Map(BTreeMap::from([
                            (1, Value::Uint(1)),
                            (2, idv(&b)),
                            (3, Value::Uint(0)),
                        ])),
                    ),
                    delta_entry(
                        b"a/b",
                        Value::Map(BTreeMap::from([
                            (1, Value::Uint(1)),
                            (2, idv(&b)),
                            (3, Value::Uint(0)),
                        ])),
                    ),
                ]),
            ),
            Subcode::InvalidDelta,
        ),
    ]
}

fn name_json(key: &str, bytes: &[u8], out: &mut serde_json::Map<String, Json>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => out.insert(key.to_owned(), json!(s)),
        Err(_) => out.insert(format!("{key}_hex"), json!(hex::encode(bytes))),
    };
}

fn ext_json(ext: &Extensions) -> Json {
    Json::Object(
        ext.iter()
            .map(|(k, v)| (k.to_string(), json!(hex::encode(&v.encode()))))
            .collect(),
    )
}

fn ts_json(t: &Timestamp) -> Json {
    json!([t.seconds, t.tz_offset_minutes])
}

/// A readable rendering of an object, recorded next to its encoding for implementers.
fn render(obj: &Object) -> Json {
    match obj {
        Object::Blob(b) => json!({ "bytes_hex": hex::encode(b) }),
        Object::Tree(t) => {
            let entries: Vec<Json> = t
                .entries()
                .iter()
                .map(|e| {
                    let mut m = serde_json::Map::new();
                    name_json("name", &e.name, &mut m);
                    m.insert("mode".into(), json!(e.mode.as_u64()));
                    m.insert("id".into(), json!(e.id.to_text()));
                    m.insert("size".into(), json!(e.size));
                    Json::Object(m)
                })
                .collect();
            let mut v = json!({ "format_version": 1, "entries": entries });
            if !t.extensions().is_empty() {
                v["extensions"] = ext_json(t.extensions());
            }
            v
        }
        Object::Meta(m) => {
            let mut v = json!({
                "format_version": 1,
                "author": m.author,
                "author_time": ts_json(&m.author_time),
                "creator": m.creator,
                "message": m.message,
            });
            if let Some(c) = &m.committer {
                v["committer"] = json!(c);
            }
            if let Some(t) = &m.committer_time {
                v["committer_time"] = ts_json(t);
            }
            if let Some(p) = &m.provenance {
                let mut pv = serde_json::Map::new();
                if !p.principal_chain.is_empty() {
                    pv.insert("principal_chain".into(), json!(p.principal_chain));
                }
                for (k, f) in [
                    ("runtime", &p.runtime),
                    ("model", &p.model),
                    ("session", &p.session),
                ] {
                    if let Some(s) = f {
                        pv.insert(k.into(), json!(s));
                    }
                }
                if let Some(d) = p.prompt_digest {
                    pv.insert("prompt_digest_hex".into(), json!(hex::encode(&d)));
                }
                if !p.checks_run.is_empty() {
                    let checks: Vec<Json> = p
                        .checks_run
                        .iter()
                        .map(|c| json!({"name": c.name, "result": c.result, "duration_ms": c.duration_ms}))
                        .collect();
                    pv.insert("checks_run".into(), json!(checks));
                }
                if let Some(c) = p.cost {
                    let mut cv = serde_json::Map::new();
                    for (k, f) in [
                        ("input_tokens", c.input_tokens),
                        ("output_tokens", c.output_tokens),
                        ("usd_micros", c.usd_micros),
                    ] {
                        if let Some(n) = f {
                            cv.insert(k.into(), json!(n));
                        }
                    }
                    pv.insert("cost".into(), Json::Object(cv));
                }
                v["provenance"] = Json::Object(pv);
            }
            if let Some(g) = &m.git {
                v["git_hex"] = json!(hex::encode(g));
            }
            if !m.extensions().is_empty() {
                v["extensions"] = ext_json(m.extensions());
            }
            v
        }
        Object::Changeset(c) => {
            let delta: Vec<Json> = c
                .delta()
                .iter()
                .map(|e| {
                    let mut m = serde_json::Map::new();
                    name_json("path", e.path.as_bytes(), &mut m);
                    match e.op {
                        bee_object::DeltaOp::Delete => {
                            m.insert("op".into(), json!("delete"));
                        }
                        bee_object::DeltaOp::Put { mode, id, size } => {
                            m.insert(
                                "op".into(),
                                json!({"mode": mode.as_u64(), "id": id.to_text(), "size": size}),
                            );
                        }
                    }
                    Json::Object(m)
                })
                .collect();
            let mut v = json!({
                "format_version": 1,
                "parents": c.parents().iter().map(|p| p.to_text()).collect::<Vec<_>>(),
                "root": c.root().to_text(),
                "meta": c.meta().to_text(),
                "change_id": c.change_id().to_text(),
                "delta": delta,
            });
            if !c.extensions().is_empty() {
                v["extensions"] = ext_json(c.extensions());
            }
            v
        }
    }
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Meta => "meta",
        ObjectKind::Changeset => "changeset",
        _ => unreachable!(),
    }
}

fn case_json(case: &Case) -> Json {
    match &case.expect {
        Expect::Accept(obj) => json!({
            "name": case.name,
            "description": case.description,
            "kind": kind_name(case.kind),
            "value": render(obj),
            "encoding_hex": hex::encode(&obj.encode()),
            "id": obj.id().to_text(),
        }),
        Expect::Reject {
            bytes,
            reason,
            subcode,
        } => json!({
            "name": case.name,
            "description": case.description,
            "kind": kind_name(case.kind),
            "encoding_hex": hex::encode(bytes),
            "error": { "reason": reason.as_str(), "subcode": subcode.map(|s| s.as_str()) },
        }),
    }
}

fn file_json(cases: &[Case]) -> Json {
    json!({
        "format": "sCBOR v1",
        "spec": "SPEC.md §3.1, §4",
        "frozen": true,
        "vectors": cases.iter().map(case_json).collect::<Vec<_>>(),
    })
}

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/vectors")
}

fn kind_from_name(name: &str) -> ObjectKind {
    match name {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "meta" => ObjectKind::Meta,
        "changeset" => ObjectKind::Changeset,
        other => panic!("unknown kind {other}"),
    }
}

fn check_error(name: &str, err: &BeeError, recorded: &Json) {
    assert_eq!(err.reason.as_str(), recorded["reason"], "{name}: {err}");
    let sub = err.subcode().map(|s| s.as_str());
    assert_eq!(json!(sub), recorded["subcode"], "{name}: {err}");
}

#[test]
fn golden_vectors_match() {
    let write = std::env::var_os("BEE_WRITE_VECTORS").is_some();
    let mut total = 0;
    for (file, cases) in files() {
        let path = vectors_dir().join(file);
        let expected = file_json(&cases);
        if !path.exists() {
            assert!(
                write,
                "{} is missing; run with BEE_WRITE_VECTORS=1 to create it",
                path.display()
            );
            let text = serde_json::to_string_pretty(&expected).unwrap() + "\n";
            std::fs::write(&path, text).unwrap();
        }
        let recorded: Json =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let rec_vectors = recorded["vectors"].as_array().unwrap();
        assert_eq!(rec_vectors.len(), cases.len(), "{file}: vector count");
        for (case, rec) in cases.iter().zip(rec_vectors) {
            total += 1;
            assert_eq!(rec["name"], case.name, "{file}: order");
            let kind = kind_from_name(rec["kind"].as_str().unwrap());
            assert_eq!(kind, case.kind);
            let bytes = hex::decode(rec["encoding_hex"].as_str().unwrap()).unwrap();
            match &case.expect {
                Expect::Accept(obj) => {
                    // The code's encoding and id equal the frozen ones.
                    assert_eq!(
                        hex::encode(&obj.encode()),
                        rec["encoding_hex"],
                        "{}: encoding",
                        case.name
                    );
                    assert_eq!(obj.id().to_text(), rec["id"], "{}: id", case.name);
                    // Decoding the frozen bytes gives the same value, and verifies against the id.
                    let id = ObjectId::parse(rec["id"].as_str().unwrap()).unwrap();
                    let decoded = Object::verify(&id, &bytes, &Limits::DEFAULT).unwrap();
                    assert_eq!(&decoded, obj, "{}: decoded value", case.name);
                    assert_eq!(
                        render(&decoded),
                        rec["value"],
                        "{}: readable value",
                        case.name
                    );
                }
                Expect::Reject { bytes: built, .. } => {
                    assert_eq!(&bytes, built, "{}: rejected bytes", case.name);
                    let err = Object::decode(kind, &bytes, &Limits::DEFAULT).unwrap_err();
                    check_error(case.name, &err, &rec["error"]);
                }
            }
        }
    }
    assert!(total >= 50, "expected a full vector set, got {total}");
}

#[test]
fn id_text_forms() {
    // Text forms are fixed by SPEC §3.1 and §3.3; these are checked literally.
    let d: [u8; 32] = core::array::from_fn(|i| i as u8);
    let digits = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    for (kind, tag) in [
        (ObjectKind::Blob, "b1"),
        (ObjectKind::Tree, "t1"),
        (ObjectKind::Changeset, "c1"),
        (ObjectKind::Meta, "m1"),
        (ObjectKind::Derived(0x10), "d110"),
        (ObjectKind::Derived(0x7f), "d17f"),
    ] {
        let id = ObjectId::new(kind, d);
        assert_eq!(id.to_text(), format!("{tag}_{digits}"));
        assert_eq!(id.as_bytes()[0], 0x01);
        assert_eq!(id.as_bytes()[1], kind.as_byte());
    }
    let g = ObjectId::git_foreign(&d[..20]).unwrap();
    assert_eq!(
        g.to_text(),
        format!("g2_{}{}", &digits[..40], "0".repeat(24))
    );
    let k = ChangeId::new(core::array::from_fn(|i| i as u8));
    assert_eq!(k.to_text(), "k_000102030405060708090a0b0c0d0e0f");
    // Importer ChangeIds (SPEC §3.3) for a fixed SHA-1.
    let sha1 = hex::decode("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    let mut input = b"swarm-import\0".to_vec();
    input.extend_from_slice(&sha1);
    let expected = &bee_object::digest(&input)[..16];
    assert_eq!(import_change_id(&sha1).as_bytes(), expected);
}

#[test]
fn empty_root_ids_are_stable() {
    // The empty root's ids are part of the contract: every repo starts from them.
    let vectors: Json = serde_json::from_str(
        &std::fs::read_to_string(vectors_dir().join("changeset-v1.json")).unwrap(),
    )
    .unwrap();
    let root = &vectors["vectors"][0];
    assert_eq!(root["name"], "changeset-empty-root");
    assert_eq!(root["id"], Changeset::empty_root().id().to_text());
    assert_eq!(root["value"]["root"], Tree::empty().id().to_text());
    assert_eq!(root["value"]["meta"], Meta::empty_root().id().to_text());
}
