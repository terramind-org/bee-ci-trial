/**
 * The community side of the demo world: the second org (Lattice Labs, an open-source org), the
 * public repos with their histories, topics, profiles, stars and follows. Static facts only;
 * `store.ts` turns them into state and `community.ts` serves them.
 *
 * Repos of orgs other than acme are keyed by full name ("lattice/engine"); acme's keep their
 * short names so the original world is untouched.
 */
import type { CommitSeed, CrSeed, PrincipalSeed, RepoSeed } from "./world";

export const LATTICE = "lattice";

const lat = (name: string) => LATTICE + "/" + name;

// ---------------------------------------------------------------- orgs and people

export interface OrgSeed {
  name: string;
  display: string;
  description: string;
  website: string;
  location: string;
  email: string;
  /** Days before the anchor the org was created. */
  ageDays: number;
}

export const LATTICE_ORG: OrgSeed = {
  name: LATTICE,
  display: "Lattice Labs",
  description: "Open-source tools for agents that write and review code together.",
  website: "https://lattice.example",
  location: "Lisbon, Portugal",
  email: "hello@lattice.example",
  ageDays: 380,
};

export const ACME_ORG_PROFILE = {
  description: "Fleet software for warehouse robots.",
  website: "https://acme.example",
  location: "Rotterdam, Netherlands",
  email: "dev@acme.example",
};

export const COMMUNITY_PRINCIPALS: PrincipalSeed[] = [
  { handle: "rhea", kind: "PRINCIPAL_KIND_HUMAN", display: "Rhea Lindqvist", github: "5120331", role: "ROLE_OWNER", org: LATTICE },
  { handle: "omar", kind: "PRINCIPAL_KIND_HUMAN", display: "Omar Haddad", github: "3098817", role: "ROLE_ADMIN", org: LATTICE },
  { handle: "kit", kind: "PRINCIPAL_KIND_HUMAN", display: "Kit Nakamura", github: "6671204", role: "ROLE_MEMBER", org: LATTICE },
  {
    handle: "patcher",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "patcher",
    owner: "rhea",
    runtime: "claude-code",
    model: "claude-opus-5",
    purpose: "Opens small, well-tested fixes against lattice/engine and lattice/agent-kit.",
  },
  {
    handle: "bench-runner",
    kind: "PRINCIPAL_KIND_AGENT",
    display: "bench-runner",
    owner: "omar",
    runtime: "codex",
    model: "gpt-5-codex",
    purpose: "Runs the nightly benchmark suites and lands the results.",
  },
];

export interface ProfileSeed {
  bio: string;
  company: string;
  location: string;
  website: string;
  pronouns?: string;
  /** Rendered profile README. */
  readmeHtml?: string;
}

/** Profile details by principal handle. Agents fall back to their purpose as the bio. */
export const PROFILES: Record<string, ProfileSeed> = {
  mara: {
    bio: "Engineering lead at Acme. Wants every change reviewed by something that can run the tests.",
    company: "Acme Robotics",
    location: "Rotterdam, Netherlands",
    website: "https://mara.example",
    pronouns: "she/her",
    readmeHtml:
      "<h2>Hi, I am Mara</h2><p>I lead the fleet console team at Acme. Most of my day is reviewing changes written by agents and by people, and tightening the policy that decides which of them may land.</p><ul><li>Currently: session hardening in <code>acme/api</code></li><li>Ask me about: land policy, leases, review queues</li></ul>",
  },
  dev: { bio: "Billing and payments. Owns the invoice routes.", company: "Acme Robotics", location: "Pune, India", website: "" },
  lin: { bio: "Docs, web and the occasional cache.", company: "Acme Robotics", location: "Shanghai, China", website: "https://lin.example" },
  sam: { bio: "Finds the bugs the tests do not.", company: "Acme Robotics", location: "Madrid, Spain", website: "" },
  rhea: {
    bio: "Maintainer of Lattice Engine. I like deterministic things.",
    company: "Lattice Labs",
    location: "Lisbon, Portugal",
    website: "https://rhea.example",
    pronouns: "she/her",
    readmeHtml:
      "<h2>Rhea Lindqvist</h2><p>I maintain <a href=\"/lattice/engine\">lattice/engine</a> and run <a href=\"/patcher\">patcher</a>, an agent that fixes small bugs while I sleep.</p><p>My rule for a good change: it should read the same to the agent that wrote it and the human who reviews it.</p>",
  },
  omar: { bio: "Benchmarks, flame graphs and the occasional rewrite.", company: "Lattice Labs", location: "Amman, Jordan", website: "https://omar.example" },
  kit: { bio: "Writes the docs people actually read.", company: "Lattice Labs", location: "Osaka, Japan", website: "" },
};

// ---------------------------------------------------------------- topics

export interface TopicSeed {
  name: string;
  displayName: string;
  shortDescription: string;
  description: string;
  featured: boolean;
  aliases: string[];
  related: string[];
  released?: string;
  createdBy?: string;
}

export const TOPICS: TopicSeed[] = [
  {
    name: "agents",
    displayName: "Agents",
    shortDescription: "Software that plans and acts on its own, including coding agents.",
    description:
      "Agents take a goal, pick tools and carry it out with little supervision. On Beehive they hold scoped tokens, take leases on the paths they edit and merge through pull requests like anyone else.\n\nProjects here cover agent loops, tool registries, evaluation harnesses and the glue that lets agents work next to people.",
    featured: true,
    aliases: ["ai-agents", "coding-agents"],
    related: ["llm", "devtools", "benchmarks"],
    released: "2024",
    createdBy: "rhea",
  },
  {
    name: "rust",
    displayName: "Rust",
    shortDescription: "A language for reliable, fast systems software.",
    description: "Rust gives memory safety without a garbage collector. Repositories tagged rust range from storage engines to command-line tools.",
    featured: true,
    aliases: ["rustlang", "rust-lang"],
    related: ["cli", "devtools", "vcs"],
    released: "2015",
  },
  {
    name: "typescript",
    displayName: "TypeScript",
    shortDescription: "JavaScript with types.",
    description: "TypeScript adds a static type system to JavaScript. Libraries, SDKs and web apps tagged typescript live here.",
    featured: false,
    aliases: ["ts"],
    related: ["devtools", "docs"],
    released: "2012",
  },
  {
    name: "python",
    displayName: "Python",
    shortDescription: "A readable language for scripting, data and glue code.",
    description: "Python is the default language of benchmarks, notebooks and data tooling.",
    featured: false,
    aliases: ["py"],
    related: ["benchmarks", "llm"],
    released: "1991",
  },
  {
    name: "cli",
    displayName: "Command line",
    shortDescription: "Tools you run from a terminal.",
    description: "Command-line programs: build tools, deploy helpers and anything that reads arguments and prints results.",
    featured: false,
    aliases: ["command-line", "terminal"],
    related: ["rust", "devtools"],
  },
  {
    name: "vcs",
    displayName: "Version control",
    shortDescription: "Tracking changes to code over time.",
    description:
      "Version control records who changed what and why. Topics here include change graphs, rebase and merge algorithms, and systems built for agents that commit far more often than people do.",
    featured: true,
    aliases: ["version-control", "scm"],
    related: ["rust", "devtools", "agents"],
    released: "1972",
  },
  {
    name: "benchmarks",
    displayName: "Benchmarks",
    shortDescription: "Measuring how fast and how well things run.",
    description: "Benchmark suites, harnesses and the reports they produce. Good benchmarks are reproducible: same inputs, same numbers.",
    featured: true,
    aliases: ["benchmarking", "performance"],
    related: ["python", "llm"],
  },
  {
    name: "docs",
    displayName: "Documentation",
    shortDescription: "Guides, references and the tooling that builds them.",
    description: "Documentation sites, generators and style guides.",
    featured: false,
    aliases: ["documentation"],
    related: ["typescript", "devtools"],
  },
  {
    name: "llm",
    displayName: "Large language models",
    shortDescription: "Models that read and write text and code.",
    description: "Clients, evaluation tools and applications built on large language models.",
    featured: true,
    aliases: ["llms", "language-models"],
    related: ["agents", "benchmarks", "python"],
    released: "2018",
  },
  {
    name: "devtools",
    displayName: "Developer tools",
    shortDescription: "Software that makes other software easier to build.",
    description: "Editors, linters, build systems, SDKs and the small utilities in between.",
    featured: true,
    aliases: ["developer-tools", "dev-tools"],
    related: ["cli", "typescript", "rust"],
  },
  {
    name: "infrastructure",
    displayName: "Infrastructure",
    shortDescription: "The machines, networks and config underneath your services.",
    description: "Provisioning, deployment and operations: Terraform modules, cluster tooling and runbooks.",
    featured: false,
    aliases: ["infra", "iac"],
    related: ["cli", "security"],
  },
  {
    name: "security",
    displayName: "Security",
    shortDescription: "Keeping software and data safe.",
    description: "Scanners, policy engines, secret detection and hardening guides.",
    featured: false,
    aliases: ["infosec"],
    related: ["infrastructure", "devtools"],
  },
];

// ---------------------------------------------------------------- repos

export interface CommunityRepo extends RepoSeed {
  /** Who is counted as the people who starred it, besides the named stargazers: how many, and how recent. */
  externalStars: number;
  /** Larger values bunch the stars closer to now. */
  heat: number;
  externalForks: number;
}

export const COMMUNITY_REPOS: CommunityRepo[] = [
  {
    name: "sdk",
    owner: "acme",
    description: "TypeScript client for the Acme fleet API",
    visibility: "VISIBILITY_PUBLIC",
    restricted: [],
    topics: ["typescript", "devtools", "sdk"],
    license: "MIT",
    homepage: "https://acme.example/sdk",
    externalStars: 86,
    heat: 1.3,
    externalForks: 9,
  },
  {
    name: "engine",
    owner: LATTICE,
    description: "Change-graph engine with deterministic three-way rebase",
    visibility: "VISIBILITY_PUBLIC",
    restricted: [],
    topics: ["rust", "vcs", "devtools"],
    license: "Apache-2.0",
    homepage: "https://lattice.example/engine",
    externalStars: 2140,
    heat: 1.6,
    externalForks: 188,
  },
  {
    name: "agent-kit",
    owner: LATTICE,
    description: "Small toolkit for building agent loops with typed tools",
    visibility: "VISIBILITY_PUBLIC",
    restricted: [],
    topics: ["agents", "llm", "typescript", "devtools"],
    license: "MIT",
    homepage: "https://lattice.example/agent-kit",
    externalStars: 960,
    heat: 2.3,
    externalForks: 74,
  },
  {
    name: "docs",
    owner: LATTICE,
    description: "The Lattice documentation site and its build script",
    visibility: "VISIBILITY_PUBLIC",
    restricted: [],
    topics: ["docs", "vcs", "cli"],
    license: "CC-BY-4.0",
    homepage: "https://docs.lattice.example",
    externalStars: 215,
    heat: 1.2,
    externalForks: 21,
  },
  {
    name: "bench",
    owner: LATTICE,
    description: "Reproducible benchmarks for change-graph operations",
    visibility: "VISIBILITY_PUBLIC",
    restricted: [],
    topics: ["benchmarks", "python", "llm"],
    license: "Apache-2.0",
    homepage: "",
    externalStars: 340,
    heat: 1.8,
    externalForks: 33,
  },
  {
    name: "internal-tools",
    owner: LATTICE,
    description: "Release and deploy helpers for the Lattice team",
    visibility: "VISIBILITY_PRIVATE",
    restricted: [],
    topics: ["infrastructure", "cli"],
    license: "",
    homepage: "",
    externalStars: 0,
    heat: 1,
    externalForks: 0,
  },
];

/** Topics for the three original acme repos, by repo name. */
export const ACME_TOPICS: Record<string, string[]> = {
  api: ["typescript", "billing"],
  web: ["typescript", "nextjs"],
  infra: ["infrastructure", "terraform"],
};

// ---------------------------------------------------------------- stars and follows

export interface StarSeed {
  /** Login of the stargazer. */
  who: string;
  /** Store key of the repo. */
  repo: string;
  /** Minutes before the anchor. */
  ago: number;
}

export const STARS: StarSeed[] = [
  { who: "mara", repo: lat("engine"), ago: 12_000 },
  { who: "mara", repo: lat("agent-kit"), ago: 800 },
  { who: "mara", repo: "sdk", ago: 200 },
  { who: "mara", repo: "api", ago: 5_000 },
  { who: "dev", repo: lat("engine"), ago: 20_000 },
  { who: "dev", repo: "sdk", ago: 9_000 },
  { who: "dev", repo: lat("bench"), ago: 300 },
  { who: "lin", repo: lat("docs"), ago: 3_000 },
  { who: "lin", repo: lat("agent-kit"), ago: 100 },
  { who: "lin", repo: "sdk", ago: 12_000 },
  { who: "sam", repo: lat("engine"), ago: 40 },
  { who: "sam", repo: "sdk", ago: 6_000 },
  { who: "rhea", repo: "sdk", ago: 1_000 },
  { who: "rhea", repo: lat("bench"), ago: 25_000 },
  { who: "rhea", repo: "api", ago: 10_000 },
  { who: "omar", repo: lat("agent-kit"), ago: 7_000 },
  { who: "omar", repo: lat("docs"), ago: 15_000 },
  { who: "kit", repo: lat("engine"), ago: 30_000 },
  { who: "kit", repo: lat("bench"), ago: 60 },
  { who: "patcher", repo: lat("engine"), ago: 25_000 },
  { who: "bench-runner", repo: lat("bench"), ago: 40_000 },
];

export interface FollowSeed {
  from: string;
  to: string;
  ago: number;
}

export const FOLLOWS: FollowSeed[] = [
  { from: "mara", to: "dev", ago: 20_000 },
  { from: "mara", to: "lin", ago: 20_000 },
  { from: "mara", to: "rhea", ago: 9_000 },
  { from: "mara", to: "patcher", ago: 2_000 },
  { from: "mara", to: LATTICE, ago: 8_000 },
  { from: "dev", to: "mara", ago: 30_000 },
  { from: "dev", to: "lin", ago: 25_000 },
  { from: "dev", to: LATTICE, ago: 7_000 },
  { from: "lin", to: "mara", ago: 30_000 },
  { from: "lin", to: "docs-writer", ago: 12_000 },
  { from: "sam", to: "mara", ago: 22_000 },
  { from: "rhea", to: "omar", ago: 28_000 },
  { from: "rhea", to: "kit", ago: 28_000 },
  { from: "rhea", to: "mara", ago: 4_000 },
  { from: "rhea", to: "bench-runner", ago: 15_000 },
  { from: "omar", to: "rhea", ago: 28_000 },
  { from: "omar", to: "kit", ago: 27_000 },
  { from: "omar", to: "bench-runner", ago: 26_000 },
  { from: "kit", to: "rhea", ago: 27_000 },
  { from: "kit", to: "omar", ago: 26_000 },
  { from: "kit", to: "mara", ago: 90 },
];

// ---------------------------------------------------------------- file contents

const join = (...lines: string[]) => lines.join("\n") + "\n";

const ENGINE_README_V1 = join(
  "# lattice/engine",
  "",
  "The change-graph engine behind Lattice. It stores changesets as a DAG and rebases them with",
  "a three-way merge that gives the same answer on every machine.",
  "",
  "## Quick start",
  "",
  "```sh",
  "cargo add lattice-engine",
  "```",
  "",
  "```rust",
  "let mut graph = Graph::new();",
  "let root = graph.add(&[], \"init\");",
  "```",
);
const ENGINE_README_V2 =
  ENGINE_README_V1 +
  join(
    "",
    "## Rebase invariants",
    "",
    "1. The result depends only on the three input trees, never on iteration order.",
    "2. Conflicts are reported in path order.",
    "3. Rebasing a change onto its own base is a no-op.",
    "",
    "| Operation | Cost |",
    "|---|---|",
    "| `Graph::add` | O(parents) |",
    "| `Graph::ancestors` | O(n) |",
    "| `rebase` | O(changed paths) |",
  );
const ENGINE_CARGO = join('[package]', 'name = "lattice-engine"', 'version = "0.4.0"', 'edition = "2021"', "", "[dependencies]", 'thiserror = "1"');
const ENGINE_LIB_V1 = join("//! Change-graph engine.", "", "pub mod graph;", "", "pub type Id = u64;");
const ENGINE_LIB_V3 = join("//! Change-graph engine.", "", "pub mod graph;", "pub mod rebase;", "", "pub type Id = u64;");
const ENGINE_GRAPH_V1 = join(
  "use crate::Id;",
  "use std::collections::BTreeMap;",
  "",
  "/// A DAG of changesets. Parents are stored in the order they were given.",
  "#[derive(Default)]",
  "pub struct Graph {",
  "    parents: BTreeMap<Id, Vec<Id>>,",
  "    next: Id,",
  "}",
  "",
  "impl Graph {",
  "    pub fn new() -> Self {",
  "        Self::default()",
  "    }",
  "",
  "    pub fn add(&mut self, parents: &[Id], _label: &str) -> Id {",
  "        self.next += 1;",
  "        self.parents.insert(self.next, parents.to_vec());",
  "        self.next",
  "    }",
  "",
  "    pub fn ancestors(&self, id: Id) -> Vec<Id> {",
  "        let mut out = Vec::new();",
  "        for p in self.parents.get(&id).into_iter().flatten() {",
  "            out.push(*p);",
  "            out.extend(self.ancestors(*p));",
  "        }",
  "        out",
  "    }",
  "}",
);
const ENGINE_GRAPH_V2 = ENGINE_GRAPH_V1.replace(
  /    pub fn ancestors[\s\S]*?\n    }\n/,
  join(
    "    pub fn ancestors(&self, id: Id) -> Vec<Id> {",
    "        let mut out = Vec::new();",
    "        let mut stack = vec![id];",
    "        while let Some(next) = stack.pop() {",
    "            for p in self.parents.get(&next).into_iter().flatten() {",
    "                out.push(*p);",
    "                stack.push(*p);",
    "            }",
    "        }",
    "        out",
    "    }",
  ),
);
const ENGINE_GRAPH_V3 = ENGINE_GRAPH_V2.replace(
  "    next: Id,\n}",
  "    next: Id,\n    /// Ancestor lists already computed, by changeset.\n    cache: BTreeMap<Id, Vec<Id>>,\n}",
);
const ENGINE_REBASE_V1 = join(
  "use std::collections::BTreeMap;",
  "",
  "pub type Tree = BTreeMap<String, String>;",
  "",
  "#[derive(Debug, PartialEq)]",
  "pub enum Outcome {",
  "    Clean(Tree),",
  "    Conflicts(Vec<String>),",
  "}",
  "",
  "/// Three-way merge of `ours` and `theirs` over their common `base`.",
  "pub fn rebase(base: &Tree, ours: &Tree, theirs: &Tree) -> Outcome {",
  "    let mut merged = base.clone();",
  "    let mut conflicts = Vec::new();",
  "    for path in ours.keys().chain(theirs.keys()) {",
  "        match (ours.get(path), theirs.get(path), base.get(path)) {",
  "            (Some(a), Some(b), _) if a == b => {",
  "                merged.insert(path.clone(), a.clone());",
  "            }",
  "            (Some(a), _, Some(o)) if a != o && theirs.get(path) == Some(o) => {",
  "                merged.insert(path.clone(), a.clone());",
  "            }",
  "            (_, Some(b), Some(o)) if b != o && ours.get(path) == Some(o) => {",
  "                merged.insert(path.clone(), b.clone());",
  "            }",
  "            _ => conflicts.push(path.clone()),",
  "        }",
  "    }",
  "    if conflicts.is_empty() { Outcome::Clean(merged) } else { Outcome::Conflicts(conflicts) }",
  "}",
);
const ENGINE_REBASE_V2 = ENGINE_REBASE_V1.replace(
  "    if conflicts.is_empty()",
  "    conflicts.sort();\n    conflicts.dedup();\n    if conflicts.is_empty()",
);
const ENGINE_TEST = join(
  "use lattice_engine::rebase::{rebase, Outcome, Tree};",
  "",
  "fn tree(files: &[(&str, &str)]) -> Tree {",
  "    files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect()",
  "}",
  "",
  "#[test]",
  "fn rebase_is_deterministic() {",
  "    let base = tree(&[(\"a\", \"1\")]);",
  "    let ours = tree(&[(\"a\", \"2\")]);",
  "    let theirs = tree(&[(\"a\", \"3\")]);",
  "    let first = rebase(&base, &ours, &theirs);",
  "    for _ in 0..100 {",
  "        assert_eq!(first, rebase(&base, &ours, &theirs));",
  "    }",
  "    assert_eq!(first, Outcome::Conflicts(vec![\"a\".to_string()]));",
  "}",
);

const KIT_README_V1 = join(
  "# lattice/agent-kit",
  "",
  "A small toolkit for agent loops: give the model typed tools, run until it stops asking for them.",
  "",
  "```ts",
  'import { Agent, tool } from "agent-kit";',
  "",
  "const agent = new Agent({ model: \"claude-sonnet-5\", tools: [] });",
  "```",
  "",
  "- Typed tools with runtime validation",
  "- Bounded loops with a step budget",
  "- Usage recorded on every run",
);
const KIT_README_V2 = KIT_README_V1 + join("", "## Registering tools", "", "Call `registry.add(tool)` before the first run. Names must be unique.");
const KIT_PACKAGE = join("{", '  "name": "agent-kit",', '  "version": "0.9.2",', '  "type": "module",', '  "scripts": { "test": "bun test" }', "}");
const KIT_INDEX_V1 = join('export { Agent } from "./agent";');
const KIT_INDEX_V2 = join('export { Agent } from "./agent";', 'export { ToolRegistry, tool } from "./tools";');
const KIT_AGENT_V1 = join(
  "export interface Step {",
  "  tool: string;",
  "  input: unknown;",
  "}",
  "",
  "export class Agent {",
  "  constructor(readonly opts: { model: string; maxSteps?: number }) {}",
  "",
  "  async run(next: () => Promise<Step | null>, call: (s: Step) => Promise<unknown>): Promise<unknown[]> {",
  "    const results: unknown[] = [];",
  "    for (let i = 0; i < (this.opts.maxSteps ?? 20); i++) {",
  "      const step = await next();",
  "      if (!step) break;",
  "      results.push(await call(step));",
  "    }",
  "    return results;",
  "  }",
  "}",
);
const KIT_AGENT_V2 = KIT_AGENT_V1.replace(
  "      results.push(await call(step));",
  join(
    "      results.push(await withRetry(() => call(step)));",
  ).trimEnd(),
) + join("", "async function withRetry<T>(fn: () => Promise<T>, tries = 3): Promise<T> {", "  for (let n = 1; ; n++) {", "    try {", "      return await fn();", "    } catch (err) {", "      if (n >= tries) throw err;", "      await new Promise((r) => setTimeout(r, 2 ** n * 100));", "    }", "  }", "}");
const KIT_AGENT_V3 = KIT_AGENT_V2.replace(
  "export class Agent {",
  join("export interface Usage {", "  steps: number;", "  retries: number;", "}", "", "export class Agent {", "  usage: Usage = { steps: 0, retries: 0 };").trimEnd(),
).replace("      const step = await next();", "      const step = await next();\n      this.usage.steps++;");
const KIT_TOOLS = join(
  "export interface Tool<I = unknown, O = unknown> {",
  "  name: string;",
  "  run(input: I): Promise<O>;",
  "}",
  "",
  "export const tool = <I, O>(name: string, run: (input: I) => Promise<O>): Tool<I, O> => ({ name, run });",
  "",
  "export class ToolRegistry {",
  "  private tools = new Map<string, Tool>();",
  "",
  "  add(t: Tool): this {",
  '    if (this.tools.has(t.name)) throw new Error("duplicate tool " + t.name);',
  "    this.tools.set(t.name, t);",
  "    return this;",
  "  }",
  "",
  "  get(name: string): Tool | undefined {",
  "    return this.tools.get(name);",
  "  }",
  "}",
);
const KIT_TEST = join(
  'import { expect, test } from "bun:test";',
  'import { Agent } from "../src/agent";',
  "",
  'test("stops when the model stops asking for tools", async () => {',
  '  const steps = [{ tool: "echo", input: 1 }, null];',
  '  const agent = new Agent({ model: "test" });',
  "  const out = await agent.run(async () => steps.shift() ?? null, async (s) => s.input);",
  "  expect(out).toEqual([1]);",
  "});",
);

const DOCS_README = join(
  "# lattice/docs",
  "",
  "Source of the Lattice documentation site.",
  "",
  "- `docs/` holds the pages, one Markdown file each",
  "- `site/build.ts` renders them to static HTML",
  "",
  "Run `bun site/build.ts` to build into `dist/`.",
);
const DOCS_START_V1 = join("# Getting started", "", "Install the CLI, then create a repository:", "", "```sh", "lattice init my-project", "```", "", "See [concepts](concepts.md#change-ids) for how changes are identified.");
const DOCS_START_V2 = DOCS_START_V1.replace("concepts.md#change-ids", "concepts.md#change-ids-and-revisions");
const DOCS_CONCEPTS_V1 = join("# Concepts", "", "## Change ids and revisions", "", "A change keeps its id when it is revised. Each revision is a new changeset with a new id.", "", "## Leases", "", "A lease tells others which paths you are editing.");
const DOCS_CONCEPTS_V2 = DOCS_CONCEPTS_V1 + join("", "## Rebase", "", "Rebasing a change replays it onto a new base. The result never depends on the order of files.");
const DOCS_CLI = join("# CLI reference", "", "| Command | What it does |", "|---|---|", "| `lattice init` | Create a repository |", "| `lattice land` | Land the current change |", "| `lattice lease` | Take a lease on paths |");
const DOCS_BUILD = join(
  'import { readdirSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";',
  "",
  'mkdirSync("dist", { recursive: true });',
  'for (const name of readdirSync("docs").filter((n) => n.endsWith(".md"))) {',
  '  const body = readFileSync("docs/" + name, "utf8");',
  '  writeFileSync("dist/" + name.replace(/\\.md$/, ".html"), "<pre>" + body + "</pre>");',
  "}",
);

const BENCH_README = join(
  "# lattice/bench",
  "",
  "Benchmarks for change-graph operations. Every suite is seeded, so two runs on the same machine",
  "produce the same inputs.",
  "",
  "```sh",
  "pip install -e .",
  "python -m bench.runner latency",
  "```",
  "",
  "Results land in `results/latest.json`.",
);
const BENCH_PYPROJECT = join("[project]", 'name = "lattice-bench"', 'version = "0.3.1"', 'requires-python = ">=3.11"');
const BENCH_INIT = join('"""Benchmarks for change-graph operations."""', "", '__version__ = "0.3.1"');
const BENCH_RUNNER_V1 = join(
  "import random",
  "import time",
  "",
  "",
  "def run(suite: str, seed: int = 7, iterations: int = 200) -> list[float]:",
  '    """Time `iterations` runs of a suite, returning seconds per run."""',
  "    rng = random.Random(seed)",
  "    samples = []",
  "    for _ in range(iterations):",
  "        size = rng.randint(100, 1000)",
  "        start = time.perf_counter()",
  "        sum(i * i for i in range(size))",
  "        samples.append(time.perf_counter() - start)",
  "    return samples",
);
const BENCH_REPORT_V1 = join(
  "import statistics",
  "",
  "",
  "def summarize(samples: list[float]) -> dict[str, float]:",
  "    ordered = sorted(samples)",
  '    return {"median": statistics.median(ordered), "max": ordered[-1]}',
);
const BENCH_REPORT_V2 = BENCH_REPORT_V1.replace(
  '    return {"median": statistics.median(ordered), "max": ordered[-1]}',
  join('    p95 = ordered[int(len(ordered) * 0.95) - 1]', '    return {"median": statistics.median(ordered), "p95": p95, "max": ordered[-1]}').trimEnd(),
);
const BENCH_RESULTS_V1 = join("{", '  "suite": "latency",', '  "median": 0.000041,', '  "p95": 0.000066', "}");
const BENCH_RESULTS_V2 = join("{", '  "suite": "latency",', '  "median": 0.000039,', '  "p95": 0.000061', "}");
const BENCH_TEST = join("from bench.report import summarize", "", "", "def test_summarize_reports_median_and_p95():", "    out = summarize([1.0, 2.0, 3.0, 4.0])", '    assert out["median"] == 2.5', '    assert out["p95"] == 4.0');

const TOOLS_README = join("# lattice/internal-tools", "", "Release and deploy helpers. Not for public use.");
const TOOLS_CARGO = join("[package]", 'name = "lattice-tools"', 'version = "0.1.0"', 'edition = "2021"');
const TOOLS_MAIN = join("mod deploy;", "", "fn main() {", '    println!("{}", deploy::plan("staging"));', "}");
const TOOLS_DEPLOY_V1 = join("pub fn plan(env: &str) -> String {", '    format!("deploy to {env}")', "}");
const TOOLS_DEPLOY_V2 = join("pub fn plan(env: &str) -> String {", '    format!("deploy to {env} after the canary passes")', "}");

const SDK_README_V1 = join("# acme/sdk", "", "TypeScript client for the Acme fleet API.", "", "```ts", 'import { Client } from "@acme/sdk";', "", 'const acme = new Client({ token: process.env.ACME_TOKEN! });', "```");
const SDK_README_V2 = SDK_README_V1 + join("", "## Invoices", "", "```ts", 'const invoice = await acme.invoices.get("inv_123");', "```");
const SDK_PACKAGE_V1 = join("{", '  "name": "@acme/sdk",', '  "version": "1.0.0",', '  "devDependencies": { "typescript": "5.5.4" }', "}");
const SDK_PACKAGE_V2 = SDK_PACKAGE_V1.replace("5.5.4", "5.6.2");
const SDK_TYPES = join("export interface Invoice {", "  id: string;", "  total: number;", '  currency: "EUR" | "USD";', "}");
const SDK_CLIENT_V1 = join(
  'import type { Invoice } from "./types";',
  "",
  "export class Client {",
  "  constructor(private opts: { token: string; baseUrl?: string }) {}",
  "",
  "  async request<T>(path: string): Promise<T> {",
  '    const res = await fetch((this.opts.baseUrl ?? "https://api.acme.example") + path, {',
  '      headers: { Authorization: "Bearer " + this.opts.token },',
  "    });",
  '    if (!res.ok) throw new Error("request failed: " + res.status);',
  "    return (await res.json()) as T;",
  "  }",
  "",
  "  invoices = { get: (id: string) => this.request<Invoice>(\"/v1/invoices/\" + id) };",
  "}",
);
const SDK_CLIENT_V2 = SDK_CLIENT_V1.replace(
  '    if (!res.ok) throw new Error("request failed: " + res.status);',
  join(
    "    if (res.status === 429 && attempt < 3) {",
    '      const wait = Number(res.headers.get("Retry-After") ?? "1");',
    "      await new Promise((r) => setTimeout(r, wait * 1000));",
    "      return this.request<T>(path, attempt + 1);",
    "    }",
    '    if (!res.ok) throw new Error("request failed: " + res.status);',
  ).trimEnd(),
).replace("request<T>(path: string)", "request<T>(path: string, attempt = 0)");
const SDK_INDEX = join('export { Client } from "./client";', 'export type { Invoice } from "./types";');

// ---------------------------------------------------------------- history

const PASS = { test: "CHECK_STATUS_PASSED", lint: "CHECK_STATUS_PASSED", e2e: "CHECK_STATUS_PASSED" } as const;

const commit = (repo: string, author: string, ago: number, message: string, files: CommitSeed["files"], costCents?: number): CommitSeed => ({
  repo,
  author,
  ago,
  message,
  files,
  ...(costCents !== undefined ? { session: "ses_" + repo.replace(/\W/g, "") + ago, costCents } : {}),
});

const E = lat("engine");
const K = lat("agent-kit");
const D = lat("docs");
const B = lat("bench");
const T = lat("internal-tools");

export const COMMUNITY_HISTORY: CommitSeed[] = [
  commit("sdk", "dev", 25_000, "Start the SDK", { "README.md": SDK_README_V1, "package.json": SDK_PACKAGE_V1, "src/types.ts": SDK_TYPES, "src/client.ts": SDK_CLIENT_V1, "src/index.ts": SDK_INDEX }),
  commit("sdk", "lin", 14_000, "Document invoice lookups", { "README.md": SDK_README_V2 }),
  commit("sdk", "docs", 6_000, "docs: SDK quickstart", { "README.md": SDK_README_V2.replace("TypeScript client for the Acme fleet API.", "TypeScript client for the Acme fleet API. Works in Node, Bun and browsers.") }, 4),
  commit("sdk", "mara", 1_500, "Retry on 429 using Retry-After", { "src/client.ts": SDK_CLIENT_V2 }),
  commit("sdk", "deps", 300, "Bump typescript from 5.5.4 to 5.6.2", { "package.json": SDK_PACKAGE_V2 }, 3),

  commit(E, "rhea", 40_000, "Start the engine crate", { "README.md": ENGINE_README_V1, "Cargo.toml": ENGINE_CARGO, "src/lib.rs": ENGINE_LIB_V1 }),
  commit(E, "omar", 31_000, "Add the change graph", { "src/graph.rs": ENGINE_GRAPH_V1 }),
  commit(E, "patcher", 20_000, "Add three-way rebase", { "src/rebase.rs": ENGINE_REBASE_V1, "src/lib.rs": ENGINE_LIB_V3 }, 52),
  commit(E, "kit", 9_000, "Document the rebase invariants", { "README.md": ENGINE_README_V2 }),
  commit(E, "rhea", 3_000, "Test that rebase is deterministic", { "tests/rebase.rs": ENGINE_TEST }),
  commit(E, "patcher", 700, "Order conflicts by path", { "src/rebase.rs": ENGINE_REBASE_V2 }, 31),
  commit(E, "omar", 100, "Walk the graph without recursion", { "src/graph.rs": ENGINE_GRAPH_V2 }),

  commit(K, "rhea", 35_000, "Start the agent loop", { "README.md": KIT_README_V1, "package.json": KIT_PACKAGE, "src/agent.ts": KIT_AGENT_V1, "src/index.ts": KIT_INDEX_V1 }),
  commit(K, "kit", 22_000, "Add the tool registry", { "src/tools.ts": KIT_TOOLS, "src/index.ts": KIT_INDEX_V2 }),
  commit(K, "patcher", 12_000, "Retry tool calls with backoff", { "src/agent.ts": KIT_AGENT_V2 }, 44),
  commit(K, "omar", 5_000, "Test that the loop stops", { "test/agent.test.ts": KIT_TEST }),
  commit(K, "patcher", 1_200, "Record usage on every run", { "src/agent.ts": KIT_AGENT_V3 }, 36),
  commit(K, "rhea", 200, "Document tool registration", { "README.md": KIT_README_V2 }),

  commit(D, "kit", 38_000, "Seed the docs site", { "README.md": DOCS_README, "docs/getting-started.md": DOCS_START_V1, "site/build.ts": DOCS_BUILD }),
  commit(D, "rhea", 19_000, "Explain change ids", { "docs/concepts.md": DOCS_CONCEPTS_V1 }),
  commit(D, "patcher", 8_000, "docs: document the CLI", { "docs/cli.md": DOCS_CLI }, 7),
  commit(D, "kit", 2_200, "Fix the anchor in getting started", { "docs/getting-started.md": DOCS_START_V2 }),
  commit(D, "patcher", 400, "docs: add rebase to concepts", { "docs/concepts.md": DOCS_CONCEPTS_V2 }, 5),

  commit(B, "omar", 42_000, "Create the benchmark harness", { "README.md": BENCH_README, "pyproject.toml": BENCH_PYPROJECT, "bench/__init__.py": BENCH_INIT, "bench/runner.py": BENCH_RUNNER_V1 }),
  commit(B, "bench-runner", 26_000, "Add the summary report", { "bench/report.py": BENCH_REPORT_V1 }, 12),
  commit(B, "omar", 9_500, "Report p95 as well as the median", { "bench/report.py": BENCH_REPORT_V2, "tests/test_report.py": BENCH_TEST }),
  commit(B, "bench-runner", 1_300, "Record the nightly latency run", { "results/latest.json": BENCH_RESULTS_V1 }, 9),
  commit(B, "bench-runner", 80, "Record the nightly latency run", { "results/latest.json": BENCH_RESULTS_V2 }, 9),

  commit(T, "rhea", 30_000, "Start the internal tools", { "README.md": TOOLS_README, "Cargo.toml": TOOLS_CARGO, "src/main.rs": TOOLS_MAIN, "src/deploy.rs": TOOLS_DEPLOY_V1 }),
  commit(T, "omar", 15_000, "Wait for the canary before deploying", { "src/deploy.rs": TOOLS_DEPLOY_V2 }),
  commit(T, "kit", 2_000, "Explain the deploy command", { "README.md": TOOLS_README + join("", "Run `cargo run -- staging` to print the deploy plan.") }),
];

export const COMMUNITY_CRS: CrSeed[] = [
  {
    key: "lat-conflict-order",
    repo: E,
    author: "patcher",
    title: "Order conflicts by path",
    body: "Conflicts came back in hash order, so two runs could list them differently. Sort and dedupe before returning.",
    ago: 800,
    state: "CHANGE_STATE_LANDED",
    landedAs: 5,
    ws: "ws/patcher/conflict-order",
    revisions: [{ ago: 800, base: 4, files: { "src/rebase.rs": ENGINE_REBASE_V2 }, contentChanged: true, checks: PASS, session: "ses_lat_conflict", costCents: 31 }],
    reviews: [
      { by: "rhea", rev: 1, verdict: "VERDICT_APPROVE", body: "Deterministic output is the whole point. Merging.", ago: 760 },
      { by: "omar", rev: 1, verdict: "VERDICT_COMMENT", body: "Could use a regression test for three conflicting paths.", ago: 750 },
    ],
    threads: [],
  },
  {
    key: "lat-ancestor-cache",
    repo: E,
    author: "omar",
    title: "Cache ancestor lookups",
    body: "Repeated `ancestors` calls on the same changeset re-walk the graph. Keep a cache keyed by id.",
    ago: 300,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/omar/ancestor-cache",
    revisions: [{ ago: 300, base: 6, files: { "src/graph.rs": ENGINE_GRAPH_V3 }, contentChanged: true, checks: PASS }],
    reviews: [
      { by: "rhea", rev: 1, verdict: "VERDICT_REQUEST_CHANGES", body: "The cache has to be cleared when a changeset is added, or old answers go stale.", ago: 200 },
      { by: "patcher", rev: 1, verdict: "VERDICT_COMMENT", body: "Reproduced the stale answer with a two-node graph.", ago: 180 },
    ],
    threads: [],
  },
  {
    key: "lat-usage",
    repo: K,
    author: "patcher",
    title: "Record usage on every run",
    body: "Counts steps per run so callers can enforce budgets.",
    ago: 1_300,
    state: "CHANGE_STATE_LANDED",
    landedAs: 4,
    ws: "ws/patcher/usage",
    revisions: [{ ago: 1_300, base: 3, files: { "src/agent.ts": KIT_AGENT_V3 }, contentChanged: true, checks: PASS, session: "ses_lat_usage", costCents: 36 }],
    reviews: [{ by: "rhea", rev: 1, verdict: "VERDICT_APPROVE", body: "Good.", ago: 1_250 }],
    threads: [],
  },
  {
    key: "lat-streaming",
    repo: K,
    author: "kit",
    title: "Stream partial tool output",
    body: "Lets callers show progress while a long tool call runs.",
    ago: 500,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/kit/streaming",
    revisions: [{ ago: 500, base: 5, files: { "src/tools.ts": KIT_TOOLS + join("", "export type Progress = (note: string) => void;") }, contentChanged: true, checks: PASS }],
    reviews: [{ by: "omar", rev: 1, verdict: "VERDICT_APPROVE", body: "Looks right; the type is minimal.", ago: 400 }],
    threads: [],
  },
  {
    key: "lat-bench-p99",
    repo: B,
    author: "bench-runner",
    title: "Add p99 to the summary",
    body: "The tail matters more than the median for the latency suite.",
    ago: 600,
    state: "CHANGE_STATE_OPEN",
    ws: "ws/bench-runner/p99",
    revisions: [{ ago: 600, base: 4, files: { "bench/report.py": BENCH_REPORT_V2.replace('"p95": p95,', '"p95": p95, "p99": ordered[int(len(ordered) * 0.99) - 1],') }, contentChanged: true, checks: PASS, session: "ses_lat_p99", costCents: 11 }],
    reviews: [{ by: "omar", rev: 1, verdict: "VERDICT_APPROVE", body: "Fine for the nightly run.", ago: 500 }],
    threads: [],
  },
];

export interface IssueSeed {
  repo: string;
  number: number;
  title: string;
  type: "bug" | "feature" | "task";
  labels: string[];
  author: string;
  ago: number;
  body: string;
}

export const COMMUNITY_ISSUES: IssueSeed[] = [
  { repo: E, number: 1, title: "rebase reports a conflict when both sides delete a file", type: "bug", labels: ["rebase"], author: "omar", ago: 4_000, body: "Both sides removing the same path should be a clean delete." },
  { repo: E, number: 2, title: "Expose graph iterators", type: "feature", labels: ["api"], author: "kit", ago: 2_500, body: "Callers want to walk ancestors lazily." },
  { repo: K, number: 1, title: "Allow per-tool timeouts", type: "feature", labels: ["tools"], author: "rhea", ago: 6_000, body: "A slow tool should not use the whole step budget." },
  { repo: B, number: 1, title: "Pin the Python version in CI", type: "task", labels: ["ci"], author: "omar", ago: 3_000, body: "Results differ across minor versions." },
];
