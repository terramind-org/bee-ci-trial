import { expect, test, type Page } from "@playwright/test";
import { orgPath, repoPath, seed } from "./helpers";

/** Main screens against the real server at 1440 and 390 px, into e2e/screenshots/ (gitignored). */
const SHOTS: { name: string; path: string; ready?: (p: Page) => Promise<void> }[] = [
  { name: "activity", path: repoPath("activity"), ready: async (p) => void (await expect(p.getByTestId("event-list").locator("li").first()).toBeVisible()) },
  { name: "changes", path: repoPath("changes") },
  { name: "change-detail", path: repoPath("changes/" + seed.changes.fix.id), ready: async (p) => void (await expect(p.getByTestId("land-panel").first()).toBeAttached()) },
  { name: "change-brief-ui", path: repoPath("changes/" + seed.changes.ui.id), ready: async (p) => void (await expect(p.locator("[data-outcome]")).toBeVisible()) },
  { name: "change-code", path: repoPath("changes/" + seed.changes.fix.id + "/code"), ready: async (p) => void (await expect(p.getByRole("heading", { name: "r2", exact: true })).toBeVisible()) },
  { name: "change-checks", path: repoPath("changes/" + seed.changes.fix.id + "/checks") },
  { name: "change-agents", path: repoPath("changes/" + seed.changes.ui.id + "/agents") },
  { name: "agents", path: repoPath("agents") },
  { name: "agent-detail", path: repoPath("agents/" + seed.agents.claude.id) },
  { name: "runs", path: repoPath("runs") },
  { name: "run-detail", path: repoPath("runs/" + seed.runs.failed), ready: async (p) => void (await expect(p.getByTestId("log")).toBeVisible()) },
  { name: "code", path: repoPath("code") },
  { name: "code-file", path: repoPath("code?path=src/billing/invoice.ts") },
  { name: "work", path: repoPath("work") },
  { name: "work-board", path: repoPath("work/board") },
  { name: "releases", path: repoPath("releases") },
  { name: "packages", path: repoPath("packages") },
  { name: "security", path: repoPath("security") },
  { name: "settings-policy", path: repoPath("settings") },
  { name: "settings-audit", path: orgPath("audit") },
  { name: "search", path: "/search" },
  { name: "import", path: "/import" },
];

for (const width of [1440, 390]) {
  test.describe("screenshots · " + width, () => {
    test.use({ viewport: { width, height: width === 390 ? 844 : 900 } });
    for (const s of SHOTS) {
      test("capture " + s.name, async ({ page }) => {
        await page.goto(s.path);
        await expect(page.locator("main")).toBeVisible();
        if (s.ready) await s.ready(page);
        await page.waitForTimeout(600);
        expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
        await page.screenshot({ path: "e2e/screenshots/" + s.name + "-" + width + ".png", fullPage: width === 1440 });
      });
    }
  });
}
