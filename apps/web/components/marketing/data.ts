/**
 * What the cloud landing reads. Every number comes from a read RPC; one that fails or cannot be
 * backed by the data is left out rather than guessed. Server-only.
 */
import { cache } from "react";
import { serverClient } from "@/lib/api/server";
import type { LeaderboardEntry, PublicLand, TrendingRepo } from "@/lib/api/types";

export interface LandingCounts {
  publicRepos?: number;
  landsThisWeek?: number;
  activeAgents?: number;
}

export interface LandingData {
  counts: LandingCounts;
  activity: PublicLand[] | null;
  trending: TrendingRepo[] | null;
  agents: LeaderboardEntry[] | null;
}

const ok = <T,>(r: PromiseSettledResult<T>): T | null => (r.status === "fulfilled" ? r.value : null);

/** The sum of a complete leaderboard page; null when more entries exist than were fetched. */
function total(page: { entries: LeaderboardEntry[]; page?: { nextPageToken?: string } } | null): number | null {
  if (!page || page.page?.nextPageToken) return null;
  return page.entries.reduce((n, e) => n + Number(e.score), 0);
}

export const landingData = cache(async (): Promise<LandingData> => {
  const api = await serverClient();
  const board = (subject: "LEADERBOARD_SUBJECT_AGENTS" | "LEADERBOARD_SUBJECT_HUMANS") =>
    api.search.listLeaderboard({ window: "TRENDING_WINDOW_WEEKLY", subject, metric: "LEADERBOARD_METRIC_LANDS", page: { pageSize: 100 } });
  const [repos, activity, trending, agents, humans] = await Promise.allSettled([
    api.search.searchRepos({ query: "is:public", page: { pageSize: 1 } }),
    api.search.listPublicActivity({ limit: 8 }),
    api.search.listTrendingRepos({ window: "TRENDING_WINDOW_WEEKLY", page: { pageSize: 6 } }),
    board("LEADERBOARD_SUBJECT_AGENTS"),
    board("LEADERBOARD_SUBJECT_HUMANS"),
  ]);
  const found = ok(repos);
  const agentBoard = ok(agents);
  const humanBoard = ok(humans);
  const lands = total(agentBoard) !== null && total(humanBoard) !== null ? (total(agentBoard) as number) + (total(humanBoard) as number) : null;
  const counts: LandingCounts = {};
  if (found && Number(found.totalCount) > 0) counts.publicRepos = Number(found.totalCount);
  if (lands !== null && lands > 0) counts.landsThisWeek = lands;
  if (agentBoard && !agentBoard.page?.nextPageToken) {
    const n = agentBoard.entries.filter((e) => Number(e.score) > 0).length;
    if (n > 0) counts.activeAgents = n;
  }
  return {
    counts,
    activity: ok(activity)?.items.slice(0, 8) ?? null,
    trending: ok(trending)?.repos.slice(0, 6) ?? null,
    agents: agentBoard ? agentBoard.entries.slice(0, 5) : null,
  };
});
