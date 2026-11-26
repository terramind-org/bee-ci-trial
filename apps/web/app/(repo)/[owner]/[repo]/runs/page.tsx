import type { Metadata } from "next";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { Button, ButtonLink, Field, Input, Select } from "@/components/kit";
import { LiveRefresh } from "@/components/live-refresh";
import { PageMenu } from "@/components/menu";
import { Quiet, Row, Rows, Sep } from "@/components/parts";
import { optionalCi } from "@/components/runs/run-facts";
import { deploymentParts, groupRuns, passingLabel, RunLine, RunList, runMatches } from "@/components/runs/run-list";
import { CiHeader } from "@/components/runs/ci-header";
import { mapLimit } from "@/lib/api/ops/shared";
import { type RepoParams, scopeFromParams, serverClient, sessionToken } from "@/lib/api/server";
import type { ListRunsRequest, RunStatus } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../request-mode";

export const metadata: Metadata = { title: "CI" };

export default async function RunsPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const sp = await searchParams;
  const rawStatus = param(sp, "status");
  const status = rawStatus === "failed" || rawStatus === "passed" || rawStatus === "running" ? rawStatus : undefined;
  const token = param(sp, "page");
  const query = param(sp, "q").trim();
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req: ListRunsRequest = { repo: ref, status, page: { pageSize: 100, pageToken: token || undefined } };
    const [{ mode }, session, list, changes, deployments] = await Promise.all([
      requestMode(),
      sessionToken(),
      api.workflow.listRuns(req),
      api.change.listChanges({ repo: ref, page: { pageSize: 100 } }),
      optionalCi(api.deployment.listDeployments({ repo: ref, page: { pageSize: 25 } })),
    ]);

    const byId = new Map(changes.changes.map((c) => [c.id, c]));
    const missing = [...new Set(list.runs.map((r) => r.changeRequestId).filter((id) => id && !byId.has(id)))];
    for (const change of await mapLimit(missing, 8, (id) => optionalCi(api.change.getChange({ repo: ref, id })))) {
      if (change) byId.set(change.id, change);
    }
    const runs = query ? list.runs.filter((r) => runMatches(r, byId.get(r.changeRequestId), query)) : list.runs;
    const now = Date.now();
    const scoped = Boolean(query || status || token || list.page?.nextPageToken);
    const empty = query || status ? "No runs match these filters." : "No runs yet.";
    const path = repoHref(fullName, "runs");
    const workflows = session ? path + "/workflows" : undefined;
    const pageURL = (page: string, filter?: RunStatus) => {
      const next = new URLSearchParams();
      if (query) next.set("q", query);
      if (filter) next.set("status", filter);
      if (page) next.set("page", page);
      return path + (next.size ? "?" + next.toString() : "");
    };
    const next = list.page?.nextPageToken;
    const deploymentRows = deployments?.deployments ?? [];

    if (mode === "bee") {
      const { groups, passed, recent, cancelled, other } = groupRuns(runs, now);
      const lines = (items: typeof runs) => items.map((run) => (
        <RunLine key={run.id} repo={fullName} run={run} change={byId.get(run.changeRequestId)} agent />
      ));
      const facts: FactGroup[] = groups.filter((g) => g.runs.length).map((g) => ({ title: g.title, lines: lines(g.runs) }));
      if (runs.length) facts.push({ title: passingLabel(recent.length, passed.length, scoped), collapsed: true, lines: lines(passed), empty: "No passing runs in these results." });
      else facts.push({ title: "Runs", empty });
      if (cancelled.length) facts.push({ title: "Cancelled", lines: lines(cancelled) });
      if (other.length) facts.push({ title: "Other completed runs", lines: lines(other) });
      if (deploymentRows.length) facts.push({ title: "Deployments", lines: deploymentRows.map((d) => {
        const { target, state } = deploymentParts(d);
        return <span key={d.id}>{d.environment}{target && <> · <code>{target}</code></>} · {state}</span>;
      }) });
      if (workflows || next || token || status || query) facts.push({ title: "Related pages", lines: [
        ...(workflows ? [<a key="workflows" className="link" href={workflows}>Workflows</a>] : []),
        ...(next ? [<a key="next" className="link" href={pageURL(next, status)}>Older runs</a>] : []),
        ...(token || status || query ? [<a key="latest" className="link" href={path}>Latest runs</a>] : []),
      ] });
      return <PageFacts page="ci" title="CI" groups={facts} tools={pageTools("ci")} />;
    }

    return (
      <>
        <LiveRefresh repo={fullName} kinds={["workflow_run.*", "workflow_job.*", "deployment.*", "run.*"]} />
        <CiHeader title="CI" actions={<>
          {workflows && <ButtonLink href={workflows} size="sm">Workflows</ButtonLink>}
          <PageMenu op={{ service: "WorkflowService", method: "ListRuns", request: req, label: "List runs" }} />
        </>} />
        <div className="flex flex-col gap-8">
          <details className="max-w-2xl" open={Boolean(query || status)}>
            <summary className="link min-h-6 cursor-pointer text-sm text-ink-2">Find a run</summary>
            <form action={path} role="search" aria-label="Find a run" className="mt-2 grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto_auto] sm:items-end">
              <Field label="Workflow, branch, pull request or actor" htmlFor="run-search">
                <Input id="run-search" name="q" defaultValue={query} />
              </Field>
              <Field label="Status" htmlFor="run-status">
                <Select id="run-status" name="status" defaultValue={status ?? ""}>
                  <option value="">All runs</option>
                  <option value="failed">Failed</option>
                  <option value="running">Running</option>
                  <option value="passed">Passed</option>
                </Select>
              </Field>
              <Button type="submit">Apply filters</Button>
            </form>
          </details>
          {runs.length ? <RunList repo={fullName} runs={runs} changes={byId} now={now} scoped={scoped} /> : <Quiet>{empty}</Quiet>}
          {(next || token || status || query) && (
            <nav aria-label="Run pages" className="flex flex-wrap gap-2">
              {next && <ButtonLink href={pageURL(next, status)} size="sm">Older runs</ButtonLink>}
              {(token || status || query) && <ButtonLink href={path} size="sm" variant="ghost">Latest runs</ButtonLink>}
            </nav>
          )}
          {deploymentRows.length > 0 && (
            <section aria-labelledby="ci-deployments">
              <h2 id="ci-deployments" className="mb-2 text-sm font-medium text-ink-2">Deployments</h2>
              <Rows>
                {deploymentRows.map((d) => {
                  const { target, state } = deploymentParts(d);
                  return <Row key={d.id} meta={<>{state}{target && <><Sep /> <span className="mono">{target}</span></>}</>}>{d.environment}</Row>;
                })}
              </Rows>
            </section>
          )}
        </div>
      </>
    );
  });
}
