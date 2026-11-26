import { expect, test } from "@playwright/test";
import { repoPath, seed } from "./helpers";

test.describe("Code", () => {
  test("browses the tree into a highlighted file with the path context", async ({ page }) => {
    await page.goto(repoPath("code"));
    await expect(page.getByRole("region", { name: "README.md" })).toContainText("Invoicing service used to demo Beehive");
    await page.getByRole("link", { name: "src/", exact: true }).click();
    await page.getByRole("link", { name: "billing/", exact: true }).click();
    await page.getByRole("link", { name: "invoice.ts" }).click();
    const table = page.getByRole("table", { name: "Contents of src/billing/invoice.ts" });
    await expect(table).toContainText("export function invoiceTotal");
    await expect(table.locator(".sh__token--keyword").first()).toBeVisible();
    const context = page.getByRole("complementary", { name: "Path context" });
    await expect(context).toContainText("Before you edit");
    await expect(context).toContainText("src/billing");
    await expect(context).toContainText("claude-code");
    await expect(context).toContainText(/approval/);
  });

  test("the tree is a plain list with who is editing, and the page has one menu", async ({ page }) => {
    await page.goto(repoPath("code"));
    const files = page.getByRole("list", { name: "Files in the repository root" });
    await expect(files).not.toHaveClass(/border/);
    // The seeded lease on src/billing shows as an avatar stack whose name is a sentence.
    await expect(files.getByRole("button", { name: /is editing .*src\/billing/ }).first()).toBeAttached();
    await expect(page.getByRole("button", { name: "More actions", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: /^Copy as/ })).toHaveCount(0);
  });

  test("file lines are anchorable", async ({ page }) => {
    await page.goto(repoPath("code?path=src/billing/tax.ts#L8"));
    await expect(page.getByRole("table", { name: "Contents of src/billing/tax.ts" })).toContainText("US-TX");
    await expect(page).toHaveURL(/#L8$/);
  });

  test("shows history and blame for a file", async ({ page }) => {
    await page.goto(repoPath("code?path=src/billing/tax.ts"));
    await page.getByRole("link", { name: "History" }).click();
    const history = page.getByRole("region", { name: /History of src\/billing\/tax.ts/ });
    await expect(history).toContainText("Add the Texas sales tax rate");
    await expect(history).toContainText("Start the invoicing service");
    await page.getByRole("link", { name: "Blame" }).click();
    const blame = page.getByRole("region", { name: /^Blame ·/ });
    await expect(blame).toContainText("Add the Texas sales tax rate");
    await expect(blame).toContainText("Start the invoicing service");
  });

  test("compares two revisions", async ({ page }) => {
    await page.goto(repoPath("code?view=compare"));
    await page.getByLabel("From", { exact: true }).fill(seed.history.base);
    await page.getByLabel("To", { exact: true }).fill("main");
    await page.getByRole("button", { name: "Compare" }).click();
    await expect(page.getByRole("region", { name: "Diff of src/billing/tax.ts" })).toContainText("US-TX");
  });

  test("a revision that does not resolve renders the not-found page", async ({ page }) => {
    await page.goto(repoPath("code?rev=no-such-ref"));
    await expect(page.getByRole("heading", { name: "Nothing here" })).toBeVisible();
  });
});
