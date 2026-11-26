/**
 * The discovery data set: a second org of public repos (plus one private repo) with topics,
 * READMEs and sources in three languages, and the people and agents that land on them. Pure
 * constants and file contents, so a spec can import this module without running the seeder
 * (importing e2e/seed.ts itself would start it).
 */

/** The second org; the first one is `dev`, which holds the playground repos. */
export const DISCOVERY_ORG = "lattice";

export interface DiscoveryRepo {
  /** `org/name`. */
  fullName: string;
  name: string;
  description: string;
  topics: string[];
  /** Primary language the language stats should report. */
  language: "Rust" | "TypeScript" | "Python";
  visibility: "public" | "private";
  /** Files of the first landed commit; README.md is always among them. */
  files: Record<string, string>;
}

/** The public repos of the discovery org: anonymous visitors can read these. */
export const PUBLIC_REPOS = ["lattice/agentkit", "lattice/tl", "lattice/benchkit", "lattice/mergekit"] as const;
/** A private repo in the discovery org: anonymous visitors must never see it. */
export const PRIVATE_REPO = "lattice/internal-roadmap";
/** Every topic set on a public discovery repo, sorted. */
export const DISCOVERY_TOPICS = ["agents", "benchmarks", "cli", "python", "rust", "typescript", "vcs"] as const;
/** The topic carried by the most public repos (a topic page with several repos). */
export const BUSIEST_TOPIC = "agents";
/** Logins: the seeded human owner, the second human, and the two agents. */
export const LOGINS = { human: "dev", reviewer: "reviewer", claude: "claude-code", codex: "codex" } as const;
/** What the seeder made of stars and follows, so specs can assert non-zero counts. */
export const EXPECTED = {
  /** Stars on lattice/agentkit (the most starred repo). */
  agentkitStars: 4,
  /** Followers of `dev`. */
  devFollowers: 3,
  /** Accounts `dev` follows. */
  devFollowing: 2,
} as const;
/** Stars per public repo, by acting login. */
export const STARS: Record<string, string[]> = {
  "lattice/agentkit": ["dev", "reviewer", "claude-code", "codex"],
  "lattice/tl": ["dev", "reviewer", "claude-code"],
  "lattice/benchkit": ["reviewer", "codex"],
  "lattice/mergekit": ["dev"],
};
/** Follows as [follower, followee]. */
export const FOLLOWS: [string, string][] = [
  ["reviewer", "dev"],
  ["claude-code", "dev"],
  ["codex", "dev"],
  ["dev", "reviewer"],
  ["dev", "claude-code"],
];

const rustFiles = (crate: string, body: string) => ({
  "Cargo.toml": `[package]\nname = "${crate}"\nversion = "0.1.0"\nedition = "2021"\n`,
  "src/lib.rs": body,
  "src/main.rs": `fn main() {\n    println!("${crate} {}", env!("CARGO_PKG_VERSION"));\n}\n`,
});

export const REPOS: DiscoveryRepo[] = [
  {
    fullName: "lattice/agentkit",
    name: "agentkit",
    description: "Building blocks for coding agents: sessions, tool calls and budgets",
    topics: ["agents", "rust"],
    language: "Rust",
    visibility: "public",
    files: {
      "README.md": "# agentkit\n\nBuilding blocks for coding agents that work on a shared branch.\n\n## What is in it\n\n- `Session`: tracks tool calls and the tokens they spent\n- `Budget`: stops a run before it overspends\n\n```rust\nlet mut s = agentkit::Session::new(1_000);\ns.spend(250);\nassert_eq!(s.remaining(), 750);\n```\n",
      ...rustFiles(
        "agentkit",
        "/// A run of one agent with a token budget.\npub struct Session {\n    budget: u64,\n    spent: u64,\n}\n\nimpl Session {\n    pub fn new(budget: u64) -> Self {\n        Session { budget, spent: 0 }\n    }\n\n    pub fn spend(&mut self, tokens: u64) {\n        self.spent = (self.spent + tokens).min(self.budget);\n    }\n\n    pub fn remaining(&self) -> u64 {\n        self.budget - self.spent\n    }\n}\n",
      ),
    },
  },
  {
    fullName: "lattice/tl",
    name: "tl",
    description: "A small command line for working with changesets",
    topics: ["cli", "rust", "vcs"],
    language: "Rust",
    visibility: "public",
    files: {
      "README.md": "# tl\n\nA small command line for working with changesets.\n\n```sh\ntl log\ntl land --ref main\n```\n\nRun `tl help` for every command.\n",
      ...rustFiles(
        "tl",
        "/// The commands tl understands.\npub const COMMANDS: [&str; 3] = [\"log\", \"land\", \"help\"];\n\npub fn is_command(word: &str) -> bool {\n    COMMANDS.contains(&word)\n}\n",
      ),
    },
  },
  {
    fullName: "lattice/benchkit",
    name: "benchkit",
    description: "Reproducible benchmarks for agent workflows",
    topics: ["agents", "benchmarks", "typescript"],
    language: "TypeScript",
    visibility: "public",
    files: {
      "README.md": "# benchkit\n\nReproducible benchmarks for agent workflows. Every run records its inputs, so two runs can be compared.\n\n```ts\nimport { measure } from \"./src/measure\";\nconsole.log(await measure(\"land\", () => landOnce()));\n```\n",
      "package.json": '{\n  "name": "benchkit",\n  "version": "0.1.0",\n  "type": "module"\n}\n',
      "src/measure.ts": "export interface Sample {\n  name: string;\n  millis: number;\n}\n\n/** Times one call of `fn` and names the sample. */\nexport async function measure(name: string, fn: () => Promise<void>): Promise<Sample> {\n  const start = performance.now();\n  await fn();\n  return { name, millis: performance.now() - start };\n}\n",
      "src/report.ts": 'import type { Sample } from "./measure";\n\nexport function report(samples: Sample[]): string {\n  return samples.map((s) => `${s.name}: ${s.millis.toFixed(1)} ms`).join("\\n");\n}\n',
    },
  },
  {
    fullName: "lattice/mergekit",
    name: "mergekit",
    description: "Three-way merge helpers for structured files",
    topics: ["vcs", "python"],
    language: "Python",
    visibility: "public",
    files: {
      "README.md": "# mergekit\n\nThree-way merge helpers for structured files.\n\n```python\nfrom mergekit import merge_lines\nmerge_lines(base, ours, theirs)\n```\n",
      "mergekit/__init__.py": '"""Three-way merge helpers."""\n\nfrom .lines import merge_lines\n\n__all__ = ["merge_lines"]\n',
      "mergekit/lines.py": '"""Line-based three-way merge."""\n\n\ndef merge_lines(base, ours, theirs):\n    """Returns the merged lines, or None when both sides changed the same line."""\n    if ours == base:\n        return list(theirs)\n    if theirs == base or ours == theirs:\n        return list(ours)\n    return None\n',
      "tests/test_lines.py": "from mergekit import merge_lines\n\n\ndef test_one_sided_change():\n    assert merge_lines([\"a\"], [\"a\"], [\"b\"]) == [\"b\"]\n",
    },
  },
  {
    fullName: PRIVATE_REPO,
    name: "internal-roadmap",
    description: "Planning notes that are not public yet",
    topics: [],
    language: "Python",
    visibility: "private",
    files: {
      "README.md": "# internal-roadmap\n\nPlanning notes for the next quarter. Private.\n",
      "notes/q4.md": "# Q4\n\n- ship the discovery pages\n",
    },
  },
];

/** A follow-up source change for each public repo: [path, new contents, commit message]. */
export const FOLLOW_UPS: Record<string, [string, string, string][]> = {
  "lattice/agentkit": [
    ["src/lib.rs", REPOS[0]!.files["src/lib.rs"]! + "\nimpl Session {\n    pub fn exhausted(&self) -> bool {\n        self.remaining() == 0\n    }\n}\n", "Add Session::exhausted"],
    ["docs/budgets.md", "# Budgets\n\nA session stops spending at its budget; `exhausted` reports it.\n", "Document budgets"],
  ],
  "lattice/tl": [
    ["src/lib.rs", REPOS[1]!.files["src/lib.rs"]! + "\npub fn usage() -> &'static str {\n    \"tl <log|land|help>\"\n}\n", "Add the usage line"],
    ["CHANGELOG.md", "# Changelog\n\n## 0.1.0\n\n- first release\n", "Start a changelog"],
  ],
  "lattice/benchkit": [
    ["src/stats.ts", "export function median(values: number[]): number {\n  const sorted = [...values].sort((a, b) => a - b);\n  return sorted[Math.floor(sorted.length / 2)] ?? 0;\n}\n", "Add a median helper"],
    ["src/report.ts", 'import type { Sample } from "./measure";\n\nexport function report(samples: Sample[]): string {\n  return samples.map((s) => `${s.name}: ${s.millis.toFixed(2)} ms`).join("\\n");\n}\n', "Report two decimals"],
  ],
  "lattice/mergekit": [
    ["mergekit/words.py", '"""Word-level merge."""\n\n\ndef merge_words(base, ours, theirs):\n    return " ".join(ours.split()) if ours != base else " ".join(theirs.split())\n', "Add a word-level merge"],
    ["tests/test_words.py", "from mergekit.words import merge_words\n\n\ndef test_takes_theirs_when_ours_unchanged():\n    assert merge_words(\"a\", \"a\", \"b\") == \"b\"\n", "Test the word-level merge"],
  ],
};
