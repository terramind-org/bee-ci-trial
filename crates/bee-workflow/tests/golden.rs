//! Golden plans for real-world-style workflows.
//!
//! Each `tests/golden/<name>.case` holds an event line, optional `vars` line, and one or more
//! files separated by `--- <path>` lines. The plan for the event over those files must equal
//! `tests/golden/<name>.plan` byte for byte. Set `BEE_BLESS=1` to rewrite the `.plan` files
//! after a deliberate change, then review the diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bee_workflow::cron::UtcMinute;
use bee_workflow::event::{
    BeeEvent, Cause, ChangeRequestEvent, ChangedPaths, CrAction, MergeQueueEvent, PushEvent,
    RepoInfo, WorkflowRunEvent, map_event,
};
use bee_workflow::plan::{PlanInput, RunInfo, plan_event};
use bee_workflow::value::{Object, Value, parse_json};

const BEFORE: &str = "1111111111111111111111111111111111111111";
const AFTER: &str = "2222222222222222222222222222222222222222";
const DEFAULT_SHA: &str = "dddddddddddddddddddddddddddddddddddddddd";
const BASE_SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD_SHA: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const MERGE_SHA: &str = "ffffffffffffffffffffffffffffffffffffffff";

fn repo() -> RepoInfo {
    RepoInfo {
        owner: "octo-org".into(),
        name: "widget".into(),
        default_branch: "main".into(),
        default_branch_sha: DEFAULT_SHA.into(),
        server_url: "https://beehive.test".into(),
        api_url: "https://beehive.test/api/v3".into(),
        graphql_url: "https://beehive.test/api/graphql".into(),
    }
}

/// `key=value` options after the positional words of an event line. Values may be quoted.
fn split_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn opts(words: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut pos = Vec::new();
    let mut kv = BTreeMap::new();
    for w in words {
        match w.split_once('=') {
            Some((k, v)) => {
                kv.insert(k.to_owned(), v.replace("\\n", "\n"));
            }
            None => pos.push(w.clone()),
        }
    }
    (pos, kv)
}

fn files_opt(kv: &BTreeMap<String, String>, default: &[&str]) -> ChangedPaths {
    match kv.get("files").map(String::as_str) {
        Some("unknown") => ChangedPaths::Unknown,
        Some("") => ChangedPaths::Known(Vec::new()),
        Some(list) => ChangedPaths::Known(list.split(',').map(str::to_owned).collect()),
        None => ChangedPaths::Known(default.iter().map(|s| s.to_string()).collect()),
    }
}

fn cr(action: CrAction, kv: &BTreeMap<String, String>) -> ChangeRequestEvent {
    ChangeRequestEvent {
        action,
        number: 42,
        title: kv
            .get("title")
            .cloned()
            .unwrap_or_else(|| "Add feature".into()),
        base_ref: kv.get("base").cloned().unwrap_or_else(|| "main".into()),
        base_sha: BASE_SHA.into(),
        head_ref: kv
            .get("head")
            .cloned()
            .unwrap_or_else(|| "feature/x".into()),
        head_sha: HEAD_SHA.into(),
        merge_sha: Some(MERGE_SHA.into()),
        has_conflicts: kv.contains_key("conflict"),
        draft: kv.get("draft").is_some_and(|v| v == "true"),
        from_fork: kv.get("fork").is_some_and(|v| v == "true"),
        labels: kv
            .get("labels")
            .map(|l| l.split(',').map(str::to_owned).collect())
            .unwrap_or_default(),
        label: kv.get("label").cloned(),
        head_commit_message: kv.get("msg").cloned(),
        changed: files_opt(kv, &["src/lib.rs"]),
    }
}

fn cr_action(s: &str) -> CrAction {
    match s {
        "opened" => CrAction::Opened,
        "revised" => CrAction::Revised,
        "landed" => CrAction::Landed,
        "closed" => CrAction::Closed,
        "reopened" => CrAction::Reopened,
        "labeled" => CrAction::Labeled,
        "edited" => CrAction::Edited,
        "ready_for_review" => CrAction::ReadyForReview,
        "enqueued" => CrAction::Enqueued,
        other => panic!("unknown change request action {other}"),
    }
}

fn parse_event(line: &str) -> (BeeEvent, Cause) {
    // `payload=` takes the rest of the line verbatim (JSON has its own quotes).
    let (line, raw_payload) = match line.split_once(" payload=") {
        Some((head, json)) => (head, Some(json.to_owned())),
        None => (line, None),
    };
    let words = split_words(line);
    let (pos, mut kv) = opts(&words);
    if let Some(p) = raw_payload {
        kv.insert("payload".to_owned(), p);
    }
    let cause = Cause {
        actor: kv.get("actor").cloned().unwrap_or_else(|| "mona".into()),
        by_workflow_token: pos.iter().any(|w| w == "bot_token"),
    };
    let ev = match pos[0].as_str() {
        "push" => {
            let git_ref = pos[1].clone();
            let deleted = pos.iter().any(|w| w == "deleted");
            BeeEvent::Push(PushEvent {
                git_ref,
                before: BEFORE.into(),
                after: if deleted {
                    "0".repeat(40)
                } else {
                    AFTER.into()
                },
                created: pos.iter().any(|w| w == "created"),
                deleted,
                forced: false,
                commit_count: kv.get("commits").map_or(1, |c| c.parse().unwrap()),
                head_commit_message: Some(
                    kv.get("msg")
                        .cloned()
                        .unwrap_or_else(|| "Update code".into()),
                ),
                changed: files_opt(&kv, &["src/main.rs"]),
            })
        }
        "pr" => BeeEvent::ChangeRequest(cr(cr_action(&pos[1]), &kv)),
        "merge_group" => BeeEvent::MergeQueue(MergeQueueEvent {
            base_ref: kv.get("base").cloned().unwrap_or_else(|| "main".into()),
            base_sha: BASE_SHA.into(),
            head_ref: "refs/heads/gh-readonly-queue/main/pr-42-cccccccc".into(),
            head_sha: MERGE_SHA.into(),
            head_commit_message: "Merge pull request #42".into(),
        }),
        "release" => BeeEvent::Release {
            action: pos[1].clone(),
            tag: pos[2].clone(),
            sha: AFTER.into(),
            name: pos[2].clone(),
            prerelease: kv.get("prerelease").is_some_and(|v| v == "true"),
        },
        "issue" => BeeEvent::Issue {
            action: pos[1].clone(),
            number: 7,
            title: kv
                .get("title")
                .cloned()
                .unwrap_or_else(|| "Bug report".into()),
            labels: kv
                .get("labels")
                .map(|l| l.split(',').map(str::to_owned).collect())
                .unwrap_or_default(),
        },
        "comment" => BeeEvent::IssueComment {
            action: pos[1].clone(),
            number: 42,
            body: kv.get("body").cloned().unwrap_or_default(),
            on_change_request: pos.iter().any(|w| w == "pr"),
        },
        "schedule" => {
            let (date, time) = pos[1].split_once('T').unwrap();
            let d: Vec<u32> = date.split('-').map(|x| x.parse().unwrap()).collect();
            let t: Vec<u32> = time.split(':').map(|x| x.parse().unwrap()).collect();
            BeeEvent::ScheduleTick(UtcMinute {
                year: d[0] as i32,
                month: d[1],
                day: d[2],
                hour: t[0],
                minute: t[1],
            })
        }
        "dispatch" => BeeEvent::WorkflowDispatch {
            workflow: pos[1].clone(),
            git_ref: kv
                .get("ref")
                .cloned()
                .unwrap_or_else(|| "refs/heads/main".into()),
            sha: AFTER.into(),
            inputs: kv
                .iter()
                .filter_map(|(k, v)| k.strip_prefix("input.").map(|n| (n.to_owned(), v.clone())))
                .collect(),
        },
        "repository_dispatch" => BeeEvent::RepositoryDispatch {
            event_type: pos[1].clone(),
            client_payload: kv
                .get("payload")
                .map(|p| parse_json(p).unwrap())
                .unwrap_or(Value::Null),
        },
        "workflow_run" => BeeEvent::WorkflowRun(WorkflowRunEvent {
            action: pos[2].clone(),
            workflow_name: pos[1].clone(),
            conclusion: kv.get("conclusion").cloned(),
            head_branch: kv.get("branch").cloned().unwrap_or_else(|| "main".into()),
            head_sha: AFTER.into(),
            event: "push".into(),
            run_id: 9001,
            chain_depth: 0,
        }),
        other => panic!("unknown event kind {other}"),
    };
    (ev, cause)
}

struct Case {
    event: String,
    vars: Object,
    files: BTreeMap<String, String>,
}

fn parse_case(text: &str) -> Case {
    let mut event = None;
    let mut vars = Object::new();
    let mut files = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    for line in text.split_inclusive('\n') {
        if let Some(path) = line.strip_prefix("--- ") {
            if let Some((p, c)) = current.take() {
                files.insert(p, c);
            }
            current = Some((path.trim().to_owned(), String::new()));
        } else if let Some((_, c)) = &mut current {
            c.push_str(line);
        } else if let Some(e) = line.strip_prefix("event: ") {
            event = Some(e.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("vars: ") {
            for w in split_words(v.trim()) {
                let (k, val) = w.split_once('=').unwrap();
                vars.insert(k, Value::from(val));
            }
        }
    }
    if let Some((p, c)) = current.take() {
        files.insert(p, c);
    }
    Case {
        event: event.expect("case needs an 'event:' line"),
        vars,
        files,
    }
}

fn run_case(path: &Path) -> String {
    let case = parse_case(&std::fs::read_to_string(path).unwrap());
    let (ev, cause) = parse_event(&case.event);
    let r = repo();
    let mut out = String::new();
    for actions_event in map_event(&ev, &cause, &r) {
        let input = PlanInput {
            repo: &r,
            files: &case.files,
            event: &actions_event,
            run: RunInfo {
                run_id: 1234,
                run_number: 56,
                run_attempt: 1,
            },
            vars: case.vars.clone(),
            resolver: None,
        };
        let plan = plan_event(&input);
        out.push_str(&format!(
            "# event {} action={}\n",
            actions_event.name,
            actions_event.action.as_deref().unwrap_or("-")
        ));
        out.push_str(&plan.render());
    }
    out
}

fn cases() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "case"))
        .collect();
    v.sort();
    v
}

#[test]
fn golden_plans() {
    let bless = std::env::var("BEE_BLESS").is_ok_and(|v| v == "1");
    let all = cases();
    let mut failures = Vec::new();
    for case in &all {
        let got = run_case(case);
        let expected_path = case.with_extension("plan");
        if bless {
            std::fs::write(&expected_path, &got).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&expected_path).unwrap_or_default();
        if got != expected {
            failures.push(format!(
                "{}:\n--- expected\n{expected}\n--- got\n{got}",
                case.display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden plan(s) differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        all.len() >= 40,
        "expected at least 40 golden cases, found {}",
        all.len()
    );
}

#[test]
fn plans_are_deterministic() {
    for case in cases() {
        assert_eq!(run_case(&case), run_case(&case), "{}", case.display());
    }
}
