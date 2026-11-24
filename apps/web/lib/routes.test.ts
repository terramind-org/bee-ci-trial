import { NextRequest } from "next/server";
import { describe, expect, it } from "vitest";
import { proxy } from "../proxy";
import { AGENTS_FALLBACK, NAV, isActive, navHref, railItems, repoMore, repoTabs } from "./nav";
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
  });
});

describe("nav", () => {
  it("resolves repo, org and global items against the current repo", () => {
    const byId = (id: string) => NAV.find((n) => n.id === id)!;
    expect(navHref(byId("changes"), "acme/api")).toBe("/acme/api/changes");
    expect(navHref(byId("settings"), "acme/api")).toBe("/acme/api/settings");
    expect(navHref(byId("org-settings"), "acme/api")).toBe("/orgs/acme/settings");
    expect(navHref(byId("search"), "acme/api")).toBe("/search");
    expect(new Set(NAV.map((n) => n.key)).size).toBe(NAV.length);
    expect(repoTabs().map((t) => t.label)).toEqual(["Code", "Change requests", "Issues", "Runs", "Updates"]);
    expect(repoMore().map((t) => t.label)).toEqual(["Activity", "Agents", "Releases", "Packages", "Pages", "Security", "Settings"]);
  });

  it("puts the global destinations in the rail, Inbox and Feed only when signed in", () => {
    expect(railItems(true).map((n) => n.label)).toEqual(["Inbox", "Feed", "Agents", "Repositories", "Explore"]);
    expect(railItems(false).map((n) => n.label)).toEqual(["Agents", "Repositories", "Explore"]);
    const agents = NAV.find((n) => n.id === "agents")!;
    expect(navHref(agents, "acme/api")).toBe("/acme/api/agents");
    expect(navHref(agents, "acme/")).toBe(AGENTS_FALLBACK);
  });

  it("keeps the old routes behind the renamed sections", () => {
    const byId = (id: string) => NAV.find((n) => n.id === id)!;
    expect(navHref(byId("work"), "acme/api")).toBe("/acme/api/work");
    expect(navHref(byId("activity"), "acme/api")).toBe("/acme/api/activity");
    expect(isActive(byId("inbox"), "/", "")).toBe(true);
    expect(isActive(byId("inbox"), "/explore", "")).toBe(false);
    expect(isActive(byId("changes"), "/acme/api/changes/01H", "acme/api")).toBe(true);
  });
});
