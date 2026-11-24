//! Parsing and validating workflow YAML into a [`Workflow`].
//!
//! Validation follows GitHub: unknown keys, wrong types, missing required keys, bad ids,
//! unknown `needs`, dependency cycles, conflicting filters, unknown activity types, malformed
//! `uses`, and expressions that reference contexts or functions not available where they are
//! written ("Unrecognized named-value") are all errors. Every error carries the line and
//! column of the node at fault, and all errors in a file are reported together.

use crate::cron::Cron;
use crate::error::{Diagnostic, ErrorKind, WorkflowError};
use crate::expr::{Condition, Expr, Function, Template};
use crate::glob::PatternList;
use crate::model::*;
use crate::value::format_number;
use crate::yaml::{self, Node, NodeKind, Pos, Scalar};

/// Maximum number of `workflow_dispatch` inputs GitHub accepts.
pub const MAX_DISPATCH_INPUTS: usize = 25;

/// Every event a workflow can listen to, with its activity types (`None`: no `types` key;
/// `Some(&[])`: free-form types, as for `repository_dispatch`).
pub const EVENTS: &[(&str, Option<&[&str]>)] = &[
    (
        "branch_protection_rule",
        Some(&["created", "edited", "deleted"]),
    ),
    (
        "check_run",
        Some(&["created", "rerequested", "completed", "requested_action"]),
    ),
    ("check_suite", Some(&["completed"])),
    ("create", None),
    ("delete", None),
    ("deployment", None),
    ("deployment_status", None),
    (
        "discussion",
        Some(&[
            "created",
            "edited",
            "deleted",
            "transferred",
            "pinned",
            "unpinned",
            "labeled",
            "unlabeled",
            "locked",
            "unlocked",
            "category_changed",
            "answered",
            "unanswered",
        ]),
    ),
    (
        "discussion_comment",
        Some(&["created", "edited", "deleted"]),
    ),
    ("fork", None),
    ("gollum", None),
    ("image_version", Some(&["created", "ready"])),
    ("issue_comment", Some(&["created", "edited", "deleted"])),
    (
        "issues",
        Some(&[
            "opened",
            "edited",
            "deleted",
            "transferred",
            "pinned",
            "unpinned",
            "closed",
            "reopened",
            "assigned",
            "unassigned",
            "labeled",
            "unlabeled",
            "locked",
            "unlocked",
            "milestoned",
            "demilestoned",
            "typed",
            "untyped",
            "field_added",
            "field_removed",
        ]),
    ),
    ("label", Some(&["created", "edited", "deleted"])),
    ("merge_group", Some(&["checks_requested"])),
    (
        "milestone",
        Some(&["created", "closed", "opened", "edited", "deleted"]),
    ),
    ("page_build", None),
    ("public", None),
    ("pull_request", Some(PULL_REQUEST_TYPES)),
    (
        "pull_request_review",
        Some(&["submitted", "edited", "dismissed"]),
    ),
    (
        "pull_request_review_comment",
        Some(&["created", "edited", "deleted"]),
    ),
    ("pull_request_target", Some(PULL_REQUEST_TYPES)),
    ("push", None),
    ("registry_package", Some(&["published", "updated"])),
    (
        "release",
        Some(&[
            "published",
            "unpublished",
            "created",
            "edited",
            "deleted",
            "prereleased",
            "released",
        ]),
    ),
    ("repository_dispatch", Some(&[])),
    ("schedule", None),
    ("status", None),
    ("watch", Some(&["started"])),
    ("workflow_call", None),
    ("workflow_dispatch", None),
    (
        "workflow_run",
        Some(&["completed", "requested", "in_progress"]),
    ),
];

/// `pull_request` and `pull_request_target` activity types.
pub const PULL_REQUEST_TYPES: &[&str] = &[
    "assigned",
    "unassigned",
    "labeled",
    "unlabeled",
    "opened",
    "edited",
    "closed",
    "reopened",
    "synchronize",
    "converted_to_draft",
    "locked",
    "unlocked",
    "enqueued",
    "dequeued",
    "milestoned",
    "demilestoned",
    "ready_for_review",
    "review_request_removed",
    "review_requested",
    "auto_merge_enabled",
    "auto_merge_disabled",
];

/// Activity types that trigger when `types` is omitted, if not all of them.
pub fn default_types(event: &str) -> Option<&'static [&'static str]> {
    match event {
        "pull_request" | "pull_request_target" => Some(&["opened", "synchronize", "reopened"]),
        _ => None,
    }
}

// Context availability, from GitHub's "Context availability" table.
const C_RUN_NAME: &[&str] = &["github", "inputs", "vars"];
const C_WF_ENV: &[&str] = &["github", "secrets", "inputs", "vars"];
const C_JOB_IF: &[&str] = &["github", "needs", "vars", "inputs"];
const C_JOB_PRE: &[&str] = &["github", "needs", "strategy", "matrix", "vars", "inputs"];
const C_JOB_ENV: &[&str] = &[
    "github", "needs", "strategy", "matrix", "vars", "secrets", "inputs",
];
const C_JOB_DEFAULTS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "env", "vars", "inputs",
];
const C_CREDENTIALS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "env", "vars", "secrets", "inputs",
];
const C_CONTAINER_ENV: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "inputs",
];
const C_ENV_URL: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "steps", "inputs",
];
const C_JOB_OUTPUTS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "secrets", "steps",
    "inputs",
];
const C_STEP_IF: &[&str] = &[
    "github", "needs", "strategy", "matrix", "job", "runner", "env", "vars", "steps", "inputs",
];
const C_STEP: &[&str] = C_JOB_OUTPUTS;
const C_CALL_SECRETS: &[&str] = &[
    "github", "needs", "strategy", "matrix", "secrets", "inputs", "vars",
];
const C_CALL_DEFAULT: &[&str] = &["github", "inputs", "vars"];
const C_CALL_OUTPUT: &[&str] = &["github", "jobs", "vars", "inputs"];

/// Which functions are allowed in a field besides the general ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Funcs {
    General,
    /// `hashFiles` allowed.
    Step,
    /// Status functions allowed (job `if`).
    JobIf,
    /// Status functions and `hashFiles` (step `if`).
    StepIf,
}

struct Parser<'a> {
    path: &'a str,
    diags: Vec<Diagnostic>,
}

/// Whether `id` is a valid job, step or input id: a letter or `_`, then letters, digits, `-`, `_`.
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parses and validates a workflow file.
pub fn parse_workflow(path: &str, source: &str) -> Result<Workflow, WorkflowError> {
    let root = yaml::parse(source).map_err(|e| WorkflowError::invalid(path, e.pos, e.message))?;
    let mut p = Parser {
        path,
        diags: Vec::new(),
    };
    let wf = p.workflow(&root);
    if p.diags.is_empty()
        && let Some(wf) = wf
    {
        return Ok(wf);
    }
    let mut diags = p.diags;
    diags.sort();
    diags.dedup();
    if diags.is_empty() {
        diags.push(Diagnostic {
            pos: root.pos,
            message: "invalid workflow".to_owned(),
        });
    }
    Err(WorkflowError {
        path: path.to_owned(),
        kind: ErrorKind::Invalid,
        diagnostics: diags,
    })
}

impl Parser<'_> {
    fn err(&mut self, pos: Pos, message: impl Into<String>) {
        self.diags.push(Diagnostic {
            pos,
            message: message.into(),
        });
    }

    fn map<'n>(&mut self, node: &'n Node, what: &str) -> Option<&'n [(Node, Node)]> {
        match node.as_map() {
            Some(m) => Some(m),
            None => {
                self.err(
                    node.pos,
                    format!("'{what}' must be a mapping, found a {}", node.type_name()),
                );
                None
            }
        }
    }

    fn check_keys(&mut self, entries: &[(Node, Node)], allowed: &[&str]) {
        for (k, _) in entries {
            let key = k.as_str().unwrap_or_default();
            if !allowed.contains(&key) {
                self.err(k.pos, format!("Unexpected value '{key}'"));
            }
        }
    }

    /// A scalar as a string (numbers and booleans in their canonical text).
    fn string(&mut self, node: &Node, what: &str) -> Option<String> {
        match node.scalar() {
            Some(Scalar::Str(s)) => Some(s.to_owned()),
            Some(Scalar::Number(n)) => Some(format_number(n)),
            Some(Scalar::Bool(b)) => Some(b.to_string()),
            Some(Scalar::Null) => Some(String::new()),
            None => {
                self.err(
                    node.pos,
                    format!("'{what}' must be a string, found a {}", node.type_name()),
                );
                None
            }
        }
    }

    /// A string or a sequence of strings.
    fn string_list(&mut self, node: &Node, what: &str) -> Option<Vec<String>> {
        match &node.kind {
            NodeKind::Seq(items) => {
                let mut out = Vec::new();
                for i in items {
                    if i.as_map().is_some() || i.as_seq().is_some() {
                        self.err(i.pos, format!("'{what}' entries must be strings"));
                        continue;
                    }
                    out.push(self.string(i, what)?);
                }
                Some(out)
            }
            NodeKind::Scalar(..) if !node.is_null() => Some(vec![self.string(node, what)?]),
            _ => {
                self.err(
                    node.pos,
                    format!("'{what}' must be a string or a sequence of strings"),
                );
                None
            }
        }
    }

    fn bool(&mut self, node: &Node, what: &str) -> Option<bool> {
        match node.scalar() {
            Some(Scalar::Bool(b)) => Some(b),
            _ => {
                self.err(
                    node.pos,
                    format!("'{what}' must be a boolean, found a {}", node.type_name()),
                );
                None
            }
        }
    }

    fn check_exprs(&mut self, pos: Pos, exprs: &[(&str, &Expr)], allowed: &[&str], funcs: Funcs) {
        for (source, e) in exprs {
            for c in e.contexts() {
                if !allowed.contains(&c.as_str()) {
                    self.err(
                        pos,
                        format!(
                            "Unrecognized named-value: '{c}'. Located within expression: {source}"
                        ),
                    );
                }
            }
            for f in e.functions() {
                let ok = match f {
                    Function::HashFiles => matches!(funcs, Funcs::Step | Funcs::StepIf),
                    f if f.is_status() => matches!(funcs, Funcs::JobIf | Funcs::StepIf),
                    _ => true,
                };
                if !ok {
                    self.err(
                        pos,
                        format!(
                            "Unrecognized function: '{}'. Located within expression: {source}",
                            f.name()
                        ),
                    );
                }
            }
        }
    }

    fn template_from_str(
        &mut self,
        text: &str,
        pos: Pos,
        allowed: &[&str],
        funcs: Funcs,
    ) -> Option<Template> {
        match Template::parse(text) {
            Ok(t) => {
                let exprs: Vec<(&str, &Expr)> = t
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        crate::expr::Part::Expr { source, expr } => Some((source.as_str(), expr)),
                        crate::expr::Part::Text(_) => None,
                    })
                    .collect();
                self.check_exprs(pos, &exprs, allowed, funcs);
                Some(t)
            }
            Err(e) => {
                self.err(pos, format!("invalid expression: {e}"));
                None
            }
        }
    }

    fn template(
        &mut self,
        node: &Node,
        what: &str,
        allowed: &[&str],
        funcs: Funcs,
    ) -> Option<Template> {
        let s = self.string(node, what)?;
        self.template_from_str(&s, node.pos, allowed, funcs)
    }

    fn dyn_value(&mut self, node: &Node, allowed: &[&str], funcs: Funcs) -> Option<Dyn> {
        Some(match &node.kind {
            NodeKind::Scalar(..) => match node.scalar() {
                Some(Scalar::Null) | None => Dyn::Null(node.pos),
                Some(Scalar::Bool(b)) => Dyn::Bool(b, node.pos),
                Some(Scalar::Number(n)) => Dyn::Number(n, node.pos),
                Some(Scalar::Str(s)) => {
                    let s = s.to_owned();
                    Dyn::Str(
                        self.template_from_str(&s, node.pos, allowed, funcs)?,
                        node.pos,
                    )
                }
            },
            NodeKind::Seq(items) => {
                let mut out = Vec::new();
                for i in items {
                    out.push(self.dyn_value(i, allowed, funcs)?);
                }
                Dyn::Seq(out, node.pos)
            }
            NodeKind::Map(entries) => {
                let mut out = Vec::new();
                for (k, v) in entries {
                    let key = k.as_str().unwrap_or_default().to_owned();
                    out.push((key, self.dyn_value(v, allowed, funcs)?));
                }
                Dyn::Map(out, node.pos)
            }
        })
    }

    /// A scalar `Dyn` of the given kind, or an expression string.
    fn dyn_scalar(&mut self, node: &Node, what: &str, want: &str, allowed: &[&str]) -> Option<Dyn> {
        let d = self.dyn_value(node, allowed, Funcs::General)?;
        let ok = match (&d, want) {
            (Dyn::Str(t, _), _) if !t.is_literal() => true,
            (Dyn::Bool(..), "boolean") | (Dyn::Number(..), "number") => true,
            (Dyn::Str(..) | Dyn::Number(..) | Dyn::Bool(..), "string") => true,
            _ => false,
        };
        if !ok {
            self.err(
                node.pos,
                format!(
                    "'{what}' must be a {want} or an expression, found a {}",
                    node.type_name()
                ),
            );
            return None;
        }
        Some(d)
    }

    fn condition(&mut self, node: &Node, allowed: &[&str], funcs: Funcs) -> Option<Condition> {
        let text = match node.scalar() {
            Some(Scalar::Bool(b)) => b.to_string(),
            Some(Scalar::Number(n)) => format_number(n),
            Some(Scalar::Str(s)) => s.to_owned(),
            Some(Scalar::Null) => String::new(),
            None => {
                self.err(node.pos, "'if' must be a string expression");
                return None;
            }
        };
        match Condition::parse(&text) {
            Ok(c) => {
                let sources: Vec<(String, Expr)> = match &c.expr {
                    crate::expr::CondExpr::Expr(e) => vec![(text.trim().to_owned(), e.clone())],
                    crate::expr::CondExpr::Template(t, _) => t
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            crate::expr::Part::Expr { source, expr } => {
                                Some((source.clone(), expr.clone()))
                            }
                            crate::expr::Part::Text(_) => None,
                        })
                        .collect(),
                };
                let refs: Vec<(&str, &Expr)> =
                    sources.iter().map(|(s, e)| (s.as_str(), e)).collect();
                self.check_exprs(node.pos, &refs, allowed, funcs);
                Some(c)
            }
            Err(e) => {
                self.err(node.pos, format!("invalid expression in 'if': {e}"));
                None
            }
        }
    }

    fn env(&mut self, node: &Node, allowed: &[&str], funcs: Funcs) -> EnvMap {
        let mut out = Vec::new();
        let Some(entries) = self.map(node, "env") else {
            return out;
        };
        for (k, v) in entries {
            let name = k.as_str().unwrap_or_default().to_owned();
            if v.as_map().is_some() || v.as_seq().is_some() {
                self.err(
                    v.pos,
                    format!("env '{name}' must be a string, number or boolean"),
                );
                continue;
            }
            if let Some(d) = self.dyn_value(v, allowed, funcs) {
                out.push((name, d));
            }
        }
        out
    }

    fn workflow(&mut self, root: &Node) -> Option<Workflow> {
        let Some(entries) = root.as_map() else {
            self.err(
                root.pos,
                "a workflow must be a mapping with 'on' and 'jobs'",
            );
            return None;
        };
        self.check_keys(
            entries,
            &[
                "name",
                "run-name",
                "on",
                "permissions",
                "env",
                "defaults",
                "concurrency",
                "jobs",
            ],
        );
        let name = root.get("name").and_then(|n| self.string(n, "name"));
        let run_name = root
            .get("run-name")
            .and_then(|n| self.template(n, "run-name", C_RUN_NAME, Funcs::General));
        let on = match root.get("on") {
            Some(n) => self.on(n),
            None => {
                self.err(root.pos, "Required property is missing: on");
                Vec::new()
            }
        };
        let permissions = root.get("permissions").and_then(|n| self.permissions(n));
        let env = root
            .get("env")
            .map(|n| self.env(n, C_WF_ENV, Funcs::General))
            .unwrap_or_default();
        let defaults = root
            .get("defaults")
            .map(|n| self.defaults(n, C_RUN_NAME))
            .unwrap_or_default();
        let concurrency = root
            .get("concurrency")
            .and_then(|n| self.concurrency(n, C_RUN_NAME));
        let jobs = match root.get("jobs") {
            Some(n) => self.jobs(n),
            None => {
                self.err(root.pos, "Required property is missing: jobs");
                Vec::new()
            }
        };
        let is_call = on.iter().any(|t| t.name == "workflow_call");
        if is_call {
            // Outputs may only reference jobs that exist.
            for t in on.iter().filter(|t| t.name == "workflow_call") {
                for o in &t.outputs {
                    for e in o.value.exprs() {
                        for job in referenced_jobs(e, "jobs") {
                            if !jobs.iter().any(|j| j.id.eq_ignore_ascii_case(&job)) {
                                self.err(
                                    t.pos,
                                    format!("output '{}' references unknown job '{job}'", o.id),
                                );
                            }
                        }
                    }
                }
            }
        }
        Some(Workflow {
            path: self.path.to_owned(),
            name,
            run_name,
            on,
            permissions,
            env,
            defaults,
            concurrency,
            jobs,
        })
    }

    fn on(&mut self, node: &Node) -> Vec<EventTrigger> {
        let mut out: Vec<EventTrigger> = Vec::new();
        let mut add = |p: &mut Self, name: &str, pos: Pos, cfg: Option<&Node>| {
            if out.iter().any(|t| t.name == name) {
                p.err(pos, format!("event '{name}' is listed more than once"));
                return;
            }
            if let Some(t) = p.event(name, pos, cfg) {
                out.push(t);
            }
        };
        match &node.kind {
            NodeKind::Scalar(..) => match self.string(node, "on") {
                Some(s) if !s.is_empty() => add(self, &s, node.pos, None),
                _ => self.err(node.pos, "No event triggers defined in 'on'"),
            },
            NodeKind::Seq(items) => {
                if items.is_empty() {
                    self.err(node.pos, "No event triggers defined in 'on'");
                }
                for i in items {
                    if let Some(s) = i.as_str() {
                        add(self, s, i.pos, None);
                    } else {
                        self.err(i.pos, "event names in 'on' must be strings");
                    }
                }
            }
            NodeKind::Map(entries) => {
                if entries.is_empty() {
                    self.err(node.pos, "No event triggers defined in 'on'");
                }
                for (k, v) in entries {
                    let name = k.as_str().unwrap_or_default().to_owned();
                    add(self, &name, k.pos, if v.is_null() { None } else { Some(v) });
                }
            }
        }
        out
    }

    fn filter(&mut self, cfg: &Node, include: &str, ignore: &str) -> Option<Filter> {
        let inc = cfg.get(include);
        let ign = cfg.get(ignore);
        match (inc, ign) {
            (Some(_), Some(i)) => {
                self.err(
                    i.pos,
                    format!("'{include}' and '{ignore}' cannot both be used for the same event"),
                );
                None
            }
            (Some(n), None) => self
                .string_list(n, include)
                .map(|l| Filter::Include(PatternList::new(&l))),
            (None, Some(n)) => self
                .string_list(n, ignore)
                .map(|l| Filter::Ignore(PatternList::new(&l))),
            (None, None) => None,
        }
    }

    fn event(&mut self, name: &str, pos: Pos, cfg: Option<&Node>) -> Option<EventTrigger> {
        let Some((_, types)) = EVENTS.iter().find(|(n, _)| *n == name) else {
            self.err(pos, format!("Unexpected value '{name}'"));
            return None;
        };
        let mut t = EventTrigger::bare(name, pos);
        let Some(cfg) = cfg else {
            if name == "schedule" {
                self.err(pos, "'schedule' needs at least one 'cron' entry");
            }
            return Some(t);
        };
        if name == "schedule" {
            let Some(items) = cfg.as_seq() else {
                self.err(cfg.pos, "'schedule' must be a sequence of 'cron' entries");
                return None;
            };
            for item in items {
                let Some(entries) = self.map(item, "schedule") else {
                    continue;
                };
                self.check_keys(entries, &["cron", "timezone"]);
                if item.get("timezone").is_some() {
                    self.err(
                        item.pos,
                        "'timezone' is not supported; cron schedules are in UTC",
                    );
                }
                match item
                    .get("cron")
                    .and_then(|c| self.string(c, "cron").map(|s| (s, c.pos)))
                {
                    Some((s, p)) => match Cron::parse(&s) {
                        Ok(c) => t.schedules.push(c),
                        Err(e) => self.err(p, e.0),
                    },
                    None => self.err(item.pos, "Required property is missing: cron"),
                }
            }
            if t.schedules.is_empty() && items.is_empty() {
                self.err(cfg.pos, "'schedule' needs at least one 'cron' entry");
            }
            return Some(t);
        }
        let entries = self.map(cfg, name)?;
        let mut allowed: Vec<&str> = Vec::new();
        if types.is_some() {
            allowed.push("types");
        }
        match name {
            "push" => allowed.extend([
                "branches",
                "branches-ignore",
                "tags",
                "tags-ignore",
                "paths",
                "paths-ignore",
            ]),
            "pull_request" | "pull_request_target" => {
                allowed.extend(["branches", "branches-ignore", "paths", "paths-ignore"])
            }
            "merge_group" => allowed.extend(["branches", "branches-ignore"]),
            "workflow_run" => allowed.extend(["workflows", "branches", "branches-ignore"]),
            "workflow_dispatch" => allowed.push("inputs"),
            "workflow_call" => allowed.extend(["inputs", "outputs", "secrets"]),
            _ => {}
        }
        self.check_keys(entries, &allowed);
        if let (Some(known), Some(n)) = (types, cfg.get("types"))
            && let Some(list) = self.string_list(n, "types")
        {
            if !known.is_empty() {
                for ty in &list {
                    if !known.contains(&ty.as_str()) {
                        self.err(
                            n.pos,
                            format!("Unexpected value '{ty}' in 'types' for '{name}'"),
                        );
                    }
                }
            }
            t.types = Some(list);
        }
        if allowed.contains(&"branches") {
            t.branches = self.filter(cfg, "branches", "branches-ignore");
        }
        if allowed.contains(&"tags") {
            t.tags = self.filter(cfg, "tags", "tags-ignore");
        }
        if allowed.contains(&"paths") {
            t.paths = self.filter(cfg, "paths", "paths-ignore");
        }
        if let Some(n) = cfg.get("workflows")
            && let Some(l) = self.string_list(n, "workflows")
        {
            t.workflows = Some(PatternList::new(&l));
        }
        if name == "workflow_run" && cfg.get("workflows").is_none() {
            self.err(cfg.pos, "Required property is missing: workflows");
        }
        if let Some(n) = cfg.get("inputs") {
            t.inputs = self.inputs(n, name == "workflow_call");
        }
        if let Some(n) = cfg.get("outputs") {
            t.outputs = self.call_outputs(n);
        }
        if let Some(n) = cfg.get("secrets") {
            t.secrets = self.call_secrets(n);
        }
        Some(t)
    }

    fn inputs(&mut self, node: &Node, call: bool) -> Vec<InputDecl> {
        let mut out = Vec::new();
        let Some(entries) = self.map(node, "inputs") else {
            return out;
        };
        if !call && entries.len() > MAX_DISPATCH_INPUTS {
            self.err(
                node.pos,
                format!(
                    "workflow_dispatch accepts at most {MAX_DISPATCH_INPUTS} inputs, found {}",
                    entries.len()
                ),
            );
        }
        for (k, v) in entries {
            let id = k.as_str().unwrap_or_default().to_owned();
            if !is_valid_id(&id) {
                self.err(k.pos, format!("invalid input id '{id}'"));
            }
            let cfg_entries: &[(Node, Node)] = if v.is_null() {
                &[]
            } else {
                match self.map(v, &id) {
                    Some(e) => e,
                    None => continue,
                }
            };
            self.check_keys(
                cfg_entries,
                &[
                    "description",
                    "required",
                    "default",
                    "type",
                    "options",
                    "deprecationMessage",
                ],
            );
            let description = v
                .get("description")
                .and_then(|n| self.string(n, "description"));
            let required = v
                .get("required")
                .and_then(|n| self.bool(n, "required"))
                .unwrap_or(false);
            let kind = match v.get("type") {
                None if call => {
                    self.err(
                        k.pos,
                        format!("Required property is missing: type (input '{id}')"),
                    );
                    InputType::String
                }
                None => InputType::String,
                Some(n) => match (self.string(n, "type").as_deref(), call) {
                    (Some("string"), _) => InputType::String,
                    (Some("boolean"), _) => InputType::Boolean,
                    (Some("number"), _) => InputType::Number,
                    (Some("choice"), false) => InputType::Choice,
                    (Some("environment"), false) => InputType::Environment,
                    (other, _) => {
                        self.err(
                            n.pos,
                            format!(
                                "Unexpected value '{}' for input type",
                                other.unwrap_or_default()
                            ),
                        );
                        InputType::String
                    }
                },
            };
            let options = match v.get("options") {
                Some(n) if kind == InputType::Choice => {
                    self.string_list(n, "options").unwrap_or_default()
                }
                Some(n) => {
                    self.err(n.pos, "'options' is only allowed for 'choice' inputs");
                    Vec::new()
                }
                None => Vec::new(),
            };
            if kind == InputType::Choice && options.is_empty() {
                self.err(k.pos, format!("choice input '{id}' needs 'options'"));
            }
            let default = v.get("default").and_then(|n| {
                if call {
                    self.dyn_value(n, C_CALL_DEFAULT, Funcs::General)
                } else {
                    // Dispatch defaults are literal.
                    self.dyn_value(n, &[], Funcs::General)
                }
            });
            if let Some(d) = &default {
                let ok = match (kind, d) {
                    (_, Dyn::Str(t, _)) if !t.is_literal() => true,
                    (InputType::Boolean, Dyn::Bool(..)) => true,
                    (InputType::Number, Dyn::Number(..)) => true,
                    (
                        InputType::String | InputType::Environment,
                        Dyn::Str(..) | Dyn::Number(..) | Dyn::Bool(..),
                    ) => true,
                    (InputType::Choice, Dyn::Str(..) | Dyn::Number(..) | Dyn::Bool(..)) => true,
                    (_, Dyn::Null(_)) => true,
                    _ => false,
                };
                if !ok {
                    self.err(
                        d.pos(),
                        format!("default of input '{id}' must be a {}", kind.name()),
                    );
                }
                if kind == InputType::Choice {
                    let text = match d {
                        Dyn::Str(t, _) => t.render(),
                        Dyn::Number(n, _) => format_number(*n),
                        Dyn::Bool(b, _) => b.to_string(),
                        _ => String::new(),
                    };
                    if !text.is_empty() && !options.contains(&text) {
                        self.err(
                            d.pos(),
                            format!("default '{text}' of input '{id}' is not one of its options"),
                        );
                    }
                }
            }
            out.push(InputDecl {
                id,
                description,
                required,
                kind,
                default,
                options,
                pos: k.pos,
            });
        }
        out
    }

    fn call_outputs(&mut self, node: &Node) -> Vec<OutputDecl> {
        let mut out = Vec::new();
        let Some(entries) = self.map(node, "outputs") else {
            return out;
        };
        for (k, v) in entries {
            let id = k.as_str().unwrap_or_default().to_owned();
            let Some(e) = self.map(v, &id) else { continue };
            self.check_keys(e, &["description", "value"]);
            let description = v
                .get("description")
                .and_then(|n| self.string(n, "description"));
            let Some(value) = v.get("value") else {
                self.err(
                    k.pos,
                    format!("Required property is missing: value (output '{id}')"),
                );
                continue;
            };
            if let Some(value) = self.template(value, "value", C_CALL_OUTPUT, Funcs::General) {
                out.push(OutputDecl {
                    id,
                    description,
                    value,
                });
            }
        }
        out
    }

    fn call_secrets(&mut self, node: &Node) -> Vec<SecretDecl> {
        let mut out = Vec::new();
        let Some(entries) = self.map(node, "secrets") else {
            return out;
        };
        for (k, v) in entries {
            let id = k.as_str().unwrap_or_default().to_owned();
            if !is_valid_id(&id) {
                self.err(k.pos, format!("invalid secret id '{id}'"));
            }
            let mut decl = SecretDecl {
                id,
                description: None,
                required: false,
            };
            if !v.is_null()
                && let Some(e) = self.map(v, &decl.id.clone())
            {
                self.check_keys(e, &["description", "required"]);
                decl.description = v
                    .get("description")
                    .and_then(|n| self.string(n, "description"));
                decl.required = v
                    .get("required")
                    .and_then(|n| self.bool(n, "required"))
                    .unwrap_or(false);
            }
            out.push(decl);
        }
        out
    }

    fn permissions(&mut self, node: &Node) -> Option<Permissions> {
        match &node.kind {
            NodeKind::Scalar(..) => match self.string(node, "permissions").as_deref() {
                Some("read-all") => Some(Permissions::ReadAll),
                Some("write-all") => Some(Permissions::WriteAll),
                Some(other) => {
                    self.err(
                        node.pos,
                        format!("Unexpected value '{other}' for 'permissions'"),
                    );
                    None
                }
                None => None,
            },
            NodeKind::Map(entries) => {
                let mut scopes = Vec::new();
                for (k, v) in entries {
                    let scope = k.as_str().unwrap_or_default().to_owned();
                    if !PERMISSION_SCOPES.contains(&scope.as_str()) {
                        self.err(k.pos, format!("Unexpected value '{scope}'"));
                        continue;
                    }
                    let access = match self.string(v, &scope).as_deref() {
                        Some("read") => Access::Read,
                        Some("write") => Access::Write,
                        Some("none") => Access::None,
                        other => {
                            self.err(
                                v.pos,
                                format!(
                                    "Unexpected value '{}' for permission '{scope}'",
                                    other.unwrap_or_default()
                                ),
                            );
                            continue;
                        }
                    };
                    if (scope == "id-token" && access == Access::Read)
                        || (scope == "models" && access == Access::Write)
                    {
                        self.err(
                            v.pos,
                            format!("'{}' is not a valid level for '{scope}'", access.name()),
                        );
                        continue;
                    }
                    scopes.push((scope, access));
                }
                Some(Permissions::Scopes(scopes))
            }
            NodeKind::Seq(_) => {
                self.err(
                    node.pos,
                    "'permissions' must be 'read-all', 'write-all' or a mapping",
                );
                None
            }
        }
    }

    fn defaults(&mut self, node: &Node, allowed: &[&str]) -> RunDefaults {
        let mut out = RunDefaults::default();
        let Some(e) = self.map(node, "defaults") else {
            return out;
        };
        self.check_keys(e, &["run"]);
        if let Some(run) = node.get("run")
            && let Some(e) = self.map(run, "run")
        {
            self.check_keys(e, &["shell", "working-directory"]);
            out.shell = run
                .get("shell")
                .and_then(|n| self.dyn_scalar(n, "shell", "string", allowed));
            out.working_directory = run
                .get("working-directory")
                .and_then(|n| self.dyn_scalar(n, "working-directory", "string", allowed));
        }
        out
    }

    fn concurrency(&mut self, node: &Node, allowed: &[&str]) -> Option<Concurrency> {
        match &node.kind {
            NodeKind::Scalar(..) => {
                let group = self.template(node, "concurrency", allowed, Funcs::General)?;
                Some(Concurrency {
                    group,
                    cancel_in_progress: Dyn::Bool(false, node.pos),
                    queue_max: false,
                    pos: node.pos,
                })
            }
            NodeKind::Map(e) => {
                self.check_keys(e, &["group", "cancel-in-progress", "queue"]);
                let Some(g) = node.get("group") else {
                    self.err(node.pos, "Required property is missing: group");
                    return None;
                };
                let group = self.template(g, "group", allowed, Funcs::General)?;
                let cancel = match node.get("cancel-in-progress") {
                    Some(n) => self.dyn_scalar(n, "cancel-in-progress", "boolean", allowed)?,
                    None => Dyn::Bool(false, node.pos),
                };
                let queue_max = match node.get("queue") {
                    None => false,
                    Some(q) => match self.string(q, "queue").as_deref() {
                        Some("single") => false,
                        Some("max") => true,
                        other => {
                            self.err(
                                q.pos,
                                format!(
                                    "Unexpected value '{}' for 'queue'",
                                    other.unwrap_or_default()
                                ),
                            );
                            false
                        }
                    },
                };
                if queue_max && !matches!(cancel, Dyn::Bool(false, _)) {
                    self.err(
                        node.pos,
                        "'queue: max' cannot be combined with 'cancel-in-progress: true'",
                    );
                }
                Some(Concurrency {
                    group,
                    cancel_in_progress: cancel,
                    queue_max,
                    pos: node.pos,
                })
            }
            NodeKind::Seq(_) => {
                self.err(node.pos, "'concurrency' must be a string or a mapping");
                None
            }
        }
    }

    fn jobs(&mut self, node: &Node) -> Vec<Job> {
        let mut jobs = Vec::new();
        let Some(entries) = self.map(node, "jobs") else {
            return jobs;
        };
        if entries.is_empty() {
            self.err(node.pos, "'jobs' must define at least one job");
        }
        for (k, v) in entries {
            let id = k.as_str().unwrap_or_default().to_owned();
            if !is_valid_id(&id) {
                self.err(
                    k.pos,
                    format!("The identifier '{id}' is invalid. IDs may only contain alphanumeric characters, '_', and '-'. IDs must start with a letter or '_'"),
                );
            }
            if jobs.iter().any(|j: &Job| j.id.eq_ignore_ascii_case(&id)) {
                self.err(
                    k.pos,
                    format!("job id '{id}' is already defined (ids are case-insensitive)"),
                );
            }
            if let Some(job) = self.job(&id, k.pos, v) {
                jobs.push(job);
            }
        }
        // needs: known jobs, no self-reference, no cycles.
        for j in &jobs {
            for n in &j.needs {
                if n.eq_ignore_ascii_case(&j.id) {
                    self.err(j.pos, format!("job '{}' cannot depend on itself", j.id));
                } else if !jobs.iter().any(|o| o.id.eq_ignore_ascii_case(n)) {
                    self.err(
                        j.pos,
                        format!("Job '{}' depends on unknown job '{n}'.", j.id),
                    );
                }
            }
        }
        if let Err(cycle) = crate::graph::topo_order(&jobs) {
            let pos = jobs
                .iter()
                .find(|j| j.id == cycle[0])
                .map_or(node.pos, |j| j.pos);
            self.err(
                pos,
                format!(
                    "The workflow must not contain a dependency cycle: {}",
                    cycle.join(" -> ")
                ),
            );
        }
        // needs.<id> in expressions must name a direct dependency.
        for j in &jobs {
            let mut exprs: Vec<&Expr> = Vec::new();
            if let Some(c) = &j.condition {
                exprs.extend(c.exprs());
            }
            collect_job_exprs(j, &mut exprs);
            for e in exprs {
                for n in referenced_jobs(e, "needs") {
                    if !j.needs.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                        self.err(
                            j.pos,
                            format!(
                                "Job '{}' references 'needs.{n}' but does not list '{n}' in needs",
                                j.id
                            ),
                        );
                    }
                }
            }
        }
        jobs
    }

    fn job(&mut self, id: &str, pos: Pos, node: &Node) -> Option<Job> {
        let entries = self.map(node, id)?;
        let is_call = node.get("uses").is_some();
        if is_call {
            self.check_keys(
                entries,
                &[
                    "name",
                    "uses",
                    "with",
                    "secrets",
                    "needs",
                    "if",
                    "permissions",
                    "strategy",
                    "concurrency",
                ],
            );
        } else {
            self.check_keys(
                entries,
                &[
                    "name",
                    "needs",
                    "permissions",
                    "if",
                    "runs-on",
                    "environment",
                    "concurrency",
                    "outputs",
                    "env",
                    "defaults",
                    "steps",
                    "timeout-minutes",
                    "strategy",
                    "continue-on-error",
                    "container",
                    "services",
                ],
            );
        }
        let name = node
            .get("name")
            .and_then(|n| self.template(n, "name", C_JOB_PRE, Funcs::General));
        let needs = match node.get("needs") {
            Some(n) => self.string_list(n, "needs").unwrap_or_default(),
            None => Vec::new(),
        };
        let condition = node
            .get("if")
            .and_then(|n| self.condition(n, C_JOB_IF, Funcs::JobIf));
        let permissions = node.get("permissions").and_then(|n| self.permissions(n));
        let strategy = node.get("strategy").and_then(|n| self.strategy(n));
        let concurrency = node
            .get("concurrency")
            .and_then(|n| self.concurrency(n, C_JOB_PRE));
        let kind = if is_call {
            JobKind::Call(Box::new(self.call_job(node)?))
        } else {
            JobKind::Steps(Box::new(self.steps_job(id, pos, node)?))
        };
        Some(Job {
            id: id.to_owned(),
            name,
            needs,
            condition,
            permissions,
            strategy,
            concurrency,
            kind,
            pos,
        })
    }

    fn strategy(&mut self, node: &Node) -> Option<Strategy> {
        let e = self.map(node, "strategy")?;
        self.check_keys(e, &["matrix", "fail-fast", "max-parallel"]);
        let matrix = match node.get("matrix") {
            None => None,
            Some(m) => match &m.kind {
                NodeKind::Map(entries) => {
                    for (k, v) in entries {
                        let key = k.as_str().unwrap_or_default();
                        let is_expr = v.as_str().is_some_and(|s| s.contains("${{"));
                        match key {
                            "include" | "exclude" => {
                                let ok = is_expr
                                    || v.as_seq().is_some_and(|items| {
                                        items.iter().all(|i| i.as_map().is_some())
                                    });
                                if !ok {
                                    self.err(
                                        v.pos,
                                        format!("matrix '{key}' must be a sequence of mappings"),
                                    );
                                }
                            }
                            _ => {
                                if v.as_seq().is_none() && !is_expr {
                                    self.err(
                                        v.pos,
                                        format!("matrix vector '{key}' must be a sequence"),
                                    );
                                }
                            }
                        }
                    }
                    if entries.is_empty() {
                        self.err(m.pos, "matrix must define at least one vector");
                    }
                    self.dyn_value(m, C_JOB_IF, Funcs::General)
                }
                NodeKind::Scalar(..) if m.as_str().is_some_and(|s| s.contains("${{")) => {
                    self.dyn_value(m, C_JOB_IF, Funcs::General)
                }
                _ => {
                    self.err(m.pos, "'matrix' must be a mapping or an expression");
                    None
                }
            },
        };
        let fail_fast = node
            .get("fail-fast")
            .and_then(|n| self.dyn_scalar(n, "fail-fast", "boolean", C_JOB_IF));
        let max_parallel = node
            .get("max-parallel")
            .and_then(|n| self.dyn_scalar(n, "max-parallel", "number", C_JOB_IF));
        Some(Strategy {
            matrix,
            fail_fast,
            max_parallel,
            pos: node.pos,
        })
    }

    fn call_job(&mut self, node: &Node) -> Option<CallJob> {
        let u = node.get("uses")?;
        let text = self.string(u, "uses")?;
        let uses = match parse_workflow_ref(&text) {
            Ok(r) => r,
            Err(m) => {
                self.err(u.pos, m);
                return None;
            }
        };
        let mut with = Vec::new();
        if let Some(w) = node.get("with")
            && let Some(entries) = self.map(w, "with")
        {
            for (k, v) in entries {
                if v.as_map().is_some() || v.as_seq().is_some() {
                    self.err(v.pos, "'with' values must be strings, numbers or booleans");
                    continue;
                }
                if let Some(d) = self.dyn_value(v, C_JOB_PRE, Funcs::General) {
                    with.push((k.as_str().unwrap_or_default().to_owned(), d));
                }
            }
        }
        let secrets = match node.get("secrets") {
            None => CallSecrets::None,
            Some(s) if s.as_str() == Some("inherit") => CallSecrets::Inherit,
            Some(s) => {
                let mut out = Vec::new();
                if let Some(entries) = self.map(s, "secrets") {
                    for (k, v) in entries {
                        if let Some(d) = self.dyn_value(v, C_CALL_SECRETS, Funcs::General) {
                            out.push((k.as_str().unwrap_or_default().to_owned(), d));
                        }
                    }
                }
                CallSecrets::Map(out)
            }
        };
        Some(CallJob {
            uses,
            with,
            secrets,
        })
    }

    fn container(&mut self, node: &Node, what: &str, service: bool) -> Option<Container> {
        if node.as_str().is_some() {
            if service {
                // Services must be mappings with an image.
                self.err(
                    node.pos,
                    format!("service '{what}' must be a mapping with 'image'"),
                );
                return None;
            }
            let image = self.dyn_value(node, C_JOB_PRE, Funcs::General)?;
            return Some(Container {
                image,
                credentials: None,
                env: Vec::new(),
                ports: Vec::new(),
                volumes: Vec::new(),
                options: None,
                pos: node.pos,
            });
        }
        let e = self.map(node, what)?;
        self.check_keys(
            e,
            &["image", "credentials", "env", "ports", "volumes", "options"],
        );
        let image = match node.get("image") {
            Some(i) => self.dyn_scalar(i, "image", "string", C_JOB_PRE)?,
            None => {
                self.err(node.pos, "Required property is missing: image");
                return None;
            }
        };
        let credentials = match node.get("credentials") {
            Some(c) => {
                if let Some(ce) = self.map(c, "credentials") {
                    self.check_keys(ce, &["username", "password"]);
                }
                self.dyn_value(c, C_CREDENTIALS, Funcs::General)
            }
            None => None,
        };
        let env = node
            .get("env")
            .map(|n| self.env(n, C_CONTAINER_ENV, Funcs::General))
            .unwrap_or_default();
        let list = |p: &mut Self, key: &str| -> Vec<Dyn> {
            match node.get(key) {
                Some(n) => match n.as_seq() {
                    Some(items) => items
                        .iter()
                        .filter_map(|i| p.dyn_value(i, C_JOB_PRE, Funcs::General))
                        .collect(),
                    None => {
                        p.err(n.pos, format!("'{key}' must be a sequence"));
                        Vec::new()
                    }
                },
                None => Vec::new(),
            }
        };
        let ports = list(self, "ports");
        let volumes = list(self, "volumes");
        let options = node
            .get("options")
            .and_then(|n| self.dyn_scalar(n, "options", "string", C_JOB_PRE));
        Some(Container {
            image,
            credentials,
            env,
            ports,
            volumes,
            options,
            pos: node.pos,
        })
    }

    fn steps_job(&mut self, id: &str, pos: Pos, node: &Node) -> Option<StepsJob> {
        let runs_on = match node.get("runs-on") {
            Some(r) => {
                match &r.kind {
                    NodeKind::Map(e) => {
                        self.check_keys(e, &["group", "labels"]);
                        if r.get("group").is_none() && r.get("labels").is_none() {
                            self.err(r.pos, "'runs-on' needs 'group' or 'labels'");
                        }
                    }
                    NodeKind::Seq(items) => {
                        for i in items {
                            if i.as_str().is_none() {
                                self.err(i.pos, "'runs-on' labels must be strings");
                            }
                        }
                    }
                    NodeKind::Scalar(..) => {}
                }
                self.dyn_value(r, C_JOB_PRE, Funcs::General)?
            }
            None => {
                self.err(
                    pos,
                    format!("Required property is missing: runs-on (job '{id}')"),
                );
                Dyn::Null(pos)
            }
        };
        let environment = match node.get("environment") {
            None => None,
            Some(e) => match &e.kind {
                NodeKind::Map(entries) => {
                    self.check_keys(entries, &["name", "url", "deployment"]);
                    if e.get("name").is_none() {
                        self.err(e.pos, "Required property is missing: name");
                    }
                    let mut out = Vec::new();
                    if let Some(n) = e.get("name") {
                        out.push((
                            "name".to_owned(),
                            self.dyn_scalar(n, "name", "string", C_JOB_PRE)?,
                        ));
                    }
                    if let Some(u) = e.get("url") {
                        out.push((
                            "url".to_owned(),
                            self.dyn_value(u, C_ENV_URL, Funcs::General)?,
                        ));
                    }
                    if let Some(d) = e.get("deployment") {
                        out.push((
                            "deployment".to_owned(),
                            self.dyn_scalar(d, "deployment", "boolean", C_JOB_PRE)?,
                        ));
                    }
                    Some(Dyn::Map(out, e.pos))
                }
                _ => Some(self.dyn_scalar(e, "environment", "string", C_JOB_PRE)?),
            },
        };
        let mut outputs = Vec::new();
        if let Some(o) = node.get("outputs")
            && let Some(entries) = self.map(o, "outputs")
        {
            for (k, v) in entries {
                if let Some(t) = self.template(v, "outputs", C_JOB_OUTPUTS, Funcs::General) {
                    outputs.push((k.as_str().unwrap_or_default().to_owned(), t));
                }
            }
        }
        let env = node
            .get("env")
            .map(|n| self.env(n, C_JOB_ENV, Funcs::General))
            .unwrap_or_default();
        let defaults = node
            .get("defaults")
            .map(|n| self.defaults(n, C_JOB_DEFAULTS))
            .unwrap_or_default();
        let timeout_minutes = node
            .get("timeout-minutes")
            .and_then(|n| self.dyn_scalar(n, "timeout-minutes", "number", C_JOB_PRE));
        let continue_on_error = node
            .get("continue-on-error")
            .and_then(|n| self.dyn_scalar(n, "continue-on-error", "boolean", C_JOB_PRE));
        let container = node
            .get("container")
            .and_then(|n| self.container(n, "container", false));
        let mut services = Vec::new();
        if let Some(s) = node.get("services")
            && let Some(entries) = self.map(s, "services")
        {
            for (k, v) in entries {
                let sid = k.as_str().unwrap_or_default().to_owned();
                if let Some(c) = self.container(v, &sid, true) {
                    services.push((sid, c));
                }
            }
        }
        let steps = match node.get("steps") {
            Some(s) => match s.as_seq() {
                Some(items) => {
                    if items.is_empty() {
                        self.err(s.pos, format!("job '{id}' must have at least one step"));
                    }
                    let mut steps: Vec<Step> = Vec::new();
                    for item in items {
                        if let Some(step) = self.step(item) {
                            if let Some(sid) = &step.id
                                && steps.iter().any(|o| {
                                    o.id.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(sid))
                                })
                            {
                                self.err(step.pos, format!("The identifier '{sid}' may not be used more than once within the same scope."));
                            }
                            steps.push(step);
                        }
                    }
                    steps
                }
                None => {
                    self.err(s.pos, "'steps' must be a sequence");
                    Vec::new()
                }
            },
            None => {
                self.err(
                    pos,
                    format!("Required property is missing: steps (job '{id}')"),
                );
                Vec::new()
            }
        };
        Some(StepsJob {
            runs_on,
            environment,
            outputs,
            env,
            defaults,
            timeout_minutes,
            continue_on_error,
            container,
            services,
            steps,
        })
    }

    fn step(&mut self, node: &Node) -> Option<Step> {
        let e = self.map(node, "steps")?;
        self.check_keys(
            e,
            &[
                "id",
                "if",
                "name",
                "uses",
                "run",
                "shell",
                "with",
                "env",
                "continue-on-error",
                "timeout-minutes",
                "working-directory",
            ],
        );
        let id = match node.get("id") {
            Some(n) => {
                let s = self.string(n, "id")?;
                if !is_valid_id(&s) {
                    self.err(n.pos, format!("The identifier '{s}' is invalid. IDs may only contain alphanumeric characters, '_', and '-'. IDs must start with a letter or '_'"));
                }
                Some(s)
            }
            None => None,
        };
        let name = node
            .get("name")
            .and_then(|n| self.template(n, "name", C_STEP, Funcs::Step));
        let condition = node
            .get("if")
            .and_then(|n| self.condition(n, C_STEP_IF, Funcs::StepIf));
        let env = node
            .get("env")
            .map(|n| self.env(n, C_STEP, Funcs::Step))
            .unwrap_or_default();
        let continue_on_error = node.get("continue-on-error").and_then(|n| {
            let d = self.dyn_value(n, C_STEP, Funcs::Step)?;
            match &d {
                Dyn::Bool(..) => Some(d),
                Dyn::Str(t, _) if !t.is_literal() => Some(d),
                _ => {
                    self.err(
                        n.pos,
                        "'continue-on-error' must be a boolean or an expression",
                    );
                    None
                }
            }
        });
        let timeout_minutes = node.get("timeout-minutes").and_then(|n| {
            let d = self.dyn_value(n, C_STEP, Funcs::Step)?;
            match &d {
                Dyn::Number(..) => Some(d),
                Dyn::Str(t, _) if !t.is_literal() => Some(d),
                _ => {
                    self.err(n.pos, "'timeout-minutes' must be a number or an expression");
                    None
                }
            }
        });
        let action = match (node.get("run"), node.get("uses")) {
            (Some(_), Some(u)) => {
                self.err(u.pos, "a step cannot have both 'uses' and 'run'");
                return None;
            }
            (None, None) => {
                self.err(node.pos, "a step must have 'uses' or 'run'");
                return None;
            }
            (Some(r), None) => {
                {
                    let k = "with";
                    if let Some(n) = node.get(k) {
                        self.err(n.pos, format!("'{k}' is only valid on a 'uses' step"));
                    }
                }
                let script = self.template(r, "run", C_STEP, Funcs::Step)?;
                let shell = node
                    .get("shell")
                    .and_then(|n| self.dyn_scalar(n, "shell", "string", C_STEP));
                let working_directory = node
                    .get("working-directory")
                    .and_then(|n| self.dyn_scalar(n, "working-directory", "string", C_STEP));
                StepAction::Run {
                    script,
                    shell,
                    working_directory,
                }
            }
            (None, Some(u)) => {
                for k in ["shell", "working-directory"] {
                    if let Some(n) = node.get(k) {
                        self.err(n.pos, format!("'{k}' is only valid on a 'run' step"));
                    }
                }
                let text = self.string(u, "uses")?;
                let action = match parse_action_ref(&text) {
                    Ok(a) => a,
                    Err(m) => {
                        self.err(u.pos, m);
                        return None;
                    }
                };
                let mut with = Vec::new();
                if let Some(w) = node.get("with")
                    && let Some(entries) = self.map(w, "with")
                {
                    for (k, v) in entries {
                        if v.as_map().is_some() || v.as_seq().is_some() {
                            self.err(v.pos, "'with' values must be strings, numbers or booleans");
                            continue;
                        }
                        if let Some(d) = self.dyn_value(v, C_STEP, Funcs::Step) {
                            with.push((k.as_str().unwrap_or_default().to_owned(), d));
                        }
                    }
                }
                StepAction::Uses { action, with }
            }
        };
        Some(Step {
            id,
            name,
            condition,
            action,
            env,
            continue_on_error,
            timeout_minutes,
            pos: node.pos,
        })
    }
}

fn collect_job_exprs<'a>(j: &'a Job, out: &mut Vec<&'a Expr>) {
    let push_t = |t: &'a Template, out: &mut Vec<&'a Expr>| out.extend(t.exprs());
    if let Some(n) = &j.name {
        push_t(n, out);
    }
    if let Some(c) = &j.concurrency {
        push_t(&c.group, out);
    }
    fn dyn_exprs<'a>(d: &'a Dyn, out: &mut Vec<&'a Expr>) {
        match d {
            Dyn::Str(t, _) => out.extend(t.exprs()),
            Dyn::Seq(items, _) => items.iter().for_each(|i| dyn_exprs(i, out)),
            Dyn::Map(e, _) => e.iter().for_each(|(_, v)| dyn_exprs(v, out)),
            _ => {}
        }
    }
    if let Some(s) = &j.strategy {
        for d in [&s.matrix, &s.fail_fast, &s.max_parallel]
            .into_iter()
            .flatten()
        {
            dyn_exprs(d, out);
        }
    }
    match &j.kind {
        JobKind::Steps(s) => {
            dyn_exprs(&s.runs_on, out);
            for (_, t) in &s.outputs {
                push_t(t, out);
            }
            for (_, d) in &s.env {
                dyn_exprs(d, out);
            }
            if let Some(e) = &s.environment {
                dyn_exprs(e, out);
            }
            for step in &s.steps {
                if let Some(c) = &step.condition {
                    out.extend(c.exprs());
                }
                if let Some(n) = &step.name {
                    push_t(n, out);
                }
                for (_, d) in &step.env {
                    dyn_exprs(d, out);
                }
                match &step.action {
                    StepAction::Run { script, .. } => push_t(script, out),
                    StepAction::Uses { with, .. } => {
                        with.iter().for_each(|(_, d)| dyn_exprs(d, out))
                    }
                }
            }
        }
        JobKind::Call(c) => {
            for (_, d) in &c.with {
                dyn_exprs(d, out);
            }
            if let CallSecrets::Map(m) = &c.secrets {
                for (_, d) in m {
                    dyn_exprs(d, out);
                }
            }
        }
    }
}

/// If `e` dereferences `<context>.<name>` with a literal name, returns the name.
fn referenced_jobs(e: &Expr, context: &str) -> Vec<String> {
    let mut found = Vec::new();
    fn walk(e: &Expr, context: &str, found: &mut Vec<String>) {
        match e {
            Expr::Property(base, name) => {
                if matches!(base.as_ref(), Expr::Context(c) if c == context) {
                    found.push(name.clone());
                }
                walk(base, context, found);
            }
            Expr::Index(base, idx) => {
                if let (Expr::Context(c), Expr::Literal(crate::value::Value::String(s))) =
                    (base.as_ref(), idx.as_ref())
                    && c == context
                {
                    found.push(s.clone());
                }
                walk(base, context, found);
                walk(idx, context, found);
            }
            Expr::Wildcard(b) | Expr::Not(b) => walk(b, context, found),
            Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                walk(a, context, found);
                walk(b, context, found);
            }
            Expr::Call(_, args) => args.iter().for_each(|a| walk(a, context, found)),
            Expr::Literal(_) | Expr::Context(_) => {}
        }
    }
    walk(e, context, &mut found);
    found
}

/// Parses `steps[*].uses`.
pub fn parse_action_ref(text: &str) -> Result<ActionRef, String> {
    if text.contains("${{") {
        return Err(format!("expressions are not allowed in 'uses': '{text}'"));
    }
    if let Some(image) = text.strip_prefix("docker://") {
        if image.is_empty() {
            return Err("'uses: docker://' needs an image".to_owned());
        }
        return Ok(ActionRef::Docker(image.to_owned()));
    }
    if text.starts_with("./") || text == "." {
        return Ok(ActionRef::Local(text.to_owned()));
    }
    let Some((repo_path, git_ref)) = text.rsplit_once('@') else {
        return Err(format!(
            "Expected format {{org}}/{{repo}}[/path]@ref. Actual '{text}'"
        ));
    };
    let mut parts = repo_path.splitn(3, '/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    let path = parts.next().filter(|p| !p.is_empty()).map(str::to_owned);
    if owner.is_empty() || repo.is_empty() || git_ref.is_empty() {
        return Err(format!(
            "Expected format {{org}}/{{repo}}[/path]@ref. Actual '{text}'"
        ));
    }
    Ok(ActionRef::Repo {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path,
        git_ref: git_ref.to_owned(),
    })
}

fn is_workflow_file(path: &str) -> bool {
    path.strip_prefix(".github/workflows/").is_some_and(|f| {
        !f.contains('/') && (f.ends_with(".yml") || f.ends_with(".yaml")) && f.len() > 4
    })
}

/// Parses `jobs.<id>.uses`.
pub fn parse_workflow_ref(text: &str) -> Result<WorkflowRef, String> {
    if text.contains("${{") {
        return Err(format!("expressions are not allowed in 'uses': '{text}'"));
    }
    if let Some(local) = text.strip_prefix("./") {
        if text.contains('@') {
            return Err(format!(
                "a local reusable workflow cannot have a ref: '{text}'"
            ));
        }
        if !is_workflow_file(local) {
            return Err(format!(
                "a reusable workflow must be a .yml or .yaml file in .github/workflows: '{text}'"
            ));
        }
        return Ok(WorkflowRef::Local(local.to_owned()));
    }
    let Some((repo_path, git_ref)) = text.rsplit_once('@') else {
        return Err(format!(
            "Expected format {{owner}}/{{repo}}/.github/workflows/{{filename}}@{{ref}}. Actual '{text}'"
        ));
    };
    let mut parts = repo_path.splitn(3, '/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || git_ref.is_empty() || !is_workflow_file(path) {
        return Err(format!(
            "Expected format {{owner}}/{{repo}}/.github/workflows/{{filename}}@{{ref}}. Actual '{text}'"
        ));
    }
    Ok(WorkflowRef::Remote {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path: path.to_owned(),
        git_ref: git_ref.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Workflow {
        parse_workflow(".github/workflows/t.yml", src).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn errs(src: &str) -> Vec<Diagnostic> {
        parse_workflow(".github/workflows/t.yml", src)
            .unwrap_err()
            .diagnostics
    }

    fn has(src: &str, needle: &str) -> Diagnostic {
        let d = errs(src);
        d.iter()
            .find(|d| d.message.contains(needle))
            .cloned()
            .unwrap_or_else(|| panic!("no error containing {needle:?} in {d:?}"))
    }

    const JOB: &str = "jobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n";

    #[test]
    fn on_string_list_and_map_forms() {
        let w = ok(&format!("on: push\n{JOB}"));
        assert_eq!(w.on[0].name, "push");
        let w = ok(&format!("on: [push, pull_request]\n{JOB}"));
        assert_eq!(w.on.len(), 2);
        let w = ok(&format!(
            "on:\n  push:\n    branches: [main]\n    tags: ['v*']\n    paths-ignore: ['docs/**']\n  pull_request:\n    types: [opened, labeled]\n  workflow_dispatch:\n{JOB}"
        ));
        assert_eq!(w.on.len(), 3);
        assert!(matches!(w.on[0].branches, Some(Filter::Include(_))));
        assert!(matches!(w.on[0].paths, Some(Filter::Ignore(_))));
        assert_eq!(
            w.on[1].types.as_deref(),
            Some(&["opened".to_owned(), "labeled".to_owned()][..])
        );
        // `on` parses as a key even unquoted, and quoted forms work too.
        ok(&format!("\"on\": push\n{JOB}"));
    }

    #[test]
    fn filter_conflicts_and_unknown_events() {
        let d = has(
            &format!("on:\n  push:\n    branches: [a]\n    branches-ignore: [b]\n{JOB}"),
            "cannot both",
        );
        assert_eq!(d.pos, Pos { line: 4, col: 22 });
        has(
            &format!("on:\n  pull_request:\n    paths: [a]\n    paths-ignore: [b]\n{JOB}"),
            "cannot both",
        );
        has(
            &format!("on:\n  push:\n    tags: [a]\n    tags-ignore: [b]\n{JOB}"),
            "cannot both",
        );
        let d = has(&format!("on: pushh\n{JOB}"), "Unexpected value 'pushh'");
        assert_eq!(d.pos, Pos { line: 1, col: 5 });
        has(
            &format!("on:\n  pull_request:\n    types: [opend]\n{JOB}"),
            "opend",
        );
        has(
            &format!("on:\n  pull_request:\n    tags: [v1]\n{JOB}"),
            "Unexpected value 'tags'",
        );
        has(
            &format!("on:\n  issues:\n    branches: [main]\n{JOB}"),
            "Unexpected value 'branches'",
        );
        has(&format!("on: []\n{JOB}"), "No event triggers");
        has(
            &format!("on:\n  workflow_run:\n    types: [completed]\n{JOB}"),
            "workflows",
        );
        has(JOB, "Required property is missing: on");
        has("on: push\n", "Required property is missing: jobs");
    }

    #[test]
    fn repository_dispatch_types_are_free_form() {
        let w = ok(&format!(
            "on:\n  repository_dispatch:\n    types: [deploy-prod, anything]\n{JOB}"
        ));
        assert_eq!(w.on[0].types.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn schedules() {
        let w = ok(&format!(
            "on:\n  schedule:\n    - cron: '30 5 * * 1,3'\n    - cron: '0 0 * * *'\n{JOB}"
        ));
        assert_eq!(w.on[0].schedules.len(), 2);
        has(
            &format!("on:\n  schedule:\n    - cron: '61 * * * *'\n{JOB}"),
            "minute",
        );
        has(
            &format!("on:\n  schedule:\n    - crn: '1 * * * *'\n{JOB}"),
            "Unexpected value 'crn'",
        );
    }

    #[test]
    fn dispatch_inputs() {
        let w = ok(&format!(
            "on:\n  workflow_dispatch:\n    inputs:\n      level:\n        type: choice\n        options: [info, debug]\n        default: info\n      dry:\n        type: boolean\n        default: false\n      count:\n        type: number\n      env:\n        type: environment\n      name:\n        required: true\n{JOB}"
        ));
        let ins = &w.on[0].inputs;
        assert_eq!(ins.len(), 5);
        assert_eq!(ins[0].kind, InputType::Choice);
        assert_eq!(ins[4].kind, InputType::String);
        assert!(ins[4].required);
        has(
            &format!(
                "on:\n  workflow_dispatch:\n    inputs:\n      l:\n        type: choice\n{JOB}"
            ),
            "needs 'options'",
        );
        has(
            &format!(
                "on:\n  workflow_dispatch:\n    inputs:\n      l:\n        type: choice\n        options: [a]\n        default: b\n{JOB}"
            ),
            "not one of its options",
        );
        has(
            &format!(
                "on:\n  workflow_dispatch:\n    inputs:\n      l:\n        type: boolean\n        default: 'yes'\n{JOB}"
            ),
            "must be a boolean",
        );
        has(
            &format!(
                "on:\n  workflow_dispatch:\n    inputs:\n      l:\n        type: secret\n{JOB}"
            ),
            "Unexpected value 'secret'",
        );
        let many: String = (0..26).map(|i| format!("      i{i}:\n")).collect();
        has(
            &format!("on:\n  workflow_dispatch:\n    inputs:\n{many}{JOB}"),
            "at most 25",
        );
    }

    #[test]
    fn workflow_call_declarations() {
        let w = ok(
            "on:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n        default: ${{ github.ref_name }}\n    secrets:\n      token:\n        required: true\n      other:\n    outputs:\n      digest:\n        value: ${{ jobs.build.outputs.digest }}\njobs:\n  build:\n    runs-on: ubuntu-latest\n    outputs:\n      digest: ${{ steps.b.outputs.digest }}\n    steps:\n      - id: b\n        run: echo\n",
        );
        let t = &w.on[0];
        assert_eq!(t.inputs.len(), 1);
        assert_eq!(t.secrets.len(), 2);
        assert!(t.secrets[0].required);
        assert_eq!(t.outputs[0].id, "digest");
        has(
            &format!("on:\n  workflow_call:\n    inputs:\n      x:\n        default: a\n{JOB}"),
            "missing: type",
        );
        has(
            &format!("on:\n  workflow_call:\n    inputs:\n      x:\n        type: choice\n{JOB}"),
            "Unexpected value 'choice'",
        );
        has(
            &format!(
                "on:\n  workflow_call:\n    outputs:\n      o:\n        value: ${{{{ jobs.nope.outputs.x }}}}\n{JOB}"
            ),
            "unknown job 'nope'",
        );
        has(
            &format!(
                "on:\n  workflow_call:\n    outputs:\n      o:\n        value: ${{{{ steps.x.outputs.y }}}}\n{JOB}"
            ),
            "Unrecognized named-value: 'steps'",
        );
    }

    #[test]
    fn permissions_forms() {
        let w = ok(&format!("on: push\npermissions: read-all\n{JOB}"));
        assert_eq!(w.permissions, Some(Permissions::ReadAll));
        let w = ok(&format!("on: push\npermissions: {{}}\n{JOB}"));
        assert_eq!(w.permissions, Some(Permissions::Scopes(vec![])));
        let w = ok(&format!(
            "on: push\npermissions:\n  contents: read\n  id-token: write\n  pull-requests: write\n{JOB}"
        ));
        assert_eq!(
            w.permissions,
            Some(Permissions::Scopes(vec![
                ("contents".into(), Access::Read),
                ("id-token".into(), Access::Write),
                ("pull-requests".into(), Access::Write)
            ]))
        );
        has(
            &format!("on: push\npermissions:\n  content: read\n{JOB}"),
            "Unexpected value 'content'",
        );
        has(
            &format!("on: push\npermissions:\n  contents: admin\n{JOB}"),
            "admin",
        );
        has(
            &format!("on: push\npermissions:\n  id-token: read\n{JOB}"),
            "not a valid level",
        );
        has(
            &format!("on: push\npermissions: all\n{JOB}"),
            "Unexpected value 'all'",
        );
    }

    #[test]
    fn concurrency_forms() {
        let w = ok(&format!(
            "on: push\nconcurrency: ci-${{{{ github.ref }}}}\n{JOB}"
        ));
        assert_eq!(
            w.concurrency.unwrap().group.render(),
            "ci-${{ github.ref }}"
        );
        let w = ok(&format!(
            "on: push\nconcurrency:\n  group: g\n  cancel-in-progress: ${{{{ github.ref != 'refs/heads/main' }}}}\n{JOB}"
        ));
        assert!(matches!(
            w.concurrency.unwrap().cancel_in_progress,
            Dyn::Str(..)
        ));
        has(
            &format!("on: push\nconcurrency:\n  cancel-in-progress: true\n{JOB}"),
            "missing: group",
        );
        has(
            &format!(
                "on: push\nconcurrency:\n  group: g\n  queue: max\n  cancel-in-progress: true\n{JOB}"
            ),
            "queue: max",
        );
        // Workflow-level concurrency cannot see matrix or secrets.
        has(
            &format!("on: push\nconcurrency: ${{{{ matrix.os }}}}\n{JOB}"),
            "Unrecognized named-value: 'matrix'",
        );
    }

    #[test]
    fn job_structure_errors() {
        has(
            "on: push\njobs:\n  a:\n    steps:\n      - run: x\n",
            "missing: runs-on",
        );
        has("on: push\njobs:\n  a:\n    runs-on: x\n", "missing: steps");
        has(
            "on: push\njobs:\n  1a:\n    runs-on: x\n    steps: [{run: x}]\n",
            "identifier '1a' is invalid",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    step: []\n    steps: [{run: x}]\n",
            "Unexpected value 'step'",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    needs: b\n    steps: [{run: x}]\n",
            "unknown job 'b'",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    needs: a\n    steps: [{run: x}]\n",
            "depend on itself",
        );
        let d = has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    needs: c\n    steps: [{run: x}]\n  b:\n    runs-on: x\n    needs: a\n    steps: [{run: x}]\n  c:\n    runs-on: x\n    needs: b\n    steps: [{run: x}]\n",
            "dependency cycle",
        );
        assert!(d.message.contains("a -> b -> c -> a") || d.message.contains("->"));
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n  A:\n    runs-on: x\n    steps: [{run: x}]\n",
            "already defined",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n  b:\n    runs-on: x\n    if: needs.a.result == 'success'\n    steps: [{run: x}]\n",
            "does not list 'a'",
        );
    }

    #[test]
    fn step_errors() {
        let base = "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n";
        has(
            &format!("{base}      - uses: actions/checkout@v4\n        run: x\n"),
            "both",
        );
        has(
            &format!("{base}      - name: nothing\n"),
            "must have 'uses' or 'run'",
        );
        has(
            &format!("{base}      - uses: actions/checkout\n"),
            "Expected format",
        );
        has(
            &format!("{base}      - uses: actions/checkout@${{{{ env.V }}}}\n"),
            "not allowed",
        );
        has(
            &format!("{base}      - run: x\n        with: {{a: 1}}\n"),
            "only valid on a 'uses'",
        );
        has(
            &format!("{base}      - uses: a/b@v1\n        shell: bash\n"),
            "only valid on a 'run'",
        );
        has(
            &format!("{base}      - id: s\n        run: x\n      - id: s\n        run: y\n"),
            "more than once",
        );
        has(
            &format!("{base}      - id: 'bad id'\n        run: x\n"),
            "is invalid",
        );
        has(
            &format!("{base}      - run: x\n        if: secrets.TOKEN != ''\n"),
            "Unrecognized named-value: 'secrets'",
        );
        has(
            &format!("{base}      - run: x\n        timeout-minutes: soon\n"),
            "must be a number",
        );
        has(
            &format!("{base}      - run: ${{{{ success() }}}}\n"),
            "Unrecognized function: 'success'",
        );
    }

    #[test]
    fn context_availability_is_enforced() {
        // hashFiles only in steps; matrix not in job if; status functions only in if.
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    env:\n      K: ${{ hashFiles('x') }}\n    steps: [{run: x}]\n",
            "Unrecognized function: 'hashFiles'",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: x\n    if: matrix.os == 'x'\n    steps: [{run: x}]\n",
            "Unrecognized named-value: 'matrix'",
        );
        has(
            "on: push\njobs:\n  a:\n    runs-on: ${{ steps.x.outputs.y }}\n    steps: [{run: x}]\n",
            "Unrecognized named-value: 'steps'",
        );
        has(
            "on: push\nenv:\n  A: ${{ env.B }}\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n",
            "Unrecognized named-value: 'env'",
        );
        ok(
            "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo ${{ hashFiles('**/Cargo.lock') }}\n        if: always() && hashFiles('x') != ''\n",
        );
    }

    #[test]
    fn reusable_workflow_jobs() {
        let w = ok(
            "on: push\njobs:\n  call:\n    uses: ./.github/workflows/build.yml\n    with:\n      target: x\n    secrets: inherit\n  remote:\n    uses: octo/shared/.github/workflows/ci.yaml@v1\n    secrets:\n      token: ${{ secrets.T }}\n",
        );
        match &w.jobs[0].kind {
            JobKind::Call(c) => {
                assert_eq!(
                    c.uses,
                    WorkflowRef::Local(".github/workflows/build.yml".into())
                );
                assert_eq!(c.secrets, CallSecrets::Inherit);
            }
            _ => panic!(),
        }
        match &w.jobs[1].kind {
            JobKind::Call(c) => {
                assert_eq!(c.uses.render(), "octo/shared/.github/workflows/ci.yaml@v1")
            }
            _ => panic!(),
        }
        has(
            "on: push\njobs:\n  c:\n    uses: ./build.yml\n",
            "must be a .yml",
        );
        has(
            "on: push\njobs:\n  c:\n    uses: o/r/.github/workflows/x.yml\n",
            "Expected format",
        );
        has(
            "on: push\njobs:\n  c:\n    uses: ./.github/workflows/x.yml\n    runs-on: x\n",
            "Unexpected value 'runs-on'",
        );
    }

    #[test]
    fn matrix_shapes() {
        let base = "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: x}]\n    strategy:\n";
        ok(&format!(
            "{base}      matrix:\n        os: [a, b]\n        include:\n          - os: c\n"
        ));
        ok(
            &format!("{base}      matrix: ${{{{ fromJSON(needs.a.outputs.m) }}}}\n")
                .replace("needs.a", "github.event"),
        );
        has(
            &format!("{base}      matrix:\n        os: a\n"),
            "must be a sequence",
        );
        has(
            &format!("{base}      matrix:\n        include: [a]\n"),
            "sequence of mappings",
        );
        has(
            &format!("{base}      matrix: []\n"),
            "mapping or an expression",
        );
        has(&format!("{base}      fail-fast: maybe\n"), "boolean");
    }

    #[test]
    fn positions_point_at_the_offending_node() {
        let src = "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo\n        bogus: 1\n";
        let d = has(src, "Unexpected value 'bogus'");
        assert_eq!(d.pos, Pos { line: 7, col: 9 });
        let e = parse_workflow("w.yml", "on: push\njobs:\n  a:\n    runs-on: [\n").unwrap_err();
        assert_eq!(e.diagnostics[0].pos.line, 5);
        assert!(e.to_bee_error().message.starts_with("w.yml:5:"));
    }

    #[test]
    fn all_errors_are_reported_together() {
        let d = errs(
            "on: pushh\njobs:\n  a:\n    runs-on: x\n    steps:\n      - bogus: 1\n        run: x\n",
        );
        assert!(d.len() >= 2);
        assert!(d.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn action_refs() {
        assert_eq!(
            parse_action_ref("docker://alpine:3").unwrap(),
            ActionRef::Docker("alpine:3".into())
        );
        assert_eq!(
            parse_action_ref("./.github/actions/x").unwrap(),
            ActionRef::Local("./.github/actions/x".into())
        );
        assert_eq!(
            parse_action_ref("github/codeql-action/init@v3").unwrap(),
            ActionRef::Repo {
                owner: "github".into(),
                repo: "codeql-action".into(),
                path: Some("init".into()),
                git_ref: "v3".into()
            }
        );
        assert!(parse_action_ref("owner@v1").is_err());
    }
}
