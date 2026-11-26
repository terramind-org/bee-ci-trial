import { NextRequest } from "next/server";
import { describe, expect, it } from "vitest";
import { proxy } from "../proxy";
import { AGENTS_FALLBACK, NAV, isActive, navHref, orgRailItems, paletteExtras, railItems, repoMore, repoTabs, withRecent } from "./nav";
import { PATH_HEADER, RESERVED_OWNERS, isRepoFullName, legacyRedirect, orgSettingsHref, ownerHref, repoFromPath, repoHref, repoSubPath, safeNext } from "./routes";

describe("url helpers", () => {
  it("builds repo, owner and org settings URLs", () => {
    expect(repoHref("acme/api")).toBe("/acme/api");
    expect(repoHref("acme/api", "changes/01H")).toBe("/acme/api/changes/01H");
    expect(repoHref("acme/api", "/code?path=a.ts")).toBe("/acme/api/code?path=a.ts");
    expect(repoHref("acme/api", "?tab=x")).toBe("/acme/api?tab=x");
    expect(ownerHref("acme")).toBe("/acme");
    expect(orgSettingsHref("acme")).toBe("/orgs/acme/settings");
    expect(orgSettingsHref("acme", "members")).toBe("/orgs/acme/settings/members");
  });

  it("never treats a reserved first segment as an owner", () => {
    for (const r of RESERVED_OWNERS) {
      expect(isRepoFullName(r + "/x")).toBe(false);
      expect(repoFromPath("/" + r + "/x/y")).toBeNull();
    }
    expect(RESERVED_OWNERS).toEqual(expect.arrayContaining(["api", "admin", "explore", "orgs", "settings", "llms.txt", "_next", "feed.atom", "feed"]));
    expect(repoFromPath("/acme/api/changes/1")).toBe("acme/api");
    expect(repoFromPath("/acme")).toBeNull();
    expect(repoSubPath("/acme/api/changes/1")).toBe("changes/1");
    expect(repoSubPath("/acme/api")).toBe("");
    expect(isRepoFullName("acme/api/x")).toBe(false);
    expect(isRepoFullName("ac me/api")).toBe(false);
  });

  it("only follows same-origin paths after sign-in", () => {
    expect(safeNext("/acme/api")).toBe("/acme/api");
    expect(safeNext("//evil.example")).toBeNull();
    expect(safeNext("/\\evil.example")).toBeNull();
    expect(safeNext("https://evil.example")).toBeNull();
    expect(safeNext(null)).toBeNull();
  });
});

describe("legacy redirects", () => {
  it("moves root-level repo screens under the remembered repo, keeping the query", () => {
    expect(legacyRedirect("/activity", "", "acme/api")).toBe("/acme/api/activity");
    expect(legacyRedirect("/changes/01H", "?rev=2", "acme/api")).toBe("/acme/api/changes/01H?rev=2");
    expect(legacyRedirect("/work/board", "", "acme/api")).toBe("/acme/api/work/board");
    expect(legacyRedirect("/code", "?path=src%2Fa.ts", "acme/api")).toBe("/acme/api/code?path=src%2Fa.ts");
    for (const s of ["code", "agents", "runs", "releases", "packages", "pages", "security"]) expect(legacyRedirect("/" + s, "", "acme/api")).toBe("/acme/api/" + s);
  });

  it("moves settings to the org and falls back to the repository list", () => {
    expect(legacyRedirect("/settings", "", "acme/api")).toBe("/orgs/acme/settings");
    expect(legacyRedirect("/settings/audit", "?action=x", "acme/api")).toBe("/orgs/acme/settings/audit?action=x");
    expect(legacyRedirect("/changes", "", undefined)).toBe("/repositories");
    expect(legacyRedirect("/settings", "", "explore/x")).toBe("/repositories");
    expect(legacyRedirect("/acme/api/changes", "", "acme/api")).toBeNull();
    expect(legacyRedirect("/explore", "", "acme/api")).toBeNull();
    expect(legacyRedirect("/", "", "acme/api")).toBeNull();
  });

  it("proxy answers 308 for legacy paths and tags every other page with its path", () => {
    const legacy = new NextRequest("http://localhost/changes?state=all", { headers: { cookie: "bee_repo=acme%2Fapi" } });
    const res = proxy(legacy);
    expect(res.status).toBe(308);
    expect(res.headers.get("location")).toBe("http://localhost/acme/api/changes?state=all");
    const bare = proxy(new NextRequest("http://localhost/runs"));
    expect(bare.headers.get("location")).toBe("http://localhost/repositories");
    const page = proxy(new NextRequest("http://localhost/orgs/acme/settings?x=1"));
    expect(page.status).toBe(200);
    expect(page.headers.get("x-middleware-request-" + PATH_HEADER)).toBe("/orgs/acme/settings?x=1");
    expect(page.headers.get("set-cookie")).toBeNull();
  });

  it("proxy remembers a preview of the agent's view asked for in the URL, and nothing it does not know", () => {
    const bee = proxy(new NextRequest("http://localhost/acme/api?mode=bee"));
    expect(bee.status).toBe(200);
    expect(bee.headers.get("set-cookie")).toMatch(/^bee_mode=bee; Path=\/; .*SameSite=lax/i);
    expect(proxy(new NextRequest("http://localhost/acme/api?mode=keeper")).headers.get("set-cookie")).toMatch(/^bee_mode=keeper;/);
    expect(proxy(new NextRequest("http://localhost/acme/api?mode=admin")).headers.get("set-cookie")).toBeNull();
  });
});

describe("nav", () => {
  const byId = (id: string) => NAV.find((n) => n.id === id)!;

  it("resolves repo, org and global items against the current repo", () => {
    expect(navHref(byId("changes"), "acme/api")).toBe("/acme/api/changes");
    expect(navHref(byId("settings"), "acme/api")).toBe("/acme/api/settings");
    expect(navHref(byId("org-settings"), "acme/api")).toBe("/orgs/acme/settings");
    expect(navHref(byId("search"), "acme/api")).toBe("/search");
    expect(new Set(NAV.map((n) => n.key)).size).toBe(NAV.length);
    expect(new Set(NAV.map((n) => n.id)).size).toBe(NAV.length);
  });

  it("gives a repository the familiar tabs, Settings only to writers, and the rest under More", () => {
    expect(repoTabs().map((t) => t.label)).toEqual(["Code", "Issues", "Pull requests", "Agents", "CI", "Safety", "Insights", "Settings"]);
    expect(repoTabs(false).map((t) => t.label)).toEqual(["Code", "Issues", "Pull requests", "Agents", "CI", "Safety", "Insights"]);
    expect(repoMore().map((t) => t.label)).toEqual(["Releases", "Packages", "Pages", "Security", "Activity", "Updates"]);
  });

  it("keeps the routes under the renamed tabs, so old URLs still open", () => {
    expect(navHref(byId("work"), "acme/api")).toBe("/acme/api/work");
    expect(byId("work").label).toBe("Issues");
    expect(navHref(byId("changes"), "acme/api")).toBe("/acme/api/changes");
    expect(byId("changes").label).toBe("Pull requests");
    expect(navHref(byId("runs"), "acme/api")).toBe("/acme/api/runs");
    expect(byId("runs").label).toBe("CI");
    expect(navHref(byId("safety"), "acme/api")).toBe("/acme/api/safety");
    expect(navHref(byId("insights"), "acme/api")).toBe("/acme/api/insights");
    expect(navHref(byId("activity"), "acme/api")).toBe("/acme/api/activity");
  });

  it("puts Needs you, Feed and Repositories in the rail, the first two only when signed in", () => {
    expect(railItems(true).map((n) => n.label)).toEqual(["Needs you", "Feed", "Repositories"]);
    expect(railItems(false).map((n) => n.label)).toEqual(["Repositories"]);
    expect(navHref(byId("inbox"), "acme/api")).toBe("/");
  });

  it("adds the organization's group for a signed-in member, on the existing settings routes", () => {
    const org = orgRailItems(true, "acme");
    expect(org.map((n) => n.label)).toEqual(["People and teams", "Agents and budgets", "Record", "Safety"]);
    expect(org.map((n) => navHref(n, "acme/api", "acme"))).toEqual(["/orgs/acme/settings/members", "/orgs/acme/settings/agents", "/orgs/acme/settings/audit", "/orgs/acme/settings/safety"]);
    expect(orgRailItems(true, "")).toEqual([]);
    expect(orgRailItems(false, "acme")).toEqual([]);
  });

  it("lists in the palette every destination the chrome does not link to", () => {
    expect(paletteExtras(true, "acme").map((n) => n.id)).toEqual(["explore", "search", "org-settings", "import"]);
    // Without an organization there are no organization pages to offer; signed out, none that need a session.
    expect(paletteExtras(true, "").map((n) => n.id)).toEqual(["explore", "search", "import"]);
    expect(paletteExtras(false, "").map((n) => n.id)).toEqual(["explore", "search"]);
    // Every nav item is in the rail, the organization group, a repository's tabs or More, or the palette's extras.
    const placed = new Set([...railItems(true), ...orgRailItems(true, "acme"), ...repoTabs(), ...repoMore(), ...paletteExtras(true, "acme")].map((n) => n.id));
    expect(NAV.filter((n) => !placed.has(n.id))).toEqual([]);
  });

  it("opens the agent leaderboard for Agents when no repository is in reach", () => {
    const agents = byId("agents");
    expect(navHref(agents, "acme/api")).toBe("/acme/api/agents");
    expect(navHref(agents, "acme/")).toBe(AGENTS_FALLBACK);
  });

  it("marks the current item, Needs you only on the home page itself", () => {
    expect(isActive(byId("inbox"), "/", "")).toBe(true);
    expect(isActive(byId("inbox"), "/explore", "")).toBe(false);
    expect(isActive(byId("changes"), "/acme/api/changes/01H", "acme/api")).toBe(true);
    expect(isActive(byId("record"), "/orgs/acme/settings/audit", "acme/api", "acme")).toBe(true);
  });

  it("keeps the five repositories opened last, newest first, without repeats", () => {
    expect(withRecent([], "acme/api")).toEqual(["acme/api"]);
    expect(withRecent(["acme/web", "acme/api"], "acme/api")).toEqual(["acme/api", "acme/web"]);
    expect(withRecent(["a/1", "a/2", "a/3", "a/4", "a/5"], "a/6")).toEqual(["a/6", "a/1", "a/2", "a/3", "a/4"]);
  });
});
