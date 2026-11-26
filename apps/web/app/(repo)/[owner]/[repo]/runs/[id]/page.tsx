import Link from "next/link";
import type { Metadata } from "next";
import type { ReactNode } from "react";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { principalLabel } from "@/components/domain";
import { LiveRefresh } from "@/components/live-refresh";
import { RunMenu } from "@/components/runs/run-menu";
import { Kv, Mark, Sect } from "@/components/parts";
import { RunActions } from "@/components/runs/run-actions";
import { CommitLink, optionalCi, pullRequestLabel, runDuration, startedBy } from "@/components/runs/run-facts";
import { AnnotationLine, JobLine, RunJobs, StepLine } from "@/components/runs/run-jobs";
import { WhatHappened } from "@/components/runs/what-happened";
import { Time } from "@/components/time";
import { CiHeader } from "@/components/runs/ci-header";
import { type RepoParams, scopeFromParams, serverClient, sessionToken } from "@/lib/api/server";
import type { Op } from "@/lib/copyas";
import { param, guard, type SearchParams } from "@/lib/page";
import { answerableFor, directoryOf } from "@/lib/pulls";
import { repoHref } from "@/lib/routes";
import { runSummary } from "@/lib/runsummary";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../../request-mode";

export const metadata: Metadata = { title: "CI run" };

export default async function RunPage({ params, searchParams }: { params: Promise<RepoParams & { id: string }>; searchParams: SearchParams }) {
  const { id } = await params;
  const selected = param(await searchParams, "job");
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName, owner } = await scopeFromParams(params);
    const request = { repo: ref, id };
    const [{ mode }, session, run] = await Promise.all([requestMode(), sessionToken(), api.workflow.getRun(request)]);
    const change = run.changeRequestId ? await optionalCi(api.change.getChange({ repo: ref, id: run.changeRequestId })) : undefined;
    const agent = [run.actor, run.triggeringActor, change?.author].find((p) => p?.kind === "PRINCIPAL_KIND_AGENT");
    const members = session && agent?.ownerId ? await optionalCi(api.org.listMembers({ org: owner })) : undefined;
    const person = answerableFor(agent, directoryOf(members?.members.map((m) => m.principal) ?? []));
    const getOp = { service: "WorkflowService" as const, method: "GetRun", request, label: "Show this run" };
    const summary = runSummary(run, Date.now());
    const title = run.workflow + " #" + run.number;
    const pull = pullRequestLabel(run, change);
    const pullHref = run.changeRequestId && repoHref(fullName, "changes/" + run.changeRequestId);
    const start = run.startedAt ?? run.createdAt;
    const elapsed = runDuration(run);
    const starter = startedBy(run);
    const back = repoHref(fullName, "runs");
    const active = !run.finishedAt && ["running", "queued", "waiting", "protection"].includes(run.status);
    const facts: [string, ReactNode][] = [];
    if (pull) facts.push(["Pull request", pullHref ? <Link className="link inline-flex min-h-6 items-center" href={pullHref}>{pull}</Link> : pull]);
    if (run.ref) facts.push(["Branch", <code key="branch" className="mono">{run.ref}</code>]);
    if (run.head) facts.push(["Commit", <CommitLink key="commit" repo={fullName} commit={run.head} />]);
    if (agent) facts.push(["Agent", <Link key="agent" className="link inline-flex min-h-6 items-center" href={repoHref(fullName, "agents/" + agent.id)}>{principalLabel(agent)}</Link>]);
    if (person) facts.push(["Answers for the agent", <Mark key="person">{principalLabel(person)}</Mark>]);
    if (starter) facts.push(["Started by", starter]);
    if (start) facts.push([run.startedAt ? "Started" : "Created", mode === "bee" ? <time dateTime={start}>{start}</time> : <Time iso={start} />]);
    if (elapsed) facts.push(["Duration", elapsed]);
    if (run.attempt > 1) facts.push(["Attempt", "attempt " + run.attempt]);

    if (mode === "bee") {
      const groups: FactGroup[] = [
        { title: "What happened", lines: [summary.sentence, ...(summary.detail ? [summary.detail] : [])] },
        { title: "Jobs", lines: run.jobs.map((job) => <JobLine key={job.id} job={job} />), empty: "No jobs have been reported for this run." },
        ...run.jobs.map((job): FactGroup => ({
          title: "Job · " + job.name + (run.jobs.filter((j) => j.name === job.name).length > 1 ? " · " + job.id : ""),
          facts: [{ label: "Job ID", value: <code>{job.id}</code> }, job.runner ? { label: "Runner", value: job.runner } : null],
          lines: [
            ...job.steps.map((step, i) => <StepLine key={i} step={step} />),
            ...run.annotations.filter((a) => a.jobId === job.id).map((a, i) => <AnnotationLine key={"annotation-" + i} annotation={a} repo={fullName} />),
          ],
        })),
        { title: "Run details", facts: facts.map(([label, value]) => ({ label, value })) },
        { title: "Related pages", lines: [<Link key="ci" href={back} className="link">CI</Link>] },
      ];
      const orphaned = run.annotations.filter((a) => !run.jobs.some((job) => job.id === a.jobId));
      if (orphaned.length) groups.splice(-2, 0, { title: "Annotations", lines: orphaned.map((a, i) => <AnnotationLine key={i} annotation={a} repo={fullName} />) });
      return <PageFacts page="run" title={title} groups={groups} tools={pageTools("run")} />;
    }

    const alternatives: { key: string; op: Op; label: string }[] = run.jobs.map((job) => ({
      key: job.id,
      op: { service: "WorkflowService", method: "StreamLogs", request: { repo: ref, runId: id, jobId: job.id }, label: "Stream this log" },
      label: "Copy as · " + job.name + " log",
    }));
    if (session && !active) alternatives.push({
      key: "rerun",
      op: { service: "WorkflowService", method: "Rerun", request: { ...request, failedOnly: run.jobs.some((job) => job.status === "failed") }, label: "Re-run this workflow" },
      label: "Copy as · re-run",
    });
    return (
      <>
        <LiveRefresh repo={fullName} kinds={["workflow_run.*", "workflow_job.*", "run.*"]} />
        <CiHeader title={title} back={back} actions={<RunMenu op={getOp} alternatives={alternatives} />} />
        <div className="flex min-w-0 flex-col gap-10">
          <WhatHappened summary={summary} />
          {session && <RunActions repo={fullName} runId={id} hasFailed={run.jobs.some((j) => j.status === "failed")} active={active} />}
          <RunJobs run={run} repo={fullName} selected={selected} />
          {facts.length > 0 && <Sect id="ci-run-details" title="Run details"><Kv rows={facts} /></Sect>}
        </div>
      </>
    );
  });
}
