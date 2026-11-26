import Link from "next/link";
import { Mark, Sep, State, type Shape } from "@/components/parts";
import type { BeeClient } from "@/lib/api/client";
import { mapLimit } from "@/lib/api/ops/shared";
import { BeeError } from "@/lib/api/transport";
import type { AgentRun, AgentRunBudget, ChangeRequest, RepoRef, RepoWorkItem, Swarm } from "@/lib/api/types";
import { usd } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { WorkCue } from "./work-motion";

export type CellState = "open" | "working" | "finished" | "empty";
export interface TaskCell {
  id?: string;
  title: string;
  state: CellState;
  attempt?: number;
}

export const cellLabels: Record<CellState, string> = {
  open: "Open",
  working: "Being worked on",
  finished: "Finished",
  empty: "Came back empty",
};

export function taskMotionKey(row: string, id: string | undefined) {
  return row + "/task/" + (id ?? "");
}

export function TaskCells({ cells, large = false, row }: { cells: TaskCell[]; large?: boolean; row: string }) {
  if (!cells.length) return null;
  return (
    <ul aria-label="Task states" className="flex flex-wrap gap-2">
      {cells.map(({ id, title, state }, index) => {
        const name = title + " · " + cellLabels[state];
        return (
          <li key={id ?? index} className={large ? "relative size-7" : "relative size-4"}>
            <span role="img" aria-label={name} title={name} data-cell={state} className="block size-full text-ink">
              <svg viewBox="0 0 16 16" aria-hidden="true" className="size-full">
                <rect x="0.5" y="0.5" width="15" height="15" stroke="currentColor" fill="none" />
                {state === "working" ? <rect x="1" y="1" width="7" height="14" fill="currentColor" /> : null}
                {state === "empty" ? <path d="M1 15L15 1" stroke="currentColor" /> : null}
              </svg>
            </span>
            {state === "finished" ? <WorkCue piece="cap" watch={taskMotionKey(row, id)} text={title + " finished."} /> : null}
          </li>
        );
      })}
    </ul>
  );
}

export function swarmCells(swarm: Swarm): TaskCell[] {
  return swarm.tasks.map((task) => ({
    id: task.id,
    title: task.title,
    attempt: task.attempts.length,
    state: task.state === "SWARM_TASK_STATE_WORKING" ? "working"
      : task.state === "SWARM_TASK_STATE_FINISHED" ? "finished"
      : task.state === "SWARM_TASK_STATE_EMPTY" ? "empty" : "open",
  }));
}

const RUN_GOALS: Record<string, string> = {
  AGENT_TASK_KIND_ISSUE: "Work on an issue",
  AGENT_TASK_KIND_REVIEW: "Review a pull request",
  AGENT_TASK_KIND_REVISE: "Revise a pull request",
  AGENT_TASK_KIND_SECURITY_FIX: "Address a security alert",
  AGENT_TASK_KIND_ADOPT_EVALUATE: "Evaluate an upgrade",
};

export function runGoal(run: AgentRun): string {
  const { task } = run;
  if (task?.prompt?.trim()) return task.prompt;
  if (task?.issue) return "Work on issue #" + task.issue;
  if (task?.changeRequest) return (task.kind === "AGENT_TASK_KIND_REVISE" ? "Revise" : "Review") + " pull request #" + task.changeRequest;
  return RUN_GOALS[task?.kind ?? ""] ?? "Agent run";
}

interface WorkState {
  label: string;
  shape: Shape;
}

const RUN_STATE: Record<NonNullable<AgentRun["state"]>, WorkState & { cell: CellState }> = {
  AGENT_RUN_STATE_UNSPECIFIED: { label: "State unavailable", shape: "open", cell: "open" },
  AGENT_RUN_STATE_QUEUED: { label: "Queued", shape: "open", cell: "open" },
  AGENT_RUN_STATE_PROVISIONING: { label: "Provisioning", shape: "running", cell: "working" },
  AGENT_RUN_STATE_RUNNING: { label: "Running", shape: "running", cell: "working" },
  AGENT_RUN_STATE_WAITING_FOR_REVIEW: { label: "Waiting for review", shape: "waiting", cell: "working" },
  AGENT_RUN_STATE_SUCCEEDED: { label: "Succeeded", shape: "passed", cell: "finished" },
  AGENT_RUN_STATE_FAILED: { label: "Failed", shape: "failed", cell: "empty" },
  AGENT_RUN_STATE_CANCELLED: { label: "Stopped", shape: "closed", cell: "empty" },
};

export function runState(run: AgentRun) {
  return RUN_STATE[run.state ?? "AGENT_RUN_STATE_UNSPECIFIED"];
}

export function runCells(run: AgentRun): TaskCell[] {
  return [{ title: runGoal(run), state: runState(run).cell }];
}

export function finishedRun(run: AgentRun): boolean {
  const { cell } = runState(run);
  return cell === "finished" || cell === "empty";
}

const SWARM_STATE: Record<Swarm["state"], WorkState> = {
  SWARM_STATE_UNSPECIFIED: { label: "State unavailable", shape: "open" },
  SWARM_STATE_PLANNING: { label: "Planning", shape: "open" },
  SWARM_STATE_WORKING: { label: "Working", shape: "running" },
  SWARM_STATE_PAUSED: { label: "Paused", shape: "waiting" },
  SWARM_STATE_STOPPED: { label: "Stopped", shape: "closed" },
  SWARM_STATE_FINISHED: { label: "Finished", shape: "passed" },
};

export function swarmState(swarm: Swarm) {
  return SWARM_STATE[swarm.state];
}

export function budgetText(cost?: { usdMicros?: string }, budget?: AgentRunBudget): string | undefined {
  const spent = cost?.usdMicros !== undefined ? usd(cost.usdMicros) + " spent" : undefined;
  const limit = budget?.maxUsdMicros !== undefined ? Number(budget.maxUsdMicros) > 0 ? usd(budget.maxUsdMicros) + " budget" : "no spending limit" : undefined;
  return [spent, limit].filter(Boolean).join(" · ") || undefined;
}

/** A work item can outlive its pull request's visibility. Keep the work row readable. */
export async function changeFacts(api: BeeClient, repo: RepoRef, ids: string[], numbers: string[] = []): Promise<ChangeRequest[]> {
  const readable = async <T,>(load: Promise<T>) => {
    try {
      return await load;
    } catch (error) {
      if (error instanceof BeeError && ["not_found", "permission_denied"].includes(error.code)) return null;
      throw error;
    }
  };
  const linked = await mapLimit([...new Set(ids)], 8, (id) => readable(api.change.getChange({ repo, id })));
  const known = new Set(linked.map((change) => change?.number));
  // Live runs can report a number before the work feed has a resolved pull request ID.
  const extra = await mapLimit([...new Set(numbers)].filter((number) => !known.has(number)), 8, (number) => readable(api.change.getChangeByNumber({ repo, number })));
  return [...linked, ...extra].filter((change) => change !== null);
}

export function pullState(change: ChangeRequest): WorkState {
  switch (change.state) {
    case "CHANGE_STATE_OPEN": return { label: "Open", shape: "open" };
    case "CHANGE_STATE_LANDED": return { label: "Merged", shape: "merged" };
    case "CHANGE_STATE_CLOSED": return { label: "Closed", shape: "closed" };
    default: return { label: "State unavailable", shape: "open" };
  }
}

export function WorkFacts({ branch, cost, budget, owner, changes = [], repo }: {
  branch?: string;
  cost?: { usdMicros?: string };
  budget?: AgentRunBudget;
  owner?: { display?: string; id?: string };
  changes?: ChangeRequest[];
  repo: string;
}) {
  const spend = budgetText(cost, budget);
  const person = owner?.display || owner?.id;
  return (
    <>
      {branch ? <><Sep /><code className="mono [overflow-wrap:anywhere]">{branch}</code></> : null}
      {changes.map((change) => {
        const state = pullState(change);
        return (
          <span key={change.id} className="inline-flex flex-wrap items-center gap-2">
            <Sep />
            <Link href={repoHref(repo, "changes/" + encodeURIComponent(change.id))} className="link inline-flex min-h-6 items-center" title={change.title}>
              Pull request #{change.number}
            </Link>
            <State shape={state.shape}>{state.label}</State>
          </span>
        );
      })}
      {spend ? <><Sep /><span>{spend}</span></> : null}
      {person ? <><Sep /><Mark>{person}</Mark></> : null}
    </>
  );
}

export function workPath(item: RepoWorkItem): string {
  return item.swarm ? "agents/swarms/" + encodeURIComponent(item.swarm.id) : "agents/runs/" + encodeURIComponent(item.run?.id ?? "");
}

export function workAnchor(item: RepoWorkItem): string {
  return (item.swarm ? "swarm-" + item.swarm.id : "run-" + item.run?.id);
}

export function acceptedJoins(swarm?: Swarm, viewerId?: string) {
  if (!viewerId || !swarm) return [];
  return swarm.joins.filter((request) => request.requester?.id === viewerId && request.state === "SWARM_JOIN_STATE_ACCEPTED");
}

export function workMotionSnapshot(items: RepoWorkItem[], viewerId?: string): Record<string, string> {
  const snapshot: Record<string, string> = {};
  for (const item of items) {
    const { swarm, run } = item;
    const row = workAnchor(item);
    const cells = swarm ? swarmCells(swarm) : run ? runCells(run) : [];
    for (const cell of cells) {
      snapshot[taskMotionKey(row, cell.id)] = cell.state + "/" + (cell.attempt ?? "");
    }
    if (swarm) {
      snapshot[row + "/pause"] = swarm.state;
      // Keep empty accepted sets in the snapshot so a request accepted within one refresh is seen.
      snapshot[row + "/join"] = JSON.stringify(acceptedJoins(swarm, viewerId).map(({ id }) => id).sort());
    }
  }
  return snapshot;
}

export async function repositoryWork(api: BeeClient, repo: RepoRef): Promise<RepoWorkItem[]> {
  const items = new Map<string, RepoWorkItem>();
  let pageToken: string | undefined;
  do {
    const result = await api.swarm.listRepoWork({ repo, page: { pageSize: 100, pageToken } });
    for (const item of result.items) {
      if (item.swarm?.id || item.run?.id) items.set(workAnchor(item), item);
    }
    pageToken = result.page?.nextPageToken;
  } while (pageToken);
  return [...items.values()].sort((a, b) => (Date.parse(b.activityAt ?? "") || 0) - (Date.parse(a.activityAt ?? "") || 0));
}
