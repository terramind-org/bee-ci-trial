import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { PageMenu } from "@/components/menu";
import { humanOwner } from "@/components/org/agents-data";
import { InviteMember, ManageMember } from "@/components/org/member-actions";
import { orgMembers, orgTeams } from "@/components/org/people-data";
import { roleName } from "@/components/org/roles";
import { EmptyState, PageHeader } from "@/components/ui";
import { currentPrincipal, serverClient } from "@/lib/api/server";
import { plural } from "@/lib/format";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { requestMode } from "@/app/request-mode";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "People and teams" };

export default async function MembersPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const api = await serverClient();
    const org = decodeURIComponent((await params).org);
    const [members, { agents }, teams, viewer, { mode }] = await Promise.all([
      orgMembers(api, org), api.agent.listAgents({ org }), orgTeams(api, org), currentPrincipal(), requestMode(),
    ]);
    const callerRole = members.find((member) => member.principal?.id === viewer?.id)?.role;
    const owner = callerRole === "ROLE_OWNER";
    const canManage = viewer?.kind === "PRINCIPAL_KIND_HUMAN" && (owner || callerRole === "ROLE_ADMIN");
    const agentDirectory = new Map(agents.map((agent) => [agent.principal.id, agent]));
    const ownerCounts = new Map<string, number>();
    for (const agent of agents) {
      const ownerId = humanOwner(agent, agentDirectory)?.id ?? agent.principal.ownerId;
      if (ownerId) ownerCounts.set(ownerId, (ownerCounts.get(ownerId) ?? 0) + 1);
    }
    const rows = members.flatMap(({ principal, role }) => {
      if (!principal || principal.kind !== "PRINCIPAL_KIND_HUMAN") return [];
      const name = principal.display || principal.id;
      const memberships = teams.filter(({ members }) => members.some((m) => m.principal?.id === principal.id)).map(({ team }) => team.name || team.slug);
      const count = ownerCounts.get(principal.id) ?? 0;
      const detail = count === 1 ? "1 agent answers to " + name : plural(count, "agent") + " answer to " + name;
      return [{ id: principal.id, name, role, memberships, detail }];
    });

    if (mode === "bee") {
      return <PageFacts page="org-members" title={org + " · people and teams"} groups={[{
        title: "people",
        lines: rows.map((row) => [row.name, roleName(row.role), row.memberships.length ? "Teams " + row.memberships.join(", ") : "No listed teams", row.detail].join(" · ")),
        empty: "No people in this organisation.",
      }]} tools={[
        toolFor("OrgService/ListMembers", "List organisation members and their roles"),
        toolFor("TeamService/ListTeams", "List teams in this organisation"),
        toolFor("TeamService/ListTeamMembers", "List people in a team"),
        toolFor("AgentService/ListAgents", "Find the agents each person answers for"),
        toolFor("OrgService/CreateOrgInvitation", "Invite someone to the organisation"),
        toolFor("OrgService/SetMember", "Change a person's role or remove their membership"),
      ]} />;
    }

    return (
      <>
        <PageHeader eyebrow={org} title="People and teams" actions={<PageMenu op={{ service: "OrgService", method: "ListMembers", request: { org }, label: "List members" }} />} />
        <SettingsTabs org={org} active="members" />
        {rows.length ? (
          <ul aria-label="People" className="divide-y divide-line border-t border-line" data-testid="org-people">
            {rows.map((row) => (
              <li key={row.id} className="grid items-start gap-3 py-4 lg:grid-cols-[minmax(0,1fr)_auto]">
                <div className="min-w-0 [overflow-wrap:anywhere]">
                  <h2 className="text-lg font-medium">{row.name}</h2>
                  <p className="mt-1 text-base text-ink-2">{roleName(row.role)} · {row.memberships.join(", ") || "No listed teams"}</p>
                  <p className="text-base text-ink-2">{row.detail}.</p>
                </div>
                {canManage && (owner || row.role !== "ROLE_OWNER") ? (
                  <ManageMember key={row.role} org={org} principalId={row.id} role={row.role} name={row.name} canGrantOwner={owner} />
                ) : null}
              </li>
            ))}
          </ul>
        ) : <EmptyState title="No people in this organisation" />}
        {canManage ? <InviteMember org={org} canGrantOwner={owner} /> : null}
      </>
    );
  });
}
