//! The GitHub importer compatibility command.
//!
//! `bee-github` remains a separate binary: it is in the same crate layer as this CLI, so
//! linking it would violate the layer contract.  This command finds that binary and delegates to
//! it, passing resolved Beehive credentials through the environment rather than the process list.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::Command;
use crate::context::{Context, ENV_ENDPOINT, ENV_TOKEN};
use crate::error::CliError;
use crate::output;

/// `bee import github <owner>/<repo>` delegates to `bee-github import`.
pub struct Github;

impl Command for Github {
    fn path(&self) -> &'static [&'static str] {
        &["import", "github"]
    }

    fn about(&self) -> &'static str {
        "Import a GitHub repository (delegates to the bee-github package)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("repo")
                    .required(true)
                    .help("GitHub repository, `owner/repo`."),
            )
            .arg(
                Arg::new("mirror-only")
                    .long("mirror-only")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("resume").long("resume").action(ArgAction::SetTrue))
            .arg(
                Arg::new("dry-run")
                    .long("dry-run")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("archive").long("archive").value_name("PATH"))
            .arg(
                Arg::new("no-wiki")
                    .long("no-wiki")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("no-lfs").long("no-lfs").action(ArgAction::SetTrue))
            .arg(Arg::new("api-url").long("api-url").value_name("URL"))
            .arg(Arg::new("hub-url").long("hub-url").value_name("URL"))
            .arg(Arg::new("org").long("org").value_name("ORG"))
            .arg(Arg::new("state").long("state").value_name("DIR"))
            .long_about(
                "Imports a GitHub repository through the separately installed `bee-github` \
                 package. The endpoint and Beehive credential use the normal `bee` global \
                 options; the credential is passed only through the child environment, never its \
                 command line. Set `BEE_GITHUB_BIN` to an absolute importer path, or install \
                 the importer beside the `bee` executable. GitHub credentials are discovered \
                 by `bee-github` from its \
                 documented environment variables, GitHub App options, or the gh CLI.",
            )
            .after_help(
                "EXAMPLE:\n  bee import github acme/api --endpoint http://127.0.0.1:7420\n\n\
                 Install the importer with the Bee GitHub integration package beside `bee`, \
                 or set BEE_GITHUB_BIN to its absolute path.",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "description": "The JSON result emitted by bee-github import.",
            "additionalProperties": true
        })
    }

    fn run(&self, _ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        unreachable!("bee import github is dispatched before ordinary commands")
    }
}

/// Delegate the parsed command and return the importer's exact exit status.
pub fn run_github(ctx: &Context, args: &ArgMatches) -> Result<i32, CliError> {
    let repo = args.get_one::<String>("repo").expect("required by clap");
    let endpoint = ctx.endpoint()?.to_string();
    let token = ctx.token()?;
    let mut child = Process::new(importer_path()?);
    child.arg("import").arg(repo).env(ENV_ENDPOINT, endpoint);
    if let Some(token) = token {
        child.env(ENV_TOKEN, token);
    }
    for (name, flag) in [
        ("mirror-only", "--mirror-only"),
        ("resume", "--resume"),
        ("dry-run", "--dry-run"),
        ("no-wiki", "--no-wiki"),
        ("no-lfs", "--no-lfs"),
    ] {
        if args.get_flag(name) {
            child.arg(flag);
        }
    }
    for (name, flag) in [
        ("archive", "--archive"),
        ("api-url", "--api-url"),
        ("hub-url", "--hub-url"),
        ("org", "--org"),
        ("state", "--state"),
    ] {
        if let Some(value) = args.get_one::<String>(name) {
            child.arg(flag).arg(value);
        }
    }
    if !ctx.json() {
        return child
            .status()
            .map(|status| status.code().unwrap_or(1))
            .map_err(start_error);
    }
    child.arg("--json").stdout(Stdio::piped());
    let output = child.output().map_err(start_error)?;
    let status = output.status.code().unwrap_or(1);
    if output.status.success() {
        let data: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
            CliError::internal(format!("`bee-github --json` returned invalid JSON: {e}"))
        })?;
        let document = serde_json::to_string(&output::envelope("bee.v1.import.github", data))
            .map_err(|e| CliError::internal(e.to_string()))?;
        std::io::stdout()
            .lock()
            .write_all(format!("{document}\n").as_bytes())
            .map_err(|e| CliError::internal(format!("cannot write importer output: {e}")))?;
    } else {
        // The importer owns its failure text and status. Keep its JSON visible without
        // allowing it to violate the CLI's one-document JSON contract.
        let data: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(
            |_| json!({"reason": "INTERNAL", "message": "bee-github returned invalid JSON"}),
        );
        let error = CliError::internal(data.to_string());
        ctx.print_error(&error);
    }
    Ok(status)
}

fn importer_path() -> Result<PathBuf, CliError> {
    if let Some(path) = std::env::var_os("BEE_GITHUB_BIN") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(CliError::unavailable(
                "BEE_GITHUB_BIN must be an absolute path to an existing regular file",
            )
            .with_payload("subcode", json!("github_importer_start_failed")));
        }
        let path = std::fs::canonicalize(&path).map_err(|_| {
            CliError::unavailable(
                "BEE_GITHUB_BIN must be an absolute path to an existing regular file",
            )
            .with_payload("subcode", json!("github_importer_start_failed"))
        })?;
        if !is_regular_file(&path) {
            return Err(CliError::unavailable(
                "BEE_GITHUB_BIN must be an absolute path to an existing regular file",
            )
            .with_payload("subcode", json!("github_importer_start_failed")));
        }
        return Ok(path);
    }

    let executable = std::env::current_exe().map_err(|e| {
        CliError::unavailable(format!("cannot locate the running `bee` executable: {e}"))
            .with_payload("subcode", json!("github_importer_start_failed"))
    })?;
    let executable = std::fs::canonicalize(executable).map_err(|e| {
        CliError::unavailable(format!("cannot resolve the running `bee` executable: {e}"))
            .with_payload("subcode", json!("github_importer_start_failed"))
    })?;
    let importer = executable
        .parent()
        .map(|directory| directory.join(importer_file_name()))
        .ok_or_else(|| {
            CliError::unavailable("cannot locate the directory of the running `bee` executable")
                .with_payload("subcode", json!("github_importer_start_failed"))
        })?;
    if is_regular_file(&importer) {
        Ok(importer)
    } else {
        Err(importer_missing_error())
    }
}

#[cfg(windows)]
const fn importer_file_name() -> &'static str {
    "bee-github.exe"
}

#[cfg(not(windows))]
const fn importer_file_name() -> &'static str {
    "bee-github"
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn importer_missing_error() -> CliError {
    CliError::unavailable(
        "`bee-github` is not installed; set BEE_GITHUB_BIN to an absolute importer path, \
         or install the Bee GitHub integration package beside the `bee` executable, then \
         retry `bee import github`",
    )
    .with_payload("subcode", json!("github_importer_missing"))
}

fn start_error(e: std::io::Error) -> CliError {
    if e.kind() == std::io::ErrorKind::NotFound {
        importer_missing_error()
    } else {
        CliError::unavailable(format!("cannot start `bee-github`: {e}"))
            .with_payload("subcode", json!("github_importer_start_failed"))
    }
}
