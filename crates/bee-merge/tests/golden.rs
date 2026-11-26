//! Golden vectors: every worked example in `docs/contracts/diff3-v1.md` §11–§12.

use crate::common;
use bee_merge::{
    ALGORITHM_ID, ConflictHunk, DiffHunk, DiffStats, MarkerStyle, diff, merge_lines, merge_two_way,
    unified_diff,
};

fn show(bytes: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(bytes))
}

#[test]
fn algorithm_id_is_frozen() {
    assert_eq!(ALGORITHM_ID, "diff3-v1");
    assert!(common::contract().starts_with("# diff3-v1\n"));
}

#[test]
fn diff_examples_match_contract() {
    let examples: Vec<_> = common::examples()
        .into_iter()
        .filter(|e| e.id().starts_with('D'))
        .collect();
    assert!(
        examples.len() >= 12,
        "contract needs at least 12 diff examples"
    );
    for e in &examples {
        let old = e.text("old");
        let new = e.text("new");
        let context: usize = e.raw("context").parse().unwrap();
        let hunks: Vec<DiffHunk> = e
            .tuples("hunks")
            .into_iter()
            .map(|t| DiffHunk {
                old_start: t[0],
                old_len: t[1],
                new_start: t[2],
                new_len: t[3],
            })
            .collect();
        assert_eq!(diff(&old, &new), hunks, "{} hunks", e.id());
        let unified = unified_diff(&old, &new, context);
        assert_eq!(
            show(&unified),
            show(&e.text("unified")),
            "{} unified",
            e.id()
        );
        let s = e.numbers("stats");
        assert_eq!(
            DiffStats::between(&old, &new),
            DiffStats {
                hunks: s[0],
                added: s[1],
                removed: s[2]
            },
            "{} stats",
            e.id()
        );
    }
}

#[test]
fn merge_examples_match_contract() {
    let examples: Vec<_> = common::examples()
        .into_iter()
        .filter(|e| e.id().starts_with('M'))
        .collect();
    assert!(
        examples.len() >= 12,
        "contract needs at least 12 merge examples"
    );
    for e in &examples {
        let ours = e.text("ours");
        let theirs = e.text("theirs");
        let base = if e.has("base") {
            e.text("base")
        } else {
            Vec::new()
        };
        let two_way = e.has("mode") && e.raw("mode") == "two-way";
        let merge = if two_way {
            merge_two_way(&ours, &theirs)
        } else {
            merge_lines(&base, &ours, &theirs)
        };
        let expected: Vec<ConflictHunk> = e
            .tuples("conflicts")
            .into_iter()
            .map(|t| ConflictHunk {
                base_start: t[0],
                base_len: t[1],
                ours_start: t[2],
                ours_len: t[3],
                theirs_start: t[4],
                theirs_len: t[5],
            })
            .collect();
        assert_eq!(
            merge.conflicts().collect::<Vec<_>>(),
            expected,
            "{} conflicts",
            e.id()
        );
        let output = e.text("output");
        assert_eq!(
            show(&merge.render(MarkerStyle::Diff3)),
            show(&output),
            "{} output",
            e.id()
        );
        if expected.is_empty() {
            assert_eq!(
                merge.merged().as_deref(),
                Some(&output[..]),
                "{} merged",
                e.id()
            );
        } else {
            assert!(merge.merged().is_none(), "{} must not merge", e.id());
        }
    }
}

#[test]
fn example_ids_are_unique_and_sequential() {
    let ids: Vec<String> = common::examples()
        .iter()
        .map(|e| e.id().to_string())
        .collect();
    for prefix in ['D', 'M'] {
        let nums: Vec<usize> = ids
            .iter()
            .filter(|i| i.starts_with(prefix))
            .map(|i| i[1..].parse().unwrap())
            .collect();
        assert_eq!(nums, (1..=nums.len()).collect::<Vec<_>>(), "{prefix} ids");
    }
}

/// Every test named in `docs/coverage/tests/merge.csv` exists in the file it names.
#[test]
fn coverage_rows_name_real_tests() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let csv = std::fs::read_to_string(format!("{root}/docs/coverage/tests/merge.csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("feature_id,test"));
    for line in lines {
        let (feature, test) = line.split_once(',').expect("two columns");
        assert!(feature.starts_with("F4"), "{line}");
        let (file, path) = test.split_once("::").expect("file::test");
        let name = path.rsplit("::").next().unwrap();
        let source = std::fs::read_to_string(format!("{root}/{file}")).unwrap();
        assert!(
            source.contains(&format!("fn {name}(")),
            "{line}: no such test"
        );
    }
}
