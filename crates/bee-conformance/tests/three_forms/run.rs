//! Running fixtures: seed once, then per service three servers on copies of the seed, the
//! setup on each, the call in each form, and the comparison.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use bee_conformance::parity::fixture::{Fixture, FixtureFile, Scenario};
use bee_conformance::parity::naming::{cli_noun, mcp_tool, service_short};
use bee_conformance::parity::normalize::{Outcome, first_difference};
use bee_conformance::parity::report::{Row, Status};
use bee_conformance::parity::template::{Vars, expand};
use bee_proto::methods::{MethodDescriptor, MethodKind, find_method};
use serde_json::Value;

use crate::api::{self, Client, is_event_stream};
use crate::cli;
use crate::mcp::{HttpSession, StdioSession};
use crate::servers::{Server, clone_dir, describe_outcome, fixtures_dir, run_step, start_on};

/// How a run is configured (environment variables, see `docs/spec/verification.md`).
#[derive(Clone, Debug)]
pub struct Settings {
    /// Services to run (short names, e.g. `IssueService`); `None` = all.
    pub services: Option<Vec<String>>,
    /// Only the API form, printing every outcome (for writing fixtures).
    pub probe: bool,
    /// Groups run at once.
    pub jobs: usize,
    /// Print every fixture's outcome.
    pub verbose: bool,
    /// Fixtures to run (method names); `None` = all.
    pub methods: Option<Vec<String>>,
}

/// One method's result.
#[derive(Clone, Debug)]
pub struct MethodResult {
    /// The report row.
    pub row: Row,
    /// What went wrong, per form.
    pub failures: Vec<String>,
    /// The API outcome label and message (probe output).
    pub api_outcome: String,
}

/// Fixture files by fully qualified service, with their paths.
pub type Files = BTreeMap<String, (PathBuf, FixtureFile)>;

/// Every fixture file by fully qualified service, and the scenario.
pub fn load() -> Result<(Scenario, Files), String> {
    let dir = fixtures_dir();
    let scenario_path = dir.join("scenario.json");
    let scenario = Scenario::parse(
        &std::fs::read_to_string(&scenario_path).map_err(|e| format!("scenario.json: {e}"))?,
    )
    .map_err(|e| format!("scenario.json: {e}"))?;
    let mut files = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| p.file_name().is_some_and(|n| n != "scenario.json"))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
        let file = FixtureFile::parse(&text).map_err(|e| format!("{name}: {e}"))?;
        let want = format!("{}.json", cli_noun(&file.service));
        if name != want {
            return Err(format!(
                "{name}: fixtures of {} belong in {want}",
                file.service
            ));
        }
        if files.insert(file.service.clone(), (path, file)).is_some() {
            return Err(format!("{name}: a second file for its service"));
        }
    }
    Ok((scenario, files))
}

/// Starts a fresh dev server, runs the scenario, stops it, and returns its data directory
/// and the scenario's variables.
pub async fn seed(http: &Client, scenario: &Scenario) -> Result<(tempfile::TempDir, Vars), String> {
    let server = start_on(tempfile::tempdir().map_err(|e| e.to_string())?).await;
    let mut vars = Vars::new();
    server.builtins(&mut vars);
    for (i, step) in scenario.steps.iter().enumerate() {
        run_step(http, &server, step, &mut vars)
            .await
            .map_err(|e| format!("scenario step {i}: {e}"))?;
    }
    // Let forge workers settle (outbox relay, event bridge) before the copy.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let Server { srv, dir, .. } = server;
    srv.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut globals = Vars::new();
    for (k, v) in vars.values() {
        if !["server", "token"].contains(&k.as_str()) {
            globals.set(k, v.clone());
        }
    }
    Ok((dir, globals))
}

/// True for methods the suite also calls over gRPC and stdio MCP (reads: idempotent, so a
/// second call on the same server gives the same answer). Judged by the method's first word
/// (`Logout` and `ResolveContentReport` are not reads).
pub fn is_read(m: &MethodDescriptor) -> bool {
    let words = bee_conformance::parity::naming::words(m.method);
    let first = words.first().map(String::as_str).unwrap_or_default();
    !is_event_stream(m)
        && ([
            "get", "list", "search", "has", "is", "read", "diff", "blame", "compare",
        ]
        .contains(&first)
            || ["Log", "WhoAmI", "MergeBase", "Resolve", "ResolveRef"].contains(&m.method))
}

/// The MCP input schemas of every tool, by name.
pub type ToolSchemas = Arc<BTreeMap<String, Value>>;

/// Runs one service's fixtures.
pub async fn run_group(
    http: Client,
    seed: PathBuf,
    globals: Vars,
    file: FixtureFile,
    settings: Settings,
    tools: ToolSchemas,
) -> Vec<MethodResult> {
    let forms = if settings.probe { 1 } else { 3 };
    let mut servers = Vec::new();
    for _ in 0..forms {
        servers.push(start_on(clone_dir(&seed)).await);
    }
    let mut session = if settings.probe {
        None
    } else {
        match HttpSession::open(&http, &servers[2].url, &servers[2].token).await {
            Ok(s) => Some(s),
            Err(e) => panic!("{}: MCP session: {e}", file.service),
        }
    };
    let mut stdio: Option<StdioSession> = None;
    let mut stdio_samples = 0;
    let mut results = Vec::new();
    // Variables per form's server: the scenario's, plus what fixtures `save`.
    let mut group: Vec<Vars> = vec![globals; servers.len()];
    for fixture in &file.fixtures {
        if let Some(only) = &settings.methods
            && !only.contains(&fixture.method)
        {
            continue;
        }
        let r = run_fixture(
            &http,
            &file.service,
            fixture,
            &servers,
            &mut group,
            session.as_mut(),
            &mut stdio,
            &mut stdio_samples,
            &tools,
        )
        .await;
        if settings.verbose || settings.probe {
            println!(
                "  {}/{}: {}{}",
                service_short(&file.service),
                fixture.method,
                r.api_outcome,
                if r.row.api == Status::Pass {
                    ""
                } else {
                    "  [FAIL]"
                }
            );
            if !settings.probe {
                for f in &r.failures {
                    println!("      {f}");
                }
            }
        }
        results.push(r);
    }
    if let Some(s) = stdio {
        s.close().await;
    }
    for s in servers {
        s.stop().await;
    }
    results
}

#[allow(clippy::too_many_arguments)]
async fn run_fixture(
    http: &Client,
    service: &str,
    fixture: &Fixture,
    servers: &[Server],
    group: &mut [Vars],
    session: Option<&mut HttpSession>,
    stdio: &mut Option<StdioSession>,
    stdio_samples: &mut usize,
    tools: &ToolSchemas,
) -> MethodResult {
    let short = format!("{}Service", service_short(service));
    let mut row = Row {
        service: short.clone(),
        method: fixture.method.clone(),
        expect: fixture.expect.clone(),
        api: Status::Fail,
        cli: if servers.len() == 3 {
            Status::Fail
        } else {
            Status::NotRun
        },
        mcp: if servers.len() == 3 {
            Status::Fail
        } else {
            Status::NotRun
        },
    };
    let mut failures = Vec::new();
    let path = format!("/{service}/{}", fixture.method);
    let Some(m) = find_method(&path) else {
        failures.push(format!("{path}: not in the descriptor"));
        return MethodResult {
            row,
            failures,
            api_outcome: "unknown method".into(),
        };
    };
    // Setup and request on every form's server.
    let mut requests = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    // Every server runs the whole setup even when one fails, so state stays identical across
    // the forms for the fixtures after this one.
    let mut prepared: Vec<Result<(Value, String), String>> = Vec::new();
    for (s, saved) in servers.iter().zip(group.iter()) {
        let mut vars = saved.clone();
        s.builtins(&mut vars);
        let mut setup_error = None;
        for step in &fixture.setup {
            if let Err(e) = run_step(http, s, step, &mut vars).await {
                setup_error = Some(format!("setup: {e}"));
                break;
            }
        }
        prepared.push(match setup_error {
            Some(e) => Err(e),
            None => {
                let token = match &fixture.token {
                    Some(t) => expand(&serde_json::json!(t), &vars)
                        .map(|v| v.as_str().unwrap_or_default().to_owned()),
                    None => Ok(s.token.clone()),
                };
                token
                    .and_then(|token| {
                        let r = expand(&fixture.request, &vars)?;
                        valid_request(m, &r)?;
                        Ok((r, token))
                    })
                    .map_err(|e| format!("request: {e}"))
            }
        });
    }
    for p in prepared {
        match p {
            Ok((r, token)) => {
                requests.push(r);
                tokens.push(token);
            }
            Err(e) => {
                let api_outcome = format!("setup or request failed: {e}");
                failures.push(e);
                return MethodResult {
                    row,
                    failures,
                    api_outcome,
                };
            }
        }
    }
    let max = fixture.max_items;
    let api_server = &servers[0];
    let (api_r, cli_r, mcp_r) = if servers.len() == 3 {
        let session = session.expect("session");
        // Another principal's token needs its own /mcp session (sessions are bound to the
        // credential that opened them).
        let mut own = None;
        if fixture.token.is_some() {
            match HttpSession::open(http, &servers[2].url, &tokens[2]).await {
                Ok(s) => own = Some(s),
                Err(e) => failures.push(format!("mcp: session for the fixture token: {e}")),
            }
        }
        let session = match own.as_mut() {
            Some(s) => s,
            None => session,
        };
        let (a, c, mc) = tokio::join!(
            api::call(http, &api_server.url, &tokens[0], m, &requests[0], max),
            cli::call(
                &servers[1].url,
                &tokens[1],
                servers[1].cli_config.path(),
                m,
                &requests[1]
            ),
            session.call(http, m, &requests[2], max),
        );
        (a, Some(c), Some(mc))
    } else {
        (
            api::call(http, &api_server.url, &tokens[0], m, &requests[0], max).await,
            None,
            None,
        )
    };
    if let Some(name) = &fixture.save {
        let outcomes = [
            Some(&api_r.outcome),
            cli_r.as_ref().map(|c| &c.outcome),
            mcp_r.as_ref().map(|c| &c.outcome),
        ];
        for (vars, outcome) in group.iter_mut().zip(outcomes) {
            if let Some(Outcome::Ok(items)) = outcome {
                let value = match items.as_slice() {
                    [one] => one.clone(),
                    many => serde_json::json!({ "items": many }),
                };
                vars.set(name, value);
            }
        }
    }
    let mut api_outcome = api_r.outcome.clone();
    api_outcome.truncate(max);
    let show = std::env::var_os("BEE_CONFORMANCE_SHOW").is_some();
    let api_label = match &api_outcome {
        Outcome::Ok(items) if show => format!(
            "ok {}\n      request {}",
            api::describe(&serde_json::Value::Array(items.clone())),
            api::describe(&requests[0])
        ),
        Outcome::Ok(_) => "ok".to_owned(),
        Outcome::Err(e) if show => format!(
            "{} ({}) {}\n      request {}",
            e.reason,
            e.subcode,
            e.message,
            api::describe(&requests[0])
        ),
        Outcome::Err(e) => format!("{} ({}) {}", e.reason, e.subcode, e.message),
    };
    let api_norm = api_server
        .normalizer()
        .outcome(&api_outcome, &fixture.volatile);
    let expect_line = |o: &Outcome| {
        format!(
            "want {}, got {} (request {})",
            fixture.expect,
            describe_outcome(o),
            api::describe(&requests[0])
        )
    };

    // API: the expectation, the Connect contract, and gRPC for reads.
    let mut api_pass = api_outcome.label() == fixture.expect && api_r.problems.is_empty();
    if api_outcome.label() != fixture.expect {
        failures.push(format!("api: {}", expect_line(&api_outcome)));
    }
    for p in &api_r.problems {
        failures.push(format!("api: {p}"));
    }
    if is_read(m)
        && servers.len() == 3
        && let Some(g) = api::call_grpc(&api_server.url, &tokens[0], m, &requests[0], max).await
    {
        match g {
            Ok(mut o) => {
                o.truncate(max);
                let n = api_server.normalizer().outcome(&o, &fixture.volatile);
                if let Some(d) = first_difference(&api_norm, &n) {
                    api_pass = false;
                    failures.push(format!("grpc differs from HTTP/JSON at {d}"));
                }
            }
            Err(e) => {
                api_pass = false;
                failures.push(format!("grpc: {e}"));
            }
        }
    }
    if api_pass {
        row.api = Status::Pass;
    }

    // CLI.
    if let Some(c) = cli_r {
        let mut o = c.outcome.clone();
        o.truncate(max);
        let n = servers[1].normalizer().outcome(&o, &fixture.volatile);
        let mut pass = o.label() == fixture.expect && c.problems.is_empty();
        if o.label() != fixture.expect {
            failures.push(format!("cli: {} [{}]", expect_line(&o), c.command));
        }
        for p in &c.problems {
            failures.push(format!("cli: {p} [{}]", c.command));
        }
        if let Some(d) = first_difference(&api_norm, &n) {
            pass = false;
            failures.push(format!("cli differs from api at {d} [{}]", c.command));
        }
        if pass {
            row.cli = Status::Pass;
        }
    }

    // MCP over HTTP, its input schema, and stdio for a sample of reads.
    if let Some(mc) = mcp_r {
        let mut o = mc.outcome.clone();
        o.truncate(max);
        let n = servers[2].normalizer().outcome(&o, &fixture.volatile);
        let mut pass = o.label() == fixture.expect && mc.problems.is_empty();
        if o.label() != fixture.expect {
            failures.push(format!("mcp: {}", expect_line(&o)));
        }
        for p in &mc.problems {
            failures.push(format!("mcp: {p}"));
        }
        if let Some(d) = first_difference(&api_norm, &n) {
            pass = false;
            failures.push(format!("mcp differs from api at {d}"));
        }
        let tool = mcp_tool(m.service, m.method);
        let arguments = crate::mcp::tool_params(m, &requests[2], max)["arguments"].clone();
        match tools.get(&tool) {
            Some(schema) => {
                let mut problems = Vec::new();
                validate_inline(schema, &arguments, &mut problems);
                for p in problems {
                    pass = false;
                    failures.push(format!("mcp: arguments do not match the inputSchema: {p}"));
                }
            }
            None => {
                pass = false;
                failures.push(format!("mcp: tools/list has no {tool}"));
            }
        }
        if is_read(m) && fixture.token.is_none() && *stdio_samples < 3 {
            *stdio_samples += 1;
            if stdio.is_none() {
                match StdioSession::open(
                    &servers[2].url,
                    &servers[2].token,
                    servers[2].cli_config.path(),
                )
                .await
                {
                    Ok(s) => *stdio = Some(s),
                    Err(e) => {
                        pass = false;
                        failures.push(format!("mcp stdio: {e}"));
                    }
                }
            }
            if let Some(s) = stdio.as_mut() {
                let r = s.call(m, &requests[2], max).await;
                let mut so = r.outcome;
                so.truncate(max);
                let sn = servers[2].normalizer().outcome(&so, &fixture.volatile);
                if let Some(d) = first_difference(&n, &sn) {
                    pass = false;
                    failures.push(format!("mcp stdio differs from /mcp at {d}"));
                }
                for p in r.problems {
                    pass = false;
                    failures.push(format!("mcp stdio: {p}"));
                }
            }
        }
        if pass {
            row.mcp = Status::Pass;
        }
    }
    MethodResult {
        row,
        failures,
        api_outcome: api_label,
    }
}

/// A fixture's request must be a valid proto3-JSON request message (each message, for client
/// streams), so every form sends it to the server instead of rejecting it locally.
fn valid_request(m: &MethodDescriptor, request: &Value) -> Result<(), String> {
    let messages = match (m.kind, request) {
        (MethodKind::ClientStreaming, Value::Array(items)) => items.clone(),
        (MethodKind::ClientStreaming, _) => {
            return Err("client-streaming fixtures give an array of messages".into());
        }
        (_, other) => vec![other.clone()],
    };
    for msg in &messages {
        let json = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        (m.request_json_to_proto)(&json)
            .map_err(|e| format!("not a valid {}: {e}", m.input_type))?;
    }
    Ok(())
}

/// Validates against an in-memory schema.
pub fn validate_inline(schema: &Value, instance: &Value, problems: &mut Vec<String>) {
    match jsonschema::options().build(schema) {
        Ok(v) => {
            for e in v.iter_errors(instance).take(3) {
                problems.push(format!("{e} at {}", e.instance_path()));
            }
        }
        Err(e) => problems.push(format!("schema does not compile: {e}")),
    }
}
