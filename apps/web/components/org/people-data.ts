import type { BeeClient } from "@/lib/api/client";
import { mapLimit } from "@/lib/api/ops/shared";
import type { PageRequest } from "@/lib/api/types";

async function allRows<T>(read: (page: PageRequest) => Promise<{ rows?: T[]; next?: string }>) {
  const rows: T[] = [];
  let pageToken: string | undefined;
  do {
    const page = await read({ pageSize: 100, pageToken });
    rows.push(...(page.rows ?? []));
    pageToken = page.next;
  } while (pageToken);
  return rows;
}

export function orgMembers(api: BeeClient, org: string) {
  return allRows(async (page) => {
    const result = await api.org.listMembers({ org, page });
    return { rows: result.members, next: result.page?.nextPageToken };
  });
}

export async function orgTeams(api: BeeClient, org: string) {
  const { teams } = await api.team.listTeams({ org });
  return mapLimit(teams, 6, async (team) => ({
    team,
    members: await allRows(async (page) => {
      const result = await api.team.listTeamMembers({ org, team: team.id || team.slug, includeChildTeams: true, page });
      return { rows: result.members, next: result.page?.nextPageToken };
    }),
  }));
}
