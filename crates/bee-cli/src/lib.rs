//! bee: the command-line client.
//!
//! Every API call is a command `bee <noun> <verb> [flags]`, generated at runtime from the
//! `bee.v1` descriptors: the noun is the service name without `Service` in kebab-case, the
//! verb the method name in kebab-case, and each request field a flag. Hand-written commands
//! (`login`, `logout`, `whoami`, `schema`, `help-agent`, `mcp`, and ergonomic commands registered in
//! [`commands::extensions`]) sit next to them.
//!
//! With `--json` every command prints exactly one document,
//! `{"schema": "bee.v1.<noun>.<verb>", "data": ...}` (one per line for streams), or the error
//! document `{"schema": "bee.v1.error", "error": {...}}`, and exits with a fixed code
//! ([`error::ExitCode`]). The output schemas live in `schemas/v1/cli/` ([`schemas`]).

pub mod catalog;
pub mod cli;
pub mod client_config;
pub mod commands;
pub mod context;
pub mod credentials;
pub mod daemon_glue;
pub mod error;
pub mod fields;
pub mod generated;
pub mod output;
pub mod rpc;
pub mod schemas;
pub mod wc;

use clap::ArgMatches;
use clap::error::ErrorKind;

use crate::catalog::catalog;
use crate::cli::{Expand, root, selected_noun};
use crate::context::{Context, GlobalOptions, print_error};
use crate::error::{CliError, ExitCode};

/// Runs the CLI with these arguments (the first is the program name) and returns the process
/// exit code.
pub fn run<I, S>(args: I) -> i32
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.into().to_string_lossy().into_owned())
        .collect();
    let json = args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let noun = selected_noun(&args);
    let expand = noun.as_deref().map_or(Expand::None, Expand::Noun);
    let matches = match root(expand).try_get_matches_from(&args) {
        Ok(matches) => matches,
        Err(error) => return clap_failure(&error, json),
    };
    let mut options = match GlobalOptions::from_matches(&matches) {
        Ok(options) => options,
        Err(error) => {
            print_error(json, &error);
            return error.exit.code();
        }
    };
    // The client configuration file and its environment overrides fill what no flag set.
    let defaults = client_config::load().and_then(|_| client_config::timeout());
    match defaults {
        Ok(timeout) => {
            options.timeout = options.timeout.or(timeout);
            options.json = options.json || client_config::json();
            options.no_daemon = options.no_daemon || !client_config::daemon_allowed();
        }
        Err(error) => {
            print_error(json, &error);
            return error.exit.code();
        }
    }
    if let Some(dir) = &options.dir
        && let Err(e) = std::env::set_current_dir(dir)
    {
        let error = CliError::usage("bad_directory", format!("-C {}: {e}", dir.display()));
        print_error(options.json, &error);
        return error.exit.code();
    }
    let mut ctx = Context::new(options);
    if let Some(("import", import_args)) = matches.subcommand()
        && let Some(("github", github_args)) = import_args.subcommand()
    {
        return match commands::run_import_github(&ctx, github_args) {
            Ok(code) => code,
            Err(error) => {
                ctx.print_error(&error);
                error.exit.code()
            }
        };
    }
    match dispatch(&mut ctx, &matches) {
        Ok(()) => ExitCode::Ok.code(),
        Err(error) => {
            ctx.print_error(&error);
            error.exit.code()
        }
    }
}

/// Prints help or version output, or a usage error, for a failed parse.
fn clap_failure(error: &clap::Error, json: bool) -> i32 {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            print!("{}", error.render());
            ExitCode::Ok.code()
        }
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand if !json => {
            print!("{}", error.render());
            ExitCode::Usage.code()
        }
        _ => {
            let text = error.render().to_string();
            let message = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take_while(|l| !l.starts_with("For more information"))
                .collect::<Vec<_>>()
                .join("\n");
            let usage = CliError::usage(
                "usage",
                if message.is_empty() {
                    "missing command; run `bee --help`".to_owned()
                } else {
                    message
                },
            );
            if json {
                print_error(true, &usage);
            } else {
                eprint!("{text}");
            }
            usage.exit.code()
        }
    }
}

fn dispatch(ctx: &mut Context, matches: &ArgMatches) -> Result<(), CliError> {
    let Some((name, sub)) = matches.subcommand() else {
        return Err(CliError::usage(
            "usage",
            "missing command; run `bee --help`",
        ));
    };
    if let Some(command) = commands::find(&[name]) {
        return commands::run(command, ctx, sub);
    }
    let Some((verb_name, verb_args)) = sub.subcommand() else {
        return Err(CliError::usage(
            "usage",
            format!("missing verb; run `bee {name} --help`"),
        ));
    };
    if let Some(command) = commands::find(&[name, verb_name]) {
        return commands::run(command, ctx, verb_args);
    }
    let catalog = catalog();
    let noun = catalog
        .noun(name)
        .ok_or_else(|| CliError::usage("usage", format!("unknown command `bee {name}`")))?;
    let verb = noun.verb(verb_name).ok_or_else(|| {
        CliError::usage("usage", format!("unknown command `bee {name} {verb_name}`"))
    })?;
    generated::run(ctx, catalog, noun, verb, verb_args)
}
