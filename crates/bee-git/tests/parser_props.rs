//! Property tests for the parsers: arbitrary input never panics, and whatever parses
//! serializes back to exactly the input.

use bee_core::{BeeError, Reason};
use bee_git::{
    GitCommit, GitObject, GitOid, GitTag, GitTree, GitTreeEntry, Importer, ObjectFormat,
    ObjectType, Pack, PackIndex, RawObject, TreeNote, delta,
};
use bee_object::Value;
use proptest::prelude::*;

fn formats() -> impl Strategy<Value = ObjectFormat> {
    prop_oneof![Just(ObjectFormat::Sha1), Just(ObjectFormat::Sha256)]
}

fn kinds() -> impl Strategy<Value = ObjectType> {
    prop_oneof![
        Just(ObjectType::Blob),
        Just(ObjectType::Tree),
        Just(ObjectType::Commit),
        Just(ObjectType::Tag),
    ]
}

fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..max)
}

fn arb_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<u64>().prop_map(Value::Uint),
        bytes(8).prop_map(Value::Bytes),
        "[a-z]{0,4}".prop_map(Value::Text),
    ];
    leaf.prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map(0u64..5, inner, 0..4).prop_map(Value::Map),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn object_parsers_never_panic_and_round_trip(
        format in formats(),
        kind in kinds(),
        data in bytes(256),
    ) {
        if let Ok(obj) = GitObject::parse(kind, &data, format) {
            prop_assert_eq!(obj.serialize(), data);
        }
    }

    #[test]
    fn commit_bodies_after_a_tree_line_round_trip(format in formats(), rest in bytes(256)) {
        let mut body = format!("tree {}\n", format.empty_tree()).into_bytes();
        body.extend_from_slice(&rest);
        // Leading `parent` lines must be valid ids; anything else parses.
        if let Ok(c) = GitCommit::parse(&body, format) {
            prop_assert_eq!(c.serialize(), body);
            let _ = (c.author(), c.committer(), c.encoding(), c.headers());
        }
    }

    #[test]
    fn tag_bodies_round_trip(format in formats(), rest in bytes(128)) {
        let mut body = format!("object {}\ntype tree\n", format.empty_tree()).into_bytes();
        body.extend_from_slice(&rest);
        if let Ok(t) = GitTag::parse(&body, format) {
            prop_assert_eq!(t.serialize(), body);
            let _ = (t.tagger(), t.name(), t.message());
        }
    }

    #[test]
    fn generated_trees_round_trip(
        format in formats(),
        entries in proptest::collection::vec(
            ("[0-7]{1,7}", proptest::collection::vec(1u8..=255, 1..12), bytes(32)),
            0..8,
        ),
    ) {
        let n = format.oid_len();
        let tree = GitTree {
            entries: entries
                .into_iter()
                .map(|(mode, mut name, raw)| {
                    name.retain(|&b| b != b'/');
                    if name.is_empty() {
                        name.push(b'x');
                    }
                    let mut id = raw;
                    id.resize(n, 7);
                    GitTreeEntry {
                        mode: mode.into_bytes(),
                        name,
                        oid: format.oid_from_bytes(&id).unwrap(),
                    }
                })
                .collect(),
        };
        let bytes = tree.serialize();
        prop_assert_eq!(GitTree::parse(&bytes, format).unwrap(), tree);
    }

    #[test]
    fn framed_and_loose_never_panic(data in bytes(128), cap in 0u64..512) {
        if let Ok(raw) = RawObject::from_framed(&data) {
            prop_assert_eq!(raw.to_framed(), data.clone());
        }
        let _ = RawObject::decode_loose(&data, cap);
    }

    #[test]
    fn loose_round_trip(kind in kinds(), data in bytes(512)) {
        let raw = RawObject::new(kind, data);
        prop_assert_eq!(RawObject::decode_loose(&raw.encode_loose(), 1 << 20).unwrap(), raw);
    }

    #[test]
    fn pack_and_index_parsers_never_panic(format in formats(), data in bytes(512)) {
        let none = |_: &GitOid| Ok(None);
        if let Ok(p) = Pack::parse(&data, format) {
            let _ = p.read_all(&none);
        }
        let _ = PackIndex::parse(&data, format);
        let mut framed = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        framed.extend_from_slice(&data);
        let sum = format.hash(&framed).unwrap();
        framed.extend_from_slice(sum.as_bytes());
        if let Ok(p) = Pack::parse(&framed, format) {
            let _ = p.read_all(&none);
        }
    }

    #[test]
    fn delta_never_panics(base in bytes(64), d in bytes(64)) {
        let _ = delta::apply(&base, &d, 1 << 20);
        let _ = delta::result_size(&d);
    }

    #[test]
    fn note_parser_never_panics(v in arb_value()) {
        if let Ok(n) = TreeNote::from_value(&v) {
            prop_assert_eq!(TreeNote::from_value(&n.to_value()).unwrap(), n);
        }
    }

    #[test]
    fn importer_never_panics_on_arbitrary_trees(format in formats(), data in bytes(200)) {
        let tree = RawObject::new(ObjectType::Tree, data);
        let toid = tree.oid(format).unwrap();
        let commit = RawObject::new(
            ObjectType::Commit,
            format!("tree {toid}\nauthor a <b> 1 +0000\n\nm\n").into_bytes(),
        );
        let coid = commit.oid(format).unwrap();
        let src = |o: &GitOid| {
            if *o == toid {
                Ok(tree.clone())
            } else if *o == coid {
                Ok(commit.clone())
            } else {
                Err(BeeError::new(Reason::MissingObject, o.to_hex()))
            }
        };
        let _ = Importer::new(format, "").import_commit(&src, &coid);
    }
}
