import type { Metadata } from "next";
import { CopyAs, PageOp } from "@/components/copy-as";
import { StatusList, StatusDot, healthOf, uptimeSince, worse, type StatusItem } from "@/components/admin/status-list";
import { Badge, Panel, Stat } from "@/components/ui";
import type { ClusterStatus, OpsHealth } from "@/lib/api/types";
import { Failed, serverClient, serverEdition, settle } from "./load";

export const metadata: Metadata = { title: "Admin" };
export const dynamic = "force-dynamic";

const healthOp = { service: "OpsService", method: "GetOpsHealth", request: { includeComponents: true }, label: "Show server health" } as const;
const clusterOp = { service: "OpsService", method: "GetClusterStatus", request: {}, label: "Show cluster status" } as const;

function components(h: OpsHealth): StatusItem[] {
  return h.components.map((c) => ({ name: c.name, status: worse(healthOf(c.liveness), healthOf(c.readiness)), detail: c.detail, reason: c.reason, critical: c.criticalPath }));
}

function cells(c: ClusterStatus): StatusItem[] {
  const out: StatusItem[] = c.cells.map((x) => ({
    name: x.name,
    status: healthOf(x.status),
    detail: `${x.region} · ${x.nodes} ${x.nodes === 1 ? "node" : "nodes"} · ${x.orgs} ${x.orgs === 1 ? "org" : "orgs"} · v${x.version}${x.servingStaleReads ? ` · serving stale reads, ${x.staleLagMs} ms behind` : ""}`,
    reason: x.metadataPrimaryUp ? "" : "metadata_primary_down",
  }));
  out.push({ name: "directory", status: c.directoryUp ? "ok" : "down", detail: c.directoryVersion });
  return out;
}

export default async function AdminOverview() {
  const api = await serverClient();
  const [health, cluster, config, edition] = await Promise.all([
    settle(api.ops.getOpsHealth(healthOp.request)),
    settle(api.ops.getClusterStatus(clusterOp.request)),
    settle(api.ops.getEffectiveOpsConfig({})),
    serverEdition(),
  ]);
  const here = health.ok && cluster.ok ? cluster.value.cells.find((c) => c.name === health.value.cell) : undefined;
  const started = config.ok && config.value.appliedAt ? Date.parse(config.value.appliedAt) : NaN;
  const stale = cluster.ok ? cluster.value.cells.filter((c) => c.servingStaleReads) : [];

  return (
    <>
      <PageOp op={healthOp} />
      {health.ok ? (
        <div className="mb-10 grid grid-cols-2 gap-x-6 gap-y-5 sm:grid-cols-5">
          <Stat label="Status" value={<span className="inline-flex items-center gap-2"><StatusDot status={worse(healthOf(health.value.liveness), healthOf(health.value.readiness))} />{health.value.readiness === "HEALTH_STATUS_OK" ? "Ready" : health.value.readiness === "HEALTH_STATUS_DEGRADED" ? "Degraded" : "Not ready"}</span>} />
          <Stat label="Version" value={<span className="mono text-md">{health.value.version || "unknown"}</span>} />
          <Stat label="Edition" value={edition === "cloud" ? "Cloud" : "Enterprise"} />
          <Stat label="Uptime" value={Number.isFinite(started) ? uptimeSince(started) : "unknown"} hint={Number.isFinite(started) ? undefined : "The server did not report when it started."} />
          <Stat label="Cell" value={<span className="mono text-md">{health.value.cell || "default"}</span>} hint={here?.region || undefined} />
        </div>
      ) : null}
      {stale.length ? (
        <div role="status" data-testid="stale-reads" className="mb-5 flex items-start gap-2.5 rounded-lg border border-line bg-warn-soft py-3 text-base">
          <Badge tone="warn">Stale reads</Badge>
          <p>
            The metadata outage cache is serving stale reads in {stale.map((c) => `${c.name} (${c.staleLagMs} ms behind)`).join(", ")}. Pushes that need the primary wait until it is back.
          </p>
        </div>
      ) : cluster.ok ? (
        <p className="mb-5 text-base text-ink-3" data-testid="fresh-reads">
          The outage cache is not serving stale reads: every cell reads from its metadata primary.
        </p>
      ) : null}
      <div className="grid grid-cols-1 gap-10 xl:grid-cols-2">
        <Panel title="Components" id="ops-components" actions={<CopyAs compact op={healthOp} />}>
          {health.ok ? <StatusList label="Components" items={components(health.value)} empty="The server reported no components." /> : <Failed what="Server health" error={health.error} />}
        </Panel>
        <Panel title="Cells" id="ops-cells" actions={<CopyAs compact op={clusterOp} />}>
          {cluster.ok ? <StatusList label="Cells" items={cells(cluster.value)} /> : <Failed what="Cluster status" error={cluster.error} />}
        </Panel>
      </div>
    </>
  );
}
