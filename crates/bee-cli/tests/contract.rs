//! CLI contract tests: the real `bee` binary against a fake Connect server, with every
//! `--json` document validated against the published schemas in `schemas/v1/`.

use crate::support;

use std::collections::BTreeSet;
use std::time::Duration;

use bee_cli::catalog::catalog;
use bee_cli::cli::{Expand, root};
use bee_cli::commands::registry;
use bee_cli::schemas::{CLI_SCHEMA_DIR, cli_schema_path, generated_files};
use bee_proto::errors::error_detail;
use bee_proto::methods::{METHODS, MethodKind};
use bee_proto::v1::{ErrorReason, error_detail::Payload};
use serde_json::{Value, json};
use support::{
    Bee, FakeServer, Reply, StreamStep, bee, object_id, repo_root, sample_response, validate,
    validate_error,
};

/// Runs `bee <noun> <verb> --json --endpoint <server> <extra>` and returns the result.
fn call(server: &FakeServer, words: &[&str], extra: &[&str]) -> support::Run {
    let mut args: Vec<&str> = words.to_vec();
    args.extend(["--json", "--endpoint", server.endpoint.as_str()]);
    args.extend(extra);
    bee(&args)
}

// ---------- Catalog coverage ----------

#[test]
fn every_rpc_has_a_reachable_command_and_a_schema_file() {
    let catalog = catalog();
    assert_eq!(catalog.commands().count(), METHODS.len());
    assert!(METHODS.len() >= 1000, "{} RPCs", METHODS.len());
    let mut paths = BTreeSet::new();
    for noun in catalog.nouns() {
        let mut tree = root(Expand::Noun(&noun.name));
        for verb in &noun.verbs {
            let error = tree
                .try_get_matches_from_mut(["bee", noun.name.as_str(), verb.name.as_str(), "--help"])
                .expect_err("--help short-circuits");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "bee {} {} is not reachable",
                noun.name,
                verb.name
            );
            let file = cli_schema_path(&[&noun.name, &verb.name]);
            let text = std::fs::read_to_string(repo_root().join(&file))
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            let schema: Value = serde_json::from_str(&text).expect("schema");
            assert_eq!(
                schema["properties"]["schema"]["const"],
                format!("bee.v1.{}.{}", noun.name, verb.name)
            );
            assert!(paths.insert(verb.method.path), "{} twice", verb.method.path);
        }
    }
    assert_eq!(paths.len(), METHODS.len());
    for command in registry() {
        let file = cli_schema_path(command.path());
        assert!(repo_root().join(&file).is_file(), "{file}");
    }
}

#[test]
fn cli_schemas_are_up_to_date() {
    let files = generated_files();
    let mut stale = Vec::new();
    for (relative, content) in &files {
        if std::fs::read_to_string(repo_root().join(relative))
            .ok()
            .as_deref()
            != Some(content)
        {
            stale.push(relative.clone());
        }
    }
    let extra: Vec<String> = std::fs::read_dir(repo_root().join(CLI_SCHEMA_DIR))
        .expect("schemas/v1/cli")
        .filter_map(|e| e.ok())
        .map(|e| format!("{CLI_SCHEMA_DIR}/{}", e.file_name().to_string_lossy()))
        .filter(|p| p.ends_with(".schema.json") && !files.contains_key(p))
        .collect();
    assert!(
        stale.is_empty() && extra.is_empty(),
        "CLI schemas are stale ({} differ, {} extra, e.g. {:?} {:?}); run \
         `cargo run -p bee-cli --bin bee-cli-gen`",
        stale.len(),
        extra.len(),
        stale.first(),
        extra.first()
    );
}

#[test]
fn registered_commands_do_not_collide_with_generated_ones() {
    let catalog = catalog();
    let mut seen = BTreeSet::new();
    for command in registry() {
        let path = command.path();
        assert!(matches!(path.len(), 1 | 2), "{path:?}");
        assert!(seen.insert(path.to_vec()), "{path:?} registered twice");
        match path {
            [word] => assert!(catalog.noun(word).is_none(), "{word} shadows a noun"),
            [noun, verb] => assert!(
                catalog.noun(noun).and_then(|n| n.verb(verb)).is_none(),
                "{noun} {verb} shadows an RPC"
            ),
            _ => unreachable!(),
        }
    }
}

// ---------- --json output validates against the published schemas ----------

#[test]
fn sampled_commands_across_services_validate_against_their_schemas() {
    let server = FakeServer::start();
    let catalog = catalog();
    let mut checked = 0;
    for noun in catalog.nouns() {
        // The first unary verb of every service, plus every third one of the VCS services.
        let unary: Vec<_> = noun
            .verbs
            .iter()
            .filter(|v| v.method.kind == MethodKind::Unary)
            .collect();
        let step = if ["write", "query", "ref", "change"].contains(&noun.name.as_str()) {
            3
        } else {
            usize::MAX
        };
        for verb in unary.iter().step_by(step) {
            let run = call(&server, &[&noun.name, &verb.name], &["--json-input", "{}"]);
            assert_eq!(run.code, 0, "bee {} {}: {run:?}", noun.name, verb.name);
            let document = run.json();
            validate(&cli_schema_path(&[&noun.name, &verb.name]), &document);
            assert_eq!(document["data"], sample_response(verb.method.output_type));
            assert_eq!(server.last().path, verb.method.path);
            assert_eq!(server.last().content_type, "application/json");
            checked += 1;
        }
    }
    assert!(checked >= 30, "only {checked} commands checked");
}

#[test]
fn whoami_and_auth_who_am_i_share_the_call() {
    let server = FakeServer::start();
    let whoami = call(&server, &["whoami"], &[]);
    assert_eq!(whoami.code, 0, "{whoami:?}");
    validate("schemas/v1/cli/whoami.schema.json", &whoami.json());
    let generated = call(&server, &["auth", "who-am-i"], &[]);
    validate(
        "schemas/v1/cli/auth.who-am-i.schema.json",
        &generated.json(),
    );
    assert_eq!(whoami.json()["data"], generated.json()["data"]);
    assert_eq!(server.last().path, "/bee.v1.AuthService/WhoAmI");
}

// ---------- Exit codes and error documents ----------

fn detail(reason: ErrorReason, message: &str) -> bee_proto::v1::ErrorDetail {
    error_detail(reason, message)
}

#[test]
fn every_error_class_has_its_exit_code_and_a_valid_error_document() {
    let server = FakeServer::start();
    let land = "/bee.v1.WriteService/Land";
    let cases = [
        (ErrorReason::MergeConflict, 3, false),
        (ErrorReason::PolicyDenied, 5, false),
        (ErrorReason::Leased, 5, false),
        (ErrorReason::SecretDetected, 5, false),
        (ErrorReason::BudgetExhausted, 5, false),
        (ErrorReason::NotFound, 1, false),
        (ErrorReason::Unauthenticated, 1, false),
        (ErrorReason::InvalidArgument, 1, false),
        (ErrorReason::VersionMismatch, 1, false),
        (ErrorReason::LandContention, 1, true),
        (ErrorReason::Unavailable, 1, true),
        (ErrorReason::Internal, 1, false),
    ];
    for (reason, exit, retryable) in cases {
        server.reply(land, Reply::Error(detail(reason, "scripted failure")));
        let run = call(&server, &["write", "land"], &["--repo", "acme/app"]);
        assert_eq!(run.code, exit, "{reason:?}: {run:?}");
        let error = validate_error(&run.json());
        assert_eq!(error["reason"], bee_proto::errors::reason_name(reason));
        assert_eq!(error["retryable"], retryable, "{reason:?}");
        assert_eq!(error["message"], "scripted failure");
    }
}

#[test]
fn error_payloads_pass_through() {
    let server = FakeServer::start();
    let mut conflict = detail(ErrorReason::MergeConflict, "conflicts in 1 file");
    conflict.payload = Some(Payload::Conflicts(bee_proto::v1::ConflictSet::default()));
    conflict.request_id = "req-1".into();
    server.reply("/bee.v1.WriteService/Merge", Reply::Error(conflict));
    let run = call(&server, &["write", "merge"], &[]);
    assert_eq!(run.code, 3);
    let error = validate_error(&run.json());
    assert_eq!(error["payload"]["requestId"], "req-1");
    assert!(error["payload"].get("conflicts").is_some(), "{error}");

    let mut throttled = detail(ErrorReason::ResourceExhausted, "slow down");
    throttled.retry_after_ms = 1500;
    server.reply("/bee.v1.RefService/ListRefs", Reply::Error(throttled));
    let run = call(&server, &["ref", "list-refs"], &[]);
    assert_eq!(run.code, 1);
    let error = validate_error(&run.json());
    assert_eq!(error["retryable"], true);
    assert_eq!(error["payload"]["retryAfterMs"], "1500");
}

#[test]
fn usage_errors_exit_2_with_an_error_document() {
    let server = FakeServer::start();
    let cases: [&[&str]; 8] = [
        &["write", "land", "--no-such-flag"],
        &["write", "land", "--method", "sideways"],
        &["write", "land", "--expected-version", "-3"],
        &["write", "land", "--head", "not-an-id"],
        &["write", "land", "--json-input", "{\"repoo\": {}}"],
        &[
            "write",
            "land",
            "--json-input",
            "{\"repo\": {\"fullNam\": \"x\"}}",
        ],
        &["no-such-noun", "verb"],
        &["write", "no-such-verb"],
    ];
    for args in cases {
        let run = call(&server, args, &[]);
        assert_eq!(run.code, 2, "{args:?}: {run:?}");
        let error = validate_error(&run.json());
        assert_eq!(error["reason"], "INVALID_ARGUMENT", "{args:?}");
    }
    assert!(
        server.received().is_empty(),
        "usage errors must not send anything"
    );

    // No endpoint anywhere.
    let run = bee(&["write", "land", "--json"]);
    assert_eq!(run.code, 2);
    assert_eq!(
        validate_error(&run.json())["payload"]["subcode"],
        "no_endpoint"
    );
}

#[test]
fn unreachable_server_is_retryable_unavailable() {
    // Bind and drop a listener to get a port nobody serves.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("port")
        .port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let run = bee(&["ref", "list-refs", "--json", "--endpoint", &endpoint]);
    assert_eq!(run.code, 1);
    let error = validate_error(&run.json());
    assert_eq!(error["reason"], "UNAVAILABLE");
    assert_eq!(error["retryable"], true);
}

#[test]
fn timeout_gives_deadline_exceeded_and_sends_connect_timeout() {
    let server = FakeServer::start();
    server.reply(
        "/bee.v1.RefService/ListRefs",
        Reply::Delay(Duration::from_secs(10)),
    );
    let run = call(&server, &["ref", "list-refs"], &["--timeout", "300ms"]);
    assert_eq!(run.code, 1);
    let error = validate_error(&run.json());
    assert_eq!(error["reason"], "DEADLINE_EXCEEDED");
    assert_eq!(error["retryable"], true);
    assert_eq!(server.last().timeout_ms.as_deref(), Some("300"));
}

// ---------- Flags to request ----------

#[test]
fn flags_map_to_request_fields() {
    let server = FakeServer::start();
    let (id_text, id_json) = object_id();
    let run = call(
        &server,
        &["write", "land"],
        &[
            "--repo",
            "acme/app",
            "--ref",
            "main",
            "--head",
            &id_text,
            "--expected-version",
            "7",
            "--allow-empty",
            "--method",
            "squash",
            "--message",
            "ship it",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate("schemas/v1/cli/write.land.schema.json", &run.json());
    assert_eq!(
        server.last().messages,
        [json!({
            "repo": {"fullName": "acme/app"},
            "ref": "main",
            "head": id_json,
            "expectedVersion": "7",
            "allowEmpty": true,
            "method": "LAND_METHOD_SQUASH",
            "message": "ship it",
        })]
    );
}

#[test]
fn repeated_map_bytes_and_nested_fields() {
    let server = FakeServer::start();
    let run = call(
        &server,
        &["issue", "create-issue"],
        &[
            "--repo",
            "01HZXREPOID",
            "--title",
            "Flaky test",
            "--labels",
            "bug",
            "--labels",
            "ci",
            "--parent",
            "{\"repo\": {\"fullName\": \"acme/app\"}, \"number\": \"4\"}",
            "--form-responses",
            "{\"key\": \"os\", \"value\": \"linux\"}",
            "--form-responses",
            "[{\"key\": \"arch\", \"value\": \"arm64\"}]",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    validate("schemas/v1/cli/issue.create-issue.schema.json", &run.json());
    assert_eq!(
        server.last().messages[0],
        json!({
            "repo": {"id": "01HZXREPOID"},
            "title": "Flaky test",
            "labels": ["bug", "ci"],
            "parent": {"repo": {"fullName": "acme/app"}, "number": "4"},
            "formResponses": [{"key": "os", "value": "linux"}, {"key": "arch", "value": "arm64"}],
        })
    );

    let run = call(
        &server,
        &["gist", "create-gist"],
        &[
            "--files",
            "a.txt=hello",
            "--files",
            "b.md=# hi=there",
            "--public",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().messages[0],
        json!({"files": {"a.txt": "hello", "b.md": "# hi=there"}, "public": true})
    );

    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 255]).expect("write");
    let file_arg = format!("@{}", dir.path().join("blob.bin").display());
    let run = call(
        &server,
        &["lease", "acquire-lease"],
        &[
            "--paths",
            "src/lib.rs",
            "--paths",
            &file_arg,
            "--paths",
            "base64:AAE=",
            "--ttl-seconds",
            "60",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().messages[0],
        json!({"paths": ["c3JjL2xpYi5ycw==", "AAEC/w==", "AAE="], "ttlSeconds": 60})
    );
}

#[test]
fn json_input_merges_under_flags_and_reads_stdin() {
    let server = FakeServer::start();
    let run = call(
        &server,
        &["write", "land"],
        &[
            "--json-input",
            "{\"repo\": {\"fullName\": \"acme/old\"}, \"ref\": \"main\", \"allow_empty\": true}",
            "--repo",
            "acme/new",
        ],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().messages[0],
        json!({"repo": {"fullName": "acme/new"}, "ref": "main", "allowEmpty": true})
    );

    let (run, _) = Bee::new(&[
        "write",
        "land",
        "--json",
        "--endpoint",
        &server.endpoint,
        "--input-file",
        "-",
        "--message",
        "from flags",
    ])
    .stdin("{\"ref\": \"release\", \"method\": \"LAND_METHOD_MERGE_COMMIT\"}")
    .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().messages[0],
        json!({"ref": "release", "method": "LAND_METHOD_MERGE_COMMIT", "message": "from flags"})
    );
}

// ---------- Streams ----------

#[test]
fn server_streams_print_ndjson() {
    let server = FakeServer::start();
    let item = sample_response("bee.v1.DiffItem");
    server.reply(
        "/bee.v1.QueryService/Diff",
        Reply::Stream(
            vec![
                StreamStep::Message(item.clone()),
                StreamStep::Message(item.clone()),
                StreamStep::Message(item.clone()),
            ],
            None,
        ),
    );
    let run = call(
        &server,
        &["query", "diff"],
        &["--repo", "acme/app", "--paths", "src"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    let lines = run.lines();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        validate("schemas/v1/cli/query.diff.schema.json", line);
        assert_eq!(line["data"], item);
    }
    let received = server.last();
    assert_eq!(received.content_type, "application/connect+json");
    assert_eq!(
        received.messages,
        [json!({"repo": {"fullName": "acme/app"}, "paths": ["c3Jj"]})]
    );

    // A failure mid-stream ends the output with an error document and its exit code.
    server.reply(
        "/bee.v1.QueryService/Diff",
        Reply::Stream(
            vec![StreamStep::Message(item)],
            Some(detail(ErrorReason::PolicyDenied, "path is restricted")),
        ),
    );
    let run = call(&server, &["query", "diff"], &[]);
    assert_eq!(run.code, 5, "{run:?}");
    let lines = run.lines();
    assert_eq!(lines.len(), 2);
    validate("schemas/v1/cli/query.diff.schema.json", &lines[0]);
    assert_eq!(validate_error(&lines[1])["reason"], "POLICY_DENIED");
}

#[test]
fn event_streams_catch_up_without_follow_and_stay_open_with_it() {
    let server = FakeServer::start();
    let event = sample_response("bee.v1.Event");
    let script = Reply::Stream(
        vec![
            StreamStep::Message(event.clone()),
            StreamStep::Message(event.clone()),
            StreamStep::Pause(Duration::from_millis(3500)),
            StreamStep::Message(event.clone()),
        ],
        None,
    );
    server.reply("/bee.v1.EventService/Subscribe", script);
    let caught_up = call(&server, &["event", "subscribe"], &["--repo", "acme/app"]);
    assert_eq!(caught_up.code, 0, "{caught_up:?}");
    assert_eq!(caught_up.lines().len(), 2);

    let followed = call(
        &server,
        &["event", "subscribe"],
        &["--repo", "acme/app", "--follow"],
    );
    assert_eq!(followed.code, 0, "{followed:?}");
    let lines = followed.lines();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        validate("schemas/v1/cli/event.subscribe.schema.json", line);
    }
}

#[test]
fn client_streams_send_every_input_message() {
    let server = FakeServer::start();
    let (id_text, id_json) = object_id();
    let input = format!(
        "{{\"repo\": {{\"fullName\": \"acme/app\"}}, \"id\": {id_json}, \"data\": \"aGk=\"}}\n\
         {{\"id\": {id_json}, \"offset\": \"2\", \"data\": \"IQ==\", \"last\": true}}\n"
    );
    let (run, _) = Bee::new(&[
        "write",
        "upload-objects",
        "--json",
        "--endpoint",
        &server.endpoint,
        "--input-file",
        "-",
    ])
    .stdin(&input)
    .run();
    assert_eq!(run.code, 0, "{run:?}");
    validate(
        "schemas/v1/cli/write.upload-objects.schema.json",
        &run.json(),
    );
    let received = server.last();
    assert_eq!(received.content_type, "application/connect+json");
    assert_eq!(received.messages.len(), 2);
    assert_eq!(received.messages[1]["last"], true);

    // Flags alone form one message.
    let run = call(
        &server,
        &["write", "upload-objects"],
        &["--id", &id_text, "--last"],
    );
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().messages,
        [json!({"id": id_json, "last": true})]
    );
}

// ---------- Global flags and credentials ----------

#[test]
fn endpoint_and_token_come_from_flags_env_or_credentials() {
    let server = FakeServer::start();
    // Environment.
    let (run, _) = Bee::new(&["ref", "list-refs", "--json"])
        .env("BEE_ENDPOINT", &server.endpoint)
        .env("BEE_TOKEN", "env-token")
        .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().authorization.as_deref(),
        Some("Bearer env-token")
    );

    // A flag beats the environment.
    let (run, _) = Bee::new(&["ref", "list-refs", "--json", "--token", "flag-token"])
        .env("BEE_ENDPOINT", &server.endpoint)
        .env("BEE_TOKEN", "env-token")
        .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(
        server.last().authorization.as_deref(),
        Some("Bearer flag-token")
    );

    // No token anywhere: no Authorization header.
    let run = call(&server, &["ref", "list-refs"], &[]);
    assert_eq!(run.code, 0);
    assert_eq!(server.last().authorization, None);

    // Credentials written by login.
    let (login, config) = Bee::new(&[
        "login",
        "--token",
        "stored-token",
        "--endpoint",
        &server.endpoint,
        "--json",
    ])
    .run();
    assert_eq!(login.code, 0, "{login:?}");
    validate("schemas/v1/cli/login.schema.json", &login.json());
    let (run, config) = Bee::with_config(&["whoami", "--json"], config).run();
    assert_eq!(run.code, 0, "{run:?}");
    validate("schemas/v1/cli/whoami.schema.json", &run.json());
    assert_eq!(
        server.last().authorization.as_deref(),
        Some("Bearer stored-token")
    );
    // The environment beats the credentials file.
    let (_, config) = Bee::with_config(&["whoami", "--json"], config)
        .env("BEE_TOKEN", "env-token")
        .run();
    assert_eq!(
        server.last().authorization.as_deref(),
        Some("Bearer env-token")
    );
    drop(config);
}

#[cfg(unix)]
#[test]
fn login_writes_private_credentials_and_logout_removes_them() {
    use std::os::unix::fs::PermissionsExt as _;
    let (run, config) = Bee::new(&["login", "--token", "-", "--json"])
        .stdin("piped-token\n")
        .run();
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.json();
    validate("schemas/v1/cli/login.schema.json", &document);
    let path = config.path().join("credentials");
    assert_eq!(
        document["data"]["credentialsPath"],
        path.display().to_string()
    );
    let mode = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    assert_eq!(stored["token"], "piped-token");

    // A credentials file others can read is refused.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let (run, config) = Bee::with_config(
        &["whoami", "--json", "--endpoint", "http://127.0.0.1:9"],
        config,
    )
    .run();
    assert_eq!(run.code, 1);
    assert_eq!(
        validate_error(&run.json())["payload"]["subcode"],
        "insecure_credentials"
    );

    let (run, config) = Bee::with_config(&["logout", "--json"], config).run();
    assert_eq!(run.code, 0);
    validate("schemas/v1/cli/logout.schema.json", &run.json());
    assert_eq!(run.json()["data"]["removed"], true);
    assert!(!path.exists());
    let (run, _) = Bee::with_config(&["logout", "--json"], config).run();
    assert_eq!(run.json()["data"]["removed"], false);

    // Login without a token is a usage error.
    let run = bee(&["login", "--json"]);
    assert_eq!(run.code, 2);
    assert_eq!(
        validate_error(&run.json())["payload"]["subcode"],
        "missing_token"
    );
}

#[test]
fn dash_c_changes_directory_before_reading_inputs() {
    let server = FakeServer::start();
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("request.json"), "{\"ref\": \"from-file\"}").expect("write");
    let other = tempfile::tempdir().expect("other");
    let (run, _) = Bee::new(&[
        "-C",
        &dir.path().display().to_string(),
        "write",
        "land",
        "--json",
        "--endpoint",
        &server.endpoint,
        "--input-file",
        "request.json",
    ])
    .current_dir(other.path())
    .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(server.last().messages[0], json!({"ref": "from-file"}));

    let run = bee(&["-C", "/definitely/not/here", "whoami", "--json"]);
    assert_eq!(run.code, 2);
    assert_eq!(
        validate_error(&run.json())["payload"]["subcode"],
        "bad_directory"
    );
}

#[cfg(unix)]
#[test]
fn unix_socket_endpoints_work() {
    let dir = tempfile::tempdir().expect("dir");
    let socket = dir.path().join("bee.sock");
    let server = FakeServer::start_unix(&socket);
    let run = call(&server, &["object", "get-server-info"], &[]);
    assert_eq!(run.code, 0, "{run:?}");
    validate(
        "schemas/v1/cli/object.get-server-info.schema.json",
        &run.json(),
    );
}

// ---------- Help, schema and the agent index ----------

#[test]
fn verb_help_lists_every_flag_with_type_and_an_example() {
    let catalog = catalog();
    for (noun, verb) in [
        ("write", "land"),
        ("issue", "create-issue"),
        ("event", "subscribe"),
    ] {
        let run = bee(&[noun, verb, "--help"]);
        assert_eq!(run.code, 0);
        let help = run.stdout;
        let verb_info = catalog.noun(noun).and_then(|n| n.verb(verb)).expect("verb");
        let message = catalog
            .message(verb_info.method.input_type)
            .expect("request");
        for field in &message.fields {
            let flag = format!("--{}", field.name.replace('_', "-"));
            assert!(help.contains(&flag), "{noun} {verb} help lacks {flag}");
        }
        assert!(help.contains("; optional"), "{help}");
        assert!(
            help.contains(&format!("EXAMPLE:\n  bee {noun} {verb}")),
            "{help}"
        );
        assert!(help.contains(&format!("schemas/v1/cli/{noun}.{verb}.schema.json")));
        assert!(help.contains("--json-input"));
    }
    let land = bee(&["write", "land", "--help"]).stdout;
    assert!(land.contains("[enum bee.v1.LandMethod: rebase|squash|merge-commit; optional]"));
    assert!(land.contains("[uint64; optional]"));
    let issue = bee(&["issue", "create-issue", "--help"]).stdout;
    assert!(issue.contains("[string; optional; repeatable]"), "{issue}");
    assert!(
        bee(&["event", "subscribe", "--help"])
            .stdout
            .contains("--follow")
    );
}

#[test]
fn noun_and_root_help_list_their_commands() {
    let catalog = catalog();
    let run = bee(&["write", "--help"]);
    assert_eq!(run.code, 0);
    for verb in &catalog.noun("write").expect("write").verbs {
        assert!(run.stdout.contains(&verb.name), "{}", verb.name);
        assert!(run.stdout.contains(&verb.summary()), "{}", verb.summary());
    }
    let run = bee(&["--help"]);
    for noun in catalog.nouns() {
        assert!(
            run.stdout.contains(&format!("  {} ", noun.name)),
            "{}",
            noun.name
        );
    }
    for word in [
        "login",
        "logout",
        "whoami",
        "schema",
        "help-agent",
        "EXIT CODES",
    ] {
        assert!(run.stdout.contains(word), "{word}");
    }
    // A noun without a verb prints its verbs and exits with the usage code.
    let run = bee(&["write"]);
    assert_eq!(run.code, 2);
    assert!(run.stdout.contains("land"));
}

#[test]
fn help_agent_indexes_every_command() {
    let run = bee(&["help-agent", "--json"]);
    assert_eq!(run.code, 0);
    let document = run.json();
    validate("schemas/v1/cli/help-agent.schema.json", &document);
    let commands = document["data"]["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), METHODS.len() + registry().len());
    for command in commands {
        let output = command["output"].as_str().expect("output");
        assert!(repo_root().join(output).is_file(), "{output}");
        if let Some(request) = command["request"].as_str() {
            assert!(repo_root().join(request).is_file(), "{request}");
        }
    }
    let text = bee(&["help-agent"]).stdout;
    assert_eq!(text.lines().count(), 5 + METHODS.len() + registry().len());
    assert!(
        text.contains("write land | unary | schemas/v1/messages/bee.v1.LandRequest.schema.json |")
    );
}

#[test]
fn schema_command_prints_request_response_and_output_schemas() {
    let run = bee(&["schema", "write", "land", "--json"]);
    assert_eq!(run.code, 0, "{run:?}");
    let document = run.json();
    validate("schemas/v1/cli/schema.schema.json", &document);
    let data = &document["data"];
    assert_eq!(data["rpc"], "/bee.v1.WriteService/Land");
    assert_eq!(data["request"]["title"], "bee.v1.LandRequest");
    assert_eq!(data["response"]["title"], "bee.v1.LandResponse");
    let on_disk: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("schemas/v1/cli/write.land.schema.json"))
            .expect("file"),
    )
    .expect("json");
    assert_eq!(data["output"], on_disk);

    let error = bee(&["schema", "error", "--json"]).json();
    assert_eq!(
        error["data"]["output"]["properties"]["schema"]["const"],
        "bee.v1.error"
    );
    let whoami = bee(&["schema", "whoami", "--json"]).json();
    assert_eq!(whoami["data"]["schema"], "bee.v1.whoami");
    let missing = bee(&["schema", "nope", "nada", "--json"]);
    assert_eq!(missing.code, 2);
    validate_error(&missing.json());
}

#[test]
fn human_mode_renders_readably_and_errors_go_to_stderr() {
    let server = FakeServer::start();
    let (id_text, id_json) = object_id();
    server.reply(
        "/bee.v1.WriteService/Land",
        Reply::Message(json!({"landed": [{"result": id_json}], "attempts": 2})),
    );
    let run = bee(&["write", "land", "--endpoint", &server.endpoint]);
    assert_eq!(run.code, 0, "{run:?}");
    assert!(
        run.stdout.contains(&format!("result: {id_text}")),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("attempts: 2"));
    assert!(serde_json::from_str::<Value>(&run.stdout).is_err());

    server.reply(
        "/bee.v1.WriteService/Land",
        Reply::Error(detail(ErrorReason::MergeConflict, "conflicts")),
    );
    let run = bee(&["write", "land", "--endpoint", &server.endpoint]);
    assert_eq!(run.code, 3);
    assert!(run.stdout.is_empty());
    assert!(
        run.stderr.starts_with("error: MERGE_CONFLICT: conflicts"),
        "{}",
        run.stderr
    );
}

/// F46-014: `config.toml` in the config directory sets client defaults (endpoint, timeout,
/// `--json`, daemon use, author); the environment beats the file and a flag beats both; an
/// unknown key is a usage error; the credentials file next to it stays private (0600).
#[cfg(unix)]
#[test]
fn client_config_file_with_environment_and_flag_overrides() {
    use std::os::unix::fs::PermissionsExt as _;
    let from_file = FakeServer::start();
    let from_env = FakeServer::start();
    let from_flag = FakeServer::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "endpoint = \"{}\"\njson = true\ntimeout = \"20s\"\ndaemon = false\nauthor = \"Ada\"\n",
            from_file.endpoint
        ),
    )
    .unwrap();
    // The file: endpoint, and JSON output without --json.
    let (run, dir) = Bee::with_config(&["ref", "list-refs", "--repo", "acme/app"], dir).run();
    assert_eq!(run.code, 0, "{run:?}");
    assert!(run.json()["schema"].is_string(), "JSON by default: {run:?}");
    assert_eq!(from_file.last().path, "/bee.v1.RefService/ListRefs");
    // The environment beats the file.
    let (run, dir) = Bee::with_config(&["ref", "list-refs", "--repo", "acme/app"], dir)
        .env("BEE_ENDPOINT", &from_env.endpoint)
        .env("BEE_JSON", "0")
        .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert!(
        serde_json::from_str::<serde_json::Value>(run.stdout.trim()).is_err()
            || !run.stdout.contains("\"schema\""),
        "BEE_JSON=0 turns JSON off: {run:?}"
    );
    assert_eq!(from_env.last().path, "/bee.v1.RefService/ListRefs");
    // A flag beats both.
    let (run, dir) = Bee::with_config(
        &[
            "ref",
            "list-refs",
            "--repo",
            "acme/app",
            "--endpoint",
            &from_flag.endpoint,
        ],
        dir,
    )
    .env("BEE_ENDPOINT", &from_env.endpoint)
    .run();
    assert_eq!(run.code, 0, "{run:?}");
    assert_eq!(from_flag.last().path, "/bee.v1.RefService/ListRefs");
    // Credentials written by login stay private next to the config.
    let (run, dir) = Bee::with_config(
        &["login", "--token", "t", "--endpoint", &from_file.endpoint],
        dir,
    )
    .run();
    assert_eq!(run.code, 0, "{run:?}");
    let mode = std::fs::metadata(dir.path().join("credentials"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // An unknown key stops every command with a usage error naming it.
    std::fs::write(dir.path().join("config.toml"), "endpont = \"x\"\n").unwrap();
    let (run, _) = Bee::with_config(&["ref", "list-refs", "--json"], dir).run();
    assert_eq!(run.code, 2, "{run:?}");
    assert!(run.stdout.contains("endpont"), "{run:?}");
}
