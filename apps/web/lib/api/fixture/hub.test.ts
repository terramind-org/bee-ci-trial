import { describe, expect, it } from "vitest";
import { createClient } from "../client";
import { FixtureStore, fixtureRpc } from "./index";
import { pathText } from "../ids";

const world = () => {
  const store = new FixtureStore({ anchor: Date.UTC(2026, 9, 18, 9, 0, 0), clock: () => 0, live: false });
  return { mara: createClient(fixtureRpc(store, "mara-token-0001")), anon: createClient(fixtureRpc(store)) };
};

describe("apps, teams, packages, pages and code search in the fixture", () => {
  it("lists the org's apps and what is installed on it", async () => {
    const { mara } = world();
    const { apps, installations } = await mara.app.listApps({ org: "acme" });
    expect(apps.map((a) => a.slug)).toEqual(["release-notes", "status-sync"]);
    expect(installations.map((i) => i.appSlug)).toEqual(["release-notes"]);
  });

  it("lists teams with their policy groups and one child team", async () => {
    const { mara } = world();
    const { teams } = await mara.team.listTeams({ org: "acme" });
    expect(teams.map((t) => t.policyGroup)).toContain("@acme/billing");
    const billing = teams.find((t) => t.slug === "billing");
    expect(billing?.parentTeamId).toBe(teams.find((t) => t.slug === "platform")?.id);
  });

  it("lists packages with their newest version, and hides non-public ones from a signed-out visitor", async () => {
    const { mara, anon } = world();
    const { packages } = await mara.package.listPackages({ owner: "acme" });
    expect(packages.map((p) => p.package.name)).toEqual(["console", "@acme/fleet-client", "acme-telemetry"]);
    expect(packages[0]?.latest?.version).toBe("1.8.2");
    expect((await anon.package.listPackages({ owner: "acme" })).packages.map((p) => p.package.name)).toEqual(["acme-telemetry"]);
  });

  it("serves a Pages site for acme/api and none for a repo without one", async () => {
    const { mara } = world();
    const view = await mara.pages.getPages({ repo: { fullName: "acme/api" } });
    expect(view.site?.status).toBe("PAGES_SITE_STATUS_BUILT");
    expect(view.deployments.length).toBeGreaterThan(2);
    expect((await mara.pages.getPages({ repo: { fullName: "acme/web" } })).site).toBeUndefined();
  });

  it("searches code with qualifiers and returns nothing for a bare qualifier", async () => {
    const { mara } = world();
    const res = await mara.search.searchCode({ query: "invoice lang:TypeScript repo:acme/api" });
    expect(res.hits.length).toBeGreaterThan(0);
    for (const h of res.hits) {
      expect(h.repoFullName).toBe("acme/api");
      expect(pathText(h.path ?? "")).toMatch(/\.ts$/);
      expect(h.lineMatches?.length).toBeGreaterThan(0);
    }
    expect((await mara.search.searchCode({ query: "repo:" })).hits).toEqual([]);
  });
});
