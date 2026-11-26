import Link from "next/link";
import type { Metadata } from "next";
import { PageFacts, type FactGroup } from "@/components/bee/page-facts";
import { ButtonLink } from "@/components/kit";
import { PageMenu } from "@/components/menu";
import { Quiet, Row, Rows, Sep, State, type Shape } from "@/components/parts";
import { WorkflowControls } from "@/components/runs/workflow-controls";
import { CiHeader } from "@/components/runs/ci-header";
import { type RepoParams, scopeFromParams, serverClient, sessionToken } from "@/lib/api/server";
import type { WorkflowDefinition } from "@/lib/api/types";
import { guard, param, type SearchParams } from "@/lib/page";
import { repoHref } from "@/lib/routes";
import { pageTools } from "@/lib/tools";
import { requestMode } from "../../../../../request-mode";

export const metadata: Metadata = { title: "Workflows" };

const STATES: Record<NonNullable<WorkflowDefinition["state"]>, [Shape, string]> = {
  WORKFLOW_STATE_UNSPECIFIED: ["open", "State unavailable"],
  WORKFLOW_STATE_ACTIVE: ["passed", "Enabled"],
  WORKFLOW_STATE_DISABLED_MANUALLY: ["closed", "Disabled"],
  WORKFLOW_STATE_DISABLED_INACTIVITY: ["closed", "Disabled due to inactivity"],
  WORKFLOW_STATE_DISABLED_FORK: ["closed", "Disabled in this fork"],
  WORKFLOW_STATE_DELETED: ["closed", "Removed"],
};

export default async function WorkflowsPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const token = param(await searchParams, "page");
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const request = { repo: ref, page: { pageSize: 100, pageToken: token || undefined } };
    const [{ mode }, session, list] = await Promise.all([requestMode(), sessionToken(), api.workflow.listWorkflows(request)]);
    const ci = repoHref(fullName, "runs");
    const base = ci + "/workflows";
    const next = list.page?.nextPageToken ? base + "?page=" + encodeURIComponent(list.page.nextPageToken) : undefined;
    const state = (workflow: WorkflowDefinition) => {
      const [shape, word] = STATES[workflow.state ?? "WORKFLOW_STATE_UNSPECIFIED"];
      return <State shape={shape}>{word}</State>;
    };

    if (mode === "bee") {
      const groups: FactGroup[] = [
        {
          title: "Workflow definitions",
          lines: list.workflows.map((w) => <span key={w.id}>{w.name}{w.path && <> · <code>{w.path}</code></>} · {state(w)}</span>),
          empty: "No workflows are defined.",
        },
        {
          title: "Related pages",
          lines: [
            <Link key="ci" href={ci} className="link">CI</Link>,
            ...(next ? [<Link key="next" href={next} className="link">More workflows</Link>] : []),
            ...(token ? [<Link key="first" href={base} className="link">First page</Link>] : []),
          ],
        },
      ];
      return <PageFacts page="workflows" title="Workflows" groups={groups} tools={pageTools("workflows")} />;
    }

    return (
      <>
        <CiHeader title="Workflows" back={ci} actions={<PageMenu op={{ service: "WorkflowService", method: "ListWorkflows", request, label: "List workflows" }} />} />
        {list.workflows.length ? (
          <Rows label="Workflow definitions">
            {list.workflows.map((w) => (
              <Row key={w.id} meta={<>{state(w)}{w.path && <><Sep /> <code className="mono">{w.path}</code></>}</>} action={session && w.id ? <WorkflowControls repo={fullName} workflow={w} /> : undefined}>
                {w.name || w.path}
              </Row>
            ))}
          </Rows>
        ) : <Quiet>No workflows are defined.</Quiet>}
        {(next || token) && (
          <nav aria-label="Workflow pages" className="mt-6 flex flex-wrap gap-2">
            {next && <ButtonLink href={next} size="sm">More workflows</ButtonLink>}
            {token && <ButtonLink href={base} size="sm" variant="ghost">First page</ButtonLink>}
          </nav>
        )}
      </>
    );
  });
}
