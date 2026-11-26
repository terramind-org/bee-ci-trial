//! Fuzzing of the typed object decoders (PRODUCT §4.25): Tree, Meta and Changeset decoding of
//! arbitrary bytes and of valid encodings with random mutations never panics, and whatever
//! decodes is canonical (it re-encodes to exactly the input).

use bee_core::{Limits, Mode, ObjectId, ObjectKind};
use bee_object::{Changeset, Meta, Object, Tree, tree::TreeEntry};
use proptest::prelude::*;

/// Valid encodings to mutate: an empty tree, a tree with every leaf mode and a subtree, the
/// empty root meta and changeset.
fn seeds() -> Vec<(ObjectKind, Vec<u8>)> {
    let blob = ObjectId::new(ObjectKind::Blob, [7u8; 32]);
    let sub = ObjectId::new(ObjectKind::Tree, [8u8; 32]);
    let tree = Tree::new(vec![
        TreeEntry::blob("a.txt", Mode::File, blob, 12),
        TreeEntry::blob("run.sh", Mode::Executable, blob, 3),
        TreeEntry::blob("link", Mode::Symlink, blob, 5),
        TreeEntry::tree("src", sub),
    ])
    .or_else(|_| {
        Tree::from_unsorted(vec![
            TreeEntry::blob("a.txt", Mode::File, blob, 12),
            TreeEntry::blob("run.sh", Mode::Executable, blob, 3),
            TreeEntry::blob("link", Mode::Symlink, blob, 5),
            TreeEntry::tree("src", sub),
        ])
    })
    .expect("a valid tree");
    vec![
        (ObjectKind::Tree, Tree::empty().encode()),
        (ObjectKind::Tree, tree.encode()),
        (ObjectKind::Meta, Meta::empty_root().encode()),
        (ObjectKind::Changeset, Changeset::empty_root().encode()),
    ]
}

/// Decodes `bytes` as every typed kind; a success must be canonical.
fn decode_all(bytes: &[u8]) {
    let limits = Limits::DEFAULT;
    if let Ok(t) = Tree::decode(bytes) {
        assert_eq!(t.encode(), bytes, "a decoded tree is canonical");
    }
    if let Ok(m) = Meta::decode(bytes) {
        assert_eq!(m.encode(), bytes, "a decoded meta is canonical");
    }
    if let Ok(c) = Changeset::decode(bytes) {
        assert_eq!(c.encode(), bytes, "a decoded changeset is canonical");
    }
    for kind in [
        ObjectKind::Blob,
        ObjectKind::Tree,
        ObjectKind::Meta,
        ObjectKind::Changeset,
    ] {
        if let Ok(o) = Object::decode(kind, bytes, &limits) {
            assert_eq!(o.encode(), bytes);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn typed_decoders_never_panic_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        decode_all(&bytes);
    }

    #[test]
    fn typed_decoders_never_panic_on_mutated_valid_objects(
        seed in 0usize..4,
        edits in proptest::collection::vec((any::<usize>(), any::<u8>(), 0u8..4), 1..8),
    ) {
        let (kind, mut bytes) = seeds().swap_remove(seed);
        for (at, b, op) in edits {
            let n = bytes.len().max(1);
            match op {
                0 => if !bytes.is_empty() { bytes[at % n] = b },
                1 => bytes.insert(at % (bytes.len() + 1), b),
                2 => if !bytes.is_empty() { bytes.remove(at % n); },
                _ => bytes.truncate(at % n),
            }
        }
        decode_all(&bytes);
        let _ = Object::decode(kind, &bytes, &Limits::DEFAULT);
    }
}

#[test]
fn seeds_are_valid_and_canonical() {
    for (kind, bytes) in seeds() {
        let o = Object::decode(kind, &bytes, &Limits::DEFAULT).expect("valid seed");
        assert_eq!(o.encode(), bytes);
    }
}
