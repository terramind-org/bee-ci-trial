import type { Metadata } from "next";
import { requestMode } from "@/app/request-mode";
import { orgMembers } from "@/components/org/people-data";
import { auditFreeze, eventFreeze, unavailableToViewer } from "@/components/safety/data";
import { recentPeriod } from "@/components/safety/model";
import { OrganisationSafety } from "@/components/safety/organisation";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { BeeError } from "@/lib/api/transport";
import { guard } from "@/lib/page";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Safety" };

async function optional<T>(request: Promise<T>) {
  try {
    return await request;
  } catch (error) {
    if (!unavailableToViewer(error)) throw error;
    return undefined;
  }
}

export default async function OrgSafetyPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const org = (await params).org;
    const [api, me, { mode }] = await Promise.all([serverClient(), currentPrincipal(), requestMode()]);
    const identity = await api.org.getOrg({ nameOrId: org });
    const [read, members, freeze] = await Promise.all([
      api.safety.getSafetySummary({ org, period: recentPeriod() }).then(
        (summary) => ({ summary, reason: undefined }),
        (error: unknown) => {
          if (!(error instanceof BeeError) || !unavailableToViewer(error)) throw error;
          return { summary: undefined, reason: "The organisation summary requires audit access for a person. " + error.reason };
        },
      ),
      mode === "keeper" && me?.kind === "PRINCIPAL_KIND_HUMAN" ? optional(orgMembers(api, org)) : undefined,
      optional(auditFreeze(api, org, identity.id).then((frozen) => ({ frozen }))),
    ]);
    const repoIds = [...new Set((read.summary?.tripwires ?? []).flatMap((item) => item.work?.repo.id ? [item.work.repo.id] : []))];
    const [directory, tripwireRepos] = await Promise.all([
      read.summary?.work.length ? optional(api.agent.listAgents({ org })) : undefined,
      Promise.all(repoIds.map((id) => optional(api.repo.getRepo({ repo: { id } })))),
    ]);
    const agents = Object.fromEntries((directory?.agents ?? []).map(({ principal }) => [principal.id, principal.display || principal.id]));
    const repos = Object.fromEntries(tripwireRepos.flatMap((repo) => repo?.fullName ? [[repo.id, repo.fullName]] : []));
    const canStop = !!freeze && !!members?.some((member) => member.principal?.id === me?.id && member.role === "ROLE_OWNER");
    return <OrganisationSafety org={org} mode={mode} summary={read.summary} summaryMissing={read.reason} agents={agents} repos={repos}
      frozen={freeze?.frozen ?? (!freeze ? await eventFreeze(api, identity.id, { fullName: org + "/*" }) : undefined)} canStop={canStop} tabs={mode === "keeper" ? <SettingsTabs org={org} active="safety" /> : undefined} />;
  });
}
