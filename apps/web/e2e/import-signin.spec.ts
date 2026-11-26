import { expect, test } from "@playwright/test";
import { expectAccessible, orgPath, pressUntil, repoPath, rootToken, seed } from "./helpers";

test.describe("Import", () => {
  test("copy as names the real StartImport call; a GitHub App is required for installation ids", async ({ page }) => {
    await page.goto("/import");
    await expect(page.getByRole("heading", { level: 1, name: "Import from GitHub" })).toBeVisible();
    await page.getByLabel("GitHub repository or organization").fill("octo-org/service");
    await page.getByLabel("Installation id").fill("12345");
    await page.locator("main").getByRole("button", { name: "Copy as" }).first().click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByLabel("CLI command")).toContainText("bee import github octo-org/service --into " + seed.org + " --dry-run --json");
    await dialog.getByRole("tab", { name: "HTTP" }).click();
    await expect(dialog.getByLabel("curl request")).toContainText("/bee.v1.ImportService/StartImport");
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Estimate with a dry run" }).click();
    // The dev server has no GitHub App configured: the server's own answer, not a fake estimate.
    await expect(page.getByRole("alert").filter({ hasText: /GitHub App/ }).first()).toBeVisible();
  });

  test("a personal access token GitHub rejects comes back with its reason", async ({ page }) => {
    await page.goto("/import");
    await page.getByLabel("GitHub repository or organization").fill("octo-org/service");
    await page.getByLabel("Personal access token").check();
    await expect(page.getByLabel("Token", { exact: true })).toHaveAttribute("type", "password");
    await page.getByLabel("Token", { exact: true }).fill("ghp_not_a_real_token");
    await page.getByRole("button", { name: "Estimate with a dry run" }).click();
    // Rejected by GitHub (UNAUTHENTICATED) or unreachable from here (UNAVAILABLE): either way, typed.
    await expect(page.getByRole("alert").filter({ hasText: /GitHub|reach|UNAVAILABLE|UNAUTHENTICATED/ }).first()).toBeVisible({ timeout: 30_000 });
    await expect(page.getByTestId("import-progress")).toHaveCount(0);
  });
});

test.describe("Sign-in", () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("signed-out visits to pages that need a session go to sign-in, which names the server", async ({ page }) => {
    await page.goto(orgPath("members"));
    await expect(page).toHaveURL(/\/signin\?next=%2Forgs%2Fdev%2Fsettings%2Fmembers$/);
    await expect(page.getByText("bee-server --dev")).toBeVisible();
  });

  test("GitHub device flow reports that this server has no GitHub app", async ({ page }) => {
    await page.goto("/signin");
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    await expectAccessible(page);
    await page.getByRole("button", { name: "Continue with GitHub" }).click();
    await expect(page.getByRole("alert").filter({ hasText: /GitHub|UNIMPLEMENTED|INVALID_STATE/ }).first()).toBeVisible();
  });

  test("token paste rejects an invalid token, accepts the dev token, returns to the page, and signs out", async ({ page }) => {
    await page.goto("/signin?next=" + encodeURIComponent(repoPath("activity")));
    await page.getByLabel("Token", { exact: true }).fill("not-a-bee-token");
    await page.getByRole("button", { name: "Sign in with token" }).click();
    await expect(page.getByRole("alert").filter({ hasText: /TOKEN|UNAUTHENTICATED|INVALID/ }).first()).toBeVisible();
    await page.getByLabel("Token", { exact: true }).fill(rootToken());
    await page.getByRole("button", { name: "Sign in with token" }).click();
    await expect(page).toHaveURL(new RegExp(repoPath("activity") + "$"));
    // The rail's foot names who is signed in; signing out is a palette action.
    await expect(page.getByRole("complementary", { name: "Beehive" }).getByRole("link", { name: /^Your profile: / })).toContainText("dev");
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await pressUntil(page, "ControlOrMeta+k", () => expect(palette).toBeVisible({ timeout: 1000 }));
    await palette.getByRole("option", { name: "Sign out", exact: true }).click();
    await expect(page).toHaveURL(/\/signin/);
    await page.goto("/import");
    await expect(page).toHaveURL(/\/signin\?next=%2Fimport$/);
  });
});
