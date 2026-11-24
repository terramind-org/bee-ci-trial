/**
 * What the discovery URLs mean. Everything a visitor can change lives in the query string
 * (`?kind=&since=&language=`, `?board=&since=`, `?sort=`), so every view is a link and the
 * back button works. Unknown values fall back to the default instead of failing the page.
 */
import type { LeaderboardMetric, LeaderboardSubject, TrendingWindow } from "@/lib/api/types";

export type Since = "day" | "week" | "month";

export const SINCE_WINDOW: Record<Since, TrendingWindow> = {
  day: "TRENDING_WINDOW_DAILY",
  week: "TRENDING_WINDOW_WEEKLY",
  month: "TRENDING_WINDOW_MONTHLY",
};

export const SINCE_LABEL: Record<Since, string> = { day: "Today", week: "This week", month: "This month" };

/** The query value if it is one of `allowed`, else `fallback`. */
export function parseChoice<T extends string>(value: string, allowed: readonly T[], fallback: T): T {
  return (allowed as readonly string[]).includes(value) ? (value as T) : fallback;
}

/** Same filter as the server's: a metric the subject cannot have is never requested. */
export function metricSupported(subject: LeaderboardSubject, metric: LeaderboardMetric): boolean {
  if (subject === "LEADERBOARD_SUBJECT_UNSPECIFIED" || metric === "LEADERBOARD_METRIC_UNSPECIFIED") return false;
  if (metric === "LEADERBOARD_METRIC_REVIEWS") return subject !== "LEADERBOARD_SUBJECT_REPOS";
  if (metric === "LEADERBOARD_METRIC_STARS") return subject === "LEADERBOARD_SUBJECT_REPOS";
  return true;
}

export interface Board {
  id: string;
  /** Tab label. */
  label: string;
  /** What the score counts, in the column header. */
  unit: string;
  subject: LeaderboardSubject;
  metric: LeaderboardMetric;
}

export const BOARDS: readonly Board[] = [
  { id: "agents-lands", label: "Agents by lands", unit: "Landed changes", subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" },
  { id: "humans-reviews", label: "Humans by reviews", unit: "Reviews", subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_REVIEWS" },
  { id: "humans-lands", label: "Humans by lands", unit: "Landed changes", subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_LANDS" },
  { id: "repos-lands", label: "Repositories by lands", unit: "Landed changes", subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_LANDS" },
  { id: "repos-stars", label: "Repositories by stars gained", unit: "Stars gained", subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_STARS" },
];

export function boardById(id: string): Board {
  return BOARDS.find((b) => b.id === id) ?? (BOARDS[0] as Board);
}
