import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader, Panel, Stat } from "@/components/ui";
import { objectIdText } from "@/lib/api/ids";
import { type RepoParams, scopeFromParams, serverClient } from "@/lib/api/server";
import type { ObjectId } from "@/lib/api/types";
import { duration, principalOf } from "@/lib/format";
import { guard } from "@/lib/page";

export const metadata: Metadata = { title: "Pages" };

const words = (v: string | undefined, prefix: string) => (v ?? "").replace(prefix, "").replace(/_/g, " ").toLowerCase();
const TONE: Record<string, "ok" | "bad" | "warn" | "neutral"> = { succeeded: "ok", built: "ok", failed: "bad", errored: "bad", building: "warn", deploying: "warn", queued: "neutral" };

export default async function PagesPage({ params }: { params: Promise<RepoParams> }) {
  return guard(async () => {
    const api = await serverClient();
    const { ref, fullName } = await scopeFromParams(params);
    const req = { repo: ref };
    const { site, deployments } = await api.pages.getPages(req);
    const status = words(site?.status, "PAGES_SITE_STATUS_");
    return (
      <>
        <PageHeader title="Pages" lede={"The static site " + fullName + " publishes, and every deployment of it."} actions={<PageMenu op={{ service: "PagesService", method: "GetPages", request: req, label: "Get the Pages site" }} />} />
        {!site ? (
          <EmptyState title="No Pages site for this repo" icon="pages">
            Create one from a folder of a ref with <code className="mono">bee api PagesService/CreatePagesSite</code>, or deploy a workflow artifact.
          </EmptyState>
        ) : (
          <div className="flex flex-col gap-10">
            <div className="grid grid-cols-2 gap-x-6 gap-y-5 sm:grid-cols-4" data-testid="pages-site">
              <Stat label="Status" value={<Badge tone={TONE[status] ?? "neutral"}>{status || "unknown"}</Badge>} />
              <Stat label="Source" value={<span className="mono text-base">{site.buildType === "PAGES_BUILD_TYPE_WORKFLOW" ? "workflow artifact" : (site.source?.ref ?? "") + ":" + (site.source?.path ?? "/")}</span>} />
              <Stat label="Access" value={<span className="text-base">{words(site.access, "PAGES_ACCESS_") || "public"}</span>} />
              <Stat label="HTTPS" value={<span className="text-base">{site.enforceHttps ? "enforced" : "not enforced"}</span>} />
            </div>
            <Panel title="Site" id="pages-site-title">
              <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
                <dt className="text-ink-3">URL</dt>
                <dd className="mono [overflow-wrap:anywhere]">{site.url || "not assigned yet"}</dd>
                <dt className="text-ink-3">Custom domain</dt>
                <dd className="mono">{site.customDomain || "none"}</dd>
                <dt className="text-ink-3">Serving</dt>
                <dd className="mono">{site.servedTree ? objectIdText(site.servedTree as ObjectId, true) : site.currentDeploymentId || "nothing deployed yet"}</dd>
                <dt className="text-ink-3">Created</dt>
                <dd>
                  <Time iso={site.createdAt} />
                </dd>
              </dl>
            </Panel>
            <Panel title="Deployments" id="pages-deploys-title">
              {deployments.length === 0 ? (
                <EmptyState title="No deployments yet" icon="pages">
                  A deployment appears here when the source ref moves or a workflow deploys an artifact.
                </EmptyState>
              ) : (
                <ul className="divide-y divide-line">
                  {deployments.map((d) => {
                    const state = words(d.state, "PAGES_DEPLOYMENT_STATE_");
                    return (
                      <li key={d.id} className="flex flex-col gap-1 py-3 md:flex-row md:items-center md:gap-4">
                        <Badge tone={TONE[state] ?? "neutral"}>{state}</Badge>
                        <span className="mono text-base">{d.ref || (d.changeset ? objectIdText(d.changeset as ObjectId, true) : d.artifactId)}</span>
                        {d.errorMessage ? <span className="text-base text-bad">{d.errorMessage}</span> : null}
                        <span className="flex-1" />
                        {d.creator ? <PrincipalName principal={principalOf(d.creator)} /> : null}
                        {d.durationMs ? <span className="text-sm text-ink-3">{duration(Number(d.durationMs))}</span> : null}
                        <Time iso={d.createdAt} />
                      </li>
                    );
                  })}
                </ul>
              )}
            </Panel>
          </div>
        )}
      </>
    );
  });
}
