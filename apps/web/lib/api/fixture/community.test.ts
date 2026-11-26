import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { SearchService } from "../../gen/bee/v1/search_pb";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from "./index";
import { ANONYMOUS_READ_METHODS } from "./anonymous";

const ANCHOR = Date.UTC(2026, 9, 18, 9, 0, 0);

function world() {
  const store = new FixtureStore({ anchor: ANCHOR, clock: () => 0, live: false });
  return { store, mara: createClient(fixtureRpc(store, "mara-token-0001")), anon: createClient(fixtureRpc(store)), as: (token: string) => createClient(fixtureRpc(store, token)) };
}

const DAILY = "TRENDING_WINDOW_DAILY" as const;
const WEEKLY = "TRENDING_WINDOW_WEEKLY" as const;
const MONTHLY = "TRENDING_WINDOW_MONTHLY" as const;

describe("anonymous access", () => {
  it("mirrors the server's ANONYMOUS_READ_METHODS", () => {
    const rust = readFileSync(new URL("../../../../../crates/bee-server/src/access.rs", import.meta.url), "utf8");
    const body = /ANONYMOUS_READ_METHODS: &\[&str\] = &\[([\s\S]*?)\];/.exec(rust)?.[1] ?? "";
    const server = [...body.matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort();
    expect([...ANONYMOUS_READ_METHODS].sort()).toEqual(server);
  });

  it("answers GetServerInfo for everyone, with the connect URL and features", async () => {
    const { anon } = world();
    const info = await anon.object.getServerInfo({});
    expect(info).toMatchObject({ connectBaseUrl: "https://hub.bee.example", edition: "cloud" });
    expect(info.features).toEqual(["ssh", "anonymous_read"]);
  });

  it("rejects writes and signed-in reads without a token as unauthenticated", async () => {
    const { anon } = world();
    await expect(anon.user.starRepo({ repo: { fullName: "lattice/engine" } })).rejects.toMatchObject({ code: "unauthenticated", reason: "UNAUTHENTICATED" });
    await expect(anon.user.listHomeFeed({})).rejects.toMatchObject({ code: "unauthenticated" });
    await expect(anon.org.listOrgs({})).rejects.toMatchObject({ code: "unauthenticated" });
    await expect(anon.org.createOrg({ name: "newco" })).rejects.toMatchObject({ code: "unauthenticated" });
    await expect(anon.agent.listAgents({ org: "lattice", repo: { fullName: "lattice/engine" } })).rejects.toMatchObject({ code: "unauthenticated" });
    await expect(anon.org.listMembers({ org: "lattice" })).rejects.toMatchObject({ code: "unauthenticated" });
    await expect(anon.lease.acquireLease({ repo: { fullName: "lattice/engine" }, ref: "main", paths: [], mode: "LEASE_MODE_ADVISORY", ttlSeconds: 60, note: "" })).rejects.toMatchObject({ code: "unauthenticated" });
  });

  it("serves the tabs of a public repo without a token, and hides private repos behind not found", async () => {
    const { anon } = world();
    const pub = { fullName: "lattice/engine" };
    const changes = await anon.change.listChanges({ repo: pub });
    expect(changes.changes.length).toBeGreaterThan(0);
    const cr = changes.changes[0]!;
    expect((await anon.change.getChange({ repo: pub, id: cr.id })).id).toBe(cr.id);
    expect((await anon.lease.listLeases({ repo: pub })).leases).toBeDefined();
    const runs = (await anon.workflow.listRuns({ repo: pub })).runs;
    expect(runs.length).toBeGreaterThan(0);
    expect((await anon.workflow.getRun({ repo: pub, id: runs[0]!.id })).id).toBe(runs[0]!.id);
    const issues = (await anon.issue.listIssues({ repo: pub })).issues;
    expect(issues.length).toBeGreaterThan(0);
    expect((await anon.issue.getIssue({ repo: pub, number: issues[0]!.number })).number).toBe(issues[0]!.number);
    expect((await anon.repo.getContext({ repo: pub, at: { expr: "main" }, paths: [] })).landRules).toBeDefined();
    const hidden = { fullName: "acme/api" };
    for (const p of [anon.change.listChanges({ repo: hidden }), anon.lease.listLeases({ repo: hidden }), anon.workflow.listRuns({ repo: hidden }), anon.issue.listIssues({ repo: hidden }), anon.repo.getContext({ repo: hidden, at: { expr: "main" }, paths: [] })]) {
      await expect(p).rejects.toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
    }
  });

  it("never reveals private repos: they look exactly like repos that do not exist", async () => {
    const { anon, mara } = world();
    const hidden = await anon.repo.getRepo({ repo: { fullName: "acme/api" } }).catch((e: unknown) => e);
    const missing = await anon.repo.getRepo({ repo: { fullName: "acme/nope" } }).catch((e: unknown) => e);
    expect(hidden).toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
    expect(missing).toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
    await expect(anon.repo.getRepo({ repo: { fullName: "lattice/internal-tools" } })).rejects.toMatchObject({ reason: "REPO_NOT_FOUND" });
    await expect(anon.query.log({ repo: { fullName: "acme/api" }, from: { expr: "main" } })).rejects.toMatchObject({ reason: "REPO_NOT_FOUND" });
    expect((await anon.repo.getRepo({ repo: { fullName: "lattice/engine" } })).fullName).toBe("lattice/engine");
    // mara is not in lattice, so its private repo is hidden from her too; her own are not.
    await expect(mara.repo.getRepo({ repo: { fullName: "lattice/internal-tools" } })).rejects.toMatchObject({ reason: "REPO_NOT_FOUND" });
    expect((await mara.repo.getRepo({ repo: { fullName: "acme/api" } })).visibility).toBe("VISIBILITY_PRIVATE");
  });

  it("serves a public repo home anonymously, with the lease list", async () => {
    const { anon } = world();
    const home = await anon.repo.getRepoHome({ fullName: "lattice/engine" });
    expect(home.leasesVisible).toBe(true);
    expect(home.starredByMe).toBe(false);
    expect(home.readme?.markdown).toContain("# lattice/engine");
    expect(home.latestLand?.meta?.message).toBe("Walk the graph without recursion");
    expect(home.cloneUrls).toEqual({ https: "https://hub.bee.example/lattice/engine.git", ssh: "git@hub.bee.example:lattice/engine.git" });
    expect(home.repo).toMatchObject({ primaryLanguage: "Rust", licenseSpdx: "Apache-2.0", openChanges: "1" });
    await expect(anon.repo.getRepoHome({ fullName: "acme/api" })).rejects.toMatchObject({ code: "not_found", reason: "REPO_NOT_FOUND" });
  });

  it("shows the lease list and the star state to a signed-in reader", async () => {
    const { mara } = world();
    const home = await mara.repo.getRepoHome({ fullName: "acme/api" });
    expect(home.leasesVisible).toBe(true);
    expect(home.leases.length).toBeGreaterThan(2);
    expect((await mara.repo.getRepoHome({ fullName: "lattice/engine" })).starredByMe).toBe(true);
  });

  it("lists recent public lands only", async () => {
    const { anon } = world();
    const { items } = await anon.search.listPublicActivity({ limit: 20 });
    expect(items.length).toBeGreaterThan(3);
    expect(items.every((i) => ["acme/sdk", "lattice/engine", "lattice/agent-kit", "lattice/docs", "lattice/bench"].includes(i.repo.fullName))).toBe(true);
    const times = items.map((i) => Date.parse(i.changeset.meta?.committerTime?.time ?? ""));
    expect(times).toEqual([...times].sort((a, b) => b - a));
  });
});

describe("the multi-owner world", () => {
  it("keeps acme's repos and adds lattice's", async () => {
    const { mara } = world();
    expect((await mara.repo.listRepos({ org: "acme" })).repos.map((r) => r.fullName)).toEqual(["acme/api", "acme/web", "acme/infra", "acme/sdk"]);
    expect((await mara.repo.listRepos({ org: "lattice" })).repos.map((r) => r.fullName)).toEqual(["lattice/engine", "lattice/agent-kit", "lattice/docs", "lattice/bench"]);
    expect((await mara.org.listOrgs({})).orgs.map((o) => o.name)).toEqual(["acme"]);
    expect((await mara.org.getOrg({ nameOrId: "lattice" })).display).toBe("Lattice Labs");
    expect((await mara.org.listMembers({ org: "lattice" })).members.map((m) => [m.principal?.display, m.role])).toEqual([
      ["Rhea Lindqvist", "ROLE_OWNER"],
      ["Omar Haddad", "ROLE_ADMIN"],
      ["Kit Nakamura", "ROLE_MEMBER"],
    ]);
    expect((await mara.org.listMembers({ org: "acme" })).members.map((m) => m.principal?.display)).toContain("Mara Okafor");
  });

  it("fills the repo counters from the world's own tables", async () => {
    const { mara, store } = world();
    const engine = await mara.repo.getRepo({ repo: { fullName: "lattice/engine" } });
    expect(engine).toMatchObject({ topics: ["rust", "vcs", "devtools"], primaryLanguage: "Rust", openIssues: "2", openChanges: "1", forks: "188", licenseSpdx: "Apache-2.0" });
    expect(Number(engine.stars)).toBe(store.stars.get("lattice/engine")?.length);
    expect(Date.parse(engine.pushedAt ?? "")).toBe(ANCHOR - 100 * 60_000);
    const api = await mara.repo.getRepo({ repo: { fullName: "acme/api" } });
    expect(api.openIssues).not.toBe("0");
    expect(api.primaryLanguage).toBe("TypeScript");
    expect((await mara.repo.getRepo({ repo: { fullName: "acme/sdk" } })).visibility).toBe("VISIBILITY_PUBLIC");
  });

  it("scopes acme-wide activity to acme and reads lattice history like any repo", async () => {
    const { mara } = world();
    const acme = await mara.event.listEvents({ repo: { fullName: "acme/*" }, limit: 1000 });
    const lattice = await mara.event.listEvents({ repo: { fullName: "lattice/*" }, limit: 1000 });
    expect(lattice.events.length).toBeGreaterThan(5);
    expect(acme.events.some((e) => lattice.events.some((l) => l.seq === e.seq))).toBe(false);
    const log = await mara.query.log({ repo: { fullName: "lattice/engine" }, from: { expr: "main" } });
    expect(log.changesets.length).toBe(7);
  });

  it("reports languages by bytes, summing to 100", async () => {
    const { mara } = world();
    for (const name of ["lattice/engine", "lattice/agent-kit", "lattice/bench", "acme/api"]) {
      const languages = (await mara.insights.getLanguages({ repo: { fullName: name }, at: { expr: "main" } })).languages ?? [];
      expect(languages.length).toBeGreaterThan(0);
      expect(Math.round(languages.reduce((n, l) => n + Number(l.percentage), 0) * 10) / 10).toBe(100);
      expect(languages.every((l) => l.color?.startsWith("#"))).toBe(true);
    }
    const bench = await mara.insights.getLanguages({ repo: { fullName: "lattice/bench" }, at: { expr: "main" } });
    expect(bench.languages?.[0]?.name).toBe("Python");
  });
});

describe("profiles", () => {
  it("returns users, agents and orgs with their own kind", async () => {
    const { mara, anon } = world();
    const rhea = await anon.user.getProfile({ owner: "rhea" });
    expect(rhea).toMatchObject({ kind: "user", login: "rhea", name: "Rhea Lindqvist", company: "Lattice Labs", orgs: ["lattice"] });
    expect(rhea.readme).toContain("<h2>");
    expect(rhea.followers).toBe(3);
    expect(rhea.publicRepos).toBeGreaterThan(0);
    const patcher = await anon.user.getProfile({ owner: "patcher" });
    expect(patcher).toMatchObject({ kind: "agent", runtime: "claude-code", ownerLogin: "rhea" });
    const org = await anon.user.getProfile({ owner: "lattice" });
    expect(org).toMatchObject({ kind: "org", login: "lattice", name: "Lattice Labs", publicRepos: 4 });
    await expect(anon.user.getProfile({ owner: "nobody-here" })).rejects.toMatchObject({ code: "not_found", reason: "USER_NOT_FOUND" });
    expect((await anon.user.getProfile({ owner: "rhea" })).followingByMe).toBe(false);
    expect((await mara.user.getProfile({ owner: "rhea" })).followingByMe).toBe(true);
    expect((await anon.user.getProfile({ owner: "mara" })).readme).toContain("Mara");
  });
});

describe("stars and follows", () => {
  it("stars and unstars idempotently and keeps every count in step", async () => {
    const { mara } = world();
    const repo = { fullName: "lattice/docs" };
    const before = await mara.repo.getRepo({ repo });
    await mara.user.starRepo({ repo });
    await mara.user.starRepo({ repo });
    expect((await mara.repo.getRepoHome({ fullName: "lattice/docs" })).starredByMe).toBe(true);
    const after = await mara.repo.getRepo({ repo });
    expect(Number(after.stars)).toBe(Number(before.stars) + 1);
    const starred = await mara.user.listStarred({ login: "mara" });
    expect(starred.repos.map((r) => r.fullName)).toContain("lattice/docs");
    expect(starred.repos[0]?.fullName).toBe("lattice/docs");
    expect((await mara.user.getProfile({ owner: "mara" })).publicRepos).toBeGreaterThan(0);
    await mara.user.unstarRepo({ repo });
    await mara.user.unstarRepo({ repo });
    expect(Number((await mara.repo.getRepo({ repo })).stars)).toBe(Number(before.stars));
    expect((await mara.user.listStarred({ login: "mara" })).repos.map((r) => r.fullName)).not.toContain("lattice/docs");
  });

  it("filters and sorts starred repos, and never lists what the viewer cannot read", async () => {
    const { mara, anon } = world();
    const ts = await mara.user.listStarred({ login: "mara", language: "TypeScript" });
    expect(ts.repos.every((r) => r.language === "TypeScript")).toBe(true);
    const byStars = await mara.user.listStarred({ login: "mara", sort: "stars" });
    const counts = byStars.repos.map((r) => Number(r.starCount));
    expect(counts).toEqual([...counts].sort((a, b) => b - a));
    expect((await mara.user.listStarred({ login: "mara" })).repos.map((r) => r.fullName)).toContain("acme/api");
    expect((await anon.user.listStarred({ login: "mara" })).repos.map((r) => r.fullName)).not.toContain("acme/api");
    const paged = await mara.user.listStarred({ login: "mara", page: { pageSize: 2 } });
    expect(paged.repos).toHaveLength(2);
    expect(paged.page?.nextPageToken).toBeTruthy();
    await expect(anon.user.listStarred({ login: "" })).rejects.toMatchObject({ code: "invalid_argument" });
  });

  it("follows and unfollows, and counts both sides", async () => {
    const { mara, anon } = world();
    const before = (await anon.user.getProfile({ owner: "sam" })).followers;
    await mara.user.followUser({ login: "sam" });
    await mara.user.followUser({ login: "sam" });
    expect((await anon.user.getProfile({ owner: "sam" })).followers).toBe(before + 1);
    expect((await mara.user.getProfile({ owner: "sam" })).followingByMe).toBe(true);
    expect((await anon.user.listFollowers({ login: "sam" })).users.map((u) => u.login)).toEqual(["mara"]);
    expect((await mara.user.listFollowing({ login: "mara" })).users.map((u) => u.login)).toContain("sam");
    await mara.user.unfollowUser({ login: "sam" });
    expect((await anon.user.getProfile({ owner: "sam" })).followers).toBe(before);
  });

  it("refuses self-follows and unknown users, and lists orgs among followed accounts", async () => {
    const { mara, anon } = world();
    await expect(mara.user.followUser({ login: "mara" })).rejects.toMatchObject({ code: "invalid_argument", reason: "CANNOT_FOLLOW_SELF" });
    await expect(mara.user.followUser({ login: "ghost" })).rejects.toMatchObject({ code: "not_found", reason: "USER_NOT_FOUND" });
    await expect(anon.user.followUser({ login: "sam" })).rejects.toMatchObject({ code: "unauthenticated" });
    const following = await anon.user.listFollowing({ login: "mara" });
    expect(following.users.map((u) => u.login)).toContain("lattice");
    expect(following.users[0]?.principal?.id).toBeTruthy();
  });
});

describe("contribution graph and achievements", () => {
  it("covers exactly the 365 days ending at the anchor's date, identically in two worlds", async () => {
    const a = await world().anon.user.getContributionGraph({ login: "rhea" });
    const b = await world().anon.user.getContributionGraph({ login: "rhea" });
    expect(a).toEqual(b);
    expect(a.days).toHaveLength(365);
    expect(a.toDate).toBe("2026-10-18");
    expect(a.fromDate).toBe("2025-10-19");
    expect(a.days.map((d) => d.date)).toEqual([...a.days.map((d) => d.date)].sort());
    expect(new Set(a.days.map((d) => d.date)).size).toBe(365);
    expect(a.total).toBe(a.days.reduce((n, d) => n + d.count, 0));
    expect(a.days.every((d) => (d.count === 0) === (d.level === 0) && d.level >= 0 && d.level <= 4)).toBe(true);
    expect(Math.max(...a.days.map((d) => d.level))).toBe(4);
    expect(a.includesPrivate).toBe(false);
  });

  it("counts real landed work on the day it landed, and private work only for its owner", async () => {
    const { anon, mara } = world();
    const publicView = await anon.user.getContributionGraph({ login: "mara" });
    const ownView = await mara.user.getContributionGraph({ login: "mara" });
    expect(publicView.includesPrivate).toBe(false);
    expect(ownView.includesPrivate).toBe(true);
    expect(ownView.total).toBeGreaterThan(publicView.total);
    const noiseOnly = await anon.user.getContributionGraph({ login: "kit" });
    expect(noiseOnly.total).toBeGreaterThan(0);
  });

  it("derives achievements from real data", async () => {
    const { anon, mara } = world();
    const rhea = await anon.user.listAchievements({ login: "rhea" });
    expect(rhea.achievements.map((a) => a.slug)).toEqual(expect.arrayContaining(["first-land", "agent-owner"]));
    expect(rhea.achievements.every((a) => ["bronze", "silver", "gold"].includes(a.tier) && a.count > 0)).toBe(true);
    const own = await mara.user.listAchievements({ login: "mara" });
    expect(own.achievements.find((a) => a.slug === "agent-owner")).toMatchObject({ tier: "silver", count: 3 });
    await expect(anon.user.listAchievements({ login: "ghost" })).rejects.toMatchObject({ reason: "USER_NOT_FOUND" });
  });
});

describe("home feed", () => {
  it("lists newest-first activity the viewer can read, filtered by kind and paged", async () => {
    const { mara } = world();
    const all = await mara.user.listHomeFeed({ page: { pageSize: 100 } });
    const times = all.events.map((e) => Date.parse(e.createdAt ?? ""));
    expect(times).toEqual([...times].sort((a, b) => b - a));
    expect(new Set(all.events.map((e) => e.kind))).toEqual(new Set(["land", "review", "release", "star", "follow"]));
    expect(all.events.every((e) => e.summary && e.subject?.kind && e.actor?.id)).toBe(true);
    expect(all.events.some((e) => e.subject?.repo?.fullName === "lattice/internal-tools")).toBe(false);
    const lands = await mara.user.listHomeFeed({ kinds: ["land"], page: { pageSize: 100 } });
    expect(lands.events.length).toBeGreaterThan(3);
    expect(lands.events.every((e) => e.kind === "land")).toBe(true);
    const first = await mara.user.listHomeFeed({ kinds: ["land"], page: { pageSize: 2 } });
    expect(first.events).toHaveLength(2);
    const next = await mara.user.listHomeFeed({ kinds: ["land"], page: { pageSize: 2, pageToken: first.page?.nextPageToken } });
    expect(next.events[0]?.id).toBe(lands.events[2]?.id);
  });
});

describe("search", () => {
  it("hides private repos from anonymous callers and shows them to members", async () => {
    const { anon, mara } = world();
    const pub = await anon.search.searchRepos({ query: "" });
    expect(pub.hits.map((h) => h.fullName).sort()).toEqual(["acme/sdk", "lattice/agent-kit", "lattice/bench", "lattice/docs", "lattice/engine"]);
    expect(pub.totalCount).toBe("5");
    const mine = await mara.search.searchRepos({ query: "" });
    expect(mine.hits.map((h) => h.fullName)).toEqual(expect.arrayContaining(["acme/api", "acme/web", "acme/infra", "acme/sdk", "lattice/engine"]));
    expect(mine.hits.map((h) => h.fullName)).not.toContain("lattice/internal-tools");
    expect((await anon.search.searchRepos({ query: "is:private" })).hits).toEqual([]);
    expect((await mara.search.searchRepos({ query: "is:private" })).hits.map((h) => h.fullName).sort()).toEqual(["acme/api", "acme/infra"]);
    expect((await mara.search.searchRepos({ query: "is:internal" })).hits.map((h) => h.fullName)).toEqual(["acme/web"]);
  });

  it("understands the repo qualifiers", async () => {
    const { anon } = world();
    const names = async (query: string, extra: { sort?: string; order?: "SEARCH_SORT_ORDER_ASC" } = {}) => (await anon.search.searchRepos({ query, ...extra })).hits.map((h) => h.fullName);
    expect(await names("topic:agents")).toEqual(["lattice/agent-kit"]);
    expect(await names("topic:ai-agents")).toEqual(["lattice/agent-kit"]);
    expect(await names("language:rust")).toEqual(["lattice/engine"]);
    expect(await names("lang:python is:public")).toEqual(["lattice/bench"]);
    expect(await names("org:lattice topic:typescript")).toEqual(["lattice/agent-kit"]);
    expect(await names("org:acme")).toEqual(["acme/sdk"]);
    expect(await names("user:patcher sort:stars")).toEqual(["lattice/engine", "lattice/agent-kit", "lattice/docs"]);
    expect(await names("stars:>1000")).toEqual(["lattice/engine"]);
    expect(await names("stars:200..400 sort:stars")).toEqual(["lattice/bench", "lattice/docs"]);
    expect(await names("stars:<100")).toEqual(["acme/sdk"]);
    expect(await names("-topic:rust org:lattice sort:stars")).toEqual(["lattice/agent-kit", "lattice/bench", "lattice/docs"]);
    expect(await names("rebase")).toEqual(["lattice/engine"]);
    expect(await names("benchmarks")).toEqual(["lattice/bench"]);
    expect(await names("lattice", { sort: "stars" })).toEqual(["lattice/engine", "lattice/agent-kit", "lattice/bench", "lattice/docs"]);
    expect(await names("org:lattice", { sort: "stars", order: "SEARCH_SORT_ORDER_ASC" })).toEqual(["lattice/docs", "lattice/bench", "lattice/agent-kit", "lattice/engine"]);
    expect(await names("org:lattice sort:updated")).toEqual(["lattice/bench", "lattice/engine", "lattice/agent-kit", "lattice/docs"]);
    expect(await names("org:lattice", { sort: "forks" })).toEqual(["lattice/engine", "lattice/agent-kit", "lattice/bench", "lattice/docs"]);
    expect(await names("zzzz-nothing")).toEqual([]);
  });

  it("pages results and rejects bad queries with a reason", async () => {
    const { anon } = world();
    const first = await anon.search.searchRepos({ query: "sort:stars", page: { pageSize: 2 } });
    expect(first.hits.map((h) => h.fullName)).toEqual(["lattice/engine", "lattice/agent-kit"]);
    expect(first.totalCount).toBe("5");
    const second = await anon.search.searchRepos({ query: "sort:stars", page: { pageSize: 2, pageToken: first.page?.nextPageToken } });
    expect(second.hits.map((h) => h.fullName)).toEqual(["lattice/bench", "lattice/docs"]);
    await expect(anon.search.searchRepos({ query: "stars:lots" })).rejects.toMatchObject({ code: "invalid_argument", reason: "INVALID_QUERY" });
    await expect(anon.search.searchRepos({ query: "is:cursed" })).rejects.toMatchObject({ code: "invalid_argument", reason: "INVALID_QUERY" });
    await expect(anon.search.searchRepos({ query: "x", sort: "vibes" })).rejects.toMatchObject({ reason: "INVALID_SORT" });
  });

  it("searches users, orgs and agents", async () => {
    const { store } = world();
    const rpc = fixtureRpc(store);
    const searchUsers = (req: { query: string }) => rpc.call(SearchService.method.searchUsers, req);
    const logins = async (query: string) => ((await searchUsers({ query })).hits ?? []).map((h) => h.login);
    expect(await logins("type:org")).toEqual(["lattice", "acme"]);
    expect(await logins("type:agent in:login patcher")).toEqual(["patcher"]);
    expect(((await searchUsers({ query: "type:org in:login lattice" })).hits ?? [])[0]).toMatchObject({ login: "lattice", isOrg: true, name: "Lattice Labs", publicRepoCount: "4" });
    expect((await logins("rhea"))[0]).toBe("rhea");
    expect((await logins("type:user location:portugal"))).toEqual(["rhea"]);
    const byFollowers = ((await searchUsers({ query: "type:user sort:followers" })).hits ?? []).map((h) => Number(h.followerCount));
    expect(byFollowers).toEqual([...byFollowers].sort((a, b) => b - a));
    await expect(logins("type:robot")).rejects.toMatchObject({ code: "invalid_argument", reason: "INVALID_QUERY" });
  });
});

describe("trending and leaderboards", () => {
  it("ranks trending repos by stars gained in the window, then lands, readable only", async () => {
    const { anon, mara } = world();
    for (const window of [DAILY, WEEKLY, MONTHLY]) {
      const t = await anon.search.listTrendingRepos({ window });
      expect(t.repos.length).toBeGreaterThan(2);
      expect(t.repos.map((r) => r.rank)).toEqual(t.repos.map((_, i) => i + 1));
      const stars = t.repos.map((r) => Number(r.starsInWindow));
      expect(stars).toEqual([...stars].sort((a, b) => b - a));
      expect(t.repos.every((r) => r.repo?.visibility === "public")).toBe(true);
      expect(t.computedAt).toBe(new Date(ANCHOR).toISOString().replace(".000Z", "Z"));
      expect(t.repos.every((r) => r.builtBy.length > 0)).toBe(true);
    }
    const mine = await mara.search.listTrendingRepos({ window: MONTHLY });
    expect(mine.repos.map((r) => r.repo?.fullName)).toContain("acme/api");
    expect(mine.repos.every((r) => r.repo?.fullName !== "lattice/internal-tools")).toBe(true);
    const rust = await anon.search.listTrendingRepos({ window: WEEKLY, language: "Rust" });
    expect(rust.repos.map((r) => r.repo?.fullName)).toEqual(["lattice/engine"]);
    const wider = await anon.search.listTrendingRepos({ window: MONTHLY });
    const narrower = await anon.search.listTrendingRepos({ window: WEEKLY });
    const sum = (r: typeof wider) => r.repos.reduce((n, x) => n + Number(x.starsInWindow), 0);
    expect(sum(wider)).toBeGreaterThan(sum(narrower));
  });

  it("agrees with the REPOS/STARS leaderboard", async () => {
    const { anon } = world();
    for (const window of [DAILY, WEEKLY, MONTHLY]) {
      const t = await anon.search.listTrendingRepos({ window });
      const l = await anon.search.listLeaderboard({ window, subject: "LEADERBOARD_SUBJECT_REPOS", metric: "LEADERBOARD_METRIC_STARS" });
      expect(l.entries.map((e) => [e.repo?.fullName, e.score])).toEqual(t.repos.filter((r) => Number(r.starsInWindow) > 0).map((r) => [r.repo?.fullName, r.starsInWindow]));
      expect(l.entries.every((e) => e.user === undefined && e.repo)).toBe(true);
    }
  });

  it("validates leaderboard requests with SPEC reasons", async () => {
    const { anon } = world();
    const call = (subject: string, metric: string) => anon.search.listLeaderboard({ window: WEEKLY, subject: subject as never, metric: metric as never });
    await expect(call("LEADERBOARD_SUBJECT_REPOS", "LEADERBOARD_METRIC_REVIEWS")).rejects.toMatchObject({ code: "invalid_argument" });
    await expect(call("LEADERBOARD_SUBJECT_AGENTS", "LEADERBOARD_METRIC_STARS")).rejects.toMatchObject({ code: "invalid_argument" });
    await expect(call("LEADERBOARD_SUBJECT_HUMANS", "LEADERBOARD_METRIC_STARS")).rejects.toMatchObject({ code: "invalid_argument" });
    await expect(call("LEADERBOARD_SUBJECT_UNSPECIFIED", "LEADERBOARD_METRIC_LANDS")).rejects.toMatchObject({ code: "invalid_argument" });
    await expect(call("LEADERBOARD_SUBJECT_AGENTS", "LEADERBOARD_METRIC_UNSPECIFIED")).rejects.toMatchObject({ code: "invalid_argument" });
  });

  it("ranks people by real counts, score descending then name", async () => {
    const { anon, mara } = world();
    const agents = await anon.search.listLeaderboard({ window: MONTHLY, subject: "LEADERBOARD_SUBJECT_AGENTS", metric: "LEADERBOARD_METRIC_LANDS" });
    expect(agents.entries.every((e) => e.user && !e.user.isOrg && e.repo === undefined)).toBe(true);
    expect(agents.entries.map((e) => e.rank)).toEqual(agents.entries.map((_, i) => i + 1));
    const scores = agents.entries.map((e) => Number(e.score));
    expect(scores).toEqual([...scores].sort((a, b) => b - a));
    // Public repos only: patcher landed 2 in engine, 2 in agent-kit, 2 in docs this month.
    expect(agents.entries.find((e) => e.user?.login === "patcher")?.score).toBe("6");
    const humans = await anon.search.listLeaderboard({ window: MONTHLY, subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_LANDS" });
    for (let i = 1; i < humans.entries.length; i++) {
      const [a, b] = [humans.entries[i - 1], humans.entries[i]];
      if (a?.score === b?.score) expect((a?.user?.login ?? "") < (b?.user?.login ?? "")).toBe(true);
    }
    expect(humans.entries.every((e) => e.user?.principal?.kind === "PRINCIPAL_KIND_HUMAN")).toBe(true);
    const reviews = await anon.search.listLeaderboard({ window: MONTHLY, subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_REVIEWS" });
    expect(reviews.entries.map((e) => e.user?.login)).toEqual(expect.arrayContaining(["rhea", "omar"]));
    expect(reviews.entries.some((e) => e.user?.login === "mara")).toBe(false);
    const signedIn = await mara.search.listLeaderboard({ window: MONTHLY, subject: "LEADERBOARD_SUBJECT_HUMANS", metric: "LEADERBOARD_METRIC_REVIEWS" });
    expect(signedIn.entries.some((e) => e.user?.login === "mara")).toBe(true);
  });

  it("ranks trending developers by lands and names their most-landed repo", async () => {
    const { anon } = world();
    const devs = await anon.search.listTrendingDevelopers({ window: WEEKLY });
    expect(devs.developers.every((d) => d.user?.principal?.kind === "PRINCIPAL_KIND_HUMAN")).toBe(true);
    const withAgents = await anon.search.listTrendingDevelopers({ window: WEEKLY, includeAgents: true });
    expect(withAgents.developers.length).toBeGreaterThan(devs.developers.length);
    expect(withAgents.developers.some((d) => d.user?.login === "patcher")).toBe(true);
    expect(withAgents.developers.every((d) => d.popularRepo?.visibility === "public")).toBe(true);
    expect(withAgents.developers.map((d) => d.rank)).toEqual(withAgents.developers.map((_, i) => i + 1));
  });
});

describe("topics", () => {
  it("serves a curated topic with repos ranked by stars", async () => {
    const { anon, mara } = world();
    const ts = await anon.search.getTopic({ name: "typescript" });
    expect(ts).toMatchObject({ name: "typescript", displayName: "TypeScript", curated: true });
    expect(ts.repoCount).toBe("2");
    expect(ts.topRepos.map((r) => r.fullName)).toEqual(["lattice/agent-kit", "acme/sdk"]);
    expect((await anon.search.getTopic({ name: "typescript", topRepoCount: 1 })).topRepos).toHaveLength(1);
    expect((await mara.search.getTopic({ name: "typescript" })).repoCount).toBe("4");
    expect((await anon.search.getTopic({ name: "ai-agents" })).name).toBe("agents");
    await expect(anon.search.getTopic({ name: "no-such-topic" })).rejects.toMatchObject({ code: "not_found", reason: "TOPIC_NOT_FOUND" });
    await expect(anon.search.getTopic({ name: "terraform" })).rejects.toMatchObject({ reason: "TOPIC_NOT_FOUND" });
    expect((await mara.search.getTopic({ name: "terraform" })).curated).toBe(false);
  });

  it("lists the featured topics", async () => {
    const { anon } = world();
    const { topics } = await anon.search.listFeaturedTopics({});
    expect(topics.length).toBeGreaterThanOrEqual(5);
    expect(topics.every((t) => t.featured)).toBe(true);
    expect(topics.map((t) => t.name)).toEqual(expect.arrayContaining(["agents", "rust", "vcs", "llm"]));
    expect((await anon.search.listFeaturedTopics({ page: { pageSize: 2 } })).topics).toHaveLength(2);
  });
});

describe("creating orgs and repos", () => {
  it("creates an org owned by the caller and then a usable repo", async () => {
    const { mara, anon, as } = world();
    const org = await mara.org.createOrg({ name: "newco", display: "NewCo" });
    expect(org).toMatchObject({ name: "newco", display: "NewCo" });
    expect((await mara.org.listOrgs({})).orgs.map((o) => o.name)).toEqual(["acme", "newco"]);
    expect((await mara.org.listMembers({ org: "newco" })).members.map((m) => [m.principal?.display, m.role])).toEqual([["Mara Okafor", "ROLE_OWNER"]]);
    const repo = await mara.repo.createRepo({ org: "newco", name: "hello", description: "First repo", topics: ["Rust", "cli", "rust"], autoInit: true, licenseTemplate: "mit" });
    expect(repo).toMatchObject({ fullName: "newco/hello", visibility: "VISIBILITY_PRIVATE", topics: ["rust", "cli"], licenseSpdx: "MIT" });
    const home = await mara.repo.getRepoHome({ fullName: "newco/hello" });
    expect(home.readme?.markdown).toContain("# hello");
    expect(home.latestLand?.meta?.message).toBe("Initial commit");
    expect((await mara.repo.listRepos({ org: "newco" })).repos.map((r) => r.fullName)).toEqual(["newco/hello"]);
    await expect(anon.repo.getRepo({ repo: { fullName: "newco/hello" } })).rejects.toMatchObject({ reason: "REPO_NOT_FOUND" });
    await expect(as("someone-else-token-9").repo.getRepo({ repo: { fullName: "newco/hello" } })).resolves.toBeTruthy();
    const pub = await mara.repo.createRepo({ org: "newco", name: "open", visibility: "VISIBILITY_PUBLIC", autoInit: true });
    expect(pub.visibility).toBe("VISIBILITY_PUBLIC");
    expect((await anon.search.searchRepos({ query: "org:newco" })).hits.map((h) => h.fullName)).toEqual(["newco/open"]);
    expect((await mara.insights.getLanguages({ repo: { fullName: "newco/hello" } })).languages).toEqual([]);
  });

  it("can leave a repo empty", async () => {
    const { mara } = world();
    await mara.repo.createRepo({ org: "acme", name: "scratch" });
    const home = await mara.repo.getRepoHome({ fullName: "acme/scratch" });
    expect(home.readme).toBeUndefined();
    expect(home.latestLand).toBeUndefined();
    expect((await mara.repo.getRepo({ repo: { fullName: "acme/scratch" } })).pushedAt).toBeUndefined();
  });

  it("rejects bad, reserved and taken names, and callers outside the org", async () => {
    const { mara } = world();
    await expect(mara.org.createOrg({ name: "Bad Name" })).rejects.toMatchObject({ code: "invalid_argument", reason: "INVALID_NAME" });
    await expect(mara.org.createOrg({ name: "-lead" })).rejects.toMatchObject({ reason: "INVALID_NAME" });
    await expect(mara.org.createOrg({ name: "a".repeat(40) })).rejects.toMatchObject({ reason: "INVALID_NAME" });
    for (const name of ["api", "admin", "explore", "trending", "topics", "leaderboards", "repositories", "pricing", "new", "prompts", "signin", "orgs", "import", "search", "settings"])
      await expect(mara.org.createOrg({ name })).rejects.toMatchObject({ code: "invalid_argument", reason: "RESERVED_NAME" });
    await expect(mara.org.createOrg({ name: "lattice" })).rejects.toMatchObject({ code: "already_exists", reason: "ORG_EXISTS" });
    await expect(mara.org.createOrg({ name: "rhea" })).rejects.toMatchObject({ reason: "ORG_EXISTS" });
    await expect(mara.repo.createRepo({ org: "acme", name: "api" })).rejects.toMatchObject({ code: "already_exists", reason: "REPO_EXISTS" });
    await expect(mara.repo.createRepo({ org: "acme", name: "Bad Repo" })).rejects.toMatchObject({ reason: "INVALID_NAME" });
    await expect(mara.repo.createRepo({ org: "acme", name: "ok", topics: ["Bad Topic"] })).rejects.toMatchObject({ reason: "INVALID_TOPIC" });
    await expect(mara.repo.createRepo({ org: "lattice", name: "nope" })).rejects.toMatchObject({ code: "permission_denied", reason: "NOT_A_MEMBER" });
    await expect(mara.repo.createRepo({ org: "ghost-org", name: "x" })).rejects.toMatchObject({ code: "not_found", reason: "ORG_NOT_FOUND" });
      });
});
