import Link from "next/link";
import { principalLabel } from "@/components/domain";
import { State, type Shape } from "@/components/parts";
import { objectIdText } from "@/lib/api/ids";
import { BeeError } from "@/lib/api/transport";
import type { ChangeRequest, Run, RunStatus } from "@/lib/api/types";
import { between, duration } from "@/lib/format";
import { repoHref } from "@/lib/routes";

export const RUN_STATES: Record<RunStatus, [Shape, string]> = {
  failed: ["failed", "Failed"],
  waiting: ["waiting", "Waiting on a person"],
  protection: ["waiting", "Waiting for deployment protection"],
  running: ["running", "Running"],
  queued: ["open", "Queued"],
  passed: ["passed", "Passed"],
  cancelled: ["closed", "Cancelled"],
  skipped: ["closed", "Skipped"],
  neutral: ["closed", "Neutral"],
  stale: ["closed", "Stale"],
  completed: ["closed", "Completed"],
};

export function RunState({ status }: { status: RunStatus }) {
  const [shape, word] = RUN_STATES[status];
  return <State shape={shape}>{word}</State>;
}

export function pullRequestLabel(run: Run, change?: ChangeRequest | null): string {
  const number = change?.number || run.changeRequestNumber;
  return number && number !== "0" ? "#" + number + (change?.title ? " " + change.title : "") : change?.title ?? "";
}

const TRIGGERS: Record<string, string> = {
  change_request: "Pull request",
  pull_request: "Pull request",
  pull_request_target: "Pull request",
  land: "Merge",
  push: "Push",
  workflow_dispatch: "Manual run",
  manual: "Manual run",
  schedule: "Schedule",
};

export function triggerLabel(trigger: string) {
  return TRIGGERS[trigger] ?? trigger.replaceAll("_", " ");
}

export function startedBy(run: Run) {
  const principal = run.triggeringActor ?? run.actor;
  const event = triggerLabel(run.trigger);
  return [principal?.id ? principalLabel(principal) : "", event].filter(Boolean).join(" · ");
}

export function runDuration(run: { startedAt?: string; finishedAt?: string }): string {
  const ms = between(run.startedAt, run.finishedAt);
  return Number.isFinite(ms) && ms >= 0 ? duration(ms) : "";
}

export function CommitLink({ repo, commit }: { repo: string; commit: NonNullable<Run["head"]> }) {
  return <Link className="link mono inline-flex min-h-6 items-center" href={repoHref(repo, "code?rev=" + encodeURIComponent(objectIdText(commit)))}>{objectIdText(commit, true)}</Link>;
}

/** Public repositories and older servers may expose runs without the optional CI reads. */
export async function optionalCi<T>(read: Promise<T>): Promise<T | undefined> {
  try {
    return await read;
  } catch (err) {
    if (err instanceof BeeError && ["permission_denied", "unauthenticated", "unimplemented", "not_found"].includes(err.code)) return undefined;
    throw err;
  }
}
