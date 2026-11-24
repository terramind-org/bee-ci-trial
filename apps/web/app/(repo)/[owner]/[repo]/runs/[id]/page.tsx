import Link from "next/link";
import type { Metadata } from "next";
import { LiveRefresh } from "@/components/live-refresh";
import { PrincipalName, RunStatusBadge, StatusIcon } from "@/components/domain";
import { Icon } from "@/components/icons";
import { LogViewer, RerunButtons } from "@/components/log-viewer";
import { Time } from "@/components/time";
import { PageMenu } from "@/components/menu";
import { WhatHappened } from "@/components/queue/what-happened";
import { Badge, PageHeader, Panel, cx } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import { between, duration } from "@/lib/format";
import { runSummary } from "@/lib/runsummary";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Run" };

export default async function RunPage({ params, searchParams }: { params: Promise<RepoParams & { id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const sp = await searchParams;
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const run = await api.workflow.getRun({ repo: ref, id });
    const cr = run.changeRequestId ? await api.change.getChange({ repo: ref, id: run.changeRequestId }).catch(() => null) : null;
    const job = run.jobs.find((j) => j.id === param(sp, "job")) ?? run.jobs.find((j) => j.status === "failed") ?? run.jobs.find((j) => j.status === "running") ?? run.jobs[0];
    const getOp = { service: "WorkflowService" as const, method: "GetRun", request: { repo: ref, id }, label: "Show this run" };
    const summary = runSummary(run, Date.now());
    return (
      <>
        <LiveRefresh repo={fullName} ids={[id]} kinds={["workflow_run.*", "workflow_job.*"]} />
        <PageHeader
          eyebrow={
            <span className="flex items-center gap-2">
              <Link href={repoHref(fullName, "runs")} className="hover:text-ink">
                Runs
              </Link>
              <Icon name="chevronRight" size={12} />
              <span>
                {run.workflow} #{run.number}
              </span>
            </span>
          }
          title={
            <span className="flex flex-wrap items-center gap-3">
              {run.workflow} <span className="num text-ink-3">#{run.number}</span>
              <RunStatusBadge status={run.status} />
            </span>
          }
          meta={
            <>
              {cr ? (
                <Link href={repoHref(fullName, "changes/" + cr.id)} className="link">
                  {cr.title}
                </Link>
              ) : (
                <span>post-land on main</span>
              )}
              <span>{run.ref}</span>
              <span className="num text-ink-3">{objectIdText(run.head, true)}</span>
              <PrincipalName principal={run.actor} showRuntime={false} />
              <span>
                started <Time iso={run.startedAt} />
              </span>
              {run.finishedAt ? <span>took {duration(between(run.startedAt, run.finishedAt))}</span> : null}
              {run.attempt > 1 ? <Badge tone="outline">attempt {run.attempt}</Badge> : null}
            </>
          }
          actions={
            <>
              <RerunButtons repo={fullName} runId={id} hasFailed={run.jobs.some((j) => j.status === "failed")} running={run.status === "running"} />
              <PageMenu op={getOp} />
            </>
          }
        />
        <WhatHappened summary={summary} />

        <div className="grid grid-cols-1 gap-5 lg:grid-cols-[260px_minmax(0,1fr)]">
          <nav aria-label="Jobs" className="flex flex-col gap-5">
            <Panel title="Jobs" id="jobs-title">
              <ul className="divide-y divide-line">
                {run.jobs.map((j) => (
                  <li key={j.id}>
                    <Link
                      href={repoHref(fullName, "runs/" + id + "?job=" + j.id)}
                      aria-current={j.id === job?.id ? "page" : undefined}
                      className={cx("-mx-3 flex items-center gap-2.5 rounded-md px-3 py-3 hover:bg-hover", j.id === job?.id && "bg-hover")}
                    >
                      <StatusIcon status={j.status} />
                      <span className="flex-1 text-base font-medium">{j.name}</span>
                      <span className="num text-sm text-ink-3">{j.finishedAt ? duration(between(j.startedAt, j.finishedAt)) : j.status}</span>
                    </Link>
                  </li>
                ))}
              </ul>
            </Panel>
            {run.annotations.length ? (
              <Panel title="Annotations" id="ann-title">
                <ul className="divide-y divide-line">
                  {run.annotations.map((a, i) => (
                    <li key={i} className="py-3 text-base">
                      <p className="flex items-center gap-2">
                        <Badge tone={a.level === "failure" ? "bad" : a.level === "warning" ? "warn" : "neutral"}>{a.level}</Badge>
                        <span className="font-medium">{a.title}</span>
                      </p>
                      <Link href={repoHref(fullName, "code?path=" + encodeURIComponent(a.path) + "#L" + a.line)} className="num mt-1 block text-sm text-accent-ink hover:underline">
                        {a.path}:{a.line}
                      </Link>
                      <p className="mt-1 whitespace-pre-wrap text-ink-2">{a.message}</p>
                    </li>
                  ))}
                </ul>
              </Panel>
            ) : null}
          </nav>
          {job ? (
            <section aria-label={"Job " + job.name} className="flex min-w-0 flex-col gap-5">
              <Panel title={"Steps · " + job.name} id="steps-title" actions={<span className="text-sm text-ink-3">{job.runner}</span>}>
                <ol className="divide-y divide-line">
                  {job.steps.map((s, i) => (
                    <li key={i} className="flex items-center gap-2.5 py-2.5 text-base">
                      <StatusIcon status={s.status} />
                      <span className="flex-1">{s.name}</span>
                      <span className="num text-sm text-ink-3">{Number(s.durationMs) ? duration(Number(s.durationMs)) : s.status}</span>
                    </li>
                  ))}
                </ol>
              </Panel>
              <div className="overflow-hidden rounded-lg border border-line bg-surface">
                <LogViewer key={job.id + ":" + run.attempt} repo={fullName} runId={id} jobId={job.id} running={job.status === "running"} />
              </div>
            </section>
          ) : null}
        </div>
      </>
    );
  });
}
