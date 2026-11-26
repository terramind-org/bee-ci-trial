//! Three-form conformance (PRODUCT §3.2, SPEC §12.4): every served RPC is called as the
//! HTTP/JSON API (gRPC for reads), as `bee <noun> <verb> --json` and as the MCP tool
//! `<service>_<method>` (stdio `bee mcp` sampled), each on its own copy of one seeded
//! `bee-server --dev`, and the outcomes must be equal after normalisation.
//!
//! Cases (run all, or name some: `cargo test -p bee-conformance --test three_forms -- <name>`):
//!
//! - `every_served_rpc_has_a_fixture`: the fixtures cover exactly the served RPCs, and every
//!   served RPC has its CLI command, output schema and MCP name by the SPEC rules.
//! - `mcp_tools_list_matches_the_descriptor`: `tools/list` (toolset `all`) lists every served
//!   RPC with the request message's schema as `inputSchema`.
//! - `unserved_rpcs_answer_unimplemented_in_every_form`: every RPC on the allow-list answers
//!   `UNIMPLEMENTED` with the same error document as API, CLI and MCP.
//! - `three_forms_agree`: the matrix. By default a representative set of services
//!   ([`DEFAULT_SERVICES`]); every service with `BEE_CONFORMANCE_FULL=1`.
//! - `verification_report_lists_every_served_rpc`: `docs/spec/verification.md` has a row per
//!   served RPC (the full run regenerates it with `BEE_CONFORMANCE_REPORT=1`).
//!
//! See `docs/spec/verification.md` for the environment variables.

mod api;
mod cli;
mod mcp;
mod run;
mod servers;
mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use bee_conformance::parity::contract::AllowList;
use bee_conformance::parity::naming::{
    cli_noun, cli_schema_file, cli_verb, mcp_tool, service_short,
};
use bee_conformance::parity::report::{self, Row, Status};
use serde_json::Value;

use crate::run::{MethodResult, Settings, ToolSchemas};

/// Services the default (gate) run covers: the VCS core, the most used forge surfaces, and
/// one service per streaming shape. The full matrix runs with `BEE_CONFORMANCE_FULL=1`.
pub const DEFAULT_SERVICES: &[&str] = &[
    "ObjectService",
    "QueryService",
    "RefService",
    "WriteService",
    "LeaseService",
    "EventService",
    "ChangeService",
    "IssueService",
    "SwarmService",
    "SafetyService",
];

/// One case of the suite: a summary on success, the failures otherwise.
type Case = fn() -> Result<String, String>;

fn main() {
    if std::env::var_os(cli::AS_CLI).is_some() {
        // Acting as `bee`: exactly what crates/bee-cli/src/main.rs does.
        let code = bee_cli::run(std::env::args_os());
        std::process::exit(code);
    }
    if let Ok(service) = std::env::var(worker::WORKER) {
        std::process::exit(worker::worker_main(&service, settings()));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let cases: [(&str, Case); 5] = [
        (
            "every_served_rpc_has_a_fixture",
            every_served_rpc_has_a_fixture,
        ),
        (
            "mcp_tools_list_matches_the_descriptor",
            mcp_tools_list_matches_the_descriptor,
        ),
        (
            "unserved_rpcs_answer_unimplemented_in_every_form",
            unserved_rpcs_answer_unimplemented_in_every_form,
        ),
        ("three_forms_agree", three_forms_agree),
        (
            "verification_report_lists_every_served_rpc",
            verification_report_lists_every_served_rpc,
        ),
    ];
    if args.iter().any(|a| a == "--list") {
        for (name, _) in &cases {
            println!("{name}: test");
        }
        return;
    }
    let selected: Vec<_> = cases
        .iter()
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|f| name.contains(f.as_str())))
        .collect();
    println!("\nrunning {} tests", selected.len());
    let mut failed = Vec::new();
    for (name, case) in &selected {
        let start = Instant::now();
        let result = std::panic::catch_unwind(case);
        let secs = start.elapsed().as_secs_f64();
        match result {
            Ok(Ok(summary)) => println!("test {name} ... ok ({secs:.1}s) {summary}"),
            Ok(Err(e)) => {
                println!("test {name} ... FAILED ({secs:.1}s)\n{e}");
                failed.push(*name);
            }
            Err(_) => {
                println!("test {name} ... FAILED ({secs:.1}s): panicked");
                failed.push(*name);
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime")
}

fn allow_list() -> Result<AllowList, String> {
    let path = cli::repo_root().join("crates/bee-server/tests/forge_services.rs");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    AllowList::parse(&text)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

fn every_served_rpc_has_a_fixture() -> Result<String, String> {
    let allow = allow_list()?;
    let served = allow.served();
    let (_, files) = run::load()?;
    let mut problems = Vec::new();
    let mut fixtures: BTreeSet<String> = BTreeSet::new();
    for (service, (_, file)) in &files {
        for f in &file.fixtures {
            let path = format!("/{service}/{}", f.method);
            match bee_proto::methods::find_method(&path) {
                None => problems.push(format!(
                    "fixture for {path}, which is not in the descriptor"
                )),
                Some(m) if !allow.serves(m) => problems.push(format!(
                    "fixture for {path}, which is on the UNIMPLEMENTED allow-list"
                )),
                Some(_) => {
                    fixtures.insert(path);
                }
            }
        }
    }
    let catalog = bee_cli::catalog::catalog();
    for m in &served {
        if !fixtures.contains(m.path) {
            problems.push(format!(
                "{} is served but has no fixture (add it to crates/bee-conformance/fixtures/{}.json)",
                m.path,
                cli_noun(m.service)
            ));
        }
        // The CLI command and output schema, by the SPEC naming rules.
        let noun = cli_noun(m.service);
        let verb = cli_verb(m.method);
        match catalog.noun(&noun).and_then(|n| n.verb(&verb)) {
            Some(v) if v.method.path == m.path => {}
            _ => problems.push(format!("{}: no `bee {noun} {verb}`", m.path)),
        }
        if !cli::repo_root()
            .join(cli_schema_file(m.service, m.method))
            .is_file()
        {
            problems.push(format!(
                "{}: no {}",
                m.path,
                cli_schema_file(m.service, m.method)
            ));
        }
    }
    if problems.is_empty() {
        Ok(format!(
            "({} served RPCs, {} fixtures, {} on the allow-list)",
            served.len(),
            fixtures.len(),
            bee_proto::methods::METHODS.len() - served.len()
        ))
    } else {
        Err(problems.join("\n"))
    }
}

/// Every tool's input schema from a dev server's `tools/list`.
fn tool_schemas(rt: &tokio::runtime::Runtime) -> Result<ToolSchemas, String> {
    rt.block_on(async {
        let server = servers::start_on(tempfile::tempdir().expect("tempdir")).await;
        let http = api::client();
        let tools = mcp::list_all_tools(&http, &server.url, &server.token).await;
        server.stop().await;
        let mut out = BTreeMap::new();
        for t in tools? {
            out.insert(
                t["name"].as_str().unwrap_or_default().to_owned(),
                t["inputSchema"].clone(),
            );
        }
        Ok(Arc::new(out))
    })
}

fn mcp_tools_list_matches_the_descriptor() -> Result<String, String> {
    let allow = allow_list()?;
    let rt = runtime();
    let schemas = tool_schemas(&rt)?;
    let mut problems = Vec::new();
    for m in allow.served() {
        let tool = mcp_tool(m.service, m.method);
        let Some(schema) = schemas.get(&tool) else {
            problems.push(format!("{}: tools/list has no {tool}", m.path));
            continue;
        };
        let file =
            cli::repo_root().join(format!("schemas/v1/messages/{}.schema.json", m.input_type));
        let Ok(text) = std::fs::read_to_string(&file) else {
            problems.push(format!("{}: no {}", m.path, file.display()));
            continue;
        };
        let message: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let mut got = schema.clone();
        match m.kind {
            bee_proto::methods::MethodKind::Unary => {}
            bee_proto::methods::MethodKind::ServerStreaming => {
                // Two collection arguments that are not sent to the server.
                if let Some(p) = got.get_mut("properties").and_then(Value::as_object_mut) {
                    for extra in ["max_items", "timeout_ms"] {
                        if p.remove(extra).is_none() {
                            problems.push(format!("{tool}: no `{extra}` argument"));
                        }
                    }
                }
            }
            _ => {
                // `{"messages": [<request>...]}` with the request schema under $defs.
                let item = &got["properties"]["messages"]["items"]["$ref"];
                let want = format!("#/$defs/{}", m.input_type);
                if item.as_str() != Some(want.as_str()) {
                    problems.push(format!("{tool}: messages items are {item}, want {want}"));
                }
                continue;
            }
        }
        // MCP accepts a compact string spelling for RepoRef in addition to the
        // published proto object schema. Compare the shared proto form here;
        // the MCP crate covers the added spelling directly.
        if let Some(repo_ref) = got
            .get_mut("$defs")
            .and_then(Value::as_object_mut)
            .and_then(|defs| defs.get_mut("bee.v1.RepoRef"))
            && let Some(object) = repo_ref
                .get("anyOf")
                .and_then(Value::as_array)
                .and_then(|forms| forms.first())
                .cloned()
        {
            *repo_ref = object;
        }
        if let Some(d) = bee_conformance::parity::normalize::first_difference(&message, &got) {
            problems.push(format!(
                "{tool}: inputSchema differs from {} at {d}",
                file.file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
        }
    }
    if problems.is_empty() {
        Ok(format!("({} tools checked)", allow.served().len()))
    } else {
        Err(problems.join("\n"))
    }
}

fn settings() -> Settings {
    let list = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| {
                v.split(',')
                    .map(|s| {
                        let s = s.trim();
                        if s.ends_with("Service") || s.contains('/') {
                            s.to_owned()
                        } else {
                            format!("{s}Service")
                        }
                    })
                    .collect::<Vec<_>>()
            })
    };
    let services = list("BEE_CONFORMANCE_SERVICES").or_else(|| {
        (!env_flag("BEE_CONFORMANCE_FULL"))
            .then(|| DEFAULT_SERVICES.iter().map(|s| s.to_string()).collect())
    });
    let methods = std::env::var("BEE_CONFORMANCE_METHODS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect());
    Settings {
        services,
        probe: std::env::var("BEE_CONFORMANCE_FORMS").is_ok_and(|v| v == "api"),
        jobs: std::env::var("BEE_CONFORMANCE_JOBS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4)
            .max(1),
        verbose: env_flag("BEE_CONFORMANCE_VERBOSE"),
        methods,
    }
}

fn three_forms_agree() -> Result<String, String> {
    let settings = settings();
    let allow = allow_list()?;
    let (scenario, files) = run::load()?;
    let rt = runtime();
    let tools = if settings.probe {
        Arc::new(BTreeMap::new())
    } else {
        tool_schemas(&rt)?
    };
    let http = api::client();
    let start = Instant::now();
    let (seed, globals) = rt.block_on(run::seed(&http, &scenario))?;
    let seeded = start.elapsed().as_secs_f64();
    let state = tempfile::tempdir().map_err(|e| e.to_string())?;
    worker::write_state(state.path(), &globals, &tools)?;
    let selected: Vec<String> = files
        .values()
        .filter(|(_, f)| {
            settings.services.as_ref().is_none_or(|only| {
                only.iter()
                    .any(|s| *s == format!("{}Service", service_short(&f.service)))
            })
        })
        .map(|(_, f)| f.service.clone())
        .collect();
    let relay = settings.verbose || settings.probe;
    let results: Vec<MethodResult> = rt.block_on(async {
        let gate = Arc::new(tokio::sync::Semaphore::new(settings.jobs));
        let mut set = tokio::task::JoinSet::new();
        for (i, service) in selected.into_iter().enumerate() {
            let gate = gate.clone();
            let seed = seed.path().to_path_buf();
            let state = state.path().to_path_buf();
            set.spawn(async move {
                let _permit = gate.acquire_owned().await.expect("semaphore");
                (i, worker::spawn_group(&service, &seed, &state, relay).await)
            });
        }
        let mut groups = Vec::new();
        while let Some(r) = set.join_next().await {
            match r {
                Ok(g) => groups.push(g),
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            }
        }
        groups.sort_by_key(|g| g.0);
        let mut all = Vec::new();
        let mut errors = Vec::new();
        for (_, g) in groups {
            match g {
                Ok(rs) => all.extend(rs),
                Err(e) => errors.push(e),
            }
        }
        if errors.is_empty() {
            Ok(all)
        } else {
            Err(errors.join("\n"))
        }
    })?;
    let elapsed = start.elapsed().as_secs_f64();
    let rows: Vec<Row> = results.iter().map(|r| r.row.clone()).collect();
    let counts = report::counts(&rows);
    if !settings.probe && env_flag("BEE_CONFORMANCE_REPORT") {
        if settings.services.is_some() || settings.methods.is_some() {
            return Err(
                "BEE_CONFORMANCE_REPORT needs the full matrix (BEE_CONFORMANCE_FULL=1, no filters)"
                    .into(),
            );
        }
        let served = allow.served().len();
        let generated = report::render(&rows, served, bee_proto::methods::METHODS.len() - served);
        let path = cli::repo_root().join("docs/spec/verification.md");
        let doc = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        std::fs::write(&path, report::splice(&doc, &generated)?).map_err(|e| e.to_string())?;
    }
    let summary = format!(
        "({} methods in {elapsed:.0}s, seed {seeded:.1}s: api {}, cli {}, mcp {}, all three {})",
        counts.methods, counts.api, counts.cli, counts.mcp, counts.all
    );
    if settings.probe {
        return Ok(summary);
    }
    let failing: Vec<&MethodResult> = results
        .iter()
        .filter(|r| [r.row.api, r.row.cli, r.row.mcp].contains(&Status::Fail))
        .collect();
    if failing.is_empty() {
        Ok(summary)
    } else {
        let mut text = format!("{summary}\n{} methods fail:\n", failing.len());
        for r in failing {
            text.push_str(&format!("- {}/{}:\n", r.row.service, r.row.method));
            for f in &r.failures {
                text.push_str(&format!("    {f}\n"));
            }
        }
        Err(text)
    }
}

fn verification_report_lists_every_served_rpc() -> Result<String, String> {
    let allow = allow_list()?;
    let path = cli::repo_root().join("docs/spec/verification.md");
    let doc = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let listed: BTreeSet<(String, String)> = report::listed_methods(&doc).into_iter().collect();
    let want: BTreeSet<(String, String)> = allow
        .served()
        .iter()
        .map(|m| {
            (
                format!("{}Service", service_short(m.service)),
                m.method.to_owned(),
            )
        })
        .collect();
    let missing: Vec<_> = want.difference(&listed).collect();
    let extra: Vec<_> = listed.difference(&want).collect();
    if missing.is_empty() && extra.is_empty() {
        Ok(format!("({} rows)", listed.len()))
    } else {
        Err(format!(
            "docs/spec/verification.md is stale; regenerate with BEE_CONFORMANCE_FULL=1 \
             BEE_CONFORMANCE_REPORT=1.\nmissing: {missing:?}\nextra: {extra:?}"
        ))
    }
}

fn unserved_rpcs_answer_unimplemented_in_every_form() -> Result<String, String> {
    use bee_conformance::parity::normalize::first_difference;
    let allow = allow_list()?;
    let unserved: Vec<&'static bee_proto::methods::MethodDescriptor> = bee_proto::methods::METHODS
        .iter()
        .filter(|m| !allow.serves(m))
        .collect();
    let rt = runtime();
    let problems = rt.block_on(async {
        let server = servers::start_on(tempfile::tempdir().expect("tempdir")).await;
        let http = api::client();
        let mut session = mcp::HttpSession::open(&http, &server.url, &server.token).await?;
        let gate = Arc::new(tokio::sync::Semaphore::new(8));
        let mut cli_calls = tokio::task::JoinSet::new();
        for m in &unserved {
            let (m, url, token, dir, gate) = (
                *m,
                server.url.clone(),
                server.token.clone(),
                server.cli_config.path().to_path_buf(),
                gate.clone(),
            );
            cli_calls.spawn(async move {
                let _permit = gate.acquire_owned().await.expect("semaphore");
                let request = empty_request(m);
                (m.path, cli::call(&url, &token, &dir, m, &request).await)
            });
        }
        let mut cli_results = BTreeMap::new();
        while let Some(r) = cli_calls.join_next().await {
            let (path, r) = r.map_err(|e| e.to_string())?;
            cli_results.insert(path, r);
        }
        let norm = server.normalizer();
        let mut problems = Vec::new();
        for m in &unserved {
            let request = empty_request(m);
            let a = api::call(&http, &server.url, &server.token, m, &request, None).await;
            let c = &cli_results[m.path];
            let p = session.call(&http, m, &request, None).await;
            let want = norm.outcome(&a.outcome, &[]);
            for (form, outcome, extra) in [
                ("api", &a.outcome, &a.problems),
                ("cli", &c.outcome, &c.problems),
                ("mcp", &p.outcome, &p.problems),
            ] {
                if outcome.label() != "UNIMPLEMENTED" {
                    problems.push(format!(
                        "{} {form}: {}",
                        m.path,
                        servers::describe_outcome(outcome)
                    ));
                }
                for e in extra {
                    problems.push(format!("{} {form}: {e}", m.path));
                }
                if let Some(d) = first_difference(&want, &norm.outcome(outcome, &[])) {
                    problems.push(format!("{} {form} differs from api at {d}", m.path));
                }
            }
        }
        server.stop().await;
        Ok::<_, String>(problems)
    })?;
    if problems.is_empty() {
        Ok(format!("({} unserved RPCs)", unserved.len()))
    } else {
        Err(problems.join("\n"))
    }
}

/// `{}`, or one empty message for client streams.
fn empty_request(m: &bee_proto::methods::MethodDescriptor) -> Value {
    if m.kind == bee_proto::methods::MethodKind::ClientStreaming {
        serde_json::json!([{}])
    } else {
        serde_json::json!({})
    }
}
