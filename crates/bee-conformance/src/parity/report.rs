//! The coverage report: services × methods × {api, cli, mcp}, rendered as Markdown and kept
//! in `docs/spec/verification.md` between two marker lines.

use std::collections::BTreeMap;

/// Line that starts the generated part of the document.
pub const BEGIN: &str = "<!-- BEGIN GENERATED: three-form conformance matrix -->";
/// Line that ends the generated part of the document.
pub const END: &str = "<!-- END GENERATED: three-form conformance matrix -->";

/// The result of one form for one method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The form agreed with the expectation and the other forms, and its contract checks held.
    Pass,
    /// It did not.
    Fail,
    /// The form did not run (the run was filtered or stopped earlier).
    NotRun,
}

impl Status {
    fn cell(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "**FAIL**",
            Status::NotRun => "-",
        }
    }
}

/// One method's row.
#[derive(Clone, Debug)]
pub struct Row {
    /// Service short name, e.g. `IssueService`.
    pub service: String,
    /// Method name.
    pub method: String,
    /// `ok` or the reason the fixture expects.
    pub expect: String,
    /// API form (HTTP/JSON, plus gRPC where sampled).
    pub api: Status,
    /// CLI form.
    pub cli: Status,
    /// MCP form.
    pub mcp: Status,
}

/// Totals of a report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Rows.
    pub methods: usize,
    /// Rows expecting success.
    pub expect_ok: usize,
    /// Passing API cells.
    pub api: usize,
    /// Passing CLI cells.
    pub cli: usize,
    /// Passing MCP cells.
    pub mcp: usize,
    /// Rows where all three pass.
    pub all: usize,
}

/// Counts passing cells.
pub fn counts(rows: &[Row]) -> Counts {
    let mut c = Counts {
        methods: rows.len(),
        ..Counts::default()
    };
    for r in rows {
        c.expect_ok += usize::from(r.expect == "ok");
        c.api += usize::from(r.api == Status::Pass);
        c.cli += usize::from(r.cli == Status::Pass);
        c.mcp += usize::from(r.mcp == Status::Pass);
        c.all += usize::from([r.api, r.cli, r.mcp].iter().all(|s| *s == Status::Pass));
    }
    c
}

/// The generated Markdown: a summary, a per-service table, and the per-method matrix.
pub fn render(rows: &[Row], served: usize, unserved: usize) -> String {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| (&a.service, &a.method).cmp(&(&b.service, &b.method)));
    let total = counts(&rows);
    let mut out = String::new();
    out.push_str(BEGIN);
    out.push_str("\n\n");
    out.push_str(&format!(
        "RPCs in the descriptor: {}. Served: {served}; on the UNIMPLEMENTED allow-list: \
         {unserved}. Fixtures: {} ({} expect success, {} expect a documented error).\n\n",
        served + unserved,
        total.methods,
        total.expect_ok,
        total.methods - total.expect_ok
    ));
    out.push_str("| Form | Passing | Of |\n|---|---:|---:|\n");
    for (name, n) in [
        ("API (HTTP/JSON; gRPC sampled)", total.api),
        ("CLI (`bee <noun> <verb> --json`)", total.cli),
        ("MCP (`/mcp` tools/call; stdio sampled)", total.mcp),
        ("All three", total.all),
    ] {
        out.push_str(&format!("| {name} | {n} | {} |\n", total.methods));
    }
    out.push_str("\n### By service\n\n");
    out.push_str("| Service | Methods | Expect ok | API | CLI | MCP |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|\n");
    let mut by_service: BTreeMap<&str, Vec<Row>> = BTreeMap::new();
    for r in &rows {
        by_service.entry(&r.service).or_default().push(r.clone());
    }
    for (service, list) in &by_service {
        let c = counts(list);
        out.push_str(&format!(
            "| {service} | {} | {} | {} | {} | {} |\n",
            c.methods, c.expect_ok, c.api, c.cli, c.mcp
        ));
    }
    out.push_str("\n### By method\n\n");
    out.push_str("| Service | Method | Expect | API | CLI | MCP |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for r in &rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.service,
            r.method,
            if r.expect == "ok" {
                "ok".to_owned()
            } else {
                format!("`{}`", r.expect)
            },
            r.api.cell(),
            r.cli.cell(),
            r.mcp.cell()
        ));
    }
    out.push('\n');
    out.push_str(END);
    out
}

/// Replaces the generated part of `doc` (between [`BEGIN`] and [`END`]) with `generated`.
pub fn splice(doc: &str, generated: &str) -> Result<String, String> {
    let start = doc
        .find(BEGIN)
        .ok_or_else(|| format!("missing marker {BEGIN}"))?;
    let end = doc
        .find(END)
        .ok_or_else(|| format!("missing marker {END}"))?
        + END.len();
    if end < start {
        return Err("markers out of order".into());
    }
    Ok(format!("{}{generated}{}", &doc[..start], &doc[end..]))
}

/// The `(service, method)` pairs listed in the generated part of `doc`.
pub fn listed_methods(doc: &str) -> Vec<(String, String)> {
    let (Some(start), Some(end)) = (doc.find(BEGIN), doc.find(END)) else {
        return Vec::new();
    };
    let body = &doc[start..end];
    let Some(matrix) = body.split("### By method").nth(1) else {
        return Vec::new();
    };
    matrix
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            (cells.len() >= 7 && cells[1] != "Service" && !cells[1].starts_with("---"))
                .then(|| (cells[1].to_owned(), cells[2].to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(service: &str, method: &str, expect: &str, cli: Status) -> Row {
        Row {
            service: service.into(),
            method: method.into(),
            expect: expect.into(),
            api: Status::Pass,
            cli,
            mcp: Status::Pass,
        }
    }

    #[test]
    fn renders_counts_and_round_trips() {
        let rows = vec![
            row("IssueService", "ListIssues", "ok", Status::Pass),
            row("IssueService", "GetIssue", "NOT_FOUND", Status::Fail),
            row("AuthService", "WhoAmI", "ok", Status::Pass),
        ];
        let c = counts(&rows);
        assert_eq!(
            (c.methods, c.expect_ok, c.api, c.cli, c.all),
            (3, 2, 3, 2, 2)
        );
        let text = render(&rows, 3, 2);
        assert!(
            text.contains("| IssueService | GetIssue | `NOT_FOUND` | pass | **FAIL** | pass |")
        );
        let doc = format!("# Title\n\n{BEGIN}\nold\n{END}\n\ntail\n");
        let new = splice(&doc, &text).expect("splices");
        assert!(new.starts_with("# Title\n\n") && new.ends_with("\n\ntail\n"));
        let listed = listed_methods(&new);
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0], ("AuthService".into(), "WhoAmI".into()));
        assert!(splice("no markers", &text).is_err());
    }
}
