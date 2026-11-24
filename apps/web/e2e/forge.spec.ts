import { expect, test } from "@playwright/test";
import { orgPath, repoPath, seed } from "./helpers";

test.describe("Releases", () => {
  test("lists releases, generates notes from landed changes and publishes an immutable release", async ({ page }) => {
    await page.goto(repoPath("releases"));
    await expect(page.getByRole("heading", { name: "v0.1.0", exact: true })).toBeVisible();
    await expect(page.locator("section, article, li").filter({ hasText: "v0.1.0" }).getByText("locked").first()).toBeVisible();
    await expect(page.getByText("pre-release").first()).toBeVisible();
    const tag = "v0.3." + (Date.now() % 100000);
    await page.getByLabel("Tag").fill(tag);
    await page.getByRole("button", { name: "Generate from landed changes" }).click();
    await expect(page.getByLabel("Notes", { exact: true })).toHaveValue(/What.s Changed/);
    await page.getByRole("button", { name: "Publish " + tag }).click();
    await expect(page.getByRole("heading", { name: tag, exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Publish " + tag }).click();
    await expect(page.getByRole("alert").filter({ hasText: /ALREADY_EXISTS|TAG_EXISTS|exists/ }).first()).toBeVisible();
  });
});

test.describe("Packages and Pages", () => {
  test("packages list what the org published, with version and tags", async ({ page }) => {
    await page.goto(repoPath("packages"));
    const list = page.getByTestId("package-list");
    await expect(list).toContainText(seed.package);
    await expect(list).toContainText("0.1.0");
    await expect(list).toContainText("latest");
    await expect(list).toContainText("npm");
  });

  test("pages shows the site, its source and deployments", async ({ page }) => {
    await page.goto(repoPath("pages"));
    const site = page.getByTestId("pages-site");
    await expect(site).toContainText("main:/docs");
    await expect(page.getByText(/https?:\/\/\S*pages\S*/).first()).toBeVisible();
    await expect(page.getByRole("region", { name: "Deployments" })).toBeVisible();
  });
});

test.describe("Security", () => {
  test("lists code scanning alerts and dismisses one with a reason", async ({ page }) => {
    await page.goto(repoPath("security"));
    const alert = page.locator("li").filter({ hasText: "Insecure randomness" });
    await expect(alert).toContainText("src/auth/session.ts:13");
    await expect(alert).toContainText("high");
    await alert.getByLabel("Reason").selectOption("Risk accepted");
    await alert.getByRole("button", { name: "Dismiss", exact: true }).click();
    await expect(page.locator("li").filter({ hasText: "Insecure randomness" })).toHaveCount(0);
    await page.getByRole("link", { name: /^Dismissed/ }).click();
    await expect(page.locator("li").filter({ hasText: "Insecure randomness" })).toContainText("dismissed: Risk accepted");
  });
});

test.describe("Search", () => {
  test("finds code across repos and links each hit into its repo", async ({ page }) => {
    await page.goto("/search?q=invoiceTotal");
    const hit = page.locator("main").getByRole("link", { name: "src/billing/invoice.ts" }).first();
    await expect(hit).toHaveAttribute("href", repoPath("code?path=src%2Fbilling%2Finvoice.ts"));
    await expect(page.locator("main")).toContainText("export function invoiceTotal");
  });
});

test.describe("Settings", () => {
  test("policy: shows the repo's policy file, ready to evaluate, and points to org settings", async ({ page }) => {
    await page.goto(repoPath("settings"));
    await expect(page.getByLabel("Policy source")).toHaveValue(/agents_may_land_without_human/);
    await expect(page.getByRole("button", { name: "Evaluate", exact: true })).toBeEnabled();
    await expect(page.getByRole("link", { name: "organization settings" })).toHaveAttribute("href", orgPath());
  });

  test("audit log verifies the hash chain, names actors and filters", async ({ page }) => {
    await page.goto(orgPath("audit"));
    await expect(page.getByTestId("chain-status")).toContainText("Hash chain verified");
    const rows = page.getByTestId("audit-row");
    await expect(rows.first()).toBeVisible();
    await page.goto(orgPath("audit?action=release."));
    await expect(rows.filter({ hasText: "release.create" }).first()).toContainText("dev");
    await page.goto(orgPath("audit?action=review."));
    await expect(rows.first()).toContainText("review.");
    for (const t of await rows.allInnerTexts()) expect(t).toContain("review.");
    await page.goto(orgPath("audit"));
    await page.getByLabel("Denied only").check();
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page).toHaveURL(/denied=1/);
    // Every row shown is a denial; the list may be empty. Wait for the filtered page to replace
    // the loading skeleton before counting, or the count reads the skeleton's zero rows.
    const empty = page.getByText("No entries match");
    await expect(rows.first().or(empty)).toBeVisible();
    for (const t of await rows.allInnerTexts()) expect(t).toContain("denied");
    if ((await rows.count()) === 0) await expect(empty).toBeVisible();
  });

  test("members, teams and apps", async ({ page }) => {
    await page.goto(orgPath("members"));
    await expect(page.locator("li").filter({ hasText: /^.*dev.*owner/s }).first()).toContainText("owns");
    await page.getByRole("link", { name: "Teams" }).click();
    await expect(page.getByTestId("team-list")).toContainText("Platform");
    await expect(page.getByTestId("team-list")).toContainText("@" + seed.org + "/");
    await page.getByRole("link", { name: "Apps" }).click();
    await expect(page.getByTestId("app-list")).toContainText("bee-ci-bot");
    await expect(page.getByTestId("app-list")).toContainText("checks:write");
  });
});
