import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { PrincipalName } from "@/components/domain";
import { Badge, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Members" };

export default async function MembersPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const api = await serverClient();
    const org = decodeURIComponent((await params).org);
    const req = { org };
    const [members, agents] = await Promise.all([api.org.listMembers(req), api.agent.listAgents({ org })]);
    return (
      <>
        <PageHeader title="Settings" lede={<>People in {org} and the agents each of them owns.</>} actions={<PageMenu op={{ service: "OrgService", method: "ListMembers", request: req, label: "List members" }} />} />
        <SettingsTabs org={org} active="members" />
        <div>
          <ul className="divide-y divide-line">
            {members.members.map((m) => {
              const owned = agents.agents.filter((a) => a.owner?.id === m.principal?.id);
              return (
                <li key={m.principal?.id} className="flex flex-col gap-1.5 py-3 md:flex-row md:items-center md:gap-4">
                  <div className="flex-1">
                    <PrincipalName principal={m.principal} />
                    <span className="mono ml-2 text-sm text-ink-3">{m.principal?.externalIds.join(", ")}</span>
                  </div>
                  <div className="flex flex-wrap items-center gap-1.5 text-base text-ink-2">
                    {owned.length ? <span>owns {owned.map((a) => a.principal.display).join(", ")}</span> : <span className="text-ink-3">no agents</span>}
                  </div>
                  <Badge tone={m.role === "ROLE_OWNER" ? "accent" : "neutral"}>{m.role.replace("ROLE_", "").toLowerCase()}</Badge>
                </li>
              );
            })}
          </ul>
        </div>
      </>
    );
  });
}
