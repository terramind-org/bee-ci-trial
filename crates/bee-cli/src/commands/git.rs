//! `bee git credential` (a git credential helper) and `bee git setup` (point a git
//! repository at Beehive), next to the generated `bee git <rpc>` commands such as
//! `git-preflight` and `create-git-credential` (`docs/spec/git-gateway.md` §2).

use std::path::Path;
use std::process::Command as Process;

use bee_proto::methods::find_method;
use bee_proto::v1::ErrorReason;
use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::Command;
use crate::context::Context;
use crate::error::CliError;
use crate::rpc::Endpoint;

/// The user name git sends with the token (the gateway ignores it).
const GIT_USER: &str = "x-bee-token";

/// `bee git credential <get|store|erase>`: the git credential helper protocol
/// (`gitcredentials(7)`). `get` answers with the bee token for the configured endpoint's
/// host and nothing for any other host; `store` and `erase` keep nothing (the token belongs
/// to `bee login`).
pub struct Credential;

/// The `key=value` attributes git writes to a helper.
fn attributes(input: &str) -> Vec<(String, String)> {
    input
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// True when `protocol` and `host` name the endpoint (`host` as git sends it: `name[:port]`,
/// the port omitted when it is the protocol's default).
fn same_server(endpoint: &Endpoint, protocol: &str, host: &str) -> bool {
    let Endpoint::Http {
        host: eh, port: ep, ..
    } = endpoint
    else {
        return false;
    };
    if protocol != "http" {
        return false;
    }
    let (h, p) = match host.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
            (h.trim_matches(['[', ']']), p.parse().unwrap_or(0))
        }
        _ => (host, 80u16),
    };
    h.eq_ignore_ascii_case(eh) && p == *ep
}

impl Command for Credential {
    fn path(&self) -> &'static [&'static str] {
        &["git", "credential"]
    }

    fn about(&self) -> &'static str {
        "Git credential helper: answers git with the bee token for this endpoint."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("operation")
                    .required(true)
                    .value_parser(["get", "store", "erase"])
                    .help("The helper operation git asks for."),
            )
            .long_about(
                "A git credential helper (gitcredentials(7)). Git writes the request \
                 attributes (protocol, host, path) on stdin; for `get` the helper answers \
                 `username` and `password` with the token from --token, BEE_TOKEN or `bee \
                 login`, but only when protocol and host are those of the endpoint (--endpoint, \
                 BEE_ENDPOINT or `bee login`); for any other server it answers nothing so \
                 other helpers can. `store` and `erase` do nothing. `bee git setup` installs \
                 it; by hand: git config credential.http://HOST:PORT.helper '!bee git credential'.",
            )
            .after_help(
                "EXAMPLE:\n  printf 'protocol=http\\nhost=127.0.0.1:8080\\n\\n' | bee git credential get",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["operation", "answered"],
            "additionalProperties": false,
            "properties": {
                "operation": {"enum": ["get", "store", "erase"]},
                "answered": {"type": "boolean", "description": "True when `get` returned a credential."},
                "username": {"type": "string"},
                "password": {"type": "string", "description": "The token."}
            }
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let op = args
            .get_one::<String>("operation")
            .map(String::as_str)
            .unwrap_or("get");
        let input = ctx.read_stdin()?;
        if op != "get" {
            return Ok(Some(json!({ "operation": op, "answered": false })));
        }
        let attrs = attributes(&input);
        let get = |k: &str| {
            attrs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        let endpoint = ctx.endpoint()?;
        let token = ctx.token()?;
        match token {
            Some(token) if same_server(&endpoint, get("protocol"), get("host")) => {
                Ok(Some(json!({
                    "operation": "get",
                    "answered": true,
                    "username": GIT_USER,
                    "password": token,
                })))
            }
            _ => Ok(Some(json!({ "operation": "get", "answered": false }))),
        }
    }

    fn render(&self, data: &Value) -> String {
        match (data["username"].as_str(), data["password"].as_str()) {
            (Some(u), Some(p)) => format!("username={u}\npassword={p}"),
            _ => String::new(),
        }
    }
}

/// `bee git setup --repo <org/name>`: points the git repository in the working directory
/// at Beehive (remote, credential helper, protocol v2) and runs `GitPreflight`.
pub struct Setup;

fn git(dir: &Path, args: &[&str]) -> Result<String, CliError> {
    let out = Process::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| CliError::usage("git_not_found", format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(CliError::new(
            ErrorReason::InvalidArgument,
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )
        .with_payload("subcode", json!("git_failed")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl Command for Setup {
    fn path(&self) -> &'static [&'static str] {
        &["git", "setup"]
    }

    fn about(&self) -> &'static str {
        "Point a git repository at Beehive: remote, credential helper, protocol v2, preflight."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("repo")
                    .long("repo")
                    .required(true)
                    .help("The repo, `org/name`."),
            )
            .arg(
                Arg::new("remote")
                    .long("remote")
                    .default_value("origin")
                    .help("The git remote to add or update."),
            )
            .arg(
                Arg::new("global")
                    .long("global")
                    .action(ArgAction::SetTrue)
                    .help("Write the credential helper and protocol to the global git config."),
            )
            .long_about(
                "Run inside a git repository (or with --dir). Adds or updates the remote \
                 (default `origin`) to <endpoint>/<org>/<name>.git, installs `bee git \
                 credential` as the credential helper for the endpoint (so the token never \
                 appears in the remote URL), sets protocol.version=2, then calls \
                 GitService/GitPreflight with the local git version and reports its problems.",
            )
            .after_help(
                "EXAMPLE:\n  bee git setup --repo acme/app --json\n  bee git setup --repo acme/app --remote bee --global",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["remote", "url", "credentialHelper", "configScope", "protocolVersion", "gitVersion", "preflight"],
            "additionalProperties": false,
            "properties": {
                "remote": {"type": "string"},
                "url": {"type": "string", "description": "The remote URL (no credential in it)."},
                "credentialHelper": {"type": "string", "description": "The helper command installed."},
                "configScope": {"enum": ["local", "global"]},
                "protocolVersion": {"type": "integer"},
                "gitVersion": {"type": "string"},
                "preflight": super::message_ref("bee.v1.GitPreflightResponse")
            }
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let repo = args.get_one::<String>("repo").cloned().unwrap_or_default();
        if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
            return Err(CliError::usage("bad_repo", "--repo is `org/name`"));
        }
        let remote = args
            .get_one::<String>("remote")
            .cloned()
            .unwrap_or_else(|| "origin".into());
        let global = args.get_flag("global");
        let endpoint = ctx.endpoint()?;
        if !matches!(endpoint, Endpoint::Http { .. }) {
            return Err(CliError::usage(
                "bad_endpoint",
                "git needs an http:// endpoint, not a unix socket",
            ));
        }
        let dir = ctx
            .options
            .dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        git(&dir, &["rev-parse", "--git-dir"])?;
        let base = endpoint.to_string();
        let url = format!("{base}/{repo}.git");
        if git(&dir, &["remote", "get-url", &remote]).is_ok() {
            git(&dir, &["remote", "set-url", &remote, &url])?;
        } else {
            git(&dir, &["remote", "add", &remote, &url])?;
        }
        let exe = std::env::current_exe()
            .map_err(|e| CliError::internal(format!("cannot locate the bee binary: {e}")))?;
        let helper = format!(
            "!'{}' --endpoint {base} git credential",
            exe.display().to_string().replace('\'', "'\\''")
        );
        let scope: &[&str] = if global { &["--global"] } else { &[] };
        let set = |key: &str, value: &str| {
            let mut a: Vec<&str> = vec!["config"];
            a.extend_from_slice(scope);
            a.extend(["--replace-all", key, value]);
            git(&dir, &a)
        };
        // An empty helper first resets inherited helpers for this URL, then ours.
        let key = format!("credential.{base}.helper");
        set(&key, "")?;
        let mut add: Vec<&str> = vec!["config"];
        add.extend_from_slice(scope);
        add.extend(["--add", &key, &helper]);
        git(&dir, &add)?;
        set("protocol.version", "2")?;
        let version = git(&dir, &["--version"])?;
        let lfs = git(&dir, &["lfs", "version"]).is_ok();
        let method = find_method("/bee.v1.GitService/GitPreflight")
            .ok_or_else(|| CliError::internal("GitPreflight is not in the catalog"))?;
        let preflight = ctx.call_unary(
            method,
            &json!({
                "repo": { "fullName": repo },
                "clientGitVersion": version,
                "transport": "http",
                "protocolVersion": 2,
                "credentialHelper": helper,
                "lfsInstalled": lfs,
                "push": true,
            }),
        )?;
        Ok(Some(json!({
            "remote": remote,
            "url": url,
            "credentialHelper": helper,
            "configScope": if global { "global" } else { "local" },
            "protocolVersion": 2,
            "gitVersion": version,
            "preflight": preflight,
        })))
    }

    fn render(&self, data: &Value) -> String {
        let mut out = format!(
            "Remote {} -> {}\nCredential helper: {}\nprotocol.version = 2 ({})\n",
            data["remote"].as_str().unwrap_or_default(),
            data["url"].as_str().unwrap_or_default(),
            data["credentialHelper"].as_str().unwrap_or_default(),
            data["configScope"].as_str().unwrap_or_default(),
        );
        let problems = data["preflight"]["problems"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if problems.is_empty() {
            out.push_str("Preflight: ok");
        }
        for p in problems {
            out.push_str(&format!(
                "Preflight {}: {} {}\n",
                p["severity"].as_str().unwrap_or_default(),
                p["message"].as_str().unwrap_or_default(),
                p["fix"].as_str().unwrap_or_default()
            ));
        }
        out.trim_end().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_stop_at_the_blank_line() {
        assert_eq!(
            attributes("protocol=http\nhost=h:1\n\nignored=1\n"),
            vec![
                ("protocol".to_string(), "http".to_string()),
                ("host".to_string(), "h:1".to_string())
            ]
        );
    }

    #[test]
    fn only_the_endpoint_host_is_answered() {
        let e = Endpoint::parse("http://127.0.0.1:8080").unwrap();
        assert!(same_server(&e, "http", "127.0.0.1:8080"));
        assert!(!same_server(&e, "http", "127.0.0.1:8081"));
        assert!(!same_server(&e, "https", "127.0.0.1:8080"));
        assert!(!same_server(&e, "http", "github.com"));
        let d = Endpoint::parse("http://bee.example").unwrap();
        assert!(same_server(&d, "http", "bee.example"));
        assert!(same_server(&d, "http", "BEE.example:80"));
    }
}
