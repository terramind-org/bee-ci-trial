//! The tool catalog: one MCP tool per `bee.v1` RPC, grouped into toolsets.
//!
//! # Naming
//!
//! A tool is named `<service>_<method>` in snake_case, with the `Service` suffix dropped:
//! `QueryService/Log` is `query_log`, `WriteService/Land` is `write_land`,
//! `AuthService/WhoAmI` is `auth_who_am_i`, `IssueService/CreateIssue` is
//! `issue_create_issue`. [`tool_name`] is the rule.
//!
//! # Schemas
//!
//! `inputSchema` is the request message's self-contained JSON Schema from
//! [`bee_proto::schema::Catalog::message_schema`] (the same documents as
//! `schemas/v1/messages/`), so arguments are the proto3-JSON request. Server-streaming tools
//! add two arguments that are not sent to the server: `max_items` (stop after that many
//! messages) and `timeout_ms` (stop after that long). The client-streaming `UploadObjects`
//! takes `{"messages": [<UploadItem>, ...]}`.
//!
//! `outputSchema` (listed only when [`crate::McpConfig::output_schemas`] is on) is the
//! response message's schema for unary tools, and
//! `{"items": [<message>...], "count", "truncated", "truncatedReason"?}` for streaming ones.
//! Results always have that shape, listed or not.
//!
//! # Toolsets
//!
//! With 1,000+ RPCs, listing every tool would bury an agent. Tools are grouped into
//! [`TOOLSETS`] by service; a session lists the toolsets it has enabled (default
//! [`DEFAULT_TOOLSET`]) and can find and enable more at run time with the `search_tools`,
//! `list_toolsets` and `enable_toolset` meta-tools (see [`crate::server`]). A service that no
//! named toolset claims is in `other`, and `all` names every tool, so nothing is unreachable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use bee_core::Ulid;
use bee_proto::methods::{METHODS, MethodDescriptor, MethodKind};
use bee_proto::schema::Catalog;
use prost::Message as _;
use prost_types::FileDescriptorSet;
use serde_json::{Map, Value, json};

/// The toolset a session starts with unless configured otherwise.
pub const DEFAULT_TOOLSET: &str = "core";

/// Toolset name that selects every tool.
pub const ALL_TOOLSET: &str = "all";

/// Toolset name of services no entry in [`TOOLSETS`] claims.
pub const OTHER_TOOLSET: &str = "other";

/// Default number of messages a streaming tool collects.
pub const DEFAULT_MAX_ITEMS: u64 = 100;

/// Default collection time of a streaming tool, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// A named group of tools.
#[derive(Clone, Copy, Debug)]
pub struct Toolset {
    /// Name used in config and in `enable_toolset`.
    pub name: &'static str,
    /// One line on what the toolset is for.
    pub description: &'static str,
    /// Services whose every RPC is in the set, by short name (`Query` for `QueryService`).
    pub services: &'static [&'static str],
    /// Single RPCs in the set, by path (`/bee.v1.AuthService/WhoAmI`).
    pub methods: &'static [&'static str],
}

/// Every named toolset. A service may appear in several.
pub const TOOLSETS: &[Toolset] = &[
    Toolset {
        name: "core",
        description: "Default set: objects, history queries, writes and lands, refs, change \
                      requests and their briefs and evidence, checks, leases, events, issues, \
                      repos, and auth WhoAmI.",
        services: &[
            "Object", "Query", "Write", "Ref", "Change", "Brief", "Check", "Lease", "Event",
            "Issue", "Repo",
        ],
        methods: &["/bee.v1.AuthService/WhoAmI"],
    },
    Toolset {
        name: "vcs",
        description: "Core version control: objects, history queries, writes, lands and refs.",
        services: &["Object", "Query", "Write", "Ref"],
        methods: &[],
    },
    Toolset {
        name: "git",
        description: "Git gateway: git objects, refs, fetch planning, commit ingest, LFS.",
        services: &["Git"],
        methods: &[],
    },
    Toolset {
        name: "changes",
        description: "Change requests, reviews, threads, merge queue, comments, and briefs with \
                      their evidence.",
        services: &["Change", "Comment", "Brief"],
        methods: &[],
    },
    Toolset {
        name: "checks",
        description: "Check runs, suites, commit statuses and post-land bisect.",
        services: &["Check"],
        methods: &[],
    },
    Toolset {
        name: "ci",
        description: "Workflows, runs, jobs, logs, runners and deployments.",
        services: &["Check", "Workflow", "Runner", "Deployment"],
        methods: &[],
    },
    Toolset {
        name: "leases",
        description: "Path and ref leases for coordinating agents.",
        services: &["Lease"],
        methods: &[],
    },
    Toolset {
        name: "events",
        description: "Event subscriptions and log, webhooks, notifications.",
        services: &["Event", "Notification"],
        methods: &[],
    },
    Toolset {
        name: "issues",
        description: "Issues, projects and discussions.",
        services: &["Issue", "Project", "Discussion"],
        methods: &[],
    },
    Toolset {
        name: "repos",
        description: "Repos and their content surfaces: releases, wiki, gists, insights, \
                      packages and pages.",
        services: &[
            "Repo", "Release", "Wiki", "Gist", "Insights", "Package", "Pages",
        ],
        methods: &[],
    },
    Toolset {
        name: "search",
        description: "Code, symbol, issue, repo and commit search; code navigation.",
        services: &["Search"],
        methods: &[],
    },
    Toolset {
        name: "security",
        description: "Security alerts and scanning, secrets and variables, policy and rulesets.",
        services: &["Security", "Secret", "Policy"],
        methods: &[],
    },
    Toolset {
        name: "auth",
        description: "Login, tokens, agents' credentials, access checks and user accounts.",
        services: &["Auth", "User"],
        methods: &[],
    },
    Toolset {
        name: "orgs",
        description: "Organizations, teams, enterprise, apps and billing.",
        services: &["Org", "Team", "Enterprise", "App", "Billing"],
        methods: &[],
    },
    Toolset {
        name: "admin",
        description: "Redaction, GC, backups, audit log, operations and moderation.",
        services: &["Admin", "Audit", "Ops", "Moderation"],
        methods: &[],
    },
    Toolset {
        name: "import",
        description: "Imports from GitHub and other forges, and exports.",
        services: &["Import"],
        methods: &[],
    },
    Toolset {
        name: "agents",
        description: "Agent registry, runs, memory, workspaces and the model gateway.",
        services: &["Agent", "Workspace", "Model"],
        methods: &[],
    },
    Toolset {
        name: "community",
        description: "Sponsors and discussions.",
        services: &["Sponsor", "Discussion"],
        methods: &[],
    },
];

/// `QueryService` -> `Query`; `bee.v1.QueryService` -> `Query`.
pub fn service_short_name(service: &str) -> &str {
    let last = service.rsplit('.').next().unwrap_or(service);
    last.strip_suffix("Service").unwrap_or(last)
}

/// snake_case of a CamelCase identifier; acronyms stay together (`WhoAmI` -> `who_am_i`,
/// `GetSBOMExport` -> `get_sbom_export`).
pub fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                None => false,
                Some(p) if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                Some(p) if p.is_ascii_uppercase() => next.is_some_and(|n| n.is_ascii_lowercase()),
                Some(_) => false,
            };
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The tool name of an RPC: `<service>_<method>` in snake_case without `Service`.
pub fn tool_name(method: &MethodDescriptor) -> String {
    format!(
        "{}_{}",
        snake_case(service_short_name(method.service)),
        snake_case(method.method)
    )
}

/// One RPC as an MCP tool.
#[derive(Debug)]
pub struct ToolDef {
    /// Tool name, see [`tool_name`].
    pub name: String,
    /// The RPC it calls.
    pub method: &'static MethodDescriptor,
    /// Human title, `Service.Method`.
    pub title: String,
    /// Description: the proto comment, then the RPC's shape and messages.
    pub description: String,
    /// Names of the toolsets that include it (never empty).
    pub toolsets: Vec<&'static str>,
    /// Heuristic: the RPC only reads.
    pub read_only: bool,
    /// Heuristic: the RPC deletes or irreversibly changes state.
    pub destructive: bool,
    input_schema: OnceLock<Value>,
    output_schema: OnceLock<Value>,
}

impl ToolDef {
    /// The first line of the description.
    pub fn summary(&self) -> &str {
        self.description.lines().next().unwrap_or_default()
    }

    /// True for server-streaming and bidi RPCs, whose tools collect a stream.
    pub fn collects_stream(&self) -> bool {
        matches!(
            self.method.kind,
            MethodKind::ServerStreaming | MethodKind::BidiStreaming
        )
    }

    /// The tool's `inputSchema`, built on first use.
    pub fn input_schema(&self) -> &Value {
        self.input_schema
            .get_or_init(|| build_input_schema(self.method))
    }

    /// The tool's `outputSchema`, built on first use.
    pub fn output_schema(&self) -> &Value {
        self.output_schema
            .get_or_init(|| build_output_schema(self.method))
    }

    /// The MCP `Tool` object. `structured` adds `title` and `outputSchema` (protocol
    /// 2025-06-18 and later); `output_schemas` turns `outputSchema` off to keep lists small.
    pub fn to_json(&self, structured: bool, output_schemas: bool) -> Value {
        let mut tool = Map::new();
        tool.insert("name".into(), json!(self.name));
        if structured {
            tool.insert("title".into(), json!(self.title));
        }
        tool.insert("description".into(), json!(self.description));
        tool.insert("inputSchema".into(), self.input_schema().clone());
        if structured && output_schemas {
            tool.insert("outputSchema".into(), self.output_schema().clone());
        }
        let mut annotations = Map::new();
        if structured {
            annotations.insert("title".into(), json!(self.title));
        }
        annotations.insert("readOnlyHint".into(), json!(self.read_only));
        if !self.read_only {
            annotations.insert("destructiveHint".into(), json!(self.destructive));
        }
        annotations.insert("openWorldHint".into(), json!(false));
        tool.insert("annotations".into(), Value::Object(annotations));
        Value::Object(tool)
    }
}

/// Every RPC tool, sorted by name.
#[derive(Debug)]
pub struct ToolCatalog {
    tools: Vec<ToolDef>,
    by_name: BTreeMap<String, usize>,
}

/// One match of [`ToolCatalog::search`].
#[derive(Clone, Copy, Debug)]
pub struct SearchHit<'a> {
    /// The tool.
    pub tool: &'a ToolDef,
    /// Relevance; higher is better.
    pub score: u32,
}

impl ToolCatalog {
    /// The process-wide catalog, built on first use from the embedded descriptors.
    pub fn global() -> &'static Self {
        static CATALOG: OnceLock<ToolCatalog> = OnceLock::new();
        CATALOG.get_or_init(Self::build)
    }

    fn build() -> Self {
        let comments = method_comments();
        let mut tools: Vec<ToolDef> = METHODS
            .iter()
            .map(|method| {
                let comment = comments
                    .get(method.path)
                    .map(String::as_str)
                    .unwrap_or_default();
                let short = service_short_name(method.service);
                let toolsets = toolsets_of(method);
                ToolDef {
                    name: tool_name(method),
                    method,
                    title: format!("{short}.{}", method.method),
                    description: describe(method, comment),
                    toolsets,
                    read_only: is_read_only(method.method),
                    destructive: is_destructive(method.method),
                    input_schema: OnceLock::new(),
                    output_schema: OnceLock::new(),
                }
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let by_name = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        Self { tools, by_name }
    }

    /// Every tool, sorted by name.
    pub fn tools(&self) -> &[ToolDef] {
        &self.tools
    }

    /// The tool with this name.
    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.by_name.get(name).map(|&i| &self.tools[i])
    }

    /// Every toolset name a session may enable: [`TOOLSETS`], `other` when some service is
    /// unclaimed, and `all`.
    pub fn toolset_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = TOOLSETS.iter().map(|t| t.name).collect();
        if self
            .tools
            .iter()
            .any(|t| t.toolsets.contains(&OTHER_TOOLSET))
        {
            names.push(OTHER_TOOLSET);
        }
        names.push(ALL_TOOLSET);
        names
    }

    /// True if `name` is a toolset this catalog knows.
    pub fn is_toolset(&self, name: &str) -> bool {
        self.toolset_names().contains(&name)
    }

    /// Description of a toolset name.
    pub fn toolset_description(&self, name: &str) -> &'static str {
        match name {
            ALL_TOOLSET => "Every bee RPC.",
            OTHER_TOOLSET => "Services no other toolset covers.",
            _ => TOOLSETS
                .iter()
                .find(|t| t.name == name)
                .map_or("", |t| t.description),
        }
    }

    /// Tools in a toolset, in name order.
    pub fn toolset_tools<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a ToolDef> + 'a {
        self.tools
            .iter()
            .filter(move |t| name == ALL_TOOLSET || t.toolsets.contains(&name))
    }

    /// Tools matching a free-text query, best first. Words match tool names, services,
    /// methods and descriptions; a trailing plural `s` is ignored. At most `limit` hits.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit<'_>> {
        let words: Vec<String> = words(query).map(|w| stem(&w)).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<SearchHit<'_>> = self
            .tools
            .iter()
            .filter_map(|tool| {
                let name_parts: Vec<String> = tool.name.split('_').map(stem).collect();
                let description = tool.description.to_ascii_lowercase();
                let mut score = 0;
                let mut matched = 0;
                for word in &words {
                    let mut s = 0;
                    if name_parts.iter().any(|p| p == word) {
                        s += 6;
                    } else if tool.name.contains(word.as_str()) {
                        s += 3;
                    }
                    if description.contains(word.as_str()) {
                        s += 1;
                    }
                    if s > 0 {
                        matched += 1;
                    }
                    score += s;
                }
                // Every word that matched adds a bonus so multi-word queries prefer tools
                // matching all of them.
                (score > 0).then_some(SearchHit {
                    tool,
                    score: score + matched * matched * 2,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.tool.name.len().cmp(&b.tool.name.len()))
                .then_with(|| a.tool.name.cmp(&b.tool.name))
        });
        hits.truncate(limit);
        hits
    }
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .flat_map(|w| {
            // CamelCase words in a query ("CreateIssue") split like tool names do.
            snake_case(w)
                .split('_')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|w| !w.is_empty())
}

fn stem(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    match lower.strip_suffix('s') {
        Some(base) if base.len() >= 3 && !base.ends_with('s') => base.to_owned(),
        _ => lower,
    }
}

fn toolsets_of(method: &MethodDescriptor) -> Vec<&'static str> {
    let short = service_short_name(method.service);
    let mut sets: Vec<&'static str> = TOOLSETS
        .iter()
        .filter(|t| t.services.contains(&short) || t.methods.contains(&method.path))
        .map(|t| t.name)
        .collect();
    let claimed = TOOLSETS
        .iter()
        .any(|t| t.name != DEFAULT_TOOLSET && t.services.contains(&short));
    if !claimed {
        sets.push(OTHER_TOOLSET);
    }
    sets
}

const READ_PREFIXES: &[&str] = &[
    "Get",
    "List",
    "Read",
    "Search",
    "Has",
    "Resolve",
    "Is",
    "Check",
    "Who",
    "Diff",
    "Log",
    "Blame",
    "Compare",
    "Stream",
    "Watch",
    "Tail",
    "Subscribe",
    "Count",
    "Validate",
    "Preview",
    "Query",
    "Find",
    "Lookup",
    "Describe",
    "Verify",
    "Explain",
    "Download",
    "Show",
];

const READ_EXACT: &[&str] = &["MergeBase", "Evaluate", "Ping", "Health"];

const DESTRUCTIVE_PREFIXES: &[&str] = &[
    "Delete", "Remove", "Purge", "Redact", "Revoke", "Destroy", "Erase", "Wipe", "Force", "Reset",
    "Transfer", "Rotate",
];

fn has_prefix_word(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase()))
}

fn is_read_only(method: &str) -> bool {
    READ_EXACT.contains(&method) || READ_PREFIXES.iter().any(|p| has_prefix_word(method, p))
}

fn is_destructive(method: &str) -> bool {
    DESTRUCTIVE_PREFIXES
        .iter()
        .any(|p| has_prefix_word(method, p))
}

fn kind_name(kind: MethodKind) -> &'static str {
    match kind {
        MethodKind::Unary => "unary",
        MethodKind::ServerStreaming => "server-streaming",
        MethodKind::ClientStreaming => "client-streaming",
        MethodKind::BidiStreaming => "bidi-streaming",
    }
}

fn describe(method: &MethodDescriptor, comment: &str) -> String {
    let short = service_short_name(method.service);
    let mut text = if comment.is_empty() {
        format!("{short}.{}.", method.method)
    } else {
        comment.to_owned()
    };
    text.push_str(&format!(
        "\n\n{} -> {} ({}).",
        method.input_type,
        method.output_type,
        kind_name(method.kind)
    ));
    match method.kind {
        MethodKind::ServerStreaming | MethodKind::BidiStreaming => text.push_str(&format!(
            " Collected as {{items, count, truncated, truncatedReason}}: at most `max_items` \
             (default {DEFAULT_MAX_ITEMS}) within `timeout_ms` (default {DEFAULT_TIMEOUT_MS})."
        )),
        MethodKind::ClientStreaming => text.push_str(&format!(
            " Pass {{\"messages\": [<{}>, ...]}}.",
            method.input_type
        )),
        MethodKind::Unary => {}
    }
    text
}

/// Proto comments of every RPC, keyed by path.
fn method_comments() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(set) = FileDescriptorSet::decode(bee_proto::FILE_DESCRIPTOR_SET) else {
        return out;
    };
    for file in &set.file {
        let mut by_path: BTreeMap<&[i32], String> = BTreeMap::new();
        for location in file
            .source_code_info
            .as_ref()
            .map(|i| i.location.as_slice())
            .unwrap_or_default()
        {
            let raw = location
                .leading_comments
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .or(location.trailing_comments.as_deref())
                .unwrap_or_default();
            let text = raw
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_owned();
            if !text.is_empty() {
                by_path.insert(location.path.as_slice(), text);
            }
        }
        for (si, service) in file.service.iter().enumerate() {
            for (mi, method) in service.method.iter().enumerate() {
                let key: [i32; 4] = [6, si as i32, 2, mi as i32];
                if let Some(text) = by_path.get(key.as_slice()) {
                    out.insert(
                        format!("/{}.{}/{}", file.package(), service.name(), method.name()),
                        text.clone(),
                    );
                }
            }
        }
    }
    out
}

/// The process-wide schema catalog.
pub fn schema_catalog() -> &'static Catalog {
    static SCHEMAS: OnceLock<Catalog> = OnceLock::new();
    SCHEMAS.get_or_init(Catalog::load)
}

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// A message schema for `name`, or a permissive object when the catalog lacks it.
fn message_schema(name: &str) -> Value {
    schema_catalog()
        .message_schema(name)
        .unwrap_or_else(|| json!({"$schema": DRAFT_2020_12, "type": "object", "title": name}))
}

const REPO_REF: &str = "bee.v1.RepoRef";
const REPO_REF_STRING_DESCRIPTION: &str =
    "A repo reference object, or a string: `org/name` for its full name or a ULID for its id.";

/// Converts MCP's compact string spelling of every `RepoRef` in an argument into the
/// proto3-JSON object spelling. The schema, rather than field names, identifies RepoRefs so
/// this also covers nested messages and repeated fields.
pub fn normalize_repo_ref_arguments(
    method: &MethodDescriptor,
    arguments: &mut Value,
) -> Result<(), String> {
    let schema = message_schema(method.input_type);
    normalize_repo_refs(arguments, &schema, &schema, "")
}

fn normalize_repo_refs(
    value: &mut Value,
    schema: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    if let Some(target) = resolve_ref(schema, root) {
        return normalize_repo_refs(value, target, root, path);
    }
    if schema.get("title").and_then(Value::as_str) == Some(REPO_REF) {
        if let Value::String(repo) = value {
            let object = if is_full_name(repo) {
                json!({"fullName": repo})
            } else if Ulid::parse(repo).is_ok() {
                json!({"id": repo})
            } else {
                let argument = if path.is_empty() { "argument" } else { path };
                return Err(format!(
                    "`{argument}` must be `org/name` (a repo full name) or a ULID (a repo id)"
                ));
            };
            *value = object;
        }
        return Ok(());
    }
    match (value, schema.get("properties").and_then(Value::as_object)) {
        (Value::Object(values), Some(properties)) => {
            for (name, value) in values {
                if let Some(field_schema) = properties.get(name) {
                    let child_path = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    normalize_repo_refs(value, field_schema, root, &child_path)?;
                }
            }
        }
        (Value::Array(values), _) => {
            if let Some(items) = schema.get("items") {
                for (index, value) in values.iter_mut().enumerate() {
                    normalize_repo_refs(value, items, root, &format!("{path}[{index}]"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_ref<'a>(schema: &'a Value, root: &'a Value) -> Option<&'a Value> {
    let reference = schema.get("$ref")?.as_str()?;
    let name = reference.strip_prefix("#/$defs/")?;
    root.get("$defs")?.get(name)
}

fn is_full_name(value: &str) -> bool {
    let Some((org, name)) = value.split_once('/') else {
        return false;
    };
    !org.is_empty()
        && !name.is_empty()
        && !org.contains('/')
        && !name.contains('/')
        && !value.chars().any(char::is_whitespace)
}

fn add_repo_ref_string_forms(schema: &mut Value) {
    if schema.get("title").and_then(Value::as_str) == Some(REPO_REF) {
        let object = schema.clone();
        *schema = json!({
            "anyOf": [
                object,
                {"type": "string", "description": REPO_REF_STRING_DESCRIPTION}
            ],
            "description": REPO_REF_STRING_DESCRIPTION,
        });
        return;
    }
    match schema {
        Value::Object(values) => {
            for value in values.values_mut() {
                add_repo_ref_string_forms(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                add_repo_ref_string_forms(value);
            }
        }
        _ => {}
    }
}

/// Moves a self-contained message schema under `$defs/<name>` of `defs`, rewriting its
/// self-references (`#`) to point there, so it can sit inside a wrapper schema.
fn hoist(schema: Value, name: &str, defs: &mut Map<String, Value>) {
    let Value::Object(mut root) = schema else {
        return;
    };
    root.remove("$schema");
    root.remove("$id");
    if let Some(Value::Object(inner)) = root.remove("$defs") {
        for (key, value) in inner {
            defs.entry(key).or_insert(value);
        }
    }
    defs.insert(name.to_owned(), Value::Object(root));
    let target = format!("#/$defs/{name}");
    for value in defs.values_mut() {
        rewrite_root_refs(value, &target);
    }
}

fn rewrite_root_refs(value: &mut Value, target: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "$ref" && child.as_str() == Some("#") {
                    *child = json!(target);
                } else {
                    rewrite_root_refs(child, target);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite_root_refs(v, target)),
        _ => {}
    }
}

fn build_input_schema(method: &MethodDescriptor) -> Value {
    let mut schema = match method.kind {
        MethodKind::Unary => message_schema(method.input_type),
        MethodKind::ServerStreaming => {
            let mut schema = message_schema(method.input_type);
            if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                props.insert(
                    "max_items".into(),
                    json!({
                        "type": "integer", "minimum": 1,
                        "description": format!("Not sent to the server: stop collecting after this many messages (default {DEFAULT_MAX_ITEMS}).")
                    }),
                );
                props.insert(
                    "timeout_ms".into(),
                    json!({
                        "type": "integer", "minimum": 1,
                        "description": format!("Not sent to the server: stop collecting after this many milliseconds (default {DEFAULT_TIMEOUT_MS}).")
                    }),
                );
            }
            schema
        }
        MethodKind::ClientStreaming | MethodKind::BidiStreaming => {
            let mut defs = Map::new();
            hoist(
                message_schema(method.input_type),
                method.input_type,
                &mut defs,
            );
            json!({
                "$schema": DRAFT_2020_12,
                "type": "object",
                "title": format!("{} stream", method.input_type),
                "description": format!("The request stream of {}, in order.", method.path.trim_start_matches('/')),
                "required": ["messages"],
                "properties": {
                    "messages": {
                        "type": "array",
                        "items": {"$ref": format!("#/$defs/{}", method.input_type)}
                    }
                },
                "$defs": defs
            })
        }
    };
    add_repo_ref_string_forms(&mut schema);
    schema
}

fn build_output_schema(method: &MethodDescriptor) -> Value {
    match method.kind {
        MethodKind::Unary | MethodKind::ClientStreaming => message_schema(method.output_type),
        MethodKind::ServerStreaming | MethodKind::BidiStreaming => {
            let mut defs = Map::new();
            hoist(
                message_schema(method.output_type),
                method.output_type,
                &mut defs,
            );
            json!({
                "$schema": DRAFT_2020_12,
                "type": "object",
                "title": format!("collected {} stream", method.output_type),
                "required": ["items", "count", "truncated"],
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {"$ref": format!("#/$defs/{}", method.output_type)}
                    },
                    "count": {"type": "integer", "minimum": 0},
                    "truncated": {"type": "boolean", "description": "True when collection stopped before the server ended the stream."},
                    "truncatedReason": {"type": "string", "enum": ["max_items", "timeout"]}
                },
                "$defs": defs
            })
        }
    }
}

/// Names of the toolsets in `requested` that the catalog does not know.
pub fn unknown_toolsets<'a>(requested: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    let catalog = ToolCatalog::global();
    requested
        .into_iter()
        .filter(|name| !catalog.is_toolset(name))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_examples() {
        assert_eq!(snake_case("WhoAmI"), "who_am_i");
        assert_eq!(snake_case("GetSBOMExport"), "get_sbom_export");
        assert_eq!(snake_case("Land"), "land");
        assert_eq!(snake_case("OpenChange"), "open_change");
        assert_eq!(snake_case("GetV2Thing"), "get_v2_thing");
    }

    #[test]
    fn names_follow_the_rule() {
        let catalog = ToolCatalog::global();
        for name in [
            "write_land",
            "query_log",
            "auth_who_am_i",
            "issue_create_issue",
        ] {
            assert!(catalog.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn heuristics() {
        assert!(is_read_only("GetObjects"));
        assert!(is_read_only("MergeBase"));
        assert!(!is_read_only("Merge"));
        assert!(!is_read_only("Issue"));
        assert!(!is_read_only("Isolate"));
        assert!(is_destructive("DeleteRef"));
        assert!(!is_destructive("Deleted"));
    }

    #[test]
    fn hoist_rewrites_self_references() {
        let schema = json!({
            "$schema": DRAFT_2020_12, "$id": "x", "type": "object",
            "properties": {"child": {"$ref": "#"}, "other": {"$ref": "#/$defs/B"}},
            "$defs": {"B": {"type": "object", "properties": {"up": {"$ref": "#"}}}}
        });
        let mut defs = Map::new();
        hoist(schema, "A", &mut defs);
        assert_eq!(defs["A"]["properties"]["child"]["$ref"], "#/$defs/A");
        assert_eq!(defs["A"]["properties"]["other"]["$ref"], "#/$defs/B");
        assert_eq!(defs["B"]["properties"]["up"]["$ref"], "#/$defs/A");
        assert!(defs["A"].get("$id").is_none());
    }
}
