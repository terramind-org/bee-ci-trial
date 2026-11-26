import { notFound } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { repoFreeze, unavailableToViewer } from "@/components/safety/data";
import { RecordSafety } from "@/components/safety/record";
import { canManageRepo, canWriteRepo, currentPrincipal, loadRepo, serverClient } from "@/lib/api/server";
import type { SafetyRef } from "@/lib/api/types";
import { guard } from "@/lib/page";

export async function SafetyRecordPage({ fullName, owner, work }: { fullName: string; owner: string; work: SafetyRef }) {
  return guard(async () => {
    const [api, me, repo, { mode }] = await Promise.all([serverClient(), currentPrincipal(), loadRepo(fullName), requestMode()]);
    if (!repo) notFound();
    const [record, frozen] = await Promise.all([
      api.safety.getSafetyRecord(work),
      repoFreeze(api, owner, repo.orgId, work.repo),
    ]);
    const canDecide = mode === "keeper" && me?.kind === "PRINCIPAL_KIND_HUMAN" &&
      (me.id === record.answerable?.id ? await canWriteRepo(fullName) : await canManageRepo(fullName));
    const reviewers = canDecide ? await api.agent.listAgents({ org: owner, repo: work.repo }).then(
      ({ agents }) => agents.filter(({ principal }) => principal.kind === "PRINCIPAL_KIND_AGENT" && !record.agents.some((agent) => agent.id === principal.id))
        .map(({ principal }) => ({ id: principal.id, label: principal.display || principal.id }))
        .sort((a, b) => a.label.localeCompare(b.label)),
      (error: unknown) => {
        if (!unavailableToViewer(error)) throw error;
        return [];
      },
    ) : undefined;
    return <RecordSafety key={record.work?.id ?? work.id} repo={fullName} repoId={repo.id} record={record}
      work={record.work ?? work} mode={mode} frozen={frozen} canDecide={canDecide} reviewers={reviewers} />;
  });
}
