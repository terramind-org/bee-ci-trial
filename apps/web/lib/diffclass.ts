/**
 * Semantic diff classes (SPEC §11.3): presentation helpers plus the deterministic classifier
 * the fixture adapter uses. The real server owns classification; the UI only displays it.
 */
import { DIFF_CLASSES, type DiffClass } from "./api/types";
import { diffLines, splitLines } from "./diff";

export type Risk = "low" | "medium" | "elevated" | "high";

export const CLASS_INFO: Record<DiffClass, { label: string; risk: Risk; blurb: string }> = {
  generated: { label: "Generated", risk: "low", blurb: "Matches a generated-path rule (lockfiles, snapshots, codegen)." },
  docs: { label: "Docs", risk: "low", blurb: "Documentation paths only." },
  comment_only: { label: "Comments only", risk: "low", blurb: "Only comments and whitespace changed." },
  test_only: { label: "Tests only", risk: "low", blurb: "Only test paths changed." },
  rename_only: { label: "Rename only", risk: "low", blurb: "Content-identical moves." },
  dependency: { label: "Dependency", risk: "medium", blurb: "Dependency manifests changed." },
  logic: { label: "Logic", risk: "elevated", blurb: "Behaviour changes in code." },
  sensitive: { label: "Sensitive", risk: "high", blurb: "Touches a path policy marks sensitive." },
};

export function isDiffClass(s: string): s is DiffClass {
  return (DIFF_CLASSES as readonly string[]).includes(s);
}

export function classInfo(s: string) {
  return isDiffClass(s) ? CLASS_INFO[s] : { label: s || "Unclassified", risk: "medium" as Risk, blurb: "" };
}

export function riskRank(s: string): number {
  return isDiffClass(s) ? DIFF_CLASSES.indexOf(s) : -1;
}

export function highestClass(classes: string[]): DiffClass {
  let best: DiffClass = "generated";
  for (const c of classes) if (isDiffClass(c) && riskRank(c) > riskRank(best)) best = c;
  return best;
}

export interface ClassRules {
  sensitive: string[];
  generated: RegExp[];
  dependency: RegExp[];
  docs: RegExp[];
  tests: RegExp[];
}

export const DEFAULT_RULES: ClassRules = {
  sensitive: ["src/auth/", ".bee/", ".github/workflows/", "migrations/"],
  generated: [/(^|\/)bun\.lock$/, /(^|\/)package-lock\.json$/, /(^|\/)Cargo\.lock$/, /\.snap$/, /(^|\/)gen\//],
  dependency: [/(^|\/)package\.json$/, /(^|\/)Cargo\.toml$/, /(^|\/)go\.mod$/, /(^|\/)requirements\.txt$/],
  docs: [/\.mdx?$/, /^docs\//],
  tests: [/^tests?\//, /\.test\.[jt]sx?$/, /_test\.go$/],
};

function isCommentLine(line: string): boolean {
  const t = line.trim();
  return t === "" || t.startsWith("//") || t.startsWith("#") || t.startsWith("/*") || t.startsWith("*") || t.startsWith("--");
}

/** Classifies one path. `oldText`/`newText` are null for adds and deletes. */
export function classifyPath(path: string, oldText: string | null, newText: string | null, rules = DEFAULT_RULES): DiffClass {
  if (rules.sensitive.some((p) => path.startsWith(p))) return "sensitive";
  if (rules.generated.some((r) => r.test(path))) return "generated";
  if (rules.dependency.some((r) => r.test(path))) return "dependency";
  if (rules.docs.some((r) => r.test(path))) return "docs";
  if (rules.tests.some((r) => r.test(path))) return "test_only";
  if (oldText !== null && newText !== null) {
    const ops = diffLines(splitLines(oldText), splitLines(newText));
    const changed = ops.filter((o) => o.kind !== "eq");
    if (changed.length > 0 && changed.every((o) => isCommentLine(o.line))) return "comment_only";
  }
  return "logic";
}

/** Detects content-identical moves: a delete and an add with the same content. */
export function classifyChange(
  files: { path: string; oldText: string | null; newText: string | null }[],
  rules = DEFAULT_RULES,
): { perPath: Record<string, DiffClass>; overall: DiffClass } {
  const perPath: Record<string, DiffClass> = {};
  const deleted = new Map(files.filter((f) => f.newText === null && f.oldText !== null).map((f) => [f.oldText, f.path]));
  for (const f of files) {
    if (f.oldText === null && f.newText !== null && deleted.has(f.newText)) {
      perPath[f.path] = rules.sensitive.some((p) => f.path.startsWith(p)) ? "sensitive" : "rename_only";
      const from = deleted.get(f.newText);
      if (from && !rules.sensitive.some((p) => from.startsWith(p))) perPath[from] = "rename_only";
    }
  }
  for (const f of files) if (!perPath[f.path]) perPath[f.path] = classifyPath(f.path, f.oldText, f.newText, rules);
  return { perPath, overall: highestClass(Object.values(perPath)) };
}
