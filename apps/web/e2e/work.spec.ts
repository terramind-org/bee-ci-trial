import { expect, test } from "@playwright/test";
import { repoPath, seed } from "./helpers";

test.describe("Work", () => {
  test("issue list is titled Issues and its rows are plain links, not boxes", async ({ page }) => {
    await page.goto(repoPath("work"));
    await expect(page.getByRole("heading", { level: 1 })).toHaveText("Issues");
    const row = page.locator("[data-nav-item]").first();
    await expect(row).toBeVisible();
    expect(await row.evaluate((el) => getComputedStyle(el).borderTopWidth)).toBe("0px");
  });

  test("issue list shows labels, sub-issue progress and linked pull requests", async ({ page }) => {
    await page.goto(repoPath("work"));
    const rows = page.locator("[data-nav-item]");
    await expect(rows.filter({ hasText: "Tax coverage for US states" })).toContainText("1/2 sub-issues");
    await expect(rows.filter({ hasText: "Invoice totals drift by a cent" })).toContainText("billing");
    await expect(rows.filter({ hasText: "Invoice totals drift by a cent" })).toContainText("pull request #" + seed.changes.fix.number + " (open)");
    await page.getByRole("link", { name: /^Closed/ }).click();
    await expect(page.locator("[data-nav-item]").filter({ hasText: "Add the Texas rate" })).toBeVisible();
  });

  test("issue detail shows sub-issues, the conversation and linked pull requests", async ({ page }) => {
    await page.goto(repoPath("work/" + seed.issues.states));
    await expect(page.getByRole("region", { name: /Sub-issues/ })).toContainText("#" + seed.issues.texas);
    await expect(page.getByRole("region", { name: /Sub-issues/ })).toContainText("#" + seed.issues.florida);
    await page.goto(repoPath("work/" + seed.issues.drift));
    await expect(page.getByRole("region", { name: "Conversation" })).toContainText("Picked this up");
    await expect(page.getByRole("region", { name: "Pull requests" })).toContainText(seed.changes.fix.title);
  });

  test("assigns an issue to an agent", async ({ page }) => {
    await page.goto(repoPath("work/" + seed.issues.florida));
    await page.getByLabel("Assignee").selectOption({ label: "codex · codex" });
    await page.getByRole("button", { name: "Assign", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "Assigned to codex" })).toBeVisible();
    await page.goto(repoPath("work"));
    await expect(page.locator("[data-nav-item]").filter({ hasText: "Add the Florida rate" }).locator('[data-state="running"]')).toHaveText("codex is working on it");
  });

  test("moves a card on the board with the keyboard-friendly select", async ({ page }) => {
    await page.goto(repoPath("work"));
    await page.getByRole("link", { name: "Board", exact: true }).click();
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(seed.project.title);
    // The columns stay; their cards are not boxes.
    expect(await page.getByRole("region", { name: "In Progress" }).evaluate((el) => getComputedStyle(el).borderTopWidth)).toBe("0px");
    const move = page.getByLabel("Move #" + seed.issues.retries + " to");
    await expect(move.locator("option:checked")).toHaveText("Todo");
    await move.press("ArrowDown");
    await expect(page.getByRole("region", { name: "In Progress" }).getByTestId("card-" + seed.issues.retries)).toBeVisible();
    await page.reload();
    await expect(page.getByRole("region", { name: "In Progress" }).getByTestId("card-" + seed.issues.retries)).toBeVisible();
  });

  test("comments on, closes and reopens an issue", async ({ page }) => {
    await page.goto(repoPath("work/" + seed.issues.rotate));
    const note = "Rotation landed behind a flag? " + Date.now();
    await page.getByLabel("Comment", { exact: true }).fill(note);
    await page.getByRole("button", { name: "Comment", exact: true }).click();
    await expect(page.getByText(note)).toBeVisible();
    await page.getByRole("button", { name: "Close issue", exact: true }).click();
    const close = page.getByRole("alertdialog", { name: "Close issue #" + seed.issues.rotate + "?" });
    await close.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(page.getByRole("button", { name: "Close issue", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close issue", exact: true }).click();
    await close.getByRole("button", { name: "Close issue", exact: true }).click();
    await expect(page.getByRole("button", { name: "Reopen", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Reopen", exact: true }).click();
    await page.getByRole("alertdialog", { name: "Reopen issue #" + seed.issues.rotate + "?" }).getByRole("button", { name: "Reopen", exact: true }).click();
    await expect(page.getByRole("button", { name: "Close issue", exact: true })).toBeVisible();
  });

  test("opens a short issue and redirects to its numbered page", async ({ page }) => {
    await page.goto(repoPath("work"));
    const title = "Trace invoice imports " + Date.now();
    await page.getByLabel(/^Describe what you want done/).fill(title);
    await page.getByRole("button", { name: "Open issue", exact: true }).click();
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(new RegExp("^#\\d+ " + title + "$"));
    await expect(page.getByTestId("issue-work")).toHaveText("Nobody has it yet.");
  });

  test("expands a long request and opens it with Markdown, labels and an assignee", async ({ page }) => {
    await page.goto(repoPath("work"));
    const form = page.getByTestId("new-issue");
    const draft = "Review invoice exports against the latest printed statements and list every place totals differ before suggesting any changes to how they are calculated.";
    await form.getByLabel(/^Describe what you want done/).fill(draft);
    await expect(form.getByLabel("Description (Markdown)")).toHaveValue(draft);
    await expect(form.getByLabel("Description (Markdown)")).toBeFocused();
    const title = "Review invoice exports " + Date.now();
    await form.getByLabel("Title", { exact: true }).fill(title);
    await form.getByLabel("Description (Markdown)").fill("Check **printed totals** first.");
    await form.getByLabel("Labels (comma separated)").fill("billing, docs");
    await form.getByLabel("Assignee").selectOption({ label: "codex · codex" });
    await form.getByRole("button", { name: "Open issue", exact: true }).click();
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(new RegExp("^#\\d+ " + title + "$"));
    await expect(page.locator("main strong").filter({ hasText: "printed totals" })).toBeVisible();
    await expect(page.getByRole("heading", { level: 1 }).locator("..")).toContainText("billing");
    await expect(page.getByRole("heading", { level: 1 }).locator("..")).toContainText("docs");
    await expect(page.getByTestId("issue-work")).toContainText("codex is working on it");
  });

  test("More detail keeps the short draft as an editable title", async ({ page }) => {
    await page.goto(repoPath("work"));
    await page.getByLabel(/^Describe what you want done/).fill("Trace invoice imports");
    await page.getByRole("button", { name: "More detail" }).click();
    await expect(page.getByLabel("Title", { exact: true })).toHaveValue("Trace invoice imports");
    await expect(page.getByLabel("Title", { exact: true })).toBeFocused();
    await expect(page.getByLabel("Description (Markdown)")).toHaveValue("");
  });
});
