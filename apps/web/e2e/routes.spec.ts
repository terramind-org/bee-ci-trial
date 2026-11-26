import { expect, test } from "@playwright/test";
import { orgPath, repoPath, seed } from "./helpers";

/** Root-level URLs from before repos had their own: each answers 308 to its new home. */
const LEGACY: [string, string][] = [
  ["/activity", repoPath("activity")],
  ["/activity?scope=all", repoPath("activity?scope=all")],
  ["/changes", repoPath("changes")],
  ["/changes?state=landed&q=label%3Adocs", repoPath("changes?state=landed&q=label%3Adocs")],
  ["/changes/" + seed.changes.fix.id + "?rev=1", repoPath("changes/" + seed.changes.fix.id + "?rev=1")],
  ["/code", repoPath("code")],
  ["/code?path=src%2Fbilling%2Ftax.ts&view=blame", repoPath("code?path=src%2Fbilling%2Ftax.ts&view=blame")],
  ["/agents", repoPath("agents")],
  ["/agents/" + seed.agents.claude.id, repoPath("agents/" + seed.agents.claude.id)],
  ["/runs", repoPath("runs")],
  ["/runs/" + seed.runs.failed, repoPath("runs/" + seed.runs.failed)],
  ["/work", repoPath("work")],
  ["/work/board", repoPath("work/board")],
  ["/work/" + seed.issues.drift, repoPath("work/" + seed.issues.drift)],
  ["/releases", repoPath("releases")],
  ["/packages", repoPath("packages")],
  ["/pages", repoPath("pages")],
  ["/security", repoPath("security")],
  ["/settings", orgPath()],
  ["/settings/audit?action=release.", orgPath("audit?action=release.")],
  ["/settings/members", orgPath("members")],
];

test.describe("Legacy URLs", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("each old path answers 308 to the remembered repo, keeping the query", async ({ request }) => {
    for (const [from, to] of LEGACY) {
      const res = await request.get(from, { maxRedirects: 0, headers: { cookie: "bee_repo=" + encodeURIComponent(seed.repo) } });
      expect(res.status(), from).toBe(308);
      expect(new URL(res.headers()["location"] ?? "", "http://x").pathname + new URL(res.headers()["location"] ?? "", "http://x").search, from).toBe(to);
    }
  });

  test("without a remembered repo they go to the repository list", async ({ request }) => {
    const res = await request.get("/changes", { maxRedirects: 0 });
    expect(res.status()).toBe(308);
    expect(new URL(res.headers()["location"] ?? "", "http://x").pathname).toBe("/repositories");
  });

  test("API routes and static files are untouched", async ({ request }) => {
    expect((await request.get("/manifest.webmanifest", { maxRedirects: 0 })).status()).toBe(200);
    expect((await request.get("/icon.svg", { maxRedirects: 0 })).status()).toBe(200);
    expect((await request.get("/api/session", { maxRedirects: 0 })).status()).not.toBe(308);
  });
});

test.describe("Owner routes", () => {
  test("a reserved first segment is never an owner", async ({ page }) => {
    for (const path of ["/orgs/" + seed.org + "/changes", "/search/" + seed.org + "/code", "/explore/" + seed.org + "/activity"]) {
      await page.goto(path);
      await expect(page.getByRole("heading", { name: "Nothing here" }), path).toBeVisible();
    }
  });

  test("a repo that does not exist is not found", async ({ page }) => {
    await page.goto("/" + seed.org + "/no-such-repo/changes");
    await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
  });

  test("repo pages carry the repo header, tabs and the scope in the URL", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const tabs = page.getByRole("navigation", { name: "Repository sections" });
    await expect(tabs.getByRole("link", { name: /^Pull requests/ })).toHaveAttribute("aria-current", "page");
    // The familiar tabs, in order; Settings shows to writers.
    await expect(tabs.getByRole("link")).toHaveText([/^Code/, /^Issues/, /^Pull requests/, /^Agents/, /^CI/, /^Safety/, /^Insights/, /^Settings/]);
    // The rarer sections sit under More.
    await tabs.getByRole("button", { name: "More" }).click();
    const more = page.getByRole("menu", { name: "More repository sections" });
    await expect(more.getByRole("menuitem")).toHaveText(["Releases", "Packages", "Pages", "Security", "Activity", "Updates"]);
    await page.keyboard.press("Escape");
    await tabs.getByRole("link", { name: "CI", exact: true }).click();
    await expect(page).toHaveURL(new RegExp(repoPath("runs") + "$"));
    await expect(page.getByRole("link", { name: seed.repo.split("/")[1], exact: true })).toHaveAttribute("href", repoPath());
  });
});

test.describe("Signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("a private repo's URL is a plain 404, without revealing it exists", async ({ page }) => {
    for (const sub of ["code", "changes", "settings"]) {
      const res = await page.goto("/" + seed.org + "/website/" + sub);
      expect(res?.status()).toBe(404);
      await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
    }
    // The same answer as for a repo that does not exist.
    expect((await page.goto("/" + seed.org + "/no-such-repo/code"))?.status()).toBe(404);
    await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
  });
});
