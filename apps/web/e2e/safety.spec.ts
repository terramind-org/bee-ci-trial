import { expect, test } from "@playwright/test";

const repo = "/acme/api/safety";
const org = "/orgs/acme/settings/safety";
const ask = "Prepare a redacted incident summary from the health route access logs.";

test.describe("Safety", () => {
  test.skip(process.env.E2E_ADAPTER !== "fixture", "Uses the fixture Safety evidence and its organisation owner.");

  test.beforeEach(async ({ page }) => {
    const session = await page.request.post("/api/session", { data: { token: "demo-token-1" } });
    expect(session.ok()).toBe(true);
  });

  test("opens a finding and shows the same record in agent view", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(repo + "?mode=keeper");
    const content = page.getByRole("main");
    await expect(content.getByRole("heading", { level: 1 })).toHaveCount(1);
    await expect(content.getByText("Detected", { exact: false }).first()).toBeVisible();
    await expect(content.getByText("Reported by first-pass", { exact: false })).toBeVisible();
    await expect(content.getByText("asked for help", { exact: false })).toBeVisible();
    await content.getByRole("link", { name: ask, exact: true }).click();
    await expect(content.getByRole("heading", { level: 1, name: ask })).toBeVisible();
    const asked = await content.getByRole("heading", { name: "Asked", exact: true }).boundingBox();
    const done = await content.getByRole("heading", { name: "Done", exact: true }).boundingBox();
    expect(asked!.y).toBeLessThan(done!.y);
    expect(asked!.x).toBe(done!.x);
    await expect(content.getByText("Not recorded by the runner", { exact: true })).toHaveCount(2);
    await expect(content.getByText("Review submitted", { exact: true })).toBeVisible();

    const record = new URL(page.url());
    record.searchParams.set("mode", "bee");
    await page.goto(record.toString());
    const facts = page.locator('[data-bee-page="safety-record"]');
    await expect(facts.getByRole("heading", { level: 1 })).toContainText(ask);
    await expect(facts.locator('[data-tool="safety_raise_concern"]')).toContainText("never counts against you");
    await expect(facts.locator('[data-tool="safety_set_verdict"]')).toHaveAttribute("data-kind", "needs a person");
  });

  test("requests a server spot check of work merged without a person", async ({ page }) => {
    await page.goto(repo + "?mode=keeper");
    await page.getByRole("link", { name: "Show me a sample of what merged without a person" }).click();
    await expect(page).toHaveURL(/sample=\d+#spot-check/);
    await expect(page.getByText("Sample from the last 30 days.")).toBeVisible();
    await expect(page.getByText("No qualifying merges were found in this period.")).toBeVisible();
  });

  test("asks for confirmation before stopping work and shows the freeze across routes", async ({ page }) => {
    await page.goto(org + "?mode=keeper");
    await expect(page.getByRole("heading", { name: "By signal" })).toBeVisible();
    await page.getByLabel("Reason for stopping").fill("Pause new agent merges while we review the incident.");
    await page.getByRole("button", { name: "Stop everything" }).click();
    const confirm = page.getByRole("alertdialog");
    await expect(confirm).toContainText("Stops running agent work and freezes agent merges until resumed. A person can still merge.");
    await confirm.getByRole("button", { name: "Stop everything" }).click();
    try {
      await expect(page.getByRole("button", { name: "Resume", exact: true })).toBeVisible();
      await page.goto(repo + "?mode=bee");
      await expect(page.getByText("This organisation has stopped agent work and frozen agent merges.", { exact: false })).toBeVisible();
      await expect(page.locator('[data-bee-page="repo-safety"]')).toBeVisible();
    } finally {
      await page.goto(org + "?mode=keeper");
      await page.getByLabel("Reason for resuming").fill("Finish the Safety screen check.");
      await page.getByRole("button", { name: "Resume", exact: true }).click();
      await confirm.getByRole("button", { name: "Resume", exact: true }).click();
      await expect(page.getByRole("button", { name: "Stop everything" })).toBeVisible();
    }
  });
});
