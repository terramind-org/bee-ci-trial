//! Coverage for F416-030 (symlinks, SPEC §4.2 and §10.3): checkout writes a symlink entry as
//! a real symlink, a scan reads it back as a symlink entry whose content is its target, and
//! replacing it with a file or removing it never touches what it points at.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use bee_core::{Mode, ObjectId};
use bee_wc::Faults;
use bee_wc::checkout::{apply, store_local};
use bee_wc::journal::Layout;
use bee_wc::manifest::{Entry, Manifest};
use bee_wc::scan::{StatCache, scan};
use bee_wc::sparse::Sparse;
use bee_wc::state::PathBytes;

fn path(p: &str) -> PathBytes {
    PathBytes(p.as_bytes().to_vec())
}

#[test]
fn symlinks_are_checked_out_as_symlinks_and_scanned_back() {
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path();
    let layout = Layout::new(root.join(".bee"));
    fs::create_dir_all(&layout.dir).expect("mkdir");
    for b in [&b"hello\n"[..], b"a.txt", b"../outside"] {
        store_local(&layout, b).expect("store");
    }
    let file = Entry::for_bytes(Mode::File, b"hello\n");
    let link = Entry::for_bytes(Mode::Symlink, b"a.txt");
    let dangling = Entry::for_bytes(Mode::Symlink, b"../outside");
    let mut fetch = |_: &[ObjectId]| Ok(BTreeMap::new());
    let done = apply(
        root,
        &layout,
        &[
            (path("a.txt"), Some(file)),
            (path("link"), Some(link)),
            (path("dir/up"), Some(dangling)),
        ],
        &BTreeSet::new(),
        &mut fetch,
        &Faults::none(),
        &bee_wc::eol::Attributes::new(),
    )
    .expect("apply");
    assert_eq!(done.written, 3);
    let meta = fs::symlink_metadata(root.join("link")).unwrap();
    assert!(meta.file_type().is_symlink(), "written as a symlink");
    assert_eq!(
        fs::read_link(root.join("link")).unwrap().to_str(),
        Some("a.txt")
    );
    assert_eq!(fs::read(root.join("link")).unwrap(), b"hello\n");
    // A symlink pointing outside the tree is written as-is, not followed.
    assert_eq!(
        fs::read_link(root.join("dir/up")).unwrap().to_str(),
        Some("../outside")
    );

    // A scan sees symlinks as symlink entries whose blob is the target text.
    let base: Manifest = [
        (b"a.txt".to_vec(), file),
        (b"link".to_vec(), link),
        (b"dir/up".to_vec(), dangling),
    ]
    .into_iter()
    .collect();
    let s = scan(
        root,
        0,
        &Sparse::default(),
        &base,
        &BTreeSet::new(),
        &StatCache::default(),
    )
    .expect("scan");
    assert_eq!(s.actual, base);

    // Turning the symlink into a file replaces the link, not its target.
    let plain = Entry::for_bytes(Mode::File, b"now a file\n");
    store_local(&layout, b"now a file\n").expect("store");
    apply(
        root,
        &layout,
        &[(path("link"), Some(plain)), (path("dir/up"), None)],
        &BTreeSet::new(),
        &mut fetch,
        &Faults::none(),
        &bee_wc::eol::Attributes::new(),
    )
    .expect("apply");
    assert!(
        !fs::symlink_metadata(root.join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(root.join("link")).unwrap(), b"now a file\n");
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello\n");
    assert!(fs::symlink_metadata(root.join("dir/up")).is_err());
}

#[test]
fn checkout_never_writes_through_a_symlinked_directory() {
    let dir = tempfile::tempdir().expect("tmp");
    let outside = tempfile::tempdir().expect("outside");
    let root = dir.path();
    let layout = Layout::new(root.join(".bee"));
    fs::create_dir_all(&layout.dir).expect("mkdir");
    std::os::unix::fs::symlink(outside.path(), root.join("sub")).expect("symlink");
    store_local(&layout, b"x").expect("store");
    let mut fetch = |_: &[ObjectId]| Ok(BTreeMap::new());
    let done = apply(
        root,
        &layout,
        &[(path("sub/f"), Some(Entry::for_bytes(Mode::File, b"x")))],
        &BTreeSet::new(),
        &mut fetch,
        &Faults::none(),
        &bee_wc::eol::Attributes::new(),
    )
    .expect("apply");
    assert_eq!(done.written, 0);
    assert_eq!(done.hidden[0].reason, "symlinked_parent");
    assert!(!outside.path().join("f").exists());
}
