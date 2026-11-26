import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { requestMode } from "@/app/request-mode";
import { repoFreeze, safetyAttention, safetyRecords, samplePeriod } from "@/components/safety/data";
import { RUN, SWARM, safetyHref } from "@/components/safety/model";
import { RepositorySafety } from "@/components/safety/repository";
import { canManageRepo, canWriteRepo, currentPrincipal, loadRepo, scopeFromParams, serverClient, type RepoParams } from "@/lib/api/server";
import { guard, param, type SearchParams } from "@/lib/page";
import { SafetyRecordPage } from "./record";

export const metadata: Metadata = { title: "Safety" };

export default async function SafetyPage({ params, searchParams }: { params: Promise<RepoParams>; searchParams: SearchParams }) {
  const [{ fullName, ref, owner }, query] = await Promise.all([scopeFromParams(params), searchParams]);
  const id = param(query, "work");
  const kind = param(query, "kind");
  if (id || kind) {
    if (!id || id.includes("/") || (kind !== RUN && kind !== SWARM)) notFound();
    return SafetyRecordPage({ fullName, owner, work: { repo: ref, id, kind } });
  }
  return guard(async () => {
    const [api, me, repo, { mode }] = await Promise.all([serverClient(), currentPrincipal(), loadRepo(fullName), requestMode()]);
    if (!repo) notFound();
    const person = me?.kind === "PRINCIPAL_KIND_HUMAN";
    const [writable, admin, batch, attention, frozen] = await Promise.all([
      person ? canWriteRepo(fullName) : false,
      person ? canManageRepo(fullName) : false,
      safetyRecords(api, ref, param(query, "page") || undefined),
      person ? safetyAttention(api, ref) : [],
      repoFreeze(api, owner, repo.orgId, ref),
    ]);
    const base = safetyHref(fullName);
    const sampleEnd = param(query, "sample");
    const period = samplePeriod(sampleEnd);
    const sample = writable && period ? (await api.safety.spotCheck({ repo: ref, period, seed: sampleEnd, limit: 5 })).records : undefined;
    return <RepositorySafety repo={fullName} repoId={repo.id} mode={mode} records={batch.records} attention={attention}
      frozen={frozen} person={person} canSample={writable} canDecideAll={admin} deciderId={writable ? me?.id : undefined}
      sample={sample} sampleHref={writable ? base + "?sample=" + Date.now() + "#spot-check" : undefined}
      next={batch.cursor ? base + "?" + new URLSearchParams({ page: batch.cursor }) : undefined} />;
  });
}
