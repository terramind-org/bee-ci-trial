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

  test("issue list shows labels, sub-issue progress and linked changes", async ({ page }) => {
    await page.goto(repoPath("work"));
    const rows = page.locator("[data-nav-item]");
    await expect(rows.filter({ hasText: "Tax coverage for US states" })).toContainText("1/2 sub-issues");
    await expect(rows.filter({ hasText: "Invoice totals drift by a cent" })).toContainText("billing");
    await expect(rows.filter({ hasText: "Invoice totals drift by a cent" })).toContainText("linked change");
    await page.getByRole("link", { name: /^Closed/ }).click();
    await expect(page.locator("[data-nav-item]").filter({ hasText: "Add the Texas rate" })).toBeVisible();
  });

  test("issue detail shows sub-issues, the conversation and linked changes", async ({ page }) => {
    await page.goto(repoPath("work/" + seed.issues.states));
    await expect(page.getByRole("region", { name: /Sub-issues/ })).toContainText("#" + seed.issues.texas);
    await expect(page.getByRole("region", { name: /Sub-issues/ })).toContainText("#" + seed.issues.florida);
    await page.goto(repoPath("work/" + seed.issues.drift));
    await expect(page.getByRole("region", { name: "Conversation" })).toContainText("Picked this up");
    await expect(page.getByRole("region", { name: "Linked changes" })).toContainText(seed.changes.fix.title);
  });

  test("assigns an issue to an agent", async ({ page }) => {
    await page.goto(repoPath("work/" + seed.issues.florida));
    await page.getByLabel("Assignee").selectOption({ label: "codex · codex" });
    await page.getByRole("button", { name: "Assign", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "Assigned to codex" })).toBeVisible();
    await page.goto(repoPath("work"));
    await expect(page.locator("[data-nav-item]").filter({ hasText: "Add the Florida rate" }).getByRole("img", { name: "codex (agent)" })).toBeVisible();
  });

  test("moves a card on the board with the keyboard-friendly select", async ({ page }) => {
    await page.goto(repoPath("work/board"));
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(seed.project.title);
    // The columns stay; their cards are not boxes.
    expect(await page.getByRole("region", { name: "In Progress" }).evaluate((el) => getComputedStyle(el).borderTopWidth)).toBe("0px");
    await page.getByLabel("Move #" + seed.issues.retries + " to").selectOption({ label: "In Progress" });
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
    await expect(page.getByRole("button", { name: "Reopen", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Reopen", exact: true }).click();
    await expect(page.getByRole("button", { name: "Close issue", exact: true })).toBeVisible();
  });
});
