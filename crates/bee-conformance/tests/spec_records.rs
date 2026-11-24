//! The specification's own records are part of the contract: the global invariants (SPEC §4.7)
//! name fsck checks that exist, every decision record (Appendix B) has its fields and points at
//! sections that exist, and every one-way door (Appendix C) has a decision record.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The text of the section starting at heading `start` up to the next heading of the same
/// or a higher level.
fn section<'a>(text: &'a str, start: &str) -> &'a str {
    let begin = text
        .find(start)
        .unwrap_or_else(|| panic!("missing section {start:?}"));
    let level = start.chars().take_while(|c| *c == '#').count();
    let rest = &text[begin + start.len()..];
    let mut end = rest.len();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= level && line.chars().nth(hashes) == Some(' ') {
            end = offset;
            break;
        }
        offset += line.len();
    }
    &rest[..end]
}

/// Every `§N` or `§N.M` section number SPEC.md has a heading for.
fn spec_sections(spec: &str) -> BTreeSet<String> {
    spec.lines()
        .filter(|l| l.starts_with('#'))
        .filter_map(|l| {
            let t = l.trim_start_matches('#').trim();
            let num: String = t
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            let num = num.trim_end_matches('.').to_string();
            (!num.is_empty()).then_some(num)
        })
        .collect()
}

fn section_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices('§') {
        let num: String = text[i + '§'.len_utf8()..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let num = num.trim_end_matches('.').to_string();
        if !num.is_empty() {
            out.push(num);
        }
    }
    out
}

#[test]
fn global_invariants_name_real_fsck_checks() {
    let spec = repo_file("SPEC.md");
    let inv = section(&spec, "### 4.7 Global invariants");
    let items: Vec<&str> = inv
        .lines()
        .filter(|l| l.len() > 3 && l.as_bytes()[0].is_ascii_digit() && &l[1..3] == ". ")
        .collect();
    assert_eq!(items.len(), 4, "four invariants: {items:?}");
    let fsck = repo_file("crates/bee-store/src/fsck.rs");
    let mut named = 0;
    for part in inv.split("fsck:").skip(1) {
        let list = part.split(')').next().unwrap_or_default();
        for kind in list.split('`').skip(1).step_by(2) {
            assert!(
                fsck.contains(&format!("=> \"{kind}\"")),
                "SPEC §4.7 names fsck kind {kind:?}, which fsck does not report"
            );
            named += 1;
        }
    }
    assert!(named >= 6, "the invariants name their fsck checks");
}

#[test]
fn decision_records_are_complete_and_reference_real_sections() {
    let spec = repo_file("SPEC.md");
    let sections = spec_sections(&spec);
    let records = section(&spec, "## Appendix B: decision records");
    let mut numbers = Vec::new();
    for rec in records.split("\n### D-").skip(1) {
        let n: u32 = rec
            .split_whitespace()
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "bad record heading: {}",
                    rec.lines().next().unwrap_or_default()
                )
            });
        numbers.push(n);
        for field in [
            "- **Status:**",
            "- **Decision:**",
            "- **Why:**",
            "- **Consequences:**",
        ] {
            assert!(rec.contains(field), "D-{n} lacks {field}");
        }
        let status = rec
            .lines()
            .find(|l| l.starts_with("- **Status:**"))
            .unwrap();
        assert!(
            ["accepted", "superseded", "proposed"]
                .iter()
                .any(|s| status.contains(s)),
            "D-{n}: {status}"
        );
        let date_ok = status
            .split([' ', ',', '(', ')', '.'])
            .any(|w| w.len() == 10 && w.as_bytes()[4] == b'-' && w.as_bytes()[7] == b'-');
        assert!(date_ok, "D-{n} has no date: {status}");
        for r in section_refs(rec) {
            assert!(
                sections.contains(&r),
                "D-{n} cites §{r}, which SPEC.md does not have"
            );
        }
    }
    let expected: Vec<u32> = (1..=numbers.len() as u32).collect();
    assert_eq!(
        numbers, expected,
        "records are numbered D-1, D-2, ... without gaps"
    );
    assert!(numbers.len() >= 10);
}

#[test]
fn every_one_way_door_has_a_decision_record() {
    let spec = repo_file("SPEC.md");
    let records = section(&spec, "## Appendix B: decision records");
    let doors = section(&spec, "## Appendix C: one-way-door review");
    let rows: Vec<&str> = doors
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Door") && !l.starts_with("|---"))
        .collect();
    assert!(rows.len() >= 8, "the doors are listed: {rows:?}");
    for row in rows {
        let record = row.trim_end_matches('|').rsplit('|').next().unwrap().trim();
        assert!(record.starts_with("D-"), "{row}");
        assert!(
            records.contains(&format!("### {record} ")),
            "door without a record: {row}"
        );
    }
    for rule in [
        "decision record",
        "migration plan",
        "golden vectors",
        "amendments.md",
    ] {
        assert!(doors.contains(rule), "the review rule names {rule:?}");
    }
}
