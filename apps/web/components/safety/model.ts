import type { SafetyFinding, SafetyPeriod, SafetyRecord, SafetyRef, SafetySignalKind, SafetyVerdict } from "@/lib/api/types";
import type { Shape } from "@/components/parts";
import { repoHref } from "@/lib/routes";

export const RUN = "SAFETY_WORK_KIND_RUN";
export const SWARM = "SAFETY_WORK_KIND_SWARM";
export const OPEN = "SAFETY_FINDING_STATE_OPEN";
export const AS_ASKED = "SAFETY_VERDICT_AS_ASKED";
export const NEEDS_LOOK = "SAFETY_VERDICT_NEEDS_A_LOOK";

export const VERDICTS = [
  [AS_ASKED, "As asked", "passed"],
  ["SAFETY_VERDICT_MORE_THAN_ASKED", "More than asked", "merged"],
  ["SAFETY_VERDICT_LESS_THAN_ASKED", "Less than asked", "running"],
  ["SAFETY_VERDICT_SOMETHING_ELSE", "Something else", "failed"],
  [NEEDS_LOOK, "Needs a look", "open"],
] as const satisfies readonly (readonly [SafetyVerdict, string, Shape])[];

export function verdictOf(value?: SafetyVerdict) {
  return VERDICTS.find(([key]) => key === value);
}

export const DETECTED = [
  "outside_task", "around_refusal", "tests_weakened", "own_leash", "said_vs_shown", "harm_to_code",
] as const;
export const REPORTED = ["secrets_and_exit", "following_strangers", "harm_to_others", "legal_flags"] as const;
export const INTRO = "This page shows evidence of what was asked against what was done. It cannot read intent.";
export const CONCERN = "Raising a concern pauses your task, reaches the person who answers for it, and never counts against you.";
export const STOPPED = "This organisation has stopped agent work and frozen agent merges. People can still merge under the repository’s rules.";
export const STOP_DESCRIPTION = "Stops running agent work and freezes agent merges until resumed. A person can still merge.";
export const RESUME_DESCRIPTION = "Lifts the freeze on agent merges. Stopped runs and paused swarms need a separate restart.";

export function signalName(kind?: SafetySignalKind) {
  return kind?.replace("SAFETY_SIGNAL_KIND_", "").toLowerCase() ?? "";
}

export function findingSource(finding: SafetyFinding) {
  const name = finding.reportedBy?.display || finding.reportedBy?.id;
  if (name) return "Reported by " + name;
  return DETECTED.some((kind) => kind === signalName(finding.report?.kind)) ? "Detected" : "Reported";
}

export function safetyHref(repo: string, work?: Pick<SafetyRef, "kind" | "id">) {
  const base = repoHref(repo, "safety");
  return work ? base + "?" + new URLSearchParams({ work: work.id, kind: work.kind }).toString() : base;
}

export function runHref(repo: string, id: string) {
  return repoHref(repo, "agents/runs/" + encodeURIComponent(id));
}

export function workLabel(record: SafetyRecord) {
  return record.asked[0]?.text.split("\n")[0]?.trim() || (record.work?.kind === SWARM ? "Swarm" : "Agent run") + (record.work?.id ? " " + record.work.id : "");
}

export function needsLook(record: SafetyRecord) {
  return record.verdict === NEEDS_LOOK || record.findings.some((f) => f.state === OPEN);
}

export function verdictReason(record: SafetyRecord) {
  if (record.override) {
    const name = record.override.person?.display || record.override.person?.id || "A person";
    return name + " set this verdict and left a note.";
  }
  switch (record.verdict) {
    case AS_ASKED:
      return "The recorded evidence did not produce a finding that changed this verdict.";
    case "SAFETY_VERDICT_MORE_THAN_ASKED":
      return "The evidence records work outside the task’s claimed scope.";
    case "SAFETY_VERDICT_LESS_THAN_ASKED":
      return "The evidence shows unfinished work or could not confirm what was reported as done.";
    case "SAFETY_VERDICT_SOMETHING_ELSE":
      return "The evidence records work outside the claimed scope and work that could not be confirmed as done.";
    case NEEDS_LOOK:
      return "A recorded finding calls for a closer look.";
    default:
      return "No verdict was recorded.";
  }
}

export function recentPeriod(now = Date.now()): SafetyPeriod {
  return { start: new Date(now - 30 * 86_400_000).toISOString(), end: new Date(now).toISOString() };
}
