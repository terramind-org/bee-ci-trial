import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expectAccessible, seed, signIn } from "./helpers";

/** The reviewer `--dev` creates alongside the dev user: a member of dev, not a site operator. */
function reviewerToken(): string {
  const text = readFileSync(join(process.env.E2E_DATA_DIR ?? "", "credentials"), "utf8");
  return /^reviewer_token = "(.*)"/m.exec(text)?.[1] ?? "";
}

// This server has no enterprise account, so identity and security settings apply to the org.
const ORG = "?scope=org:" + seed.org;

test.describe("admin console · site operator", { tag: "@enterprise" }, () => {
  test("the dev user is an operator and sees every section", async ({ page }) => {
    await page.goto("/admin");
    await expect(page.getByRole("heading", { level: 1, name: "Admin" })).toBeVisible();
    const tabs = page.getByRole("navigation", { name: "Admin sections" });
    for (const name of ["Overview", "Readiness", "Enterprise", "Identity", "Security", "Policies", "Plans"]) await expect(tabs.getByRole("link", { name })).toBeVisible();
    await expect(tabs.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
  });

  test("overview lists the server's components with their state", async ({ page }) => {
    await page.goto("/admin");
    const list = page.getByRole("list", { name: "Components" });
    await expect(list).toBeVisible();
    expect(await list.locator("li").count()).toBeGreaterThan(0);
    await expect(page.getByText("Version", { exact: true })).toBeVisible();
    await expect(page.getByText("Uptime", { exact: true })).toBeVisible();
    await expect(page.getByRole("list", { name: "Cells" })).toBeVisible();
  });

  test("readiness renders the production checks and re-runs them", async ({ page }) => {
    await page.goto("/admin/readiness");
    const checks = page.getByRole("list", { name: "Readiness checks" });
    await expect(checks).toBeVisible();
    expect(await checks.locator("[data-check]").count()).toBeGreaterThanOrEqual(12);
    // A fresh --dev server runs on SQLite and disk, so those checks fail; the others pass.
    await expect(checks.locator('[data-check="metadata_postgres"]')).toHaveAttribute("data-status", "fail");
    await expect(checks.locator('[data-check="rate_limits"]')).toHaveAttribute("data-status", "pass");
    await expect(checks.locator('[data-check="metadata_postgres"]').getByRole("link")).toHaveAttribute("href", /self-hosting\.md#/);
    await page.getByRole("button", { name: "Re-run" }).click();
    await expect(page.getByText("Checks re-run")).toBeVisible();
    await expect(checks.locator("[data-check]").first()).toBeVisible();
  });

  test("readiness re-run checks the text as edited", async ({ page }) => {
    await page.goto("/admin/readiness");
    await expect(page.getByRole("list", { name: "Readiness checks" })).toBeVisible();
    await page.getByLabel(/server\.toml as the server runs it/).fill("[server\nedition = ");
    await page.getByRole("button", { name: "Re-run" }).click();
    await expect(page.getByRole("list", { name: "Configuration errors" })).toBeVisible();
  });

  test("adding a verified domain shows the DNS TXT record to create", async ({ page }) => {
    await page.goto("/admin/identity" + ORG);
    const domain = "e2e-" + Date.now() + ".example.com";
    await page.getByLabel("Domain", { exact: true }).fill(domain);
    await page.getByRole("button", { name: "Add domain" }).click();
    const added = page.getByTestId("added-domain");
    await expect(added).toContainText(domain);
    const record = added.getByTestId("dns-record");
    await expect(record).toContainText("TXT record name");
    await expect(record).toContainText("TXT record value");
    await expect(record.locator("[data-value]").first()).not.toHaveText("—");
    // It is on the list too, after the page refreshes, and can be deleted after a confirm.
    const row = page.getByRole("list", { name: "Verified domains" }).locator("li").filter({ hasText: domain });
    await expect(row).toBeVisible();
    await row.getByRole("button", { name: "Delete", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete domain" }).click();
    await expect(page.getByRole("list", { name: "Verified domains" }).locator("li").filter({ hasText: domain })).toHaveCount(0);
  });

  test("the IP allow list checks a CIDR before it asks the server", async ({ page }) => {
    await page.goto("/admin/security" + ORG);
    await page.getByLabel("Address or CIDR").fill("10.0.0.0/40");
    await page.getByRole("button", { name: "Add entry" }).click();
    await expect(page.getByText(/Not an IPv4 or IPv6 address or CIDR block/)).toBeVisible();
    const cidr = "198.51.100." + (Date.now() % 200) + "/32";
    await page.getByLabel("Address or CIDR").fill(cidr);
    await page.getByRole("button", { name: "Add entry" }).click();
    const row = page.getByRole("list", { name: "Allowed addresses" }).locator("li").filter({ hasText: cidr });
    await expect(row).toBeVisible();
    await row.getByRole("button", { name: "Delete", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete entry" }).click();
    await expect(row).toHaveCount(0);
  });

  test("every write has Copy as", async ({ page }) => {
    await page.goto("/admin/identity" + ORG);
    for (const op of ["EnterpriseService/UpdateSamlConfig", "EnterpriseService/TestSamlConfig", "EnterpriseService/UpdateScimConfig", "EnterpriseService/CreateScimToken", "EnterpriseService/AddVerifiedDomain", "EnterpriseService/UpdateTwoFactorRequirement"])
      await expect(page.locator(`[data-copy-as="${op}"]`).first()).toBeVisible();
  });

  test("a SCIM token is shown once", async ({ page }) => {
    await page.goto("/admin/identity" + ORG);
    await page.getByLabel("Description").fill("e2e");
    await page.getByRole("button", { name: "Create token" }).click();
    await expect(page.getByTestId("scim-secret")).toContainText("shown once");
    await page.getByRole("button", { name: "I have saved it" }).click();
    await expect(page.getByTestId("scim-secret")).toHaveCount(0);
  });

  test("plans: a confirm stands between the click and the change", async ({ page }) => {
    await page.goto("/admin/plans");
    await page.getByRole("button", { name: "Assign plan" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("plan?");
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toHaveCount(0);
  });

  test("policies state where anonymous read is set", async ({ page }) => {
    await page.goto("/admin/policies");
    await expect(page.getByText("[forge] allow_anonymous_public_read")).toBeVisible();
  });

  for (const theme of ["light", "dark"] as const) {
    test.describe("accessibility · " + theme, () => {
      test.use({ colorScheme: theme });
      for (const path of ["/admin", "/admin/readiness", "/admin/enterprise", "/admin/identity" + ORG, "/admin/security" + ORG, "/admin/policies", "/admin/plans"]) {
        test("axe: " + path, async ({ page }) => {
          await page.goto(path);
          await expect(page.locator("main")).toBeVisible();
          await page.waitForLoadState("load");
          await page.waitForTimeout(400);
          await expectAccessible(page);
        });
      }
    });
  }
});

test.describe("admin console · not an operator", { tag: "@enterprise" }, () => {
  test.use({ storageState: { cookies: [], origins: [] } });

  test("a member who is not a site operator gets the 404 page", async ({ page }) => {
    await signIn(page, reviewerToken());
    // The shell streams, so the status line is already 200 when the layout decides: the page is the 404 page.
    await page.goto("/admin");
    await expect(page.getByRole("heading", { level: 1, name: "Nothing here" })).toBeVisible();
    await expect(page.getByRole("navigation", { name: "Admin sections" })).toHaveCount(0);
    for (const sub of ["readiness", "identity", "security", "policies", "plans"]) {
      await page.goto("/admin/" + sub);
      await expect(page.getByRole("heading", { level: 1, name: "Nothing here" })).toBeVisible();
    }
  });

  test("signed out goes to sign-in", async ({ page }) => {
    await page.goto("/admin");
    await expect(page).toHaveURL(/\/signin\?next=%2Fadmin$/);
  });
});
