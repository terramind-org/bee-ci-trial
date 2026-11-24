//! Short forms of common generated commands: `bee cr open|list|view` for ChangeService,
//! `bee cr brief|checklist` for BriefService, `bee auth token` for
//! `AuthService/MintToken` and `bee redact` for `AdminService/Redact`. Each fills the repo
//! and revisions from the working copy, then makes exactly the call the generated command would.
//! `bee cr evidence` lives in [`super::evidence`].

use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::history::{Scope, repo_arg, scope};
use super::{arg, args_of, flag};
use crate::catalog::catalog;
use crate::commands::{Command, message_ref};
use crate::context::{Context, parse_duration};
use crate::error::CliError;
use crate::output;

/// Every alias.
pub fn all() -> Vec<&'static dyn Command> {
    vec![
        &CrOpen,
        &CrList,
        &CrView,
        &CrBrief,
        &CrChecklist,
        &super::evidence::CrEvidence,
        &AuthToken,
        &Redact,
    ]
}

/// `bee cr open`.
pub struct CrOpen;

impl Command for CrOpen {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "open"]
    }

    fn about(&self) -> &'static str {
        "Open a change request for the checked-out change (ChangeService/OpenChange)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        let command = command
            .long_about(
                "Open a change request whose head is --head (default: the checked-out change) \
                 against --target (default: the working copy's target ref). Same call as `bee \
                 change open-change`; output data is a bee.v1.ChangeRequest plus, after a \
                 brief or by default, the `checklist` of what it still owes. Say what the \
                 change does for a busy reader with the brief flags (SPEC §11.9).",
            )
            .arg(
                Arg::new("title")
                    .long("title")
                    .short('t')
                    .value_name("TEXT")
                    .required(true)
                    .help("Title."),
            )
            .arg(
                Arg::new("body")
                    .long("body")
                    .short('b')
                    .value_name("TEXT")
                    .help("Description (markdown)."),
            )
            .arg(
                Arg::new("head")
                    .long("head")
                    .value_name("REV")
                    .help("Head revision (default: the checked-out change)."),
            )
            .arg(
                Arg::new("target")
                    .long("target")
                    .value_name("REF")
                    .help("Target ref (default: the working copy's)."),
            )
            .arg(
                Arg::new("draft")
                    .long("draft")
                    .action(ArgAction::SetTrue)
                    .help("Open as a draft."),
            )
            .arg(
                Arg::new("reviewer")
                    .long("reviewer")
                    .value_name("PRINCIPAL")
                    .action(ArgAction::Append)
                    .help("Request a review (repeatable)."),
            )
            .arg(repo_arg());
        brief_flags(command).after_help(
            "EXAMPLE:\n  bee cr open --title \"Fix login redirect\" --json\n  \
             bee cr open --title \"Faster search\" --outcome \"Search answers in under 100 ms\" \\\n    \
             --highlight \"Cache query plans::src/search/\" --risk \"Stale results::cache expires after 60 s\"\n\n\
             With a brief, the checklist of what the change still owes is printed (and sent as \
             `checklist` in --json): see `bee cr checklist`.",
        )
    }

    fn output_schema(&self) -> Value {
        json!({
            "allOf": [
                message_ref("bee.v1.ChangeRequest"),
                {
                    "type": "object",
                    "properties": {
                        "checklist": {
                            "description": "What the brief and evidence of the new change request still owe (BriefService/GetBriefChecklist).",
                            "$ref": crate::schemas::message_uri("bee.v1.BriefChecklist")
                        }
                    }
                }
            ]
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let brief = brief_from_args(args)?;
        let head_rev = arg(args, "head").unwrap_or_else(|| scope.default_rev.clone());
        let resolved = scope.unary(
            "/bee.v1.QueryService/Resolve",
            json!({"repo": scope.repo_json(), "revisions": [{"expr": head_rev}]}),
        )?;
        let head = resolved["ids"]
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| {
                CliError::new(
                    bee_proto::v1::ErrorReason::NotFound,
                    format!("revision {head_rev:?} does not resolve"),
                )
            })?;
        let mut req = json!({
            "repo": scope.repo_json(),
            "targetRef": arg(args, "target").unwrap_or_else(|| scope.target_ref.clone()),
            "head": head,
            "title": arg(args, "title").unwrap_or_default(),
            "body": arg(args, "body").unwrap_or_default(),
            "draft": flag(args, "draft"),
            "reviewerIds": args_of(args, "reviewer"),
        });
        if let Some(brief) = brief {
            req["brief"] = brief;
        }
        let mut opened = scope.unary("/bee.v1.ChangeService/OpenChange", req)?;
        // The checklist is advice for the author; a server that cannot compute it must not
        // hide the change request that was just opened.
        let id = opened["id"].as_str().unwrap_or_default().to_owned();
        match scope.unary(
            "/bee.v1.BriefService/GetBriefChecklist",
            json!({"repo": scope.repo_json(), "changeId": id}),
        ) {
            Ok(checklist) => opened["checklist"] = checklist,
            Err(e) => eprintln!("note: the brief checklist is not available: {}", e.message),
        }
        Ok(Some(opened))
    }

    fn render(&self, data: &Value) -> String {
        let mut text = output::render_message(catalog(), "bee.v1.ChangeRequest", data);
        if data["checklist"].is_object() {
            text.push_str("\n\n");
            text.push_str(&render_checklist(
                &data["checklist"],
                data["id"].as_str().unwrap_or("<CR>"),
            ));
        }
        text
    }
}

/// `bee cr list`.
pub struct CrList;

impl Command for CrList {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "list"]
    }

    fn about(&self) -> &'static str {
        "List change requests of the repo (ChangeService/ListChanges)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("state")
                    .long("state")
                    .value_name("STATE")
                    .value_parser(["open", "landed", "closed", "all"])
                    .default_value("open")
                    .help("Which change requests."),
            )
            .arg(Arg::new("query").long("query").short('q').value_name("QUERY").help("Qualifier query, e.g. \"review:required path:billing/\"."))
            .arg(
                Arg::new("limit")
                    .long("limit")
                    .short('n')
                    .value_name("N")
                    .default_value("50")
                    .value_parser(clap::value_parser!(u32))
                    .help("Page size."),
            )
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee cr list --json\n  bee cr list --state all -q \"author:@me\"\n\nSame call as `bee change list-changes`; output data is a bee.v1.ListChangesResponse.")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.ListChangesResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let mut req = json!({
            "repo": scope.repo_json(),
            "page": {"pageSize": args.get_one::<u32>("limit").copied().unwrap_or(50)},
        });
        match arg(args, "state").as_deref() {
            Some("open") | None => req["state"] = json!("CHANGE_STATE_OPEN"),
            Some("landed") => req["state"] = json!("CHANGE_STATE_LANDED"),
            Some("closed") => req["state"] = json!("CHANGE_STATE_CLOSED"),
            _ => {}
        }
        if let Some(q) = arg(args, "query") {
            req["query"] = json!(q);
        }
        scope
            .unary("/bee.v1.ChangeService/ListChanges", req)
            .map(Some)
    }

    fn render(&self, data: &Value) -> String {
        let lines: Vec<String> = data["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                format!(
                    "#{} {} [{}] {}",
                    c["number"].as_str().unwrap_or_default(),
                    c["id"].as_str().unwrap_or_default(),
                    c["state"]
                        .as_str()
                        .unwrap_or_default()
                        .trim_start_matches("CHANGE_STATE_")
                        .to_lowercase(),
                    c["title"].as_str().unwrap_or_default()
                )
            })
            .collect();
        if lines.is_empty() {
            "no change requests".into()
        } else {
            lines.join("\n")
        }
    }
}

/// `bee cr view`.
pub struct CrView;

impl Command for CrView {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "view"]
    }

    fn about(&self) -> &'static str {
        "Show one change request (ChangeService/GetChange)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(Arg::new("id").value_name("ID").required(true).help("Change request id."))
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee cr view 01J9Z3M8Q4X7R2T5V6W8Y9A0BC --json\n\nSame call as `bee change get-change`; output data is a bee.v1.ChangeRequest.")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.ChangeRequest")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        scope
            .unary(
                "/bee.v1.ChangeService/GetChange",
                json!({"repo": scope.repo_json(), "id": arg(args, "id").unwrap_or_default()}),
            )
            .map(Some)
    }

    fn render(&self, data: &Value) -> String {
        output::render_message(catalog(), "bee.v1.ChangeRequest", data)
    }
}

/// The brief flags `cr open` and `cr brief` share.
fn brief_flags(command: clap::Command) -> clap::Command {
    command
        .arg(
            Arg::new("outcome")
                .long("outcome")
                .value_name("TEXT")
                .help("One line: what is different for users (at most 280 characters)."),
        )
        .arg(
            Arg::new("highlight")
                .long("highlight")
                .value_name("TEXT[::PATHS]")
                .action(ArgAction::Append)
                .help("One thing the change does, with the comma-separated paths that do it (repeatable)."),
        )
        .arg(
            Arg::new("ask")
                .long("ask")
                .value_name("QUESTION::A|B")
                .action(ArgAction::Append)
                .help("A decision you want from a person, with 2 to 6 options split by | (repeatable)."),
        )
        .arg(
            Arg::new("risk")
                .long("risk")
                .value_name("TEXT[::MITIGATION]")
                .action(ArgAction::Append)
                .help("A risk, and what mitigates it (repeatable)."),
        )
        .arg(
            Arg::new("rollback")
                .long("rollback")
                .value_name("TEXT")
                .help("How to undo the change, in words."),
        )
        .arg(
            Arg::new("try-it")
                .long("try-it")
                .value_name("URL")
                .help("https URL where the change can be tried."),
        )
        .arg(
            Arg::new("from")
                .long("from")
                .value_name("FILE")
                .help("Start from a brief in JSON (a bee.v1.ChangeBrief); the flags replace its outcome, rollback and URL and add to its lists."),
        )
}

/// Splits `text::rest` once; the second part is empty when there is no `::`.
fn split_pair(text: &str) -> (&str, &str) {
    match text.split_once("::") {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (text.trim(), ""),
    }
}

/// The brief the flags (and `--from`) describe, if any were given.
fn brief_from_args(args: &ArgMatches) -> Result<Option<Value>, CliError> {
    let mut brief = match arg(args, "from") {
        Some(file) => {
            let text = std::fs::read_to_string(&file).map_err(|e| {
                CliError::usage("bad_flag", format!("--from {file}: cannot read it: {e}"))
            })?;
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                CliError::usage("bad_flag", format!("--from {file}: not JSON: {e}"))
            })?;
            let value = match value.get("brief") {
                Some(inner) if inner.is_object() => inner.clone(),
                _ => value,
            };
            if !value.is_object() {
                return Err(CliError::usage(
                    "bad_flag",
                    format!("--from {file}: expected a JSON object (a ChangeBrief)"),
                ));
            }
            Some(value)
        }
        None => None,
    };
    let mut touched = brief.is_some();
    let mut set = |brief: &mut Option<Value>, f: &dyn Fn(&mut Value)| {
        touched = true;
        f(brief.get_or_insert_with(|| json!({})));
    };
    for (flag_name, key) in [
        ("outcome", "outcome"),
        ("rollback", "rollback"),
        ("try-it", "tryItUrl"),
    ] {
        if let Some(v) = arg(args, flag_name) {
            set(&mut brief, &|b| b[key] = json!(v));
        }
    }
    let append = |b: &mut Value, key: &str, item: Value| {
        if !b[key].is_array() {
            b[key] = json!([]);
        }
        if let Some(list) = b[key].as_array_mut() {
            list.push(item);
        }
    };
    for text in args_of(args, "highlight") {
        let (words, paths) = split_pair(&text);
        let paths: Vec<&str> = paths
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        set(&mut brief, &|b| {
            append(b, "highlights", json!({"text": words, "paths": paths}));
        });
    }
    for text in args_of(args, "ask") {
        let (question, options) = split_pair(&text);
        let options: Vec<&str> = options
            .split('|')
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .collect();
        if question.is_empty() || options.len() < 2 {
            return Err(CliError::usage(
                "bad_flag",
                format!("--ask {text:?}: use \"question::option one|option two\" (2 to 6 options)"),
            ));
        }
        set(&mut brief, &|b| {
            append(b, "asks", json!({"question": question, "options": options}));
        });
    }
    for text in args_of(args, "risk") {
        let (words, mitigation) = split_pair(&text);
        set(&mut brief, &|b| {
            append(b, "risks", json!({"text": words, "mitigation": mitigation}));
        });
    }
    Ok(if touched { brief } else { None })
}

/// The id of a change request named by id or by number (`12`, `#12`).
pub(super) fn change_id(scope: &Scope, text: &str) -> Result<String, CliError> {
    let digits = text.trim().trim_start_matches('#');
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(text.trim().to_owned());
    }
    let found = scope.unary(
        "/bee.v1.ChangeService/GetChangeByNumber",
        json!({"repo": scope.repo_json(), "number": digits}),
    )?;
    found["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CliError::internal("the server sent a change request without an id"))
}

/// The CR argument `cr brief`, `cr checklist` and `cr evidence` share.
pub(super) fn change_arg() -> Arg {
    Arg::new("change")
        .value_name("CR")
        .required(true)
        .help("Change request: its id, or its number (12 or #12).")
}

/// A checklist in words, with the command that settles each open item.
pub(super) fn render_checklist(checklist: &Value, cr: &str) -> String {
    let items = checklist["items"].as_array().cloned().unwrap_or_default();
    let owed: Vec<&Value> = items
        .iter()
        .filter(|i| !i["satisfied"].as_bool().unwrap_or(false))
        .collect();
    let mut out = format!(
        "Checklist for revision {}: {}",
        checklist["revision"].as_u64().unwrap_or(0),
        if owed.is_empty() {
            "nothing owed".to_owned()
        } else {
            format!("{} still owed", owed.len())
        }
    );
    for item in &items {
        let done = item["satisfied"].as_bool().unwrap_or(false);
        out.push_str(&format!(
            "\n  [{}] {}",
            if done { "x" } else { " " },
            item["detail"].as_str().unwrap_or_default()
        ));
        if !done {
            let fix = match item["id"].as_str().unwrap_or_default() {
                "brief" => format!(
                    "bee cr brief {cr} --outcome \"...\" --highlight \"what changed::paths\""
                ),
                "brief:risk" => {
                    format!(
                        "bee cr brief {cr} --risk \"what could go wrong::how it is covered\" --rollback \"...\""
                    )
                }
                _ => format!("bee cr evidence add {cr} FILE --alt \"what it shows\""),
            };
            out.push_str(&format!("\n      fix: {fix}"));
        }
    }
    out
}

/// `bee cr brief`.
pub struct CrBrief;

impl Command for CrBrief {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "brief"]
    }

    fn about(&self) -> &'static str {
        "Set a change request's brief: outcome, highlights, asks, risks (BriefService/SetChangeBrief)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        brief_flags(
            command
                .long_about(
                    "Write what a busy person reads first about a change request: the outcome in \
                     one line, what it does, the decisions you want from them, the risks and how \
                     to roll back (SPEC §11.9). The brief you send replaces the previous one. \
                     Prints the checklist of what the brief and evidence still owe. Same call as \
                     `bee brief set-change-brief`; output data is a \
                     bee.v1.SetChangeBriefResponse.",
                )
                .arg(change_arg())
                .arg(
                    Arg::new("expected-version")
                        .long("expected-version")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u64))
                        .help("Fail with VERSION_MISMATCH if the change request moved past this version."),
                )
                .arg(repo_arg()),
        )
        .after_help(
            "EXAMPLE:\n  bee cr brief 12 --outcome \"Checkout no longer double-charges\" \\\n    \
             --highlight \"Charge once per order id::src/billing/charge.rs\" \\\n    \
             --ask \"Refund past double charges?::yes|no\" --rollback \"Revert this change; no data moves\" --json",
        )
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.SetChangeBriefResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let brief = brief_from_args(args)?.ok_or_else(|| {
            CliError::usage(
                "empty_brief",
                "give at least --outcome, --highlight, --ask, --risk, --rollback, --try-it or --from",
            )
        })?;
        let mut req = json!({
            "repo": scope.repo_json(),
            "changeId": change_id(&scope, &arg(args, "change").unwrap_or_default())?,
            "brief": brief,
        });
        if let Some(v) = args.get_one::<u64>("expected-version") {
            req["expectedVersion"] = json!(v.to_string());
        }
        scope
            .unary("/bee.v1.BriefService/SetChangeBrief", req)
            .map(Some)
    }

    fn render(&self, data: &Value) -> String {
        let mut text = format!(
            "brief saved: {}",
            data["brief"]["outcome"].as_str().unwrap_or("(no outcome)")
        );
        if data["checklist"].is_object() {
            text.push('\n');
            text.push_str(&render_checklist(&data["checklist"], "<CR>"));
        }
        text
    }
}

/// `bee cr checklist`.
pub struct CrChecklist;

impl Command for CrChecklist {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "checklist"]
    }

    fn about(&self) -> &'static str {
        "Show what a change request's brief and evidence still owe (BriefService/GetBriefChecklist)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "List what a person looking at this change request would still want to see: a \
                 brief for logic changes, a risk or rollback for sensitive paths, a screenshot, \
                 before and after, or a video for UI changes. Advice only: landing never \
                 consults it. Same call as `bee brief get-brief-checklist`; output data is a \
                 bee.v1.BriefChecklist.",
            )
            .arg(change_arg())
            .arg(
                Arg::new("revision")
                    .long("revision")
                    .value_name("N")
                    .value_parser(clap::value_parser!(u32))
                    .help("Revision to check (default: the current one)."),
            )
            .arg(repo_arg())
            .after_help("EXAMPLE:\n  bee cr checklist 12 --json")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.BriefChecklist")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let scope = scope(ctx, args)?;
        let mut req = json!({
            "repo": scope.repo_json(),
            "changeId": change_id(&scope, &arg(args, "change").unwrap_or_default())?,
        });
        if let Some(n) = args.get_one::<u32>("revision") {
            req["revision"] = json!(n);
        }
        scope
            .unary("/bee.v1.BriefService/GetBriefChecklist", req)
            .map(Some)
    }

    fn render(&self, data: &Value) -> String {
        render_checklist(data, "<CR>")
    }
}

/// `bee auth token`.
pub struct AuthToken;

impl Command for AuthToken {
    fn path(&self) -> &'static [&'static str] {
        &["auth", "token"]
    }

    fn about(&self) -> &'static str {
        "Mint an attenuated token for yourself or an agent (AuthService/MintToken)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Mint a token narrower than the current one: limited to repos, refs, actions and \
                 path prefixes, with an expiry. Same call as `bee auth mint-token`; output data \
                 is a bee.v1.MintTokenResponse. Store it with `bee login --token -`.",
            )
            .arg(Arg::new("principal").long("principal").value_name("ID").help("Principal to mint for (default: yourself)."))
            .arg(Arg::new("repo").long("repo").value_name("REPO").action(ArgAction::Append).help("Repo full name (org/name), org/* or repo id (repeatable)."))
            .arg(Arg::new("ref").long("ref").value_name("PATTERN").action(ArgAction::Append).help("Ref pattern, e.g. ws/* (repeatable)."))
            .arg(
                Arg::new("action")
                    .long("action")
                    .value_name("ACTION")
                    .action(ArgAction::Append)
                    .help("read, commit, land, review, lease, issues_write, admin (repeatable)."),
            )
            .arg(Arg::new("read-path").long("read-path").value_name("PREFIX").action(ArgAction::Append).help("Readable path prefix (repeatable)."))
            .arg(Arg::new("write-path").long("write-path").value_name("PREFIX").action(ArgAction::Append).help("Writable path prefix (repeatable)."))
            .arg(Arg::new("expires-in").long("expires-in").value_name("DURATION").help("Lifetime: 30m, 8h, ..."))
            .arg(Arg::new("session").long("session").value_name("ID").help("Session id copied into provenance."))
            .after_help("EXAMPLE:\n  bee auth token --action read --action commit --ref 'ws/*' --expires-in 8h --json")
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.MintTokenResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        use base64::Engine as _;
        let b64 = |v: Vec<String>| -> Vec<String> {
            v.iter()
                .map(|p| {
                    base64::engine::general_purpose::STANDARD.encode(p.trim_matches('/').as_bytes())
                })
                .collect()
        };
        let mut scope = json!({
            "repos": args_of(args, "repo"),
            "refs": args_of(args, "ref"),
            "actions": args_of(args, "action"),
            "readPaths": b64(args_of(args, "read-path")),
            "writePaths": b64(args_of(args, "write-path")),
        });
        if let Some(session) = arg(args, "session") {
            scope["session"] = json!(session);
        }
        if let Some(d) = arg(args, "expires-in") {
            let secs = parse_duration(&d)?.as_secs() as i64;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            scope["expiresAt"] = json!(bee_wc::remote::rfc3339(now + secs));
        }
        let req = json!({
            "principalId": arg(args, "principal").unwrap_or_default(),
            "scope": scope,
        });
        let method = bee_proto::methods::find_method("/bee.v1.AuthService/MintToken")
            .ok_or_else(|| CliError::internal("MintToken is not in the catalog"))?;
        ctx.call_unary(method, &req).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        output::render_message(catalog(), "bee.v1.MintTokenResponse", data)
    }
}

/// `bee redact`.
pub struct Redact;

impl Command for Redact {
    fn path(&self) -> &'static [&'static str] {
        &["redact"]
    }

    fn about(&self) -> &'static str {
        "Redact content: a path at a revision, objects, or changeset metadata (AdminService/Redact)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Make content unreadable everywhere without changing any id (SPEC §13). Name \
                 what to redact with --path (the blob at that path in --at, or every version of \
                 it with --all-versions), --object (blob or meta ids) and --meta (the metadata of \
                 the changesets these revisions resolve to); combine them freely. The objects \
                 are quarantined: unreadable at once, restorable with `bee admin unredact`, \
                 and purged for good after --purge-after (default: the server's quarantine \
                 window) or immediately with --now. Needs the `redact` permission; --now also \
                 needs `purge`. Same call as `bee admin redact`; output data is a \
                 bee.v1.RedactResponse.",
            )
            .arg(
                Arg::new("path")
                    .long("path")
                    .value_name("PATH")
                    .help("File path (relative to the current directory inside a working copy)."),
            )
            .arg(
                Arg::new("at")
                    .long("at")
                    .value_name("REV")
                    .help("Revision --path resolves in (default: the target ref)."),
            )
            .arg(
                Arg::new("all-versions")
                    .long("all-versions")
                    .action(ArgAction::SetTrue)
                    .requires("path")
                    .help("Every version of --path in the history of --at."),
            )
            .arg(
                Arg::new("object")
                    .long("object")
                    .value_name("ID")
                    .action(ArgAction::Append)
                    .help("Blob or meta id, e.g. b1_... (repeatable)."),
            )
            .arg(
                Arg::new("meta")
                    .long("meta")
                    .value_name("REV")
                    .action(ArgAction::Append)
                    .help("Redact the metadata (message, author) of this changeset (repeatable)."),
            )
            .arg(
                Arg::new("reason")
                    .long("reason")
                    .value_name("TEXT")
                    .required(true)
                    .help("Why; recorded in the audit log and the redaction list."),
            )
            .arg(
                Arg::new("purge-after")
                    .long("purge-after")
                    .value_name("DURATION")
                    .conflicts_with("now")
                    .help("Quarantine time before the purge: 30m, 12h, 7d, ..."),
            )
            .arg(
                Arg::new("now")
                    .long("now")
                    .action(ArgAction::SetTrue)
                    .help("Purge immediately (no quarantine; cannot be undone)."),
            )
            .arg(repo_arg())
            .group(
                clap::ArgGroup::new("target")
                    .args(["path", "object", "meta"])
                    .multiple(true)
                    .required(true),
            )
            .after_help(
                "EXAMPLE:\n  bee redact --path config/prod.env --all-versions --reason \"leaked key\" --json\n  \
                 bee redact --object b1_3f... --now --reason \"customer data\" --json\n  \
                 bee redact --meta main~2 --reason \"personal data in a message\" --json",
            )
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.RedactResponse")
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        use base64::Engine as _;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let scope = scope(ctx, args)?;
        let at = arg(args, "at").unwrap_or_else(|| scope.target_ref.clone());
        let mut ids = Vec::new();
        for text in args_of(args, "object") {
            let id = bee_core::ObjectId::parse(&text)
                .map_err(|e| CliError::usage("bad_object_id", format!("--object {text:?}: {e}")))?;
            ids.push(json!({"raw": b64(id.as_bytes())}));
        }
        let metas = args_of(args, "meta");
        if !metas.is_empty() {
            let revisions: Vec<Value> = metas.iter().map(|r| json!({"expr": r})).collect();
            let resolved = scope.unary(
                "/bee.v1.QueryService/Resolve",
                json!({"repo": scope.repo_json(), "revisions": revisions}),
            )?;
            for (i, rev) in metas.iter().enumerate() {
                let ok = resolved["statuses"][i]
                    .as_str()
                    .is_none_or(|s| s == "ITEM_STATUS_OK");
                match resolved["ids"].get(i) {
                    Some(id) if ok && id.get("raw").is_some() => ids.push(id.clone()),
                    _ => {
                        return Err(CliError::new(
                            bee_proto::v1::ErrorReason::NotFound,
                            format!("--meta {rev:?} does not resolve"),
                        ));
                    }
                }
            }
        }
        let mut req = json!({
            "repo": scope.repo_json(),
            "ids": ids,
            "reason": arg(args, "reason").unwrap_or_default(),
            "purgeNow": flag(args, "now"),
            "meta": !metas.is_empty(),
            "at": {"expr": at},
        });
        if let Some(p) = arg(args, "path") {
            let path = scope.paths(&[p])?.pop().unwrap_or_default();
            req["path"] = json!(b64(path.as_bytes()));
            req["allVersions"] = json!(flag(args, "all-versions"));
        }
        if let Some(d) = arg(args, "purge-after") {
            let secs = parse_duration(&d)?.as_secs().max(1);
            let secs = u32::try_from(secs)
                .map_err(|_| CliError::usage("bad_duration", "--purge-after is too long"))?;
            req["purgeAfterSeconds"] = json!(secs);
        }
        scope.unary("/bee.v1.AdminService/Redact", req).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        let n = data["redacted"].as_array().map(Vec::len).unwrap_or(0);
        format!(
            "redacted {n} object{} (redaction epoch {}){}",
            if n == 1 { "" } else { "s" },
            data["redactionEpoch"].as_str().unwrap_or("0"),
            data["effectiveBy"]
                .as_str()
                .map(|t| format!("; unreadable through every tier by {t}"))
                .unwrap_or_default()
        )
    }
}
