import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { LeaderboardTable } from "../components/discover/leaderboard-table";
import { BOARDS, boardById, metricSupported, parseChoice } from "../components/discover/params";
import { href } from "../components/discover/pager";
import { ExploreTabs } from "../components/discover/explore-tabs";
import { RepoEntries, NoRepoResults } from "../components/discover/repo-entry";
import { RankRow } from "../components/discover/rank-row";
import { TopicGrid } from "../components/discover/topic-grid";
import { TrendingDeveloperList, TrendingRepoList, type Relations } from "../components/discover/trending-list";
import { WindowSwitch } from "../components/discover/window-switch";
import type { LeaderboardEntry, Principal, SearchRepoHit, SearchUserHit, Topic, TrendingDeveloper, TrendingRepo } from "./api/types";

const agent: Principal = { id: "p1", kind: "PRINCIPAL_KIND_AGENT", display: "triage", ownerId: "o", runtime: "claude-code", externalIds: [] };
const userHit = (login: string, principal?: Principal): SearchUserHit => ({ login, name: "", isOrg: false, bio: "", location: "", avatarUrl: "", followerCount: "0", publicRepoCount: "0", principal });
const repoHit = (fullName: string, over: Partial<SearchRepoHit> = {}): SearchRepoHit => ({
  fullName,
  description: "A repo",
  topics: [],
  primaryLanguage: "Rust",
  starCount: "12",
  forkCount: "0",
  openIssueCount: "0",
  licenseSpdxId: "",
  archived: false,
  isFork: false,
  isTemplate: false,
  visibility: "public",
  ...over,
});
const out: Relations = { signedIn: false, starred: [], following: [] };

describe("discover params", () => {
  it("falls back for unknown query values", () => {
    expect(parseChoice("month", ["day", "week", "month"] as const, "week")).toBe("month");
    expect(parseChoice("year", ["day", "week", "month"] as const, "week")).toBe("week");
    expect(boardById("nope").id).toBe("agents-lands");
  });

  it("only offers boards the server answers", () => {
    for (const b of BOARDS) expect(metricSupported(b.subject, b.metric), b.id).toBe(true);
    expect(metricSupported("LEADERBOARD_SUBJECT_REPOS", "LEADERBOARD_METRIC_REVIEWS")).toBe(false);
    expect(metricSupported("LEADERBOARD_SUBJECT_AGENTS", "LEADERBOARD_METRIC_STARS")).toBe(false);
    expect(metricSupported("LEADERBOARD_SUBJECT_UNSPECIFIED", "LEADERBOARD_METRIC_LANDS")).toBe(false);
  });

  it("builds query strings without empty values", () => {
    expect(href("/trending", { kind: "agents", since: undefined, language: "" })).toBe("/trending?kind=agents");
    expect(href("/trending", {})).toBe("/trending");
  });
});

describe("window switch", () => {
  it("links every window and marks the current one", () => {
    const html = renderToStaticMarkup(h(WindowSwitch, { current: "week", options: ["day", "week", "month"], hrefFor: (s: string) => "/trending?since=" + s }));
    expect(html).toContain('aria-label="Time window"');
    expect(html).toContain('href="/trending?since=day"');
    expect(html).toMatch(/aria-current="page"[^>]*>This week</);
    expect(html.match(/aria-current/g)).toHaveLength(1);
  });
});

describe("rank row", () => {
  it("prints the rank in the sans tabular face and weights only the top three", () => {
    const top = renderToStaticMarkup(h(RankRow, { rank: 2, trailing: "9", children: "x" }));
    const low = renderToStaticMarkup(h(RankRow, { rank: 7, children: "x" }));
    expect(top).toMatch(/num[^"]*font-semibold[^"]*">2</);
    expect(low).toMatch(/num[^"]*text-ink-3[^"]*">7</);
    expect(top).not.toContain("mono");
    expect(low).not.toContain("font-semibold");
  });
  it("draws a hairline share bar only when it is given a share", () => {
    expect(renderToStaticMarkup(h(RankRow, { rank: 1, share: 0.4, children: "x" }))).toContain("width:40%");
    expect(renderToStaticMarkup(h(RankRow, { rank: 1, children: "x" }))).not.toContain("width:");
  });
});

describe("topic grid", () => {
  const topic = (name: string, count: string): Topic => ({ name, displayName: name === "ts" ? "TypeScript" : name, shortDescription: "Typed JS", description: "", featured: true, curated: true, aliases: [], related: [], logoUrl: "", repoCount: count, topRepos: [] });
  it("links each topic with its repo count", () => {
    const html = renderToStaticMarkup(h(TopicGrid, { topics: [topic("ts", "1"), topic("rust", "1200")] }));
    expect(html).toContain('href="/topics/ts"');
    expect(html).toContain(">TypeScript<");
    expect(html).toContain(">1</span> repository<");
    expect(html).toContain("1.2k");
    expect(html).toContain("repositories");
  });
});

describe("trending list", () => {
  const repos: TrendingRepo[] = [{ repo: repoHit("lattice/agentkit"), starsInWindow: "4", forksInWindow: "0", builtBy: [agent], rank: 1 }];
  it("shows rank, name, language, stars in the window and a star control", () => {
    const html = renderToStaticMarkup(h(TrendingRepoList, { repos, since: "week", relations: out }));
    expect(html).toContain('href="/lattice/agentkit"');
    expect(html).toContain("Rust");
    expect(html).toContain("+4");
    expect(html).toContain("this week");
    expect(html).toContain("/signin?next=");
    expect(html).toContain("claude-code");
  });
  it("starts the star button in the viewer's state", () => {
    const html = renderToStaticMarkup(h(TrendingRepoList, { repos, since: "day", relations: { signedIn: true, starred: ["lattice/agentkit"], following: [] } }));
    expect(html).toContain('aria-pressed="true"');
  });
  it("says so when nothing is trending", () => {
    expect(renderToStaticMarkup(h(TrendingRepoList, { repos: [], since: "month", relations: out }))).toContain("Nothing is trending this month yet");
  });
  it("lists developers with mark, runtime, popular repo and a follow control, but never follow-yourself", () => {
    const devs: TrendingDeveloper[] = [
      { user: userHit("triage", agent), popularRepo: repoHit("lattice/agentkit"), rank: 1 },
      { user: userHit("dev", { ...agent, id: "me", kind: "PRINCIPAL_KIND_HUMAN", runtime: "" }), rank: 2 },
    ];
    const html = renderToStaticMarkup(h(TrendingDeveloperList, { developers: devs, kind: "agents", since: "week", relations: { signedIn: true, starred: [], following: ["triage"], meId: "me" } }));
    expect(html).toContain('href="/triage"');
    expect(html).toContain("claude-code");
    expect(html).toContain("Popular");
    expect(html).toContain('aria-label="triage (agent)"');
    expect(html).toContain(">Following<");
    expect(html.match(/aria-pressed/g)).toHaveLength(1);
  });
});

describe("leaderboard list", () => {
  const entries: LeaderboardEntry[] = [
    { rank: 1, user: userHit("triage", agent), score: "6" },
    { rank: 2, user: userHit("patcher", { ...agent, id: "p2", display: "patcher" }), score: "3" },
    { rank: 3, user: userHit("deps"), score: "1" },
  ];
  it("prints rank, subject, score and each share of the listed total", () => {
    const html = renderToStaticMarkup(h(LeaderboardTable, { entries, unit: "Landed changes", caption: "Agents by lands, this week" }));
    expect(html).toContain('aria-label="Agents by lands, this week"');
    expect(html).toContain("Landed changes");
    expect(html).toMatch(/num[^"]*font-semibold[^"]*">6</);
    expect(html).toContain("60%");
    expect(html).toContain("30%");
    expect(html).toContain("10%");
    expect(html).toContain("claude-code");
    expect(html).not.toMatch(/[\u{1F300}-\u{1FAFF}]/u);
  });
  it("lists repositories by name", () => {
    const html = renderToStaticMarkup(h(LeaderboardTable, { entries: [{ rank: 1, repo: repoHit("lattice/tl"), score: "5" }], unit: "Stars gained", caption: "x" }));
    expect(html).toContain('href="/lattice/tl"');
    expect(html).toContain(">Repository<");
    expect(html).not.toContain("<table");
  });
  it("is empty-state safe", () => {
    expect(renderToStaticMarkup(h(LeaderboardTable, { entries: [], unit: "Reviews", caption: "x" }))).toContain("Nobody on this board yet");
  });
});

describe("explore tabs", () => {
  it("keeps the three old routes as sub-views and marks the current one", () => {
    const html = renderToStaticMarkup(h(ExploreTabs, { current: "trending" }));
    for (const href of ["/explore", "/trending", "/leaderboards"]) expect(html).toContain('href="' + href + '"');
    expect(html).toMatch(/aria-current="page"[^>]*>Trending</);
    expect(html.match(/aria-current/g)).toHaveLength(1);
  });
});

describe("repository entries", () => {
  it("are unboxed articles with the owner's mark, name, stars and topics", () => {
    const html = renderToStaticMarkup(h(RepoEntries, { hits: [repoHit("lattice/agentkit", { topics: ["agents"] })], empty: "none" }));
    expect(html).toContain('<article class="flex items-start gap-4 py-5"');
    expect(html).not.toContain("rounded-lg border");
    expect(html).toContain('href="/lattice/agentkit"');
    expect(html).toContain('aria-label="12 stars"');
    expect(html).toContain('href="/topics/agents"');
    expect(html).not.toContain("mono");
  });
  it("says why a filtered list is empty and offers the way out", () => {
    const html = renderToStaticMarkup(h(NoRepoResults, { filtered: true, clearHref: "/repositories" }));
    expect(html).toContain("No repositories match these filters");
    expect(html).toContain('href="/repositories"');
  });
});
