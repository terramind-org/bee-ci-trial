/**
 * Plain words for the Inbox: runtime names, counts, policy requirements, checks and risk. Pure
 * functions of their inputs, so pages render the same on the server and in tests.
 */
import { DIFF_CLASSES, type CheckRun, type Principal } from "@/lib/api/types";

export function isAgent(p: Principal | undefined): boolean {
  return p?.kind === "PRINCIPAL_KIND_AGENT";
}

const RUNTIMES: Record<string, string> = { "claude-code": "Claude Code", codex: "Codex", cursor: "Cursor", aider: "Aider", gemini: "Gemini CLI" };

/** "claude-code/3.2" → "Claude Code"; unknown runtimes are title-cased. */
export function runtimeName(runtime: string): string {
  const base = (runtime.split("/")[0] ?? "").trim().toLowerCase();
  if (!base) return "";
  return (
    RUNTIMES[base] ??
    base
      .split(/[-_ ]+/)
      .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
      .join(" ")
  );
}

const NUMBER_WORDS = ["no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine"];

export function countWord(n: number): string {
  return NUMBER_WORDS[n] ?? String(n);
}

/** The noun alone, singular or plural: noun(2, "agent is", "agents are") → "agents are". */
export function noun(n: number, one: string, many = one + "s"): string {
  return n === 1 ? one : many;
}

export function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** A parsed `approval:<from>:<scope> (have of need)` entry from EvaluateLanding.missing. */
export interface ApprovalNeed {
  from: "human" | "any" | "owner";
  /** The diff class for human approvals, the path for owner approvals. */
  scope: string;
  have: number;
  need: number;
}

export function parseApproval(entry: string): ApprovalNeed | null {
  const m = /^approval:(human|any|owner)(?::([^ ]+))? \((\d+) of (\d+)\)$/.exec(entry.trim());
  if (!m) return null;
  return { from: m[1] as ApprovalNeed["from"], scope: m[2] ?? "", have: Number(m[3]), need: Number(m[4]) };
}

/**
 * One missing landing rule as a predicate of the change, e.g. "needs one more approval from a
 * person", so a list of them reads after "it" or "Blocked:".
 */
export function missingInWords(entry: string): string {
  const a = parseApproval(entry);
  if (a) {
    const left = Math.max(a.need - a.have, 1);
    const more = a.have > 0 ? " more" : "";
    const noun = left === 1 ? "approval" : "approvals";
    const who = a.from === "human" ? "from a person" : a.from === "owner" ? `from an owner of ${a.scope}` : "";
    return `needs ${countWord(left)}${more} ${noun}${who ? " " + who : ""}`;
  }
  const check = /^check:([^ ]+) \((failed|running|queued|missing)\)$/.exec(entry);
  if (check) return check[2] === "failed" ? `failed the ${check[1]} check` : check[2] === "missing" ? `has not run the ${check[1]} check yet` : `is still waiting for the ${check[1]} check`;
  const lease = /^lease:exclusive:(.+)$/.exec(entry);
  if (lease) return `needs exclusive editing of ${lease[1]}`;
  const stack = /^stack: parent (.+) must land first$/.exec(entry);
  if (stack) return `waits for “${stack[1]}” to merge first`;
  return entry;
}

/** Risk from the SPEC §11.3 diff class: a label, a sentence and a position on an 8-step meter. */
export interface Risk {
  level: "Very low" | "Low" | "Medium" | "High";
  why: string;
  /** 1-based position in DIFF_CLASSES; 0 when the class is unknown. */
  step: number;
  steps: number;
}

const WHY: Record<string, [Risk["level"], string]> = {
  generated: ["Very low", "Only generated files change."],
  docs: ["Very low", "Only documentation changes."],
  comment_only: ["Very low", "Only comments change."],
  test_only: ["Low", "Only tests change."],
  rename_only: ["Low", "Files move or are renamed; their contents do not change."],
  dependency: ["Medium", "A dependency changes version."],
  logic: ["Medium", "Program behaviour changes."],
  sensitive: ["High", "It touches paths the policy marks sensitive."],
};

export function riskOf(diffClass: string): Risk {
  const [level, why] = WHY[diffClass] ?? ["Medium", "The kind of change is not classified."];
  return { level, why, step: DIFF_CLASSES.indexOf(diffClass as (typeof DIFF_CLASSES)[number]) + 1, steps: DIFF_CLASSES.length };
}

export type CheckState = "passed" | "failed" | "running" | "queued" | "skipped";

export function checkState(c: CheckRun): CheckState {
  switch (c.status) {
    case "CHECK_STATUS_PASSED":
      return "passed";
    case "CHECK_STATUS_FAILED":
      return "failed";
    case "CHECK_STATUS_RUNNING":
      return "running";
    case "CHECK_STATUS_SKIPPED":
      return "skipped";
    default:
      return "queued";
  }
}

/** "2 of 3 checks passed; e2e is still running." */
export function checksInWords(checks: CheckRun[]): string {
  if (!checks.length) return "No checks have run.";
  const by = (s: CheckState) => checks.filter((c) => checkState(c) === s).map((c) => c.name);
  const passed = by("passed").length;
  const failed = by("failed");
  const pending = [...by("running"), ...by("queued")];
  const head = passed === checks.length ? `All ${countWord(passed)} checks passed.` : `${passed} of ${checks.length} checks passed`;
  if (passed === checks.length) return head;
  const parts: string[] = [];
  if (failed.length) parts.push(`${list(failed)} failed`);
  if (pending.length) parts.push(`${list(pending)} ${pending.length === 1 ? "is" : "are"} still running`);
  return head + (parts.length ? "; " + parts.join(", ") : "") + ".";
}

/** "a", "a and b", "a, b and c". */
export function list(items: string[]): string {
  if (items.length <= 1) return items.join("");
  return items.slice(0, -1).join(", ") + " and " + items[items.length - 1];
}
