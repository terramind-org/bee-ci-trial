//! History over the API, no clone needed: `log`, `diff`, `show`, `blame`, `cat`. Inside a
//! working copy the repo and the default revision (the checked-out change) come from it.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_wc::Transport as _;
use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::{CliTransport, arg, args_of, cli_error, flag, repo_paths, schema};
use crate::commands::{Command, message_ref};
use crate::context::Context;
use crate::error::CliError;

/// Every history command.
pub fn all() -> Vec<&'static dyn Command> {
    vec![&Log, &Diff, &Show, &Blame, &Cat]
}

/// Where a history command runs: repo, default revision, transport, and the working copy
/// root when there is one.
pub(super) struct Scope {
    pub repo: String,
    pub default_rev: String,
    pub target_ref: String,
    pub transport: CliTransport,
    pub root: Option<std::path::PathBuf>,
}

pub(super) fn repo_arg() -> Arg {
    Arg::new("repo")
        .long("repo")
        .value_name("REPO")
        .help("Repo as org/name (default: the working copy's).")
}

pub(super) fn scope(ctx: &Context, args: &ArgMatches) -> Result<Scope, CliError> {
    let cwd = std::env::current_dir().map_err(|e| CliError::internal(e.to_string()))?;
    let root = bee_wc::find_root(&cwd);
    let (wc_repo, wc_endpoint, head, target_ref) = match &root {
        Some(r) => {
            let config = bee_wc::read_config(r).map_err(cli_error)?;
            let state: bee_wc::state::State = bee_wc::state::read_state(&r.join(".bee"))
                .map_err(cli_error)?
                .ok_or_else(|| CliError::internal("the working copy has no state.toml"))?;
            (
                Some(config.repo),
                config.endpoint,
                Some(state.head.to_text()),
                config.target_ref,
            )
        }
        None => (None, None, None, "main".to_owned()),
    };
    let explicit_repo = arg(args, "repo");
    let repo = explicit_repo.clone().or(wc_repo.clone()).ok_or_else(|| {
        CliError::usage(
            "missing_repo",
            "not in a working copy: pass --repo org/name",
        )
    })?;
    let same_repo = explicit_repo.is_none() || explicit_repo == wc_repo;
    let default_rev = match head {
        Some(h) if same_repo => h,
        _ => target_ref.clone(),
    };
    Ok(Scope {
        transport: CliTransport::new(
            ctx,
            if same_repo {
                wc_endpoint.as_deref()
            } else {
                None
            },
        ),
        repo,
        default_rev,
        target_ref,
        root: if same_repo { root } else { None },
    })
}

impl Scope {
    pub(super) fn repo_json(&self) -> Value {
        if self.repo.contains('/') {
            json!({"fullName": self.repo})
        } else {
            json!({"id": self.repo})
        }
    }

    pub(super) fn unary(&self, path: &str, request: Value) -> Result<Value, CliError> {
        self.transport
            .unary(path, &request)
            .map_err(|d| CliError::from_detail(&d, "request failed"))
    }

    pub(super) fn stream(&self, path: &str, request: Value) -> Result<Vec<Value>, CliError> {
        self.transport
            .server_stream(path, &request)
            .map_err(|d| CliError::from_detail(&d, "request failed"))
    }

    /// Repo paths for paths given on the command line: relative to the current directory
    /// inside a working copy, else taken as repo paths.
    pub(super) fn paths(&self, given: &[String]) -> Result<Vec<String>, CliError> {
        match &self.root {
            Some(root) => repo_paths(root, given),
            None => Ok(given
                .iter()
                .map(|g| g.trim_matches('/').to_owned())
                .collect()),
        }
    }
}

fn b64(text: &str) -> String {
    STANDARD.encode(text.as_bytes())
}

fn unb64(v: &Value) -> Vec<u8> {
    v.as_str()
        .and_then(|s| STANDARD.decode(s).ok())
        .unwrap_or_default()
}

fn id_text(v: &Value) -> String {
    let raw = unb64(&v["raw"]);
    bee_core::ObjectId::from_bytes(&raw)
        .map(|id| id.to_text())
        .unwrap_or_default()
}

fn mode_text(v: &Value) -> Option<String> {
    match v.as_str()? {
        "MODE_FILE" => Some("file".into()),
        "MODE_EXECUTABLE" => Some("executable".into()),
        "MODE_SYMLINK" => Some("symlink".into()),
        "MODE_TREE" => Some("tree".into()),
        _ => None,
    }
}

/// A server `DiffItem` in the `bee diff` file shape.
fn file_of(item: &Value) -> Value {
    let status = match item["status"].as_str().unwrap_or_default() {
        "DIFF_STATUS_ADDED" => "added",
        "DIFF_STATUS_DELETED" => "deleted",
        "DIFF_STATUS_RENAMED" => "renamed",
        "DIFF_STATUS_COPIED" => "copied",
        "DIFF_STATUS_MODE_CHANGED" => "mode-changed",
        _ => "modified",
    };
    let path = String::from_utf8_lossy(&unb64(&item["path"])).into_owned();
    let old_path = String::from_utf8_lossy(&unb64(&item["oldPath"])).into_owned();
    let mut unified = String::new();
    let hunks = item["hunks"].as_array().cloned().unwrap_or_default();
    if !hunks.is_empty() {
        let a = if status == "added" {
            "/dev/null".to_owned()
        } else {
            format!(
                "a/{}",
                if old_path.is_empty() {
                    &path
                } else {
                    &old_path
                }
            )
        };
        let b = if status == "deleted" {
            "/dev/null".to_owned()
        } else {
            format!("b/{path}")
        };
        unified.push_str(&format!("--- {a}\n+++ {b}\n"));
        for h in hunks {
            unified.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h["oldStart"].as_u64().unwrap_or(0),
                h["oldLen"].as_u64().unwrap_or(0),
                h["newStart"].as_u64().unwrap_or(0),
                h["newLen"].as_u64().unwrap_or(0)
            ));
            for l in h["lines"].as_array().into_iter().flatten() {
                let line = l.as_str().unwrap_or_default();
                unified.push_str(line);
                if !line.ends_with('\n') {
                    unified.push_str("\n\\ No newline at end of file\n");
                }
            }
        }
    }
    json!({
        "path": path,
        "status": status,
        "oldMode": mode_text(&item["old"]["mode"]),
        "newMode": mode_text(&item["new"]["mode"]),
        "binary": item["binary"].as_bool().unwrap_or(false),
        "additions": item["additions"].as_u64().unwrap_or(0),
        "deletions": item["deletions"].as_u64().unwrap_or(0),
        "unified": unified,
    })
}

fn server_diff(
    scope: &Scope,
    from: &str,
    to: &str,
    paths: &[String],
    context: u32,
) -> Result<Vec<Value>, CliError> {
    let items = scope.stream(
        "/bee.v1.QueryService/Diff",
        json!({
            "repo": scope.repo_json(),
            "from": {"expr": from},
            "to": {"expr": to},
            "paths": paths.iter().map(|p| b64(p)).collect::<Vec<_>>(),
            "includeHunks": true,
            "contextLines": context,
            "detectRenames": true,
        }),
    )?;
    Ok(items.iter().map(file_of).collect())
}

fn render_files(data: &Value) -> String {
    let mut out = String::new();
    for f in data["files"].as_array().into_iter().flatten() {
        let unified = f["unified"].as_str().unwrap_or_default();
        if unified.is_empty() {
            out.push_str(&format!(
                "{} {}{}\n",
                f["status"].as_str().unwrap_or_default(),
                f["path"].as_str().unwrap_or_default(),
                if f["binary"].as_bool() == Some(true) {
                    " (binary)"
                } else {
                    ""
                }
            ));
        } else {
            out.push_str(unified);
        }
    }
    if out.is_empty() {
        "no differences".into()
    } else {
        out.trim_end().to_owned()
    }
}

/// `bee log`.
pub struct Log;

impl Command for Log {
    fn path(&self) -> &'static [&'static str] {
        &["log"]
    }

    fn about(&self) -> &'static str {
        "Show history from a revision (default: the checked-out change), from the server."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(Arg::new("rev").long("rev").value_name("REV").help("Start here (default: the checked-out change, else main)."))
            .arg(Arg::new("path").long("path").value_name("PATH").help("Only changes touching PATH."))
            .arg(
                Arg::new("limit")
                    .long("limit")
                    .short('n')
                    .value_name("N")
                    .default_value("50")
                    .value_parser(clap::value_parser!(u32))
                    .help("At most N changesets."),
            )
            .arg(Arg::new("first-parent").long("first-parent").action(ArgAction::SetTrue).help("Follow first parents only."))
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee log --json\n  bee log --rev main --path src/auth -n 10\n\nOutput data is a bee.v1.LogResponse (changesets with meta).")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.LogResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let rev = arg(args, "rev").unwrap_or_else(|| scope.default_rev.clone());
        let mut req = json!({
            "repo": scope.repo_json(),
            "from": {"expr": rev},
            "firstParent": flag(args, "first-parent"),
            "includeMeta": true,
            "page": {"pageSize": args.get_one::<u32>("limit").copied().unwrap_or(50)},
        });
        if let Some(p) = arg(args, "path") {
            let p = scope.paths(&[p])?.pop().unwrap_or_default();
            req["path"] = json!(b64(&p));
        }
        scope.unary("/bee.v1.QueryService/Log", req).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        let mut out = Vec::new();
        for c in data["changesets"].as_array().into_iter().flatten() {
            let msg = c["meta"]["message"].as_str().unwrap_or_default();
            out.push(format!(
                "{} {} {}",
                id_text(&c["id"]),
                c["meta"]["author"].as_str().unwrap_or_default(),
                msg.lines().next().unwrap_or_default()
            ));
        }
        out.join("\n")
    }
}

/// `bee diff`.
pub struct Diff;

impl Command for Diff {
    fn path(&self) -> &'static [&'static str] {
        &["diff"]
    }

    fn about(&self) -> &'static str {
        "Show uncommitted changes as unified diffs, or the diff between two revisions."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "In a working copy without --from/--to: uncommitted changes against the checked-out \
                 change. With --from and/or --to: the server's diff between revisions (--from \
                 defaults to the parent of --to; --to defaults to the checked-out change, else main).",
            )
            .arg(Arg::new("paths").value_name("PATH").num_args(0..).action(ArgAction::Append).help("Limit to these paths."))
            .arg(Arg::new("from").long("from").value_name("REV").help("Old side."))
            .arg(Arg::new("to").long("to").value_name("REV").help("New side."))
            .arg(
                Arg::new("context")
                    .long("context")
                    .short('U')
                    .value_name("N")
                    .default_value("3")
                    .value_parser(clap::value_parser!(u32))
                    .help("Context lines."),
            )
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee diff --json\n  bee diff --from main~1 --to main src/")
    }

    fn output_schema(&self) -> Value {
        schema::diff()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let context = args.get_one::<u32>("context").copied().unwrap_or(3);
        let from = arg(args, "from");
        let to = arg(args, "to");
        let scope = scope(ctx, args)?;
        let paths = scope.paths(&args_of(args, "paths"))?;
        if from.is_none() && to.is_none() && scope.root.is_some() && arg(args, "repo").is_none() {
            let (mut wc, t) = super::open(ctx)?;
            let files = wc.diff(&t, &paths, context as usize).map_err(cli_error)?;
            let files = bee_wc::to_json(&files).map_err(CliError::internal)?;
            return Ok(Some(json!({"source": "working-copy", "files": files})));
        }
        let to = to.unwrap_or_else(|| scope.default_rev.clone());
        let from = from.unwrap_or_else(|| format!("{to}~1"));
        let paths: Vec<String> = paths.into_iter().filter(|p| p != ".").collect();
        let files = server_diff(&scope, &from, &to, &paths, context)?;
        Ok(Some(json!({"source": "server", "files": files})))
    }

    fn render(&self, data: &Value) -> String {
        render_files(data)
    }
}

/// `bee show`.
pub struct Show;

impl Command for Show {
    fn path(&self) -> &'static [&'static str] {
        &["show"]
    }

    fn about(&self) -> &'static str {
        "Show a changeset (default: the checked-out change): meta, provenance and its diff."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("rev")
                    .value_name("REV")
                    .help("Revision (default: the checked-out change, else main)."),
            )
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee show --json\n  bee show main~2")
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["changesets", "files"],
            "additionalProperties": false,
            "properties": {
                "changesets": message_ref("bee.v1.GetChangesetsResponse"),
                "files": schema::diff()["properties"]["files"].clone()
            }
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let rev = arg(args, "rev").unwrap_or_else(|| scope.default_rev.clone());
        let resolved = scope.unary(
            "/bee.v1.QueryService/Resolve",
            json!({"repo": scope.repo_json(), "revisions": [{"expr": rev}]}),
        )?;
        let Some(id) = resolved["ids"].as_array().and_then(|a| a.first()).cloned() else {
            return Err(CliError::new(
                bee_proto::v1::ErrorReason::NotFound,
                format!("revision {rev:?} does not resolve"),
            ));
        };
        let changesets = scope.unary(
            "/bee.v1.QueryService/GetChangesets",
            json!({"repo": scope.repo_json(), "ids": [id], "includeMeta": true}),
        )?;
        let cs = &changesets["changesets"][0];
        let id_txt = id_text(&cs["id"]);
        let files = match cs["parents"].as_array().and_then(|p| p.first()) {
            Some(parent) => server_diff(&scope, &id_text(parent), &id_txt, &[], 3)?,
            None => Vec::new(),
        };
        Ok(Some(json!({"changesets": changesets, "files": files})))
    }

    fn render(&self, data: &Value) -> String {
        let cs = &data["changesets"]["changesets"][0];
        let meta = &cs["meta"];
        let mut out = format!(
            "changeset {}\nauthor {}\ncreator {}\n\n    {}\n",
            id_text(&cs["id"]),
            meta["author"].as_str().unwrap_or_default(),
            meta["creator"].as_str().unwrap_or_default(),
            meta["message"]
                .as_str()
                .unwrap_or_default()
                .replace('\n', "\n    ")
        );
        out.push('\n');
        out.push_str(&render_files(data));
        out
    }
}

/// `bee blame`.
pub struct Blame;

impl Command for Blame {
    fn path(&self) -> &'static [&'static str] {
        &["blame"]
    }

    fn about(&self) -> &'static str {
        "Show which changeset last changed each line of a file, from the server."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(Arg::new("path").value_name("PATH").required(true).help("File to blame."))
            .arg(Arg::new("rev").long("rev").value_name("REV").help("Revision (default: the checked-out change, else main)."))
            .arg(Arg::new("lines").long("lines").short('L').value_name("START:END").help("Only these lines (1-based, inclusive)."))
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee blame src/lib.rs --json\n  bee blame src/lib.rs -L 10:20\n\nOutput data is a bee.v1.BlameResponse; `bee show <changeset>` tells who (and which agent) made it.")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.BlameResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let rev = arg(args, "rev").unwrap_or_else(|| scope.default_rev.clone());
        let path = scope
            .paths(&[arg(args, "path").unwrap_or_default()])?
            .pop()
            .unwrap_or_default();
        let mut req = json!({"repo": scope.repo_json(), "at": {"expr": rev}, "path": b64(&path)});
        if let Some(lines) = arg(args, "lines") {
            let (a, b) = lines
                .split_once(':')
                .ok_or_else(|| CliError::usage("bad_flag", "--lines takes START:END"))?;
            let parse = |s: &str| {
                s.trim().parse::<u32>().map_err(|_| {
                    CliError::usage("bad_flag", format!("--lines: {s:?} is not a line number"))
                })
            };
            req["startLine"] = json!(parse(a)?);
            req["endLine"] = json!(parse(b)?);
        }
        scope.unary("/bee.v1.QueryService/Blame", req).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        data["ranges"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let start = r["startLine"].as_u64().unwrap_or(0);
                let count = r["lineCount"].as_u64().unwrap_or(0);
                format!(
                    "{}-{} {}",
                    start,
                    (start + count).saturating_sub(1),
                    id_text(&r["changeset"])
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `bee cat`.
pub struct Cat;

impl Command for Cat {
    fn path(&self) -> &'static [&'static str] {
        &["cat"]
    }

    fn about(&self) -> &'static str {
        "Print a file as of a revision, from the server."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("path")
                    .value_name("PATH")
                    .required(true)
                    .help("File to print."),
            )
            .arg(
                Arg::new("rev")
                    .long("rev")
                    .value_name("REV")
                    .help("Revision (default: the checked-out change, else main)."),
            )
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee cat README.md --rev main~1\n  bee cat src/lib.rs --json")
    }

    fn output_schema(&self) -> Value {
        schema::cat()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let rev = arg(args, "rev").unwrap_or_else(|| scope.default_rev.clone());
        let path = scope
            .paths(&[arg(args, "path").unwrap_or_default()])?
            .pop()
            .unwrap_or_default();
        let chunks = scope.stream(
            "/bee.v1.QueryService/ReadPath",
            json!({"repo": scope.repo_json(), "at": {"expr": rev}, "path": b64(&path)}),
        )?;
        let mut bytes = Vec::new();
        let mut mode = "file".to_owned();
        let mut blob = String::new();
        let mut size = 0u64;
        for c in &chunks {
            bytes.extend(unb64(&c["data"]));
            if let Some(m) = mode_text(&c["mode"]) {
                mode = m;
            }
            if c.get("blob").is_some() {
                blob = id_text(&c["blob"]);
            }
            if let Some(s) = c["size"].as_str().and_then(|s| s.parse().ok()) {
                size = s;
            }
        }
        let (encoding, content) = match String::from_utf8(bytes.clone()) {
            Ok(text) => ("utf8", text),
            Err(_) => ("base64", STANDARD.encode(&bytes)),
        };
        Ok(Some(json!({
            "path": path,
            "rev": rev,
            "mode": mode,
            "blob": blob,
            "size": size,
            "encoding": encoding,
            "content": content,
        })))
    }

    fn render(&self, data: &Value) -> String {
        let content = data["content"].as_str().unwrap_or_default();
        if data["encoding"] == "base64" {
            format!("(binary, {} bytes, base64)\n{content}", data["size"])
        } else {
            content.strip_suffix('\n').unwrap_or(content).to_owned()
        }
    }
}
