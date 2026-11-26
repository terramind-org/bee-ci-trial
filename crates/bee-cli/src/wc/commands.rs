//! Working-copy commands.

use bee_wc::{CommitOptions, CreateOptions, OpReport, Pick, Sparse};
use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::{
    CliTransport, arg, args_of, cli_error, data, flag, open, repo_paths, schema, signature,
};
use crate::commands::Command;
use crate::context::Context;
use crate::daemon_glue;
use crate::error::CliError;

/// Every working-copy command.
pub fn all() -> Vec<&'static dyn Command> {
    vec![
        &CloneCmd,
        &Init,
        &Status,
        &Commit,
        &Amend,
        &Describe,
        &Sync,
        &Resolve,
        &Land,
        &Goto,
        &Undo,
        &OpLog,
        &StackList,
        &StackNew,
        &StackEdit,
        &StackSquash,
        &StackSplit,
        &SparseList,
        &SparseSet,
        &SparseAdd,
        &SparseRemove,
    ]
}

const EXIT_NOTE: &str = "Exit codes: 0 ok, 3 conflicts (markers written; edit, then `bee \
                         resolve`), 4 refused because of uncommitted changes, 5 policy denied.";

fn author_args(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new("author").long("author").value_name("NAME").help(
                "Author name. Default: BEE_AUTHOR, then `author` in config.toml, then $USER.",
            ),
        )
        .arg(
            Arg::new("date")
                .long("date")
                .value_name("UNIX_SECONDS")
                .help("Author time. Default: BEE_DATE, then now."),
        )
}

fn message_arg(required: bool) -> Arg {
    Arg::new("message")
        .short('m')
        .long("message")
        .value_name("TEXT")
        .required(required)
        .help("Change message (first line is the subject).")
}

fn paths_arg(help: &'static str) -> Arg {
    Arg::new("paths")
        .value_name("PATH")
        .num_args(0..)
        .action(ArgAction::Append)
        .help(help)
}

fn change_arg(help: &'static str) -> Arg {
    Arg::new("change")
        .long("change")
        .value_name("CHANGE")
        .help(help)
}

/// Readable rendering of a mutating command's report.
pub fn render_op(data: &Value) -> String {
    let mut out = data["summary"].as_str().unwrap_or_default().to_owned();
    if out.is_empty() {
        out = data["op"].as_str().unwrap_or_default().to_owned();
    }
    if let Some(land) = data.get("land").filter(|l| !l.is_null()) {
        out.push_str(&format!(
            "\n{} at version {} ({} attempt(s)): {}",
            land["ref"].as_str().unwrap_or_default(),
            land["version"],
            land["attempts"],
            land["target"].as_str().unwrap_or_default()
        ));
        for l in land["landed"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "\n  {} -> {}{}",
                l["changeId"].as_str().unwrap_or_default(),
                l["result"].as_str().unwrap_or_default(),
                if l["skipped"].as_bool() == Some(true) {
                    " (already landed)"
                } else {
                    ""
                }
            ));
        }
    }
    let written = data["written"].as_u64().unwrap_or(0);
    let removed = data["removed"].as_u64().unwrap_or(0);
    if written + removed > 0 {
        out.push_str(&format!("\nfiles: {written} written, {removed} removed"));
    }
    out.push_str(&render_stack(&data["stack"]));
    for c in data["conflicts"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\nconflict ({}): {}{}",
            c["kind"].as_str().unwrap_or_default(),
            c["path"].as_str().unwrap_or_default(),
            if c["resolved"].as_bool() == Some(true) {
                " (resolved)"
            } else {
                ""
            }
        ));
    }
    for h in data["hidden"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\nnot written ({}): {}",
            h["reason"].as_str().unwrap_or_default(),
            h["path"].as_str().unwrap_or_default()
        ));
    }
    out
}

fn render_stack(stack: &Value) -> String {
    let mut out = String::new();
    let items = stack.as_array().cloned().unwrap_or_default();
    if !items.is_empty() {
        out.push_str("\nstack (top first):");
    }
    for c in items.iter().rev() {
        out.push_str(&format!(
            "\n  {} {} {} {}",
            if c["head"].as_bool() == Some(true) {
                "@"
            } else {
                " "
            },
            c["index"],
            c["changeId"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(14)
                .collect::<String>(),
            c["subject"].as_str().unwrap_or_default()
        ));
    }
    out
}

fn op_data(r: Result<OpReport, bee_wc::WcError>) -> Result<Option<Value>, CliError> {
    data(&r.map_err(cli_error)?)
}

fn sparse_from(values: &[String]) -> Result<Sparse, CliError> {
    Sparse::from_prefixes(values).map_err(cli_error)
}

/// `bee clone`.
pub struct CloneCmd;

impl Command for CloneCmd {
    fn path(&self) -> &'static [&'static str] {
        &["clone"]
    }

    fn about(&self) -> &'static str {
        "Make a working copy of a repo (optionally sparse): files of the target ref, no history."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Make a working copy of REPO (org/name) in DIR (default: the repo name). Only the \
                 files of the target ref inside the sparse profile are fetched; history stays on \
                 the server (`bee log`, `bee blame`). The endpoint is remembered in \
                 .bee/config.json.",
            )
            .arg(
                Arg::new("repo")
                    .value_name("REPO")
                    .required(true)
                    .help("Repo as org/name."),
            )
            .arg(
                Arg::new("directory")
                    .value_name("DIR")
                    .help("Directory to create."),
            )
            .arg(
                Arg::new("ref")
                    .long("ref")
                    .value_name("REF")
                    .default_value("main")
                    .help("Protected ref to sync with and land on."),
            )
            .arg(
                Arg::new("sparse")
                    .long("sparse")
                    .value_name("PREFIX")
                    .action(ArgAction::Append)
                    .help("Check out only paths under PREFIX (repeatable)."),
            )
            .after_help(
                "EXAMPLE:\n  bee clone dev/playground --endpoint http://127.0.0.1:8080 --json\n  \
                 bee clone acme/mono mono --sparse services/api",
            )
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        create(ctx, args, false)
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

fn create(ctx: &Context, args: &ArgMatches, adopt: bool) -> Result<Option<Value>, CliError> {
    create_with(ctx, args, adopt, None)
}

/// `clone`, `init` and `checkout --virtual`: `sparse` replaces the `--sparse` profile.
pub(super) fn create_with(
    ctx: &Context,
    args: &ArgMatches,
    adopt: bool,
    sparse: Option<Sparse>,
) -> Result<Option<Value>, CliError> {
    let repo = arg(args, "repo").unwrap_or_default();
    if !repo.contains('/') {
        return Err(CliError::usage(
            "bad_repo",
            format!("{repo:?} is not org/name"),
        ));
    }
    let dir = match arg(args, "directory") {
        Some(d) => std::path::PathBuf::from(d),
        None if adopt => std::path::PathBuf::from("."),
        None => std::path::PathBuf::from(repo.rsplit('/').next().unwrap_or("repo")),
    };
    let cwd = std::env::current_dir().map_err(|e| CliError::internal(e.to_string()))?;
    let dir = cwd.join(dir);
    let transport = CliTransport::new(ctx, None);
    let opts = CreateOptions {
        endpoint: transport.endpoint(),
        repo,
        target_ref: arg(args, "ref").unwrap_or_else(|| "main".into()),
        sparse: match sparse {
            Some(s) => s,
            None => sparse_from(&args_of(args, "sparse"))?,
        },
        seed: None,
        adopt,
    };
    let open = bee_wc::OpenOptions {
        faults: bee_wc::Faults::from_env(),
        ..bee_wc::OpenOptions::default()
    };
    let (wc, report) = bee_wc::create(&dir, &opts, &transport, &open).map_err(cli_error)?;
    drop(wc);
    if let Some(link) = transport.daemon() {
        link.watch(&dir);
    }
    data(&report)
}

/// `bee init`.
pub struct Init;

impl Command for Init {
    fn path(&self) -> &'static [&'static str] {
        &["init"]
    }

    fn about(&self) -> &'static str {
        "Turn an existing directory into a working copy of a repo, keeping files already there."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Bind DIR (default: the current directory) to REPO's target ref. Files of the ref \
                 that are missing are written; files already present are kept and show up in \
                 `bee status` as changes.",
            )
            .arg(
                Arg::new("repo")
                    .long("repo")
                    .value_name("REPO")
                    .required(true)
                    .help("Repo as org/name."),
            )
            .arg(
                Arg::new("directory")
                    .value_name("DIR")
                    .help("Directory (default: .)."),
            )
            .arg(
                Arg::new("ref")
                    .long("ref")
                    .value_name("REF")
                    .default_value("main")
                    .help("Protected ref to sync with and land on."),
            )
            .arg(
                Arg::new("sparse")
                    .long("sparse")
                    .value_name("PREFIX")
                    .action(ArgAction::Append)
                    .help("Check out only paths under PREFIX (repeatable)."),
            )
            .after_help("EXAMPLE:\n  bee init --repo dev/playground --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        create(ctx, args, true)
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee status`.
pub struct Status;

impl Command for Status {
    fn path(&self) -> &'static [&'static str] {
        &["status"]
    }

    fn about(&self) -> &'static str {
        "Show uncommitted changes, the local stack and any rebase in progress (no network)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Compare the working tree with the checked-out change using the stat cache \
                 (changed files are re-hashed with BLAKE3). There is no staging area: every \
                 tracked file, and every untracked file not ignored by .gitignore, is part of the \
                 next commit. Never touches the network. Also finishes an operation a crash \
                 interrupted (`recovered`).",
            )
            .after_help("EXAMPLE:\n  bee status --json")
    }

    fn output_schema(&self) -> Value {
        schema::status()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        if let Some(link) = t.daemon() {
            return data(&daemon_glue::status(link, &wc)?);
        }
        let report = wc.status().map_err(cli_error)?;
        data(&report)
    }

    fn render(&self, data: &Value) -> String {
        render_status(data)
    }
}

fn render_status(data: &Value) -> String {
    let mut out = format!(
        "repo {} on {} (version {})\nhead {}",
        data["repo"].as_str().unwrap_or_default(),
        data["targetRef"].as_str().unwrap_or_default(),
        data["ontoVersion"],
        data["head"].as_str().unwrap_or_default()
    );
    if let Some(r) = data["recovered"].as_str() {
        out.push_str(&format!("\nrecovered an interrupted {r}"));
    }
    out.push_str(&render_stack(&data["stack"]));
    if let Some(rebase) = data.get("rebase").filter(|r| !r.is_null()) {
        out.push_str(&format!(
            "\nrebase ({}) stopped at {}: {}; resolve conflicts then `bee resolve`, or \
             `bee sync --abort`",
            rebase["kind"].as_str().unwrap_or_default(),
            rebase["current"].as_str().unwrap_or_default(),
            rebase["currentSubject"].as_str().unwrap_or_default()
        ));
    }
    for c in data["conflicts"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "\n  {} {} ({})",
            if c["resolved"].as_bool() == Some(true) {
                "R"
            } else {
                "C"
            },
            c["path"].as_str().unwrap_or_default(),
            c["kind"].as_str().unwrap_or_default()
        ));
    }
    let changes = data["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() && data["clean"].as_bool() == Some(true) {
        out.push_str("\nclean");
    }
    for c in changes {
        let tag = match c["status"].as_str() {
            Some("added") => "A",
            Some("deleted") => "D",
            _ => "M",
        };
        out.push_str(&format!(
            "\n  {tag} {}",
            c["path"].as_str().unwrap_or_default()
        ));
    }
    out
}

/// `bee commit`.
pub struct Commit;

impl Command for Commit {
    fn path(&self) -> &'static [&'static str] {
        &["commit"]
    }

    fn about(&self) -> &'static str {
        "Record uncommitted changes as a new change on the server (no staging, no push)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .long_about(
                    "Upload changed files and create a changeset (CreateChangeset) whose parent is \
                     the checked-out change; it joins the local stack. The commit is on the server \
                     when the command returns; `bee land` puts the stack on the target ref. With \
                     PATHs, only changes under them are committed.",
                )
                .arg(message_arg(true))
                .arg(paths_arg("Commit only changes under these paths."))
                .arg(
                    Arg::new("allow-empty")
                        .long("allow-empty")
                        .action(ArgAction::SetTrue)
                        .help("Commit even with no changes."),
                )
                .arg(Arg::new("runtime").long("runtime").value_name("NAME").help("Provenance: agent runtime, e.g. claude-code/3.2."))
                .arg(Arg::new("model").long("model").value_name("ID").help("Provenance: model id."))
                .arg(Arg::new("session").long("session").value_name("ID").help("Provenance: session id.")),
        )
        .after_help(format!(
            "EXAMPLE:\n  bee commit -m \"Fix the login redirect\" --json\n  bee commit -m \"Docs only\" docs/\n\n{EXIT_NOTE}"
        ))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let paths = repo_paths(wc.root(), &args_of(args, "paths"))?;
        let provenance = {
            let runtime = arg(args, "runtime").unwrap_or_default();
            let model = arg(args, "model").unwrap_or_default();
            let session = arg(args, "session").unwrap_or_default();
            if runtime.is_empty() && model.is_empty() && session.is_empty() {
                None
            } else {
                Some(bee_proto::v1::Provenance {
                    runtime,
                    model,
                    session,
                    ..Default::default()
                })
            }
        };
        let opts = CommitOptions {
            message: arg(args, "message").unwrap_or_default(),
            author: signature(args)?,
            allow_empty: flag(args, "allow-empty"),
            paths,
            provenance,
        };
        op_data(wc.commit(&t, &opts))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee amend`.
pub struct Amend;

impl Command for Amend {
    fn path(&self) -> &'static [&'static str] {
        &["amend"]
    }

    fn about(&self) -> &'static str {
        "Fold uncommitted changes (and/or a new message) into the checked-out change."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .long_about(
                    "Rewrite the checked-out change with the uncommitted changes folded in; it keeps \
                     its ChangeId, author and time. Changes above it in the stack are rebased onto \
                     the new version server-side. Use `bee stack edit` to amend a lower change.",
                )
                .arg(message_arg(false))
                .arg(paths_arg("Fold in only changes under these paths.")),
        )
        .after_help(format!(
            "EXAMPLE:\n  bee amend --json\n  bee amend -m \"Better subject\"\n\n{EXIT_NOTE}"
        ))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let paths = repo_paths(wc.root(), &args_of(args, "paths"))?;
        let message = arg(args, "message");
        op_data(wc.amend(&t, message.as_deref(), &paths, &signature(args)?))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee describe`.
pub struct Describe;

impl Command for Describe {
    fn path(&self) -> &'static [&'static str] {
        &["describe"]
    }

    fn about(&self) -> &'static str {
        "Change the message of a change of the stack (default: the checked-out one)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .arg(message_arg(true))
                .arg(change_arg("Change to describe: position (1 = bottom), ChangeId or changeset prefix.")),
        )
        .after_help("EXAMPLE:\n  bee describe -m \"Add retry to the uploader\" --json\n  bee describe --change 1 -m \"First step\"")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let message = arg(args, "message").unwrap_or_default();
        op_data(wc.describe(
            &t,
            arg(args, "change").as_deref(),
            &message,
            &signature(args)?,
        ))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee sync` (alias `rebase`).
pub struct Sync;

impl Command for Sync {
    fn path(&self) -> &'static [&'static str] {
        &["sync"]
    }

    fn about(&self) -> &'static str {
        "Rebase the local stack onto the target ref's tip (server-side) and check it out."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .visible_alias("rebase")
                .long_about(
                    "Rebase every change of the local stack onto the current tip of the target ref \
                     with the server's Rebase, then update the working tree. Refused (exit 4) while \
                     there are uncommitted changes. On conflicts the files get diff3 markers \
                     (`<<<<<<< ours` = the target, `>>>>>>> theirs` = your change) and the command \
                     exits 3: edit them and run `bee resolve` (or `bee sync --continue`), or \
                     `bee sync --abort` to go back.",
                )
                .arg(
                    Arg::new("continue")
                        .long("continue")
                        .action(ArgAction::SetTrue)
                        .help("Continue a stopped rebase once conflicts are fixed (same as `bee resolve`)."),
                )
                .arg(
                    Arg::new("abort")
                        .long("abort")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("continue")
                        .help("Abandon a stopped rebase and restore the state before it."),
                ),
        )
        .after_help(format!(
            "EXAMPLE:\n  bee sync --json\n  bee sync --abort\n\n{EXIT_NOTE}"
        ))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        if flag(args, "abort") {
            return op_data(wc.abort(&t));
        }
        if flag(args, "continue") {
            return op_data(wc.resolve(&t, &[], None, &signature(args)?));
        }
        op_data(wc.sync(&t))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee resolve`.
pub struct Resolve;

impl Command for Resolve {
    fn path(&self) -> &'static [&'static str] {
        &["resolve"]
    }

    fn about(&self) -> &'static str {
        "Mark conflicts resolved (markers must be gone) and continue the stopped rebase."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .long_about(
                    "Mark conflicting files (default: all) resolved. A file still holding a \
                     `<<<<<<< ours` or `>>>>>>> theirs` line is refused (exit 3) unless a side is \
                     picked. Once every conflict is resolved, the working tree is recorded as the \
                     change (same ChangeId and message) and the rebase continues; it may stop again \
                     on the next change.",
                )
                .arg(paths_arg("Conflicting paths to resolve (default: all)."))
                .arg(Arg::new("ours").long("ours").action(ArgAction::SetTrue).help("Take the target's side."))
                .arg(
                    Arg::new("theirs")
                        .long("theirs")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("ours")
                        .help("Take your change's side."),
                )
                .arg(
                    Arg::new("base")
                        .long("base")
                        .action(ArgAction::SetTrue)
                        .conflicts_with_all(["ours", "theirs"])
                        .help("Take the merge base."),
                ),
        )
        .after_help(format!(
            "EXAMPLE:\n  bee resolve --json\n  bee resolve src/lib.rs --theirs\n\n{EXIT_NOTE}"
        ))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let paths = repo_paths(wc.root(), &args_of(args, "paths"))?;
        let pick = if flag(args, "ours") {
            Some(Pick::Ours)
        } else if flag(args, "theirs") {
            Some(Pick::Theirs)
        } else if flag(args, "base") {
            Some(Pick::Base)
        } else {
            None
        };
        op_data(wc.resolve(&t, &paths, pick, &signature(args)?))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee land`.
pub struct Land;

impl Command for Land {
    fn path(&self) -> &'static [&'static str] {
        &["land"]
    }

    fn about(&self) -> &'static str {
        "Land the stack (up to the checked-out change) on the target ref, under policy."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Call Land for the stack ending at the checked-out change. The server rebases it \
                 onto the tip and coalesces concurrent lands on one ref; `land.attempts` and \
                 `land.landed` report the outcome. Refused (exit 4) with uncommitted changes. If \
                 the stack conflicts with the target, the conflicts are written to the working \
                 tree (exit 3): fix them, `bee resolve`, then land again. Exit 5 when policy \
                 refuses (missing approvals or checks, leases).",
            )
            .after_help(format!("EXAMPLE:\n  bee land --json\n\n{EXIT_NOTE}"))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        op_data(wc.land(&t))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee goto` (`bee checkout REV` does the same; see `daemon_cmds::Checkout`).
pub struct Goto;

impl Command for Goto {
    fn path(&self) -> &'static [&'static str] {
        &["goto"]
    }

    fn about(&self) -> &'static str {
        "Check out a change of the stack, its base (@base), or any revision."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Move the working copy to REV: a stack position (1 = bottom), a ChangeId, \
                 `@base` (the target-ref tip the stack sits on), or any revision (`main`, \
                 `main~3`, a changeset id). Uncommitted edits to files the move does not touch are \
                 kept; edits it would overwrite refuse the move (exit 4). Leaving a non-empty \
                 stack for an unrelated revision needs --discard-stack (`bee undo` brings it back).",
            )
            .arg(Arg::new("rev").value_name("REV").required(true).help("Where to go."))
            .arg(
                Arg::new("discard-stack")
                    .long("discard-stack")
                    .action(ArgAction::SetTrue)
                    .help("Allow leaving unlanded changes behind."),
            )
            .after_help("EXAMPLE:\n  bee goto 2 --json\n  bee goto main~1 --discard-stack")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let rev = arg(args, "rev").unwrap_or_default();
        op_data(wc.goto(&t, &rev, flag(args, "discard-stack")))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee undo`.
pub struct Undo;

impl Command for Undo {
    fn path(&self) -> &'static [&'static str] {
        &["undo"]
    }

    fn about(&self) -> &'static str {
        "Undo the latest local operation (commit, amend, sync, stack edits, sparse changes)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Revert the most recent operation in the operation log (`bee oplog`) that was not \
                 undone yet. Undoing commit or amend keeps the files, so their changes show as \
                 uncommitted again; other operations restore the files too (refused with exit 4 if \
                 that would overwrite uncommitted changes). Server objects stay. A land cannot be \
                 undone locally.",
            )
            .after_help("EXAMPLE:\n  bee undo --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        op_data(wc.undo(&t))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee oplog`.
pub struct OpLog;

impl Command for OpLog {
    fn path(&self) -> &'static [&'static str] {
        &["oplog"]
    }

    fn about(&self) -> &'static str {
        "List the working copy's operation log (what `bee undo` would revert)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help("EXAMPLE:\n  bee oplog --json")
    }

    fn output_schema(&self) -> Value {
        schema::oplog()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (wc, _t) = open(ctx)?;
        let log = wc.oplog().map_err(cli_error)?;
        let undone: std::collections::BTreeSet<u64> = log.iter().filter_map(|r| r.undoes).collect();
        let ops: Vec<Value> = log
            .iter()
            .map(|r| {
                json!({
                    "seq": r.seq,
                    "op": r.op,
                    "summary": r.summary,
                    "undoable": r.undoable,
                    "undoes": r.undoes,
                    "undone": undone.contains(&r.seq),
                })
            })
            .collect();
        Ok(Some(json!({"ops": ops})))
    }

    fn render(&self, data: &Value) -> String {
        data["ops"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| {
                format!(
                    "{:>4} {:<12} {}{}",
                    o["seq"],
                    o["op"].as_str().unwrap_or_default(),
                    o["summary"].as_str().unwrap_or_default(),
                    if o["undone"].as_bool() == Some(true) {
                        " (undone)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `bee stack list`.
pub struct StackList;

impl Command for StackList {
    fn path(&self) -> &'static [&'static str] {
        &["stack", "list"]
    }

    fn about(&self) -> &'static str {
        "List the local stack of unlanded changes, bottom first."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help("EXAMPLE:\n  bee stack list --json")
    }

    fn output_schema(&self) -> Value {
        schema::stack_list()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (wc, _t) = open(ctx)?;
        let st = wc.state();
        let stack =
            bee_wc::to_json(&bee_wc::report::stack_views(st)).map_err(CliError::internal)?;
        Ok(Some(json!({
            "head": st.head.to_text(),
            "onto": st.onto.to_text(),
            "targetRef": wc.config().target_ref,
            "stack": stack,
        })))
    }

    fn render(&self, data: &Value) -> String {
        let s = render_stack(&data["stack"]);
        if s.is_empty() {
            "the stack is empty".into()
        } else {
            s.trim_start().to_owned()
        }
    }
}

/// `bee stack new`.
pub struct StackNew;

impl Command for StackNew {
    fn path(&self) -> &'static [&'static str] {
        &["stack", "new"]
    }

    fn about(&self) -> &'static str {
        "Start a new empty change after the checked-out one; `bee amend` fills it."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(command.arg(message_arg(true)))
            .after_help("EXAMPLE:\n  bee stack new -m \"Add the parser\" --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let message = arg(args, "message").unwrap_or_default();
        op_data(wc.stack_new(&t, &message, &signature(args)?))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee stack edit`.
pub struct StackEdit;

impl Command for StackEdit {
    fn path(&self) -> &'static [&'static str] {
        &["stack", "edit"]
    }

    fn about(&self) -> &'static str {
        "Check out a change of the stack so `amend` rewrites it and `commit` inserts after it."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("change")
                    .value_name("CHANGE")
                    .required(true)
                    .help("Position (1 = bottom), ChangeId or changeset prefix."),
            )
            .after_help("EXAMPLE:\n  bee stack edit 2 --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        op_data(wc.stack_edit(&t, &arg(args, "change").unwrap_or_default()))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee stack squash`.
pub struct StackSquash;

impl Command for StackSquash {
    fn path(&self) -> &'static [&'static str] {
        &["stack", "squash"]
    }

    fn about(&self) -> &'static str {
        "Fold a change (default: the checked-out one) into the change below it."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .arg(
                    Arg::new("change")
                        .value_name("CHANGE")
                        .help("Change to fold (default: the checked-out one)."),
                )
                .arg(message_arg(false)),
        )
        .after_help(
            "EXAMPLE:\n  bee stack squash --json\n  bee stack squash 3 -m \"Parser and tests\"",
        )
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        op_data(wc.squash(
            &t,
            arg(args, "change").as_deref(),
            arg(args, "message").as_deref(),
            &signature(args)?,
        ))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee stack split`.
pub struct StackSplit;

impl Command for StackSplit {
    fn path(&self) -> &'static [&'static str] {
        &["stack", "split"]
    }

    fn about(&self) -> &'static str {
        "Move the given paths of a change into a new change inserted before it."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        author_args(
            command
                .arg(
                    Arg::new("paths")
                        .value_name("PATH")
                        .num_args(1..)
                        .required(true)
                        .action(ArgAction::Append)
                        .help("Paths (or directories) that move to the new change."),
                )
                .arg(change_arg(
                    "Change to split (default: the checked-out one).",
                ))
                .arg(message_arg(false)),
        )
        .after_help("EXAMPLE:\n  bee stack split docs/ -m \"Docs first\" --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (mut wc, t) = open(ctx)?;
        let paths = repo_paths(wc.root(), &args_of(args, "paths"))?;
        op_data(wc.split(
            &t,
            arg(args, "change").as_deref(),
            &paths,
            arg(args, "message").as_deref(),
            &signature(args)?,
        ))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee sparse list`.
pub struct SparseList;

impl Command for SparseList {
    fn path(&self) -> &'static [&'static str] {
        &["sparse", "list"]
    }

    fn about(&self) -> &'static str {
        "Show the sparse profile (`.` = the whole repo)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help("EXAMPLE:\n  bee sparse list --json")
    }

    fn output_schema(&self) -> Value {
        schema::sparse_list()
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (wc, _t) = open(ctx)?;
        Ok(Some(json!({"sparse": wc.state().sparse.display()})))
    }

    fn render(&self, data: &Value) -> String {
        data["sparse"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn sparse_change(
    ctx: &Context,
    args: &ArgMatches,
    how: fn(&Sparse, &Sparse) -> Result<Sparse, bee_wc::WcError>,
) -> Result<Option<Value>, CliError> {
    let (mut wc, t) = open(ctx)?;
    let given = sparse_from(&args_of(args, "prefixes"))?;
    let next = how(&wc.state().sparse, &given).map_err(cli_error)?;
    op_data(wc.set_sparse(&t, next))
}

fn prefixes_arg() -> Arg {
    Arg::new("prefixes")
        .value_name("PREFIX")
        .num_args(1..)
        .required(true)
        .action(ArgAction::Append)
        .help("Repo path prefixes (`.` = the whole repo).")
}

/// `bee sparse set`.
pub struct SparseSet;

impl Command for SparseSet {
    fn path(&self) -> &'static [&'static str] {
        &["sparse", "set"]
    }

    fn about(&self) -> &'static str {
        "Replace the sparse profile and update the working tree."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(prefixes_arg())
            .after_help("EXAMPLE:\n  bee sparse set services/api libs/common --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        sparse_change(ctx, args, |_, given| Ok(given.clone()))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee sparse add`.
pub struct SparseAdd;

impl Command for SparseAdd {
    fn path(&self) -> &'static [&'static str] {
        &["sparse", "add"]
    }

    fn about(&self) -> &'static str {
        "Add prefixes to the sparse profile and fetch their files."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(prefixes_arg())
            .after_help("EXAMPLE:\n  bee sparse add docs --json")
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        sparse_change(ctx, args, |cur, given| Ok(cur.with_added(given)))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}

/// `bee sparse remove`.
pub struct SparseRemove;

impl Command for SparseRemove {
    fn path(&self) -> &'static [&'static str] {
        &["sparse", "remove"]
    }

    fn about(&self) -> &'static str {
        "Remove prefixes from the sparse profile and delete their files from disk."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.arg(prefixes_arg()).after_help(format!(
            "EXAMPLE:\n  bee sparse remove docs --json\n\n{EXIT_NOTE}"
        ))
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        sparse_change(ctx, args, |cur, given| cur.with_removed(given))
    }

    fn render(&self, data: &Value) -> String {
        render_op(data)
    }
}
