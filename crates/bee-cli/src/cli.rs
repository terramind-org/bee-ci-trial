//! The clap command tree: global flags, registered commands, and one `bee <noun> <verb>` per
//! RPC generated at runtime from the catalog.
//!
//! Building every verb's flags for 1,000+ RPCs on each run would be wasted work, so the tree
//! expands only the noun named on the command line ([`selected_noun`]); [`root`] with
//! [`Expand::All`] builds everything (tests use it to check the whole tree).

use clap::{Arg, ArgAction, Command};

use crate::catalog::{Cardinality, Catalog, Kind, Noun, Scalar, Verb, catalog};
use crate::commands::{self, registry};
use crate::error::ExitCode;
use crate::fields::{self, field_flags, flag_facts, value_name};
use crate::schemas;

/// The global flags: `(flag, environment variable, description)`.
pub const GLOBAL_FLAGS: &[(&str, Option<&str>, &str)] = &[
    (
        "--endpoint <URL>",
        Some("BEE_ENDPOINT"),
        "Server endpoint: http://host:port[/prefix] or unix:/path. Default: BEE_ENDPOINT, \
         then the endpoint saved by `bee login`.",
    ),
    (
        "--token <TOKEN>",
        Some("BEE_TOKEN"),
        "Bearer token. Default: BEE_TOKEN, then ~/.config/bee/credentials (mode 0600, \
         written by `bee login`).",
    ),
    (
        "--json",
        None,
        "Print exactly one JSON document {\"schema\":\"bee.v1.<noun>.<verb>\",\"data\":...} \
         (NDJSON for streams), or {\"schema\":\"bee.v1.error\",\"error\":{...}} on failure.",
    ),
    ("-C <DIR>", None, "Run as if bee was started in DIR."),
    (
        "--timeout <DURATION>",
        None,
        "Fail with DEADLINE_EXCEEDED after DURATION: 500ms, 30s, 2m, 1h; a bare number is \
         seconds.",
    ),
    (
        "--no-daemon",
        Some("BEE_NO_DAEMON"),
        "Do not use the node daemon (BEE_DAEMON_SOCKET): fetch objects from the server and \
         walk the working tree for `status`.",
    ),
];

/// How much of the tree to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expand<'a> {
    /// Verbs of no noun (nouns are listed with their summaries only).
    None,
    /// Verbs of this noun.
    Noun(&'a str),
    /// Verbs of every noun.
    All,
}

/// Global flags that take a value, as they may appear before the noun.
const VALUE_FLAGS: &[&str] = &["--endpoint", "--token", "--timeout", "-C"];

/// The noun named on a command line: the first positional word (after `help`, if that comes
/// first), skipping global flags and their values.
pub fn selected_noun(args: &[String]) -> Option<String> {
    let mut iter = args.iter().skip(1);
    let mut after_help = false;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return None;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if arg == "help" && !after_help {
            after_help = true;
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn root_after_help() -> String {
    let mut text = String::from(
        "Commands are `bee <noun> <verb> [flags]`: one per API call, where the noun is the \
         service (`write`, `change`, `issue`, ...) and the verb the method in kebab-case \
         (`bee write land`, `bee change open-change`). `bee <noun> --help` lists verbs; \
         `bee <noun> <verb> --help` lists every flag with its type and an example. \
         `bee help-agent` prints the whole index; `bee schema <noun> <verb>` prints its \
         JSON Schemas.\n\nEXIT CODES:",
    );
    for (code, meaning) in ExitCode::TABLE {
        text.push_str(&format!("\n  {}  {meaning}", code.code()));
    }
    text.push_str("\n\nEXAMPLE:\n  bee login --token \"$TOKEN\" --endpoint http://127.0.0.1:8080\n  bee change list-changes --repo acme/app --json");
    text
}

fn global_args() -> Vec<Arg> {
    let heading = "Global options";
    vec![
        Arg::new("endpoint")
            .long("endpoint")
            .value_name("URL")
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[0].2),
        Arg::new("token")
            .long("token")
            .value_name("TOKEN")
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[1].2),
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[2].2),
        Arg::new("dir")
            .short('C')
            .value_name("DIR")
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[3].2),
        Arg::new("timeout")
            .long("timeout")
            .value_name("DURATION")
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[4].2),
        Arg::new("no-daemon")
            .long("no-daemon")
            .action(ArgAction::SetTrue)
            .global(true)
            .help_heading(heading)
            .help(GLOBAL_FLAGS[5].2),
    ]
}

/// The whole command tree, with the verbs of the nouns `expand` selects.
pub fn root(expand: Expand<'_>) -> Command {
    let catalog = catalog();
    let mut root = Command::new("bee")
        .version(env!("CARGO_PKG_VERSION"))
        .about("bee: source control and a forge built for AI agents.")
        .after_help(root_after_help())
        .subcommand_required(true)
        .arg_required_else_help(true)
        .args(global_args());
    let registered = registry();
    for command in registered.iter().filter(|c| c.path().len() == 1) {
        root = root.subcommand(registered_command(*command));
    }
    for noun in catalog.nouns() {
        let expanded = match expand {
            Expand::None => false,
            Expand::Noun(name) => name == noun.name,
            Expand::All => true,
        };
        root = root.subcommand(noun_command(catalog, noun, expanded));
    }
    // Registered verbs under a word that is not a generated noun form their own group.
    let mut groups: Vec<&str> = registered
        .iter()
        .filter(|c| c.path().len() == 2 && catalog.noun(c.path()[0]).is_none())
        .map(|c| c.path()[0])
        .collect();
    groups.sort_unstable();
    groups.dedup();
    for group in groups {
        let mut command = Command::new(group.to_owned())
            .about(format!("{group} commands."))
            .subcommand_required(true)
            .arg_required_else_help(true);
        for registered_verb in registered.iter().filter(|c| c.path()[0] == group) {
            command = command.subcommand(registered_command(*registered_verb));
        }
        root = root.subcommand(command);
    }
    root
}

fn registered_command(command: &dyn commands::Command) -> Command {
    let name = command.path().last().copied().unwrap_or_default();
    let base = Command::new(name.to_owned()).about(command.about());
    command.configure(base)
}

fn noun_command(catalog: &Catalog, noun: &Noun, expanded: bool) -> Command {
    let mut command = Command::new(noun.name.clone())
        .about(noun.summary())
        .long_about(format!(
            "{}\n\nEach verb calls one RPC of {}. Run `bee {} <verb> --help` for its flags.",
            if noun.comment.is_empty() {
                noun.summary()
            } else {
                noun.comment.clone()
            },
            noun.service,
            noun.name
        ))
        .subcommand_required(true)
        .arg_required_else_help(true);
    if expanded {
        for verb in &noun.verbs {
            command = command.subcommand(verb_command(catalog, noun, verb));
        }
        for registered_verb in registry()
            .into_iter()
            .filter(|c| c.path().len() == 2 && c.path()[0] == noun.name)
        {
            command = command.subcommand(registered_command(registered_verb));
        }
    }
    command
}

/// True if the request has a `bool follow` field and the verb is an event stream: the plain
/// `--follow` flag then sets that field (and keeps the stream open).
pub fn follow_is_field(catalog: &Catalog, verb: &Verb) -> bool {
    verb.is_event_stream()
        && catalog
            .message(verb.method.input_type)
            .and_then(|m| m.field("follow"))
            .is_some_and(|f| f.kind == Kind::Scalar(Scalar::Bool))
}

/// One generated `bee <noun> <verb>` with a flag per request field.
pub fn verb_command(catalog: &Catalog, noun: &Noun, verb: &Verb) -> Command {
    let method = verb.method;
    let mut long_about = if verb.comment.is_empty() {
        verb.summary()
    } else {
        verb.comment.clone()
    };
    long_about.push_str(&format!(
        "\n\nCalls {} ({}): request {}, response {}.",
        method.path,
        verb.shape(),
        method.input_type,
        method.output_type
    ));
    let mut command = Command::new(verb.name.clone())
        .about(verb.summary())
        .long_about(long_about)
        .after_help(after_help(catalog, noun, verb));
    let follow_field = follow_is_field(catalog, verb);
    if let Some(message) = catalog.message(method.input_type) {
        for flag in field_flags(message, follow_field) {
            let field = flag.field;
            let mut help = fields::flag_description(field);
            if !help.is_empty() {
                help.push(' ');
            }
            help.push_str(&flag_facts(catalog, field));
            let mut arg = Arg::new(flag.id.clone())
                .long(flag.long.clone())
                .value_name(value_name(catalog, field))
                .help_heading("Request fields")
                .help(help);
            arg = match (&field.cardinality, &field.kind) {
                (Cardinality::Single, Kind::Scalar(Scalar::Bool)) => arg
                    .num_args(0..=1)
                    .require_equals(true)
                    .default_missing_value("true")
                    .action(ArgAction::Set),
                (Cardinality::Single, _) => arg.action(ArgAction::Set),
                _ => arg.action(ArgAction::Append),
            };
            command = command.arg(arg);
        }
    }
    let input_help = if method.kind.is_unary()
        || method.kind == bee_proto::methods::MethodKind::ServerStreaming
    {
        format!(
            "The whole request as proto3-JSON (a {} object); flags override its fields",
            method.input_type
        )
    } else {
        format!(
            "Request messages ({}) as a JSON array or one JSON object per line; without it, \
             the flags form one message",
            method.input_type
        )
    };
    command = command
        .arg(
            Arg::new("json-input")
                .long("json-input")
                .value_name("JSON")
                .help_heading("Input")
                .help(input_help.clone()),
        )
        .arg(
            Arg::new("input-file")
                .long("input-file")
                .value_name("PATH|-")
                .help_heading("Input")
                .help(format!("{input_help}; read from a file, or `-` for stdin")),
        );
    if verb.is_event_stream() && !follow_field {
        command = command.arg(
            Arg::new("follow")
                .long("follow")
                .action(ArgAction::SetTrue)
                .help_heading("Streaming")
                .help(
                    "Keep the stream open and print events as they arrive (until the server \
                     ends it, --timeout, or Ctrl-C). Without it, stop after 2s with no new event.",
                ),
        );
    }
    command
}

fn sample_value(catalog: &Catalog, flag: &fields::FieldFlag<'_>) -> Option<String> {
    let field = flag.field;
    if let Cardinality::Map(..) = field.cardinality {
        return Some("'key=value'".into());
    }
    Some(match &field.kind {
        Kind::Scalar(Scalar::Bool) => String::new(),
        Kind::Scalar(Scalar::String | Scalar::Bytes) => format!("'<{}>'", flag.long),
        Kind::Scalar(Scalar::Double | Scalar::Float) => "1.5".into(),
        Kind::Scalar(_) => "1".into(),
        Kind::Enum(name) => catalog.enum_type(name)?.choices().first()?.clone(),
        Kind::Message(name) => match name.as_str() {
            "bee.v1.RepoRef" => "acme/app".into(),
            "bee.v1.ObjectId" => "'<object-id>'".into(),
            "bee.v1.ChangeId" => "'<change-id>'".into(),
            "bee.v1.Revision" => "main".into(),
            "google.protobuf.Timestamp" => "2026-01-02T03:04:05Z".into(),
            "google.protobuf.Duration" => "30s".into(),
            _ => return None,
        },
    })
}

/// A runnable example invocation of a generated command.
pub fn example(catalog: &Catalog, noun: &Noun, verb: &Verb) -> String {
    let mut line = format!("bee {} {}", noun.name, verb.name);
    if let Some(message) = catalog.message(verb.method.input_type) {
        let flags = field_flags(message, follow_is_field(catalog, verb));
        let mut chosen = 0;
        let mut seen_oneofs = Vec::new();
        for flag in &flags {
            if chosen == 3 {
                break;
            }
            if let Some(oneof) = &flag.field.oneof {
                if seen_oneofs.contains(oneof) {
                    continue;
                }
                seen_oneofs.push(oneof.clone());
            }
            if let Some(value) = sample_value(catalog, flag) {
                if value.is_empty() {
                    line.push_str(&format!(" --{}", flag.long));
                } else {
                    line.push_str(&format!(" --{} {value}", flag.long));
                }
                chosen += 1;
            }
        }
        if chosen == 0
            && let Some(flag) = flags.first()
        {
            line.push_str(&format!(
                " --json-input '{{\"{}\": ...}}'",
                flag.field.json_name
            ));
        }
    }
    line.push_str(" --json");
    line
}

fn after_help(catalog: &Catalog, noun: &Noun, verb: &Verb) -> String {
    let method = verb.method;
    let path = [noun.name.as_str(), verb.name.as_str()];
    let output = match method.kind {
        bee_proto::methods::MethodKind::Unary | bee_proto::methods::MethodKind::ClientStreaming => {
            format!(
                "--json prints {{\"schema\":\"bee.v1.{}.{}\",\"data\":<{}>}}",
                noun.name, verb.name, method.output_type
            )
        }
        _ => format!(
            "--json prints one {{\"schema\":\"bee.v1.{}.{}\",\"data\":<{}>}} per streamed \
             message, one per line (NDJSON)",
            noun.name, verb.name, method.output_type
        ),
    };
    let mut exits = Vec::new();
    for (code, _) in ExitCode::TABLE {
        exits.push(format!("{}={}", code.code(), code.label()));
    }
    format!(
        "EXAMPLE:\n  {}\n\nINPUT:   flags, or --json-input/--input-file with a proto3-JSON {}; \
         flags override.\nOUTPUT:  {}; failures print {{\"schema\":\"bee.v1.error\",\
         \"error\":{{\"reason\",\"message\",\"retryable\",\"payload\"}}}}.\nSCHEMAS: request {}, \
         output {} (or `bee schema {} {}`).\nEXIT:    {}",
        example(catalog, noun, verb),
        method.input_type,
        output,
        schemas::message_path(method.input_type),
        schemas::cli_schema_path(&path),
        noun.name,
        verb.name,
        exits.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn selects_the_noun_past_global_flags() {
        assert_eq!(
            selected_noun(&args(&["bee", "--endpoint", "x", "write", "land"])).as_deref(),
            Some("write")
        );
        assert_eq!(
            selected_noun(&args(&["bee", "--json", "-C", "d", "help", "change"])).as_deref(),
            Some("change")
        );
        assert_eq!(selected_noun(&args(&["bee", "--json"])), None);
    }

    #[test]
    fn whole_tree_is_valid() {
        root(Expand::All).debug_assert();
    }

    #[test]
    fn examples_name_real_flags() {
        let catalog = catalog();
        let noun = catalog.noun("write").expect("write");
        let land = example(catalog, noun, noun.verb("land").expect("land"));
        assert!(land.starts_with("bee write land --repo acme/app"), "{land}");
        assert!(land.ends_with("--json"), "{land}");
    }
}
