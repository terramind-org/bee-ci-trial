import { expect, test } from "@playwright/test";
import { pressUntil, repoPath, seed } from "./helpers";

test.describe("Shell, palette and keyboard", () => {
  test("command palette jumps to a pull request by title", async ({ page }) => {
    await page.goto(repoPath("activity"));
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await pressUntil(page, "ControlOrMeta+k", () => expect(palette).toBeVisible({ timeout: 1000 }));
    await page.getByPlaceholder(/Jump to a pull request/).fill("rotate session");
    await expect(palette.getByRole("option", { name: /Rotate session ids on role change/ })).toBeVisible();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(seed.changes.rotate.title);
  });

  test("g-chords navigate, ? lists shortcuts, y opens copy-as", async ({ page }) => {
    await page.goto(repoPath("activity"));
    // The chord attaches once the page hydrates.
    await expect(async () => {
      await page.locator("body").press("g");
      await page.locator("body").press("a");
      await expect(page).toHaveURL(new RegExp(repoPath("agents") + "$"), { timeout: 1000 });
    }).toPass({ timeout: 15_000 });
    await page.locator("body").press("?");
    await expect(page.getByRole("dialog", { name: "Keyboard shortcuts" })).toContainText("Go to Packages");
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await pressUntil(page, "y", () => expect(page.getByRole("dialog").getByLabel("CLI command")).toContainText("--org " + seed.org, { timeout: 1000 }));
  });

  test("j and k move focus through list items, / focuses the filter", async ({ page }) => {
    await page.goto(repoPath("changes"));
    await expect(page.locator("[data-nav-item]").nth(1)).toBeVisible();
    await page.locator("body").press("j");
    await expect(page.locator("[data-nav-item]").first()).toBeFocused();
    await page.keyboard.press("j");
    await expect(page.locator("[data-nav-item]").nth(1)).toBeFocused();
    await page.keyboard.press("k");
    await expect(page.locator("[data-nav-item]").first()).toBeFocused();
    await page.keyboard.press("/");
    await expect(page.getByLabel("Filter changes")).toBeFocused();
  });

  test("the theme follows the system until a choice is made, then the choice persists", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "dark" });
    await page.goto(repoPath("activity"));
    const theme = () => page.evaluate(() => document.documentElement.dataset.theme);
    const switcher = page.getByRole("complementary", { name: "Beehive" }).getByRole("group", { name: "Theme" });
    // Nothing stored: the system decides, and the System position is the pressed one.
    expect(await theme()).toBe("dark");
    await expect(switcher.getByRole("button", { name: "Follow the system theme" })).toHaveAttribute("aria-pressed", "true");
    await page.emulateMedia({ colorScheme: "light" });
    await expect.poll(theme).toBe("light");
    // A choice overrides the system and survives a reload.
    await switcher.getByRole("button", { name: "Switch to dark theme" }).click();
    expect(await theme()).toBe("dark");
    await page.reload();
    expect(await theme()).toBe("dark");
    await expect(switcher.getByRole("button", { name: "Switch to dark theme" })).toHaveAttribute("aria-pressed", "true");
    // System forgets the choice.
    await switcher.getByRole("button", { name: "Follow the system theme" }).click();
    expect(await theme()).toBe("light");
  });

  test("switching repository rescopes every screen", async ({ page }) => {
    await page.goto(repoPath("code"));
    await expect(page.getByRole("region", { name: "README.md" })).toContainText("Invoicing service");
    // The palette switches to the same section of another repository.
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await pressUntil(page, "ControlOrMeta+k", () => expect(palette).toBeVisible({ timeout: 1000 }));
    await palette.getByRole("option", { name: "Switch to dev/website", exact: true }).click();
    await expect(page).toHaveURL(/\/dev\/website\/code$/);
    await expect(page.getByRole("region", { name: "README.md" })).toContainText("website");
    await expect(page.getByRole("region", { name: "README.md" })).not.toContainText("Invoicing service");
    // The palette's "Go to repo…" lists the org's repositories.
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByRole("dialog", { name: "Command palette" }).getByRole("option", { name: /Go to repo/ }).click();
    await page.getByRole("dialog", { name: "Command palette" }).getByRole("option", { name: seed.repo, exact: true }).click();
    await expect(page).toHaveURL(new RegExp(repoPath() + "$"));
    await page.goto(repoPath("code"));
    await expect(page.getByRole("region", { name: "README.md" })).toContainText("Invoicing service");
  });

  test("works at 390px: navigation drawer and review on a phone", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(repoPath("activity"));
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
    await page.getByRole("button", { name: "Open navigation" }).click();
    await page.getByRole("dialog", { name: "Navigation" }).getByRole("link", { name: "Pull requests" }).click();
    await expect(page).toHaveURL(new RegExp(repoPath("changes") + "$"));
    await page.locator("[data-nav-item]").filter({ hasText: seed.changes.fix.title }).click();
    await expect(page.getByTestId("land-panel").first()).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
  });

  test("serves an installable web app manifest and makes no third-party requests", async ({ page, request }) => {
    const res = await request.get("/manifest.webmanifest");
    expect(res.ok()).toBe(true);
    expect((await res.json()).display).toBe("standalone");
    const foreign: string[] = [];
    page.on("request", (r) => {
      if (!r.url().startsWith("http://localhost")) foreign.push(r.url());
    });
    for (const p of ["activity", "changes", "code", "agents"]) await page.goto(repoPath(p));
    expect(foreign).toEqual([]);
  });

  test("the browser reaches the server only through the session proxy", async ({ page }) => {
    const calls: { url: string; auth: string | null }[] = [];
    page.on("request", (r) => {
      if (r.url().includes("/bee.v1.")) calls.push({ url: r.url(), auth: r.headers()["authorization"] ?? null });
    });
    await page.goto(repoPath("activity"));
    await expect(page.getByTestId("event-list").locator("li").first()).toBeVisible();
    expect(calls.length).toBeGreaterThan(0);
    for (const c of calls) {
      expect(new URL(c.url).pathname.startsWith("/api/connect/bee.v1.")).toBe(true);
      expect(c.auth).toBeNull();
    }
  });

  test("unknown routes and ids render the not-found page", async ({ page }) => {
    await page.goto(repoPath("changes/01NOTAREALCHANGE0000000000"));
    await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
  });
});
