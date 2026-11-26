import { expect, test } from "@playwright/test";
import { expectAccessible, orgPath, pressUntil, repoPath, seed } from "./helpers";

/** Every screen, both themes, against the real server: zero serious or critical axe violations. */
const SCREENS = [
  repoPath("activity"),
  repoPath("changes"),
  repoPath("changes/" + seed.changes.fix.id),
  repoPath("changes/" + seed.changes.ui.id),
  repoPath("changes/" + seed.changes.fix.id + "/code"),
  repoPath("changes/" + seed.changes.fix.id + "/checks"),
  repoPath("changes/" + seed.changes.ui.id + "/agents"),
  repoPath("agents"),
  repoPath("agents/" + seed.agents.claude.id),
  repoPath("runs"),
  repoPath("runs/" + seed.runs.failed),
  repoPath("code"),
  repoPath("code?path=src/billing/invoice.ts"),
  repoPath("code?path=src/billing/tax.ts&view=blame"),
  repoPath("code?path=src/billing/tax.ts&view=history"),
  repoPath("code?view=compare"),
  repoPath("work"),
  repoPath("work/" + seed.issues.states),
  repoPath("work/board"),
  repoPath("releases"),
  repoPath("packages"),
  repoPath("pages"),
  repoPath("security"),
  "/search?q=invoiceTotal",
  repoPath("settings"),
  orgPath(),
  orgPath("audit"),
  orgPath("members"),
  orgPath("teams"),
  orgPath("apps"),
  "/import",
];

for (const theme of ["light", "dark"] as const) {
  test.describe("accessibility · " + theme, () => {
    test.use({ colorScheme: theme });

    for (const path of SCREENS) {
      test("axe: " + path, async ({ page }) => {
        await page.goto(path);
        await page.waitForLoadState("load");
        await expect(page.locator("main")).toBeVisible();
        await page.waitForTimeout(400);
        await expectAccessible(page);
      });
    }

    test("axe: command palette and copy-as dialogs", async ({ page }) => {
      await page.goto(repoPath("changes"));
      await expect(page.locator("[data-nav-item]").first()).toBeVisible();
      await pressUntil(page, "ControlOrMeta+k", () => expect(page.getByRole("dialog", { name: "Command palette" })).toBeVisible({ timeout: 1000 }));
      await page.waitForTimeout(400); // let the enter animation finish before measuring contrast
      await expectAccessible(page);
      await page.keyboard.press("Escape");
      // Copy as lives in the page's "…" menu (the last one in main; the repo header has its own).
      await page.locator("main").getByRole("button", { name: "More actions" }).last().click();
      await page.getByRole("menuitem", { name: /Copy as/ }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await page.waitForTimeout(400);
      await expectAccessible(page);
    });
  });
}

test.describe("accessibility · signed out", () => {
  test.use({ storageState: { cookies: [], origins: [] } });
  test("axe: /signin", async ({ page }) => {
    await page.goto("/signin");
    await expect(page.locator("main")).toBeVisible();
    await expectAccessible(page);
  });
});

test("axe: 390px layout with the navigation drawer open", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(repoPath("changes"));
  await page.getByRole("button", { name: "Open navigation" }).click();
  await expect(page.getByRole("dialog", { name: "Navigation" })).toBeVisible();
  await page.waitForTimeout(400);
  await expectAccessible(page);
});
