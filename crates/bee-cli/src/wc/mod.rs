//! Ergonomic commands: the working copy (`clone`, `status`, `commit`, `sync`, `land`, ...),
//! history over the API (`log`, `diff`, `show`, `blame`, `cat`) and short aliases for common
//! generated commands (`cr open/list/view`, `auth token`).
//!
//! Every command here is a [`crate::commands::Command`], so it gets the `--json` envelope,
//! a generated output schema under `schemas/v1/cli/`, and the fixed exit codes: 3 for
//! conflicts, 4 when uncommitted changes refuse an operation, 5 for policy denials.

mod aliases;
mod commands;
mod daemon_cmds;
mod evidence;
mod history;
mod schema;
mod transport;

use std::path::{Path, PathBuf};

use bee_proto::errors::parse_reason;
use bee_proto::v1::ErrorReason;
use bee_wc::{ExitClass, OpenOptions, Signature, WcError, WorkingCopy};
use clap::ArgMatches;
use serde_json::{Value, json};

use crate::commands::Command;
use crate::context::{Context, ENV_ENDPOINT};
use crate::error::{CliError, ExitCode};

pub use transport::CliTransport;

/// Every ergonomic command, in help order.
pub fn commands() -> Vec<&'static dyn Command> {
    let mut all = commands::all();
    all.extend(history::all());
    all.extend(aliases::all());
    all.extend(daemon_cmds::all());
    all
}

/// The CLI error for a working-copy error, with its fixed exit code.
pub fn cli_error(e: WcError) -> CliError {
    let mut c = match &e.detail {
        Some(d) => CliError::from_detail(d, &e.message),
        None => {
            let reason = parse_reason(&e.reason).unwrap_or(ErrorReason::Internal);
            let mut c = CliError::new(reason, e.message.clone());
            if !e.subcode.is_empty() {
                c = c.with_payload("subcode", json!(e.subcode));
            }
            c
        }
    };
    c.exit = match e.exit_class() {
        ExitClass::Usage => ExitCode::Usage,
        ExitClass::Conflicts => ExitCode::Conflicts,
        ExitClass::Dirty => ExitCode::DirtyRefusal,
        ExitClass::PolicyDenied => ExitCode::PolicyDenied,
        ExitClass::Error => ExitCode::for_reason(&c.reason),
    };
    if !e.paths.is_empty() {
        c = c.with_payload("paths", json!(e.paths));
    }
    if let Some(report) = e.report {
        c = c.with_payload("report", *report);
    }
    c
}

/// The working copy containing the current directory.
pub fn root() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::internal(format!("cannot read the current directory: {e}")))?;
    bee_wc::find_root(&cwd).ok_or_else(|| {
        cli_error(WcError::state(
            "not_a_working_copy",
            format!(
                "{} is not inside a bee working copy; run `bee clone <org/repo>` first",
                cwd.display()
            ),
        ))
    })
}

/// The endpoint to use for a working copy: `--endpoint`, then `BEE_ENDPOINT`, then the
/// one the working copy was cloned from, then the saved credentials.
pub fn endpoint_for(ctx: &Context, wc_endpoint: Option<&str>) -> Result<String, CliError> {
    if let Some(e) = ctx.options.endpoint.clone() {
        return Ok(e);
    }
    if let Ok(e) = std::env::var(ENV_ENDPOINT)
        && !e.trim().is_empty()
    {
        return Ok(e);
    }
    if let Some(e) = wc_endpoint {
        return Ok(e.to_owned());
    }
    ctx.endpoint().map(|e| e.to_string())
}

/// Opens the working copy around the current directory, with its transport.
pub fn open(ctx: &Context) -> Result<(WorkingCopy, CliTransport), CliError> {
    let root = root()?;
    let config = bee_wc::read_config(&root).map_err(cli_error)?;
    let transport = CliTransport::new(ctx, config.endpoint.as_deref());
    let opts = OpenOptions {
        faults: bee_wc::Faults::from_env(),
        ..OpenOptions::default()
    };
    let wc = WorkingCopy::open(&root, &transport, &opts).map_err(cli_error)?;
    Ok((wc, transport))
}

/// Author and time for new changesets: `--author`/`BEE_AUTHOR` (else `$USER`), and
/// `--date`/`BEE_DATE` as Unix seconds (else now).
pub fn signature(args: &ArgMatches) -> Result<Signature, CliError> {
    let name = arg(args, "author")
        .or_else(crate::client_config::author)
        .or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "bee".to_owned());
    let date =
        arg(args, "date").or_else(|| std::env::var("BEE_DATE").ok().filter(|s| !s.is_empty()));
    let time = match date {
        Some(d) => d.trim().parse::<i64>().map_err(|_| {
            CliError::usage("bad_flag", format!("--date: {d:?} is not Unix seconds"))
        })?,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    Ok(Signature {
        name,
        time,
        tz_offset_minutes: 0,
    })
}

/// A string argument, if the command defines it and it was given.
pub fn arg(args: &ArgMatches, id: &str) -> Option<String> {
    match args.try_get_one::<String>(id) {
        Ok(Some(v)) => Some(v.clone()),
        _ => None,
    }
}

/// Every value of a repeatable or multi-valued argument.
pub fn args_of(args: &ArgMatches, id: &str) -> Vec<String> {
    match args.try_get_many::<String>(id) {
        Ok(Some(v)) => v.cloned().collect(),
        _ => Vec::new(),
    }
}

/// A flag's value (false if the command does not define it).
pub fn flag(args: &ArgMatches, id: &str) -> bool {
    matches!(args.try_get_one::<bool>(id), Ok(Some(true)))
}

/// Converts paths given on the command line (relative to the current directory) into
/// repo paths relative to `root`.
pub fn repo_paths(root: &Path, given: &[String]) -> Result<Vec<String>, CliError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::internal(format!("cannot read the current directory: {e}")))?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let mut out = Vec::new();
    for g in given {
        let joined = cwd.join(g);
        let mut parts: Vec<String> = Vec::new();
        let rel = joined.strip_prefix(&root).map_err(|_| {
            CliError::usage("path_outside", format!("{g} is outside the working copy"))
        })?;
        for c in rel.components() {
            match c {
                std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                std::path::Component::ParentDir if parts.pop().is_none() => {
                    return Err(CliError::usage(
                        "path_outside",
                        format!("{g} is outside the working copy"),
                    ));
                }
                _ => {}
            }
        }
        out.push(if parts.is_empty() {
            ".".to_owned()
        } else {
            parts.join("/")
        });
    }
    Ok(out)
}

/// Serializes a report as the command's data.
pub fn data<T: WcSerialize>(report: &T) -> Result<Option<Value>, CliError> {
    report.json().map(Some).map_err(CliError::internal)
}

/// Working-copy values that serialize as command data.
pub trait WcSerialize {
    /// The JSON form.
    fn json(&self) -> Result<Value, String>;
}

impl WcSerialize for bee_wc::OpReport {
    fn json(&self) -> Result<Value, String> {
        bee_wc::to_json(self)
    }
}

impl WcSerialize for bee_wc::StatusReport {
    fn json(&self) -> Result<Value, String> {
        bee_wc::to_json(self)
    }
}
