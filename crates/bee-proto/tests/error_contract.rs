//! SPEC §12.2, the `ErrorReason` comments in common.proto and `bee_proto::errors` must agree.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bee_proto::errors::{REASONS, ReasonInfo, connect_code_name};

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// (grpc code name, connect code, retryable) as written in the documents.
type Row = (String, String, bool);

fn expected(info: &ReasonInfo) -> Row {
    let grpc = match info.code {
        tonic::Code::Cancelled => "CANCELLED".to_owned(),
        code => connect_code_name(code).to_ascii_uppercase(),
    };
    (grpc, info.connect_code().to_owned(), info.retryable)
}

#[test]
fn spec_table_matches_code() {
    let spec = repo_file("SPEC.md");
    let section = spec
        .split("### 12.2 Errors")
        .nth(1)
        .and_then(|s| s.split("### 12.3").next())
        .expect("SPEC §12.2");
    let mut rows = BTreeMap::new();
    for line in section.lines().filter(|l| l.starts_with("| `")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        let reason = cells[1].trim_matches('`').to_owned();
        let http: u16 = cells[4].parse().expect("http status");
        let row = (cells[2].to_owned(), cells[3].to_owned(), cells[5] == "yes");
        assert!(rows.insert(reason, (row, http)).is_none(), "duplicate row");
    }
    assert_eq!(rows.len(), REASONS.len(), "one SPEC row per reason");
    for info in REASONS {
        let (row, http) = rows
            .get(info.name())
            .unwrap_or_else(|| panic!("SPEC §12.2 has no row for {}", info.name()));
        assert_eq!(row, &expected(info), "{}", info.name());
        assert_eq!(*http, info.http_status(), "{}", info.name());
    }
}

#[test]
fn proto_comments_match_code() {
    let proto = repo_file("proto/bee/v1/common.proto");
    let body = proto
        .split("enum ErrorReason {")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .expect("ErrorReason enum");
    let mut rows = BTreeMap::new();
    let mut comment: Vec<String> = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(text) = line.strip_prefix("//") {
            comment.push(text.trim().to_owned());
            continue;
        }
        if let Some(name) = line
            .strip_prefix("ERROR_REASON_")
            .and_then(|l| l.split_whitespace().next())
            && name != "UNSPECIFIED"
        {
            let first = comment.first().expect("comment above value");
            let head = first.split('.').next().expect("head");
            let parts: Vec<&str> = head.split(" / ").map(str::trim).collect();
            assert_eq!(parts.len(), 3, "{name}: {first}");
            rows.insert(
                name.to_owned(),
                (
                    parts[0].to_owned(),
                    parts[1].to_owned(),
                    parts[2] == "retryable",
                ),
            );
        }
        comment.clear();
    }
    assert_eq!(rows.len(), REASONS.len());
    for info in REASONS {
        assert_eq!(
            rows.get(info.name()),
            Some(&expected(info)),
            "{}",
            info.name()
        );
    }
}
