/**
 * Checks in words (docs/spec/web-ui.md §3, the Brief tab's "Is it safe?"): check runs grouped by
 * outcome, each group a sentence, plus one summary line for the status sentence and the review
 * queue. Also the small prose helpers the other brief libraries share. Pure: no clock, no I/O.
 */
import type { CheckRun } from "./api/types";

const NUMBER_WORDS = ["no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine"];

/** 0-9 as words ("no", "one", …), larger numbers as digits. */
export function countWord(n: number): string {
  return NUMBER_WORDS[n] ?? String(n);
}

/** "a", "a and b", "a, b and c". */
export function listing(items: string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return items.slice(0, -1).join(", ") + " and " + items[items.length - 1];
}

/** Up to `max` names, then "and N more". */
export function shortListing(items: string[], max = 3): string {
  if (items.length <= max) return listing(items);
  return items.slice(0, max).join(", ") + " and " + (items.length - max) + " more";
}

export function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

export type CheckState = "failed" | "running" | "queued" | "passed" | "skipped";

/** The order groups are shown in: what needs attention first. */
export const CHECK_STATES: readonly CheckState[] = ["failed", "running", "queued", "passed", "skipped"];

export function checkState(c: Pick<CheckRun, "status">): CheckState {
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

export interface CheckGroup {
  state: CheckState;
  /** "Failed", "Still running", "Waiting to start", "Passed", "Skipped". */
  label: string;
  /** "test and lint failed." */
  sentence: string;
  checks: CheckRun[];
}

const LABEL: Record<CheckState, string> = { failed: "Failed", running: "Still running", queued: "Waiting to start", passed: "Passed", skipped: "Skipped" };

function groupSentence(state: CheckState, names: string[]): string {
  const many = names.length !== 1;
  const who = names.length > 6 ? countWord(names.length) + " checks" : listing(names);
  switch (state) {
    case "failed":
      return capitalize(who) + " failed.";
    case "running":
      return capitalize(who) + (many ? " are" : " is") + " still running.";
    case "queued":
      return capitalize(who) + (many ? " are" : " is") + " waiting to start.";
    case "passed":
      return capitalize(who) + " passed.";
    case "skipped":
      return capitalize(who) + (many ? " were" : " was") + " skipped.";
  }
}

/**
 * Checks grouped by outcome, in CHECK_STATES order; within a group by name, so the result does
 * not depend on the order the server listed them in. When a check ran more than once (the same
 * name), only its last run in the input counts.
 */
export function groupChecks(checks: CheckRun[]): CheckGroup[] {
  const latest = new Map<string, CheckRun>();
  for (const c of checks) latest.set(c.name, c);
  const all = [...latest.values()].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const out: CheckGroup[] = [];
  for (const state of CHECK_STATES) {
    const group = all.filter((c) => checkState(c) === state);
    if (group.length) out.push({ state, label: LABEL[state], sentence: groupSentence(state, group.map((c) => c.name)), checks: group });
  }
  return out;
}

export interface CheckTally {
  total: number;
  passed: number;
  failed: string[];
  pending: string[];
  skipped: number;
}

export function tallyChecks(checks: CheckRun[]): CheckTally {
  const groups = groupChecks(checks);
  const names = (s: CheckState) => groups.find((g) => g.state === s)?.checks.map((c) => c.name) ?? [];
  return {
    total: groups.reduce((n, g) => n + g.checks.length, 0),
    passed: names("passed").length,
    failed: names("failed"),
    pending: [...names("running"), ...names("queued")].sort(),
    skipped: names("skipped").length,
  };
}

/**
 * One clause for the whole set, lower case so it can follow a colon:
 *   "all 14 checks pass", "test failed", "2 of 14 checks still running", "no checks have run".
 */
export function checksClause(checks: CheckRun[]): string {
  const t = tallyChecks(checks);
  if (t.total === 0) return "no checks have run";
  if (t.failed.length) return shortListing(t.failed) + (t.failed.length === 1 ? " check failed" : " checks failed");
  if (t.pending.length) return t.pending.length + " of " + t.total + (t.total === 1 ? " check" : " checks") + " still running";
  const ran = t.total - t.skipped;
  if (ran === 0) return "every check was skipped";
  const all = ran === 1 ? "the one check passes" : "all " + ran + " checks pass";
  return t.skipped ? all + " (" + t.skipped + " skipped)" : all;
}

/** The summary as a sentence: "All 14 checks pass." */
export function checksSentence(checks: CheckRun[]): string {
  return capitalize(checksClause(checks)) + ".";
}
