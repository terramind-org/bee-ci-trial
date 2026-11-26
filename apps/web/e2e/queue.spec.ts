import { expect, test } from "@playwright/test";
import { expectAccessible, pressUntil, repoPath, seed } from "./helpers";

const GROUPS = ["Needs you", "Ready to land", "Waiting on agents", "Blocked", "Recently landed"];

test.describe("Review queue", () => {
  test("groups changes in a fixed order, each change once", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const headings = page.locator("section[data-group] > h2");
    await expect(headings).toHaveCount(GROUPS.length);
    for (const [i, name] of GROUPS.entries()) await expect(headings.nth(i)).toContainText(name);
    // Every row is a link to its change, and no change is listed twice.
    const hrefs = await page.locator("section[data-group] a[data-nav-item]").evaluateAll((els) => els.map((e) => e.getAttribute("href")));
    expect(hrefs.length).toBeGreaterThan(0);
    expect(new Set(hrefs).size).toBe(hrefs.length);
  });

  test("a docs change with nothing owed is ready to land and says so in words", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const ready = page.locator("section[data-group='ready']");
    const row = ready.locator("a[data-nav-item]").filter({ hasText: seed.changes.docs.title });
    await expect(row).toBeVisible();
    await expect(row.getByTestId("status-sentence")).toContainText("Ready to land");
    await expect(row).toContainText("Docs");
  });

  test("a row shows the outcome line and the hero thumbnail from the brief", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const row = page.locator("a[data-nav-item]").filter({ hasText: seed.changes.ui.title });
    await expect(row).toContainText(seed.briefs.ui.outcome);
    const hero = row.getByTestId("hero-thumb").locator("img");
    await expect(hero).toHaveAttribute("alt", seed.briefs.ui.afterAlt);
    await expect.poll(() => hero.evaluate((img: HTMLImageElement) => img.complete && img.naturalWidth > 0)).toBe(true);
    // The thumbnail comes from this origin: the browser never reaches the bee server.
    expect(await hero.getAttribute("src")).toMatch(/^\/api\/evidence\//);
    // A change with no evidence keeps the tile and shows its state glyph instead of a picture.
    const bare = page.locator("a[data-nav-item]").filter({ hasText: seed.changes.rotate.title });
    await expect(bare.getByTestId("hero-thumb").locator("img")).toHaveCount(0);
  });

  test("the unanswered ask puts the change under Needs you, with the reason", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const needs = page.locator("section[data-group='needs-you']");
    const row = needs.locator("a[data-nav-item]").filter({ hasText: seed.changes.ui.title });
    await expect(row).toContainText("The author asks you to decide: " + seed.briefs.ui.ask.question);
  });

  test("a change that landed recently is listed under Recently landed", async ({ page }) => {
    await page.goto(repoPath("changes"));
    const landed = page.locator("section[data-group='landed']");
    await expect(landed.locator("a[data-nav-item]").filter({ hasText: seed.changes.bump.title })).toContainText("Merged");
  });

  test("the filter searches titles and outcome lines and hides empty groups", async ({ page }) => {
    await page.goto(repoPath("changes"));
    await page.getByLabel("Filter changes").fill(seed.briefs.cache.outcome.split(" ").slice(0, 3).join(" "));
    const rows = page.locator("a[data-nav-item]");
    await expect(rows.filter({ hasText: seed.changes.cache.title })).toHaveCount(1);
    await expect(rows.filter({ hasText: seed.changes.docs.title })).toHaveCount(0);
    expect(await page.locator("section[data-group]").count()).toBeLessThan(GROUPS.length);
    await page.getByLabel("Filter changes").fill("zzz-no-such-change");
    await expect(page.getByText("No matches")).toBeVisible();
  });

  test("the Landed and Closed tabs are flat lists in the same row", async ({ page }) => {
    await page.goto(repoPath("changes?state=landed"));
    await expect(page.locator("section[data-group] > h2")).toHaveCount(0);
    await expect(page.locator("a[data-nav-item]").filter({ hasText: seed.changes.bump.title })).toContainText("Merged");
    await page.goto(repoPath("changes?state=closed"));
    await expect(page.locator("a[data-nav-item]").filter({ hasText: seed.changes.closed.title })).toContainText("Closed without merging");
  });

  test("is keyboard navigable and accessible", async ({ page }) => {
    await page.goto(repoPath("changes"));
    await expect(page.locator("a[data-nav-item]").first()).toBeVisible();
    await pressUntil(page, "j", () => expect(page.locator("a[data-nav-item]:focus")).toHaveCount(1, { timeout: 1000 }));
    await expectAccessible(page);
  });
});
