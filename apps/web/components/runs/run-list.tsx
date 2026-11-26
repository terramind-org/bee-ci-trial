import Link from "next/link";
import type { ReactNode } from "react";
import { Quiet, Row, Rows, Sep, State, type Shape } from "@/components/parts";
import { Time } from "@/components/time";
import { objectIdText } from "@/lib/api/ids";
import type { ChangeRequest, Deployment, Run, RunStatus } from "@/lib/api/types";
import { repoHref } from "@/lib/routes";
import { pullRequestLabel, runDuration, RunState, startedBy } from "./run-facts";

const WEEK = 7 * 24 * 60 * 60 * 1000;

export const groupTitles = ["Failed", "Waiting on a person", "Running"] as const;
const groupStates: RunStatus[][] = [["failed"], ["waiting"], ["running", "queued", "protection"]];

export function groupRuns(runs: Run[], now: number) {
  const groups = groupTitles.map((title, i) => ({ title, runs: runs.filter((r) => groupStates[i]?.includes(r.status)) }));
  const passed = runs.filter((r) => r.status === "passed");
  const recent = passed.filter((r) => {
    const end = Date.parse(r.finishedAt ?? "");
    return end >= now - WEEK && end <= now;
  });
  const cancelled = runs.filter((r) => r.status === "cancelled");
  const other = runs.filter((r) => ["skipped", "neutral", "stale", "completed"].includes(r.status));
  return { groups, passed, recent, cancelled, other };
}

export function passingLabel(recent: number, total: number, scoped: boolean): string {
  if (!recent && total) return total === 1 ? "1 passing run" : total + " passing runs";
  const label = recent + (recent === 1 ? " run passed this week" : " runs passed this week");
  return scoped ? label + " in these results" : label;
}

export function runMatches(run: Run, change: ChangeRequest | undefined, query: string): boolean {
  return [run.workflow, run.number, run.ref, pullRequestLabel(run, change), startedBy(run), ...run.jobs.map((j) => j.name)]
    .join(" ").toLowerCase().includes(query.toLowerCase());
}

function runDetails(run: Run): ReactNode[] {
  const failed = run.jobs.filter((job) => job.status === "failed").map((job) => job.name);
  return [
    failed.length ? failed.join(", ") + " failed" : "",
    startedBy(run),
    runDuration(run),
    objectIdText(run.head, true) && <span className="mono">{objectIdText(run.head, true)}</span>,
    run.attempt > 1 ? "attempt " + run.attempt : "",
  ].filter(Boolean);
}

export function RunLine({ repo, run, change, agent = false }: { repo: string; run: Run; change?: ChangeRequest; agent?: boolean }) {
  const pull = pullRequestLabel(run, change);
  const details = runDetails(run);
  const at = run.startedAt ?? run.createdAt;
  const workflow = (
    <Link href={repoHref(repo, "runs/" + run.id)} data-nav-item className="link inline-flex min-h-6 items-center">
      {run.workflow} #{run.number}
    </Link>
  );
  const target = pull || (run.ref && <span className="mono">{run.ref}</span>);
  if (agent) return <>{workflow}{target && <> · {target}</>} · <RunState status={run.status} />{details.map((value, i) => <span key={i}> · {value}</span>)}{at && <> · <time dateTime={at}>{at}</time></>}</>;
  return (
    <Row meta={<><RunState status={run.status} />{details.map((value, i) => <span key={i}><Sep /> {value}</span>)}{at && <span><Sep /> <Time iso={at} /></span>}</>}>
      {workflow}{target && <> <Sep /> {target}</>}
    </Row>
  );
}

const DEPLOYMENT_STATES: Record<NonNullable<Deployment["state"]>, [Shape, string]> = {
  DEPLOYMENT_STATE_UNSPECIFIED: ["open", "State unavailable"],
  DEPLOYMENT_STATE_PENDING: ["waiting", "Pending"],
  DEPLOYMENT_STATE_QUEUED: ["open", "Queued"],
  DEPLOYMENT_STATE_IN_PROGRESS: ["running", "Running"],
  DEPLOYMENT_STATE_SUCCESS: ["passed", "Succeeded"],
  DEPLOYMENT_STATE_FAILURE: ["failed", "Failed"],
  DEPLOYMENT_STATE_ERROR: ["failed", "Error"],
  DEPLOYMENT_STATE_INACTIVE: ["closed", "Inactive"],
};

export function deploymentParts(deployment: Deployment) {
  const [shape, state] = DEPLOYMENT_STATES[deployment.state ?? "DEPLOYMENT_STATE_UNSPECIFIED"];
  return {
    state: <State shape={shape}>{state}</State>,
    target: [deployment.ref, objectIdText(deployment.commit, true)].filter(Boolean).join(" · "),
  };
}

export function RunList({ repo, runs, changes, now, scoped }: { repo: string; runs: Run[]; changes: Map<string, ChangeRequest>; now: number; scoped: boolean }) {
  const { groups, passed, recent, cancelled, other } = groupRuns(runs, now);
  const rows = (list: Run[]) => <Rows>{list.map((run) => <RunLine key={run.id} repo={repo} run={run} change={changes.get(run.changeRequestId)} />)}</Rows>;
  return (
    <div className="flex flex-col gap-8">
      {groups.map(({ title, runs }) => runs.length > 0 && (
        <section key={title} aria-label={title}>
          <h2 className="mb-2 text-sm font-medium text-ink-2">{title}</h2>
          {rows(runs)}
        </section>
      ))}
      {runs.length ? (
        <details className="border-t border-line pt-3">
          <summary className="link min-h-6 cursor-pointer text-base text-ink-2">{passingLabel(recent.length, passed.length, scoped)}</summary>
          {passed.length ? <div className="mt-3">{rows(passed)}</div> : <Quiet className="mt-2">No passing runs in these results.</Quiet>}
        </details>
      ) : null}
      {[{ title: "Cancelled", runs: cancelled }, { title: "Other completed runs", runs: other }].map(({ title, runs }) => runs.length > 0 && (
        <section key={title} aria-label={title}>
          <h2 className="mb-2 text-sm font-medium text-ink-2">{title}</h2>
          {rows(runs)}
        </section>
      ))}
    </div>
  );
}
