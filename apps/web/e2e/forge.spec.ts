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
  test("opens Rules first and proposes edits through a pull request", async ({ page }) => {
    await page.goto(repoPath("settings"));
    const nav = page.getByRole("navigation", { name: "Settings sections" });
    await expect(nav.getByRole("link")).toHaveText(["Rules", "Instructions", "Access", "Visibility", "More settings"]);
    await expect(nav.getByRole("link", { name: "Rules", exact: true })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("heading", { level: 1 })).toHaveText("Settings");
    await expect(page.getByRole("region", { name: "Rules", exact: true })).toContainText(/needs two approvals from a person/);
    await page.getByRole("button", { name: "Draft a change", exact: true }).click();
    await expect(page.getByLabel("Policy source")).toHaveValue(/agents_may_land_without_human/);
    await expect(page.getByRole("button", { name: "Evaluate", exact: true })).toBeEnabled();
    await page.getByRole("button", { name: "Propose this change", exact: true }).click();
    await expect(page.getByText(/bee -C policy-draft cr open/)).toBeVisible();
    await expect(page.getByRole("button", { name: "Save", exact: true })).toHaveCount(0);
    await nav.getByRole("link", { name: "More settings" }).click();
    await expect(page.getByRole("link", { name: "Organization settings", exact: true })).toHaveAttribute("href", orgPath());
  });

  test("opens instructions and access and asks before changing visibility", async ({ page }) => {
    await page.goto(repoPath("settings?section=instructions"));
    const nav = page.getByRole("navigation", { name: "Settings sections" });
    await expect(page.getByRole("heading", { name: "Instructions", exact: true })).toBeVisible();
    await expect(page.getByText("Change repository instructions through a pull request.")).toBeVisible();
    await nav.getByRole("link", { name: "Access" }).click();
    await expect(page.getByRole("heading", { name: "Deploy keys" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Agent tokens" })).toHaveAttribute("href", repoPath("agents"));
    await nav.getByRole("link", { name: "Visibility" }).click();
    const change = page.getByRole("button", { name: /^Make (public|private)$/ });
    await expect(change).toHaveAttribute("data-agent-confirm", /Makes .* (public|private)/);
    await change.click();
    const dialog = page.getByRole("alertdialog");
    await expect(dialog.getByText(/Anyone can read|Only people and agents with access/)).toBeVisible();
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();
  });

  test("gives the agent the same sections and marks visibility for a person", async ({ page }) => {
    await page.goto(repoPath("settings?mode=bee"));
    const view = page.locator('[data-bee-page="settings"]');
    await expect(view.locator("[data-bee-group] > h2")).toHaveText([
      "## rules", "## instructions", "## access", "## visibility", "## more settings", "## tools on this page",
    ]);
    await expect(view.locator('[data-tool="repo_update_repo"]')).toHaveAttribute("data-kind", "needs a person");
    await expect(view.locator('[data-tool="change_open_change"]')).toContainText("Open a pull request proposing a change");
  });

  test("the record verifies its chain, names actors and filters", async ({ page }) => {
    await page.goto(orgPath("audit"));
    await expect(page.getByRole("heading", { name: "Record", exact: true })).toBeVisible();
    await expect(page.getByTestId("chain-status")).toHaveText(/The record is intact up to entry \d+\./);
    const rows = page.getByTestId("audit-row");
    await expect(rows.first()).toBeVisible();
    await page.getByLabel("Actor").selectOption(seed.human.id);
    await page.getByLabel("Action prefix").fill("release.");
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page).toHaveURL(/action=release\./);
    await expect(rows.filter({ hasText: "release.create" }).first()).toContainText("dev");
    await page.goto(orgPath("audit?action=review."));
    await expect(rows.first()).toContainText("review.");
    for (const t of await rows.allInnerTexts()) expect(t).toContain("review.");
    await page.goto(orgPath("audit"));
    await page.getByLabel("Outcome").selectOption("refused");
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page).toHaveURL(/outcome=refused/);
    // Every row shown is a denial; the list may be empty. Wait for the filtered page to replace
    // the loading skeleton before counting, or the count reads the skeleton's zero rows.
    const empty = page.getByText("No entries match");
    await expect(rows.first().or(empty)).toBeVisible();
    for (const t of await rows.allInnerTexts()) expect(t).toContain("refused");
    if ((await rows.count()) === 0) await expect(empty).toBeVisible();
  });

  test("members, teams and apps", async ({ page }) => {
    await page.goto(orgPath("members"));
    const person = page.getByTestId("org-people").getByRole("listitem").filter({ has: page.getByRole("heading", { name: "dev", exact: true }) });
    await expect(person).toContainText("Owner");
    await expect(person).toHaveText(/\b[1-9]\d* agents? answers? to dev\./);
    await expect(person.getByRole("button", { name: "Remove", exact: true })).toHaveAttribute("data-agent-confirm", /Removes dev/);
    await page.getByRole("link", { name: "Teams" }).click();
    await expect(page.getByTestId("team-list")).toContainText("Platform");
    await expect(page.getByTestId("team-list")).toContainText("@" + seed.org + "/");
    await expect(page.getByTestId("team-list")).toContainText("dev (maintainer)");
    await page.getByRole("link", { name: "Apps" }).click();
    await expect(page.getByTestId("app-list")).toContainText("bee-ci-bot");
    await expect(page.getByTestId("app-list")).toContainText("checks:write");
  });

  test("agents and budgets lists principals, owner budgets and tools", async ({ page }) => {
    await page.goto(orgPath("agents"));
    await expect(page.getByRole("heading", { level: 1, name: "Agents and budgets" })).toBeVisible();
    const agents = page.getByTestId("org-agent");
    for (const principal of Object.values(seed.agents)) {
      const agent = agents.filter({ has: page.getByRole("heading", { name: principal.display, exact: true }) });
      await expect(agent.locator("[data-mark]")).toHaveText(seed.human.display + " answers for this agent");
    }
    await expect(agents.filter({ hasText: seed.agents.claude.display }).getByRole("button", { name: /^Revoke / }).first()).toHaveAttribute("data-agent-confirm", /^Revokes /);
    const budget = page.getByTestId("org-agent-budget").filter({ hasText: seed.human.display });
    await budget.getByRole("button", { name: /^Edit budget for / }).click();
    await expect(budget.getByRole("button", { name: "Set budget", exact: true })).toHaveAttribute("data-agent-confirm", /^Sets the monthly limits /);

    await page.goto(orgPath("agents?mode=bee"));
    const facts = page.locator('[data-bee-page="org-agents"]');
    await expect(facts).toContainText(seed.agents.claude.display);
    await expect(facts).toContainText(seed.human.display);
    await expect(facts.locator('[data-tool="agent_set_agent_budget"]')).toHaveAttribute("data-kind", "needs a person");
  });
});
