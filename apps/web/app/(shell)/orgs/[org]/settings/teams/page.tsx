import type { Metadata } from "next";
import { PageFacts } from "@/components/bee/page-facts";
import { PageMenu } from "@/components/menu";
import { orgTeams } from "@/components/org/people-data";
import { EmptyState, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { guard } from "@/lib/page";
import { toolFor } from "@/lib/tools";
import { requestMode } from "@/app/request-mode";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Teams" };

export default async function TeamsPage({ params }: { params: Promise<{ org: string }> }) {
  return guard(async () => {
    const org = decodeURIComponent((await params).org);
    const api = await serverClient();
    const [teams, { mode }] = await Promise.all([orgTeams(api, org), requestMode()]);
    const byId = new Map(teams.map(({ team }) => [team.id, team.name || team.slug]));
    const rows = teams.map(({ team, members }) => {
      const name = team.name || team.slug;
      const names = members.flatMap(({ principal, role, inherited, viaTeamId }) => {
        if (!principal?.id) return [];
        const via = inherited && byId.get(viaTeamId);
        return [(principal.display || principal.id) + (role === "TEAM_MEMBER_ROLE_MAINTAINER" ? " (maintainer)" : "") + (via ? " via " + via : "")];
      });
      return {
        id: team.id,
        name,
        description: team.description,
        policyGroup: team.policyGroup,
        members: names.length ? "Members include " + names.join(", ") + "." : "No listed members.",
        parent: byId.get(team.parentTeamId),
        secret: team.privacy === "TEAM_PRIVACY_SECRET",
        synced: team.idpSynced,
      };
    });

    if (mode === "bee") {
      return <PageFacts page="org-teams" title={org + " · teams"} groups={rows.length ? rows.map((row) => ({
        title: row.name + (row.policyGroup ? " · " + row.policyGroup : ""),
        facts: [
          row.description ? { label: "purpose", value: row.description } : null,
          { label: "membership", value: row.members },
          row.parent ? { label: "parent team", value: row.parent } : null,
          row.secret && { label: "visibility", value: "Secret" },
          row.synced && { label: "membership source", value: "Identity provider" },
        ],
      })) : [{ title: "teams", empty: "No visible teams." }]} tools={[
        toolFor("TeamService/ListTeams", "List teams and their policy groups"),
        toolFor("TeamService/ListTeamMembers", "List the members and maintainers of a team"),
      ]} />;
    }

    return (
      <>
        <PageHeader eyebrow={org} title="Teams" actions={<PageMenu op={{ service: "TeamService", method: "ListTeams", request: { org }, label: "List teams" }} />} />
        <SettingsTabs org={org} active="teams" />
        {rows.length ? (
          <ul className="divide-y divide-line border-t border-line" data-testid="team-list">
            {rows.map((row) => (
              <li key={row.id} className="space-y-1 py-4 [overflow-wrap:anywhere]">
                <h2 className="text-lg font-medium">{row.name}</h2>
                {row.description ? <p className="text-base text-ink-2">{row.description}</p> : null}
                <p className="text-base text-ink-2">{row.members}</p>
                <p className="flex flex-wrap gap-x-3 gap-y-1 text-sm text-ink-2">
                  {row.policyGroup ? <code>{row.policyGroup}</code> : null}
                  {row.parent ? <span>Part of {row.parent}</span> : null}
                  {row.secret ? <span>Secret</span> : null}
                  {row.synced ? <span>Synced from the identity provider</span> : null}
                </p>
              </li>
            ))}
          </ul>
        ) : <EmptyState title="No visible teams" />}
      </>
    );
  });
}
