//! Ergonomic aliases (`cr open|list|view|brief|checklist|evidence`, `auth token`) make the same calls as the generated
//! commands, and every ergonomic command has help with an example and a published schema.

use crate::support;

use bee_cli::schemas::cli_schema_path;
use serde_json::json;
use support::{FakeServer, Reply, bee, object_id, repo_root, sample_response, validate};

fn call(server: &FakeServer, args: &[&str]) -> support::Run {
    let mut all = args.to_vec();
    all.extend(["--json", "--endpoint", server.endpoint.as_str()]);
    bee(&all)
}

#[test]
fn cr_open_resolves_the_head_and_opens_a_change_request() {
    let server = FakeServer::start();
    let (_, id_json) = object_id();
    server.reply(
        "/bee.v1.QueryService/Resolve",
        Reply::Message(json!({"ids": [id_json.clone()], "statuses": ["ITEM_STATUS_OK"]})),
    );
    let run = call(
        &server,
        &[
            "cr", "open", "--repo", "acme/app", "--head", "main", "--title", "Fix it", "--draft",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "open"]), &run.json());
    // Opening is followed by a read of the checklist.
    assert_eq!(server.last().path, "/bee.v1.BriefService/GetBriefChecklist");
    let sent = server.received();
    let sent = sent
        .iter()
        .find(|r| r.path == "/bee.v1.ChangeService/OpenChange")
        .expect("OpenChange sent");
    let req = &sent.messages[0];
    assert_eq!(req["repo"]["fullName"], "acme/app");
    assert_eq!(req["head"], id_json);
    assert_eq!(req["title"], "Fix it");
    assert_eq!(req["draft"], true);
    assert_eq!(req["targetRef"], "main");
    let mut expected = sample_response("bee.v1.ChangeRequest");
    expected["checklist"] = sample_response("bee.v1.BriefChecklist");
    assert_eq!(run.json()["data"], expected);
}

#[test]
fn cr_list_and_view_map_to_list_changes_and_get_change() {
    let server = FakeServer::start();
    let run = call(
        &server,
        &["cr", "list", "--repo", "acme/app", "-q", "review:required"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "list"]), &run.json());
    let req = &server.last().messages[0];
    assert_eq!(server.last().path, "/bee.v1.ChangeService/ListChanges");
    assert_eq!(req["state"], "CHANGE_STATE_OPEN");
    assert_eq!(req["query"], "review:required");
    let run = call(
        &server,
        &[
            "cr",
            "view",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "view"]), &run.json());
    assert_eq!(server.last().path, "/bee.v1.ChangeService/GetChange");
    assert_eq!(
        server.last().messages[0]["id"],
        "01J9Z3M8Q4X7R2T5V6W8Y9A0BC"
    );
    // Outside a working copy without --repo: a usage error.
    let run = call(&server, &["cr", "list"]);
    assert_eq!(run.code, 2, "{run:?}");
}

#[test]
fn cr_open_sends_the_brief_and_returns_the_checklist() {
    let server = FakeServer::start();
    let (_, id_json) = object_id();
    server.reply(
        "/bee.v1.QueryService/Resolve",
        Reply::Message(json!({"ids": [id_json], "statuses": ["ITEM_STATUS_OK"]})),
    );
    let run = call(
        &server,
        &[
            "cr",
            "open",
            "--repo",
            "acme/app",
            "--head",
            "main",
            "--title",
            "Faster search",
            "--outcome",
            "Search answers in under 100 ms",
            "--highlight",
            "Cache query plans::src/search/plan.rs, src/search/cache.rs",
            "--risk",
            "Stale results::the cache expires after 60 s",
            "--ask",
            "Ship the cache?::yes|no",
            "--rollback",
            "Revert the change",
            "--try-it",
            "https://preview.example/search",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let doc = run.json();
    validate(&cli_schema_path(&["cr", "open"]), &doc);
    let sent = server.received();
    let open = sent
        .iter()
        .find(|r| r.path == "/bee.v1.ChangeService/OpenChange")
        .expect("OpenChange sent");
    let brief = &open.messages[0]["brief"];
    assert_eq!(brief["outcome"], "Search answers in under 100 ms");
    assert_eq!(
        brief["highlights"],
        json!([{"text": "Cache query plans", "paths": ["src/search/plan.rs", "src/search/cache.rs"]}])
    );
    assert_eq!(
        brief["risks"],
        json!([{"text": "Stale results", "mitigation": "the cache expires after 60 s"}])
    );
    assert_eq!(
        brief["asks"],
        json!([{"question": "Ship the cache?", "options": ["yes", "no"]}])
    );
    assert_eq!(brief["rollback"], "Revert the change");
    assert_eq!(brief["tryItUrl"], "https://preview.example/search");
    assert_eq!(server.last().path, "/bee.v1.BriefService/GetBriefChecklist");
    assert_eq!(
        server.last().messages[0]["changeId"],
        sample_response("bee.v1.ChangeRequest")["id"]
    );
    assert_eq!(
        doc["data"]["checklist"],
        sample_response("bee.v1.BriefChecklist")
    );
    // Without a brief nothing is invented: the field is not sent.
    let run = call(
        &server,
        &[
            "cr", "open", "--repo", "acme/app", "--head", "main", "--title", "Plain",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let sent = server.received();
    let open = sent
        .iter()
        .rev()
        .find(|r| r.path == "/bee.v1.ChangeService/OpenChange")
        .expect("OpenChange sent");
    assert!(open.messages[0].get("brief").is_none(), "{open:?}");
    // A bad ask is a usage error and sends nothing.
    let before = server.received().len();
    let run = call(
        &server,
        &[
            "cr",
            "open",
            "--repo",
            "acme/app",
            "--title",
            "x",
            "--ask",
            "Only one option?::yes",
        ],
    );
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(server.received().len(), before);
}

#[test]
fn cr_brief_and_checklist_map_to_the_brief_service() {
    let server = FakeServer::start();
    let dir = tempfile::tempdir().expect("tmp");
    let file = dir.path().join("brief.json");
    std::fs::write(
        &file,
        r#"{"outcome": "from the file", "highlights": [{"text": "kept", "paths": ["a.rs"]}]}"#,
    )
    .expect("write");
    let run = call(
        &server,
        &[
            "cr",
            "brief",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--from",
            file.to_str().expect("utf8"),
            "--outcome",
            "from the flag",
            "--highlight",
            "added",
            "--expected-version",
            "4",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "brief"]), &run.json());
    let sent = server.last();
    assert_eq!(sent.path, "/bee.v1.BriefService/SetChangeBrief");
    let req = &sent.messages[0];
    assert_eq!(req["repo"]["fullName"], "acme/app");
    assert_eq!(req["changeId"], "01J9Z3M8Q4X7R2T5V6W8Y9A0BC");
    assert_eq!(req["expectedVersion"], "4");
    assert_eq!(req["brief"]["outcome"], "from the flag");
    assert_eq!(
        req["brief"]["highlights"],
        json!([{"text": "kept", "paths": ["a.rs"]}, {"text": "added", "paths": []}])
    );
    // A brief with nothing in it is refused before anything is sent.
    let before = server.received().len();
    let run = call(&server, &["cr", "brief", "12", "--repo", "acme/app"]);
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(server.received().len(), before);

    // A number resolves to the change request first.
    let run = call(
        &server,
        &[
            "cr",
            "checklist",
            "#12",
            "--repo",
            "acme/app",
            "--revision",
            "2",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "checklist"]), &run.json());
    let sent = server.received();
    let n = sent.len();
    assert_eq!(sent[n - 2].path, "/bee.v1.ChangeService/GetChangeByNumber");
    assert_eq!(sent[n - 2].messages[0]["number"], "12");
    assert_eq!(sent[n - 1].path, "/bee.v1.BriefService/GetBriefChecklist");
    assert_eq!(
        sent[n - 1].messages[0]["changeId"],
        sample_response("bee.v1.ChangeRequest")["id"]
    );
    assert_eq!(sent[n - 1].messages[0]["revision"], 2);
}

/// A 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn cr_evidence_adds_lists_and_removes() {
    use base64::Engine as _;
    let server = FakeServer::start();
    let dir = tempfile::tempdir().expect("tmp");
    let png = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("png");
    let before = dir.path().join("before.png");
    let after = dir.path().join("after.png");
    std::fs::write(&before, &png).expect("write");
    std::fs::write(&after, &png).expect("write");
    let (before, after) = (
        before.to_str().expect("utf8"),
        after.to_str().expect("utf8"),
    );

    // Two images with --before/--after are a before and after, sent inline.
    let run = call(
        &server,
        &[
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--before",
            before,
            "--after",
            after,
            "--alt",
            "The checkout page",
            "--after-alt",
            "The checkout page with the new total",
            "--hero",
            "--revision",
            "3",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "evidence"]), &run.json());
    let sent = server.last();
    assert_eq!(sent.path, "/bee.v1.BriefService/AddEvidence");
    let req = &sent.messages[0];
    assert_eq!(req["spec"]["kind"], "EVIDENCE_KIND_BEFORE_AFTER");
    assert_eq!(req["spec"]["title"], "before");
    assert_eq!(req["spec"]["hero"], true);
    assert_eq!(req["revision"], 3);
    let assets = req["assets"].as_array().expect("assets");
    assert_eq!(assets.len(), 2);
    assert_eq!(assets[0]["role"], "EVIDENCE_ASSET_ROLE_BEFORE");
    assert_eq!(assets[0]["contentType"], "image/png");
    assert_eq!(assets[0]["data"], PNG);
    assert_eq!(assets[0]["alt"], "The checkout page");
    assert_eq!(assets[1]["role"], "EVIDENCE_ASSET_ROLE_AFTER");
    assert_eq!(assets[1]["alt"], "The checkout page with the new total");

    // Numbers need no files.
    let run = call(
        &server,
        &[
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--bench",
            "p95 latency:ms:120:85:lower",
            "--bench",
            "throughput:req/s:900:1200:higher",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let spec = &server.last().messages[0]["spec"];
    assert_eq!(spec["kind"], "EVIDENCE_KIND_BENCHMARK");
    assert_eq!(spec["benchmark"][0]["name"], "p95 latency");
    assert_eq!(spec["benchmark"][0]["baseline"], 120.0);
    assert_eq!(spec["benchmark"][0]["lowerIsBetter"], true);
    assert_eq!(spec["benchmark"][1]["lowerIsBetter"], false);
    assert!(
        server.last().messages[0]["assets"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );

    let run = call(
        &server,
        &[
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--tests",
            "214:0:3:61000",
            "--summary",
            "All 214 billing tests pass",
            "--check-run",
            "cr_1",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let req = &server.last().messages[0];
    assert_eq!(req["spec"]["kind"], "EVIDENCE_KIND_TEST_REPORT");
    assert_eq!(req["spec"]["testReport"]["passed"], 214);
    assert_eq!(req["spec"]["testReport"]["durationMs"], "61000");
    assert_eq!(req["checkRunId"], "cr_1");

    // From a workflow artifact the names are members of the artifact, nothing is read.
    let run = call(
        &server,
        &[
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--artifact",
            "art_1",
            "--kind",
            "screenshot",
            "--alt",
            "The home page",
            "shots/home.png",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let req = &server.last().messages[0];
    assert_eq!(req["workflowArtifactId"], "art_1");
    assert_eq!(
        req["artifactAssets"],
        json!([{"role": "EVIDENCE_ASSET_ROLE_MAIN", "path": "shots/home.png", "alt": "The home page"}])
    );

    // Nothing to attach, an unknown file type and a bad number are usage errors.
    let before_count = server.received().len();
    for args in [
        vec!["cr", "evidence", "add", "12", "--repo", "acme/app"],
        vec![
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "logo.svg",
        ],
        vec![
            "cr",
            "evidence",
            "add",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--bench",
            "p95:ms:120",
        ],
    ] {
        let run = call(&server, &args);
        assert_eq!(run.code, 2, "{args:?}: {run:?}");
    }
    assert_eq!(server.received().len(), before_count);

    let run = call(
        &server,
        &[
            "cr",
            "evidence",
            "list",
            "01J9Z3M8Q4X7R2T5V6W8Y9A0BC",
            "--repo",
            "acme/app",
            "--hero-only",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "evidence"]), &run.json());
    let sent = server.last();
    assert_eq!(sent.path, "/bee.v1.BriefService/ListEvidence");
    assert_eq!(
        sent.messages[0]["changeIds"],
        json!(["01J9Z3M8Q4X7R2T5V6W8Y9A0BC"])
    );
    assert_eq!(sent.messages[0]["heroOnly"], true);

    let run = call(
        &server,
        &["cr", "evidence", "rm", "01JEVIDENCE", "--repo", "acme/app"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["cr", "evidence"]), &run.json());
    let sent = server.last();
    assert_eq!(sent.path, "/bee.v1.BriefService/DeleteEvidence");
    assert_eq!(sent.messages[0]["id"], "01JEVIDENCE");
}

#[test]
fn auth_token_mints_an_attenuated_token() {
    let server = FakeServer::start();
    let run = call(
        &server,
        &[
            "auth",
            "token",
            "--action",
            "read",
            "--action",
            "commit",
            "--ref",
            "ws/*",
            "--write-path",
            "src/",
            "--expires-in",
            "8h",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate(&cli_schema_path(&["auth", "token"]), &run.json());
    let sent = server.last();
    assert_eq!(sent.path, "/bee.v1.AuthService/MintToken");
    let scope = &sent.messages[0]["scope"];
    assert_eq!(scope["actions"], json!(["read", "commit"]));
    assert_eq!(scope["refs"], json!(["ws/*"]));
    assert_eq!(scope["writePaths"], json!(["c3Jj"]));
    assert!(scope["expiresAt"].as_str().expect("expiry").ends_with('Z'));
}

#[test]
fn every_ergonomic_command_has_help_an_example_and_a_schema() {
    for command in bee_cli::wc::commands() {
        let path = command.path();
        let mut args: Vec<&str> = path.to_vec();
        args.push("--help");
        let run = bee(&args);
        assert_eq!(run.code, 0, "{path:?}: {run:?}");
        assert!(
            run.stdout.contains("EXAMPLE:"),
            "{path:?} help lacks an example"
        );
        assert!(
            run.stdout.contains(&format!("bee {}", path.join(" "))),
            "{path:?} example does not show the command"
        );
        let file = cli_schema_path(path);
        assert!(repo_root().join(&file).is_file(), "{file}");
    }
    // Visible aliases.
    assert_eq!(bee(&["rebase", "--help"]).code, 0);
    assert_eq!(bee(&["checkout", "--help"]).code, 0);
}

/// Splits a command line the way a shell would, enough for the prompts: quotes group, a
/// backslash before a newline continues the line, `<placeholders>` become `x`.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            // A comment runs to the end of the line.
            (None, '#') if !started => break,
            (None, '\\') if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (_, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    // `a && b`: only the first command is the one under test.
    if let Some(end) = out
        .iter()
        .position(|w| matches!(w.as_str(), "&&" | "||" | ";"))
    {
        out.truncate(end);
    }
    out
}

/// Every `bee ...` command in the fenced blocks and inline code of a prompt, as words.
fn cited_commands(text: &str) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    let mut fenced = false;
    let mut block = String::new();
    let mut texts: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if fenced {
                texts.push(std::mem::take(&mut block));
            }
            fenced = !fenced;
        } else if fenced {
            block.push_str(line);
            block.push('\n');
        } else {
            let spans = line.split('`').skip(1).step_by(2);
            for span in spans {
                // A bare command name in prose (`bee land`) is a reference, not a command line.
                if span.starts_with("bee ") && span.contains(" --") {
                    texts.push(span.to_owned());
                }
            }
        }
    }
    for block in texts {
        // Join continued lines, then take each line that runs `bee`.
        let joined = block.replace("\\\n", " ");
        for line in joined.lines() {
            let line = line.split(" | ").last().unwrap_or(line).trim();
            if let Some(rest) = line.strip_prefix("bee ") {
                let mut rest = rest
                    .replace("{{repo}}", "acme/app")
                    .replace("{{endpoint}}", "http://127.0.0.1:1");
                while let (Some(a), Some(b)) = (rest.find('<'), rest.find('>')) {
                    if a >= b {
                        break;
                    }
                    rest.replace_range(a..=b, "x");
                }
                let mut w = vec!["bee".to_owned()];
                w.extend(words(&rest));
                found.push(w);
            }
        }
    }
    found
}

#[test]
fn every_command_the_prompts_cite_parses() {
    use bee_cli::cli::{Expand, root};
    let dir = repo_root().join("docs/prompts");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("prompts") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read");
        for command in cited_commands(&text) {
            let args = command;
            // `--help` and `--version` end parsing early with a "success" kind of error.
            let parsed = root(Expand::All).try_get_matches_from(args.clone());
            if let Err(e) = parsed {
                assert!(
                    matches!(
                        e.kind(),
                        clap::error::ErrorKind::DisplayHelp
                            | clap::error::ErrorKind::DisplayVersion
                    ),
                    "{}: `{}` does not parse: {e}",
                    path.display(),
                    args.join(" ")
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 60, "only {checked} commands were found");
}
