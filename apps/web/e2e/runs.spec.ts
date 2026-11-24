import { expect, test } from "@playwright/test";
import { expectAccessible, repoPath, seed } from "./helpers";

test.describe("Runs", () => {
  test("lists workflow runs with their change, trigger and job icons", async ({ page }) => {
    await page.goto(repoPath("runs"));
    const rows = page.locator("[data-nav-item]");
    await expect(rows.first()).toBeVisible();
    await expect(rows.filter({ hasText: seed.changes.fix.title }).first()).toContainText("pull request");
    await expect(rows.filter({ hasText: "push" }).first()).toContainText("main");
    await expect(rows.filter({ hasText: "failed" }).first()).toBeVisible();
  });

  test("filters runs by status", async ({ page }) => {
    await page.goto(repoPath("runs"));
    await page.getByRole("link", { name: /^Failed/ }).click();
    await expect(page).toHaveURL(/status=failed/);
    const rows = page.locator("[data-nav-item]");
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toContainText("failed");
  });

  test("a failed run opens with a What happened summary in words", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    const summary = page.getByTestId("what-happened");
    await expect(summary.getByRole("heading", { name: "What happened" })).toBeVisible();
    // "Failed after 3m 12s: 1 of 4 jobs failed (test)": the outcome, the clock, and which jobs.
    await expect(summary.getByTestId("run-sentence")).toHaveText(/^Failed( after [\dhms ]+)?: \d+ of \d+ jobs? failed \(\S+.*\)$/);
    // It sits above the jobs and the log.
    const top = (loc: ReturnType<typeof page.locator>) => loc.evaluate((el) => el.getBoundingClientRect().top);
    expect(await top(summary)).toBeLessThan(await top(page.getByRole("navigation", { name: "Jobs" })));
    await expect(summary).toContainText(/annotations? points? at code/);
  });

  test("Copy as lives in the page menu, not as an inline button", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await expect(page.getByRole("main").getByRole("button", { name: "Copy as", exact: true })).toHaveCount(0);
    await page.getByRole("main").getByRole("button", { name: "More actions" }).last().click();
    await expect(page.getByRole("menuitem", { name: /Copy as/ })).toBeVisible();
    await page.keyboard.press("Escape");
    await expectAccessible(page);
  });

  test("failed run shows jobs, the log, the annotation and reruns failed jobs", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await expect(page.getByRole("heading", { level: 1 })).toContainText("#" + seed.runs.failedNumber);
    await page.getByRole("link", { name: /lint/ }).first().click();
    await expect(page.getByTestId("log")).toContainText("Process completed with exit code 1.");
    await expect(page.getByTestId("log")).toContainText("Tax must be rounded before it is added to the total");
    const notes = page.getByRole("region", { name: "Annotations" });
    await expect(notes).toContainText("src/billing/invoice.ts:27");
    await notes.getByRole("link", { name: /src\/billing\/invoice.ts:27/ }).click();
    await expect(page).toHaveURL(/\/code\?path=src%2Fbilling%2Finvoice.ts|\/code\?path=src\/billing\/invoice.ts/);
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await page.getByRole("button", { name: "Rerun failed jobs" }).click();
    await expect(page.getByText("attempt 2").first()).toBeVisible();
  });
});
