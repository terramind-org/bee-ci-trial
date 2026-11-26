import { expect, test } from "@playwright/test";
import { repoRef } from "./seed/lib";
import { api, repoPath, seed } from "./helpers";

test.describe("Activity", () => {
  test("replays the server's events, then shows live ones on the same stream", async ({ page }) => {
    const subscribes: string[] = [];
    page.on("request", (r) => {
      if (r.url().includes("EventService/Subscribe")) subscribes.push(r.url());
    });
    await page.goto(repoPath("activity"));
    const list = page.getByTestId("event-list");
    await expect(list.locator("li").first()).toBeVisible();
    // Seeded history arrives as one replay: change requests, runs, releases.
    await expect(list).toContainText(seed.changes.bump.title);
    await expect(list.locator('li[data-kind="workflow_run.completed"]').first()).toBeVisible();

    // Something happens through the public API; it arrives without a reload or a poll.
    const title = "Live event " + Date.now();
    await api().issue.createIssue({ repo: repoRef(seed.repo), title, body: "from the e2e test" });
    await expect(list.locator("li").first()).toContainText(title);
    expect(subscribes.length).toBe(1);
  });

  test("filters by kind and to agents only", async ({ page }) => {
    await page.goto(repoPath("activity"));
    const list = page.getByTestId("event-list");
    await expect(list.locator("li").first()).toBeVisible();
    await page.getByRole("radio", { name: "Leases", exact: true }).click();
    await expect(list.locator("li").first()).toBeVisible();
    const kinds = await list.locator("li").evaluateAll((els) => els.map((e) => e.getAttribute("data-kind")));
    expect(kinds.length).toBeGreaterThan(0);
    expect(kinds.every((k) => k?.startsWith("lease."))).toBe(true);
    await expect(list).toContainText("src/billing");

    await page.getByRole("radio", { name: "Everything", exact: true }).click();
    await page.getByLabel("Agents only").check();
    const actors = await list.locator("li p > span:first-child").allInnerTexts();
    expect(actors.length).toBeGreaterThan(0);
    expect(actors.every((t) => t === "claude-code" || t === "codex")).toBe(true);
  });

  test("pause buffers new events and resume shows them", async ({ page }) => {
    await page.goto(repoPath("activity"));
    await expect(page.getByTestId("event-list").locator("li").first()).toBeVisible();
    await page.getByRole("button", { name: "Pause" }).click();
    const title = "Buffered event " + Date.now();
    await api().issue.createIssue({ repo: repoRef(seed.repo), title, body: "" });
    await expect(page.getByRole("button", { name: /Resume \(\d+ new\)/ })).toBeVisible();
    await expect(page.getByTestId("event-list")).not.toContainText(title);
    await page.getByRole("button", { name: /Resume/ }).click();
    await expect(page.getByTestId("event-list").locator("li").first()).toContainText(title);
  });

  test("groups events by day and by change request, each one a sentence", async ({ page }) => {
    await page.goto(repoPath("activity"));
    const list = page.getByTestId("event-list");
    await expect(list.locator("li").first()).toBeVisible();
    await expect(list.getByRole("heading", { level: 3 }).first()).toHaveText(/Today|Yesterday|\w+day, /);
    // A change request's events sit under one link to it, and read as "<who> <did what>".
    const group = list.locator("li").filter({ hasText: /merged it|opened it|pushed a new version of it/ }).first();
    await expect(group).toBeVisible();
    await expect(list.locator("li").filter({ hasText: "claude-code" }).first()).toContainText(/claude-code (opened|merged|pushed|claimed|let go|commented|approved|asked|added|wrote)/);
  });

  test("copy as shows the CLI, HTTP and MCP forms of the stream", async ({ page }) => {
    await page.goto(repoPath("activity"));
    await page.getByRole("button", { name: "More actions", exact: true }).click();
    await page.getByRole("menuitem", { name: /Copy as/ }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByLabel("CLI command")).toContainText("bee events --repo " + seed.repo + " --from 0 --follow --json");
    await dialog.getByRole("tab", { name: "HTTP" }).click();
    await expect(dialog.getByLabel("curl request")).toContainText("/bee.v1.EventService/Subscribe");
    await expect(dialog.getByLabel("curl request")).toContainText("application/connect+json");
    await dialog.getByRole("tab", { name: "MCP" }).click();
    await expect(dialog.getByLabel("MCP tool call")).toContainText('"name": "event_subscribe"');
  });

  test("side panels show who holds which lease and the merge queue", async ({ page }) => {
    await page.goto(repoPath("activity"));
    const leases = page.getByRole("region", { name: "Working now" });
    await expect(leases.locator("li").filter({ hasText: "src/billing" })).toContainText("claude-code");
    await expect(leases.locator("li").filter({ hasText: "src/auth" })).toContainText("codex");
    await expect(leases.locator("li").filter({ hasText: "src/auth" })).toContainText("exclusive");
    await expect(page.getByRole("region", { name: /Merge queue/ })).toBeVisible();
    await expect(page.getByText(/open change requests?,/)).toBeVisible();
    await expect(page.getByText(/agents? (is|are) working/)).toBeVisible();
  });

  test("org-wide scope streams every repo of the org", async ({ page }) => {
    await page.goto(repoPath("activity?scope=all"));
    const list = page.getByTestId("event-list");
    await expect(list.locator('li[data-kind="repo.created"]').first()).toBeVisible();
  });
});
