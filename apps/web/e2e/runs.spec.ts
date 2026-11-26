import { expect, test } from "@playwright/test";
import { expectAccessible, repoPath, seed } from "./helpers";

test.describe("CI", () => {
  test("leads with failures and folds passing history into one line", async ({ page }) => {
    await page.goto(repoPath("runs"));
    await expect(page.getByRole("heading", { level: 1, name: "CI" })).toBeVisible();
    const failed = page.getByRole("region", { name: "Failed", exact: true });
    const row = failed.getByRole("listitem").filter({ hasText: seed.changes.fix.title }).first();
    await expect(row).toContainText("#" + seed.changes.fix.number);
    await expect(row).toContainText("Pull request");
    await expect(row).toContainText("lint failed");
    await expect(row.locator('[data-state="failed"]')).toHaveText("Failed");
    await expect(row.locator("time")).toBeVisible();

    const passing = page.locator("details").filter({ has: page.locator("summary", { hasText: /runs? passed this week/ }) });
    await expect(passing).not.toHaveAttribute("open");
    await expect(passing.getByRole("link").first()).toBeHidden();
    await passing.locator("summary").click();
    await expect(passing).toHaveAttribute("open", "");
    await expect(passing.getByRole("listitem").filter({ hasText: "Push" }).first()).toContainText("main");
  });

  test("filters by status and puts workflows one click away", async ({ page }) => {
    await page.goto(repoPath("runs"));
    await page.locator("summary", { hasText: "Find a run" }).click();
    await page.getByLabel("Status", { exact: true }).selectOption("failed");
    await page.getByRole("button", { name: "Apply filters" }).click();
    await expect(page).toHaveURL(/[?&]status=failed/);
    await expect(page.getByRole("region", { name: "Failed", exact: true }).getByRole("listitem")).toHaveCount(1);
    await expect(page.getByRole("region", { name: "Running", exact: true })).toHaveCount(0);
    await page.getByRole("link", { name: "Workflows", exact: true }).click();
    await expect(page).toHaveURL(/\/runs\/workflows$/);
    await expect(page.getByRole("heading", { level: 1, name: "Workflows" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Download badge" }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Disable workflow" }).first()).toHaveAttribute("data-agent-confirm", /Disables this workflow/);
  });

  test("a failed run opens with a What happened summary in words", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    const summary = page.getByTestId("what-happened");
    await expect(summary.getByRole("heading", { name: "What happened" })).toBeVisible();
    await expect(summary.getByTestId("run-sentence")).toHaveText(/^Failed( after [\dhms ]+)?: \d+ of \d+ jobs? failed \(\S+.*\)$/);
    const top = (loc: ReturnType<typeof page.locator>) => loc.evaluate((el) => el.getBoundingClientRect().top);
    expect(await top(summary)).toBeLessThan(await top(page.getByRole("region", { name: "Jobs" })));
    await expect(summary).toContainText(/annotations? points? at code/);
  });

  test("Copy as lives in the run page menu", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await expect(page.getByRole("main").getByRole("button", { name: "Copy as", exact: true })).toHaveCount(0);
    await page.getByRole("main").getByRole("button", { name: "More actions" }).last().click();
    await expect(page.getByRole("menuitem", { name: "Copy as CLI, HTTP or MCP", exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await expectAccessible(page);
  });

  test("opens failed jobs and their evidence by default, and re-runs failed jobs", async ({ page }) => {
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await expect(page.getByRole("heading", { level: 1 })).toContainText("#" + seed.runs.failedNumber);
    const jobs = page.getByRole("region", { name: "Jobs" });
    const failedJob = jobs.locator("details").filter({ has: page.locator("summary", { hasText: /lint.*Failed/ }) });
    await expect(failedJob).toHaveAttribute("open", "");
    const passedJobs = jobs.locator("details").filter({ has: page.locator("summary", { hasText: "Passed" }) });
    await expect(passedJobs.locator("[data-testid=log]")).toHaveCount(0);
    await expect(failedJob.getByTestId("log")).toContainText("Process completed with exit code 1.");
    await expect(failedJob.getByTestId("log")).toContainText("Tax must be rounded before it is added to the total");
    const notes = failedJob.getByRole("region", { name: "Annotations" });
    await expect(notes).toContainText("src/billing/invoice.ts:27");
    await notes.getByRole("link", { name: /src\/billing\/invoice.ts:27/ }).click();
    await expect(page).toHaveURL(/\/code\?path=src%2Fbilling%2Finvoice.ts|\/code\?path=src\/billing\/invoice.ts/);
    await page.goto(repoPath("runs/" + seed.runs.failed));
    await page.getByRole("button", { name: "Re-run failed jobs" }).click();
    await expect(page.getByText("attempt 2").first()).toBeVisible();
  });

  test("shows the same run facts and tools in agent mode", async ({ page }) => {
    await page.goto(repoPath("runs?mode=bee"));
    await expect(page.locator('[data-bee-page="ci"]')).toContainText(seed.changes.fix.title);
    await expect(page.locator('[data-tool="workflow_list_workflow_runs"]')).toBeVisible();
    await page.goto(repoPath("runs/" + seed.runs.failed + "?mode=bee"));
    await expect(page.locator('[data-bee-page="run"]')).toContainText("What happened");
    await expect(page.locator('[data-tool="workflow_tail_workflow_job_logs"]')).toBeVisible();
    await expect(page.locator('[data-tool="workflow_cancel_workflow_run"]')).toHaveAttribute("data-kind", "needs a person");
  });
});
