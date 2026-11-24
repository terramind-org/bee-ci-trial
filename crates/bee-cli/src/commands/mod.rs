//! The command registry: hand-written commands that sit next to the commands generated from
//! the RPC catalog.
//!
//! Generated commands are `bee <noun> <verb>`, one per RPC (see [`crate::cli`]). Everything
//! else implements [`Command`] and is listed by [`registry`]: the built-ins here (`login`,
//! `logout`, `whoami`, `schema`, `help-agent`, `mcp`) and the ergonomic commands other modules add in
//! [`extensions`] (for example working-copy commands such as `status` or `commit`).
//!
//! A registered command has a path of one or two words: `["status"]` is a top-level command;
//! `["git", "credential"]` adds a verb under the generated `git` noun (or under a new group if
//! no noun has that name). Paths must not collide with each other or with a generated
//! `(noun, verb)`; a test enforces it. Each command's `--json` output is
//! `{"schema": "bee.v1.<path joined by .>", "data": ...}` and is described by the generated
//! file `schemas/v1/cli/<path joined by .>.schema.json`, built from [`Command::output_schema`].

mod auth;
mod extensions;
mod git;
mod import;
mod mcp;
mod meta;

use clap::ArgMatches;
use serde_json::Value;

use crate::context::Context;
use crate::error::CliError;
use crate::output;

pub use extensions::extensions;

/// A hand-written command.
pub trait Command: Send + Sync {
    /// Where the command lives: one word (top level) or `[noun, verb]`.
    fn path(&self) -> &'static [&'static str];

    /// One-line description for the parent's `--help`.
    fn about(&self) -> &'static str;

    /// Adds arguments, long help and an `EXAMPLE:` section to the command (whose name and
    /// about are already set).
    fn configure(&self, command: clap::Command) -> clap::Command {
        command
    }

    /// JSON Schema (draft 2020-12) of the `data` member of the `--json` output; for a
    /// streaming command, of each item.
    fn output_schema(&self) -> Value;

    /// True if the command prints a stream of items (NDJSON with `--json`) through
    /// [`Context::emit`] instead of returning one document.
    fn streaming(&self) -> bool {
        false
    }

    /// Runs the command. Returns the `data` to print, or `None` when the command already
    /// printed its items with [`Context::emit`].
    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError>;

    /// Readable rendering of `data` when `--json` is not given.
    fn render(&self, data: &Value) -> String {
        output::render_value(data)
    }
}

/// Every registered command: built-ins first, then [`extensions`].
pub fn registry() -> Vec<&'static dyn Command> {
    let mut all: Vec<&'static dyn Command> = vec![
        &auth::Login,
        &auth::Logout,
        &auth::WhoAmI,
        &meta::Schema,
        &meta::HelpAgent,
        &mcp::Mcp,
        &git::Credential,
        &git::Setup,
        &import::Github,
    ];
    all.extend(extensions());
    all
}

/// Runs the `bee import github` compatibility command.  It is dispatched specially because
/// its child process's exit status is part of the command contract.
pub fn run_import_github(ctx: &Context, args: &ArgMatches) -> Result<i32, CliError> {
    import::run_github(ctx, args)
}

/// The registered command at `path`.
pub fn find(path: &[&str]) -> Option<&'static dyn Command> {
    registry().into_iter().find(|c| c.path() == path)
}

/// The schema id of a registered command, e.g. `bee.v1.whoami`.
pub fn schema_of(command: &dyn Command) -> String {
    output::schema_id(command.path())
}

/// Runs a registered command and prints its result.
pub fn run(command: &dyn Command, ctx: &mut Context, args: &ArgMatches) -> Result<(), CliError> {
    if let Some(data) = command.run(ctx, args)? {
        let human = command.render(&data);
        ctx.print_document(&schema_of(command), data, &human)?;
    }
    Ok(())
}

/// Shorthand for the `$ref` of a message schema by its published URI.
pub fn message_ref(message: &str) -> Value {
    serde_json::json!({ "$ref": crate::schemas::message_uri(message) })
}
