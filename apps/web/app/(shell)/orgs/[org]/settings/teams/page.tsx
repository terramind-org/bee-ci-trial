import type { Metadata } from "next";
import { PageMenu } from "@/components/menu";
import { Time } from "@/components/time";
import { Badge, EmptyState, PageHeader } from "@/components/ui";
import { serverClient } from "@/lib/api/server";
import { guardSection } from "@/lib/page";
import { SettingsTabs } from "../tabs";

export const metadata: Metadata = { title: "Teams" };

export default async function TeamsPage({ params }: { params: Promise<{ org: string }> }) {
  const org = decodeURIComponent((await params).org);
  const req = { org };
  return (
    <>
      <PageHeader title="Settings" lede={<>Teams in {org}. Each is a policy group (@{org}/slug) that land rules and owners can name.</>} actions={<PageMenu op={{ service: "TeamService", method: "ListTeams", request: req, label: "List teams" }} />} />
      <SettingsTabs org={org} active="teams" />
      {await guardSection("Teams", async () => {
        const api = await serverClient();
        const { teams } = await api.team.listTeams(req);
        const byId = new Map(teams.map((t) => [t.id, t]));
        return (
        <div>
          {teams.length === 0 ? (
            <EmptyState title="No teams yet" icon="team">
              Create one with <code className="mono">bee api TeamService/CreateTeam</code>; policy can then require its approval.
            </EmptyState>
          ) : (
            <ul className="divide-y divide-line" data-testid="team-list">
              {teams.map((t) => (
                <li key={t.id} className="flex flex-col gap-1.5 py-3 md:flex-row md:items-center md:gap-4">
                  <div className="min-w-0 flex-1">
                    <p className="flex flex-wrap items-center gap-2">
                      <span className="font-medium text-ink">{t.name}</span>
                      <span className="mono text-sm text-ink-3">{t.policyGroup || "@" + org + "/" + t.slug}</span>
                      {t.privacy === "TEAM_PRIVACY_SECRET" ? <Badge tone="outline">secret</Badge> : null}
                      {t.idpSynced ? <Badge>IdP synced</Badge> : null}
                    </p>
                    {t.description ? <p className="text-base text-ink-2">{t.description}</p> : null}
                    {t.parentTeamId ? <p className="text-sm text-ink-3">child of {byId.get(t.parentTeamId)?.name ?? t.parentTeamId}</p> : null}
                  </div>
                  <div className="flex gap-4 text-base text-ink-2">
                    <span>
                      <span className="num font-semibold text-ink">{t.memberCount ?? 0}</span> members
                    </span>
                    <span>
                      <span className="num font-semibold text-ink">{t.repoCount ?? 0}</span> repos
                    </span>
                    <Time iso={t.createdAt} />
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
        );
      })}
    </>
  );
}
