//! `bee cr evidence add|list|rm`: attach what a change does (screenshots, before and after,
//! video, benchmarks, test reports, preview links) to a change request, and read or remove it
//! (BriefService, SPEC §11.9).
//!
//! `add` sends the files inline (`AddEvidence`) while they total at most 8 MiB; larger files go
//! through `BeginEvidenceUpload`, one `PUT` per file to the signed URL the server returns, and
//! `FinalizeEvidence` with the SHA-256 of each file. Either way the result is one `Evidence`.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::aliases::{change_arg, change_id};
use super::history::{Scope, repo_arg, scope};
use super::{arg, args_of, flag};
use crate::commands::{Command, message_ref};
use crate::context::Context;
use crate::error::CliError;

/// Files totalling at most this many bytes travel inside the `AddEvidence` request.
const INLINE_LIMIT: usize = 8 << 20;

/// The evidence kinds `--kind` accepts, as written on the command line.
const KINDS: [&str; 9] = [
    "screenshot",
    "before-after",
    "video",
    "benchmark",
    "image",
    "audio",
    "document",
    "preview-link",
    "test-report",
];

/// `bee cr evidence`.
pub struct CrEvidence;

impl Command for CrEvidence {
    fn path(&self) -> &'static [&'static str] {
        &["cr", "evidence"]
    }

    fn about(&self) -> &'static str {
        "Attach, list or remove evidence of a change request: add, list, rm (BriefService)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Evidence shows what a change request does: a screenshot, a before and after, a \
                 short video, benchmark numbers, a test report or a preview link (SPEC §11.9). \
                 `add` and `rm` need the right to edit the change request; `list` needs read \
                 access. Evidence is flagged stale once a newer revision exists, so capture it \
                 again after revising. Same calls as `bee brief add-evidence`, \
                 `list-evidence` and `delete-evidence` (with uploads of large files handled \
                 for you); output data is a bee.v1.Evidence, a \
                 bee.v1.ListEvidenceResponse or a bee.v1.DeleteEvidenceResponse.",
            )
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(add_command())
            .subcommand(list_command())
            .subcommand(rm_command())
            .after_help(
                "EXAMPLE:\n  bee cr evidence add 12 --before before.png --after after.png \\\n    \
                 --alt \"The checkout page\" --hero --json\n  \
                 bee cr evidence add 12 --bench \"p95 latency:ms:120:85:lower\" --json\n  \
                 bee cr evidence list 12 --json\n  \
                 bee cr evidence rm 01J9Z3M8Q4X7R2T5V6W8Y9A0BC --json",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "anyOf": [
                message_ref("bee.v1.Evidence"),
                message_ref("bee.v1.ListEvidenceResponse"),
                message_ref("bee.v1.DeleteEvidenceResponse"),
            ]
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        match args.subcommand() {
            Some(("add", m)) => add(ctx, m).map(Some),
            Some(("list", m)) => list(ctx, m).map(Some),
            Some(("rm", m)) => rm(ctx, m).map(Some),
            _ => Err(CliError::usage(
                "missing_subcommand",
                "use `bee cr evidence add|list|rm`",
            )),
        }
    }

    fn render(&self, data: &Value) -> String {
        if let Some(items) = data["evidence"].as_array() {
            if items.is_empty() {
                return "no evidence".into();
            }
            return items.iter().map(describe).collect::<Vec<_>>().join("\n");
        }
        if data["id"].is_string() {
            return describe(data);
        }
        "removed".into()
    }
}

fn add_command() -> clap::Command {
    clap::Command::new("add")
        .about("Attach one piece of evidence to a change request.")
        .long_about(
            "Attach one piece of evidence. The kind is read from the files when --kind is not \
             given: one image is a screenshot, several are an image set, --before with --after \
             is a before and after, a video, audio or document is its own kind, and --bench, \
             --tests and --url make a benchmark, test report and preview link. Every image \
             needs --alt (what it shows, in words). With --artifact, the files name members \
             of that workflow artifact instead of files on disk; with --check-run, the \
             evidence is reported as verified by that check run.",
        )
        .arg(change_arg())
        .arg(
            Arg::new("file")
                .value_name("FILE")
                .num_args(0..)
                .help("Files of the evidence (images, a video, audio or a document)."),
        )
        .arg(
            Arg::new("kind")
                .long("kind")
                .value_name("KIND")
                .value_parser(KINDS)
                .help("What the evidence is (default: read from the files and flags)."),
        )
        .arg(Arg::new("title").long("title").value_name("TEXT").help("Title (default: the file name)."))
        .arg(Arg::new("caption").long("caption").value_name("TEXT").help("One or two sentences on what to look at."))
        .arg(Arg::new("alt").long("alt").value_name("TEXT").help("What an image shows, for people who cannot see it."))
        .arg(Arg::new("before").long("before").value_name("FILE").help("The image from before the change."))
        .arg(Arg::new("before-alt").long("before-alt").value_name("TEXT").help("Alt text of --before (default: --alt)."))
        .arg(Arg::new("after").long("after").value_name("FILE").help("The image from after the change."))
        .arg(Arg::new("after-alt").long("after-alt").value_name("TEXT").help("Alt text of --after (default: --alt)."))
        .arg(Arg::new("poster").long("poster").value_name("FILE").help("A still frame shown before a video plays."))
        .arg(Arg::new("captions").long("captions").value_name("FILE").help("WebVTT captions of a video or audio file."))
        .arg(
            Arg::new("width")
                .long("width")
                .value_name("PX")
                .value_parser(clap::value_parser!(u32))
                .help("Width of a video (images are measured by the server)."),
        )
        .arg(
            Arg::new("height")
                .long("height")
                .value_name("PX")
                .value_parser(clap::value_parser!(u32))
                .help("Height of a video."),
        )
        .arg(
            Arg::new("duration-ms")
                .long("duration-ms")
                .value_name("MS")
                .value_parser(clap::value_parser!(u64))
                .help("Length of a video or audio file."),
        )
        .arg(
            Arg::new("bench")
                .long("bench")
                .value_name("NAME:UNIT:BASE:VALUE:lower|higher")
                .action(ArgAction::Append)
                .help("A benchmark series: the baseline, the new value and which direction is better (repeatable)."),
        )
        .arg(
            Arg::new("tests")
                .long("tests")
                .value_name("PASSED:FAILED:SKIPPED[:MS]")
                .help("A test report in numbers."),
        )
        .arg(Arg::new("summary").long("summary").value_name("TEXT").help("The test report in one line of words."))
        .arg(
            Arg::new("failure")
                .long("failure")
                .value_name("NAME::MESSAGE")
                .action(ArgAction::Append)
                .help("A failed test of the report (repeatable)."),
        )
        .arg(Arg::new("url").long("url").value_name("URL").help("Preview link: an https URL where the change runs."))
        .arg(
            Arg::new("path")
                .long("path")
                .value_name("PATH")
                .action(ArgAction::Append)
                .help("A repo path the evidence shows (repeatable)."),
        )
        .arg(Arg::new("hero").long("hero").action(ArgAction::SetTrue).help("Show this first; it replaces the current hero."))
        .arg(
            Arg::new("order")
                .long("order")
                .value_name("N")
                .value_parser(clap::value_parser!(i32))
                .help("Position in the gallery, ascending."),
        )
        .arg(
            Arg::new("revision")
                .long("revision")
                .value_name("N")
                .value_parser(clap::value_parser!(u32))
                .help("Revision the evidence was captured for (default: the current one)."),
        )
        .arg(Arg::new("check-run").long("check-run").value_name("ID").help("Report the evidence as verified by this check run you reported."))
        .arg(Arg::new("artifact").long("artifact").value_name("ID").help("Copy the files from this workflow artifact of a run on the change."))
        .arg(repo_arg())
        .after_help(
            "EXAMPLE:\n  bee cr evidence add 12 shot.png --alt \"The new settings page\" --hero --json",
        )
}

fn list_command() -> clap::Command {
    clap::Command::new("list")
        .about("List the evidence of a change request.")
        .arg(change_arg())
        .arg(
            Arg::new("hero-only")
                .long("hero-only")
                .action(ArgAction::SetTrue)
                .help("Only the hero (or, without one, the first item)."),
        )
        .arg(repo_arg())
        .after_help("EXAMPLE:\n  bee cr evidence list 12 --json")
}

fn rm_command() -> clap::Command {
    clap::Command::new("rm")
        .about("Remove one piece of evidence and its files.")
        .arg(
            Arg::new("id")
                .value_name("EVIDENCE")
                .required(true)
                .help("Evidence id (from `bee cr evidence list`)."),
        )
        .arg(repo_arg())
        .after_help("EXAMPLE:\n  bee cr evidence rm 01J9Z3M8Q4X7R2T5V6W8Y9A0BC --json")
}

/// One evidence item on one line.
fn describe(item: &Value) -> String {
    let kind = item["spec"]["kind"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("EVIDENCE_KIND_")
        .to_lowercase()
        .replace('_', "-");
    let source = match item["source"].as_str().unwrap_or_default() {
        "EVIDENCE_SOURCE_CHECK_RUN" | "EVIDENCE_SOURCE_WORKFLOW_ARTIFACT" => "verified by CI",
        _ => "from the agent",
    };
    let mut line = format!(
        "{} {kind} \"{}\" ({source}, revision {}",
        item["id"].as_str().unwrap_or_default(),
        item["spec"]["title"].as_str().unwrap_or_default(),
        item["revision"].as_u64().unwrap_or(0)
    );
    if item["stale"].as_bool().unwrap_or(false) {
        line.push_str(", stale: capture it again");
    }
    if item["state"].as_str() == Some("EVIDENCE_STATE_PENDING") {
        line.push_str(", upload not finished");
    }
    line.push(')');
    line
}

/// A file of the evidence: where it is and the part it plays.
struct Part {
    role: &'static str,
    /// A path on disk, or a member path inside a workflow artifact with `--artifact`.
    path: String,
    alt: Option<String>,
}

/// What the bytes are, from the file name. Servers sniff the bytes and refuse a mismatch.
fn content_type_of(path: &str) -> Result<&'static str, CliError> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Ok(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "vtt" => "text/vtt",
        _ => {
            return Err(CliError::usage(
                "bad_media",
                format!(
                    "{path}: cannot tell the file type from its name; evidence files are png, \
                     jpg, webp, gif, mp4, webm, mp3, ogg, wav, pdf, txt, md, csv or vtt"
                ),
            ));
        }
    })
}

/// The kind the files and flags describe, as the enum name.
fn kind_of(args: &ArgMatches, parts: &[Part]) -> Result<String, CliError> {
    if let Some(k) = arg(args, "kind") {
        return Ok(format!(
            "EVIDENCE_KIND_{}",
            k.to_uppercase().replace('-', "_")
        ));
    }
    let named = |k: &str| format!("EVIDENCE_KIND_{k}");
    if !args_of(args, "bench").is_empty() {
        return Ok(named("BENCHMARK"));
    }
    if arg(args, "tests").is_some() {
        return Ok(named("TEST_REPORT"));
    }
    if arg(args, "url").is_some() {
        return Ok(named("PREVIEW_LINK"));
    }
    if parts.iter().any(|p| p.role == "EVIDENCE_ASSET_ROLE_BEFORE") {
        return Ok(named("BEFORE_AFTER"));
    }
    let mains: Vec<&Part> = parts
        .iter()
        .filter(|p| p.role == "EVIDENCE_ASSET_ROLE_MAIN")
        .collect();
    let first = mains.first().ok_or_else(|| {
        CliError::usage(
            "missing_evidence",
            "give a file, --before and --after, --bench, --tests or --url (or say --kind)",
        )
    })?;
    let ctype = content_type_of(&first.path)?;
    Ok(named(match ctype.split('/').next().unwrap_or_default() {
        "video" => "VIDEO",
        "audio" => "AUDIO",
        "image" if mains.len() > 1 => "IMAGE",
        "image" => "SCREENSHOT",
        _ => "DOCUMENT",
    }))
}

/// The title to use when `--title` is not given: the first file's name, else the kind in words.
fn default_title(parts: &[Part], kind: &str) -> String {
    parts
        .first()
        .and_then(|p| Path::new(&p.path).file_stem())
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            kind.trim_start_matches("EVIDENCE_KIND_")
                .to_lowercase()
                .replace('_', " ")
        })
}

fn bad(flag_name: &str, text: &str, expected: &str) -> CliError {
    CliError::usage(
        "bad_flag",
        format!("--{flag_name} {text:?}: use {expected}"),
    )
}

fn number<T: std::str::FromStr>(
    flag_name: &str,
    text: &str,
    expected: &str,
) -> Result<T, CliError> {
    text.trim()
        .parse()
        .map_err(|_| bad(flag_name, text, expected))
}

/// `--bench "name:unit:base:value:lower"`, read from the right so a name may hold colons.
fn benchmark(text: &str) -> Result<Value, CliError> {
    const FORM: &str = "name:unit:baseline:value:lower|higher";
    let parts: Vec<&str> = text.rsplitn(5, ':').collect();
    let [direction, value, baseline, unit, name] = parts.as_slice() else {
        return Err(bad("bench", text, FORM));
    };
    Ok(json!({
        "name": name.trim(),
        "unit": unit.trim(),
        "baseline": number::<f64>("bench", baseline, FORM)?,
        "value": number::<f64>("bench", value, FORM)?,
        "lowerIsBetter": match direction.trim() {
            "lower" => true,
            "higher" => false,
            _ => return Err(bad("bench", text, FORM)),
        },
    }))
}

/// `--tests passed:failed:skipped[:ms]` with `--summary` and `--failure`.
fn test_report(args: &ArgMatches, text: &str) -> Result<Value, CliError> {
    const FORM: &str = "passed:failed:skipped[:duration_ms]";
    let nums: Vec<&str> = text.split(':').collect();
    if !(3..=4).contains(&nums.len()) {
        return Err(bad("tests", text, FORM));
    }
    let failures: Vec<Value> = args_of(args, "failure")
        .iter()
        .map(|f| {
            let (name, message) = f.split_once("::").unwrap_or((f.as_str(), ""));
            json!({"name": name.trim(), "message": message.trim()})
        })
        .collect();
    let mut report = json!({
        "passed": number::<u32>("tests", nums[0], FORM)?,
        "failed": number::<u32>("tests", nums[1], FORM)?,
        "skipped": number::<u32>("tests", nums[2], FORM)?,
        "failures": failures,
        "summary": arg(args, "summary").unwrap_or_default(),
    });
    if let Some(ms) = nums.get(3) {
        report["durationMs"] = json!(number::<u64>("tests", ms, FORM)?.to_string());
    }
    Ok(report)
}

/// The spec the flags describe.
fn spec_of(
    scope: &Scope,
    args: &ArgMatches,
    parts: &[Part],
    kind: &str,
) -> Result<Value, CliError> {
    let mut spec = json!({
        "kind": kind,
        "title": arg(args, "title").unwrap_or_else(|| default_title(parts, kind)),
        "caption": arg(args, "caption").unwrap_or_default(),
        "hero": flag(args, "hero"),
    });
    if let Some(order) = args.get_one::<i32>("order") {
        spec["order"] = json!(order);
    }
    let series: Vec<Value> = args_of(args, "bench")
        .iter()
        .map(|b| benchmark(b))
        .collect::<Result<_, _>>()?;
    if !series.is_empty() {
        spec["benchmark"] = json!(series);
    }
    if let Some(tests) = arg(args, "tests") {
        spec["testReport"] = test_report(args, &tests)?;
    }
    if let Some(url) = arg(args, "url") {
        spec["url"] = json!(url);
    }
    let paths = args_of(args, "path");
    if !paths.is_empty() {
        spec["paths"] = json!(scope.paths(&paths)?);
    }
    Ok(spec)
}

/// The files of the evidence, in the order the server stores them.
fn parts_of(args: &ArgMatches) -> Vec<Part> {
    let alt = arg(args, "alt");
    let part = |role: &'static str, path: String, alt: Option<String>| Part { role, path, alt };
    let mut parts: Vec<Part> = args_of(args, "file")
        .into_iter()
        .map(|f| part("EVIDENCE_ASSET_ROLE_MAIN", f, alt.clone()))
        .collect();
    for (flag_name, role, own_alt) in [
        (
            "before",
            "EVIDENCE_ASSET_ROLE_BEFORE",
            arg(args, "before-alt"),
        ),
        ("after", "EVIDENCE_ASSET_ROLE_AFTER", arg(args, "after-alt")),
        ("poster", "EVIDENCE_ASSET_ROLE_POSTER", None),
        ("captions", "EVIDENCE_ASSET_ROLE_CAPTIONS", None),
    ] {
        if let Some(path) = arg(args, flag_name) {
            let alt = if role == "EVIDENCE_ASSET_ROLE_CAPTIONS" {
                None
            } else {
                own_alt.or_else(|| alt.clone())
            };
            parts.push(part(role, path, alt));
        }
    }
    parts
}

/// The size, type and alt fields every request form of an asset shares.
fn media_fields(args: &ArgMatches, part: &Part, asset: &mut Value) {
    if let Some(alt) = &part.alt {
        asset["alt"] = json!(alt);
    }
    if part.role == "EVIDENCE_ASSET_ROLE_MAIN" {
        if let Some(w) = args.get_one::<u32>("width") {
            asset["width"] = json!(w);
        }
        if let Some(h) = args.get_one::<u32>("height") {
            asset["height"] = json!(h);
        }
        if let Some(ms) = args.get_one::<u64>("duration-ms") {
            asset["durationMs"] = json!(ms.to_string());
        }
    }
}

fn add(ctx: &mut Context, args: &ArgMatches) -> Result<Value, CliError> {
    let scope = scope(ctx, args)?;
    // Everything the flags can get wrong is checked before the first request is made.
    let parts = parts_of(args);
    let kind = kind_of(args, &parts)?;
    let spec = spec_of(&scope, args, &parts, &kind)?;
    let change = change_id(&scope, &arg(args, "change").unwrap_or_default())?;
    let mut req = json!({"repo": scope.repo_json(), "changeId": change, "spec": spec});
    if let Some(r) = args.get_one::<u32>("revision") {
        req["revision"] = json!(r);
    }
    if let Some(run) = arg(args, "check-run") {
        req["checkRunId"] = json!(run);
    }
    if let Some(artifact) = arg(args, "artifact") {
        req["workflowArtifactId"] = json!(artifact);
        let mut assets: Vec<Value> = parts
            .iter()
            .map(|p| {
                let mut a = json!({"role": p.role, "path": p.path});
                media_fields(args, p, &mut a);
                a
            })
            .collect();
        // A single-file artifact has no member to name: its one file is the main asset.
        if assets.is_empty()
            && matches!(
                kind.as_str(),
                "EVIDENCE_KIND_SCREENSHOT"
                    | "EVIDENCE_KIND_VIDEO"
                    | "EVIDENCE_KIND_IMAGE"
                    | "EVIDENCE_KIND_AUDIO"
                    | "EVIDENCE_KIND_DOCUMENT"
            )
        {
            let mut a = json!({"role": "EVIDENCE_ASSET_ROLE_MAIN"});
            if let Some(alt) = arg(args, "alt") {
                a["alt"] = json!(alt);
            }
            assets.push(a);
        }
        req["artifactAssets"] = json!(assets);
        return scope.unary("/bee.v1.BriefService/AddEvidence", req);
    }
    let mut files = Vec::new();
    for p in &parts {
        let data = std::fs::read(&p.path)
            .map_err(|e| CliError::usage("bad_flag", format!("{}: cannot read it: {e}", p.path)))?;
        files.push((p, content_type_of(&p.path)?, data));
    }
    let total: usize = files.iter().map(|(_, _, d)| d.len()).sum();
    if total <= INLINE_LIMIT {
        let assets: Vec<Value> = files
            .iter()
            .map(|(p, ctype, data)| {
                let mut a = json!({
                    "role": p.role,
                    "contentType": ctype,
                    "data": STANDARD.encode(data),
                });
                media_fields(args, p, &mut a);
                a
            })
            .collect();
        req["assets"] = json!(assets);
        return scope.unary("/bee.v1.BriefService/AddEvidence", req);
    }
    upload(&scope, req, &files, args)
}

/// Begin, one `PUT` per file, Finalize: the way for files too big to send inline.
fn upload(
    scope: &Scope,
    mut req: Value,
    files: &[(&Part, &'static str, Vec<u8>)],
    args: &ArgMatches,
) -> Result<Value, CliError> {
    let assets: Vec<Value> = files
        .iter()
        .map(|(p, ctype, data)| {
            let mut a = json!({
                "role": p.role,
                "contentType": ctype,
                "sizeBytes": data.len().to_string(),
            });
            media_fields(args, p, &mut a);
            a
        })
        .collect();
    req["assets"] = json!(assets);
    let begun = scope.unary("/bee.v1.BriefService/BeginEvidenceUpload", req.clone())?;
    let targets = begun["uploads"].as_array().cloned().unwrap_or_default();
    if targets.len() != files.len() {
        return Err(CliError::internal(format!(
            "the server returned {} upload URLs for {} files",
            targets.len(),
            files.len()
        )));
    }
    let mut digests = Vec::new();
    for ((part, ctype, data), target) in files.iter().zip(&targets) {
        let url = target["uploadUrl"]
            .as_str()
            .ok_or_else(|| CliError::internal("the server sent an upload target without a URL"))?;
        let ctype = target["uploadHeaders"]
            .as_object()
            .and_then(|h| {
                h.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .and_then(|(_, v)| v.as_str())
            })
            .unwrap_or(ctype);
        scope
            .transport
            .put(url, ctype, data.clone())
            .map_err(|d| CliError::from_detail(&d, "upload failed"))?;
        digests.push(json!({
            "role": part.role,
            "sha256": format!("{:x}", Sha256::digest(data)),
        }));
    }
    scope.unary(
        "/bee.v1.BriefService/FinalizeEvidence",
        json!({
            "repo": scope.repo_json(),
            "evidenceId": begun["evidence"]["id"],
            "digests": digests,
        }),
    )
}

fn list(ctx: &mut Context, args: &ArgMatches) -> Result<Value, CliError> {
    let scope = scope(ctx, args)?;
    let change = change_id(&scope, &arg(args, "change").unwrap_or_default())?;
    scope.unary(
        "/bee.v1.BriefService/ListEvidence",
        json!({
            "repo": scope.repo_json(),
            "changeIds": [change],
            "heroOnly": flag(args, "hero-only"),
        }),
    )
}

fn rm(ctx: &mut Context, args: &ArgMatches) -> Result<Value, CliError> {
    let scope = scope(ctx, args)?;
    scope.unary(
        "/bee.v1.BriefService/DeleteEvidence",
        json!({"repo": scope.repo_json(), "id": arg(args, "id").unwrap_or_default()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmarks_parse_from_the_right() {
        let b = benchmark("p95: latency:ms:120:85.5:lower").expect("bench");
        assert_eq!(b["name"], "p95: latency");
        assert_eq!(b["unit"], "ms");
        assert_eq!(b["baseline"], 120.0);
        assert_eq!(b["value"], 85.5);
        assert_eq!(b["lowerIsBetter"], true);
        assert_eq!(
            benchmark("rps:req/s:900:1200:higher").expect("bench")["lowerIsBetter"],
            false
        );
        for text in ["p95:ms:120:85", "p95:ms:x:85:lower", "p95:ms:120:85:faster"] {
            assert!(benchmark(text).is_err(), "{text}");
        }
    }

    #[test]
    fn file_types_come_from_the_name() {
        assert_eq!(content_type_of("a/b.PNG").expect("png"), "image/png");
        assert_eq!(content_type_of("demo.webm").expect("webm"), "video/webm");
        assert_eq!(content_type_of("notes.md").expect("md"), "text/markdown");
        assert!(content_type_of("diagram.svg").is_err());
        assert!(content_type_of("noext").is_err());
    }

    #[test]
    fn evidence_lines_say_where_it_came_from_and_when_it_is_stale() {
        let line = describe(&json!({
            "id": "e1",
            "revision": 2,
            "stale": true,
            "source": "EVIDENCE_SOURCE_CHECK_RUN",
            "spec": {"kind": "EVIDENCE_KIND_BEFORE_AFTER", "title": "Checkout"},
        }));
        assert_eq!(
            line,
            "e1 before-after \"Checkout\" (verified by CI, revision 2, stale: capture it again)"
        );
    }
}
