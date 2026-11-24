import Link from "next/link";
import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { LiveRefresh } from "@/components/live-refresh";
import { PrincipalName, RunStatusBadge, StatusIcon } from "@/components/domain";
import { FilterList } from "@/components/filter-list";
import { Time } from "@/components/time";
import { EmptyState, PageHeader, TabLinks } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import type { RunStatus } from "@/lib/api/types";
import { between, duration } from "@/lib/format";
import { repoHref } from "@/lib/routes";
import { guard, param, type SearchParams } from "@/lib/page";

export const metadata: Metadata = { title: "Runs" };

const TABS: (RunStatus | "")[] = ["", "running", "failed", "passed"];

export default async function RunsPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const status = (TABS.includes(param(sp, "status") as RunStatus) ? param(sp, "status") : "") as RunStatus | "";
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req = { repo: ref, status, page: { pageSize: 100 } };
    const [list, all, changes] = await Promise.all([api.workflow.listRuns(req), api.workflow.listRuns({ repo: ref, page: { pageSize: 200 } }), api.change.listChanges({ repo: ref })]);
    const titles = new Map(changes.changes.map((c) => [c.id, c.title]));
    const items = list.runs.map((r) => ({
      key: r.id,
      text: [r.workflow, r.number, r.status, r.ref, titles.get(r.changeRequestId), r.actor?.display, r.trigger].join(" "),
      node: (
        <Link href={repoHref(fullName, "runs/" + r.id)} data-nav-item className="flex flex-col gap-1.5 px-3.5 py-2.5 hover:bg-hover focus-visible:bg-hover md:flex-row md:items-center md:gap-4">
          <div className="flex min-w-0 flex-1 items-center gap-3">
            <RunStatusBadge status={r.status} />
            <div className="min-w-0">
              <p className="truncate font-medium text-ink">
                {r.workflow} <span className="num text-ink-3">#{r.number}</span> · {titles.get(r.changeRequestId) ?? (r.trigger === "land" ? "post-land on main" : r.ref)}
              </p>
              <p className="flex flex-wrap items-center gap-x-3 text-sm text-ink-2">
                <span className="mono text-xs">{r.ref}</span>
                <span className="mono text-xs text-ink-3">{objectIdText(r.head, true)}</span>
                <span>{r.trigger.replace("_", " ")}</span>
                {r.attempt > 1 ? <span>attempt {r.attempt}</span> : null}
              </p>
            </div>
          </div>
          <div className="flex items-center gap-4 text-sm text-ink-2">
            <span className="inline-flex gap-0.5" aria-label={r.jobs.map((j) => j.name + " " + j.status).join(", ")}>
              {r.jobs.map((j) => (
                <StatusIcon key={j.id} status={j.status} />
              ))}
            </span>
            <PrincipalName principal={r.actor} showRuntime={false} link={false} />
            <span className="num w-16 text-right">{r.finishedAt ? duration(between(r.startedAt, r.finishedAt)) : "running"}</span>
            <Time iso={r.startedAt} className="w-16 text-right text-ink-3" />
          </div>
        </Link>
      ),
    }));
    const n = (s: RunStatus) => all.runs.filter((r) => r.status === s).length;
    return (
      <>
        <PageOp op={{ service: "WorkflowService", method: "ListRuns", request: req, label: "List runs" }} />
        <LiveRefresh repo={fullName} kinds={["workflow_run.*"]} />
        <PageHeader title="Runs" meta={<span>Workflow runs, their jobs and the checks they report.</span>} actions={<CopyAs op={{ service: "WorkflowService", method: "ListRuns", request: req, label: "List runs" }} />} />
        <TabLinks
          label="Run status"
          items={TABS.map((t) => ({ href: repoHref(fullName, t ? "runs?status=" + t : "runs"), label: t ? t[0]?.toUpperCase() + t.slice(1) : "All", active: status === t, count: t ? n(t) : all.runs.length }))}
        />
        <div className="rounded-lg border border-line bg-surface">
          <FilterList items={items} label="Filter runs" placeholder="Filter by workflow, change, ref or actor…" empty={<EmptyState title="No runs" icon="runs">Runs start when a change request opens or a change lands.</EmptyState>} />
        </div>
      </>
    );
  });
}
